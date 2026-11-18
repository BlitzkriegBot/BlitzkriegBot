//! One persisted settlement journal line's trade-record shape.
//! Moved verbatim from `settlement.rs` (pure code motion).

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// What one settlement booked, as persisted in the journal. Everything the
/// trade record and the panel need, plus the claim it belongs to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettlementRecord {
    pub key: String,
    pub position_id: String,
    pub condition_id: String,
    pub token_id: String,
    pub strategy: String,
    pub asset: String,
    #[serde(with = "crate::decimal")]
    pub shares: Decimal,
    #[serde(with = "crate::decimal")]
    pub payout_per_share: Decimal,
    #[serde(with = "crate::decimal")]
    pub payout_usd: Decimal,
    pub winning: bool,
    pub claim_id: String,
    /// The market settles through the NegRisk adapter (drives the redeem call).
    #[serde(default)]
    pub neg_risk: bool,
    /// The outcome token this market pays for.
    #[serde(default)]
    pub winning_token_id: String,
    /// The position's outcome index in the resolution's order, and how many
    /// outcomes that order had — enough to rebuild the NegRisk redeem amounts
    /// after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_index: Option<usize>,
    #[serde(default)]
    pub outcome_count: usize,
    pub source: String,
    /// The account the payout lands in (§9.2), carried so a restart's claim
    /// replay can restore the attribution. Empty on journal lines written
    /// before the field existed — the credit path routes that to the active
    /// book, the 0.3 behaviour.
    #[serde(default)]
    pub account_id: String,
    /// #647: the venue whose collateral the claim redeems, carried so a
    /// restart's claim replay restores the routing attribution the same way
    /// `account_id` does. Empty on journal lines written before the field
    /// existed — the request then rides the active-plugin fallback.
    #[serde(default)]
    pub venue: String,
    pub applied_at_ms: i64,
}
