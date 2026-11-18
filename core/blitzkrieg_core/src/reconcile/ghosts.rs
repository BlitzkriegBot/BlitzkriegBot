//! Ghost detection (step 2 of `reconcile_scoped`).
//! Moved verbatim from `reconcile.rs` (pure code motion).

use crate::model::*;
use crate::ome::Ome;
use rust_decimal::Decimal;

use super::types::{ReconcileAction, ReconcileReport, VenueSnapshot};

/// Orders we still consider live are snapshot first so the OME is not borrowed
/// while we mutate it.
pub(super) fn detect_ghosts(
    ome: &mut Ome,
    snap: &VenueSnapshot,
    skip: &std::collections::HashSet<OrderId>,
    report: &mut ReconcileReport,
) -> CoreResult<()> {
    let open: std::collections::HashSet<&str> =
        snap.open_order_ids.iter().map(String::as_str).collect();
    let candidates: Vec<(OrderId, Option<OrderId>)> = ome
        .live_orders()
        .into_iter()
        .map(|o| (o.order_id.clone(), o.venue_order_id.clone()))
        .collect();

    for (core_id, venue_id) in candidates {
        if skip.contains(&core_id) {
            // Another venue's executor owns this order: this snapshot cannot
            // speak for it (its own sweep decides).
            continue;
        }
        let Some(venue_id) = venue_id else {
            // Dry orders have no venue id; never touch them from a venue snapshot.
            continue;
        };
        if open.contains(venue_id.as_str()) {
            continue; // venue still has it open — fine
        }
        // Gone from the venue. Refresh: a fill may have completed it in step 1.
        let fresh_status = ome.get(&core_id).map(|o| (o.status, o.filled_size, o.size));
        match fresh_status {
            Some((status, filled, size)) if status == OrderStatus::Filled || filled >= size => {
                if status != OrderStatus::Filled {
                    ome.mark_terminal(&core_id, OrderStatus::Filled, snap.now_ms)?;
                }
                report.actions.push(ReconcileAction::MarkedFilled {
                    core_order_id: core_id,
                });
            }
            Some((_, filled, _)) if filled > Decimal::ZERO => {
                ome.mark_terminal(&core_id, OrderStatus::Cancelled, snap.now_ms)?;
                report.actions.push(ReconcileAction::MarkedCancelled {
                    core_order_id: core_id,
                });
            }
            _ => {
                let submitted_at_ms = ome.get(&core_id).map(|o| o.submitted_at_ms).unwrap_or(0);
                if snap.now_ms - submitted_at_ms < 10_000 {
                    continue;
                }
                // A ghost: the venue has neither the order nor any fill for it.
                // #457: this is the kernel closing an order nobody else closed
                // — ForceClosed, not Cancelled.
                ome.mark_terminal(&core_id, OrderStatus::ForceClosed, snap.now_ms)?;
                report.suspect_ghost_ids.push(core_id.clone());
                report.actions.push(ReconcileAction::MarkedForceClosed {
                    core_order_id: core_id,
                });
            }
        }
    }

    Ok(())
}
