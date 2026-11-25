//! The order-intent idempotency gate (issue #185).
//!
//! # Where this sits
//!
//! `place_gated` funnels every order into `new_order_id` → `reserve` →
//! `ome.submit`. That sequence is where the ledger's claim must happen: AFTER
//! every read-only gate (so a refused intent does not pin the round window) and
//! BEFORE the first write to the ledger or the OME (so a duplicate can never
//! reach either). `place_pending` (the live adapter's async POST path) gets the
//! same claim for the same reason.
//!
//! # What counts as a gated intent
//!
//! Only OPENING legs: `Side::Buy`, not a close intent (`exit:` / `flatten:` /
//! `exit-residual:` / `exit-escalated:`), and not a maker→taker escalation
//! (`{order_id}:escalated`). The reasons are load-bearing:
//!
//! * **Closes are never gated.** A stale intent row must not be able to block a
//!   close — trapping exposure is the #174 class of defect, far worse than the
//!   duplicate this ledger prevents. Closes keep their own live-sell dedup.
//! * **Escalations are never gated.** The escalated taker IS the same entry
//!   intent continuing; gating it would refuse the leg that completes a
//!   partially-filled maker.
//!
//! # The three answers a duplicate can get
//!
//! * [`IntentClaim::Claimed`] — this caller owns the intent and must submit,
//!   then record the outcome.
//! * `Existing(Ordered { .. })` — the first submit placed that order. The
//!   duplicate gets `Ok` with THAT order id and status (and the first
//!   post-submit refusal when there was one). It never places a second order.
//! * `Existing(Ordered { rejection: Some(..) })` — the first order was itself
//!   rejected by the kernel after submit (a dry taker the book could not
//!   fill). The duplicate gets the SAME order id, status AND reason.
//!
//! A submit refused BEFORE any order exists never records anything: the claim
//! happens after every refusal gate, so a gate-refused attempt leaves the
//! intent retryable — a stale refusal must not lock the window out.
//! * `Existing(Reserved)` — the first submit is still in flight. There is no
//!   result to replay yet, so the duplicate is refused explicitly (fail-closed:
//!   it must not submit a second order).
//!
//! # Store-wired vs store-less
//!
//! With no durable store (`CoreConfig::db_path == None`: the backtester and the
//! in-process test corpus) there is no ledger and this gate stays open — the
//! OME's live-key dedup is then the only guard, exactly as before. That
//! boundary is real and must be stated wherever it is claimed: durability is a
//! property of the store, not of the process.

use super::Core;
use super::config::PlaceOutcome;
use crate::db::intents::{IntentClaim, IntentKey, IntentState};
use crate::model::{CoreError, CoreErrorCode, OrderRequest, Side};

/// The outcome of consulting the ledger before a submit.
pub(crate) enum IntentGate {
    /// No claim held (no ledger, or this request is not a gated intent):
    /// proceed exactly as before.
    Open,
    /// This caller claimed the intent and must submit, then settle the claim.
    Claimed(IntentKey),
    /// A duplicate: answer with the first submit's real order result.
    Replay(PlaceOutcome),
    /// A duplicate whose first submit was refused: answer with the same error.
    ReplayRefusal(CoreError),
}

/// The intent identity of `req`, or `None` when the request must not be gated.
///
/// The identity is the #185 contract's key: `(account_id, strategy,
/// condition_id, round_slot, direction)`. `round_slot` here is the request's
/// PERSISTED venue round id (`round_id::venue_round_id`), which is stable
/// across restarts — the scanner slot index would not survive a config change,
/// the venue start timestamp does.
pub(crate) fn intent_key_for(req: &OrderRequest) -> Option<IntentKey> {
    if req.side != Side::Buy {
        return None;
    }
    if crate::risk::is_close_intent(&req.internal_key) {
        return None;
    }
    // A maker→taker escalation continues the SAME entry intent: the maker
    // already claimed the row. Gating the chase would refuse the leg that
    // completes the partial fill.
    if req.internal_key.ends_with(":escalated") {
        return None;
    }
    Some(IntentKey::new(
        req.account_id.as_str(),
        req.strategy.clone(),
        req.condition_id.clone(),
        req.round_slot,
        req.direction.clone(),
    ))
}

impl Core {
    /// Consult the ledger and claim the intent when this request is gated.
    ///
    /// A ledger that cannot answer is a REFUSAL, never an implicit "no
    /// duplicate" (§3/L4): the whole point of the structure is that its absence
    /// must be visible, not silently helpful.
    pub(crate) fn intent_gate(
        &mut self,
        req: &OrderRequest,
        now_ms: i64,
    ) -> Result<IntentGate, CoreError> {
        let Some(key) = intent_key_for(req) else {
            return Ok(IntentGate::Open);
        };
        let Some(ledger) = self.intent_ledger.as_ref() else {
            // Documented store-less boundary: no durable ledger in this build
            // shape, so the live-key dedup is the guard (unchanged behaviour).
            return Ok(IntentGate::Open);
        };
        let claim = ledger.claim(&key, now_ms).map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!(
                    "intent ledger unavailable ({e}) — refusing the entry rather than \
                     submitting without the duplicate guard"
                ),
            )
        })?;
        match claim {
            IntentClaim::Claimed => Ok(IntentGate::Claimed(key)),
            IntentClaim::Existing(state) => Ok(match state {
                IntentState::Ordered {
                    order_id,
                    status,
                    rejection,
                } => {
                    tracing::info!(
                        intent = %key.internal_spelling(),
                        order = %order_id,
                        "duplicate order intent replayed: the first submit already placed it"
                    );
                    IntentGate::Replay(PlaceOutcome {
                        order_id,
                        status: crate::db::intents::parse_order_status(&status)
                            .unwrap_or(crate::model::OrderStatus::Pending),
                        rejection: rejection.map(|r| {
                            let code = crate::db::intents::parse_wire_code(&r.code)
                                .unwrap_or(CoreErrorCode::Internal);
                            CoreError::new(code, r.message)
                        }),
                    })
                }
                IntentState::Reserved => IntentGate::ReplayRefusal(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "intent {} is already being submitted (claim held); refusing the \
                         concurrent duplicate",
                        key.internal_spelling()
                    ),
                )),
                // The claim re-arms these inside the same transaction, so a
                // surface here means the re-arm lost a race. Treat as claimed:
                // the unique key still guarantees one submit.
                IntentState::Consumed | IntentState::Abandoned => IntentGate::Claimed(key),
            }),
        }
    }

    /// Record the first submit's real result: the order it placed.
    pub(crate) fn settle_intent_ordered(
        &mut self,
        key: &IntentKey,
        outcome: &PlaceOutcome,
        now_ms: i64,
    ) {
        let Some(ledger) = self.intent_ledger.as_ref() else {
            return;
        };
        let rejection = outcome
            .rejection
            .as_ref()
            .map(|e| (crate::db::intents::wire_code(e.code), e.message.as_str()));
        if let Err(e) = ledger.record_ordered(
            key,
            &outcome.order_id,
            &crate::db::intents::order_status_wire(outcome.status),
            rejection.as_ref().map(|(c, m)| (c.as_str(), *m)),
            now_ms,
        ) {
            // The order EXISTS. Losing the intent row costs a future duplicate
            // guard, not the order — say so at error level (never silently).
            tracing::error!(
                intent = %key.internal_spelling(),
                order = %outcome.order_id,
                error = %e,
                "intent ledger could not record the placed order — a later duplicate may re-submit"
            );
        }
    }

    /// Release a claim whose submit never reached the OME. Only a claim with no
    /// order behind it is ever released: releasing an `ordered` row would let a
    /// duplicate place a second order for an order that exists.
    pub(crate) fn abandon_intent(&mut self, key: &IntentKey, now_ms: i64) {
        let Some(ledger) = self.intent_ledger.as_ref() else {
            return;
        };
        if let Err(e) = ledger.release(key, now_ms) {
            tracing::error!(
                intent = %key.internal_spelling(),
                error = %e,
                "intent ledger could not release the claim"
            );
        }
    }

    /// Issue #848: the gap-fill's replace RETIRES a resting order and submits
    /// the same intent again. The superseded row must stop replaying the old
    /// order id, so the replacement can be recorded — mark it consumed BEFORE
    /// the cancel, so a failed cancel still leaves the intent re-claimable
    /// (the retry then finds the resting order through the 848 path again).
    pub(crate) fn supersede_intent(&mut self, req: &OrderRequest, now_ms: i64) {
        let (Some(key), Some(ledger)) = (intent_key_for(req), self.intent_ledger.as_ref()) else {
            return;
        };
        if let Err(e) = ledger.mark_consumed(&key, now_ms) {
            tracing::error!(
                intent = %key.internal_spelling(),
                error = %e,
                "intent ledger could not supersede the replaced intent"
            );
        }
    }
}
