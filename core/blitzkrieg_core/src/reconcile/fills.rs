//! Missed-fill repair (step 1 of `reconcile_scoped`).
//! Moved verbatim from `reconcile.rs` (pure code motion).

use crate::model::{CoreResult, Fill, FillStatus};
use crate::ome::Ome;

use super::types::{ReconcileAction, ReconcileReport, VenueSnapshot};

/// Each venue trade row is its own idempotent unit (keyed by the venue's trade
/// id, shared with the user-WS channel), so applying per row — never summing an
/// order's cumulative total into one fresh-keyed fill — means a fill the WS
/// already booked is a no-op here and a missing one is booked exactly once, at
/// its own price.
pub(super) fn repair_missed_fills(
    ome: &mut Ome,
    snap: &VenueSnapshot,
    report: &mut ReconcileReport,
) -> CoreResult<()> {
    for t in &snap.trades {
        let Some(order) = ome.by_venue_or_id(&t.venue_order_id).cloned() else {
            // Not one of our orders (manual close on the venue): hand it to
            // the caller for position-book reconciliation.
            report.unknown_fills.push(t.clone());
            continue;
        };
        let trade_id = if t.trade_id.is_empty() {
            t.tx_hash.clone()
        } else {
            Some(t.trade_id.clone())
        };
        let fill = Fill {
            order_id: order.order_id.clone(),
            trade_id,
            token_id: order.token_id.clone(),
            // E28 (§9.2): a gap fill moves cash in the order's own book.
            account_id: order.account_id.clone(),
            side: order.side,
            price: t.price,
            size: t.size, // per-trade executed size, NOT cumulative
            status: FillStatus::Confirmed,
            ts_ms: t.ts_ms,
            tx_hash: t.tx_hash.clone(),
            // Carry the venue's role through the gap fill.
            maker: t.maker,
        };
        // `apply_fill_report` (not `apply_fill`): a trade for an order that has
        // already ended is absorbed into a terminal status, and the sweep must
        // hand that fact back to the caller instead of dropping it (#178).
        let outcome = ome.apply_fill_report(fill, snap.now_ms)?;
        if let Some(delta) = outcome.delta {
            report.actions.push(ReconcileAction::FilledGap {
                core_order_id: order.order_id.clone(),
                delta,
            });
        }
        if let Some(late) = outcome.late_fill {
            report.late_fills.push(late);
        }
    }
    Ok(())
}
