//! Binance spot market extension for the Blitzkrieg trading core.
//!
//! Provides a standalone `DataFeed` (Binance `@trade` combined stream) that is
//! **independent of Polymarket**. Previously the Binance spot loop lived inside
//! `extensions/polymarket/src/feed.rs` — an accidental coupling of two fully
//! independent data sources. This crate hosts it on its own `MarketPlugin`,
//! with its own feed task, its own reconnect policy, and no shared channel
//! with the Polymarket orderbook feed.
//!
//! Everything Binance-spot-specific lives here: the feed (`feed`), the plugin
//! registration (`plugin`). The core links this crate via its `binance_spot`
//! feature and sees only the `MarketPlugin` trait object.

use blitzkrieg_bk_contract::market_data_api::MarketDataApi;
use blitzkrieg_bk_contract::{
    BoxFuture, CoreError, CoreErrorCode, TokenId, TopOfBookUpdate,
};
use rust_decimal::Decimal;

pub mod feed;
pub mod plugin;
mod proxy;

pub use plugin::BinanceSpotPlugin;

// ── MarketDataApi skeleton (issue #36): honest default, like StoreApi ──
// Issue #82 execution seam skeleton: not wired to any venue yet — every call
// fails closed with `CoreErrorCode::Internal` until venue integration lands.
use blitzkrieg_bk_contract::execution_api::ExecutionApi;
use blitzkrieg_bk_contract::types::PendingOrder;

#[derive(Debug)]
pub struct BinanceSpotMarketData;

impl Default for BinanceSpotMarketData {
    fn default() -> Self {
        Self
    }
}

impl MarketDataApi for BinanceSpotMarketData {
    fn subscribe(&self, _tokens: Vec<TokenId>) -> BoxFuture<'_, Result<(), CoreError>> {
        Box::pin(async move {
            Err(CoreError::new(
                CoreErrorCode::Internal,
                "market_data_api::subscribe: no backend implements this yet",
            ))
        })
    }

    fn best_bid_ask(&self, _token_id: &TokenId) -> Result<Option<(Decimal, Decimal)>, CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "market_data_api::best_bid_ask: no backend implements this yet",
        ))
    }

    fn top_of_book(&self, _token_id: &TokenId) -> Result<Option<TopOfBookUpdate>, CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "market_data_api::top_of_book: no backend implements this yet",
        ))
    }
}

/// Binance spot order placement/cancellation backend — not wired yet.
#[derive(Debug)]
pub struct BinanceSpotExecutionApi;

impl ExecutionApi for BinanceSpotExecutionApi {
    fn place_order(&self, _order: &PendingOrder) -> BoxFuture<'_, Result<String, CoreError>> {
        Box::pin(async move {
            Err(CoreError::new(
                CoreErrorCode::Internal,
                "execution_api::place_order: no backend implements this yet".to_string(),
            ))
        })
    }

    fn cancel_order(&self, _core_order_id: &str) -> BoxFuture<'_, Result<(), CoreError>> {
        Box::pin(async move {
            Err(CoreError::new(
                CoreErrorCode::Internal,
                "execution_api::cancel_order: no backend implements this yet".to_string(),
            ))
        })
    }
}
// ── Strategy lifecycle seam (issue #39): no backend yet ─────────────────

use blitzkrieg_bk_contract::strategy_api::StrategyApi;

/// `StrategyApi` backend for the Binance spot extension — currently the honest
/// "no backend implements this yet" refusal. The core holds
/// `Option<Box<dyn StrategyApi>>` (`None` = disabled); once a backend exists
/// the composition root installs `Some(Box::new(BinanceSpotStrategyApi))`.
#[derive(Debug, Default)]
pub struct BinanceSpotStrategyApi;

impl StrategyApi for BinanceSpotStrategyApi {
    fn load_strategy(&self, _name: &str) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "strategy_api::load_strategy: no backend implements this yet".to_string(),
        ))
    }
    fn enable_strategy(&self, _name: &str) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "strategy_api::enable_strategy: no backend implements this yet".to_string(),
        ))
    }
}
use blitzkrieg_bk_contract::event_api::EventApi;

/// Binance Spot event bus stub: no EventApi backend exists yet, so every
/// call refuses and `Core.event_api` stays `None`. Wire a real Binance
/// publisher/subscriber here once one exists.
#[derive(Debug)]
pub struct BinanceSpotEventApi;

impl EventApi for BinanceSpotEventApi {
    fn publish(&self, topic: &str, payload: &str) -> Result<(), CoreError> {
        let _ = (topic, payload);
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "event_api::publish: no backend implements this yet",
        ))
    }

    fn subscribe(&self, topic: &str) -> Result<(), CoreError> {
        let _ = topic;
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "event_api::subscribe: no backend implements this yet",
        ))
    }
}
