//! Receivable claims: redemptions, pair merges and unverifiable-expiry fallbacks.
//! Moved verbatim from `settlement.rs` (pure code motion).

use blitzkrieg_market_api::RedemptionRequest;
use rust_decimal::Decimal;

/// A settled position whose collateral has not reached the wallet yet: the
/// receivable. One claim per (market, account): a market with positions on
/// both outcomes redeems once — but positions of DIFFERENT accounts on the
/// same market never share a claim (the payout lands in the wallet that paid
/// for the shares, §9.2; the account split happens at booking time).
#[derive(Debug, Clone, PartialEq)]
pub struct RedemptionClaim {
    pub id: String,
    pub condition_id: String,
    /// The account whose wallet the redemption pays into — carried from the
    /// position's `account_id` at booking time (E28 §9.2, the same principle
    /// `MergeClaim.account_id` already lives by). A claim redeems into
    /// exactly one book.
    pub account_id: String,
    /// #647: the venue whose on-chain collateral this claim redeems —
    /// carried from the position's execution venue at booking time (the same
    /// stamp the executor drain routes orders by). Empty = untagged: the
    /// request drains to the host's active plugin, the pre-#647 behaviour.
    pub venue: String,
    pub neg_risk: bool,
    pub winning_token_id: String,
    /// Shares held per outcome, aligned to the resolution's outcome order: the
    /// NegRisk adapter redeems exactly these amounts.
    pub outcome_shares: Vec<Decimal>,
    pub payout_usd: Decimal,
    pub position_ids: Vec<String>,
    pub created_at_ms: i64,
    /// Attempts handed to the venue (bumped when a request is dispatched, so a
    /// lost result cannot turn into a spin).
    pub attempts: u32,
    pub next_attempt_ms: i64,
    /// No retry can fix this one (the signer does not hold the positions, the
    /// market is not resolved on-chain yet): reported once, left to the operator.
    pub manual: bool,
    pub last_error: Option<String>,
    pub tx_hash: Option<String>,
    pub block_number: Option<u64>,
}

impl RedemptionClaim {
    /// The boundary DTO the venue executes.
    pub fn request(&self) -> RedemptionRequest {
        RedemptionRequest {
            id: self.id.clone(),
            condition_id: self.condition_id.clone(),
            neg_risk: self.neg_risk,
            outcome_shares: self.outcome_shares.clone(),
            expected_payout_usd: self.payout_usd,
            winning_token_id: self.winning_token_id.clone(),
            venue: self.venue.clone(),
        }
    }
}

/// A merge whose collateral is still on its way: complete share-pairs were
/// burned on-chain at intent time, but the $1.00/pair of USDC only reaches the
/// wallet when the venue's merge transaction mines. The pairs are an account
/// receivable, not cash, until that receipt — the SAME principle settlement
/// lives by, and for the same reason: crediting `balance` at intent time says
/// the wallet holds money it does not hold, and the accounting audit's venue
/// leg (live) or the local cash identity (dry) goes red, correctly.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeClaim {
    pub id: String,
    pub condition_id: String,
    pub account_id: String,
    /// #647: the venue the burned pairs sit on — carried from the merging
    /// legs' execution venue, so the merge request drains to the executor
    /// that can actually run the CTF merge. Empty = untagged (active-plugin
    /// fallback, the pre-#647 behaviour).
    pub venue: String,
    /// The market settles through the NegRisk adapter (drives the merge call).
    pub neg_risk: bool,
    /// Complete pairs burned = the USDC collateral due (1.00 per pair).
    pub pairs_usd: Decimal,
    pub created_at_ms: i64,
    pub attempts: u32,
    pub next_attempt_ms: i64,
    pub manual: bool,
    pub last_error: Option<String>,
    pub tx_hash: Option<String>,
    pub block_number: Option<u64>,
}

impl MergeClaim {
    /// The boundary DTO the venue executes.
    pub fn request(&self) -> blitzkrieg_market_api::MergeRequest {
        blitzkrieg_market_api::MergeRequest {
            id: self.id.clone(),
            condition_id: self.condition_id.clone(),
            neg_risk: self.neg_risk,
            pairs_usd: self.pairs_usd,
            venue: self.venue.clone(),
        }
    }
}

/// Issue #991/#1002: an unverifiable-expiry close's notional, parked as a
/// RECEIVABLE instead of spendable cash (`BK_UNVERIFIED_EXPIRY_POLICY=receivable`).
///
/// A MINIMAL claim, the #461 merge-claim shape: it carries only what the
/// fallback path actually knows (the position id, the account, the fallback
/// price and the share count it was priced at). It is deliberately NOT a
/// [`RedemptionClaim`]: a redemption claim describes ON-CHAIN collateral to
/// redeem through the CTF adapter (outcome-share vectors, winning token,
/// NegRisk) — data a venue verdict that has not arrived cannot supply, and
/// inventing it would be redeeming a fabrication (#684's own reasoning). The
/// venue's cash truth for this claim is an ACCOUNTING event, not an on-chain
/// redeem: when a resolution or a reconcile fill prices the condition, the
/// claim redeems LOCALLY (receivable → cash) with #998-style correction rows
/// for the venue-vs-fallback delta.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpiryFallbackClaim {
    pub id: String,
    /// The position the fallback close wrote off.
    pub position_id: String,
    /// The market whose verdict this claim is waiting on.
    pub condition_id: String,
    /// The outcome token the position held — the key venue truth prices the
    /// redemption with (`resolution.payout_per_share(&token_id)`, or a venue
    /// sell fill's price on this token from the reconcile sweep).
    pub token_id: String,
    /// The account whose book carries the receivable (§9.2 attribution — the
    /// same rule `MergeClaim.account_id` and `RedemptionClaim.account_id` live
    /// by: a claim redeems into exactly one book).
    pub account_id: String,
    /// Shares written off at close (`closed.dust_shares` — the un-sold
    /// remainder `close()` priced at the fallback price).
    pub shares: Decimal,
    /// The bookkeeping price the close booked (`price_source` names its basis:
    /// `"binary_threshold"` for a real observation, `"unobserved"` when no
    /// quote ever arrived, in which case this is zero): the receivable's
    /// booked amount is `fallback_price × shares`.
    pub fallback_price: Decimal,
    pub created_at_ms: i64,
    /// The close's `price_source` spelling (`"binary_threshold"` |
    /// `"unobserved"`; issue #176 deleted `"current"` / `"entry_fallback"`) —
    /// carried so the redemption's correction row can name what the estimate
    /// was based on. Additive field.
    pub price_source: String,
    /// The trade record's net PnL as booked at close — the retract triple's
    /// third leg. The redemption amends the recorded net to venue truth
    /// (Δrealized must equal the ledger's correction delta or the anchored
    /// trade-leg identity breaks structurally), and matching on
    /// (position id, exit time, booked net) is what keeps the amendment from
    /// eating a different close.
    pub booked_net_pnl_usd: Decimal,
    /// The close's `exited_at_ms` — the retract triple's second leg.
    pub closed_at_ms: i64,
}

impl ExpiryFallbackClaim {
    /// The booked (estimated) amount: what the close priced the un-sold shares
    /// at. The redemption books cash against THIS number as the model row and
    /// corrects to venue truth.
    pub fn booked_usd(&self) -> Decimal {
        self.fallback_price * self.shares
    }
}
