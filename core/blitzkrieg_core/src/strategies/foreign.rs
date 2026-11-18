//! External strategy adapter for C ABI v2.
//!
//! A [`ForeignStrategy`] wraps a dlopen'ed v2 vtable and implements the SAME
//! full [`EngineStrategy`] contract as an in-tree strategy. "External" is only
//! a loading difference: it receives every `on_book` with the full depth ladder
//! and derived metrics, round/market context, config and hot parameters, and it
//! returns entries, close intents, trend breaks, confirmation state and
//! diagnostics. Entries are sized by the host; close intents are priced and
//! submitted by the host through the shared exit path. Nothing credential-,
//! network-, OME- or socket-related ever crosses this boundary.
//!
//! All structured outputs cross as heap JSON strings allocated and freed by the
//! SAME loaded library (its `bk_strategy_free_string`), so allocators never mix.
//!
//! E2-c (#28): the adapter is now per-strategy in the evolution chain too.
//!  - hot parameters are pushed from THIS strategy's own cell in the kernel's
//!    [`ParamRegistry`](crate::registry::ParamRegistry) — a name→value bag
//!    of the knobs the library itself declared, not spread_arb's four fields;
//!  - `evolvable_knobs` reads the OPTIONAL `bk_strategy_evolvable_knobs` symbol,
//!    so a library self-certifies which knobs may evolve and over what domain;
//!  - `shadow_factory` builds a **twin** by calling the library's `create()`
//!    again (a second, independent instance from the SAME handle table) and
//!    pushing it the counterfactual parameters — so a shadow variant runs the
//!    library's own entry/exit logic, exactly as the in-tree path does.
//!
//! Library ownership: the `Library` is held behind an `Arc` (`LoadedLibrary`) and
//! every instance destroys its own handle in `Drop`, which runs BEFORE the `Arc`
//! releases the library — so no vtable function pointer can outlive its library.
//! A twin holds the same `Arc`, so the library stays mapped while any twin lives.
//!
//! Privacy note: the struct fields are `pub(super)` so the domain children
//! (`strategy`, `shadow`) can construct/inspect instances; the public surface
//! stays `ForeignStrategy`, `LoadedLibrary`, `from_loaded` and `new`.

use crate::signal::TradePairDecl;
use crate::strategies::StrategyExitIntent;
use arc_swap::ArcSwap;
use blitzkrieg_strategy_api::{
    BkEvolvableKnobsFn, BkGateExemptionsFn, BkHandle, BkStrategyVtable, TrailingExitVerdict,
};
use libloading::Library;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::ffi::c_char;
use std::sync::Arc;
use strategy_logic::params::{KnobSpec, StrategyParams};

mod book_marshal;
mod eval_ctx;
mod reserved_keys;
mod shadow;
mod strategy;

#[cfg(test)]
mod tests;

/// A dlopen'ed strategy library plus the symbols the kernel resolves once, at
/// load time. Shared (`Arc`) by every instance the library produces, including
/// shadow twins.
pub struct LoadedLibrary {
    lib: Library,
    /// `bk_strategy_create` — returns the library's STATIC vtable pointer. Called
    /// once per instance (the live strategy and each twin) so every instance gets
    /// its own opaque handle from the same table.
    pub(super) create_sym: unsafe extern "C" fn() -> *const BkStrategyVtable,
    /// This library's own JSON deallocator (`bk_strategy_free_string`).
    pub(super) free_string: Option<unsafe extern "C" fn(*mut c_char)>,
    // Optional symbols (absent = capability not declared).
    pub(super) gate_exemptions_fn: Option<BkGateExemptionsFn>,
    pub(super) evolvable_knobs_fn: Option<BkEvolvableKnobsFn>,
    /// OPTIONAL `bk_strategy_bind_eval_ctx` (E-parity): the fresh-book gate.
    /// None = the library does not use the context (an older library) and
    /// keeps the plain v2 contract unchanged.
    pub(super) bind_eval_ctx_fn: Option<blitzkrieg_strategy_api::BkBindEvalCtxFn>,
    /// OPTIONAL `bk_strategy_config_view` (E-parity): the config currently in
    /// force, as the strategy reports it. None = "declares nothing".
    pub(super) config_view_fn: Option<blitzkrieg_strategy_api::BkConfigViewFn>,
    /// OPTIONAL `bk_strategy_settlement_holds`: hold-to-settlement declaration
    /// (expiry redemption instead of the exit ladder). None = "no declaration".
    pub(super) settlement_holds_fn: Option<blitzkrieg_strategy_api::BkSettlementHoldsFn>,
    /// OPTIONAL `bk_strategy_on_kline` (#642): the CLOSED-bar hook. None =
    /// the library is never driven by the K-line path (the old default).
    pub(super) on_kline_fn: Option<blitzkrieg_strategy_api::BkOnKlineFn>,
    /// OPTIONAL `bk_strategy_entry_pair` (#642): the pair-declaration default
    /// applied to entries without their own per-entry `pair`. None = no
    /// default (the old behaviour).
    pub(super) entry_pair_fn: Option<blitzkrieg_strategy_api::BkEntryPairFn>,
    /// OPTIONAL `bk_strategy_trailing_exit` (#642): three-state trailing
    /// verdict (-1 follow kernel / 1 trail / 0 never). None = follow kernel.
    pub(super) trailing_exit_fn: Option<blitzkrieg_strategy_api::BkTrailingExitFn>,
    /// OPTIONAL `bk_strategy_atomic_batch` (#642): the E-VA batch
    /// declaration. None / 0 = the per-order path.
    pub(super) atomic_batch_fn: Option<blitzkrieg_strategy_api::BkAtomicBatchFn>,
}

impl LoadedLibrary {
    /// # Safety
    /// `lib` must have been opened by the caller and must export the mandatory v2
    /// symbols; the resolved symbols are valid for as long as `lib` is loaded,
    /// which this struct guarantees by owning it.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn new(
        lib: Library,
        create_sym: unsafe extern "C" fn() -> *const BkStrategyVtable,
        free_string: Option<unsafe extern "C" fn(*mut c_char)>,
        gate_exemptions_fn: Option<BkGateExemptionsFn>,
        evolvable_knobs_fn: Option<BkEvolvableKnobsFn>,
        bind_eval_ctx_fn: Option<blitzkrieg_strategy_api::BkBindEvalCtxFn>,
        config_view_fn: Option<blitzkrieg_strategy_api::BkConfigViewFn>,
        settlement_holds_fn: Option<blitzkrieg_strategy_api::BkSettlementHoldsFn>,
        on_kline_fn: Option<blitzkrieg_strategy_api::BkOnKlineFn>,
        entry_pair_fn: Option<blitzkrieg_strategy_api::BkEntryPairFn>,
        trailing_exit_fn: Option<blitzkrieg_strategy_api::BkTrailingExitFn>,
        atomic_batch_fn: Option<blitzkrieg_strategy_api::BkAtomicBatchFn>,
    ) -> Arc<Self> {
        Arc::new(Self {
            lib,
            create_sym,
            free_string,
            gate_exemptions_fn,
            evolvable_knobs_fn,
            bind_eval_ctx_fn,
            config_view_fn,
            settlement_holds_fn,
            on_kline_fn,
            entry_pair_fn,
            trailing_exit_fn,
            atomic_batch_fn,
        })
    }

    /// The library's static vtable, copied by value.
    pub(super) fn vtable(&self) -> *const BkStrategyVtable {
        // SAFETY: symbol resolved from this library; the library guarantees a
        // pointer to static data.
        unsafe { (self.create_sym)() }
    }

    /// Path the library was loaded from (diagnostics).
    pub fn path(&self) -> Option<std::path::PathBuf> {
        // libloading exposes the path only through its own handle; the load
        // report carries it, so this is a no-op for observability.
        None
    }

    /// Keeps the mapping alive; dropping this drops the `Library` itself.
    pub fn library(&self) -> &Library {
        &self.lib
    }
}

/// A loaded external v2 strategy. Owns its opaque handle; shares the library.
pub struct ForeignStrategy {
    pub(super) lib: Arc<LoadedLibrary>,
    pub(super) vtable: BkStrategyVtable,
    pub(super) handle: BkHandle,
    pub(super) name: String,
    /// The account scope this INSTANCE trades for (set by the loader when the
    /// host registers a per-account scope; default-scope loads keep the
    /// default account). Packages are account-blind by declaration — this is
    /// host metadata, never strategy input.
    pub(super) account_id: String,
    pub(super) version: String,
    /// Deallocator resolved from THIS library for its JSON outputs.
    pub(super) free_string: Option<unsafe extern "C" fn(*mut c_char)>,
    /// Optional v2 symbol `bk_strategy_gate_exemptions` (E2-b / #27). None when
    /// the library does not export it — then the strategy declares nothing and
    /// stays fully gated, exactly like the in-tree default.
    pub(super) gate_exemptions_fn: Option<BkGateExemptionsFn>,
    /// Optional v2 symbol `bk_strategy_evolvable_knobs` (E2-c / #28). None when
    /// the library does not export it — then the strategy is **not evolvable**
    /// (no variants are built for it), which is the explicit declaration, not an
    /// error.
    pub(super) evolvable_knobs_fn: Option<BkEvolvableKnobsFn>,
    /// This strategy's own evolvable knobs, resolved once at load (they are a
    /// property of the library build, not of the runtime state).
    pub(super) knobs: Vec<KnobSpec>,
    /// OPTIONAL binder symbol (copied from the shared library record).
    pub(super) bind_eval_ctx_fn: Option<blitzkrieg_strategy_api::BkBindEvalCtxFn>,
    /// OPTIONAL config-view symbol (copied from the shared library record).
    pub(super) config_view_fn: Option<blitzkrieg_strategy_api::BkConfigViewFn>,
    /// OPTIONAL hold-to-settlement symbol (copied from the shared library
    /// record).
    pub(super) settlement_holds_fn: Option<blitzkrieg_strategy_api::BkSettlementHoldsFn>,
    /// OPTIONAL closed-bar hook (#642). None = never driven by K-lines.
    pub(super) on_kline_fn: Option<blitzkrieg_strategy_api::BkOnKlineFn>,
    /// OPTIONAL pair-declaration default (#642): the `{key, leg}` pair read
    /// once at load (a property of the library build, like the knobs) and
    /// attached by `adopt_eval_json` to entries without their own per-entry
    /// `pair`. None = no default.
    pub(super) entry_pair: Option<TradePairDecl>,
    /// OPTIONAL three-state trailing verdict (#642), carried in the SHARED
    /// capability model's enum (issue #843) — `FollowKernel` = follow the
    /// kernel-wide `risk.setExit` trailingEnabled value, bit-identically to
    /// every existing library. The `i32`→enum decode happens ONCE here through
    /// [`blitzkrieg_strategy_api::TrailingExitVerdict::from_abi_int`].
    pub(super) wants_trailing_exit: TrailingExitVerdict,
    /// OPTIONAL atomic-batch declaration (#642). false = the per-order path.
    pub(super) atomic_batch: bool,

    // Outputs accumulated during evaluate(), drained by the host.
    pub(super) exit_intents: Vec<StrategyExitIntent>,
    pub(super) breaks: Vec<(String, Decimal)>,

    // Shadow Evolution: THIS strategy's cell plus the JSON we last pushed, so the
    // strategy only sees a callback on an actual change.
    pub(super) params: Option<Arc<ArcSwap<StrategyParams>>>,
    pub(super) last_hot_json: Option<String>,

    // token → asset, refreshed from each round's markets so on_book (which only
    // carries a token) can still label the view with the underlying asset.
    pub(super) assets: HashMap<String, String>,
}
