//! Core configuration column, moved verbatim from `service.rs` (pure code
//! motion, no logic edits): the config surface (`CoreConfig`,
//! `StrategyLimit`, `VirtualAccountLimit`, `ShadowEvolutionTuning`, its
//! `Default`), the fee-quote readouts, the durable-log records
//! (`AppliedFillLog`, `AuditRuntime`), the error/placement records
//! (`LastError`, `PlaceOutcome`) and the replay bookkeeping types
//! (`InflightTaker`) with their tuning constants.
//!
//! `service.rs` re-exports the public surface with `pub use config::*;`, so
//! every path (`crate::service::CoreConfig`, ...) resolves exactly as before
//! the split. Items that were file-private in `service.rs` keep their old
//! visibility from the service module's point of view via `pub(super)`.

use crate::model::*;
use crate::position::PositionConfig;
use crate::risk::RiskConfig;
use crate::sim::FillModel;
use rust_decimal::Decimal;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::PathBuf;

mod engine;
mod fee;
mod records;
mod types;

#[cfg(test)]
mod tests;

pub use engine::*;
pub use fee::*;
pub use records::*;
pub use types::*;

#[derive(Debug, Clone)]
pub struct CoreConfig {
    pub mode: Mode,
    pub default_maker_timeout_ms: i64,
    /// Ticks a taker exit's limit is set below the executable bid so the FOK
    /// stays marketable in thin books (the venue still fills at the bid or
    /// better). 0 = price exactly at the executable bid.
    pub exit_taker_slip_ticks: i64,
    pub risk: RiskConfig,
    /// Starting simulated cash balance in DRY mode.
    pub dry_seed_balance: Decimal,
    /// Condition ids to subscribe on the live user channel (empty = REST only).
    pub markets: Vec<String>,
    /// Position sizing/exit/cooldown config (Rust owns positions in P2).
    pub positions: PositionConfig,
    /// Consecutive-loss breaker threshold and cooldown.
    pub max_consecutive_losses: u32,
    pub breaker_cooldown_sec: i64,
    /// Per-account round length anchor: `account_id → seconds`. Each account's
    /// own grid (`ACCOUNT_<ID>_ROUND_DURATION_SEC` env key). Absent for an
    /// account → falls back to `default_round_duration_sec`. This is the
    /// single source of truth for timing math (slot derivation, expiry, stop
    /// tightening) — replacing the old global `round_duration_sec`.
    pub account_round_durations: HashMap<String, i64>,
    /// #1170 (方案 A): per-strategy round-family bindings — the strategy name
    /// (in [`crate::engine::strategy_env_fold`] spelling) → the round duration
    /// (sec) the package trades. Populated at boot from the
    /// `STRATEGY_<NAME>_ROUND_DURATION_SEC` env cells (the ACCOUNT_ grammar
    /// applied to strategies); a bound strategy hears only ITS family's
    /// rollover, reads ITS family's round clock, and sees only its family's
    /// market rows — the multi-family cache poisoning #1170 diagnosed becomes
    /// structurally impossible. Empty map = every strategy unbound: the
    /// historic dispatch, byte-identical.
    pub strategy_round_families: HashMap<String, i64>,
    /// Fallback round length for accounts without an explicit
    /// `ACCOUNT_<ID>_ROUND_DURATION_SEC`. Populated from
    /// `ACCOUNT_DEFAULT_ROUND_DURATION_SEC` env key (default 900), or from
    /// the deprecated `RUNTIME_ROUND_DURATION_SEC` with a deprecation warning.
    pub default_round_duration_sec: i64,
    /// Deprecated: kept for test compatibility. Use `default_round_duration_sec`
    /// and `account_round_durations` instead. This field is NOT consulted by
    /// any runtime code path — it exists only so existing test constructors
    /// keep compiling. Set `default_round_duration_sec` for the actual value.
    pub round_duration_sec: i64,
    /// The round lengths switchable this session: the DECLARED UNION of the
    /// registered market plugins' `supported_round_secs` (`MarketPluginRegistry
    /// ::allowed_round_secs`), computed once at boot — which durations may be
    /// armed is venue knowledge, so the kernel keeps no hardcoded list; this
    /// field only mirrors what the plugins declared. EVERY consumer
    /// (`round_duration.get` readout, `round_duration.set`, the persisted-cell
    /// reader, the ACCOUNT_* parser, `account.rounds.set`) admits a value
    /// only when this union contains it, and an EMPTY union refuses EVERY
    /// value (fail-closed: with no plugin claiming a family, accepting any
    /// value would be the kernel inventing knowledge the declaration face
    /// never made — not "allow everything").
    pub allowed_round_secs: Vec<i64>,
    /// The labels the registered market plugins declare for their round
    /// families (`MarketPluginRegistry::round_labels`, issue 648), computed
    /// once at boot — `sec → label` as the plugins named their own Gamma
    /// slug families. The kernel holds no hardcoded label table of its own:
    /// `round_duration.get` renders exactly the labels the plugins claimed,
    /// and a duration missing here reads unlabeled.
    pub round_labels: BTreeMap<i64, &'static str>,
    /// Where the effective round length came from — one of `cli` / `env` /
    /// `runtime` / `toml` / `default`. The boot resolution owns the spelling;
    /// `round_duration.get` surfaces it so the UI can say "an explicit flag
    /// outranks this card". Not a decision input anywhere.
    pub round_duration_source: &'static str,
    /// The round-duration UI card's durable cell (`data/runtime/config.json`
    /// in production). `None` disables the write path — the backtester and
    /// most in-process tests run with `None`, and a `round_duration.set` then
    /// refuses with "persist is disabled" instead of writing into whatever
    /// checkout the caller happens to run from (the same posture as
    /// `strategy_state_path`).
    pub runtime_config_path: Option<String>,
    /// The `.env` file the launcher loads at start (`<cwd>/.env` in
    /// production) — the write half of `market.credentials.set` upserts
    /// validated credential keys HERE (the single persistent home for
    /// UI-submitted credentials; the config.json cell is retired). `None`
    /// disables the write path — the backtester and most in-process tests
    /// run with `None`, and a `market.credentials.set` then refuses with
    /// "persist is disabled" (the same posture as `runtime_config_path`).
    /// The READ side never consults this field: the kernel's own environment
    /// already carries the launcher's `.env` load, so every predicate reads
    /// `std::env` directly.
    pub env_file_path: Option<String>,
    /// 操作者裁决 2026-11-14（#1199 第六轮）：`.env` 对内核是**只读**的——
    /// 唯一的持久化后端是 SQLite。这是 SQLite 文件的路径（生产 =
    /// `db::default_db_path()`，由 `session::run` 设置）；`Some` 时装配
    /// 打开 [`crate::store::SqliteStore`] 并 fail-closed（打开失败拒绝启动，
    /// 绝不静默降级成无存储）。`None` = 无存储（回测、多数进程内测试）：
    /// 所有写面如实拒绝，绝不写 `.env`。`.env` 的读面不受本字段影响。
    pub db_path: Option<PathBuf>,
    /// Every registered plugin's DECLARED credential env names (issue 648,
    /// `plugin → [env var name]` from `MarketPlugin::required_credential_env`),
    /// computed once at boot from the registry — the same report posture as
    /// `allowed_round_secs`. The credential readout, the `market.credentials.set`
    /// validation and the RFC #541 ready predicates all read it from here; the
    /// kernel holds no hardcoded venue→env table of its own. Empty map = no
    /// credential-bearing plugin is compiled in.
    pub credential_vars: BTreeMap<String, Vec<String>>,
    /// The account book's own legacy path (`user_layer/configs/accounts.toml`
    /// in production; retired as a configuration source by issue 751/923 —
    /// now the write-gate basis) — the write half of `account.venue.set`
    /// refuses while a file exists at this path; the book is re-read from
    /// the `.env` alone at the next boot. `None` disables the write
    /// path: the backtester and most in-process tests run with `None`, and a
    /// venue bind then refuses with "persistence is disabled" instead of
    /// writing into whatever checkout the caller happens to run from (the
    /// same posture as `runtime_config_path`, which this mirrors). The READ
    /// half never consults this as a source — `Core::new` hands the path to
    /// the loader (which probes it as residue and warns), tests inject a
    /// book via [`Core::with_account_book`]; only tests that exercise the
    /// persist round-trip set this to their own private file.
    pub accounts_config_path: Option<String>,
    /// #458: where manual, destructive operations (`system.reset_trading`,
    /// and later `orders.force_close`) append their audit record. `None`
    /// (the backtester, most in-process tests) means a reset REFUSES rather
    /// than wipe records with no audit trail — the manual-ops line is the
    /// only trace a reset leaves. Production resolves this to
    /// `data/audit/manual-ops.jsonl` in `main.rs`, like `runtime_config_path`.
    pub manual_ops_audit_path: Option<String>,
    /// #481/④: where the cross-venue mapping ledger persists
    /// (`data/runtime/unified-mapping.json` in production). `None` keeps the
    /// ledger in memory — the backtester and most in-process tests never
    /// confirm a mapping, so there is nothing to persist.
    pub unified_ledger_path: Option<String>,
    /// #861: where the RUNTIME risk state persists (hot entry caps from
    /// `risk.setLimits`, the nine systemic bounds from `risk.setSystemic`,
    /// the kill switch and the freeze book — `positions.risk-state.json`
    /// beside the position log in production, the daily-loss file's family).
    /// `None` (the backtester, most in-process tests) keeps every risk knob
    /// session-scoped exactly as before: nothing is written, nothing is
    /// restored, no restart default-freeze applies.
    pub risk_state_path: Option<String>,
    /// When false the core never auto-submits exits on tick; positions are only
    /// closed by explicit fills/flatten. Used by order-layer tests and for a
    /// "positions managed externally" deployment. Default true.
    pub auto_exits_enabled: bool,
    /// Instantiate the self-driving market-data engine (signal → order). Default
    /// false so the P0/P1/P2 bridges keep working until the feed layer is wired.
    pub engine_enabled: bool,
    /// Assets the engine trades and its sizing/round timing (mirrors the TS config).
    pub assets: Vec<String>,
    /// Strategies to switch ON after the engine is installed, by name (E4-a).
    /// The kernel ships ZERO enabled strategies — what starts enabled is the
    /// operator's persisted intent (`strategy_state_path`) plus explicit
    /// `--enable-strategy` flags, so adding a strategy can never change what an
    /// existing session trades. Unknown names are reported and ignored, never
    /// fatal.
    pub enabled_strategies: Vec<String>,
    /// Strategies to switch OFF after the engine is installed, by name. Applied
    /// after `enabled_strategies`, so the explicit "off" wins if both name the
    /// same strategy.
    pub disabled_strategies: Vec<String>,
    /// Directory scanned at startup for user-layer strategy libraries
    /// (`*.dylib`/`*.so`): every library found is dlopen'd and REGISTERED —
    /// registration never enables; what trades is decided solely by
    /// `enabled_strategies` (persisted intent + CLI flags). `None` = no
    /// directory (auto-load off). Load failures are reported and skipped,
    /// never fatal — a broken file must not brick the kernel.
    pub strategy_dir: Option<String>,
    /// E30 (§6.4): directory scanned at startup for Lua strategy PACKAGES
    /// (directories carrying `manifest.json`). Same discipline as
    /// `strategy_dir`: every valid package LOADS and registers DISABLED; the
    /// enable decision comes from the persisted set plus `--enable-strategy`.
    /// Deliberately SEPARATE from `strategy_dir` — that path is the dylib
    /// scanner's root, and Lua packages are never dylibs (§6.4: mixing them
    /// in would blur the measurement fixtures' semantics). `None` = off;
    /// default `user_layer/strategies_lua` under the repo.
    pub lua_strategy_dir: Option<String>,
    /// Where the effective enabled-set is recorded so runtime toggles and boot
    /// flags survive a restart (`data/strategy-state.json` in production —
    /// per-mode `data/strategy-state.dry.json` for the dry book, issue 953;
    /// see [`crate::strategy_state`]). `None` disables persistence — the backtester
    /// and most in-process tests run with `None`, and a toggle then changes
    /// nothing on disk.
    pub strategy_state_path: Option<String>,
    /// The strategies the OPERATOR named on the command line
    /// (`--enable-strategy`), kept apart from `enabled_strategies` because the
    /// startup self-check is about an EXPLICIT request, not about what a
    /// persisted state file replays (#265).
    ///
    /// Empty for every in-process core and for the backtester, which is why the
    /// refusal below cannot surprise a caller that never asked for a strategy by
    /// name. The two sets are related by construction: `main` builds
    /// `enabled_strategies` as the persisted set plus these names.
    pub requested_strategies: Vec<String>,
    /// #265 — start even when none of the requested strategies resolved. The
    /// default is to REFUSE: an explicit request that loads nothing used to be
    /// one `WARN` line followed by a healthy-looking kernel that had no strategy
    /// at all (exit 0, socket listening, health probe green). `--allow-zero-strategies`
    /// is the acknowledged escape hatch for a harness that wants that boot.
    pub allow_zero_strategies: bool,
    pub min_round_age_sec: i64,
    pub size_usd: Decimal,
    pub min_shares: Decimal,
    pub max_shares: Decimal,
    /// Issue #1189 (PR-D): per-account share band overrides —
    /// `account_id → min shares`. [`CoreConfig::share_band_for`] prefers the
    /// account row over the global `min_shares`.
    pub account_min_shares: HashMap<String, Decimal>,
    /// Issue #1189 (PR-D): per-account share band overrides —
    /// `account_id → max shares`. [`CoreConfig::share_band_for`] prefers the
    /// account row over the global `max_shares`.
    pub account_max_shares: HashMap<String, Decimal>,
    /// Issue #1189 (PR-D): per-account market plugin overrides —
    /// `account_id → plugin name`. [`CoreConfig::market_plugin_for`] prefers
    /// the account row over the caller-supplied fallback.
    pub account_market_plugins: HashMap<String, String>,
    /// P0 #202 — the per-entry budget as a percentage of the account's cash
    /// equity, replacing the absolute `size_usd` when > 0. 0 (the default)
    /// keeps every deployment's current order sizes exactly as they are.
    pub size_pct: Decimal,
    /// Engine trend confirmation window (sec) and floor (ms) — exposed so tests
    /// and ops can shorten the confirmation window without touching code.
    pub trend_confirm_sec: i64,
    pub trend_window_floor_ms: i64,
    /// spread_arb entry knobs (E15): `None` keeps the shipped default in
    /// [`crate::signal::SpreadArbConfig`]; a value overrides it in
    /// `engine_config` — the one mapping that drives both the live server and
    /// the backtester, so a sweep varies candidates purely through config.
    pub spread_arb_trend_entry_factor: Option<Decimal>,
    pub spread_arb_entry_min_obi: Option<Decimal>,
    pub spread_arb_entry_max_spread_pct: Option<Decimal>,
    pub spread_arb_entry_dip_max_pct: Option<Decimal>,
    pub spread_arb_entry_bounce_min_pct: Option<Decimal>,
    /// Turn-filter lookback window (sec); `None` = default 5.
    pub spread_arb_entry_bounce_window_sec: Option<i64>,
    /// Start Rust-native WS feeds (P4): Polymarket orderbook + Chainlink 60s TWAP.
    /// When enabled Node no longer has to push `books.*` / `spot.price`.
    pub feed_ws_enabled: bool,
    /// Select the operative market plugin by name (matches `MarketPlugin::name`,
    /// e.g. "polymarket"). None = first registered. Ignored when the name is not
    /// registered (falls back to the first) so a stray flag never bricks startup.
    pub market_plugin: Option<String>,
    /// Reference oracle stream symbols (e.g. ["BTC","ETH"]); empty disables reference feed.
    pub spot_assets: Vec<String>,
    /// Where to append near-miss records (blocked signals + path). None = off.
    pub near_miss_path: Option<String>,
    /// #363: where the account-isolated execution policy loads from. The
    /// binary entry (`main.rs`, #380) resolves the SHIPPED default against the
    /// install root (the executable's ancestors, then the cwd's) and stores
    /// the outcome here — the library reads this path VERBATIM, so which cwd
    /// the process started in can no longer flip "guarded kernel" to "neutral
    /// kernel" (#380's defect). Embedders and tests pass an explicit path (or
    /// `None` to load nothing — the backtester and most in-process tests,
    /// which is the same as the zero-config deployment: code defaults,
    /// hardcoded-behaviour parity). A file that exists but is malformed
    /// refuses the boot (fail-closed) with a `load-refused` audit line — the
    /// kernel never trades under a policy the operator did not write.
    pub execution_policy_path: Option<String>,
    /// #386: where the execution-policy audit journal appends and the
    /// `history` arm reads back. `None` = the shipped
    /// `data/audit/execution_policy.jsonl` spelling (cwd-relative — one core
    /// per data directory owns it, the same road the other journals take;
    /// boot `load-refused` lines included). Tests pass a PRIVATE temp path so
    /// parallel test cores never share one journal file: two interleaved
    /// sets once fused into a single unparseable line and vanished from the
    /// history two tests were counting (the flake PR #378 disclosed).
    pub execution_policy_audit_path: Option<String>,
    /// #363: operator override for the per-asset position cap (the
    /// `--max-positions-per-asset` flag). `None` = the execution policy's
    /// value decides (`[accounts.<id>]` → `[defaults]` → code default 1).
    /// An explicit CLI number wins over the policy file, matching the
    /// CLI > env > TOML > default precedence the rest of the kernel uses.
    pub max_positions_per_asset: Option<u32>,
    /// Where to append closed trades (Node-compatible JSONL). None = off.
    pub trade_log_path: Option<String>,
    /// Where to persist tracked ORDERS (crash recovery + orphan detection).
    /// None = off. Default `data/orders/orders.jsonl`.
    pub order_log_path: Option<String>,
    /// Where to persist OPEN POSITIONS (crash recovery). None = off.
    /// Default `data/positions/positions.jsonl` live (per-mode
    /// `data/positions/positions.dry.jsonl` for the dry book's own snapshot,
    /// issue 953; the binary's `main` derives the default like the trade
    /// log's split above).
    pub position_log_path: Option<String>,
    /// Discover rounds inside the core (Gamma slug queries) instead of relying on
    /// Node to push `engine.markets`. Default true when the engine is on.
    pub discovery_enabled: bool,
    /// Update-check switches (VERSIONING.md §7). Resolved in `main` through
    /// CLI > env; the runtime cell persisted in
    /// `data/update/state.json` outranks all three. Both default false — a
    /// default-configured stack makes zero outbound connections (INV-3).
    pub update_check_enabled: bool,
    /// Apply an update automatically (the kernel still never installs: the
    /// launcher owns that, P12). Default false.
    pub update_auto: bool,
    /// #379: seconds between AUTOMATIC update checks. `0` (the default) means
    /// NO scheduler task exists — the clock is an explicit configuration
    /// choice (`--update-interval-secs` / `BLITZKRIEG_UPDATE_INTERVAL_SECS`),
    /// never a default.
    pub update_interval_secs: u64,
    /// Start with Shadow Evolution enabled (opt-in; default false).
    pub shadow_evolution_enabled: bool,
    /// Optional Shadow Evolution tuning (tests/ops). None = crate defaults.
    pub shadow_evolution_tuning: Option<ShadowEvolutionTuning>,
    /// Optional per-strategy entry caps, keyed by strategy name (P-1.1). An
    /// absent entry means unlimited — a strategy keeps its current behaviour
    /// unless an operator configures a cap.
    pub strategy_limits: HashMap<String, StrategyLimit>,
    /// E-VA: per-strategy VIRTUAL quota pots, keyed by strategy name. Empty
    /// (the shipped default) = the virtual-account layer does not exist: no
    /// pot, no reservation, no release — every strategy keeps the per-order
    /// real-ledger path byte-identically. A configured strategy that also
    /// declares `atomic_batch` gets the atomic batch pre-reservation.
    pub virtual_limits: HashMap<String, VirtualAccountLimit>,
    /// Maker→taker escalation deadline for engine entries (P-1.2). Defaults to the
    /// long-standing 5000 ms; a backtest may model a different venue latency.
    pub entry_maker_timeout_ms: i64,
    /// Fill-realism model for the dry matcher (P-1.2). Default = identity, so the
    /// live/dry path is unchanged; a backtest raises slippage/latency/probability.
    pub fill_model: FillModel,
    /// Where to mirror every market-data event the engine consumes (P-1.3).
    /// None = off (default). Used to build the local L2/tick corpus a backtest
    /// replays.
    pub event_archive_path: Option<String>,
    /// Stop recording once the archive reaches this size (MiB); 0 = unlimited.
    /// The cap never deletes data — a full archive just stops growing. Old
    /// segments are only removed by the disk floor
    /// (`event_archive_min_free_mb`), never by the cap. This is the
    /// whole-session bound; `event_archive_rotate_mb` bounds one segment.
    pub event_archive_max_mb: u64,
    /// Rotate the archive into a new segment every this many MiB (0 = never).
    /// Default 256 MiB: a 24/7 capture needs this — `≈11 MB/min` reaches any sane
    /// session cap within hours, and an un-rotated archive that hits the cap goes
    /// dark silently. Rotation renames the finished segment (UTC-stamped) and
    /// opens a fresh one. When `event_archive_min_free_mb` is set, the oldest
    /// rotated segments are deleted once the volume floor is breached; nothing
    /// is ever deleted without that floor being configured.
    pub event_archive_rotate_mb: u64,
    /// Stop recording rather than let the archive's volume drop below this many
    /// MiB free (0 = no guard). Once segments have been rotated away, the oldest
    /// ones are deleted while the floor is breached; recording stops only when
    /// the floor cannot be restored by deletion. Checked once per rotation, so it
    /// costs nothing on the hot path.
    pub event_archive_min_free_mb: u64,
    /// How old an orderbook may be before the engine refuses to price off it,
    /// in milliseconds (issue #205). `0` = the check is OFF (any age accepted).
    ///
    /// This is the knob that decides how long after the feed goes quiet the bot
    /// stops trading, so it is an operator setting rather than a compiled
    /// constant: CLI `--max-orderbook-stale-ms` / `BK_MAX_ORDERBOOK_STALE_MS`,
    /// validated at startup and echoed in the boot log. The default is
    /// [`crate::engine::DEFAULT_MAX_ORDERBOOK_STALE_MS`] — the value every
    /// deployment has always run with, so an unconfigured kernel is unchanged.
    pub max_orderbook_stale_ms: i64,
    /// 任务二 1-3 — the NTP clock-offset probe's server host. The empty string
    /// is the documented "off" spelling: `probe::should_spawn_ntp` refuses it,
    /// so the production server spawns ZERO network tasks. The measurement
    /// loop is started ONLY by `ipc::server::run` (production); a replay
    /// never constructs a socket. Default `probe::DEFAULT_NTP_HOST`.
    pub ntp_host: String,
    /// 任务二 1-3 — seconds between NTP probes. 0 = the periodic loop is off.
    /// Default `probe::DEFAULT_NTP_INTERVAL_SECS` (300). The core itself never
    /// reads this — the server does, at the one production spawn site.
    pub ntp_interval_secs: i64,
    /// How often the in-kernel accounting audit runs, in seconds (issue #189).
    /// 0 disables it. Default 30: the acceptance criterion is "drift is alerted,
    /// persisted and blocks new entries within 30s of appearing".
    pub audit_interval_sec: i64,
    /// Where to append one JSONL record per audit (issue #189). None = derive
    /// `reconcile.jsonl` beside the order log; `order_log_path: None` (the
    /// backtester and most tests) means no audit file at all.
    pub audit_log_path: Option<String>,
    /// Where to persist the applied-fill idempotency table (issue #178). None =
    /// derive `applied-fills.jsonl` beside the order log; `order_log_path: None`
    /// means no persistence, i.e. the table is per-session.
    pub applied_log_path: Option<String>,
    /// Whether a failing audit blocks NEW entries (default true; issue #189).
    /// Exits are never blocked — a drifted ledger must not be able to trap the
    /// bot in a position (the #174 failure mode). Turning this off keeps the
    /// alert and the persisted record while leaving the gates alone.
    pub audit_halt_entries: bool,
    /// Where to persist the SETTLEMENT journal (issue #175). None = derive
    /// `settlements.jsonl` beside the order log; `order_log_path: None` means no
    /// persistence, i.e. settlement idempotency is per-session.
    pub settlement_log_path: Option<String>,
    /// The ledger cash journal directory, one file per account per mode (issue #941): `data/ledger/<account>.journal.live.jsonl` (live trades) / `.journal.dry.jsonl` (dry trades),
    /// per account, one append per cash movement, replayed at boot so the
    /// balance survives a restart (last row's `balance_after`). `None` = the
    /// historical memory-only ledger (`--no-ledger-journal`, the backtester,
    /// in-process tests) — nothing is written and nothing is replayed, and
    /// boot seeds exactly as before.
    pub ledger_journal_dir: Option<String>,
    /// TEST HOOK, dry mode only: make the first N redemption attempts of every
    /// claim fail before one succeeds. `0` (the default) is the production
    /// behaviour — the simulated redemption always lands. A failure is a real
    /// `RedemptionFailure` through the real path (`note_failure` → backoff →
    /// retry), so the retry clock, the receivable and the manual-stop rule are
    /// exercised exactly as a venue failure would exercise them. The live path
    /// never reads this field: `simulate_redemptions` is the only consumer and it
    /// runs only where `settles_locally()` holds.
    pub dry_redeem_fail: u32,
    /// TEST HOOK, dry mode only: report those injected failures as `manual`, i.e.
    /// stop the automatic retries for good (`next_attempt_ms = i64::MAX`). Only
    /// meaningful together with [`CoreConfig::dry_redeem_fail`].
    pub dry_redeem_manual: bool,
    /// This core replays archived events, and may therefore charge a taker-fee
    /// schedule other than the shipped one (#234, item 4).
    ///
    /// `--fee-model` is the counterfactual knob for a replay, and the CLI refuses
    /// it without `--backtest`; this field is the same statement made where the
    /// charge happens. The two live charge sites
    /// ([`Core::live_taker_fee_pct`]) assert that a core which has NOT declared
    /// itself a replay is charging the shipped schedule — the isolation used to
    /// live only in the CLI, which meant a future caller of `set_fee_schedule`
    /// could have repriced live fills with no code objecting. Default `false`:
    /// only the backtester sets it.
    pub fee_schedule_replay: bool,
    /// Model the venue's maker rebate in fills (almach-benchmark REPORT §5
    /// 失真点 5): the official rule pays a market's makers 20% of the taker
    /// fee that market accrued, pro-rata by maker share. A replay cannot see
    /// the pool, so `apply_delta_effects` books the per-fill approximation —
    /// each MAKER fill earns `0.20 ×` the taker fee that same notional would
    /// have paid — as a cash credit AND a position-flow accrual in one step,
    /// which keeps `balance == seed + Σ net` an identity and lets a
    /// maker-vs-taker comparison read the rebate it was missing.
    ///
    /// Replay-only for the same reason `fee_schedule_replay` is: a live core
    /// crediting modeled cash would mint money the venue never reported. The
    /// backtester sets this together with `fee_schedule_replay`; default
    /// `false` = every existing deployment and test is byte-identical.
    /// #388's stream rows (real MAKER_REBATE ledger lines) credit on top of
    /// this, never instead of it — a corpus carrying real rows and a replay
    /// that models the pool would double-count, which is a converter-choices
    /// decision an operator makes per corpus, not something the kernel
    /// guesses.
    pub model_maker_rebate: bool,
    /// E25 (#331): write the intent-audit trail (`data/audit/intents.jsonl` +
    /// throttled `INTENT_DECISION` pushes). The ARBITRATION GATES always run;
    /// this silences only the WRITES (`--no-intent-audit`), which is exactly
    /// what P1's with/without-baseline double-run proves off the trading path.
    pub intent_audit_enabled: bool,
    /// Replay fast path (`--backtest-fast`, CLI-refused without `--backtest`,
    /// mirroring `--fee-model`). Gates three classes of per-tick work that a
    /// frozen book makes provably redundant:
    ///   1. the exit LADDER for hold-to-settlement strategies (the host
    ///      discards every holder verdict anyway — settlement owns them);
    ///   2. full-book re-valuation of positions whose book did not change;
    ///   3. Lua evaluation on ticks where no data arrived and no round-timing
    ///      boundary was crossed (the same inputs reproduce the same output).
    ///
    /// Dirty-token and dirty-order tracking keep the traded paths exact.
    /// Default `false`: every default run keeps the historical byte-for-byte
    /// tick behaviour, evaluations counter included.
    pub backtest_fast: bool,
    /// Issue #390 (replay nofast): the maintenance grid the replay drives the
    /// core on (ms). The deadline-scheduled exit ladder backfills its counter
    /// ratchets over this step, so the core must know the schedule the caller
    /// runs. Default 1 ms — follows the replay default; live never reads this
    /// field. Only read when
    /// `backtest_fast` is false and an engine drives positions (the nofast
    /// replay shape); every other path ignores it.
    pub tick_schedule_ms: i64,
    /// Issue #390: the replay maintenance scheduling. `true` makes the core
    /// apply the deadline-driven maintenance gates (Lua eval dirty-wake,
    /// settlement sync, escalation sweep, kline beat, exit-ladder schedule)
    /// to the NOFAST replay path — the same output-preserving gates
    /// `backtest_fast` applies, minus the all-holder skip (a nofast replay
    /// observes the ladder's suppressed-stop reports, so the ladder still
    /// runs on its schedule). Set ONLY by the replay driver; a live kernel
    /// keeps the historic every-tick behaviour on every path. Default
    /// `false`.
    pub replay_scheduling: bool,
    /// Event-driven engine evaluation on the live/dry kernel (the
    /// operator's choice: evaluate only when market data changes). A quiet
    /// maintenance tick skips the Lua call and re-runs the standing
    /// candidates of the last real evaluation through the same kernel gates
    /// (`engine_replay_cycle`). A real evaluation still runs on every engine
    /// input change, every whole second (the round clock strategies read),
    /// every book freshness expiry, and after any cycle whose strategies
    /// emitted exits or pair declarations. Only engaged while every enabled
    /// strategy declares it never reads the millisecond clock. Ignored by
    /// replays (they keep `replay_scheduling`). Default `true`; `false`
    /// restores the every-tick Lua call.
    pub event_driven_eval: bool,
    /// Issue #991/#1002: what an unverifiable-expiry fallback close books.
    /// `Receivable` (default) is the #991 behaviour: the fallback notional is
    /// booked as a claim in the settlement receivable pool — estimated money
    /// is not spendable money — and the claim redeems into cash (with
    /// #998-style correction rows for any venue-vs-fallback delta) when
    /// venue truth arrives. `Cash` keeps the #684 behaviour, byte-identical:
    /// the fallback notional is credited to spendable cash at close time
    /// (`Ledger::credit_expiry_fallback`). Read ONCE at boot
    /// (`BK_UNVERIFIED_EXPIRY_POLICY=cash|receivable`); NOT hot-reloadable —
    /// a policy switch changes what already-booked journal rows mean, so it
    /// takes a restart (documented in `.env.example`).
    pub unverified_expiry_policy: UnverifiedExpiryPolicy,
    /// Issue #991 boundary 1: how old a fallback receivable claim may get
    /// (ms from `created_at_ms`) before the kernel warns that the venue still
    /// has not answered. Amber, never red: an aging claim is ATTENTION (the
    /// settlement path may still be waiting on a structurally-never-answered
    /// market), not trading-down. Default [`RECEIVABLE_MAX_AGE_MS_DEFAULT`]
    /// (48h) — `BK_RECEIVABLE_MAX_AGE_MS`.
    pub receivable_max_age_ms: i64,
}

/// Issue #991/#1002: the two complete models an unverifiable-expiry close can
/// book under. `serde` spells it lowercase on the wire (`"cash"` /
/// `"receivable"`); the env value parses through
/// [`UnverifiedExpiryPolicy::parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnverifiedExpiryPolicy {
    /// #684 behaviour, unchanged: the fallback close credits spendable cash
    /// immediately at the bookkeeping price.
    Cash,
    /// Issue #991: the fallback close books a receivable claim; cash moves
    /// only when venue truth redeems the claim (reconcile/resolution path),
    /// with correction rows for the delta.
    #[default]
    Receivable,
}

impl UnverifiedExpiryPolicy {
    /// Parse the env spelling. Accepts the two documented values only — an
    /// unknown value is a boot-time error, never a silent fall-back to cash
    /// (a typo'd policy must not quietly change the accounting model).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cash" => Some(Self::Cash),
            "receivable" => Some(Self::Receivable),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cash => "cash",
            Self::Receivable => "receivable",
        }
    }
}

/// Issue #991 boundary 1: the shipped aging bound — 48 hours. A venue that
/// has not answered a settlement query in 48h is structurally silent (the
/// #684 signature); the receivable it owes us must become VISIBLE, not
/// quietly sit in the panel's receivable number forever.
pub const RECEIVABLE_MAX_AGE_MS_DEFAULT: i64 = 172_800_000;

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Dry,
            default_maker_timeout_ms: 5000,
            exit_taker_slip_ticks: 2,
            risk: RiskConfig::default(),
            dry_seed_balance: Decimal::from(10_000),
            markets: Vec::new(),
            positions: PositionConfig::default(),
            max_consecutive_losses: 3,
            breaker_cooldown_sec: 300,
            round_duration_source: "default",
            account_round_durations: HashMap::new(),
            // #1170 (方案 A): empty = every strategy unbound (the historic
            // whole-table dispatch).
            strategy_round_families: HashMap::new(),
            default_round_duration_sec: 900,
            round_duration_sec: 900,
            allowed_round_secs: Vec::new(),
            round_labels: BTreeMap::new(),
            runtime_config_path: None,
            env_file_path: None,
            db_path: None,
            credential_vars: BTreeMap::new(),
            accounts_config_path: None,
            manual_ops_audit_path: None,
            unified_ledger_path: None,
            risk_state_path: None,
            auto_exits_enabled: true,
            intent_audit_enabled: true,
            engine_enabled: false,
            assets: vec!["BTC".into(), "ETH".into(), "SOL".into(), "XRP".into()],
            enabled_strategies: Vec::new(),
            disabled_strategies: Vec::new(),
            strategy_dir: None,
            lua_strategy_dir: None,
            strategy_state_path: None,
            requested_strategies: Vec::new(),
            allow_zero_strategies: false,
            min_round_age_sec: 30,
            size_usd: Decimal::new(25, 1), // 2.5
            min_shares: Decimal::from(10),
            max_shares: Decimal::from(10),
            // Issue #1189 (PR-D): empty = no account carries its own band or
            // market plugin; every account resolves to the global values.
            account_min_shares: HashMap::new(),
            account_max_shares: HashMap::new(),
            account_market_plugins: HashMap::new(),
            size_pct: Decimal::ZERO,
            trend_confirm_sec: 60,
            trend_window_floor_ms: 10_000,
            spread_arb_trend_entry_factor: None,
            spread_arb_entry_min_obi: None,
            spread_arb_entry_max_spread_pct: None,
            spread_arb_entry_dip_max_pct: None,
            spread_arb_entry_bounce_min_pct: None,
            spread_arb_entry_bounce_window_sec: None,
            feed_ws_enabled: false,
            market_plugin: None,
            spot_assets: vec!["BTC".into(), "ETH".into(), "SOL".into(), "XRP".into()],
            near_miss_path: None,
            // #363: default to the shipped config spelling; the loader treats
            // a MISSING file as the zero-config deployment (code defaults).
            // #380: this is a SPELLING, not a location — the binary entry
            // resolves it against the install root before the core is built;
            // the library honours it verbatim (see the field's doc above).
            execution_policy_path: Some(
                crate::execution_policy::EXECUTION_POLICY_CONFIG_PATH.to_string(),
            ),
            // #386: the shipped `data/audit/…` spelling resolves against the
            // process cwd — production's single core owns that directory; a
            // test that wants isolation passes its own private path.
            execution_policy_audit_path: None,
            max_positions_per_asset: None,
            // #trades-mode-split: THIS default is the zero-config DRY
            // deployment's ledger — the historical `data/trades/trades.jsonl`
            // the dry-era rows and dry tooling already own. A LIVE boot never
            // reads this cell: the binary's `main` derives its default through
            // `crate::trade_db::default_trade_log_path(mode)` (live →
            // `trades-live.jsonl`) before the config is built. Keep the two
            // spellings mirrored when the default tree changes.
            trade_log_path: Some("data/trades/trades.jsonl".to_string()),
            order_log_path: Some("data/orders/orders.jsonl".to_string()),
            position_log_path: Some("data/positions/positions.jsonl".to_string()),
            discovery_enabled: true,
            update_check_enabled: false,
            update_auto: false,
            update_interval_secs: 0,
            shadow_evolution_enabled: false,
            shadow_evolution_tuning: None,
            strategy_limits: HashMap::new(),
            virtual_limits: HashMap::new(),
            entry_maker_timeout_ms: 5000,
            fill_model: crate::sim::FillModel::default(),
            event_archive_path: None,
            event_archive_max_mb: 512,
            event_archive_rotate_mb: 256,
            event_archive_min_free_mb: 0,
            // #205: the shipped freshness budget, unchanged.
            max_orderbook_stale_ms: crate::engine::DEFAULT_MAX_ORDERBOOK_STALE_MS,
            // 任务二 1-3: the shipped NTP probe posture — pool.ntp.org every
            // 5 minutes. `should_spawn_ntp("")` = off is the deployment's
            // documented kill switch (`--ntp-host ""`).
            ntp_host: crate::probe::DEFAULT_NTP_HOST.to_string(),
            ntp_interval_secs: crate::probe::DEFAULT_NTP_INTERVAL_SECS,
            audit_interval_sec: 30,
            audit_log_path: None,
            applied_log_path: None,
            ledger_journal_dir: None,
            audit_halt_entries: true,
            settlement_log_path: None,
            dry_redeem_fail: 0,
            dry_redeem_manual: false,
            // #234: nothing but the backtester may charge a counterfactual fee.
            fee_schedule_replay: false,
            model_maker_rebate: false,
            // Replay fast path: off by default — historical tick behaviour is
            // byte-for-byte until the operator opts in with --backtest-fast.
            backtest_fast: false,
            // Issue #390: the maintenance grid the exit ladder's deadline
            // schedule backfills over (the replay driver's tick step).
            tick_schedule_ms: 1,
            // Issue #390: replay maintenance scheduling off by default — the
            // live kernel stays every-tick on every path.
            replay_scheduling: false,
            // Event-driven live evaluation on by default (see the field).
            event_driven_eval: true,
            // Issue #991/#1002/#1273: unverifiable expiry books a receivable
            // claim by default — estimated money is not spendable cash until
            // venue truth redeems it. `Cash` (#684 byte-identical behaviour)
            // stays opt-in via BK_UNVERIFIED_EXPIRY_POLICY=cash.
            unverified_expiry_policy: UnverifiedExpiryPolicy::Receivable,
            // Issue #991 boundary 1: the shipped 48h aging bound.
            receivable_max_age_ms: RECEIVABLE_MAX_AGE_MS_DEFAULT,
        }
    }
}
