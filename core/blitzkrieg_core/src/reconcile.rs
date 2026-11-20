//! Reconciliation — compare the local OME against an authoritative exchange
//! snapshot (current open orders + recent trades) and repair drift.
//!
//! Pure logic over snapshots so it is unit-testable and identical for the
//! live REST sweep and a replayed/offline source. It handles:
//!
//!  - missed fills: a trade exists at the venue for our order but the OME never
//!    applied it (WS gap) → synthesise an authoritative cumulative Fill
//!  - ghost orders: local thinks an order is LIVE but the venue reports it gone
//!    AND it has a fill that completed it → mark Filled; otherwise it was
//!    cancelled away → mark Cancelled. We never invent a resting order.
//!
//! Caller is responsible for fetching the snapshot and executing any venue
//! cancel for true ghosts; this module returns explicit actions.

use crate::model::*;
use crate::service::Core;

mod audit;
mod fills;
mod ghosts;
#[cfg(test)]
mod tests;
mod types;

pub use audit::*;
pub use types::*;

/// Reconcile the OME against an authoritative snapshot.
///
/// `ome.apply_fill` stays the only mutator of fill state. Ghost status repair
/// walks `Core::cancel` / `release_order_bookkeeping` (issue #55) so the
/// terminal guard and reservation release run exactly like an explicit cancel.
pub fn reconcile(core: &mut Core, snap: &VenueSnapshot) -> CoreResult<ReconcileReport> {
    reconcile_scoped(core, snap, &std::collections::HashSet::new())
}

/// [`reconcile`] scoped to ONE reporting venue.
///
/// `skip` holds the core ids that belong to a DIFFERENT venue's executor: that
/// snapshot is not authoritative for them, so they are left untouched. Without
/// the scope every sweep judged every venue's live orders, and with two plugins
/// running each one force-closed the other's resting orders as ghosts — while
/// they kept resting at their own venue, unmanaged (the orphan sweep skips ids
/// `known_venue_ids` still lists).
pub fn reconcile_scoped(
    core: &mut Core,
    snap: &VenueSnapshot,
    skip: &std::collections::HashSet<OrderId>,
) -> CoreResult<ReconcileReport> {
    let mut report = ReconcileReport::default();
    fills::repair_missed_fills(&mut core.ome, snap, &mut report)?;
    ghosts::detect_ghosts(core, snap, skip, &mut report)?;
    Ok(report)
}
