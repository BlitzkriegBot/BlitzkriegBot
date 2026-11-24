//! Chainlink 60s TWAP oracle plugin — registration seam for the core.

use blitzkrieg_bk_contract::{
    BoxFuture, CoreResult, DataFeed, DataFeedConfig, MarketCapabilities, MarketHost, MarketMode,
    MarketPlugin, MarketType, TokenId,
};
use std::sync::Arc;

pub struct ChainlinkPlugin;

impl ChainlinkPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ChainlinkPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl MarketPlugin for ChainlinkPlugin {
    fn name(&self) -> &str {
        "chainlink"
    }

    fn market_type(&self) -> MarketType {
        MarketType::Spot
    }

    /// Capabilities: websocket feed pushing 60-second Chainlink TWAP settlement
    /// reference prices to the host via `on_spot`. No orderbook snapshot and no
    /// execution — this is an oracle reference feed only.
    fn declare_modes(&self) -> Vec<MarketMode> {
        vec![MarketMode {
            market_type: MarketType::Spot,
            structure: None,
            capabilities: MarketCapabilities::WEBSOCKET_FEED,
        }]
    }

    /// Reference oracle feed has no trading rounds of its own.
    fn supported_round_secs(&self) -> Option<Vec<i64>> {
        None
    }

    fn round_labels(&self) -> &[(i64, &'static str)] {
        &[]
    }

    fn data_feed(&self) -> Option<&dyn DataFeed> {
        Some(&CHAINLINK_FEED)
    }

    /// PolyBolt uses Polymarket L2 CLOB credentials when available and falls
    /// back to public RTDS in credential-free dry sessions, so no dedicated
    /// credentials block startup.
    fn required_credential_env(&self) -> &[&str] {
        &[]
    }
}

// ── Data feed ──────────────────────────────────────────────────────────────

struct ChainlinkDataFeed;

static CHAINLINK_FEED: ChainlinkDataFeed = ChainlinkDataFeed;

impl DataFeed for ChainlinkDataFeed {
    fn name(&self) -> &str {
        "chainlink"
    }

    fn start<'a>(
        &'a self,
        host: Arc<dyn MarketHost>,
        config: DataFeedConfig,
        _initial_tokens: Vec<TokenId>,
    ) -> BoxFuture<'a, CoreResult<()>> {
        Box::pin(async move {
            if config.spot_assets.is_empty() {
                return Ok(());
            }
            // `chainlink_twap_loop` never returns (reconnects forever). It must
            // run on a background task so kernel boot is never blocked before
            // the IPC socket is bound.
            tokio::spawn(
                async move { crate::feed::chainlink_twap_loop(host, config.spot_assets).await },
            );
            Ok(())
        })
    }
}

#[cfg(test)]
mod start_must_return_tests {
    fn data_feed_impl_src() -> &'static str {
        let src = include_str!("plugin.rs");
        let start = src
            .find("impl DataFeed for ChainlinkDataFeed")
            .expect("DataFeed impl");
        match src[start..].find("mod start_must_return_tests") {
            Some(rel) => &src[start..start + rel],
            None => &src[start..],
        }
    }

    /// `chainlink_twap_loop` never returns. Awaiting it in `DataFeed::start`
    /// blocks kernel boot so the IPC socket is never bound.
    #[test]
    fn start_spawns_the_loop_and_does_not_await_it() {
        let src = data_feed_impl_src();
        assert!(
            src.contains("tokio::spawn"),
            "DataFeed::start must spawn the never-returning chainlink_twap_loop so the kernel can bind its IPC socket"
        );
        assert!(
            !src.contains("match crate::feed::chainlink_twap_loop(host, config.spot_assets).await"),
            "DataFeed::start must not await chainlink_twap_loop as its own future"
        );
    }
}
