//! Risk gate seam (issue #84): the read-only risk/threshold abstraction the
//! core consults before admitting an entry.
//!
//! This trait declares **no threshold values** — no kill-switch language, no
//! daily-loss cap, no `RiskGate` constant. It is only the seam: a backend
//! (extension or risk service, none wired yet) owns every judgment and every
//! number. The core's `Core.risk_api` is `Option<Box<dyn RiskApi>>`; `None`
//! = disabled, and every call falls through to the default `Err` below.

use crate::account::AccountId;
use crate::types::{CoreError, CoreErrorCode};
use rust_decimal::Decimal;
use std::fmt::Debug;

/// Read-only risk gate / threshold API.
///
/// Implementors answer whether an entry is permitted and what the account's
/// day-to-date used loss is. The actual thresholds live behind the implementor;
/// this trait is intentionally empty of constants.
pub trait RiskApi: Debug + Send + Sync {
    /// Returns `Ok(true)` when the entry for `account` at `notional_usd`
    /// notional USD is permitted, `Ok(false)` when the gate rejects it.
    fn check_entry_risk(
        &self,
        _account: &AccountId,
        _notional_usd: Decimal,
    ) -> Result<bool, CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "risk_api::check_entry_risk: no backend implements this yet".to_string(),
        ))
    }

    /// The account's day-to-date realized loss in USD — the value a daily-loss
    /// cap would be compared against.
    fn daily_loss_used(&self, _account: &AccountId) -> Result<Decimal, CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "risk_api::daily_loss_used: no backend implements this yet".to_string(),
        ))
    }
}
