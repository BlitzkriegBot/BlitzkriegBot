//! Shadow twin factory for E2-c evolution: a twin is a second instance from
//! the same library (`create()` again), seeded with counterfactual
//! parameters before its first evaluation, so the counterfactual runs the
//! library's own entry/exit logic.
//! Moved verbatim from `foreign.rs` (pure code motion).

use super::{ForeignStrategy, LoadedLibrary};
use crate::strategies::EngineStrategy;
use crate::strategies::shadow_twin::ShadowFactory;
use blitzkrieg_strategy_api::BkHandle;
use std::collections::HashMap;
use std::ffi::CStr;
use std::sync::Arc;
use strategy_logic::params::{KnobSpec, StrategyParams};

/// Builds independent twins of a loaded external strategy (E2-c / #28).
///
/// A twin is a SECOND instance from the same library, so the counterfactual
/// runs the library's own logic — the same contract the in-tree twin path
/// uses. The library stays mapped while any twin lives.
pub(super) struct ForeignShadowFactory {
    pub(super) lib: Arc<LoadedLibrary>,
    pub(super) name: String,
    pub(super) knobs: Vec<KnobSpec>,
}

impl ForeignShadowFactory {
    /// Read the pair-declaration default from a JUST-CREATED twin handle: the
    /// live instance's `read_entry_pair` runs on ITS handle, and a twin owns a
    /// different one, so the declaration is re-read per instance (the same
    /// instance-per-handle rule `take_json`'s deallocator pairing demands).
    /// None on any absence/malformation, exactly like the live read.
    fn read_twin_entry_pair(&self, handle: BkHandle) -> Option<crate::signal::TradePairDecl> {
        let f = self.lib.entry_pair_fn?;
        // SAFETY: the twin handle was created by this library's own create()
        // above; the JSON it returns is freed through the library's own
        // deallocator (resolved once, shared by every instance).
        let raw = unsafe { f(handle) };
        let v = if raw.is_null() {
            return None;
        } else {
            // SAFETY: pointer came from this library's CString::into_raw; copy
            // then free once via the matching allocator.
            let parsed = unsafe { CStr::from_ptr(raw) }
                .to_str()
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
            if let Some(free) = self.lib.free_string {
                // SAFETY: same provenance as above.
                unsafe { free(raw) };
            }
            parsed?
        };
        let obj = v.as_object()?;
        let key = obj.get("key").and_then(|k| k.as_str())?.to_string();
        let leg = obj.get("leg").and_then(|l| l.as_str())?.to_string();
        Some(crate::signal::TradePairDecl { key, leg })
    }
}

impl ShadowFactory for ForeignShadowFactory {
    fn strategy(&self) -> String {
        self.name.clone()
    }

    fn knobs(&self) -> Vec<KnobSpec> {
        self.knobs.clone()
    }

    fn make(&self, params: &StrategyParams) -> Option<Box<dyn EngineStrategy>> {
        let vtable = unsafe { std::ptr::read(self.lib.vtable()) };
        let create = vtable.create?;
        let handle = unsafe { create() };
        if handle.is_null() {
            return None;
        }
        let mut twin = ForeignStrategy {
            lib: self.lib.clone(),
            vtable,
            handle,
            name: self.name.clone(),
            account_id: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            version: String::new(),
            free_string: self.lib.free_string,
            gate_exemptions_fn: self.lib.gate_exemptions_fn,
            evolvable_knobs_fn: self.lib.evolvable_knobs_fn,
            knobs: self.knobs.clone(),
            bind_eval_ctx_fn: self.lib.bind_eval_ctx_fn,
            config_view_fn: self.lib.config_view_fn,
            settlement_holds_fn: self.lib.settlement_holds_fn,
            on_kline_fn: self.lib.on_kline_fn,
            // The twin is a second instance of the SAME library, so it reads
            // the SAME declarations from its own handle — one instance per
            // declaration read keeps every handle's own deallocator in play.
            entry_pair: self.read_twin_entry_pair(handle),
            wants_trailing_exit: self
                .lib
                .trailing_exit_fn
                .map(|f| {
                    // SAFETY: valid twin handle; a pure integer read. The shared
                    // verdict decoder (issue #843) keeps the fail-closed
                    // out-of-range rule alive on the twin path too.
                    blitzkrieg_strategy_api::TrailingExitVerdict::from_abi_int(unsafe { f(handle) })
                })
                .unwrap_or(blitzkrieg_strategy_api::TrailingExitVerdict::FollowKernel),
            atomic_batch: self
                .lib
                .atomic_batch_fn
                .map(|f| -> bool {
                    // SAFETY: valid twin handle; a pure integer read.
                    unsafe { f(handle) != 0 }
                })
                .unwrap_or(false),
            exit_intents: Vec::new(),
            breaks: Vec::new(),
            params: None,
            last_hot_json: None,
            assets: HashMap::new(),
        };
        // Seed the twin with its counterfactual values BEFORE it is driven, so its
        // very first evaluation already uses them (next-tick semantics would give
        // the twin one tick at the live parameters).
        twin.apply_params_direct(params);
        Some(Box::new(twin))
    }
}
