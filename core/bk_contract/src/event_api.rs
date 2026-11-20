//! Unified event publish/subscribe seam (issue #41).
//!
//! `blitzkrieg-bk-contract` is a leaf crate (DEV §2.1 #2), so this trait
//! speaks only primitives (`&str` topic/payload) and never references
//! `UnifiedEvent` or any core type. A real backend is installed on
//! `Core.event_api` (`None` = disabled); until one exists every call below
//! refuses with `CoreErrorCode::Internal`.

use crate::types::{CoreError, CoreErrorCode};
use std::fmt::Debug;

/// Unified event publish/subscribe API.
///
/// Venue backends publish to / subscribe from an external bus (NATS, Redis
/// streams, …). The core holds at most one `Box<dyn EventApi>` in
/// [`crate::service::Core::event_api`]; `None` means event publishing is
/// disabled and every call refuses.
pub trait EventApi: Debug + Send + Sync {
    /// Publish `payload` on `topic`. Refuses until a backend is wired.
    fn publish(&self, topic: &str, payload: &str) -> Result<(), CoreError> {
        let _ = (topic, payload);
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "event_api::publish: no backend implements this yet",
        ))
    }

    /// Subscribe to `topic`. Refuses until a backend is wired.
    fn subscribe(&self, topic: &str) -> Result<(), CoreError> {
        let _ = topic;
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "event_api::subscribe: no backend implements this yet",
        ))
    }
}
