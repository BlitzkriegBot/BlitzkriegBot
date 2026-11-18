//! Auxiliary configuration types: per-strategy limits, virtual account
//! limits, Shadow Evolution tuning, and the durable/replay records.
//! Moved verbatim from `config.rs` (pure code motion, no logic edits).

use crate::model::*;
use rust_decimal::Decimal;

/// Entry caps and sizing for one strategy (E2-a). Every field is optional;
/// `None` = inherit the global value (no cap / global sizing). A per-strategy
/// sizing override can only tighten the global risk band, never widen it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StrategyLimit {
    /// Max simultaneously open positions held by the strategy.
    pub max_open_positions: Option<usize>,
    /// Max notional (USD, sum of entry cost) held open by the strategy. A new
    /// entry is rejected when open notional + its own notional would exceed it.
    #[serde(with = "crate::decimal::opt", default)]
    pub max_open_notional_usd: Option<Decimal>,
    /// Target notional per entry (USD). Clamped to the global `size_usd`.
    #[serde(with = "crate::decimal::opt", default)]
    pub size_usd: Option<Decimal>,
    /// Floor on shares per entry. Raised to the global `min_shares` when lower.
    #[serde(with = "crate::decimal::opt", default)]
    pub min_shares: Option<Decimal>,
    /// Ceiling on shares per entry. Clamped to the global `max_shares`.
    #[serde(with = "crate::decimal::opt", default)]
    pub max_shares: Option<Decimal>,
    /// Relative allocation weight (E16/#98): a leg with a weight gets a weighted
    /// share of the per-entry budget instead of an identical one. Scales the
    /// leg's own size override or the global `size_usd`; a weight can only
    /// shrink the budget, and a non-positive weight disables the leg's entries
    /// entirely.
    #[serde(with = "crate::decimal::opt", default)]
    pub size_weight: Option<Decimal>,
    /// This leg's own equity-relative budget (#202). `None` = the global
    /// `size_pct`; a value is clamped to it when the global is armed, and
    /// stands alone when the global is off.
    #[serde(with = "crate::decimal::opt", default)]
    pub size_pct: Option<Decimal>,
}

impl StrategyLimit {
    /// The sizing knobs this limit overrides (`None` across the board when it
    /// configures only caps — the engine then uses the globals).
    pub fn sizing(&self) -> crate::engine::StrategySize {
        crate::engine::StrategySize {
            size_usd: self.size_usd,
            min_shares: self.min_shares,
            max_shares: self.max_shares,
            size_weight: self.size_weight,
            size_pct: self.size_pct,
        }
    }
}

/// E-VA: one strategy's VIRTUAL quota — a risk-layer budget on the
/// (account, strategy) axis that exists NEXT TO the real cash ledger, never
/// inside it. A strategy declaring `atomic_batch` (and listed here) reserves
/// its whole candidate batch against this pot BEFORE the first leg goes out;
/// an over-cap batch is refused whole, through the same StrategyLimit
/// rejection accounting (`limit.virtualCap` bucket) — pair legs land
/// together or not at all.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualAccountLimit {
    /// The pot's ceiling: the strategy's combined outstanding virtual
    /// reservation on each listed account (its resting reservations plus any
    /// not-yet-released batch hold). `None` = no virtual quota (the strategy
    /// keeps the per-order real-ledger path).
    #[serde(with = "crate::decimal::opt", default)]
    pub max_batch_notional_usd: Option<Decimal>,
    /// Which account scopes this strategy's pot covers. Empty = the pot is
    /// not armed anywhere (the default-off posture).
    #[serde(default)]
    pub accounts: Vec<String>,
}

/// Overridable Shadow Evolution thresholds (all optional).
#[derive(Debug, Clone, Default)]
pub struct ShadowEvolutionTuning {
    pub min_sample_count: Option<u32>,
    pub min_win_rate_improvement: Option<Decimal>,
    pub min_profit_factor_improvement: Option<Decimal>,
    pub min_observation_secs: Option<i64>,
    pub cooldown_secs: Option<i64>,
    pub variant_count: Option<usize>,
    /// Rolling metrics window (seconds). Settable from the config file, which
    /// expresses it in minutes (`evaluation_window_minutes`).
    pub evaluation_window_secs: Option<i64>,
    /// Per-evolution step ceiling (Lock 2). `main` clamps this to the built-in
    /// ceiling before it gets here — a config file may tighten the gradient lock
    /// but nothing may widen it.
    pub max_gradient: Option<Decimal>,
    /// Directory for the per-strategy audit files (`<dir>/<strategy>.jsonl`).
    /// Defaults to `data/evolution`; a harness points it at a scratch directory
    /// so an experiment never writes into the operator's real audit history.
    pub audit_dir: Option<String>,
    /// E13 (#95): start in the unattended mode (`auto_evolve`). The switch is
    /// also runtime-toggleable and its LAST runtime state is persisted, so this
    /// only seeds the very first boot.
    pub auto_evolve: Option<bool>,
    /// Seconds between DEEP evolution rounds (the config file expresses this
    /// in minutes; `main` converts).
    pub evolution_cycle_secs: Option<i64>,
    /// Seconds an undecided proposal stays decidable (default 7 days).
    pub proposal_ttl_secs: Option<i64>,
    /// Knobs one DEEP-cycle variant moves simultaneously (>= 1).
    pub deep_dims: Option<usize>,
}

/// The kernel's last recorded error, as `engine.stats.lastError` exposes it.
///
/// One slot with exactly one writer ([`Core::note_error`]): whichever path went
/// wrong last — venue refusal, safety net, or the kernel's own refusal of a leg
/// — the panel banner and any outside observer read the same fact, with the code
/// that classifies it and the instant it happened. Before #180 the only such
/// slot was `last_venue_error`, which the taker rejection below never touched,
/// so `lastError` stayed empty (or stale) while orders were being refused.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastError {
    pub ts_ms: i64,
    pub code: CoreErrorCode,
    pub message: String,
}

impl LastError {
    /// How the LEGACY `engine.stats.lastVenueError` key renders this record.
    ///
    /// The shipped panel (`ui/webapp/webui/src/pages/Overview.vue`) renders this
    /// string as free text, and that is the compatibility this method owes: the
    /// shape `{ tsMs, message }` and the "last error, newest wins" instant are
    /// unchanged, so the banner keeps working without a frontend change.
    ///
    /// It is NOT byte-identical to the pre-#180 strings on every path, and the
    /// earlier claim that it was is wrong. The venue-reject path is identical
    /// (`{:?}` on the code, a colon, the message). The self-check and reconcile
    /// paths carried NO code prefix before #180 — they were bare prose
    /// (`"trading self-check failed: …"`, `"reconciliation sweep failed (n): …"`)
    /// — and now gain one, because one slot with one writer cannot render two
    /// shapes. Nothing shipped parses those two strings: the panel shows them as
    /// text, and the only test that reads the key asserts `contains`, not
    /// equality. `lastError` above is the structured form to migrate to.
    pub fn legacy_message(&self) -> String {
        format!("{:?}: {}", self.code, self.message)
    }
}

/// One placement's result: the order's id, the status the OME now holds for it,
/// and — when the kernel itself refused the leg after accepting it (a
/// dry/read-only taker whose book cannot fill it) — the structured reason.
///
/// `status == Rejected` with a `rejection` is the leg the venue would have
/// killed; `status == Rejected` without one is a leg an external actor (the
/// venue, a reconcile) retired. Before #180 the drier path returned neither, so
/// `orders.place` answered a bare `REJECTED` and the caller could not tell "no
/// crossing liquidity" from "not enough depth" from "risk said no".
#[derive(Debug, Clone)]
pub struct PlaceOutcome {
    pub order_id: OrderId,
    pub status: OrderStatus,
    pub rejection: Option<CoreError>,
}

/// Upper bound on the kernel's pending-close exit-reason table (issue #190).
///
/// The table holds one entry per token whose closing SELL has been submitted but
/// not yet filled — a *pending-close* ledger, not a history — so its live
/// working set is the engine's tradeable fan-out: the round's tokens across the
/// configured assets (8 tokens × 3 assets = 24 on the shipped Polymarket
/// deployment) plus whatever manual flattens add on top. 64 is ≈2.5× the widest
/// live set, so a legitimate session never reaches the bound; it exists so a leak
/// is bounded for the life of the process instead of growing forever. Eviction
/// prefers entries whose position is already gone (stale by construction) and
/// drops the oldest otherwise; both are counted in `engine.stats.exitReasons`.
pub const EXIT_REASON_TABLE_MAX: usize = 64;

/// The counter-evidence bar for the dry settlement ladder: a partner-token bid
/// at or above this names the held leg the loser. Same value as the live
/// venue path's own decisiveness bar (Gamma's DECISIVE_HIGH, 0.99) — the two
/// ladders demand the same strength of verdict before they book money.
pub(crate) const COUNTER_DECISIVE_HIGH: Decimal = rust_decimal_macros::dec!(0.99);

/// #351: a dry-mode taker leg in flight — the fill model prices travel time
/// (`taker_latency_ms`) and report time (`taker_rtt_ms`), so the leg must
/// never fill on the decision-time quote. Stage 1 (match) fires at the first
/// drain with `now >= execute_at_ms` against the book as it stood then
/// (strictly no future data); stage 2 (report) books the fill or the kill at
/// the first drain with `now >= report_at_ms`.
#[derive(Debug, Clone)]
pub struct InflightTaker {
    pub order_id: OrderId,
    /// When the leg arrives at the venue and is matched.
    pub execute_at_ms: i64,
    /// When the report (fill or kill) reaches the local books.
    pub report_at_ms: i64,
    /// Stage 1: the price the venue would have printed — walk VWAP plus the
    /// model's slippage, full size (FOK is all-or-nothing).
    pub matched_price: Option<Decimal>,
    /// Stage 1: why the venue would have killed the FOK.
    pub rejected_msg: Option<String>,
}
