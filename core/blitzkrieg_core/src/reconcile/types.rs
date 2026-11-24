//! Venue-level reconciliation types.
//! Moved verbatim from `reconcile.rs` (pure code motion).

use crate::model::*;
use crate::ome::{FillDelta, LateFill};

#[derive(Debug, Clone, PartialEq)]
pub struct VenueTrade {
    pub venue_order_id: String,
    pub trade_id: String,
    pub token_id: TokenId,
    pub side: Side,
    /// Per-trade executed size (NOT cumulative).
    pub size: rust_decimal::Decimal,
    pub price: rust_decimal::Decimal,
    pub ts_ms: i64,
    pub tx_hash: Option<String>,
    /// The venue's maker/taker report for this trade, when it has one. A gap
    /// fill synthesized from it must charge the fee the venue actually charged,
    /// so the role travels with the trade rather than being re-guessed (E17).
    pub maker: Option<bool>,
    /// #175: the account whose cash this trade moved. `None` = the venue side
    /// could not prove one. That is a first-class "unknown", NOT a prompt to
    /// fall back to the `default` book: a token can be held by several
    /// accounts at once, so the account is the only thing that makes an
    /// external fill attributable. An unattributed fill is parked and
    /// reported (see [`FoldVerdict`]).
    pub account: Option<AccountId>,
}

/// #175: what happened to ONE venue trade during the fold.
///
/// The defect this replaces was a silent best-effort: `apply_external_fills`
/// matched an external fill to the first open position with the same token id
/// (no account comparison) and hard-coded `default_account_id()` for unknown
/// BUYs. With two accounts holding one token, account B's manual venue sell
/// closed account A's position and booked A a realized gain while B's cash was
/// gone and B's shares were still on the book — cash moved between books that
/// had never met.
///
/// A fold is therefore a VERDICT, not a side effect: every trade either folds
/// into exactly the position and account it is proven to belong to, or it is
/// parked. Parked is a first-class state — reported through
/// `positions.unverified_exposure` and announced with a `RiskAlert` — because
/// "we do not know whose money this was" must never be rounded to "nobody's".
#[derive(Debug, Clone, PartialEq)]
pub enum FoldVerdict {
    /// The trade was applied to this position, in this account's book.
    Folded {
        position: String,
        account: AccountId,
    },
    /// No account on the trade, or the account/token pair resolved to more
    /// than one open position: refused, never guessed.
    Parked(UnattributedFill),
}

/// #175: an external fill the core refused to attribute. Everything needed to
/// hand it to an operator (and to the unverified-exposure report) without
/// re-deriving anything, plus the reason it could not be folded.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnattributedFill {
    pub venue: String,
    pub venue_order_id: String,
    pub trade_id: String,
    pub token_id: TokenId,
    pub account: Option<AccountId>,
    pub side: Side,
    #[serde(with = "crate::decimal")]
    pub size: rust_decimal::Decimal,
    #[serde(with = "crate::decimal")]
    pub price: rust_decimal::Decimal,
    pub ts_ms: i64,
    /// Which attribution rule refused: `missing_account`, `ambiguous_position`
    /// or `unknown_position`. Greppable, and the same word rides the RiskAlert.
    pub reason: UnattributedReason,
}

/// #175: why a venue trade could not be attributed to one position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnattributedReason {
    /// The venue-side row carried no account (the ABI addition is optional).
    /// Folding it would mean picking a book — the exact bug #175 fixes.
    MissingAccount,
    /// The account WAS named, but that (account, token) pair matches more than
    /// one open position: the book itself cannot say which row the sell
    /// reduces, so no row is touched.
    AmbiguousPosition,
    /// The account was named and no open position of ours matches it on the
    /// token at all (a sell against inventory this kernel never opened).
    UnknownPosition,
}

impl UnattributedReason {
    pub fn as_str(self) -> &'static str {
        match self {
            UnattributedReason::MissingAccount => "missing_account",
            UnattributedReason::AmbiguousPosition => "ambiguous_position",
            UnattributedReason::UnknownPosition => "unknown_position",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct VenueSnapshot {
    /// Exchange order ids currently resting/open.
    pub open_order_ids: Vec<String>,
    /// Recent trades for our orders (any status); used to fill gaps.
    pub trades: Vec<VenueTrade>,
    pub now_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
// `FilledGap` carries a whole `FillDelta` (~232 bytes) while the other variants
// carry only an order id, so the enum is as large as its largest variant. Boxing
// the delta would shrink every action at the cost of a heap allocation and an
// indirection on every match arm of a type with three variants — and this enum
// is constructed once per reconcile gap, not in a hot loop. Not worth it.
#[allow(clippy::large_enum_variant)]
pub enum ReconcileAction {
    /// A fill missing from the local OME was applied (WS gap repaired).
    FilledGap {
        core_order_id: OrderId,
        delta: FillDelta,
    },
    /// Local-live order is fully filled per the venue (no longer open).
    MarkedFilled { core_order_id: OrderId },
    /// Local-live order vanished from the venue with no fill → marked cancelled.
    MarkedCancelled { core_order_id: OrderId },
    /// Local-live order the venue no longer knows and no fill ever completed:
    /// the kernel closed it itself (#457). The `Cancelled` vs `ForceClosed`
    /// split is the audit trail's only way to tell "we withdrew" from
    /// "we gave up waiting".
    MarkedForceClosed { core_order_id: OrderId },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReconcileReport {
    pub actions: Vec<ReconcileAction>,
    /// Local-live orders with no venue presence and no completing fill; a real
    /// ghost that may warrant a cancel if the venue snapshot is trustworthy.
    pub suspect_ghost_ids: Vec<OrderId>,
    /// Trades the venue executed on orders the OME has never heard of — a
    /// manual close the operator made directly on the venue. The OME cannot
    /// apply these (no order to fill), so the caller folds them into the
    /// position book by token id; otherwise the position is a ghost the exit
    /// rules keep managing forever.
    ///
    /// #175: the token is no longer enough to fold — the caller must also be
    /// able to name the ACCOUNT (see [`VenueTrade::account`]), and reports each
    /// outcome as a [`FoldVerdict`].
    pub unknown_fills: Vec<VenueTrade>,
    /// Fills the sweep applied to an order that had already ended (issue #178):
    /// the size is booked, the terminal status keeps, and the caller reports it.
    /// Before the fix these silently "revived" a Cancelled order into
    /// PartiallyFilled, so a cancelled leg came back to life in the exit rules.
    pub late_fills: Vec<LateFill>,
}
