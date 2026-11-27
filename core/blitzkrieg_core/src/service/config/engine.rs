//! `CoreConfig` behaviour: engine config mapping, share bands, plugin
//! selection, engine install, startup self-check, strategy auto-load, and
//! dylib discovery. Moved verbatim from `config.rs` (pure code motion,
//! no logic edits).

use super::CoreConfig;
use crate::engine::EngineConfig;
use crate::scanner::ScannerConfig;
use crate::service::Core;
use crate::signal::{SpreadArbConfig, TrendConfig};
use crate::strategy_engine::loader::{APPROVED_ROOTS, ENV_ALLOW_DIRS};
use rust_decimal::Decimal;
use std::path::Path;

impl CoreConfig {
    /// Build the engine config from this core config — the ONE mapping used by
    /// both the live server and the backtester, so a replay drives the identical
    /// engine a live run would (P-1.2).
    pub fn engine_config(&self) -> EngineConfig {
        EngineConfig {
            scanner: ScannerConfig {
                assets: self.assets.clone(),
                round_duration_sec: self.default_round_duration_sec,
                min_round_age_sec: self.min_round_age_sec,
                min_time_left_sec: self.positions.exit.min_time_left_sec,
            },
            trend: TrendConfig {
                confirm_sec: self.trend_confirm_sec,
                window_floor_ms: self.trend_window_floor_ms,
                ..Default::default()
            },
            spread_arb: {
                let mut sa = SpreadArbConfig {
                    trend_confirm_sec: self.trend_confirm_sec,
                    ..Default::default()
                };
                if let Some(v) = self.spread_arb_trend_entry_factor {
                    sa.trend_entry_factor = v;
                }
                if let Some(v) = self.spread_arb_entry_min_obi {
                    sa.entry_min_obi = v;
                }
                if let Some(v) = self.spread_arb_entry_max_spread_pct {
                    sa.entry_max_spread_pct = v;
                }
                if let Some(v) = self.spread_arb_entry_dip_max_pct {
                    sa.entry_dip_max_pct = v;
                }
                if let Some(v) = self.spread_arb_entry_bounce_min_pct {
                    sa.entry_bounce_min_pct = v;
                }
                if let Some(v) = self.spread_arb_entry_bounce_window_sec {
                    sa.entry_bounce_window_sec = v;
                }
                sa
            },
            size_usd: self.size_usd,
            min_shares: self.min_shares,
            max_shares: self.max_shares,
            size_pct: self.size_pct,
            account_size_pct: self.account_size_pct.clone(),
            account_min_shares: self.account_min_shares.clone(),
            account_max_shares: self.account_max_shares.clone(),
            // #205: the runtime freshness budget. It flows through this one
            // mapping, so a live server and a backtest replay see the same value.
            max_orderbook_stale_ms: self.max_orderbook_stale_ms,
            strategy_sizes: self
                .strategy_limits
                .iter()
                .map(|(name, limit)| (name.clone(), limit.sizing()))
                .collect(),
            ..Default::default()
        }
    }

    /// Issue #1189 (PR-D): the share band for one account — account row wins,
    /// a missing row falls back to the global `min_shares` / `max_shares`.
    pub fn share_band_for(&self, account_id: &str) -> (Decimal, Decimal) {
        (
            self.account_min_shares
                .get(account_id)
                .copied()
                .unwrap_or(self.min_shares),
            self.account_max_shares
                .get(account_id)
                .copied()
                .unwrap_or(self.max_shares),
        )
    }

    /// Issue #1189 (PR-D): the market plugin for one account — account row
    /// wins, a missing row falls back to the caller-supplied `fallback`
    /// (the `RUNTIME_MARKET_PLUGIN` env or a compiled default; resolved by
    /// the caller, never hard-coded here).
    pub fn market_plugin_for(&self, account_id: &str, fallback: &str) -> String {
        self.account_market_plugins
            .get(account_id)
            .cloned()
            .unwrap_or_else(|| fallback.to_string())
    }

    /// Install the engine described by this config (the live server's startup
    /// step, reused verbatim by the backtester).
    ///
    /// The strategy selection is applied through the same `set_strategy_enabled`
    /// the IPC method uses, so a session started with `--enable-strategy` and one
    /// toggled at runtime end up in exactly the same state (including the Shadow
    /// Evolution re-registration the toggle triggers). Afterwards the effective
    /// enabled-set is persisted (when `strategy_state_path` is configured) so
    /// the next boot replays it — the boot lists and the runtime toggles write
    /// the same file, and the last explicit intent wins.
    ///
    /// Returns `Err` when an EXPLICIT request resolved to zero live strategies
    /// (#265): the operator named strategies and not one of them exists, so the
    /// kernel would boot with a healthy-looking banner and trade nothing. The
    /// refusal belongs to the caller (the server ends the boot, the backtester
    /// fails the run) rather than to this method, so the facts stay inspectable
    /// and an embedded caller decides what a zero-strategy engine means to it.
    /// [`CoreConfig::allow_zero_strategies`] is the acknowledged opt-out.
    pub fn install_engine(&self, core: &mut Core) -> Result<(), String> {
        core.enable_engine(crate::engine::Engine::new(self.engine_config()));
        // Grid-pool membership and per-account anchors must come from Core's
        // config — the SQLite overlay's destination — not from `self`.
        // Session boot clones CoreConfig before `Core::new`; `install_risk_db`
        // then writes `account_round_durations` into `core.config()` only.
        // Seeding from `self` left the pool at the process default, so every
        // overlaid family was admitted as `NoMarkets`. The same source of
        // truth also covers re-assembly after `round_duration.set`.
        let live = core.config().clone();
        core.seed_scanner_grids(&live);
        // #1170 (方案 A): push the parsed family bindings into the engine
        // BEFORE the strategy packages load — registration stamps each scope
        // from the engine's table, so every load/reload/rebuild inherits the
        // binding without another push.
        core.apply_strategy_round_families(&self.strategy_round_families);
        core.apply_account_round_durations(&live.account_round_durations);
        // #297: SQLite risk-DB overlay wrote per-account sizing & share bands
        // into `core.config()` (not `&self`). Push them into the newly
        // installed engine so non-default accounts execute with their own
        // stored knobs from the first cycle.
        if let Some(e) = core.engine.as_mut() {
            for (acct, &pct) in &live.account_size_pct {
                e.set_account_size_pct(acct, pct);
            }
            let mut band_accounts: std::collections::HashSet<&str> = live
                .account_min_shares
                .keys()
                .chain(live.account_max_shares.keys())
                .map(String::as_str)
                .collect();
            for acct in band_accounts.drain() {
                let (min_s, max_s) = live.share_band_for(acct);
                e.set_account_share_band(acct, min_s, max_s);
            }
        }
        self.load_strategy_dir(core);
        self.load_lua_strategy_dir(core);
        for name in &self.enabled_strategies {
            // E27 (§8.2): the startup `--enable-strategy` request runs through
            // the SAME handshake as `strategy.enable`: a strategy the active
            // plugin cannot serve is SKIPPED with an ERROR line — never
            // enabled silently, never able to trade in a market it declared
            // itself unfit for.
            let (found, applied, refusal) = core.set_strategy_enabled_checked(name, true);
            if let Some(reason) = refusal {
                tracing::error!(
                    strategy = name,
                    reason = reason,
                    "startup enable refused by the market-mode handshake; strategy skipped"
                );
                continue;
            }
            if !found || !applied {
                tracing::warn!(
                    strategy = name,
                    "unknown strategy requested at startup; ignored"
                );
            }
        }
        for name in &self.disabled_strategies {
            if !core.set_strategy_enabled(name, false) {
                tracing::warn!(
                    strategy = name,
                    "unknown strategy requested at startup; ignored"
                );
            }
        }
        // The persisted enabled-set survives a plugin switch: a strategy the
        // HANDSHAKE refused at boot stays in `strategy-state.json` under its
        // name, so switching the market plugin back (or to any other plugin
        // that serves it) re-enables it with no manual re-toggling. What is
        // recorded is the operator's INTENT; the handshake gate keeps the
        // refused strategy OFF for this session without erasing the intent.
        // (2026-10: the first cut saved the effective set only, so one
        // plugin switch silently stripped every incompatible strategy from
        // the file — and switching back brought nothing back.) An explicit
        // `--disable-strategy` / runtime disable still removes the name:
        // that is a decision, not a market mismatch.
        if let Some(path) = &self.strategy_state_path {
            let mut enabled = core.enabled_strategy_names();
            for name in &self.enabled_strategies {
                if !enabled.contains(name) && !self.disabled_strategies.contains(name) {
                    let known = core.strategy_names().iter().any(|n| n == name);
                    if known {
                        // Registered but handshake-refused this boot: keep
                        // the intent so a future session can serve it.
                        enabled.push(name.clone());
                    }
                }
            }
            if let Err(e) = crate::strategy_state::save(std::path::Path::new(path), &enabled) {
                // #188 (R7.1 L4): a mirror that cannot be written is a
                // divergence the operator must hear about — the boot used the
                // set this line failed to record, so the NEXT boot replays a
                // stale one. SQLite stays the authority; the boot continues.
                tracing::error!(
                    path = %path,
                    error = %e,
                    "strategy state mirror NOT written at boot — the persisted \
                     enabled-set is now stale relative to what this core is running; \
                     SQLite account_settings.strategies remains the authority"
                );
            }
        }
        // ④ 统一映射 + 跨所套利腿配置：install_engine 换掉了整个 Engine（启动、
        // 插件切换、进化重注册），Engine 内存里的 confirmed 映射与 arb_leg_config
        // 会一起丢 —— Core 持有的账本/配置是唯一真值源，每次安装后回灌一遍。
        if let Some(e) = core.engine.as_mut() {
            for event in core.unified_ledger.events() {
                e.put_unified_mapping(event.clone());
            }
            if core.arb_leg_config.is_some() {
                e.set_arb_leg_config(core.arb_leg_config.clone());
            }
        }
        self.strategy_startup_self_check(core)
    }

    /// The startup self-check (#265): state what was REQUESTED, what RESOLVED and
    /// what is enabled, and refuse a boot that resolves an explicit request to
    /// nothing.
    ///
    /// The line is printed unconditionally, refusals included, because it is the
    /// contract `scripts/walk-forward-sweep.mjs` reads back out of the kernel's
    /// log: a replay that could not resolve the strategies it asked for must be
    /// recognisable as such rather than arriving as a report with zero trades
    /// ("the strategy had no signal" and "there was no strategy" have to be
    /// different answers).
    ///
    /// A name the operator ALSO disabled explicitly is not part of the request —
    /// `--enable-strategy x --disable-strategy x` asked for nothing — so it can
    /// never be the reason this refuses.
    ///
    /// "Resolved" is asked of the live registry (`Core::strategy_names`), not of
    /// the enable loop: the question is whether the name exists as a strategy in
    /// this process, which is exactly what a missing library makes false. The
    /// persisted set is deliberately not consulted — a name that no longer
    /// resolves there was never requested by this invocation.
    pub(super) fn strategy_startup_self_check(&self, core: &Core) -> Result<(), String> {
        let requested: Vec<&str> = self
            .requested_strategies
            .iter()
            .map(String::as_str)
            .filter(|n| !self.disabled_strategies.iter().any(|d| d.as_str() == *n))
            .collect();
        let registered = core.strategy_names();
        let resolved = requested
            .iter()
            .filter(|n| registered.iter().any(|r| r == *n))
            .count();
        let enabled = core.enabled_strategy_names();
        eprintln!(
            "blitzkrieg-core: strategy startup self-check: requested={} resolved={} enabled=[{}]",
            requested.len(),
            resolved,
            enabled.join(", ")
        );
        if requested.is_empty() || resolved > 0 || self.allow_zero_strategies {
            return Ok(());
        }
        Err(format!(
            "refusing to start: none of the {} explicitly requested strateg{} resolved to a live \
             strategy ({}), so this kernel would run with no strategy at all — while reporting \
             itself healthy (the engine's live set is [{}]). Take the libraries from the strategy \
             directory (`--strategy-dir`, `BK_STRATEGY_DIR`, or the default `user_layer/strategies` \
             of the checkout the binary lives in) — every library there is refused unless it sits \
             under an approved strategy root (`{}` under a repository root, or a directory named \
             in {}) — or acknowledge the empty set with --allow-zero-strategies. See the \
             `strategy auto-load` lines above for what was rejected and why",
            requested.len(),
            if requested.len() == 1 { "y" } else { "ies" },
            requested.join(", "),
            enabled.join(", "),
            APPROVED_ROOTS.join("`, `"),
            ENV_ALLOW_DIRS,
        ))
    }

    /// Auto-load every strategy library under `strategy_dir` and enable it.
    ///
    /// This is the "drop a strategy in the folder and it works" path: the tree
    /// is walked (bounded depth) so both a bare `libfoo.dylib` and a
    /// `cargo build --release` product under `target/release/` are found, each
    /// library goes through the same policy-checked dlopen the IPC
    /// `strategy.load` uses, and a load that fails is reported and skipped —
    /// one broken file never blocks the rest or the kernel itself. Duplicate
    /// copies of the same strategy (debug + release) are skipped after the
    /// first registration: the engine refuses a second instance of a live name.
    /// Runs BEFORE `enabled_strategies` so a config that names a just-loaded
    /// library enables it in the same pass.
    pub(super) fn load_strategy_dir(&self, core: &mut Core) {
        #[cfg(feature = "strategy-loading")]
        let Some(dir) = &self.strategy_dir else {
            return;
        };
        #[cfg(feature = "strategy-loading")]
        {
            let mut libs = Vec::new();
            collect_strategy_libs(Path::new(dir), 0, &mut libs);
            // Plain path order. It used to rank the shipped legs first
            // (spread_arb, then trend_follow, then mean_reversion) so a
            // debug/release pair resolved to a predictable winner; with no
            // shipped strategies the ranking had no reachable branch left, and
            // path order alone still makes the first copy the one that wins.
            libs.sort();
            for path in libs {
                // E27 (§8.2 row 1): the startup scan is the LOG-ONLY handshake
                // site — an incompatible library registers disabled with the
                // mismatch on its receipt; the enable gate is where refusal
                // happens.
                let receipt = core.load_strategy_lib_scan(&path.to_string_lossy());
                // The success receipt is exactly "<name>@<version> registered
                // into the engine dispatch…"; a duplicate registration is
                // REJECTED with a message that itself contains the word
                // "registered", so the match must be on the full success shape.
                // A duplicate is benign — debug and release builds of the same
                // crate both land in the tree — so it is reported as skipped,
                // not failed, and the first copy (sorted order) wins.
                if receipt.contains("already registered") {
                    eprintln!(
                        "blitzkrieg-core: strategy auto-load skipped (duplicate): {}",
                        path.display()
                    );
                    continue;
                }
                let ok = receipt.contains("registered into the engine dispatch");
                // The message text must NOT contain the substring "auto-load":
                // the E30 gate takes the FIRST stream line carrying that
                // marker plus the package name, so a tracing line here would
                // shadow the receipt eprintln below.
                tracing::info!(path = %path.display(), ok, receipt = %receipt, "strategy package scanned");
                // The receipt IS this line's payload: the E30 gate
                // (lua-sandbox-check.mjs) parses `auto-load (ok|FAILED): …`
                // from the stream — a fixed-shape status summary (name@version,
                // registration phrase, disabled marker), not free kernel text.
                eprintln!(
                    "blitzkrieg-core: strategy auto-load {}: {}",
                    if ok { "ok" } else { "FAILED" },
                    receipt
                );
            }
        }
        #[cfg(not(feature = "strategy-loading"))]
        {
            let _ = core;
        }
    }

    /// E30 (§6.6): scan `lua_strategy_dir` for Lua strategy packages and
    /// register each one DISABLED — the Lua mirror of `load_strategy_dir`.
    /// Load failures are reported and skipped, never fatal; a duplicate name
    /// is skipped (the first package in sorted order wins, like dylibs).
    pub(super) fn load_lua_strategy_dir(&self, core: &mut Core) {
        let Some(dir) = &self.lua_strategy_dir else {
            return;
        };
        for pkg in crate::strategy_engine::lua_loader::discover_lua_packages(Path::new(dir)) {
            let receipt = core.load_lua_package_inner(&pkg, false);
            if receipt.contains("already registered") {
                eprintln!(
                    "blitzkrieg-core: lua strategy auto-load skipped (duplicate): {}",
                    pkg.display()
                );
                continue;
            }
            let ok = receipt.contains("registered into the engine dispatch");
            // Same "no auto-load substring" rule as the dylib scan above —
            // the tracing line must not shadow the receipt eprintln below.
            tracing::info!(pkg = %pkg.display(), ok, receipt = %receipt, "lua strategy package scanned");
            // Same contract as the dylib scan above: the E30 gate parses
            // `auto-load (ok|FAILED): <receipt>` from the stream — a fixed-shape
            // status summary, not free kernel text.
            eprintln!(
                "blitzkrieg-core: lua strategy auto-load {}: {}",
                if ok { "ok" } else { "FAILED" },
                receipt
            );
        }
    }
}

/// Depth-bounded walk gathering `*.dylib`/`*.so` files under `dir`.
///
/// Bounded because `strategy_dir` may point at a crate checkout whose `target/`
/// tree is enormous; 6 levels comfortably covers `target/<profile>/deps/` while
/// never descending into the dependency graph's own build dirs. Symlinks are
/// not followed (a loop must not hang startup); unreadable subtrees are skipped.
pub fn collect_strategy_libs(
    dir: &std::path::Path,
    depth: usize,
    out: &mut Vec<std::path::PathBuf>,
) {
    const MAX_DEPTH: usize = 6;
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.')
                || name == "deps"
                || name == "build"
                || name == "incremental"
                || name.contains("probe")
                || name.contains("devcheck")
            {
                continue;
            }
            collect_strategy_libs(&path, depth + 1, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("dylib") | Some("so")
        ) {
            // Probe libraries (`devcheck_probe`) are generated into this tree by
            // `strategy-devcheck`, loaded through IPC `strategy.load`, and deleted
            // again. Auto-loading one at startup would make that gate measure a
            // strategy it did not register.
            if let Some(file_name) = path.file_name().and_then(|f| f.to_str())
                && (file_name.contains("probe") || file_name.contains("devcheck"))
            {
                continue;
            }
            out.push(path);
        }
    }
}
