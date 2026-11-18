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
    pub unknown_fills: Vec<VenueTrade>,
    /// Fills the sweep applied to an order that had already ended (issue #178):
    /// the size is booked, the terminal status keeps, and the caller reports it.
    /// Before the fix these silently "revived" a Cancelled order into
    /// PartiallyFilled, so a cancelled leg came back to life in the exit rules.
    pub late_fills: Vec<LateFill>,
}
