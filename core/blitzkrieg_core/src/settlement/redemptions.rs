//! Redeeming claims at the venue and recording confirmations and failures.
//! Moved verbatim from `settlement.rs` (pure code motion).

use blitzkrieg_market_api::RedemptionFailure;
use rust_decimal::Decimal;

use super::{JournalLine, SettlementBook, backoff_ms};

impl SettlementBook {
    /// Claims due for an attempt. Dispatching one stamps it as attempted with
    /// the current backoff, so a lost result retries later instead of spinning.
    pub fn take_redemptions(
        &mut self,
        now_ms: i64,
    ) -> Vec<blitzkrieg_market_api::RedemptionRequest> {
        let due: Vec<String> = self
            .claims
            .values()
            .filter(|c| !c.manual && c.next_attempt_ms <= now_ms)
            .map(|c| c.id.clone())
            .collect();
        let mut out = Vec::new();
        for id in due {
            if let Some(c) = self.claims.get_mut(&id) {
                c.attempts += 1;
                c.next_attempt_ms = now_ms + backoff_ms(c.attempts);
                out.push(c.request());
            }
        }
        out
    }

    /// #647: the per-venue drain — every due claim whose request names
    /// `venue`, plus the UNTAGGED (`""`-venue) claims when `untagged_fallback`
    /// (the requested plugin is the host's active one). Take-once on the
    /// dispatch stamp: a sibling executor never sees a claim this drain
    /// handed out, and a second venue's redeem is never mis-routed to a
    /// foreign bridge. Same dispatch semantics as [`Self::take_redemptions`]
    /// (each handed-out claim is stamped with its backoff immediately).
    pub fn take_redemptions_for_venue(
        &mut self,
        venue: &str,
        untagged_fallback: bool,
        now_ms: i64,
    ) -> Vec<blitzkrieg_market_api::RedemptionRequest> {
        let due: Vec<String> = self
            .claims
            .values()
            .filter(|c| {
                !c.manual
                    && c.next_attempt_ms <= now_ms
                    && (c.venue == venue || (untagged_fallback && c.venue.is_empty()))
            })
            .map(|c| c.id.clone())
            .collect();
        let mut out = Vec::new();
        for id in due {
            if let Some(c) = self.claims.get_mut(&id) {
                c.attempts += 1;
                c.next_attempt_ms = now_ms + backoff_ms(c.attempts);
                out.push(c.request());
            }
        }
        out
    }

    /// Replay fast path: the earliest instant any pending redemption claim
    /// can become due (`i64::MAX` when none). `take_redemptions` dispatches
    /// only when `next_attempt_ms <= now`, and a dispatch moves the clock
    /// strictly later, so skipping the drain until this instant dispatches
    /// exactly the same set at exactly the same stamps.
    pub fn next_redemption_ms(&self) -> i64 {
        self.claims
            .values()
            .filter(|c| !c.manual)
            .map(|c| c.next_attempt_ms)
            .min()
            .unwrap_or(i64::MAX)
    }

    /// Replay fast path: the earliest instant any pending merge claim can
    /// become due (`i64::MAX` when none). Same argument as
    /// [`Self::next_redemption_ms`]: `take_merges` dispatches only when
    /// `next_attempt_ms <= now`, and a dispatch moves the clock strictly
    /// later, so skipping the drain until this instant dispatches exactly the
    /// same set at exactly the same stamps.
    pub fn next_merge_ms(&self) -> i64 {
        self.merges
            .values()
            .filter(|m| !m.manual)
            .map(|m| m.next_attempt_ms)
            .min()
            .unwrap_or(i64::MAX)
    }

    /// A redemption attempt failed. Alerts on every failure (never silent) and
    /// arms the retry — `manual` failures stop the automatic retries for good.
    pub fn note_failure(&mut self, id: &str, failure: &RedemptionFailure, now_ms: i64) -> bool {
        let Some(c) = self.claims.get_mut(id) else {
            return false;
        };
        c.last_error = Some(failure.message.clone());
        c.manual = failure.manual;
        if failure.manual {
            c.next_attempt_ms = i64::MAX;
        } else {
            c.next_attempt_ms = now_ms + backoff_ms(c.attempts);
        }
        true
    }

    /// A redemption landed: the claim leaves the receivable. Returns the payout
    /// that must be credited to the cash ledger (once) and the account whose
    /// book it lands in (§9.2; empty on journals written before the field —
    /// the caller routes that to the active book, the 0.3 behaviour).
    pub fn note_confirmed(
        &mut self,
        id: &str,
        tx_hash: Option<String>,
        block_number: Option<u64>,
        now_ms: i64,
    ) -> Option<(String, Decimal)> {
        let claim = self.claims.remove(id)?;
        *self
            .redeemed_per_condition
            .entry(claim.condition_id.clone())
            .or_insert(0) += 1;
        self.redeemed.insert(id.to_string());
        if let Some(j) = self.journal.as_ref()
            && let Err(e) = j.append(&JournalLine::Redeemed {
                at_ms: now_ms,
                claim_id: id.to_string(),
                tx_hash,
                block_number,
            })
        {
            tracing::error!(claim_id = %id, error = %e, "redeemed store write failed");
        }
        Some((claim.account_id, claim.payout_usd))
    }
}
