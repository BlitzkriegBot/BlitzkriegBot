//! # P1-1 / P0-5 · one metrics entity, one win-rate formula, one net-PnL name
//!
//! **Abolishes** the field soup the audit measured: there is no single call that
//! returns balance, equity, cumulative net PnL, realized, unrealized, win rate
//! and fees together, so the panel stitches a screen from three or four calls
//! whose spellings disagree. "Net profit" exists under three definitions at
//! once (`cumulative_net_pnl = balance − opening_equity` in the ledger,
//! `attributedNetPnl` in the fold, and `seeded_total` documented as
//! `balance − seeded_total` — the last two contradicting each other seven lines
//! apart in `core/blitzkrieg_core/src/schema/views.rs:662-674`).
//!
//! Win rate is worse: `trade_summary` yields a percentage 0–100 while `shadow`
//! yields a fraction 0–1, and the frontend's own comment
//! (`ui/webapp/webui/src/lib/format.ts:52-70`) records the real incident where
//! the second multiplication produced `7330%`.
//!
//! **Replaces them with** [`AccountMetrics`] — one struct, one wire name
//! ([`METRICS_SNAPSHOT`]), one net-PnL field, and one win-rate formula
//! ([`Tally::win_rate`]) that every producer calls instead of re-deriving.
//!
//! **Issue:** P1-1 (contract) + P0-5 (the frontend must stop doing its own
//! arithmetic and print `equity: None` as an em dash). **Consuming task:** T6,
//! which is a **UI task** per RULES §四 and therefore belongs to the frontend
//! engineer; the Rust half is the snapshot producer.
//!
//! ## The one thing T6 may not do
//!
//! It may not widen this struct into "all the old fields as optional". The point
//! of one entity is that a missing value is `None` and the consumer shows
//! 「未知」 — not that the consumer falls back to a second, differently-shaped
//! number (R7.1 L5).

use crate::model::AccountId;

/// Win/loss/flat counts. The **input** to the one win-rate formula.
///
/// **Abolishes:** the `(wins, losses, flats)` triple travelling as three loose
/// integers, which is how each producer ended up with its own idea of whether a
/// flat counts (trade_db excludes it, backtest includes it, shadow uses the
/// whole sample).
/// **Replaced by:** this struct as `AccountMetrics::tally`, so the triple cannot
/// be reassembled with a different denominator.
/// **Issue:** P1-1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    /// Fully-closed positions that finished with a positive net result.
    pub wins: u32,
    /// Fully-closed positions that finished with a negative net result.
    pub losses: u32,
    /// Fully-closed positions that finished flat. Counted and reported, but
    /// **excluded from the denominator** — see [`Tally::win_rate`].
    pub flats: u32,
}

impl Tally {
    /// The single win-rate formula in the system: `wins / (wins + losses)`,
    /// flats excluded, `None` when there are no decided outcomes.
    ///
    /// **Abolishes:** every second denominator — the `0–100` producer, the
    /// `0–1` producer, and the frontend's re-derivation. The audit records the
    /// real incident caused by the disagreement (`7330%`).
    /// **Replaced by:** this method, called by every producer. `Option` rather
    /// than `0.0` because "no decided trades yet" is unknown, not zero — R7.1
    /// L4/L5: a fabricated `0.0` renders as a real statistic.
    /// **Issue:** P1-1.
    ///
    /// This is the **only** method with a body allowed by the T0 freeze: the
    /// issue text supplies it verbatim, and a duplicated formula is exactly what
    /// the contract exists to delete. Everything else in this module is a
    /// declaration.
    pub fn win_rate(&self) -> Option<f64> {
        let d = self.wins + self.losses;
        (d > 0).then(|| self.wins as f64 / d as f64)
    }
}

/// The wire verb that returns [`AccountMetrics`].
///
/// **Abolishes:** the screen-by-screen assembly (`balance`, `engine.stats`,
/// `trade.summary`, `shadow.stats`) that made the panel hold two ledgers and
/// "reconcile" them client-side (`identityBroken`).
/// **Replaced by:** one snapshot method, so the panel makes one call and prints
/// what it got.
/// **Issue:** P1-1 / P0-5.
pub const METRICS_SNAPSHOT: &str = "metrics.snapshot";

/// One account's complete money picture at one instant.
///
/// **Abolishes:** `BalanceResult` / `AccountView` / `TradeSummary` as three
/// independent structs with drifting field names and optionality.
/// **Replaced by:** this struct as the single snapshot payload; the old views
/// become projections of it.
/// **Issue:** P1-1 / P0-5.
///
/// `f64` on the wire is deliberate and matches the existing views (`BalanceView`
/// and friends are `f64`); the *arithmetic* stays in `Decimal` inside the kernel
/// (DEV §7) and these are the already-computed results. T6 does the conversion
/// once at the boundary — it does not re-do the maths in `f64`.
///
/// `Option` means **unknown**, and every consumer must render it as such. None of
/// these may be defaulted to `0.0` downstream: §3.3 of the audit traces a
/// nine-thousand-dollar phantom gain to precisely that.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountMetrics {
    /// Which account these numbers belong to. Non-optional: an unscoped metric
    /// set is the #1221/#1223 bug class.
    pub account: AccountId,
    /// When the snapshot was taken, epoch milliseconds.
    pub as_of_ms: i64,
    /// Ledger cash, verbatim (`Ledger::balance`, `core/blitzkrieg_core/src/ledger.rs:278`).
    pub balance: f64,
    /// Cash reserved against open orders (`Ledger::reserved`, `:287`).
    pub reserved: f64,
    /// `balance - reserved` (`Ledger::available`, `:295`). Carried, not
    /// recomputed — a second subtraction is a second chance to disagree.
    pub available: f64,
    /// The principal the cumulative figure is measured against. `None` until
    /// the first venue report establishes it — never defaulted to `balance`.
    pub opening_equity: Option<f64>,
    /// **The single net-PnL definition.** `None` when `opening_equity` is
    /// unknown, because without a principal there is no net figure to report.
    ///
    /// **Abolishes:** `attributedNetPnl` (fold-shaped) and
    /// `balance − seeded_total` (panel-shaped) as competing definitions.
    /// **Issue:** P1-1 / P0-5.
    pub cumulative_net_pnl: Option<f64>,
    /// Mark-to-market PnL of open positions. A real `f64`: open positions are
    /// always measurable (each carries a `PriceState`), so there is no unknown
    /// case here.
    pub unrealized_pnl: f64,
    /// `opening_equity + cumulative_net_pnl + unrealized_pnl`, computed by the
    /// kernel. `None` = unknown, and the consumer must render 「—」.
    ///
    /// **Abolishes:** `Overview.vue:418`'s `equity === null ? '—' :
    /// money(cash)` — the em dash that renders cash instead. Also
    /// `equity: seed + realized` in `ui/webapp/webui/src/lib/balance.ts:92-95`.
    /// **Issue:** P0-5.
    pub equity: Option<f64>,
    /// The win/loss/flat triple the win rate derives from. Carried alongside
    /// the rate so a consumer can show the counts without recomputing.
    pub tally: Tally,
    /// `tally.win_rate()`, carried for the wire so the consumer never derives
    /// it. Must equal `tally.win_rate()`; T6 asserts that rather than trusting
    /// two paths.
    pub win_rate: Option<f64>,
    /// Realized PnL before fees.
    pub gross_pnl: f64,
    /// Fees paid, already subtracted from `cumulative_net_pnl` and reported
    /// separately so the panel can show them without adding them back.
    pub fees: f64,
}
