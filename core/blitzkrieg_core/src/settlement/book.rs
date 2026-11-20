//! The settlement book impl: apply, replay, queries, blind-state and fallback
//! receivables. Moved verbatim from `settlement.rs` (pure code motion).

use crate::position::OpenPosition;
use crate::store::StoreApi;
use rust_decimal::Decimal;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::JournalLine;
use super::QueryState;
use super::SettlementBook;
use super::SettlementJournal;
use super::claims::{ExpiryFallbackClaim, MergeClaim, RedemptionClaim};
use super::constants::{BLIND_AFTER_MS, EXPIRY_TRUST_MS, REQUERY_MS};
use super::record::SettlementRecord;

impl SettlementBook {
    /// Open the book, replaying the durable journal when a path is configured.
    pub fn new(path: Option<&Path>) -> Self {
        let mut book = Self {
            applied: BTreeMap::new(),
            closed: BTreeSet::new(),
            claims: BTreeMap::new(),
            redeemed: BTreeSet::new(),
            redeemed_per_condition: BTreeMap::new(),
            queries: BTreeMap::new(),
            booked: 0,
            blind_alerted: false,
            merges: BTreeMap::new(),
            merged: BTreeSet::new(),
            fallback_claims: BTreeMap::new(),
            fallback_redeemed: BTreeSet::new(),
            journal: path.map(|p| SettlementJournal {
                path: p.to_path_buf(),
                store: None,
            }),
        };
        if let Some(j) = book.journal.as_ref() {
            for line in j.load() {
                book.replay(line);
            }
        }
        book
    }

    /// Install a `StoreApi` carrier so settlement money facts land durably.
    pub fn set_store(&mut self, store: Box<dyn StoreApi>) {
        if let Some(j) = self.journal.as_mut() {
            j.set_store(store);
        }
    }

    fn replay(&mut self, line: JournalLine) {
        match line {
            JournalLine::Settlement { record, .. } => {
                // A worthless leg of a market that never got a paying one has no
                // claim (empty id): replaying it must not invent one.
                if !record.claim_id.is_empty() {
                    let shares = settled_shares(&record);
                    let widths = shares.len().max(2);
                    let claim = self
                        .claims
                        .entry(record.claim_id.clone())
                        .or_insert_with(|| RedemptionClaim {
                            id: record.claim_id.clone(),
                            condition_id: record.condition_id.clone(),
                            // Restored from the record (journals written before
                            // the field existed replay under the empty spelling;
                            // the credit path routes that to the active book
                            // exactly as 0.3 did).
                            account_id: record.account_id.clone(),
                            // #647: the venue the claim redeems on — same
                            // restore-or-empty rule as `account_id`: journals
                            // written before the field existed replay untagged
                            // and drain to the active plugin as before.
                            venue: record.venue.clone(),
                            neg_risk: record.neg_risk,
                            winning_token_id: record.winning_token_id.clone(),
                            outcome_shares: Vec::new(),
                            payout_usd: Decimal::ZERO,
                            position_ids: Vec::new(),
                            created_at_ms: record.applied_at_ms,
                            attempts: 0,
                            next_attempt_ms: 0,
                            manual: false,
                            last_error: None,
                            tx_hash: None,
                            block_number: None,
                        });
                    if claim.outcome_shares.len() < widths {
                        claim.outcome_shares.resize(widths, Decimal::ZERO);
                    }
                    for (i, s) in shares.iter().enumerate() {
                        if let Some(slot) = claim.outcome_shares.get_mut(i) {
                            *slot += *s;
                        }
                    }
                    claim.payout_usd += record.payout_usd;
                    claim.position_ids.push(record.position_id.clone());
                    claim.neg_risk = record.neg_risk;
                    // #647: same empty-inherits-first-stamp rule as the live
                    // fold, so a replayed claim routes like the original.
                    if claim.venue.is_empty() {
                        claim.venue = record.venue.clone();
                    }
                    if claim.winning_token_id.is_empty() {
                        claim.winning_token_id = record.winning_token_id.clone();
                    }
                }
                self.applied.insert(record.key.clone(), *record);
            }
            JournalLine::Closed { key, .. } => {
                self.closed.insert(key);
            }
            JournalLine::Redeemed { claim_id, .. } => {
                if let Some(c) = self.claims.remove(&claim_id) {
                    *self
                        .redeemed_per_condition
                        .entry(c.condition_id.clone())
                        .or_insert(0) += 1;
                }
                self.redeemed.insert(claim_id);
            }
            JournalLine::MergeBurned {
                claim_id,
                condition_id,
                account_id,
                venue,
                neg_risk,
                pairs_usd,
                at_ms,
                ..
            } => {
                // Replay re-opens the claim unless a later MergeConfirmed line
                // already removed it (the match arm below inserts nothing then).
                self.merges
                    .entry(claim_id.clone())
                    .or_insert_with(|| MergeClaim {
                        id: claim_id.clone(),
                        condition_id: condition_id.clone(),
                        account_id,
                        // #647: empty on journals written before the field
                        // existed — the replayed claim drains untagged.
                        venue,
                        neg_risk,
                        pairs_usd,
                        created_at_ms: at_ms,
                        attempts: 0,
                        next_attempt_ms: 0,
                        manual: false,
                        last_error: None,
                        tx_hash: None,
                        block_number: None,
                    });
            }
            JournalLine::MergeConfirmed { claim_id, .. } => {
                self.merges.remove(&claim_id);
                self.merged.insert(claim_id);
            }
            JournalLine::ExpiryFallbackBooked {
                at_ms,
                claim_id,
                position_id,
                condition_id,
                token_id,
                account_id,
                shares,
                fallback_price,
                price_source,
                booked_net_pnl_usd,
                closed_at_ms,
            } => {
                // Replay re-opens the claim unless a later ExpiryFallbackRedeemed
                // line already removed it (that arm inserts nothing then) — the
                // same last-writer-wins order the merge family replays by.
                // `created_at_ms` rides the journal's `at_ms`, so the aging
                // clock (boundary 1) survives a restart exactly as the claim
                // itself does.
                self.fallback_claims
                    .entry(claim_id.clone())
                    .or_insert_with(|| ExpiryFallbackClaim {
                        id: claim_id.clone(),
                        position_id,
                        condition_id,
                        token_id,
                        account_id,
                        shares,
                        fallback_price,
                        created_at_ms: at_ms,
                        price_source,
                        booked_net_pnl_usd,
                        closed_at_ms,
                    });
            }
            JournalLine::ExpiryFallbackRedeemed { claim_id, .. } => {
                self.fallback_claims.remove(&claim_id);
                self.fallback_redeemed.insert(claim_id);
            }
        }
    }

    /// Is this position already settled? The durable half of the idempotency
    /// guard: a replayed resolution must not book the same position twice.
    pub fn is_applied(&self, key: &str) -> bool {
        self.applied.contains_key(key)
    }

    pub fn applied_count(&self) -> usize {
        self.applied.len()
    }

    pub fn booked_this_session(&self) -> u64 {
        self.booked
    }

    /// Collateral earned but not yet in the wallet — the receivable the
    /// accounting identity carries. Issue #991: fallback-closed notional
    /// booked as receivables rides the SAME pool (estimated money must never
    /// read as spendable cash, and the audit identity must cover it exactly
    /// like a settled claim).
    pub fn receivable_usd(&self) -> Decimal {
        self.claims
            .values()
            .fold(Decimal::ZERO, |a, c| a + c.payout_usd)
            + self.fallback_receivable_usd()
    }

    /// The receivable of ONE account's claims (each claim carries the
    /// `account_id` it was booked from — E28 §9.2): the accounting audit
    /// compares THIS book's balance against THIS book's receivable; a sibling
    /// account's pending redemption is a different wallet's money.
    pub fn receivable_usd_for(&self, account: &str) -> Decimal {
        self.claims
            .values()
            .filter(|c| c.account_id == account)
            .fold(Decimal::ZERO, |a, c| a + c.payout_usd)
            + self.fallback_receivable_usd_for(account)
    }

    /// Issue #991: the fallback-claim slice of the receivable pool — booked
    /// but unverified expiry close notionals, pooled with the settled claims
    /// above so every consumer (audit identity, panel, day budget) reads ONE
    /// receivable number.
    pub fn fallback_receivable_usd(&self) -> Decimal {
        self.fallback_claims
            .values()
            .fold(Decimal::ZERO, |a, c| a + c.booked_usd())
    }

    /// ONE account's fallback-claim slice (same principle as
    /// [`Self::receivable_usd_for`]).
    pub fn fallback_receivable_usd_for(&self, account: &str) -> Decimal {
        self.fallback_claims
            .values()
            .filter(|c| c.account_id == account)
            .fold(Decimal::ZERO, |a, c| a + c.booked_usd())
    }

    /// Issue #991: fallback receivable claims still awaiting venue truth.
    pub fn fallback_claims(&self) -> impl Iterator<Item = &ExpiryFallbackClaim> {
        self.fallback_claims.values()
    }

    pub fn pending_fallback_claim_count(&self) -> usize {
        self.fallback_claims.len()
    }

    /// Issue #991 boundary 1: the OLDEST open fallback claim's age in ms
    /// (`None` when no fallback claim is open) — the continuous aging signal
    /// the panel's amber banner reads. The per-claim warn is edge-detected by
    /// the caller; this is the number that survives page refreshes.
    pub fn oldest_fallback_claim_age_ms(&self, now_ms: i64) -> Option<i64> {
        self.fallback_claims
            .values()
            .map(|c| now_ms - c.created_at_ms)
            .min()
    }

    pub fn claims(&self) -> impl Iterator<Item = &RedemptionClaim> {
        self.claims.values()
    }

    pub fn claim(&self, id: &str) -> Option<&RedemptionClaim> {
        self.claims.get(id)
    }

    pub fn pending_claim_count(&self) -> usize {
        self.claims.len()
    }

    /// How many claims still have an automatic attempt coming.
    pub fn retryable_claim_count(&self) -> usize {
        self.claims.values().filter(|c| !c.manual).count()
    }

    /// Manually-required claims: no retry can land them.
    pub fn manual_claim_count(&self) -> usize {
        self.claims.values().filter(|c| c.manual).count()
    }

    /// Whether the settlement path is blind: a market is watched and the newest
    /// sign of life from the venue is older than [`BLIND_AFTER_MS`], so nothing
    /// will settle until the venue answers. `None` while healthy.
    ///
    /// "Sign of life" is the first dispatch or the moment of the last answer
    /// (recorded as the re-query clock minus its interval), so both failure modes
    /// show up: a query never answered, and a venue that answered once and then
    /// went silent.
    pub fn blind_since_ms(&self, now_ms: i64) -> Option<i64> {
        self.queries
            .values()
            .filter(|q| q.first_sent_ms > 0)
            .map(|q| q.first_sent_ms.max(q.next_query_ms - REQUERY_MS))
            .min()
            .filter(|t| now_ms - *t > BLIND_AFTER_MS)
    }

    /// Edge-detect the blind state so the caller alerts once per episode rather
    /// than every tick. Returns true on the healthy → blind transition.
    pub fn note_blind_state(&mut self, blind: bool) -> bool {
        let edge = blind && !self.blind_alerted;
        self.blind_alerted = blind;
        edge
    }

    /// #684: the watched markets whose settlement query has NEVER been
    /// answered (`answers == 0`) and whose first dispatch is older than
    /// `min_ms`, as `(condition_id, first_sent_ms)` pairs. This is the
    /// structural signature of a market the venue will never rule on: the
    /// round list only carries future rounds, so an already-expired condition
    /// cannot appear in any answer, ever. A venue that CAN answer is heard
    /// within one re-query interval — every answer counts, including a
    /// `resolved: false` "not yet" — so only true silence accumulates here.
    /// Read-only accessor: the query map's shape stays private.
    pub fn unanswered_since(&self, min_ms: i64, now_ms: i64) -> Vec<(String, i64)> {
        self.queries
            .values()
            .filter(|q| q.answers == 0 && q.first_sent_ms > 0 && now_ms - q.first_sent_ms > min_ms)
            .map(|q| (q.condition_id.clone(), q.first_sent_ms))
            .collect()
    }

    // ── Queries ─────────────────────────────────────────────────────────────

    /// Mark the markets the core needs a verdict for. Called on the maintenance
    /// tick with the open positions; returns how many markets became newly
    /// tracked. A market already tracked keeps its query clock.
    ///
    /// A market whose recorded end lies far BEHIND the position's own entry is
    /// not a market end (see [`EXPIRY_TRUST_MS`]): such a position is left open
    /// rather than settled at an invented payout.
    pub fn track_markets(&mut self, positions: &[OpenPosition], now_ms: i64) -> usize {
        let mut added = 0usize;
        for p in positions {
            if p.condition_id.is_empty()
                || p.expires_at_ms > now_ms
                || p.entered_at_ms - p.expires_at_ms > EXPIRY_TRUST_MS
            {
                continue; // not past its expiry: nothing can have resolved yet
            }
            match self.queries.get_mut(&p.condition_id) {
                Some(q) => {
                    if !q.token_ids.contains(&p.token_id) {
                        q.token_ids.push(p.token_id.clone());
                    }
                }
                None => {
                    self.queries.insert(
                        p.condition_id.clone(),
                        QueryState {
                            condition_id: p.condition_id.clone(),
                            token_ids: vec![p.token_id.clone()],
                            expires_at_ms: p.expires_at_ms,
                            first_sent_ms: 0,
                            sent_at_ms: 0,
                            next_query_ms: 0,
                            answers: 0,
                        },
                    );
                    added += 1;
                }
            }
        }
        added
    }

    /// Queries the venue has not been handed yet (and that are not cooling
    /// down). Dispatching them stamps the clock, so an unanswered query is
    /// retried at most once per [`REQUERY_MS`] and its silence is measurable.
    pub fn take_queries(&mut self, now_ms: i64) -> Vec<blitzkrieg_market_api::SettlementQuery> {
        let mut out = Vec::new();
        for q in self.queries.values_mut() {
            // Waiting for a fresh answer, or inside the re-query cooldown.
            let waiting = q.sent_at_ms > 0 && now_ms - q.sent_at_ms <= REQUERY_MS;
            if waiting || (q.sent_at_ms == 0 && now_ms < q.next_query_ms) {
                continue;
            }
            if q.first_sent_ms == 0 {
                q.first_sent_ms = now_ms;
            }
            q.sent_at_ms = now_ms;
            out.push(blitzkrieg_market_api::SettlementQuery {
                condition_id: q.condition_id.clone(),
                token_ids: q.token_ids.clone(),
                expires_at_ms: q.expires_at_ms,
            });
        }
        out
    }

    /// Record that a market was answered (resolved or not), so silence and
    /// "not resolved yet" are distinguishable. A market whose positions are all
    /// settled stops being tracked, by the caller.
    pub fn note_answer(&mut self, condition_id: &str, _resolved: bool, now_ms: i64) {
        if let Some(q) = self.queries.get_mut(condition_id) {
            q.answers += 1;
            q.sent_at_ms = 0;
            q.next_query_ms = now_ms + REQUERY_MS;
        }
    }

    /// Drop the query for a market with nothing left to settle.
    pub fn forget_market(&mut self, condition_id: &str) {
        self.queries.remove(condition_id);
    }

    /// How many markets are being tracked right now.
    pub fn tracked_markets(&self) -> usize {
        self.queries.len()
    }

    /// Replay fast path: the earliest instant any tracked query may become
    /// dispatchable again — its re-query cooldown end (`next_query_ms`) when
    /// it has been answered and is waiting, or `now + REQUERY_MS` when a
    /// dispatch is currently in flight (the answer may land any time, but
    /// the next retry cannot be due before the interval ends). `i64::MAX`
    /// when nothing is tracked. The caller may skip the query drain entirely
    /// until this instant without moving any dispatch timestamp: `take_que
    /// ries` stamps only when it actually hands one out, and a skipped tick
    /// would have handed out nothing (the cooldown comparison is `<= now`).
    pub fn next_query_deadline_ms(&self) -> i64 {
        self.queries
            .values()
            .map(|q| {
                if q.sent_at_ms > 0 {
                    q.sent_at_ms + REQUERY_MS
                } else {
                    q.next_query_ms
                }
            })
            .min()
            .unwrap_or(i64::MAX)
    }

    /// The markets the book is waiting on a verdict for.
    pub fn tracked_condition_ids(&self) -> Vec<String> {
        self.queries.keys().cloned().collect()
    }

    /// Claims already redeemed (durable, across restarts).
    pub fn redeemed_count(&self) -> usize {
        self.redeemed.len()
    }

    /// The newest redemption problem, for the panel: `(claim_id, message,
    /// manual)`. `None` when nothing is wrong.
    pub fn last_error(&self) -> Option<(&str, &str, bool)> {
        self.claims
            .values()
            .filter_map(|c| {
                c.last_error
                    .as_deref()
                    .map(|e| (c.id.as_str(), e, c.manual))
            })
            .next_back()
    }

    // ── Booking (entries.rs) ──────────────────────────────────────────────

    // ── Redemption (redemptions.rs) ───────────────────────────────────────

    /// Positions whose settlement was committed but whose close never happened
    /// (a crash between the journal append and the close). The caller closes
    /// them exactly once — see `Core::recover_settlements` — and then marks them
    /// closed with [`SettlementBook::note_closed`].
    pub fn recover<'a>(&self, positions: &'a [OpenPosition]) -> Vec<&'a OpenPosition> {
        positions
            .iter()
            .filter(|p| {
                let key = super::booking::settlement_key(p);
                self.is_applied(&key) && !self.closed.contains(&key)
            })
            .collect()
    }

    /// The committed settlement of a position key, for the recovery close.
    pub fn record(&self, key: &str) -> Option<&SettlementRecord> {
        self.applied.get(key)
    }

    /// Mark a settled position's close (and trade record) as durable. Appended
    /// AFTER the trade record, so recovery that sees this line knows a second
    /// close would double-count the realized PnL.
    pub fn note_closed(&mut self, key: &str, now_ms: i64) {
        if !self.closed.insert(key.to_string()) {
            return;
        }
        if let Some(j) = self.journal.as_ref()
            && let Err(e) = j.append(&JournalLine::Closed {
                at_ms: now_ms,
                key: key.to_string(),
            })
        {
            tracing::error!(key = %key, error = %e, "settlement close store write failed");
        }
    }

    /// Has this settlement's close been made durable?
    pub fn is_closed(&self, key: &str) -> bool {
        self.closed.contains(key)
    }

    /// One-line summary for the log/panel.
    pub fn summary(&self) -> String {
        format!(
            "settled={} pending_redeem={} receivable={} manual={}",
            self.applied.len(),
            self.claims.len(),
            self.receivable_usd(),
            self.manual_claim_count()
        )
    }

    // ── Issue #991: the fallback receivable pool ────────────────────────────

    /// Issue #991: book an unverifiable-expiry close's notional as a
    /// receivable claim (the `receivable` arm of
    /// `BK_UNVERIFIED_EXPIRY_POLICY`). The journal line is the durable commit
    /// point — replaying it re-opens the claim, so the estimated notional
    /// never vanishes across a restart. Idempotent per claim id: a re-book of
    /// a live id is a no-op (the caller mints the id from the position id,
    /// and a closed position cannot fall back twice).
    #[allow(clippy::too_many_arguments)] // the claim IS the position's own facts, spelled
    pub fn book_fallback_claim(
        &mut self,
        position_id: &str,
        condition_id: &str,
        token_id: &str,
        account_id: &str,
        shares: Decimal,
        fallback_price: Decimal,
        price_source: &str,
        booked_net_pnl_usd: Decimal,
        closed_at_ms: i64,
        now_ms: i64,
    ) -> String {
        let id = format!("expiry-fallback:{position_id}");
        if self.fallback_claims.contains_key(&id) {
            return id;
        }
        self.fallback_claims.insert(
            id.clone(),
            ExpiryFallbackClaim {
                id: id.clone(),
                position_id: position_id.to_string(),
                condition_id: condition_id.to_string(),
                token_id: token_id.to_string(),
                account_id: account_id.to_string(),
                shares,
                fallback_price,
                created_at_ms: now_ms,
                price_source: price_source.to_string(),
                booked_net_pnl_usd,
                closed_at_ms,
            },
        );
        if let Some(j) = self.journal.as_ref()
            && let Err(e) = j.append(&JournalLine::ExpiryFallbackBooked {
                at_ms: now_ms,
                claim_id: id.clone(),
                position_id: position_id.to_string(),
                condition_id: condition_id.to_string(),
                token_id: token_id.to_string(),
                account_id: account_id.to_string(),
                shares,
                fallback_price,
                price_source: price_source.to_string(),
                booked_net_pnl_usd,
                closed_at_ms,
            })
        {
            tracing::error!(claim_id = %id, error = %e, "fallback claim store write failed");
        }
        id
    }

    /// Issue #991: venue truth arrived for a fallback claim — redeem it
    /// LOCALLY (an accounting event; no on-chain redeem exists for a claim
    /// whose collateral was never verified). Returns the redemption's full
    /// instruction: `(account_id, booked_usd, booked_net, closed_at_ms,
    /// shares, fallback_price)` — the caller credits
    /// `venue_truth × shares` against `booked_usd` (the #998 two-row shape)
    /// and amends the recorded trade net from `booked_net` by the same delta
    /// (the anchored identity: Δcash must equal Δrealized). Unknown or
    /// already-redeemed ids return `None` (never a second credit).
    pub fn redeem_fallback_claim(
        &mut self,
        position_id: &str,
        now_ms: i64,
    ) -> Option<(String, Decimal, Decimal, i64, Decimal, Decimal)> {
        let id = format!("expiry-fallback:{position_id}");
        let claim = self.fallback_claims.remove(&id)?;
        let booked_usd = claim.booked_usd();
        self.fallback_redeemed.insert(id.clone());
        if let Some(j) = self.journal.as_ref()
            && let Err(e) = j.append(&JournalLine::ExpiryFallbackRedeemed {
                at_ms: now_ms,
                claim_id: id,
            })
        {
            tracing::error!(claim_id = %position_id, error = %e, "fallback redeem store write failed");
        }
        Some((
            claim.account_id,
            booked_usd,
            claim.booked_net_pnl_usd,
            claim.closed_at_ms,
            claim.shares,
            claim.fallback_price,
        ))
    }

    /// Is there an OPEN fallback claim for this position?
    pub fn has_fallback_claim(&self, position_id: &str) -> bool {
        self.fallback_claims
            .contains_key(&format!("expiry-fallback:{position_id}"))
    }
}

/// The outcome-share vector a persisted settlement record describes: shares
/// parked at the record's outcome index, in the resolution's outcome order.
fn settled_shares(record: &SettlementRecord) -> Vec<Decimal> {
    let n = record.outcome_count.max(2);
    let mut v = vec![Decimal::ZERO; n];
    if let Some(i) = record.outcome_index
        && let Some(slot) = v.get_mut(i)
    {
        *slot = record.shares;
    }
    v
}
