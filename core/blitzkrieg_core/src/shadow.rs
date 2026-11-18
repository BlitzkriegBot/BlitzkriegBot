//! Shadow replay + near-miss recording (P4).
//!
//! Reads a shadow log — the price path of every position that actually opened,
//! own side and opposite side, written by the producer of `data/signals/` — and
//! replays exit policies over those recorded paths using the SAME `exit_policy`
//! module the live engine trades with. Because the policy is shared, a backtest
//! cannot drift from production. The walk-forward grid and the frozen holdout
//! below pick an exit configuration out of that replay.
//!
//! The near-miss half is the other direction: it records signals the entry gate
//! BLOCKED together with the path that followed, so offline replay can answer
//! "had we relaxed the gate, would this have made money?".
//!
//! Observation-only: nothing here trades.
//!
//! Pure code motion from `shadow.rs` (no logic edits): `Sample`, `ShadowRecord`,
//! `NearMissReason`, `NearMissRecord`, `ReplayResult` stay in the hub; children
//! own `NearMissRecorder`, `replay`, walk-forward, bucketing, parsing.

use rust_decimal::Decimal;

mod buckets;
mod near_miss;
mod parse;
mod replay;
mod walk_forward;

#[cfg(test)]
mod tests;

pub use buckets::*;
pub use near_miss::*;
pub use parse::*;
pub use replay::*;
pub use walk_forward::*;

#[derive(Debug, Clone)]
pub struct Sample {
    pub t_ms: i64,
    pub price: Decimal,
    pub bid: Option<Decimal>,
    pub ask: Option<Decimal>,
}

#[derive(Debug, Clone)]
pub struct ShadowRecord {
    pub token_id: String,
    pub asset: String,
    pub direction: String,
    pub entry_price: Decimal,
    pub shares: Decimal,
    pub was_maker_entry: bool,
    pub entered_at_ms: i64,
    pub expires_at_ms: i64,
    pub own: Vec<Sample>,
    pub opposite: Vec<Sample>,
    /// Realised net PnL if the position closed within the window.
    pub actual_net_pnl: Option<Decimal>,
    /// Seconds left in the round when the position was entered (context.timeLeftSec).
    pub time_left_at_entry_sec: Option<Decimal>,
}

// ── Near-miss recording ──────────────────────────────────────────────────────
//
// Records signals the entry GATE blocked (timing/momentum) together with the
// token's subsequent price path. This is the data the shadow log lacks: it lets
// offline replay answer "if we had relaxed the gate and taken this trade, would
// it have made or lost money?" — without which relaxing min-time-left is a guess.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NearMissReason {
    Timing,
    Momentum,
}

#[derive(Debug, Clone)]
pub struct NearMissRecord {
    pub token_id: String,
    pub asset: String,
    pub direction: String,
    pub blocked_at_ms: i64,
    /// Price the signal wanted (the resting bid).
    pub entry_price: Decimal,
    /// The mid at block time (context for how deep the dip was).
    pub mid_at_block: Decimal,
    pub reason: NearMissReason,
    /// Seconds left in the round at block time.
    pub time_left_sec: i64,
    pub round_slot: i64,
    /// Subsequent mid path, elapsed ms from block time.
    pub path: Vec<Sample>,
    pub window_end_ms: i64,
}

/// Result of replaying one exit policy over a recorded path.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayResult {
    pub pnl: Decimal,
    pub exit_reason: crate::model::ExitReason,
    pub exit_price: Decimal,
}
