//! Strategy lifecycle seam (issue #39): a StoreApi-style trait the core calls
//! into for strategy loading/enablement, held on `Core` as
//! `Option<Box<dyn StrategyApi>>` (`None` = disabled). Backends live in
//! extension crates; this crate only declares the seam.
//!
//! R7: the C ABI vtable (`BK_ABI_VERSION = 2`) is frozen — this is a pure
//! Rust trait seam. No ABI symbols are added, removed or changed.

use std::fmt::Debug;

use crate::{CoreError, CoreErrorCode};

/// Unified strategy lifecycle API (issue #39).
///
/// The core owns enablement (`strategy_api: None` = disabled). An extension
/// implements this trait and hands `Box<dyn StrategyApi>` to the composition
/// root at assembly; methods with no backend refuse honestly via the default
/// body, so every call site compiles and behaves identically with or without
/// a backend wired.
pub trait StrategyApi: Debug + Send + Sync {
    /// Load a strategy by name (install + validate, leave it disabled).
    fn load_strategy(&self, _name: &str) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "strategy_api::load_strategy: no backend implements this yet".to_string(),
        ))
    }

    /// Enable a loaded strategy (admission + start).
    fn enable_strategy(&self, _name: &str) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "strategy_api::enable_strategy: no backend implements this yet".to_string(),
        ))
    }
}
