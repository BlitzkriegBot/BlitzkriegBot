//! IPC contract — Unix-domain-socket JSON-RPC 2.0.
//!
//! These serde types are the SINGLE schema between Rust core and Node. Node
//! mirrors them with zod; there is no `any`/`unknown` on the wire.
//!
//! Framing: one JSON object per line (`\n`), UTF-8.
//!
//!   Node → Rust  request:      { jsonrpc:"2.0", id, method, params }
//!   Rust → Node  response:     { jsonrpc:"2.0", id, result | error }
//!   Rust → Node  notification: { jsonrpc:"2.0", method:"core.event", params: Event }
//!
//! Node never sends a private key; the core loads credentials itself.
//!
//! # Canonical ownership (#840 PR2.5 终审, freeze wave)
//!
//! This module IS the canonical owner of the JSON-RPC wire schema — one
//! definition of every params/view/Event type, consumed in-kernel through the
//! single `use crate::schema::*` seam (`ipc::server` and the service
//! producers) and by the UI only as JSON over the socket. It deliberately does
//! NOT move to `blitzkrieg-bk-contract` in the freeze wave:
//!
//! 1. **The event stream carries kernel domain payloads.** `Event`'s shadow
//!    evolution variants embed `shadow_evolution::EvolutionProposal`, whose
//!    `from_params`/`to_params` are `strategy_logic::params::StrategyParams` —
//!    an internal implementation crate the contract leaf is red-lined against
//!    depending on. Moving the schema would either drag a strategy-domain
//!    crate into `bk_contract` or fork `Event` into two definitions (the one
//!    thing the freeze forbids).
//! 2. **The other kernel couplings are the wire's substance, not accidents:**
//!    `OrderRequest`/`TrackedOrder`/`Fill`/`Mode` (the order lifecycle the OME
//!    owns), `AccountStatus`, `JournalKind`, the risk-limit views — these are
//!    the readouts of core state machines #839 explicitly keeps in core.
//!    Moving their definitions to a leaf would relocate kernel state types
//!    behind the market-plugin boundary without a consumer that needs them
//!    there (the UI speaks JSON, not Rust).
//! 3. **The wire is frozen where the type is.** The 22-family golden
//!    (`tests/schema_family_golden.rs`) pins every family's serde shape
//!    byte-exactly, and the freeze commit puts the whole surface under
//!    `[CONTRACT-BREAKING]` discipline — same enforcement a move would buy,
//!    without the churn.
//!
//! Retirement path: when the evolution payloads get wire-local decoupling (or
//! a second compiled consumer of the schema appears), the pure-DTO majority
//! moves to `bk_contract::schema` and this module becomes a `pub use` shim —
//! reviewed at the 0.5.0 contract review at the latest. Until then: single
//! definition, golden-locked, change = `[CONTRACT-BREAKING]`.

use crate::account::AccountStatus;
use crate::kline::{Kline, KlineInterval};
use crate::model::*;
use crate::ome::FillDelta;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

mod blueprint;
mod envelope;
mod exit_policy;
mod market_plugin;
mod methods;
mod risk_limits;
mod round_duration;
mod strategy_params;
mod unified;
mod views;

pub use blueprint::*;
pub use envelope::*;
pub use exit_policy::*;
pub use market_plugin::*;
pub use methods::*;
pub use risk_limits::*;
pub use round_duration::*;
pub use strategy_params::*;
pub use unified::*;
pub use views::*;

// ── E29 (§12.2) — K-line subscriptions ───────────────────────────────────────
// (`kline.history`'s envelope was frozen by Wave 0 below; E29 extends its
// BEHAVIOUR — real bars — without re-spelling the shape. One semantic note
// now that bars exist: `klines` is oldest first with the still-growing bar
// riding LAST (`isClosed = false`), so a chart draws it translucent without
// a second call.)

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KlineSubscribeParams {
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default)]
    pub intervals: Vec<crate::kline::KlineInterval>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KlineSubscribeResult {
    /// Size of the session's subscription set after this call (dedup —
    /// resubscribing an existing pair changes nothing).
    pub subscribed: usize,
}

// ── Typed params / results ───────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceParams {
    #[serde(flatten)]
    pub order: OrderRequest,
    /// Resting maker order escalates to taker after this many ms if unfilled
    /// (order modes maker_then_taker). `0` = the core's configured default,
    /// a negative value = never escalate (the leg rests until cancelled or the
    /// round ends).
    #[serde(default)]
    pub maker_timeout_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlaceResult {
    #[serde(rename = "orderId")]
    pub order_id: OrderId,
    pub status: OrderStatus,
    /// Why the KERNEL refused the leg, when it did — the dry/read-only taker the
    /// book could not fill (#180). Same vocabulary a rejected call carries in
    /// `data.coreCode`, so a client branches on one or the other the same way.
    /// Absent for an order the kernel accepted (even one the venue may refuse
    /// later: that arrives on `Event::Error`, not on this result), which keeps
    /// the shape backward compatible for existing consumers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<CoreErrorCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CancelParams {
    #[serde(rename = "orderId")]
    pub order_id: OrderId,
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelAllParams {
    #[serde(default)]
    pub token_id: Option<TokenId>,
}
#[derive(Debug, Clone, Serialize)]
pub struct CancelAllResult {
    pub cancelled: usize,
}

/// E31-c: retire the resting remainder of a partially filled order. Same body
/// as a plain cancel — the distinct method exists so a strategy's DECISION
/// ("cancel what is left, keep my filled shares") is visible in ops traces.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelRemainingParams {
    pub order_id: OrderId,
}

/// #457: force-close a ghost order. Same body as a plain cancel — the distinct
/// method exists so the audit trail reads "the operator declared this order
/// dead", not "the operator withdrew a live order".
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForceCloseParams {
    pub order_id: OrderId,
}

/// E31-c: flatten only the shares the fills have already accrued behind an
/// order, leaving its resting remainder alone. The kernel prices, sizes,
/// risk-checks and submits the SELL; the strategy only names the position.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseFilledParams {
    pub order_id: OrderId,
}
#[derive(Debug, Clone, Serialize)]
pub struct CloseFilledResult {
    /// Number of positions the flatten submitted (0 or 1).
    pub closed: usize,
}

/// Ledger balance readout. Wire = camelCase; every Decimal travels via
/// `crate::decimal` as a JSON **number**: finite values serialize as f64
/// (≤17 significant digits round-trip losslessly; only non-finite values
/// fall back to a string), optional fields are omitted when absent so
/// older cores read "unknown", never zero.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceResult {
    #[serde(with = "crate::decimal")]
    pub balance: Decimal,
    #[serde(with = "crate::decimal")]
    pub reserved: Decimal,
    #[serde(with = "crate::decimal")]
    pub available: Decimal,
    /// Starting principal in DRY mode (`--seed-balance`). `None` in LIVE / on
    /// older cores — treat as "unknown", not zero. Semantically this is the
    /// DRY opening equity; LIVE opening equity travels in `opening_equity`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<Decimal>,
    /// Account opening equity — the one-time principal the cumulative net PnL
    /// is measured against. DRY: equals `seed`. LIVE: the venue's first
    /// reported equity, persisted to the account file exactly once (never
    /// overwritten). `None` until known → `cumulative_net_pnl` is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opening_equity: Option<Decimal>,
    /// Cumulative net PnL = balance − opening_equity (the single ledger
    /// truth, §1046/#1063: `reserved` collateral is NOT part of it — the
    /// pure function structurally excludes it). `None` when `opening_equity`
    /// is unknown (LIVE before the first venue report, or an older core).
    /// Never derived from trade folding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_net_pnl: Option<Decimal>,
}
