//! Settlement and redemption (issue #175).
//!
//! A position only becomes USDC two ways: somebody buys it, or the market
//! resolves and the winning conditional tokens are redeemed on-chain. Before
//! this module the kernel only knew the first — a position that survived to its
//! market's resolution stayed on the books at a marked price forever, its cash
//! locked until an operator redeemed by hand. For a small live account one
//! stuck position is a fifth of the trading capital.
//!
//! The flow, and why it is shaped this way:
//!
//! ```text
//!   venue answers a resolution          core books the settlement
//!   ─────────────────────────           ────────────────────────
//!   MarketResolution ──► booking_for ──► close position at the payout price
//!                                        (trade record, reason `settlement`)
//!                                     ──► open a RedemptionClaim (RECEIVABLE)
//!   RedemptionResult ◄── redeem ────────────────────────────────┘
//!        └─ confirmed ──► receipt → balance (cash in hand)
//! ```
//!
//! The one design decision worth stating plainly: **the payout is an account
//! receivable, not cash, until the redemption is mined.** Crediting `balance`
//! at settlement would say the wallet holds money it does not hold yet, and the
//! in-kernel accounting audit compares `balance` against the venue's reported
//! free cash every 30s (and blocks new entries on drift) — it would go red
//! immediately, correctly. `CashIdentity` therefore carries `receivable`
//! alongside `balance`, and the identity telescopes over their sum: settling
//! moves no money, redeeming moves the receivable into cash, and the audit
//! stays exactly as true at every instant. See `reconcile::CashIdentity`.
//!
//! Idempotency is durable, not in-memory: every booking is appended to a
//! settlement journal before the position is closed, and the journal is
//! replayed at startup. A second settlement round — after a restart, or with a
//! position book restored from a snapshot written before the close — must not
//! book the same position twice, and must not lose the claim whose cash is
//! still on-chain. `SettlementBook::recover` closes that window explicitly.

use crate::store::StoreApi;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

mod book;
mod booking;
mod claims;
mod constants;
mod entries;
mod merge;
mod record;
mod redemptions;

#[cfg(test)]
mod tests;

pub use booking::*;
pub use claims::*;
pub use constants::*;
pub use record::*;

/// The settlement book: what was settled (durable), what is still owed to us
/// (the receivable), and what has been asked of the venue.
pub struct SettlementBook {
    /// Applied settlements by key — the idempotency table.
    applied: BTreeMap<String, SettlementRecord>,
    /// Settlements whose close + trade record were made durable.
    closed: BTreeSet<String>,
    /// Claims not yet redeemed, by id.
    claims: BTreeMap<String, RedemptionClaim>,
    /// Claim ids already redeemed, so an id is never reused.
    redeemed: BTreeSet<String>,
    /// Per-condition redeemed-claim count (drives claim id suffixes).
    redeemed_per_condition: BTreeMap<String, u32>,
    /// Open settlement queries by condition id.
    queries: BTreeMap<String, QueryState>,
    /// Settlements booked this session (diagnostic).
    booked: u64,
    /// Whether the blind state was already alerted (edge detection).
    blind_alerted: bool,
    /// #461: complete-pair merges burned but not yet confirmed on-chain, by id.
    merges: BTreeMap<String, MergeClaim>,
    /// Merge ids already confirmed, so an id is never confirmed twice.
    merged: BTreeSet<String>,
    /// Issue #991: unverifiable-expiry closes booked as receivables
    /// (`BK_UNVERIFIED_EXPIRY_POLICY=receivable`), by id.
    fallback_claims: BTreeMap<String, ExpiryFallbackClaim>,
    /// Fallback-claim ids already redeemed, so an id never redeems twice.
    fallback_redeemed: BTreeSet<String>,
    journal: Option<SettlementJournal>,
    /// The shared StoreApi — every booking also lands as a `MoneyFact::Settlement`
    /// fact beside the byte-identical JSONL journal append (R7).
    store: Option<Arc<dyn StoreApi>>,
}

/// How one market's resolution query is going.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryState {
    pub condition_id: String,
    /// Tokens the core holds on this market, as a lookup hint.
    pub token_ids: Vec<String>,
    pub expires_at_ms: i64,
    /// First time the query was handed to the venue (0 = never). The age of
    /// this stamp is what makes silence measurable: `answers == 0` after
    /// [`BLIND_AFTER_MS`] means the venue half is not answering at all.
    pub first_sent_ms: i64,
    /// Last dispatch (0 = ready to dispatch, >0 = waiting for an answer).
    pub sent_at_ms: i64,
    /// Earliest next dispatch (set after an answer).
    pub next_query_ms: i64,
    /// Answers received, resolved or not: a venue that says "not resolved yet"
    /// is alive, which is not the same as one that never says anything.
    pub answers: u32,
}

/// One line of the settlement journal. Append-only; replayed at startup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum JournalLine {
    /// A position was settled (the durable commit point).
    Settlement {
        at_ms: i64,
        // Boxed: the record is far larger than every other variant, and a journal
        // line is written/read one at a time, so the extra indirection is free.
        record: Box<SettlementRecord>,
    },
    /// The settled position's close and its trade record are durable. Recovery
    /// must then NOT close it again — a second close would write a second trade
    /// record and double the realized PnL the audit reads.
    Closed { at_ms: i64, key: String },
    /// A claim's collateral reached the wallet (`tx_hash` is `dry-simulated` in
    /// dry mode, where there is no chain to wait for).
    Redeemed {
        at_ms: i64,
        claim_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tx_hash: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        block_number: Option<u64>,
    },
    /// #461: complete pairs were burned and their collateral is now a
    /// receivable. The durable commit point of a merge — replaying it re-opens
    /// the claim, so the $1/pair is never lost across a restart.
    MergeBurned {
        at_ms: i64,
        claim_id: String,
        condition_id: String,
        account_id: String,
        /// #647: the venue the burned pairs sit on (empty on journals written
        /// before the field existed — the replayed claim drains untagged).
        #[serde(default)]
        venue: String,
        neg_risk: bool,
        #[serde(with = "crate::decimal")]
        pairs_usd: Decimal,
    },
    /// #461: the merge's collateral reached the wallet.
    MergeConfirmed {
        at_ms: i64,
        claim_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tx_hash: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        block_number: Option<u64>,
    },
    /// Issue #991: an unverifiable-expiry close booked its notional as a
    /// receivable claim. The durable commit point of the receivable model —
    /// replaying it re-opens the claim, so the estimated notional is never
    /// lost across a restart (the same shape [`JournalLine::MergeBurned`]
    /// gives the merge path).
    ExpiryFallbackBooked {
        at_ms: i64,
        claim_id: String,
        position_id: String,
        condition_id: String,
        /// The outcome token the position held (venue truth prices the
        /// redemption with).
        #[serde(default)]
        token_id: String,
        account_id: String,
        #[serde(with = "crate::decimal")]
        shares: Decimal,
        #[serde(with = "crate::decimal")]
        fallback_price: Decimal,
        /// `"binary_threshold"` (a real book observation thresholded into the
        /// binary `{0, 1}` domain) | `"unobserved"` (no quote ever arrived, the
        /// booked price is 0). Issue #176 deleted the `"current"` /
        /// `"entry_fallback"` spellings; rows written before it still replay.
        #[serde(default)]
        price_source: String,
        /// The trade record's net at close (the retract-amend match leg).
        #[serde(default, with = "crate::decimal")]
        booked_net_pnl_usd: Decimal,
        /// The close's `exited_at_ms` (the retract-amend match leg).
        #[serde(default)]
        closed_at_ms: i64,
    },
    /// Issue #991: venue truth arrived for a fallback receivable and the
    /// claim redeemed LOCALLY (receivable → cash at the venue number). This
    /// is an accounting event — no on-chain redeem request exists for a
    /// fallback claim — so there is no tx_hash/block to carry.
    ExpiryFallbackRedeemed { at_ms: i64, claim_id: String },
}

/// The journal file: append-only JSONL beside the other core logs.
struct SettlementJournal {
    path: PathBuf,
}

impl SettlementJournal {
    fn append(&self, line: &JournalLine) {
        if !crate::jsonl::append(&self.path, line) {
            tracing::warn!(
                path = %self.path.display(),
                "settlement journal append failed — settlement kept in memory, not durable; trading continues"
            );
        }
    }

    fn load(&self) -> Vec<JournalLine> {
        crate::jsonl::load::<JournalLine>(
            &self.path,
            "settlement journal: skipped unparseable lines",
        )
    }
}

/// First retry delay after a failed redemption.
fn backoff_ms(attempts: u32) -> i64 {
    let shift = attempts.saturating_sub(1).min(6);
    (constants::RETRY_BASE_MS << shift).min(constants::RETRY_MAX_MS)
}
