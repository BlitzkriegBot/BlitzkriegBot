//! `ForeignStrategy` behaviour: instance lifecycle (`from_loaded`, `Drop`),
//! the v2 hook calls (`on_book`/`on_round`/`on_kline`), evaluate adoption
//! (`find_candidates`/`diagnostics`), config and hot-params, and the
//! `EngineStrategy` trait impl.
//! Moved verbatim from `foreign.rs` (pure code motion).

use super::book_marshal::BookMarshal;
use super::eval_ctx::{BoundCtx, EvalCtxMarshal};
use super::reserved_keys::{adopt_eval_json, parse_breaks};
use super::shadow::ForeignShadowFactory;
use super::{ForeignStrategy, LoadedLibrary};
use crate::model::OrderbookSnapshot;
use crate::registry::ParamRegistry;
use crate::strategies::{
    EngineStrategy, GateExemptions, ShadowFactory, StrategyCtx, StrategyExitIntent, TradeSignal,
};
use blitzkrieg_strategy_api::{BkHandle, BkMarket, BkRound, BkRoundView};
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_char};
use std::sync::Arc;
use strategy_logic::params::{KnobDeclaration, KnobSpec, StrategyParams};

impl ForeignStrategy {
    /// # Safety
    /// Caller provides a successfully negotiated v2 library: vtable/handle come
    /// from `lib`, the library implements the documented contract (static
    /// vtable, valid create/destroy, JSON outputs freed by its own
    /// `bk_strategy_free_string`), and it is only ever driven single-threaded
    /// by the kernel's strategy loop.
    pub unsafe fn from_loaded(
        lib: Arc<LoadedLibrary>,
        name: String,
        version: String,
        account_id: String,
    ) -> Self {
        let vtable = unsafe { std::ptr::read(lib.vtable()) };
        // SAFETY: negotiation in the loader established a v2 library with a
        // static vtable and a non-null create().
        let handle: BkHandle = unsafe { vtable.create.unwrap_or_else(|| unreachable!())() };
        let free_string = lib.free_string;
        let gate_exemptions_fn = lib.gate_exemptions_fn;
        let evolvable_knobs_fn = lib.evolvable_knobs_fn;
        let bind_eval_ctx_fn = lib.bind_eval_ctx_fn;
        let config_view_fn = lib.config_view_fn;
        let settlement_holds_fn = lib.settlement_holds_fn;
        let on_kline_fn = lib.on_kline_fn;
        let trailing_exit_fn = lib.trailing_exit_fn;
        let atomic_batch_fn = lib.atomic_batch_fn;
        let mut s = Self {
            lib,
            vtable,
            handle,
            name,
            account_id,
            version,
            free_string,
            gate_exemptions_fn,
            evolvable_knobs_fn,
            knobs: Vec::new(),
            bind_eval_ctx_fn,
            config_view_fn,
            settlement_holds_fn,
            on_kline_fn,
            // Provisional None: reading it needs `&self` (the deallocator),
            // same shape as read_knobs below.
            entry_pair: None,
            wants_trailing_exit: trailing_exit_fn
                .map(|f| {
                    // SAFETY: valid handle; the symbol is a pure integer read.
                    // The verdict decodes through the shared capability model
                    // (issue #843): any value outside {-1, 0, 1} is "no
                    // declaration", so a garbage int can never read as a Trail.
                    blitzkrieg_strategy_api::TrailingExitVerdict::from_abi_int(unsafe { f(handle) })
                })
                .unwrap_or(blitzkrieg_strategy_api::TrailingExitVerdict::FollowKernel),
            atomic_batch: atomic_batch_fn
                .map(|f| -> bool {
                    // SAFETY: valid handle; a pure integer read.
                    unsafe { f(handle) != 0 }
                })
                .unwrap_or(false),
            exit_intents: Vec::new(),
            breaks: Vec::new(),
            params: None,
            last_hot_json: None,
            assets: HashMap::new(),
        };
        // Read the declaration once: it is a load-time property, and the load
        // report must be able to state it (E2-c: "not evolvable" is explicit).
        s.knobs = s.read_knobs();
        // #642: the pair-declaration default, same read-once rule. Absent
        // symbol / null / malformed JSON = no default (never a guess).
        s.entry_pair = s.read_entry_pair();
        s
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    /// The knobs this library declared evolvable at load time.
    pub fn declared_knobs(&self) -> &[KnobSpec] {
        &self.knobs
    }

    /// Take a JSON heap string returned by the library and copy it into Rust,
    /// freeing it through the library's own deallocator. None on null/UTF-8
    /// error (a malformed output is treated as "empty", never a crash).
    unsafe fn take_json(&self, p: *mut c_char) -> Option<serde_json::Value> {
        if p.is_null() {
            return None;
        }
        // SAFETY: pointer came from this library's CString::into_raw; copy then
        // free once via the matching allocator.
        let parsed = unsafe { CStr::from_ptr(p) }
            .to_str()
            .ok()
            .and_then(|s| serde_json::from_str(s).ok());
        if let Some(f) = self.free_string {
            // SAFETY: same provenance as above.
            unsafe { f(p) };
        }
        parsed
    }

    /// Take a heap string returned by the library and copy it into Rust as-is,
    /// freeing it through the library's own deallocator. None on null/UTF-8
    /// error (a malformed output is treated as "empty", never a crash).
    unsafe fn take_raw_string(&self, p: *mut c_char) -> Option<String> {
        if p.is_null() {
            return None;
        }
        // SAFETY: pointer came from this library's CString::into_raw; copy then
        // free once via the matching allocator.
        let s = unsafe { CStr::from_ptr(p) }
            .to_str()
            .ok()
            .map(str::to_string);
        if let Some(f) = self.free_string {
            // SAFETY: same provenance as above.
            unsafe { f(p) };
        }
        s
    }

    /// E-parity: the config currently in force, reported through the library's
    /// OPTIONAL `bk_strategy_config_view` symbol. Missing symbol, null return
    /// or malformed UTF-8 all mean "declares nothing" — the same default an
    /// in-tree strategy gets from the trait default.
    pub fn read_config_view(&self) -> Option<String> {
        let f = self.config_view_fn?;
        // SAFETY: valid handle; the returned string is owned by the library and
        // freed through its own deallocator by take_raw_string.
        unsafe { self.take_raw_string(f(self.handle)) }
    }

    /// Read (and validate) the optional knob declaration. Malformed JSON, a null
    /// return or a missing symbol all mean "declares nothing" — never a panic and
    /// never a partially trusted declaration (incoherent specs are dropped).
    fn read_knobs(&self) -> Vec<KnobSpec> {
        let Some(f) = self.evolvable_knobs_fn else {
            return Vec::new();
        };
        // SAFETY: valid handle; the returned JSON is owned by the library and
        // freed through the library's own deallocator by take_json.
        let text = unsafe { self.take_json(f(self.handle)) }.map(|v| v.to_string());
        match text {
            Some(t) => KnobDeclaration::parse(&t).knobs,
            None => Vec::new(),
        }
    }

    /// #642: read the OPTIONAL `bk_strategy_entry_pair` declaration — the
    /// pair-declaration DEFAULT applied by [`adopt_eval_json`] to entries
    /// without their own per-entry `pair`. Absent symbol / NULL return /
    /// malformed JSON / missing fields = `None` ("no default", the old
    /// behaviour), never a partially trusted declaration.
    fn read_entry_pair(&self) -> Option<crate::signal::TradePairDecl> {
        let f = self.lib.entry_pair_fn?;
        // SAFETY: valid handle; the returned JSON is owned by the library and
        // freed through its own deallocator by take_json.
        let v = unsafe { self.take_json(f(self.handle)) }?;
        let obj = v.as_object()?;
        // Both labels are readable strings carried verbatim; a wrong-typed
        // field degrades to "no default" (never a guessed label).
        let key = obj.get("key").and_then(|k| k.as_str())?.to_string();
        let leg = obj.get("leg").and_then(|l| l.as_str())?.to_string();
        Some(crate::signal::TradePairDecl { key, leg })
    }

    /// Push a parameter set to the library's `on_hot_params`, deduplicating
    /// against the last JSON sent. The payload is THIS strategy's own bag of
    /// knob values (decimal strings), not another strategy's field list.
    fn push_params_json(&mut self, params: &StrategyParams) {
        let Some(f) = self.vtable.on_hot_params else {
            return;
        };
        let Ok(json) = serde_json::to_string(params) else {
            return;
        };
        if self.last_hot_json.as_deref() == Some(json.as_str()) {
            return;
        }
        if let Ok(cs) = CString::new(json.clone()) {
            // SAFETY: valid handle + borrowed NUL string for the call.
            unsafe { f(self.handle, cs.as_ptr()) };
        }
        self.last_hot_json = Some(json);
    }

    fn push_hot_params_if_changed(&mut self) {
        let Some(cell) = &self.params else { return };
        let params = (**cell.load()).clone();
        self.push_params_json(&params);
    }

    /// Apply a parameter set immediately, bypassing the registry cell. Used to
    /// seed a shadow twin with its counterfactual values before it is driven.
    pub(super) fn apply_params_direct(&mut self, params: &StrategyParams) {
        self.last_hot_json = None;
        self.push_params_json(params);
    }

    /// Bind the evaluation context (round view + priceable books) to the
    /// library for the duration of ONE hook call, if the library opted in via
    /// the OPTIONAL `bk_strategy_bind_eval_ctx` symbol. The returned rows/views
    /// own everything the context points at; the caller must keep them alive
    /// until after the FFI call and then invoke [`Self::unbind_ctx`].
    fn bind_ctx(
        &self,
        marshal: &EvalCtxMarshal,
        round: BkRound,
        markets: *const BkMarket,
        market_count: usize,
    ) -> Option<BoundCtx> {
        let f = self.bind_eval_ctx_fn?;
        let (ctx, rows, views) = marshal.build(round, markets, market_count);
        // SAFETY: valid handle; `ctx` borrows `rows`/`views`, which the caller
        // keeps alive until the matching unbind.
        unsafe { f(self.handle, &ctx) };
        Some((ctx, rows, views))
    }

    /// Unbind whatever context is bound (harmless when the library has no
    /// binder or nothing is bound).
    fn unbind_ctx(&self) {
        if let Some(f) = self.bind_eval_ctx_fn {
            // SAFETY: valid handle; null only ends the borrow window.
            unsafe { f(self.handle, std::ptr::null()) };
        }
    }

    /// Bind the context, run `call` (the FFI evaluate/diagnostics hook), and
    /// guarantee the unbind (bind null) happens on EVERY path — including a
    /// panic. The `BkEvalCtx` the library sees borrows from host-owned storage
    /// that is freed when the guard drops; leaving it bound across that free
    /// would be a use-after-free waiting for the next hook call. A panicking
    /// hook is caught, the guard dropped (unbind first, rows/views after), and
    /// the panic RESUMED, so kernel-level panic semantics are exactly what they
    /// were before the binder existed. (A library built with the plain "C"
    /// unwind convention aborts inside its own frame before this can catch —
    /// this guard is the belt to that suspenders, covering host-side marshalling
    /// panics and C-unwind libraries.)
    fn call_with_bound_ctx<T>(
        &self,
        marshal: &EvalCtxMarshal,
        round: BkRound,
        markets: *const BkMarket,
        market_count: usize,
        call: impl FnOnce() -> T,
    ) -> T {
        let bound = self.bind_ctx(marshal, round, markets, market_count);
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(call));
        self.unbind_ctx();
        drop(bound);
        match out {
            Ok(v) => v,
            Err(e) => std::panic::resume_unwind(e),
        }
    }
}

// SAFETY: the loaded instance is driven only by the kernel's single strategy
// loop. The vtable is static; the opaque handle is owned exclusively here.
unsafe impl Send for ForeignStrategy {}
unsafe impl Sync for ForeignStrategy {}

impl EngineStrategy for ForeignStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    /// E2-b / #27: read the library's OPTIONAL `bk_strategy_gate_exemptions`
    /// symbol. A library that does not export it, returns null, or returns
    /// malformed JSON declares nothing and stays fully gated — the same default
    /// an in-tree strategy gets from the trait.
    fn gate_exemptions(&self) -> GateExemptions {
        let Some(f) = self.gate_exemptions_fn else {
            return GateExemptions::none();
        };
        // SAFETY: valid handle; the returned JSON string is owned by the library
        // and freed through the library's own deallocator by take_json.
        let v = unsafe { self.take_json(f(self.handle)) };
        v.as_ref()
            .map(GateExemptions::from_json)
            .unwrap_or_default()
    }

    /// E2-c / #28: the knobs this library declared evolvable (empty ⇒ not
    /// evolvable). Resolved once at load; a v2 library without the optional
    /// symbol is simply not evolvable, with no ABI bump required.
    fn evolvable_knobs(&self) -> Vec<KnobSpec> {
        self.knobs.clone()
    }

    fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
        if self.knobs.is_empty() {
            return None; // explicit "not evolvable"
        }
        Some(Box::new(ForeignShadowFactory {
            lib: self.lib.clone(),
            name: self.name.clone(),
            knobs: self.knobs.clone(),
        }))
    }

    /// E-parity: the library's own config-in-force view, via the OPTIONAL
    /// `bk_strategy_config_view` symbol ("nothing declared" when absent).
    fn config_view_json(&self) -> Option<String> {
        self.read_config_view()
    }

    /// Hold-to-settlement declaration, via the library's OPTIONAL
    /// `bk_strategy_settlement_holds` symbol. Missing symbol = not declared =
    /// the trait default (false) — every existing library keeps its exits.
    fn holds_to_settlement(&self) -> bool {
        let Some(f) = self.settlement_holds_fn else {
            return false;
        };
        // SAFETY: valid handle; the symbol is a pure read of library state.
        unsafe { f(self.handle) != 0 }
    }

    /// #642: the three-state trailing verdict, read ONCE at load (a property
    /// of the library build). Absent symbol = `FollowKernel` = follow the
    /// kernel-wide `risk.setExit` trailingEnabled value, bit-identically to
    /// every existing library. The shared enum converts to the trait's
    /// `Option<bool>` in ONE place (`to_option_bool`).
    fn wants_trailing_exit(&self) -> Option<bool> {
        self.wants_trailing_exit.to_option_bool()
    }

    /// #642: the E-VA atomic-batch declaration, read ONCE at load. Absent
    /// symbol = false = the per-order path, bit-identically to every existing
    /// library.
    fn atomic_batch(&self) -> bool {
        self.atomic_batch
    }

    /// #642: the pair-declaration DEFAULT read once at load (see the field
    /// doc). Applied by [`adopt_eval_json`] to entries that carry no
    /// per-entry `pair`; `None` = no default = the old unpaired path.
    fn entry_pair(&self) -> Option<crate::signal::TradePairDecl> {
        self.entry_pair.clone()
    }

    fn on_book(&mut self, token_id: &str, snap: &OrderbookSnapshot, now_ms: i64) {
        let Some(f) = self.vtable.on_book else { return };
        // on_book carries only a token; label the view with the asset resolved
        // from the last round's markets (falls back to the token itself).
        let asset = self
            .assets
            .get(token_id)
            .map(|s| s.as_str())
            .unwrap_or(token_id);
        let Some(m) = BookMarshal::new(token_id, asset, snap) else {
            return;
        };
        // These raw-pointer arrays borrow from `m`; bind and hold them (along
        // with `m`) across the FFI call so nothing is freed underneath it.
        let (bid_levels, ask_levels) = m.level_views();
        let view = m.view(&bid_levels, &ask_levels, now_ms);
        // SAFETY: valid handle and a live view whose backing outlives the call.
        unsafe { f(self.handle, &view) };
    }

    fn on_round(&mut self, slot: i64, time_left_sec: i64, now_ms: i64) {
        if let Some(f) = self.vtable.on_round {
            // E-parity: the round's real timing, not zeros. The trait contract
            // promises the same context an in-tree strategy sees; the seconds
            // remaining and the host clock are part of that.
            let round = BkRound {
                slot,
                time_left_sec,
                now_ms,
            };
            // SAFETY: valid handle + borrowed round for the call.
            unsafe { f(self.handle, &round) };
        }
    }

    /// #642: forward CLOSED bars through the OPTIONAL `bk_strategy_on_kline`
    /// symbol — the same dispatch the Lua side gets (§10.4: CLOSED bars only,
    /// in closure order; the engine's on_kline path already guarantees both).
    /// The view is a borrowed decimal-STRING mirror of the shared `Kline` the
    /// Lua `bk_on_kline` table sees; missing symbol = never called.
    fn on_kline(&mut self, kline: &crate::kline::Kline) {
        let Some(f) = self.on_kline_fn else {
            return;
        };
        let cs = |d: Decimal| CString::new(d.to_string()).unwrap_or_default();
        let Ok(symbol) = CString::new(kline.symbol.as_str()) else {
            return;
        };
        let interval = CString::new(kline.interval.wire_name()).unwrap_or_default();
        let open = cs(kline.open);
        let high = cs(kline.high);
        let low = cs(kline.low);
        let close = cs(kline.close);
        let volume = cs(kline.volume);
        let view = blitzkrieg_strategy_api::BkKlineView {
            symbol: symbol.as_ptr(),
            interval: interval.as_ptr(),
            open_time_ms: kline.open_time_ms,
            close_time_ms: kline.close_time_ms,
            open: open.as_ptr(),
            high: high.as_ptr(),
            low: low.as_ptr(),
            close: close.as_ptr(),
            volume: volume.as_ptr(),
            trade_count: kline.trade_count,
            is_closed: u8::from(kline.is_closed),
        };
        // SAFETY: valid handle + a borrowed view whose backing outlives the
        // call (every CString above lives to the end of this function).
        unsafe { f(self.handle, &view) };
    }

    fn take_breaks(&mut self) -> Vec<(String, Decimal)> {
        // Merge any breaks the evaluate() JSON reported with the library's own
        // drained take_breaks hook output.
        if let Some(f) = self.vtable.take_breaks {
            // SAFETY: hook returns a heap JSON array owned by the library.
            if let Some(v) = unsafe { self.take_json(f(self.handle)) } {
                parse_breaks(&v)
                    .into_iter()
                    .for_each(|b| self.breaks.push(b));
            }
        }
        std::mem::take(&mut self.breaks)
    }

    fn confirmed_tokens(&self) -> HashSet<String> {
        let Some(f) = self.vtable.confirmed_tokens else {
            return HashSet::new();
        };
        // SAFETY: hook returns a heap JSON array owned by the library.
        match unsafe { self.take_json(f(self.handle)) } {
            Some(serde_json::Value::Array(a)) => a
                .into_iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
            _ => HashSet::new(),
        }
    }

    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        let Some(evaluate) = self.vtable.evaluate else {
            return Vec::new();
        };
        self.push_hot_params_if_changed();

        // Refresh token→asset for on_book labelling between evaluations.
        for m in ctx.markets() {
            self.assets.insert(m.up_token_id.clone(), m.asset.clone());
            self.assets.insert(m.down_token_id.clone(), m.asset.clone());
        }

        // Backing storage for the market rows must live across the FFI call.
        struct Rows {
            strs: Vec<(CString, CString, CString, CString, CString, CString)>,
            view: Vec<BkMarket>,
        }
        let mut rows = Rows {
            strs: Vec::new(),
            view: Vec::new(),
        };
        for m in ctx.markets() {
            let cs = |s: &str| CString::new(s).unwrap_or_default();
            rows.strs.push((
                cs(&m.asset),
                cs(&m.condition_id),
                cs(&m.question_id),
                cs(&m.up_token_id),
                cs(&m.down_token_id),
                cs(&m.venue),
            ));
        }
        rows.view = rows
            .strs
            .iter()
            .zip(ctx.markets().iter())
            .map(|(s, m)| BkMarket {
                asset: s.0.as_ptr(),
                condition_id: s.1.as_ptr(),
                question_id: s.2.as_ptr(),
                up_token: s.3.as_ptr(),
                down_token: s.4.as_ptr(),
                expires_at_ms: m.expires_at_ms,
                slot: m.round_slot,
                neg_risk: if m.neg_risk { 1 } else { 0 },
                venue: s.5.as_ptr(),
            })
            .collect();

        let round = BkRound {
            slot: ctx.round_slot(),
            time_left_sec: ctx.time_left_sec(),
            now_ms: ctx.now_ms(),
        };
        let rv = BkRoundView {
            round,
            markets: if rows.view.is_empty() {
                std::ptr::null()
            } else {
                rows.view.as_ptr()
            },
            market_count: rows.view.len(),
        };

        // E-parity: bind the evaluation context (round view + the PRICEABLE
        // books, i.e. exactly what `ctx.fresh_book` would answer) for the
        // duration of this evaluate call. The marshal owns every CString the
        // context points at and outlives the FFI call below; the unbind runs on
        // every path, panic included (see call_with_bound_ctx).
        let eval_marshal = EvalCtxMarshal::new(ctx.markets(), &ctx.fresh_book, ctx.now_ms());

        // SAFETY: valid handle and a round view whose backing lives to end of
        // scope; the returned JSON is copied and freed via the library.
        let out = self.call_with_bound_ctx(
            &eval_marshal,
            round,
            rv.markets,
            rv.market_count,
            || unsafe { evaluate(self.handle, &rv) },
        );
        let Some(v) = (unsafe { self.take_json(out) }) else {
            return Vec::new();
        };
        // The JSON→intents adoption lives in a FREE function so the §2.4
        // reserved-key seal is testable without a loaded library — reverse
        // acceptance B drives it directly (`reserved_key_tests` below).
        adopt_eval_json(
            &self.name,
            &self.account_id,
            v,
            ctx,
            &mut self.exit_intents,
            &mut self.breaks,
            self.entry_pair.as_ref(),
        )
    }

    fn take_exit_intents(&mut self) -> Vec<StrategyExitIntent> {
        std::mem::take(&mut self.exit_intents)
    }

    fn diagnostics(&self, ctx: &StrategyCtx<'_>) -> Vec<serde_json::Value> {
        let Some(f) = self.vtable.diagnostics else {
            return Vec::new();
        };
        // E-parity: the diagnostics hook sees the same bound context its
        // evaluate sees, so a diagnostic can report freshness-gated values
        // exactly like an in-tree strategy's `diagnostics(&ctx)` does.
        let round = BkRound {
            slot: ctx.round_slot(),
            time_left_sec: ctx.time_left_sec(),
            now_ms: ctx.now_ms(),
        };
        // Backing for the market rows, mirroring find_candidates.
        let mut strs: Vec<(CString, CString, CString, CString, CString, CString)> = Vec::new();
        for m in ctx.markets() {
            let cs = |s: &str| CString::new(s).unwrap_or_default();
            strs.push((
                cs(&m.asset),
                cs(&m.condition_id),
                cs(&m.question_id),
                cs(&m.up_token_id),
                cs(&m.down_token_id),
                cs(&m.venue),
            ));
        }
        let view: Vec<BkMarket> = strs
            .iter()
            .zip(ctx.markets().iter())
            .map(|(s, m)| BkMarket {
                asset: s.0.as_ptr(),
                condition_id: s.1.as_ptr(),
                question_id: s.2.as_ptr(),
                up_token: s.3.as_ptr(),
                down_token: s.4.as_ptr(),
                expires_at_ms: m.expires_at_ms,
                slot: m.round_slot,
                neg_risk: if m.neg_risk { 1 } else { 0 },
                venue: s.5.as_ptr(),
            })
            .collect();
        let markets_ptr = if view.is_empty() {
            std::ptr::null()
        } else {
            view.as_ptr()
        };
        let eval_marshal = EvalCtxMarshal::new(ctx.markets(), &ctx.fresh_book, ctx.now_ms());
        // SAFETY: hook returns a heap JSON array owned by the library; the
        // bound context (if any) borrows from `eval_marshal`/`bound`, which
        // outlive the call (unbind runs on every path, panic included).
        let raw =
            self.call_with_bound_ctx(&eval_marshal, round, markets_ptr, view.len(), || unsafe {
                f(self.handle)
            });
        let parsed = unsafe { self.take_json(raw) };
        match parsed {
            Some(serde_json::Value::Array(a)) => a,
            _ => Vec::new(),
        }
    }

    fn set_hot_params(&mut self, registry: Option<Arc<ParamRegistry>>) {
        // Resolve only OUR cell: a library that declared no knobs has no cell and
        // therefore receives no hot parameters at all (explicit "not evolvable").
        // `None` detaches: the library then runs on the config the kernel pushed
        // through `on_config`, which is the pre-evolution behaviour exactly.
        let cell = registry.as_ref().and_then(|r| r.handle_for(&self.name));
        // Reset the dedup marker so a fresh cell always pushes once.
        self.last_hot_json = None;
        self.params = cell;
        // An already-published value is delivered on the next evaluation, not
        // here: the push is issued from the hot path so it happens at most once
        // per actual change.
    }

    fn on_config(&mut self, config: &serde_json::Value) {
        let Some(f) = self.vtable.on_config else {
            return;
        };
        // RFC-645 A-2: the kernel already marshalled the ONE projection
        // (`engine_strategy_config_json`, engine.rs) — the exact bytes this
        // site used to build inline (the golden pin lives in
        // `engine_strategy_config_json_is_the_foreign_marshal_golden`,
        // test_support.rs). This is now a PASS-THROUGH: serialize what the
        // trait handed us, no re-marshal, so builtin adapters and dylibs see
        // byte-identical config pushes by construction.
        if let Ok(cs) = CString::new(config.to_string()) {
            // SAFETY: valid handle + borrowed JSON for the call; the library's
            // accept/reject return is advisory (kernel keeps driving either way).
            let _ = unsafe { f(self.handle, cs.as_ptr()) };
        }
    }
}

impl Drop for ForeignStrategy {
    fn drop(&mut self) {
        if let Some(destroy) = self.vtable.destroy {
            // SAFETY: handle came from this library's create(). Destroying it
            // here, before the `Arc<LoadedLibrary>` field is released, is what
            // guarantees the library is still mapped when its code runs.
            unsafe { destroy(self.handle) };
        }
    }
}
