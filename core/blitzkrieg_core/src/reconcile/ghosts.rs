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
                    // #184: stamp with `VenueFill` evidence and release with
                    // `?` — FAIL-CLOSED. The old `let _ =` here could leave a
                    // terminal `Filled` row whose reservation was never
                    // released (a stray hold the audit halts on).
                    //
                    // The release is PRE-FLIGHTED first (`ensure_release_exact`):
                    // if the book disagrees with the order's commitment, the
                    // sweep aborts with the order still live and the next pass
                    // retries the whole transition — never a half-completed
                    // pair (terminal row / un-released hold).
                    let order = core
                        .ome
                        .get(&core_id)
                        .cloned()
                        .expect("ghost order just observed live");
                    core.ensure_release_exact(&order)?;
                    let was_live = order.status.is_live();
                    core.ome.mark_terminal_with_evidence(
                        &core_id,
                        OrderStatus::Filled,
                        crate::model::TerminalEvidenceSource::VenueFill {
                            venue_order_id: venue_id.clone(),
                        },
                        snap.now_ms,
                    )?;
                    core.release_order_bookkeeping(&order, was_live, "reconcile")?;
                }
                report.actions.push(ReconcileAction::MarkedFilled {
                    core_order_id: core_id,
                });
            }
            Some((_, filled, _)) if filled > Decimal::ZERO => {
                // Partial fill but the venue dropped the order. The snapshot
                // proves the venue no longer holds it, so the withdrawal the
                // kernel wanted has taken effect — that absence IS the
                // evidence. Stamp first, release second (#184); the old shape
                // routed this through `Core::cancel`, whose new semantics
                // correctly refuse to terminalise without an ack, so the two
                // had to be separated.
                let order = core
                    .ome
                    .get(&core_id)
                    .cloned()
                    .expect("ghost order just observed live");
                core.ensure_release_exact(&order)?;
                let was_live = order.status.is_live();
                core.ome.mark_terminal_with_evidence(
                    &core_id,
                    OrderStatus::Cancelled,
                    crate::model::TerminalEvidenceSource::VenueCancelAck {
                        venue_order_id: venue_id.clone(),
                    },
                    snap.now_ms,
                )?;
                core.release_order_bookkeeping(&order, was_live, "reconcile")?;
                core.emit_order(&core_id);
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
                // #457 + #184: the kernel is closing an order nobody else
                // closed — `ForceClosed` with `ReconciledGhost` evidence (no
                // venue receipt is claimed). The sweep's MarkedForceClosed arm
                // runs the release with `?` exactly once, so the release is
                // pre-flighted here first: a book that disagrees aborts with
                // the order still live rather than half-completing the pair.
                let order = core
                    .ome
                    .get(&core_id)
                    .cloned()
                    .expect("ghost order just observed live");
                core.ensure_release_exact(&order)?;
                core.ome.mark_terminal_with_evidence(
                    &core_id,
                    OrderStatus::ForceClosed,
                    crate::model::TerminalEvidenceSource::ReconciledGhost,
                    snap.now_ms,
                )?;
                report.suspect_ghost_ids.push(core_id.clone());
                report.actions.push(ReconcileAction::MarkedForceClosed {
                    core_order_id: core_id,
                });
            }
        }
    }

    Ok(())
}
