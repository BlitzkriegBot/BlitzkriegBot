//! Chainlink 60s TWAP oracle reference-price extension for the Blitzkrieg trading core.
//!
//! Provides a standalone `DataFeed` streaming the Chainlink 60-second
//! time-weighted average price (`price.crypto.twap`, `window_seconds = 60`)
//! used by Polymarket binary crypto up/down market resolution. Replaces the
//! retired `binance_spot` extension.
//!
//! Everything Chainlink-specific lives here: the feed (`feed`), the plugin
//! registration (`plugin`), and the proxy tunnel (`proxy`). The core links this
//! crate via the launcher's `chainlink` feature and sees only the `MarketPlugin`
//! trait object.

use blitzkrieg_bk_contract::event_api::EventApi;
use blitzkrieg_bk_contract::execution_api::ExecutionApi;
use blitzkrieg_bk_contract::market_data_api::MarketDataApi;
use blitzkrieg_bk_contract::strategy_api::StrategyApi;
use blitzkrieg_bk_contract::types::PendingOrder;
use blitzkrieg_bk_contract::{BoxFuture, CoreError, CoreErrorCode, TokenId, TopOfBookUpdate};
use rust_decimal::Decimal;

pub mod feed;
pub mod plugin;
mod proxy;

pub use plugin::ChainlinkPlugin;

/// Read-only oracle reference feed: no orderbook surface.
#[derive(Debug, Default)]
pub struct ChainlinkMarketData;

impl MarketDataApi for ChainlinkMarketData {
    fn subscribe(&self, _tokens: Vec<TokenId>) -> BoxFuture<'_, Result<(), CoreError>> {
        Box::pin(async move {
            Err(CoreError::new(
                CoreErrorCode::Internal,
                "market_data_api::subscribe: chainlink is a reference price feed only",
            ))
        })
    }

    fn best_bid_ask(&self, _token_id: &TokenId) -> Result<Option<(Decimal, Decimal)>, CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "market_data_api::best_bid_ask: chainlink is a reference price feed only",
        ))
    }

    fn top_of_book(&self, _token_id: &TokenId) -> Result<Option<TopOfBookUpdate>, CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "market_data_api::top_of_book: chainlink is a reference price feed only",
        ))
    }
}

/// Chainlink is an oracle price source, never an execution venue.
#[derive(Debug, Default)]
pub struct ChainlinkExecutionApi;

impl ExecutionApi for ChainlinkExecutionApi {
    fn place_order(&self, _order: &PendingOrder) -> BoxFuture<'_, Result<String, CoreError>> {
        Box::pin(async move {
            Err(CoreError::new(
                CoreErrorCode::Internal,
                "execution_api::place_order: chainlink is a reference price feed only".to_string(),
            ))
        })
    }

    fn cancel_order(&self, _core_order_id: &str) -> BoxFuture<'_, Result<(), CoreError>> {
        Box::pin(async move {
            Err(CoreError::new(
                CoreErrorCode::Internal,
                "execution_api::cancel_order: chainlink is a reference price feed only".to_string(),
            ))
        })
    }
}

/// Strategy lifecycle stub: Chainlink hosts no strategies.
#[derive(Debug, Default)]
pub struct ChainlinkStrategyApi;

impl StrategyApi for ChainlinkStrategyApi {
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

/// Event bus stub: Chainlink pushes ticks via `MarketHost::on_spot`.
#[derive(Debug, Default)]
pub struct ChainlinkEventApi;

impl EventApi for ChainlinkEventApi {
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
