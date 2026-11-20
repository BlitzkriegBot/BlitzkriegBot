//! Execution seam: unified order placement/cancellation for market backends.
//!
//! Mirror of [`crate::store::StoreApi`]: the core holds the seam as
//! `Option<Box<dyn ExecutionApi>>`, `None` = execution disabled. Backends live
//! in `extensions/*` (one per venue, no core dependency — #841). The trait's
//! default refuses every call, so a core without a wired backend fails closed
//! instead of silently swallowing placements or cancels.

use crate::plugin::BoxFuture;
use crate::types::{CoreError, CoreErrorCode, PendingOrder};
use std::fmt::Debug;

/// Unified order placement/cancellation API for a market backend.
///
/// Implementations live in `extensions/*`. The core dispatches through the
/// `Option<Box<dyn ExecutionApi>>` slot on [`crate::blitzkrieg_core::service::Core`];
/// `None` means no backend is wired and every call fails with
/// `CoreErrorCode::Internal`.
pub trait ExecutionApi: Debug + Send + Sync {
    /// Place a locally-accepted order on the venue. Returns the venue order id.
    fn place_order(&self, order: &PendingOrder) -> BoxFuture<'_, Result<String, CoreError>> {
        let _ = order;
        Box::pin(async move {
            Err(CoreError::new(
                CoreErrorCode::Internal,
                "execution_api::place_order: no backend implements this yet".to_string(),
            ))
        })
    }

    /// Cancel a core order on the venue.
    fn cancel_order(&self, core_order_id: &str) -> BoxFuture<'_, Result<(), CoreError>> {
        let _ = core_order_id;
        Box::pin(async move {
            Err(CoreError::new(
                CoreErrorCode::Internal,
                "execution_api::cancel_order: no backend implements this yet".to_string(),
            ))
        })
    }
}
