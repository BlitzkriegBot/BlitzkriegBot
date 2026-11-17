//! Binance spot market plugin — registration seam for the core.

use blitzkrieg_bk_contract::{
    BoxFuture, CoreResult, DataFeed, DataFeedConfig, MarketCapabilities, MarketHost, MarketMode,
    MarketPlugin, MarketStructure, MarketType, TokenId,
};
use std::sync::Arc;

pub struct BinanceSpotPlugin;

impl BinanceSpotPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for BinanceSpotPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl MarketPlugin for BinanceSpotPlugin {
    fn name(&self) -> &str {
        "binance_spot"
    }
    fn market_type(&self) -> MarketType {
        MarketType::Spot
    }
    /// Capabilities: websocket feed (pushes `@trade` ticks to the host via
    /// `on_spot`). No orderbook snapshot, no execution — this is a reference
    /// price feed for the momentum filter only.
    fn declare_modes(&self) -> Vec<MarketMode> {
        vec![MarketMode {
            market_type: MarketType::Spot,
            structure: Some(MarketStructure::CentralLimitOrderBook),
            capabilities: MarketCapabilities::WEBSOCKET_FEED,
        }]
    }
    /// Supported round lengths: spot has no rounds — undeclared (`None`),
    /// meaning the kernel treats every duration as possible. The panel filters
    /// nothing for spot; spot is a reference feed, not a trading venue here.
    fn supported_round_secs(&self) -> Option<Vec<i64>> {
        None
    }
    fn round_labels(&self) -> &[(i64, &'static str)] {
        &[]
    }
    fn data_feed(&self) -> Option<&dyn DataFeed> {
        Some(&BINANCE_SPOT_FEED)
    }
    /// Spot feed is read-only and credential-free — no API keys needed for the
    /// public `@trade` stream.
    fn required_credential_env(&self) -> &[&str] {
        &[]
    }
}

// ── Data feed ──────────────────────────────────────────────────────────────

struct BinanceSpotDataFeed;

static BINANCE_SPOT_FEED: BinanceSpotDataFeed = BinanceSpotDataFeed;

impl DataFeed for BinanceSpotDataFeed {
    fn name(&self) -> &str {
        "binance-spot"
    }

    fn start<'a>(
        &'a self,
        host: Arc<dyn MarketHost>,
        config: DataFeedConfig,
        _initial_tokens: Vec<TokenId>,
    ) -> BoxFuture<'a, CoreResult<()>> {
        Box::pin(async move {
            // `config.spot_assets` names the assets to stream (e.g. ["BTC","ETH"]).
            // Empty = no spot feed (dry runs, or operator opted out).
            if config.spot_assets.is_empty() {
                return Ok(());
            }
            // `binance_spot_loop` never returns (reconnects forever); matching on
            // `!` coerces to `CoreResult<()>`.
            match crate::feed::binance_spot_loop(host, config.spot_assets).await {}
        })
    }
}
