//! `MarketDataApi` — the unified market-data READ seam (issue #36).
//!
//! The trait lives in `blitzkrieg-bk-contract` (leaf crate) and is implemented
//! by market extensions (polymarket, binance_spot). The core holds an
//! `Option<Box<dyn MarketDataApi>>` and never knows which venue is behind it.
//! Every method defaults to an honest `Err` ("no backend implements this yet"),
//! mirroring `StoreApi` — a missing backend fails loud instead of silently
//! returning empty data.

use crate::types::{CoreError, CoreErrorCode};
use crate::types::*;
use std::fmt::Debug;
use rust_decimal::Decimal;
use crate::BoxFuture;

/// Unified market-data READ API.
///
/// Implemented by market extensions (polymarket, binance_spot, …). The core
/// holds `Option<Box<dyn MarketDataApi>>` and calls through it without knowing
/// the venue. Every method defaults to an `Err` so an absent backend fails
/// loud instead of fabricating prices.
pub trait MarketDataApi: Debug + Send + Sync {
    /// Subscribe to market-data updates for `tokens`.
    ///
    /// Default: no backend implements this yet.
    fn subscribe(&self, _tokens: Vec<TokenId>) -> BoxFuture<'_, Result<(), CoreError>> {
        Box::pin(async move {
            Err(CoreError::new(
                CoreErrorCode::Internal,
                "market_data_api::subscribe: no backend implements this yet",
            ))
        })
    }

    /// Best bid / best ask for `token_id`.
    ///
    /// Default: no backend implements this yet.
    fn best_bid_ask(&self, _token_id: &TokenId) -> Result<Option<(Decimal, Decimal)>, CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "market_data_api::best_bid_ask: no backend implements this yet",
        ))
    }

    /// Full top-of-book update for `token_id`.
    ///
    /// Default: no backend implements this yet.
    fn top_of_book(&self, _token_id: &TokenId) -> Result<Option<TopOfBookUpdate>, CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "market_data_api::top_of_book: no backend implements this yet",
        ))
    }
}
