//! One position's settlement, as computed from a venue resolution.
//! Moved verbatim from `settlement.rs` (pure code motion).

use crate::position::OpenPosition;
use blitzkrieg_market_api::MarketResolution;
use rust_decimal::Decimal;

/// One position's settlement, as computed from a venue resolution. Pure data:
/// the caller decides whether it becomes a booking, which keeps the arithmetic
/// testable without a core.
#[derive(Debug, Clone, PartialEq)]
pub struct SettlementBooking {
    /// Durable idempotency key: a position settles at most once, ever.
    pub key: String,
    pub position_id: String,
    pub condition_id: String,
    pub token_id: String,
    pub strategy: String,
    pub asset: String,
    pub shares: Decimal,
    pub payout_per_share: Decimal,
    /// Collateral the winning shares are worth: `shares × payout`.
    pub payout_usd: Decimal,
    pub winning: bool,
    /// E28 §9.2: the account the position belongs to — the redemption claim
    /// (and its eventual payout) lands in THIS account's book.
    pub account_id: String,
    /// #647: the position's own execution venue — the redemption claim
    /// (and its request) carries it so the multi-plugin drain routes the
    /// redeem to the venue that holds the collateral. Empty = untagged (the
    /// pre-routing world: the request drains to the host's active plugin).
    pub venue: String,
    /// Index of the position's token in the resolution's outcome order, when the
    /// resolution names it (drives the NegRisk redeem amounts).
    pub outcome_index: Option<usize>,
    /// Filled by [`SettlementBook::book`]: the claim this settlement landed in,
    /// empty when the position resolved worthless (nothing to redeem).
    pub claim_id: String,
}

/// The idempotency key of one settled position. Stable across a restart: the
/// position id is restored from the position log, and `entered_at_ms` pins it
/// to the round it belongs to (position ids are per-process counters, so the
/// timestamp is what makes the key unique over a long-lived deployment).
pub fn settlement_key(p: &OpenPosition) -> String {
    format!("{}:{}:{}", p.condition_id, p.token_id, p.entered_at_ms)
}

/// Compute what settling this position books, or `None` when the resolution
/// says nothing about the position's token (a resolution for another market, or
/// one that does not cover every outcome).
pub fn booking_for(p: &OpenPosition, resolution: &MarketResolution) -> Option<SettlementBooking> {
    if p.condition_id != resolution.condition_id {
        return None;
    }
    let payout_per_share = resolution.payout_per_share(&p.token_id)?;
    let shares = p.shares.max(Decimal::ZERO);
    let outcome_index = resolution
        .payouts
        .iter()
        .position(|(t, _)| t == &p.token_id);
    Some(SettlementBooking {
        key: settlement_key(p),
        position_id: p.id.clone(),
        condition_id: p.condition_id.clone(),
        token_id: p.token_id.clone(),
        strategy: p.strategy.clone(),
        asset: p.asset.clone(),
        shares,
        payout_per_share,
        payout_usd: shares * payout_per_share,
        winning: payout_per_share > Decimal::ZERO,
        account_id: p.account_id.as_str().to_string(),
        // #647: the venue this position trades on, stamped at entry from the
        // SAME execution-venue lookup the executor drain routes by — the
        // claim's redeem request must reach the venue holding the collateral.
        venue: p.venue.clone(),
        outcome_index,
        claim_id: String::new(),
    })
}
