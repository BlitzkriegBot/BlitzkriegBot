//! Strategy loading/unloading and the Lua package scanner
//! (`load_strategy_lib`, `load_strategy_lib_scan`, `load_strategy_lib_inner`,
//! `load_lua_package_inner`, `reload_lua_packages`, `unload_strategy`,
//! `reload_strategy`). Moved verbatim from `service/strategies.rs` (pure code
//! motion).

use crate::exit_policy::fee_schedule;
use crate::strategy_engine::lua_loader::{
    LuaEngineAdapter, discover_lua_packages, load_lua_package, try_load_native_blueprint_package,
};
use blitzkrieg_lua_runtime::FeeScheduleView;
use blitzkrieg_market_api::net::now_ms;
use std::collections::HashSet;
use std::path::Path;

use super::Core;
use crate::service::helpers::ARB_STRATEGY;

impl Core {
    pub fn load_strategy_lib(&mut self, path: &str) -> String {
        self.load_strategy_lib_inner(path, true)
    }

    /// The startup scan path (`load_strategy_dir`). §8.2 row 1: the scan only
    /// LOGS what was declared — it never refuses. An incompatible library
    /// still registers (disabled) with the mismatch named on its receipt;
    /// the enable handshake (both `strategy.enable` and the startup
    /// `--enable-strategy` loop) is the gate.
    pub(crate) fn load_strategy_lib_scan(&mut self, path: &str) -> String {
        self.load_strategy_lib_inner(path, false)
    }

    pub(crate) fn load_strategy_lib_inner(&mut self, path: &str, enforce: bool) -> String {
        #[cfg(feature = "strategy-loading")]
        {
            use crate::strategy_engine::loader::load_foreign;
            let p = Path::new(path);
            // E30 (§6.4): a DIRECTORY is a Lua package, never a dylib — route
            // it through the Lua loader so the two stacks cannot load each
            // other's artifacts (the dylib scanner owns `*.dylib`, the Lua
            // loader owns `manifest.json` packages).
            if p.is_dir() {
                return self.load_lua_package_inner(p, enforce);
            }
            let Some(engine) = self.engine.as_mut() else {
                return "Failed: strategy engine not attached (load libraries after engine init)"
                    .into();
            };
            match load_foreign(p) {
                Ok(loaded) => {
                    let name = loaded.name.clone();
                    let version = loaded.version.clone();
                    // E27 (§8.2): the handshake verdict BEFORE registration.
                    // ENFORCE (the IPC `strategy.load` path): a library whose
                    // declaration the active plugin cannot serve is refused,
                    // not registered-and-silently-unable-to-enable. SCAN (the
                    // startup `strategy_dir` walk): §8.2 row 1 — the scan only
                    // logs, so the library registers disabled and the receipt
                    // itself names the mismatch; the enable handshake is the
                    // gate there. Either way the text names both sides (R-B).
                    let refusal =
                        Self::modes_refusal(&self.plugin_modes, &name, &loaded.declared_modes);
                    if enforce && let Some(refusal) = refusal {
                        // `refusal` already opens with "incompatible modes:" and
                        // names both sides (§8.2) — prefix only the verdict tag.
                        return format!("Failed: {refusal}");
                    }
                    let incompatible = match refusal {
                        Some(r) => format!("; INCOMPATIBLE: {r}"),
                        None => String::new(),
                    };
                    // E2-b: the load receipt names the gates the library declared
                    // unnecessary, so an opt-out is visible before it is enabled.
                    // D-31: if the declaration carries a `time_left_sec` floor,
                    // state it — the receipt is the operator's only pre-enable
                    // view of how far the opt-out actually reaches.
                    let declared = if loaded.gate_exemptions.any() {
                        let floor = match loaded.gate_exemptions.timing_min_time_left_sec {
                            Some(n) => format!(" (timing floor {n}s)"),
                            None => String::new(),
                        };
                        format!(
                            "; declares gate exemptions: {}{floor}",
                            loaded.gate_exemptions.gates().join(",")
                        )
                    } else {
                        String::new()
                    };
                    // E2-c: the same receipt names the evolvable knobs, so "not
                    // evolvable" (no optional symbol) is distinguishable at load
                    // from "evolvable but nothing applied yet".
                    let evolvable = if loaded.evolvable_knobs.is_empty() {
                        "; not evolvable (no knobs declared)".to_string()
                    } else {
                        let names: Vec<&str> = loaded
                            .evolvable_knobs
                            .iter()
                            .map(|k| k.name.as_str())
                            .collect();
                        format!("; declares evolvable knobs: {}", names.join(","))
                    };
                    match engine.register_user_strategy(
                        Box::new(loaded.strategy),
                        format!("dylib:{}", p.display()),
                    ) {
                        Ok(_) => {
                            // E27: the declaration is recorded only on a
                            // successful registration, so `strategy.list` and
                            // the enable-time handshake describe live dispatch
                            // members and nothing else.
                            self.strategy_declarations
                                .insert(name.clone(), loaded.declared_modes.clone());
                            // The new strategy's declaration must reach the manager
                            // too: a library loaded AFTER evolution was enabled would
                            // otherwise never get a unit, so it could not evolve at
                            // all until a restart.
                            self.rewire_hot_params(now_ms());
                            // API 1.0 naming (DEV_V0_3 §2.2, E24 #330): the
                            // receipt KEEPS its old text — the test above pins
                            // it — and only APPENDS the protocol line. E24
                            // handed this hunk to the service.rs hotspot
                            // guardian (E25); it lands here.
                            format!(
                                "{name}@{version} registered into the engine dispatch (disabled{declared}{evolvable}{incompatible}); API 1.0 (line protocol 2)"
                            )
                        }
                        Err(reason) => format!("rejected: {reason}"),
                    }
                }
                Err(outcome) => format!("{outcome:?}"),
            }
        }
        #[cfg(not(feature = "strategy-loading"))]
        {
            let outcome = crate::strategy_engine::loader::load_strategy(Path::new(path));
            format!("{outcome:?}")
        }
    }

    /// E30 (#336): load one Lua strategy package (a directory carrying
    /// `manifest.json`) and register it into the live dispatch. The E27
    /// handshake applies UNCHANGED — a Lua declaration the active plugin
    /// cannot serve is refused in ENFORCE mode (IPC `strategy.load`) or
    /// registered-disabled-with-verbatim-receipt in SCAN mode (the startup
    /// walk), byte-identical to the dylib rows (§8.2). The receipt carries
    /// the `(lua)` marker so an operator sees the runtime at a glance
    /// (§6.6: 回执含 `(lua)` 标记). A poisoning breach inside the sandbox is
    /// reported on the receipt; it is not a load failure (the strategy is
    /// alive, it just refused to run).
    pub(crate) fn load_lua_package_inner(
        &mut self,
        dir: &std::path::Path,
        enforce: bool,
    ) -> String {
        let Some(engine) = self.engine.as_mut() else {
            return "Failed: strategy engine not attached (load libraries after engine init)"
                .into();
        };
        if let Some(native_res) = try_load_native_blueprint_package(dir) {
            return match native_res {
                Ok(loaded) => {
                    let name = loaded.name.clone();
                    let version = loaded.version.clone();
                    let refusal =
                        Self::modes_refusal(&self.plugin_modes, &name, &loaded.declared_modes);
                    if enforce && let Some(refusal) = refusal {
                        return format!("Failed: {refusal}");
                    }
                    let incompatible = match refusal {
                        Some(r) => format!("; INCOMPATIBLE: {r}"),
                        None => String::new(),
                    };
                    match engine.register_user_strategy(
                        Box::new(loaded.strategy),
                        format!("blueprint:{}", dir.display()),
                    ) {
                        Ok(_) => {
                            if !loaded.declared_modes.is_empty() {
                                self.strategy_declarations
                                    .insert(name.clone(), loaded.declared_modes.clone());
                            }
                            self.rewire_hot_params(now_ms());
                            format!(
                                "{name}@{version} (rust-blueprint) registered into the engine dispatch (disabled{incompatible})"
                            )
                        }
                        Err(reason) => format!("rejected: {reason}"),
                    }
                }
                Err(reason) => format!("Failed: {reason}"),
            };
        }
        match load_lua_package(dir) {
            Ok(loaded) => {
                let name = loaded.name.clone();
                let version = loaded.version.clone();
                let tunables = loaded.tunables.clone();
                let holds = loaded.holds_to_settlement;
                let exemptions = loaded.gate_exemptions;
                let atomic = loaded.atomic_batch;
                let trailing = loaded.trailing_exit;
                let entry_maker = loaded.entry_maker;
                // E27 (§8.2): the SAME handshake verdict BEFORE registration.
                let refusal =
                    Self::modes_refusal(&self.plugin_modes, &name, &loaded.declared_modes);
                if enforce && let Some(refusal) = refusal {
                    return format!("Failed: {refusal}");
                }
                let incompatible = match refusal {
                    Some(r) => format!("; INCOMPATIBLE: {r}"),
                    None => String::new(),
                };
                // The fee projection (`bk.fees()`): the kernel's ONE schedule,
                // stamped at load so a strategy prices the fee from the same
                // curve the charge path settles in.
                let sched = fee_schedule();
                let fee_view = FeeScheduleView {
                    name: sched.name.to_string(),
                    rate: sched.rate.to_string(),
                    exponent: sched.exponent,
                };
                let adapter = LuaEngineAdapter::new(loaded.strategy, tunables)
                    .declare(holds, exemptions)
                    .declare_atomic_batch(atomic)
                    .declare_trailing_exit(trailing)
                    .declare_entry_maker(entry_maker)
                    // RFC-645 A-1: the ONE arbiter package declares the
                    // pair-book statistics surface (决议②: the loader answers
                    // for the Lua side); every other package keeps the
                    // `false` default. The engine's acceptance predicate
                    // still gates the row's arb* fields.
                    .declare_pair_book_stats(name == ARB_STRATEGY);
                adapter.set_fee_schedule(fee_view);
                match engine
                    .register_user_strategy(Box::new(adapter), format!("lua:{}", dir.display()))
                {
                    Ok(_) => {
                        // E30: an ABSENT manifest.modes stays absent — an empty
                        // Vec would render `modes: []` on strategy.list, and
                        // §8.3's contract is `null = undeclared` (E27). Insert
                        // only a package that actually declared modes.
                        if !loaded.declared_modes.is_empty() {
                            self.strategy_declarations
                                .insert(name.clone(), loaded.declared_modes.clone());
                        }
                        self.rewire_hot_params(now_ms());
                        format!(
                            "{name}@{version} (lua) registered into the engine dispatch (disabled{incompatible})"
                        )
                    }
                    Err(reason) => format!("rejected: {reason}"),
                }
            }
            Err(reason) => format!("Failed: {reason}"),
        }
    }

    /// Re-scan the Lua strategy dir (`user_layer/strategies_lua`) and register
    /// any package not already in the engine's provenance ledger
    /// (`package_sources`). No disk write — pure re-scan plus in-memory
    /// registration. The non-restart recovery path for a package refused at
    /// boot (e.g. a stale manifest sha that has since been corrected): after
    /// reload, `strategy.list` shows it again and `account.strategies.set` can
    /// add it back (issue #975). Returns the names freshly loaded.
    pub fn reload_lua_packages(&mut self) -> Vec<String> {
        let Some(dir) = self.config.lua_strategy_dir.as_deref() else {
            return Vec::new();
        };
        let Some(engine) = self.engine.as_mut() else {
            return Vec::new();
        };
        let already: HashSet<String> = engine.scope_sources().into_iter().map(|(n, _)| n).collect();
        let mut loaded: Vec<String> = Vec::new();
        for pkg in discover_lua_packages(Path::new(dir)) {
            let name = match pkg.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if already.contains(&name) {
                continue;
            }
            let receipt = self.load_lua_package_inner(&pkg, false);
            if receipt.contains("registered into the engine dispatch") {
                loaded.push(name);
            }
        }
        loaded
    }

    /// Unload a dynamic strategy library (E9-b): drop its instance from the
    /// live dispatch and `dlclose` it. The engine refuses in-tree or still-
    /// enabled strategies; here we additionally refuse an OPEN POSITION held
    /// under this strategy's name — a strategy that still owns exposure is not
    /// removed from under the exit machinery. Auditable by design: the outcome
    /// string is what the operator sees on the IPC receipt.
    pub fn unload_strategy(&mut self, name: &str) -> String {
        let open = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.strategy == name)
            .count();
        if open > 0 {
            return format!(
                "rejected: {name} still holds {open} open position(s) — close them before unloading"
            );
        }
        match self
            .engine
            .as_mut()
            .map(|e| e.unregister_user_strategy(name))
        {
            Some(Ok(true)) => {
                // E27: the declaration leaves with the instance — the
                // handshake and `strategy.list` describe live members only.
                self.strategy_declarations.remove(name);
                format!("{name} unloaded (engine dispatch removed); library freed")
            }
            Some(Ok(false)) => format!("not found: no strategy named {name}"),
            Some(Err(reason)) => format!("rejected: {reason}"),
            None => "Failed: strategy engine not attached".into(),
        }
    }

    /// Load + swap in one call (E9-b `strategy.reload`): load the NEW library
    /// file first, and only when it parses and registers the SAME name does
    /// the old instance drop. A reload failure leaves the old instance live —
    /// a broken build can never blank a running dispatch. Because the swap is
    /// name-keyed, after-reload enable state is preserved.
    pub fn reload_strategy(&mut self, name: &str, path: &str) -> String {
        // 1. The name must be KNOWN to this session. A live instance names
        //    itself in the dispatch; a torn-down scope still names the
        //    PACKAGE in the engine's provenance ledger (issue 925: the
        //    default-account disable now tears the scope down outright — a
        //    devloader's disable→reload round-trip must survive that, so a
        //    provenance-only package reloads as a fresh load instead of
        //    dying "not found").
        let live = self
            .engine
            .as_ref()
            .map(|e| e.strategy_source(name).map(|s| s.to_string()))
            .unwrap_or(None);
        let packaged = self
            .engine
            .as_ref()
            .map(|e| e.strategy_registered(name))
            .unwrap_or(false);
        if !packaged {
            return format!("not found: no strategy named {name}");
        }
        let was_enabled = self.enabled_names_contains(name);
        // Temporary disable is NOT needed — the swap runs below; but a still-
        // enabled strategy cannot be swapped by the engine's guard, so the
        // load is tried first and the guard happens after.
        let open = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.strategy == name)
            .count();
        if open > 0 {
            return format!(
                "rejected: {name} still holds {open} open position(s) — a reload with exposure in flight is undefined; close first"
            );
        }

        // 2. Atomic swap: remember where the old instance came from BEFORE it
        //    is dropped. The engine refuses same-name registration while the
        //    old scope is live, so the old instance must come down first; if
        //    the new library then refuses to register, restore the old one
        //    from its recorded source so the session never ends up with
        //    neither instance (reload is a swap, not a destroy-and-hope).
        let old_source = live.clone();
        if live.is_some() {
            let old = self.unload_strategy(name);
            if !old.starts_with(name) && !old.contains("unloaded") {
                // rejected/Failed — refuse before loading new
                return format!("reload aborted: {old}");
            }
        }
        let receipt = self.load_strategy_lib(path);
        if !receipt.contains("registered") {
            // Roll back the old instance — a broken build must not blank a
            // running dispatch.
            if let Some(source) = old_source
                && let Some(old_path) = source
                    .strip_prefix("dylib:")
                    .or_else(|| source.strip_prefix("lua:"))
                    .or_else(|| source.strip_prefix("blueprint:"))
            {
                let restore = self.load_strategy_lib_inner(old_path, false);
                if restore.contains("registered") {
                    // Preserve previous enable state.
                    if was_enabled {
                        self.set_strategy_enabled(name, true);
                    }
                    return format!(
                        "reload FAILED (new library rejected, old instance restored): {receipt}"
                    );
                }
            }
            return format!("reload FAILED (old instance already dropped): {receipt}");
        }
        // Preserve previous enable state.
        if was_enabled {
            self.set_strategy_enabled(name, true);
        }
        let _ = live; // receipt text already carries the new source
        format!("reload OK: {receipt}")
    }

    pub(super) fn enabled_names_contains(&self, name: &str) -> bool {
        self.enabled_strategy_names().contains(&name.to_string())
    }
}
