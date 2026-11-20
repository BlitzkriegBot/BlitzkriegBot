//! Ghost detection (step 2 of `reconcile_scoped`).
//! Moved verbatim from `reconcile.rs` (pure code motion).
//!
//! Issue #55: ghost orders must NOT be closed through `ome.mark_terminal`
//! directly. The terminal transition is routed through `Core::cancel` for the
//! `Cancelled` path (its #7 terminal guard + reservation release + venue cancel
//! queue + emit) and, for the non-cancel terminal states (`Filled` /
//! `ForceClosed`), through `release_order_bookkeeping` + `mark_terminal` — the
//! same bookkeeping `Core::cancel` runs — so no reservation outlives its order
//! and an already-terminal order is never silently overwritten (#54's release
//! guard on `mark_terminal` applies here too).

use crate::model::*;
use crate::service::Core;
use rust_decimal::Decimal;

use super::types::{ReconcileAction, ReconcileReport, VenueSnapshot};

/// Orders we still consider live are snapshot first so the OME is not borrowed
/// while we mutate it.
pub(super) fn detect_ghosts(
    core: &mut Core,
    snap: &VenueSnapshot,
    skip: &std::collections::HashSet<OrderId>,
    report: &mut ReconcileReport,
) -> CoreResult<()> {
    let open: std::collections::HashSet<&str> =
        snap.open_order_ids.iter().map(String::as_str).collect();
    let candidates: Vec<(OrderId, Option<OrderId>)> = core
        .ome
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
        let fresh_status = core
            .ome
            .get(&core_id)
            .map(|o| (o.status, o.filled_size, o.size));
        match fresh_status {
            Some((status, filled, size)) if status == OrderStatus::Filled || filled >= size => {
                if status != OrderStatus::Filled {
                    // Venue reports a full fill the OME hasn't absorbed yet.
                    // The sweep (reconcile_for_venue) runs release_order_bookkeeping
                    // with `?` exactly once for MarkedFilled/MarkedForceClosed, so
                    // detect_ghosts only stamps the terminal status here — a second
                    // release would observe an already-emptied reservation and abort.
                    let order = core
                        .ome
                        .get(&core_id)
                        .cloned()
                        .expect("ghost order just observed live");
                    let was_live = order.status.is_live();
                    // Filled-absorption bookkeeping: release exactly once here
                    // (the sweep only persists MarkedFilled). Errors are
                    // swallowed like the pre-#54 path — the sweep's ForceClosed
                    // arm is the fail-closed release point with `?`.
                    let _ = core.release_order_bookkeeping(&order, was_live, "reconcile");
                    core.ome
                        .mark_terminal(&core_id, OrderStatus::Filled, snap.now_ms)?;
                }
                report.actions.push(ReconcileAction::MarkedFilled {
                    core_order_id: core_id,
                });
            }
            Some((_, filled, _)) if filled > Decimal::ZERO => {
                // Partial fill but the venue dropped the order: cancel through
                // Core::cancel so its #7 terminal guard, reservation release,
                // venue-cancel queue and emit run exactly like an explicit
                // operator cancel (issue #55).
                core.cancel(&core_id, snap.now_ms)?;
                report.actions.push(ReconcileAction::MarkedCancelled {
                    core_order_id: core_id,
                });
            }
            _ => {
                let submitted_at_ms = core
                    .ome
                    .get(&core_id)
                    .map(|o| o.submitted_at_ms)
                    .unwrap_or(0);
                if snap.now_ms - submitted_at_ms < 10_000 {
                    continue;
                }
                // A ghost: the venue has neither the order nor any fill for it.
                // #457: this is the kernel closing an order nobody else closed
                // — ForceClosed, not Cancelled.
                // Stamp ForceClosed; release_order_bookkeeping runs exactly once
                // with `?` in reconcile_for_venue's MarkedForceClosed arm —
                // detect_ghosts must NOT release again (result discarded here,
                // second release sees an empty reservation and aborts the sweep).
                core.ome
                    .mark_terminal(&core_id, OrderStatus::ForceClosed, snap.now_ms)?;
                report.suspect_ghost_ids.push(core_id.clone());
                report.actions.push(ReconcileAction::MarkedForceClosed {
                    core_order_id: core_id,
                });
            }
        }
    }

    Ok(())
}
