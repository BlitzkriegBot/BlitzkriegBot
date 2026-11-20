//! Committing one settlement: the idempotent booking entry and its claim fold.
//! Moved verbatim from `settlement.rs` (pure code motion).

use crate::store::{MoneyFact, WriteOutcome};
use blitzkrieg_market_api::MarketResolution;
use rust_decimal::Decimal;

use super::claims::RedemptionClaim;
use super::record::SettlementRecord;
use super::{JournalLine, SettlementBook};

impl SettlementBook {
    /// Commit one settlement: claim + durable journal line. Returns the booking
    /// that was committed, or `None` when the key was already applied — the
    /// caller must then NOT close the position a second time (see
    /// [`SettlementBook::recover`] for the restart case).
    ///
    /// The CLAIM belongs to the market, not to one position: a NegRisk redemption
    /// takes the whole outcome-share vector in one call, so a leg that resolved
    /// worthless still rides along with its (zero-valued) amount while the market
    /// has a paying leg. A market where every leg is worthless gets no claim at
    /// all — losing conditional tokens are worth nothing to redeem, so a
    /// transaction for them would only cost gas. `claim_id` is empty on such a
    /// booking.
    pub fn book(
        &mut self,
        booking: &super::booking::SettlementBooking,
        resolution: &MarketResolution,
        now_ms: i64,
    ) -> Option<super::booking::SettlementBooking> {
        if self.is_applied(&booking.key) {
            return None;
        }
        // The market+account's open claim, if it has one (redemption removes
        // it). One claim per (market, account): both outcomes of one account's
        // market still redeem once (one NegRisk call), but a second account's
        // positions on the same market get their OWN claim — the payout lands
        // in the wallet that paid for the shares (§9.2). An empty
        // `booking.account_id` (a journal written before the field existed)
        // keeps the historical single-claim shape under the empty-string key.
        let open_claim = self
            .claims
            .values()
            .find(|c| c.condition_id == booking.condition_id && c.account_id == booking.account_id)
            .map(|c| c.id.clone());
        let claim_id =
            if booking.payout_usd > Decimal::ZERO {
                Some(open_claim.unwrap_or_else(|| {
                    self.claim_id_for(&booking.condition_id, &booking.account_id)
                }))
            } else {
                open_claim
            };
        let winning_token_id = resolution
            .winning_token()
            .cloned()
            .unwrap_or_else(|| booking.token_id.clone());
        let outcome_shares = outcome_shares_for(resolution, booking);
        if let Some(id) = claim_id.as_ref() {
            self.accumulate_claim(
                id,
                booking,
                resolution,
                &outcome_shares,
                &winning_token_id,
                now_ms,
            );
        }
        let record = SettlementRecord {
            key: booking.key.clone(),
            position_id: booking.position_id.clone(),
            condition_id: booking.condition_id.clone(),
            token_id: booking.token_id.clone(),
            strategy: booking.strategy.clone(),
            asset: booking.asset.clone(),
            shares: booking.shares,
            payout_per_share: booking.payout_per_share,
            payout_usd: booking.payout_usd,
            winning: booking.winning,
            neg_risk: resolution.neg_risk,
            winning_token_id,
            outcome_index: booking.outcome_index,
            outcome_count: resolution.payouts.len().max(2),
            claim_id: claim_id.unwrap_or_default(),
            source: resolution.source.clone(),
            account_id: booking.account_id.clone(),
            venue: booking.venue.clone(),
            applied_at_ms: now_ms,
        };
        self.applied.insert(record.key.clone(), record.clone());
        self.booked += 1;
        if let Some(ref s) = self.store {
            match s.append_money_fact(&MoneyFact::Settlement(&record)) {
                Ok(WriteOutcome::Persisted) => {}
                Ok(outcome) => tracing::warn!(
                    position_id = %record.position_id,
                    "settlement money_fact not persisted ({outcome:?}) — journal line kept, trading continues"
                ),
                Err(e) => tracing::warn!(
                    position_id = %record.position_id,
                    "settlement money_fact append refused — journal line kept, store write failed: {e:?}"
                ),
            }
        }
        if let Some(j) = self.journal.as_ref() {
            let key = record.key.clone();
            if let Err(e) = j.append(&JournalLine::Settlement {
                at_ms: now_ms,
                record: Box::new(record),
            }) {
                tracing::error!(key = %key, error = %e, "settlement store write failed");
            }
        }
        let mut settled = booking.clone();
        settled.claim_id = self
            .applied
            .get(&settled.key)
            .map(|r| r.claim_id.clone())
            .unwrap_or_default();
        Some(settled)
    }

    /// Fold one booking into its market+account's open claim (one claim per
    /// market PER ACCOUNT, so one account's positions on both outcomes redeem
    /// once while a second account's positions get their own claim).
    fn accumulate_claim(
        &mut self,
        claim_id: &str,
        booking: &super::booking::SettlementBooking,
        resolution: &MarketResolution,
        outcome_shares: &[Decimal],
        winning_token_id: &str,
        now_ms: i64,
    ) {
        let claim = self
            .claims
            .entry(claim_id.to_string())
            .or_insert_with(|| RedemptionClaim {
                id: claim_id.to_string(),
                condition_id: booking.condition_id.clone(),
                account_id: booking.account_id.clone(),
                venue: booking.venue.clone(),
                neg_risk: resolution.neg_risk,
                winning_token_id: winning_token_id.to_string(),
                outcome_shares: Vec::new(),
                payout_usd: Decimal::ZERO,
                position_ids: Vec::new(),
                created_at_ms: now_ms,
                attempts: 0,
                next_attempt_ms: 0,
                manual: false,
                last_error: None,
                tx_hash: None,
                block_number: None,
            });
        if claim.outcome_shares.len() < outcome_shares.len() {
            claim
                .outcome_shares
                .resize(outcome_shares.len(), Decimal::ZERO);
        }
        for (i, s) in outcome_shares.iter().enumerate() {
            if let Some(slot) = claim.outcome_shares.get_mut(i) {
                *slot += *s;
            }
        }
        claim.payout_usd += booking.payout_usd;
        claim.position_ids.push(booking.position_id.clone());
        claim.neg_risk = resolution.neg_risk;
        // #647: a claim folded from an untagged leg stays routable — the venue
        // stamp rides the same "empty inherits the first stamp" rule the
        // winning token uses, so every leg of one claim is one venue's
        // collateral (the drain hands it to exactly one executor).
        if claim.venue.is_empty() {
            claim.venue = booking.venue.clone();
        }
        if claim.winning_token_id.is_empty() {
            claim.winning_token_id = winning_token_id.to_string();
        }
    }

    /// The claim id for a new settlement on this market: the plain condition id
    /// for the market's FIRST claim, then `#2`, `#3`, … (the claim's ordinal), so
    /// a redeemed id is never reused. A SECOND account settling the same market
    /// (its claim id is taken) also falls through to the next ordinal — two
    /// accounts never share a claim id, so their payouts stay attributable.
    fn claim_id_for(&self, condition_id: &str, _account_id: &str) -> String {
        let used = self
            .redeemed_per_condition
            .get(condition_id)
            .copied()
            .unwrap_or(0);
        if used == 0 && !self.claims.contains_key(condition_id) {
            return condition_id.to_string();
        }
        format!("{condition_id}#{}", used + 1)
    }
}

/// The outcome-share vector one booking contributes to its claim: the booking's
/// shares parked at its outcome index, in the resolution's outcome order.
fn outcome_shares_for(
    resolution: &MarketResolution,
    booking: &super::booking::SettlementBooking,
) -> Vec<Decimal> {
    let n = resolution.payouts.len().max(2);
    let mut v = vec![Decimal::ZERO; n];
    if let Some(i) = booking.outcome_index
        && let Some(slot) = v.get_mut(i)
    {
        *slot = booking.shares;
    }
    v
}
