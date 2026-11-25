//! Core service — assembles the OME, ledger, risk gate and dry matcher and
//! implements every typed command. Synchronous except for the event sink, so it
//! is fully unit-testable; the async UDS layer and live venue sit on top.

use crate::model::*;
use crate::ome::Ome;
use crate::position::{DailyLossStore, PositionManager};
use crate::risk::{LossBreakers, RiskGate};
use crate::schema::Event;
use crate::settlement::SettlementBook;
use crate::shadow_evolution::{ShadowEvolution, ShadowEvolutionConfig};
use crate::sim::Book;
use blitzkrieg_market_api::event_api::EventApi;
use blitzkrieg_market_api::execution_api::ExecutionApi;
use blitzkrieg_market_api::market_data_api::MarketDataApi;
use blitzkrieg_market_api::net::now_ms;
use blitzkrieg_market_api::risk_api::RiskApi;
use blitzkrieg_market_api::strategy_api::StrategyApi;
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::mpsc;

mod accounts;
mod assembly;
mod assembly_boot;
mod assembly_risk_db;
mod cockpit_env;
mod config;
mod config_gates;
mod daily_loss_gate;
mod engine_cycle;
mod extensions;
mod family_feed_sync;
mod helpers;
mod persistence;
mod pricing;
mod risk_admission;
mod round_duration;
mod settlement_orders;
mod strategies;
mod trading_exit;
mod trading_reset;
mod trading_risk_limits;
mod trading_round_duration;
mod views;

pub use config::*;
pub(crate) use helpers::*;
pub use views::*;
// #186: `ORDERS_DEFAULT_LIMIT` is the wire handler's bound; the two pure page
// helpers below it back the pagination tests, so they are test-only here.
pub(crate) use settlement_orders::ORDERS_DEFAULT_LIMIT;
#[cfg(test)]
pub(crate) use settlement_orders::{orders_page_keeps, orders_page_window};
// #824 cycle 2: the venue-credential predicates moved into engine_cycle (their
// only consumer); surfaced here so the integration tree keeps a public handle.
pub use engine_cycle::{venue_credentials_missing, venue_credentials_ready};
// #834 cycle 1: the freeze/circuit-breaker state book moved next to its only
// manager; surfaced for the struct field and the integration tree's handle.
pub(crate) use risk_admission::FreezeBook;
// #1045: the single in-memory per-account entry gate (replaces
// FreezeBook's retired `account_freezes` + `auto_frozen` tables).
pub(crate) use risk_admission::{AccountGate, GLOBAL_KILL_ACCOUNT, GateReason};
// #187: the VENUE axis of the entry gate — a halt that needs no account to
// hang on, so a venue failure with no bound account has a home instead of
// escalating to the process-wide kill switch.
pub(crate) use risk_admission::VenueHalts;
// #861: the runtime risk-state cell (path + parsed file) the Core struct
// holds — same re-export shape as FreezeBook above.
pub(crate) use risk_admission::RiskStateCell;

// Helpers moved out of the hub into domain children; re-exported so external
// paths (`crate::service::marketable_exit_price`, `crate::service::build_lua_scope`,
// `crate::service::reconcile_config_gates` / `_for`) keep resolving.
pub(crate) use config_gates::{reconcile_config_gate_for, reconcile_config_gates};
pub(crate) use pricing::marketable_exit_price;

/// Cap on `pending_venue_cancels`. The live bridge drains the queue once per
/// tick, so a healthy run never gets near it; reaching it means the bridge or
/// the venue has stopped taking cancels. Newest wins — the OLDEST entry is the
/// one dropped, and [`Core::queue_venue_cancel`] warns when it happens (#1291).
pub(crate) const MAX_PENDING_VENUE_CANCELS: usize = 256;

pub struct Core {
    pub(crate) config: CoreConfig,
    // issue #55: `reconcile` (crate::reconcile) routes ghost terminal
    // transitions through `Core::cancel` / `release_order_bookkeeping` and
    // needs `ome`; `pub(crate)` matches `config`/`accounts`/`risk`/`positions`.
    pub(crate) ome: Ome,
    /// #351: dry taker legs in flight — two-stage time-causal fill
    /// (match at `execute_at_ms`, report at `report_at_ms`). Empty
    /// whenever `fill_model.taker_latency_ms <= 0 && taker_rtt_ms <= 0`.
    inflight_takers: Vec<InflightTaker>,
    /// E28 (§9.3): the per-account money truth. "The ledger" is now a lookup
    /// keyed by the order/request's `account_id` at every money-moving site —
    /// the active account's book stands in where no order context exists
    /// (seeding, equity views, the audit's cash identity). `Ledger` itself is
    /// UNTOUCHED: reserve/release/settle keep their exact signatures.
    pub(crate) accounts: crate::account::AccountLedgers,
    pub(crate) risk: RiskGate,
    /// issue #1189: the per-account risk/exit store, when one is open. `None`
    /// = the env fallback (every test, and a kernel with no store wired).
    /// Panel writes go here; a missing row falls back to the global matrix.
    /// issue #1199 r2 (T-03, zeroed): the risk/exit write port's carrier
    /// decision (`Core::persist_risk_bounds` / `Core::persist_exit`) reads
    /// this FIRST — an open DB is the cockpit carrier (the whole merged row,
    /// the pre-seam `AccountRiskDb::save_risk` / `save_exit` write) and the
    /// `None` posture is the backtester's memory-only one.
    account_risk_db: Option<crate::risk::account_store::AccountRiskDb>,
    /// issue #1199 第六轮（操作者裁决 2026-11-14）：SQLite 后端
    /// （[`crate::store::SqliteStore`]，由 `config.db_path` 打开）——唯一
    /// 的持久化后端；`.env` 对内核只读。`None` = persistence disabled
    /// for this core (backtester, most in-process tests) — the standard
    /// "persistence is disabled" refusals read this.
    pub(crate) store: Option<Arc<dyn crate::store::StoreApi>>,
    /// issue #82: the unified order placement/cancellation seam
    /// ([`blitzkrieg_market_api::execution_api::ExecutionApi`]). `None` =
    /// no backend wired — every placement/cancel fails closed with
    /// `CoreErrorCode::Internal`; extensions fill in the venue wiring.
    pub execution_api: Option<Box<dyn ExecutionApi>>,
    /// issue #41: optional unified event publish/subscribe backend. `None`
    /// = event publishing disabled; every [`EventApi`] call on a disabled
    /// core refuses with `CoreErrorCode::Internal`. Install a `Box<dyn
    /// EventApi>` here at assembly when a venue bus backend exists.
    pub event_api: Option<Box<dyn EventApi>>,
    /// Issue #36: the unified market-data READ seam. `None` = the
    /// `MarketDataApi` backend is disabled for this core; every read then
    /// fails loud (`"market_data_api::…: no backend implements this yet"`)
    /// instead of fabricating prices.
    pub market_data_api: Option<Box<dyn MarketDataApi>>,
    /// issue #84: optional risk gate seam. `None` = the gate is disabled and
    /// every call falls back to the trait default ("no backend implements
    /// this yet"); no threshold values live in the core — a backend supplies
    /// them (read-only abstraction, issue #84 / R6: nothing here changes any
    /// risk threshold).
    pub risk_api: Option<Box<dyn RiskApi>>,
    /// issue #39: the strategy lifecycle seam (StoreApi-style). `None` =
    /// disabled — no backend wired at assembly; extensions implement
    /// `StrategyApi` and hand `Some(Box<dyn StrategyApi>)` to the
    /// composition root. The C ABI vtable is untouched (R7): pure Rust seam.
    pub strategy_api: Option<Box<dyn StrategyApi>>,
    /// Per-strategy consecutive-loss breakers (KI-10 / D-18 A). The daily loss
    /// cap and kill switch stay global — see [`LossBreakers`].
    breaker: LossBreakers,
    /// E25 (#331): the intent-audit sink — one JSONL line per arbitrated
    /// suggestion plus the throttled `INTENT_DECISION` push state (§3.4).
    intent_audit: crate::arbitration::AuditSink,
    pub(crate) positions: PositionManager,
    books: HashMap<TokenId, Book>,
    /// Traffic-economy first: one high-freq feed per (venue, family),
    /// refcounted across accounts. Placeholders are one row per
    /// (venue, family, asset) for venues that have a configured account.
    pub(crate) family_feed: std::sync::Mutex<crate::market::family_feed::FamilyFeedPlane>,
    /// The pending-close exit-reason ledger — map, insertion order and the
    /// eviction/sweep counters as one grouped state machine (issue #780).
    /// The four helpers in `settlement_orders/closing.rs` are its only way
    /// in or out.
    exit_reason_ledger: ExitReasonLedger,
    /// Strategy close intents drained from the engine in `engine_evaluate` and
    /// consumed by the shared exit-submission path in `run_exit_checks`.
    strategy_exits: Vec<crate::strategies::StrategyExitIntent>,
    /// Positions whose "exit withheld: no fresh best bid" state has ALREADY
    /// been logged (2026-11-08 pre-release find): a restored position whose
    /// venue book never freshens re-emits the intent every engine tick, and
    /// the per-tick INFO line ran at hundreds of lines/second for minutes
    /// (~2M lines from one boot). Log the STATE CHANGE, not the state: one
    /// line when the withhold first hits a position, the entry cleared as
    /// soon as a fresh bid resolves it again.
    withheld_exit_logged: std::collections::HashSet<String>,
    /// Entry-rejection tuples already logged (same 2026-11-08 find, entry
    /// side): while a gate holds (loss breaker, capacity, …) the strategy
    /// re-emits candidates every tick and the identical INFO line ran at
    /// ~34-100 lines/second for the whole episode. One line per distinct
    /// (strategy, gate, reason, detail) episode; the set is bounded — when
    /// it grows past the cap it drains, so a pathological detail variety
    /// degrades to the old per-tick logging instead of unbounded memory.
    entry_reject_logged: std::collections::HashSet<String>,
    /// 跨所套利弹窗 v2: the leg configuration in force (the `ACCOUNT_ARB_*`
    /// env grammar's memory mirror). `None` = unconfigured.
    /// Written by `arb_config_set_persisting` (`.env` + memory in one
    /// transaction), read by the engine hot-push (the adapter's account
    /// stamp + the strategy's `bk.params()` leg entries).
    arb_leg_config: Option<crate::account::CrossVenueArbConfig>,
    /// RFC 568 阶段一：内核侧配对状态簿（PairBook）。事实事件驱动的观察者
    /// 账本：engine 的提交管线在 arb_precheck 过闸后把带 pair 声明的腿
    /// declare 进来，OME 的成交/死亡事实经 on_order_event 吸收，sweep 在
    /// flatten 后落 Closed。进程内 BTreeMap，不持久化。
    pair_book: crate::pair::PairBook,
    /// Optional self-driving engine (P3). When present, book/spot/round events
    /// flow in and the core evaluates strategies and places orders on its own.
    /// Private to `service` (#780 boundary hygiene): the replay driver hosts
    /// strategies and swaps counterfactual params through the narrow seams
    /// ([`Core::host_replay_strategy`] / [`Core::set_engine_hot_params`]) and
    /// out-of-service readers take [`Core::engine`] — the crate-visible field
    /// let backtest (and any future IPC caller) bypass the install-time
    /// invariants `enable_engine` wires.
    engine: Option<crate::engine::Engine>,
    /// E27 (§8.2): market-mode declarations of loaded user strategies, keyed
    /// by strategy name. In-tree strategies and libraries without a
    /// declaration are ABSENT = undeclared = the handshake does not apply
    /// (§8.1 — every 0.2 library's state).
    strategy_declarations: HashMap<String, Vec<blitzkrieg_strategy_api::StrategyMode>>,
    /// E27 (§8.2): every active plugin's declaration snapshot, injected by the
    /// server assembly BEFORE any strategy is loaded or enabled, so all three
    /// handshake sites judge against the same view. Multi-plugin: a strategy's
    /// declaration must be satisfiable by ANY ONE of these — a venue the
    /// strategy cannot serve must not block the venues it can. Empty
    /// (backtester, tests) leaves the handshake inert: nothing to be
    /// incompatible with.
    plugin_modes: Vec<crate::compat::PluginModes>,
    /// Per-plugin subscription controls (P4 → multi-plugin data plane). The
    /// key is the market-plugin name; with one active plugin this map holds a
    /// single entry and behaves exactly as the old `feed: Option<...>` slot.
    /// A set of tokens always reaches ONLY the named plugin's control — a
    /// Polymarket rollover must never replace a Kalshi subscription set.
    feeds: std::collections::BTreeMap<
        String,
        std::sync::Arc<dyn blitzkrieg_market_api::SubscriptionControl>,
    >,
    /// Extension registry (plugin lifecycle).
    extensions: crate::extension::ExtensionRegistry,
    /// Shadow Evolution (opt-in; disabled by default).
    shadow_evolution: ShadowEvolution,
    /// Closed-trade log (Node-compatible), when `trade_log_path` is set.
    trade_db: Option<crate::trade_db::TradeDb>,
    /// Durable order log, when `order_log_path` is set. Orders are restored from
    /// it at startup so a restart never forgets resting orders (orphan guard).
    order_db: Option<crate::order_db::OrderDb>,
    /// Durable snapshot of the OPEN position book, when `position_log_path` is
    /// set. Restored at startup so a restart never forgets open positions
    /// (unmanaged-exit guard; same failure class as the orphan-order bug).
    position_db: Option<crate::position_db::PositionDb>,
    /// Feed/decision counters for observability (P4 diagnostics).
    stats: CoreStats,
    /// 环境差异排查探针（任务二 1-3）的全部内核侧记账，分组收拢（issue
    /// #780）：三个 live watcher、最近一次汇总的快照与 warn 计数。只观测
    /// ——日志 + 计数器，从不拦截数据或订单路径。
    probe_health: ProbeHealth,
    /// #377 (治 #2): conditions the archive declared to carry an on-chain
    /// verdict (`RoundMarkets` markets flagged `archive_verdict`). The dry
    /// settlement ladder must not query these markets — the stream settles
    /// them — and querying would stamp a query clock no archive answer ever
    /// resets, tripping the blind alert.
    archive_verdicts: HashSet<String>,
    /// When this core's ACCOUNTING SESSION began, in epoch ms (issue #200).
    ///
    /// A seeded (`dry`/`readonly`) core re-seeds its cash ledger here
    /// (`Ledger::set_balance(dry_seed_balance)` in `new`) while its trade log
    /// and position log outlive the process. A cold ledger beside a trade book
    /// that still holds earlier sessions answers a different question than the
    /// cash identity asks, so an out-of-process audit of a seeded core has to
    /// scope `realized` to the trades closed at/after this instant. A live core
    /// ignores it: its opening cash is the venue's, and its identity telescopes
    /// across polls instead.
    started_at_ms: i64,
    /// Optional mirror of every market-data event into a JSONL archive (P-1.3).
    /// The backtester replays this file.
    event_archive: Option<crate::data_source::EventArchive>,
    /// ④ 统一映射账本：候选配对的唯一真值源（自动配对只提议、操作员
    /// confirm 才生效）。confirmed 的事件灌进 Engine（put_unified_mapping）
    /// 供策略与 unified.list 读；本字段持有持久态与审计线。
    unified_ledger: blitzkrieg_market_api::unified::MappingLedger,
    /// ④ 统一映射：最近一轮的跨所配对候选（自动匹配只提议）。每个新轮
    /// 全量替换；confirm 成功的配对离开货架。会话内状态，不持久化 ——
    /// 下一轮会重新提议，持久化只会留下过期货架。
    unified_candidates: Vec<blitzkrieg_market_api::unified::matching::MatchCandidate>,
    /// Per-strategy order/trade accounting (P-1.1). Session-scoped: reset on
    /// restart, like `stats`; the durable per-trade record is the trade log.
    ///
    /// Keyed (strategy, account) — PR H: two accounts assigned the same
    /// strategy are two independent scopes, and the panel reads the stats of
    /// the ACTIVE account. Key form `(strategy, '\u{1}', account)`; the
    /// single-name helpers below keep the call sites readable.
    strategy_accounting: HashMap<String, StrategyAccounting>,
    /// E-VA: the strategy-level VIRTUAL quota book — per-(account, strategy)
    /// pots fed by the batch pre-reservation and drained by the order state
    /// machine's terminal transitions. Built from `config.virtual_limits`;
    /// an empty config builds an empty book whose `governed` answer is
    /// always false, so every hook below is a one-comparison no-op on the
    /// shipped default (the replay-baseline posture).
    virtual_accounts: crate::risk::virtual_account::VirtualAccounts,
    /// Venue-rejection cooldowns keyed by placement intent (`internal_key`):
    /// after the venue refuses an order, retries of the SAME intent back off
    /// exponentially and stop after a cap — a stuck exit once burned 380+
    /// rejections hammering a venue that kept refusing at full tick rate.
    place_cooldowns: HashMap<String, RejectCooldown>,
    /// Orders whose escalation this process already reported as skipped because
    /// the cross the book offers is refused by the risk gate (#261). The sweep
    /// retries on every maker timeout, so without this the same unaffordable
    /// cross would reprint the same line every few seconds. An id leaves the
    /// set as soon as the order stops being live, or when a later escalation
    /// actually goes through.
    escalation_skips: HashSet<String>,
    /// Venue cancels the core has already committed to locally (order marked
    /// Cancelled by a regime pull, round cleanup, escalation or explicit
    /// cancel) while the venue may still hold the order resting. The live
    /// bridge drains this and issues the actual venue cancels — retiring an
    /// order locally without telling the venue leaves a resting orphan.
    /// Each entry carries the owning venue (read off the order token's round
    /// market at enqueue time; `""` = untagged) so the multi-plugin drain can
    /// hand every id to ITS venue's executor — the queue is take-once, and a
    /// sibling executor swallowing another venue's id would strand the cancel
    /// forever.
    pending_venue_cancels: Vec<(String, String)>,
    /// 冻结/熔断状态簿（#834，Core 上帝结构第一环）：account-scoped kill
    /// book（account id → reason）、#685 冻结自愈簿、venue 拒单连击（全局
    /// +按账户）、venue 维 sweep 连败（全局+按场地）、venue 自由现金簿。
    /// 冻结/熔断剩余状态簿（#834）：`account_freezes` + `auto_frozen` 已被
    /// `account_gates` 单一闸门取代（#1045），本簿只保留全局/场地维连击
    /// 与自由现金事实；唯一管理者是 [`FreezeBook`]（risk_admission/freeze.rs）。
    freeze_book: FreezeBook,
    /// #1045: 唯一账户入场闸门，每账户一份，纯内存，永不落盘，重启即清。
    /// 所有入场路径只读 `gate.blocked`；出场永不阻断。
    account_gates: HashMap<String, AccountGate>,
    /// #187: the VENUE axis of the same gate. `account_gates` can only hold a
    /// halt that has an account to hang it on; a venue failure with no bound
    /// account had nowhere to land, and the old code escalated it to
    /// `GLOBAL_KILL_ACCOUNT` — one venue's blip stopped every venue. This table
    /// keys on the venue itself, so that shape has a home. Entries are
    /// `HaltScope::Venue` only (enforced by `VenueHalts::halt`), and only
    /// `risk.kill` (an operator act) can still reach the global gate.
    venue_halts: VenueHalts,
    /// #861: the runtime risk state's durable mirror (`Option<RiskStateCell>`
    /// — path + parsed file). `None` (backtester, most tests) = the historic
    /// session-scoped behaviour: no risk knob is written or restored, and no
    /// restart default-freeze applies. `Some`: every kill/freeze/limit write
    /// lands in `positions.risk-state.json`, and a boot WITH a state file
    /// restores caps/freezes and default-freezes every account until an
    /// operator resumes it (the fail-closed restart, risk_admission/persist.rs).
    risk_state: Option<RiskStateCell>,
    /// Last error of any kind recorded this session (venue refusal, failed
    /// self-check or reconcile sweep, kill switch, kernel-side order refusal),
    /// surfaced to the panel as `engine.stats.lastError`. Session-scoped: a
    /// restart starts with no error, and nothing clears it but a new one.
    last_error: Option<LastError>,
    /// Newest external (unknown-order) fill timestamp already folded into the
    /// position book; persisted next to the position log.
    recon_watermark_ms: i64,
    /// Whether this core's boot REPLAYED durable cash journals (at least one
    /// `data/ledger/<account>.journal.<live|dry>.jsonl` existed on disk, issue
    /// #941). The server's
    /// post-construction seed reads it: a replayed balance is a boot fact and
    /// must not be re-seeded over. Always false when
    /// `config.ledger_journal_dir` is `None`.
    boot_replayed_journal: bool,
    /// Newest trading-capability self-check report (panel visibility).
    last_self_check: Option<blitzkrieg_market_api::SelfCheckReport>,
    /// Durable journal of the applied-fill idempotency table (issue #178). The
    /// OME never touches the filesystem; the service drains its mutation journal
    /// after every applied fill and appends it here, and restores the table from
    /// it at startup.
    applied_log: Option<AppliedFillLog>,
    /// In-kernel accounting audit state (issue #189).
    audit: AuditRuntime,
    /// Settlement + redemption book (issue #175): which positions a market
    /// resolution has settled (durable, so a second round cannot re-book them),
    /// and the claims whose collateral is still on-chain. In dry mode the core
    /// resolves its own markets and simulates the redemption; in live mode the
    /// venue plugin answers and redeems.
    settlement: SettlementBook,
    /// Issue #991 boundary 1: the receivable-aging warn's edge memory — the
    /// `oldest_fallback_claim_age_ms` value the last warn was emitted against
    /// (0 = no warn outstanding). The warn fires when the oldest claim crosses
    /// `receivable_max_age_ms`, and only re-fires when the claim is redeemed
    /// and a NEW one ages past the bound again (the alert is per-episode, not
    /// per-tick; the continuous face is the panel's amber banner, which reads
    /// `oldestFallbackClaimAgeMs` every snapshot). Session-scoped: a restart
    /// may re-warn once, which is honest — the operator was away.
    receivable_warned_age_ms: i64,
    /// The event archive's recording state as THIS core last saw it (issue
    /// #669): `true` = recording (and the boot assumption — an archive that
    /// opened stopped — still arms the edge below, because `stop()` may also
    /// have fired at open). On each tick the archive's live `status().recording`
    /// is folded through here, and a `true → false` edge emits ONE RiskAlert —
    /// the disk-floor/lock/io stop is otherwise a single journal warn line that
    /// a 24/7 operator finds days later as missing corpus. Staying false is
    /// silent (the alert fired once; the banner reads `stats.archive`), and a
    /// defensive `false → true` reset just re-arms the detector.
    archive_recording_last: bool,
    /// 幽灵成交探测器（issue 692）：已告警过的 venue → 上次告警的残差
    /// （厘粒度 mantissa）。#702 修订：去重键 = 同一个未解释残差 —— 同一
    /// 条腿的 reconcile 周期重放只响一次；残差变化（叠加新异动）或先恢复
    /// （|残差|≤ε 清槽）后的新异动，重新响。
    ghost_cash_alerted: std::collections::HashMap<String, i128>,
    /// #702: each venue's own last reported number — the previous side of the
    /// move the NEXT report is judged against. The ledger balance is NOT this:
    /// a fill the venue already reported moves the book between venue reports,
    /// and comparing the new report to the book (instead of to the venue's own
    /// last word) would read a fully-explained correction as an anomaly.
    venue_truth_last: std::collections::HashMap<String, Decimal>,
    /// issue 702: per venue, the GLOBAL row seq of the last journal row the venue's
    /// truth reports had already ANSWERED FOR — the evidence window cursor.
    /// Rows with a higher seq are facts the venue's current number has not yet
    /// been checked against; rows at or below it are reconciled history. A
    /// CHANGED report advances the cursor (the delta was judged against
    /// everything before it); an EQUAL report deliberately keeps the cursor —
    /// facts booked between two equal reports happened after the venue's last
    /// word and belong to the still-open window.
    ///
    /// A cursor, not a timestamp: journal rows stamp real wall clock while
    /// replay/tests drive virtual clocks — comparing the two bases silently
    /// swallows every fact.
    ///
    /// issue 723: the seq, not the row COUNT. The journal is a bounded window
    /// whose length freezes at [`crate::ledger::JOURNAL_CAP`] — a count-keyed
    /// cursor skipped everything forever after (booked read 0, every legal
    /// correction read as unattributed). The global seq = total rows ever
    /// appended − window length + index (see
    /// `Ledger::journal_seq_basis`), monotonic across eviction; the counter
    /// is in-memory only and boot replay anchors it to the loaded row count.
    /// Entries die with their ledger generation: account remove and account
    /// reset clear the entries of the affected venues — a rebuilt ledger's
    /// rows must never be judged by the previous generation's numbers.
    venue_fact_cursor: std::collections::HashMap<String, u64>,
    /// F4: close-accounting credentials for fill-driven full closes, keyed by
    /// the OME trade key. A MATCHED sell that emptied a position books TradeDb /
    /// strategy PnL / breaker immediately; if the venue later reports that same
    /// trade FAILED, the credential is what lets the service unwind each of
    /// those ledgers instead of leaving phantom profit behind. FIFO-capped:
    /// the FAILED normally arrives seconds after the fill that created it.
    ///
    /// #463: the queue is JOURNALED — every push appends a line to
    /// `close-credentials.jsonl` (beside the order log, same derivation as the
    /// other side logs) and every consumption appends a `consumed` line. A
    /// restart between the close fill and the venue's FAILED used to lose the
    /// credential (the position was already out of every recovery file) and
    /// strand phantom PnL; now boot replays the journal, keeping only lines
    /// whose key has no later `consumed`.
    close_credentials: std::collections::VecDeque<(String, CloseCredential)>,
    /// #463: where the credential journal appends. `None` (no order log — the
    /// backtester, most tests) keeps the queue memory-only, as before.
    close_credential_log: Option<std::path::PathBuf>,
    next_id: u64,
    tx: Option<mpsc::UnboundedSender<Event>>,
    /// E29 (§12.2): per-`(symbol, interval)` wall-clock (ms) of the LAST
    /// throttled push of that bucket's still-growing bar. Only the throttle
    /// state lives here — the bars live in the engine's aggregator; the map
    /// is touched only where an engine exists, so a core without one
    /// allocates nothing.
    kline_preview_last_ms: std::collections::HashMap<(String, crate::kline::KlineInterval), i64>,
    /// #363: the account-isolated execution policy. Loaded fail-closed at
    /// boot (a malformed file refuses the boot, with a `load-refused` audit
    /// line), replaced WHOLESALE on an IPC set/reset (hot reload — no
    /// restart), and consulted by `engine_evaluate` before an entry order is
    /// placed. `None` only when the config file is absent (the zero-config
    /// deployment): every default then matches the historical hardcoded
    /// behaviour bit for bit.
    execution_policy: Option<crate::execution_policy::Policy>,
    /// #757: the POLICY_* keys the IPC write face has made authoritative in
    /// THIS process (`Some` = the value written, `None` = swept by a reset /
    /// an absent section field). The launcher exported the boot `.env` into
    /// the process environment, so a same-process reload that read the
    /// process env first would judge under the OLD value: `set` wrote 5, the
    /// file said 5, and the effective policy still read 100. `reload_
    /// execution_policy` resolves these keys from here; keys never written
    /// through IPC are absent and keep resolving from the deployment env.
    execution_policy_env_writes: std::collections::BTreeMap<String, Option<String>>,
    /// #363: account-level cooldowns a policy rule opened, keyed by account
    /// id, holding the ms instant entries may resume. Checked (and expired)
    /// in `engine_evaluate`; set by the same path when a rule answers
    /// `PolicyOutput::Cooldown`.
    policy_cooldowns: HashMap<String, i64>,
    /// Replay fast path (`backtest_fast` / nofast replay scheduling) state,
    /// grouped (issue #780): dirty-input wakes and the deadline stamps the
    /// maintenance passes schedule against. Consumed and recomputed by the
    /// engine cycle; nothing here is observable behaviour — the flags only
    /// decide WHICH ticks do work.
    fast_path: ReplayFastPath,
    /// #531 pair-chase state: the chase this scope last offered for a naked
    /// leg, keyed (account, strategy, naked_token) → (missing_token, at_ms).
    /// The window reader (`run_pair_orphan_sweep`) is the only consumer — a
    /// chase placed within its window buys the leg the benefit of the doubt
    /// before the sweep's flatten fallback fires, and the record retires
    /// when the fallback does. Process-local (an in-memory debounce, not a
    /// durable fact): a restart simply re-judges from the book.
    pair_chase_attempts: HashMap<(String, String, String), (String, i64)>,
    /// B1c test seam: a staged (venue, duration) subscription set the
    /// admission gate reads instead of the (still empty, pre-B1d) real one.
    /// `None` outside tests and before staging — the gate then reads the
    /// constant empty set and admits everything (today's behaviour).
    #[cfg(test)]
    test_duration_subscriptions: Option<HashSet<(String, i64)>>,
    /// #774 test seam: a fill staged to land exactly between the escalation
    /// sweep's size snapshot and its maker cancel. Consumed once by
    /// `run_escalation_sweep`; `None` outside tests.
    #[cfg(test)]
    test_escalation_snapshot_fill: Option<Fill>,
}

/// The pending-close exit-reason ledger (issue #190), grouped as one state
/// machine (issue #780): the map keyed by token, its insertion order, and the
/// two counters the bound's clean-up paths feed. The co-occurrence survey of
/// the Core fields put this family at the top: every method that touches one
/// of these fields touches only the others — no method mixes them with any
/// outside field — so the group is self-contained by construction.
struct ExitReasonLedger {
    /// Exit reason chosen for an in-flight closing SELL, keyed by token id, so a
    /// full sell fill closes the position with the right reason.
    ///
    /// A PENDING-CLOSE ledger, not a history (issue #190): an entry exists only
    /// while the token still has an open position, and the table can never
    /// exceed [`EXIT_REASON_TABLE_MAX`]. A leaked entry would be inherited by
    /// the NEXT position on the same token and mis-attribute its exit.
    exit_reasons: HashMap<TokenId, ExitReason>,
    /// Insertion order of [`Self::exit_reasons`], oldest first. The map cannot
    /// name its own oldest entry and the bound has to evict one; kept in
    /// lockstep with the map's key set by the four helpers in `closing.rs`.
    exit_reason_order: VecDeque<TokenId>,
    /// Entries dropped by the [`EXIT_REASON_TABLE_MAX`] bound, session-scoped
    /// and surfaced in `engine.stats`: an eviction loses real exit attribution,
    /// so it is counted rather than silent.
    exit_reasons_evicted: u64,
    /// Entries dropped because their token no longer has an open position — the
    /// expected path (a close, a round rollover).
    exit_reasons_swept: u64,
}

/// 环境差异排查探针（任务二 1-3）的内核侧记账（issue #780 分组）：三个
/// live watcher、最近一次汇总的快照与 warn 事件计数。只观测——日志 +
/// 计数器，从不拦截数据或订单路径；所有时间判定吃调用方的 `now_ms`
/// （回放里是虚拟时钟）。NTP 的网络查询不在这里：`probe::spawn_loop`
/// 只由生产的 `server::run()` 启动，测量结果经 `note_ntp_offset` 灌回。
struct ProbeHealth {
    probe_ticks: crate::probe::TickHealth,
    probe_spot: crate::probe::SpotWatch,
    probe_ntp: crate::probe::NtpWatch,
    /// 最近一次汇总的窗口 tick 数（stats 面直读 probe_ticks 会拿走窗口，
    /// 汇总清零——所以汇总瞬间留档）。
    probe_last_window_ticks: u64,
    /// 最近一次汇总点名的零 tick token（已排序）。
    probe_last_zero: Vec<String>,
    /// 最近一次汇总的最长静默 ms。
    probe_last_max_silence_ms: i64,
    /// 最近一次汇总时 spot 的停更时长（None = 从未见 spot）。
    probe_last_spot_stale_ms: Option<i64>,
    /// 断流 warn 总次数与 spot 停更 warn 总次数（事件计数，独立于
    /// probe_ticks.broken 的当前状态）。
    stats_probe_stream_breaks: u64,
    stats_probe_spot_stales: u64,
}

/// Replay fast path (`backtest_fast`) state, grouped (issue #780): the dirty
/// input sets and wake flags the engine cycle consumes, plus the deadline
/// stamps (`*_ms`) every maintenance pass recomputes and schedules against.
/// Consumed (cleared / recomputed) by each pass; a quiet tick skips the work
/// until a deadline passes.
struct ReplayFastPath {
    /// Replay fast path: tokens whose book changed in the
    /// latest `engine_on_data` or fill application — the only positions whose
    /// marks can move on the next maintenance tick, and the only cycles a
    /// Lua evaluation can change its output on (entry eligibility demands a
    /// fresh book). Consumed (cleared) by each maintenance tick.
    fast_dirty_tokens: HashSet<String>,
    /// Replay fast path: an engine INPUT arrived since the last real Lua
    /// evaluation (book / top-of-book / spot / round rollover) — the only
    /// cycles whose evaluation output can differ from the previous one.
    /// Consumed by `engine_evaluate`.
    fast_eval_wake: bool,
    /// Replay fast path: the next virtual instant a round-timing verdict can
    /// flip (min-round-age / min-time-left boundaries). Recomputed after every
    /// real evaluation and round rollover; quiet ticks skip the Lua call
    /// until this instant.
    fast_next_timing_ms: i64,
    /// Replay fast path: the live-order set changed (place / cancel / any
    /// position open or close) — wakes the escalation sweep and the audit-
    /// relevant sweeps that a quiet tick would otherwise skip.
    fast_orders_dirty: bool,
    /// Replay fast path: the open-position set changed — wakes the
    /// settlement watch sync (a new position's expiry must join the watch
    /// list when it passes, exactly when the full scan would have added it).
    fast_positions_dirty: bool,
    /// Replay fast path: earliest `escalate_at_ms` over live orders
    /// (`i64::MAX` = none). Recomputed whenever the escalation sweep runs.
    fast_next_escalation_ms: i64,
    /// Replay fast path: earliest `expires_at_ms` over open positions
    /// (`i64::MAX` = none) — when it passes, the settlement watch must sync
    /// even with no event in sight. Recomputed on position-set changes.
    fast_next_expiry_ms: i64,
    /// Replay fast path: virtual time of the last Lua evaluation. Quiet ticks
    /// skip the call until `FAST_EVAL_FALLBACK_MS` elapses (a safety net for
    /// strategies with internal time-based state; entry eligibility itself
    /// always rides the dirty-token wake).
    fast_last_eval_ms: i64,
    /// Issue #390 (replay nofast): the next virtual instant one of the
    /// every-tick maintenance passes whose OUTPUT the replay observes can
    /// change — the earliest open position's expiry passes (`TimeExit`/
    /// `ForceExit`/`StaleProfit`/`StagnantProfit`/settlement track, all of
    /// them clocks), or the 1 s throttle beats (suppressed-stop reports, held
    /// exits, accounting audit). The exit LADDER is scheduled against this
    /// deadline, dirty tokens, and its own 1 s report beat; everything else
    /// this flag covers already rides its own deadline in `tick`.
    /// `i64::MAX` = nothing scheduled.
    fast_next_exit_deadline_ms: i64,
    /// Event-driven live evaluation: `Engine::inputs_fingerprint` as read
    /// just before the last real Lua evaluation (after the host pushes).
    live_eval_fingerprint: u64,
    /// Event-driven live evaluation: the first instant a book crosses the
    /// freshness limit, as known at the last real evaluation (`i64::MAX` =
    /// none).
    live_next_stale_ms: i64,
    /// Event-driven live evaluation: the last real evaluation produced
    /// strategy exits or pair declarations/refusals — the next cycle must be
    /// real (those emissions are not part of the staged candidate set).
    live_force_eval: bool,
}

/// F4: what a fill-driven full close changed, so a later FAILED status for the
/// same trade can reverse every one of those ledgers exactly.
///
/// #463: `Serialize`/`Deserialize` because the credential is journaled — the
/// queue alone dies with the process, and a restart between the close fill and
/// the venue's FAILED is precisely the window this struct exists for.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloseCredential {
    /// The position-book row as it stood BEFORE the closing fill removed it —
    /// the exact inventory to restore.
    restored: crate::position::OpenPosition,
    /// The closed record `on_position_closed` booked (TradeDb / accounting /
    /// breaker / daily PnL), for unwinding.
    closed: crate::position::ClosedPosition,
    /// The strategy breaker's consecutive-loss count just before this close
    /// was recorded: a reversed WIN had reset the streak, and that is the only
    /// recoverable value (retract restores it).
    streak_before: u32,
    /// E26 (§4.2): the ACCOUNT-level streak's count just before this close —
    /// the same recovery fact for the reserved `__account__:` key. Captured
    /// even when the account bound is off (a zero count is the truth then),
    /// so a close cannot choose whether to leave evidence.
    account_streak_before: u32,
}

/// #463: one journal line for the credential log — a push (with the credential)
/// or a consumption tombstone (key only). Boot replays pushes whose key has no
/// later tombstone.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CloseCredentialLine {
    Push {
        trade_id: String,
        #[serde(flatten)]
        cred: Box<CloseCredential>,
    },
    Consumed {
        trade_id: String,
    },
}

/// Replay fast path: a quiet tick still re-runs the Lua evaluation at least
/// this often (virtual ms), as the safety net for strategies whose decisions
/// read the round clock in ways the timing boundary does not model. The
/// boundary wake (`fast_next_timing_ms`) and the dirty-input wake cover the
/// proven inputs; this bounds anything else to a 2 s staleness.
const FAST_EVAL_FALLBACK_MS: i64 = 2_000;
/// Issue #390 (replay nofast): the exit ladder re-runs at least this often on
/// quiet ticks. Bounds every THROTTLED report the ladder feeds — suppressed
/// stops (`stop_suppression_repeat_sec` in shipped configs is 1 s+; the
/// throttle itself decides what is emitted) and the held-exit / escalation
/// reports (`check_exits` stamps them from its own run) — so their
/// time-quantised stamps stay exactly what the every-tick pass produced.
pub(super) const EXIT_REPORT_BEAT_MS: i64 = 1_000;

/// ④ 统一映射：一个 listing 的面板形状（`unified.candidates` / confirm
/// 请求体共用）。`PlatformListing` 本身可 serde，但表单字段名直接固化在
/// 这里，避免把 Rust 内部字段漂移暴露成协议。
pub(super) fn listing_json(
    l: &blitzkrieg_market_api::unified::PlatformListing,
) -> serde_json::Value {
    serde_json::json!({
        "venue": l.venue,
        "conditionId": l.condition_id,
        "upTokenId": l.up_token_id,
        "downTokenId": l.down_token_id,
        "asset": l.asset,
        "title": l.title,
        "settlementRules": l.settlement_rules,
        "settlementSource": l.settlement_source,
        "expiresAtMs": l.expires_at_ms,
        "status": l.status,
    })
}

/// ④ ConfirmError has no Display (the ledger crate keeps refusals as data);
/// the IPC layer needs one operator-facing line per variant.
pub(super) fn confirm_error_text(
    e: blitzkrieg_market_api::unified::ledger::ConfirmError,
) -> String {
    use blitzkrieg_market_api::unified::ledger::ConfirmError;
    match e {
        ConfirmError::NeedsTwoVenues => {
            "confirm refused: a mapping needs at least two distinct venues".to_string()
        }
        ConfirmError::DuplicateVenue(v) => {
            format!(
                "confirm refused: two listings from the same venue ({})",
                v.as_str()
            )
        }
        ConfirmError::TitleTooDissimilar { a, b, similarity } => {
            format!("confirm refused: titles too dissimilar ({similarity:.2}) between {a} and {b}")
        }
        ConfirmError::AssetMismatch {
            a,
            b,
            a_asset,
            b_asset,
        } => format!(
            "confirm refused: different assets ({a_asset} vs {b_asset}) between {a} and {b}"
        ),
        ConfirmError::AlreadyMapped {
            venue,
            condition_id,
            event_id,
        } => format!(
            "confirm refused: {} listing {condition_id} is already mapped in event {event_id}",
            venue.as_str()
        ),
    }
}

// ── 跨所套利弹窗 v2: the arb leg configuration surface ──────────────────

// ── RFC 568 阶段一：PairBook 的内核接线面 ───────────────────────────────

// ── 账户↔venue 解耦：per-account strategy ENABLEMENT 语义层 ─────────────
//
// 语义裁决（本轮）：不存在「为账户分配策略」的呈现逻辑 —— 策略全局可见；
// operator `.env` 的 ACCOUNT_<ID>_STRATEGIES 记录的是每账户×策略的启用状态（面板启用
// 按钮的持久化）。scope (strategy, account) 运行 ⇔ 策略已安装 AND 账户
// 已启用 AND 策略 ∈ 该账户启用集。被停用的 scope 若有持仓 → exits-only：
// 不再开新仓，TP/SL/挂单巡逻/退出意图照常管理到平完。以下三个原语是
// server.rs 的 handler 接线面（strategy.enable / strategy.disable /
// strategy.list 的语义半）；wire 形状由 ipc 层负责。

// ── 策略 tunables 读/写（issue #481 G3）─────────────────────────────────

// ── 环境差异排查探针（任务二 1-3）───────────────────────────────────────

// ── E-VA: strategy-level virtual accounts ───────────────────────────────
//
// The virtual quota book itself never appears in the placement pipeline —
// these helpers are the ONLY seams. Each one short-circuits on
// `governed`, which is false for every scope `config.virtual_limits` does
// not name, so an unconfigured kernel takes the exact historical path.

// ── Live venue entry points ─────────────────────────────────────────────
// In DRY the core synthesises fills itself. In LIVE the CLOB adapter drives
// these same OME/ledger paths from venue acknowledgements and the user-WS
// stream, so dry and live share one authoritative state machine.

// ── In-kernel accounting audit (issue #189) ─────────────────────────────
// The kernel's own three-way money check, on a timer: the ledger's books
// against themselves, against the trade records, and against the venue's
// reported free cash. A failure alerts, persists a record and blocks NEW
// entries — deliberately not the kill switch, which (see #174) blocks closes
// too and would trap the position the drift is about.

// ── Pending-close exit reasons (issue #190) ─────────────────────────────
//
// The table remembers which reason an in-flight closing SELL was submitted
// with, so the fill that completes the close records the right attribution.
// An entry therefore lives exactly as long as the close it belongs to, and
// these four helpers are the only way in or out — `exit_reasons` is never
// touched directly. Before #190 the table was only ever inserted into: an
// exit that never filled (cancelled, replaced by a settlement, a venue pull)
// left its reason behind for the NEXT position on that token to inherit.

// ── #457: ghost orders die on the clock, not only on the sweep ───────────
//
// The reconciliation sweep only closes a ghost when a snapshot proves the
// venue forgot it. But the dominant ghost on this market needs no proof at
// all: an order resting on a round that has EXPIRED can never fill — its
// market is settlement-dead — and if the venue never acks the cancel, the
// order haunted `orders.list` forever. The kernel now closes those itself:
//   * an order whose round ended (round_slot N covers [N·dur, (N+1)·dur))
//     is ForceClosed at the tick that observes the expiry — the kernel
//     closing an order nobody else closed. In DRY the mark IS the cancel
//     (nothing external can hold it); in LIVE the venue-side cancel still
//     rides through `queue_venue_cancel` as evidence-gathering.
//   * `force_close_order` is the manual verb: same semantics, operator
//     intent, but REFUSES live orders that could still fill — a manual
//     force-close is for ghosts, not for dodging a cancel.
//

// ── Settlement & redemption (issue #175) ────────────────────────────────
//
// A position that survives to its market's resolution used to sit in the
// book forever: never closed, never paid, with its cash permanently short.
// This section closes it at the resolution's payout price and books that
// payout as a RECEIVABLE — never as cash. The distinction is the whole
// design: the wallet does not hold the money until the on-chain redemption
// confirms, and crediting `balance` early would make the ledger claim cash
// the venue does not have, which is exactly what the live leg of the
// accounting audit reads as divergence (and halts entries over). The
// receivable is part of `CashIdentity::total`, so the anchored identity
// stays true at every instant: settlement moves the payout into `total`
// against the realized PnL of the close, redemption moves it from the
// receivable into `balance` without changing `total`.
//
// Booking is idempotent on two levels, because a settlement is real money:
// an in-memory applied set (this process) and an append-only journal whose

fn parse_direction(s: &str) -> SignalDirection {
    match s.to_ascii_lowercase().as_str() {
        "down" => SignalDirection::Down,
        _ => SignalDirection::Up,
    }
}

/// Instantiate ONE account scope of a registered Lua strategy from its
/// package dir (the source the default scope recorded at load: `lua:<dir>`).
/// Returns `None` when the source is not a reloadable Lua package (a dylib
/// or a vanished dir) — the scope-rebuild caller skips the name with a
/// warning, it never panics. Mirrors `load_lua_package_inner`'s adapter
/// wiring: the manifest declares (holds/exemptions) + the kernel's ONE fee
/// schedule stamped into the Lua state.
pub(super) fn build_lua_scope(
    sources: &std::collections::HashMap<String, String>,
    name: &str,
    account: &str,
) -> Option<Box<dyn crate::strategies::EngineStrategy>> {
    use crate::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
    let source = sources.get(name)?;
    let dir = source.strip_prefix("lua:")?;
    let loaded = load_lua_package(std::path::Path::new(dir)).ok()?;
    if loaded.name != name {
        // The dir drifted (another package now lives there): refuse — a
        // scope must host the strategy the assignment names.
        return None;
    }
    let mut adapter = LuaEngineAdapter::scoped_new(
        loaded.strategy,
        loaded.tunables.clone(),
        account.to_string(),
    );
    adapter = adapter
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
        .declare_atomic_batch(loaded.atomic_batch)
        // RFC-645 A-1: the arbiter package's statistics declaration follows
        // the name, exactly as the default-scope load path stamps it.
        .declare_pair_book_stats(name == ARB_STRATEGY);
    let sched = crate::exit_policy::fee_schedule();
    adapter.set_fee_schedule(blitzkrieg_lua_runtime::FeeScheduleView {
        name: sched.name.to_string(),
        rate: sched.rate.to_string(),
        exponent: sched.exponent,
    });
    Some(Box::new(adapter))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::risk::RiskConfig;
    use crate::settlement::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    use rust_decimal_macros::dec;

    // The shared fixtures (dry_core / order / reset_params / reset_audit_dir /
    // live_core) and every original inline test moved verbatim (pure code
    // motion, no logic edits) to `service_inline_tests.rs`, keeping this file
    // under the size cap. The child reaches the items declared here through
    // `use super::*`, exactly like every sibling theme file.
    mod service_inline_tests;
    pub(crate) use service_inline_tests::{
        dry_core, live_core, order, reset_audit_dir, reset_params,
    };
    mod account_gate_tests;
    mod account_precision_tests;
    mod audit_fix_tests;
    mod balance_trust_749_tests;
    mod books_mirror_tests;
    mod boot_carrier_gate_tests;
    mod exit_reason_table_tests;
    mod expiry_fallback_tests;
    mod fee_quote_tests;
    mod fill_model_tests;
    mod freeze_self_heal_tests;
    mod ghost_cash_move_tests;
    mod ghost_fill_tests;
    mod halt_scope_tests;
    mod journal_cursor_tests;
    mod maker_rebate_tests;
    mod mode_snapshot_tests;
    mod order_state_machine_462_tests;
    mod pair_entry_maker_tests;
    mod postonly_entry_maker_tests;
    mod receivable_991_tests;
    mod reconcile_attribution_tests;
    mod release_validation_tests;
    mod risk_bottom_line_963_tests;
    mod round_expiry_tests;
    mod round_family_b1b_tests;
    mod round_grid_pool_1272_tests;
    mod settlement_service_tests;
    mod shadow_evolution_tests;
    mod strategy_dispatch_tests;
    mod strategy_family_binding_tests;
    mod strategy_params_tests;
    // #183: the subscription-union falsification suite (declared after
    // `tests_subscription`, whose module doc carries the harness background).
    mod subscription_union_tests;
    mod tests_account_reset;
    mod tests_place_fill;
    mod tests_reset_trading;
    mod tests_strategy_lib;
    mod tests_subscription;
    mod tests_venue_lifecycle;
    mod trading_capability_tests;
}
