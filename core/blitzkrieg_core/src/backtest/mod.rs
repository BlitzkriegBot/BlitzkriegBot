//! Event-driven backtester (P-1.2): replay a recorded market-data stream through
//! the SAME core the live path runs.
//!
//! There is no separate simulation engine here. `EventBacktester` owns a real
//! [`Core`] in [`Mode::Dry`] and drives the full live chain — 行情 → 信号 →
//! 风控 → 账本 → OME → 出场 — on a virtual clock:
//!
//! 1. maintenance runs on its own `tick_ms` schedule — the live loop's timer,
//!    independent of event density;
//! 2. at each fire the core runs `tick` (escalations + exits) and
//!    `engine_evaluate` (signal → order), exactly like `ipc::server`'s interval;
//! 3. events pulled from the [`DataSource`] are delivered at their own
//!    timestamps through the same `engine_on_data` choke point a live feed uses.
//!
//! Because the replay drives that one path, the fill model
//! ([`crate::sim::FillModel`]) is the only difference between a faithful replay
//! (identity model) and a stress run (slippage / latency / fill probability).
//!
//! Acceptance for the live/backtest pair is *identical* results when the same
//! events are replayed with the identity model: the archive written by a live
//! run records exactly the events (and their timestamps) the engine consumed, so
//! a replay is a re-run of the same decisions, not an approximation.

pub mod driver;
pub mod hot_params;
pub mod regime;
pub mod types;

#[cfg(test)]
mod tests;

pub use crate::sim::FillModel;
pub use driver::EventBacktester;
pub use types::*;
pub use types::{MAX_MESSAGES, MAX_TRADE_LINES, json_decimal};
