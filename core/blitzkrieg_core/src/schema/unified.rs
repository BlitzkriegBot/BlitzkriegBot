//! Unified mapping surface: `unified.candidates` / `unified.confirm` /
//! `unified.revoke`. Moved verbatim from `schema/mod.rs` (pure code motion).

use blitzkrieg_market_api::unified::PlatformListing;
use serde::Deserialize;

/// `unified.confirm` params. `listings` is the candidate pair verbatim (the
/// `unified.candidates` rows round-trip: `PlatformListing` is serde both
/// ways); the ledger revalidates everything on its side.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedConfirmParams {
    pub title: String,
    pub listings: Vec<PlatformListing>,
    /// Audit trail only; defaults to "panel".
    #[serde(default)]
    pub actor: Option<String>,
}

/// `unified.revoke` params.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedRevokeParams {
    pub event_id: String,
    #[serde(default)]
    pub actor: Option<String>,
}
