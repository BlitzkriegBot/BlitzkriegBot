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

pub mod feed;
pub mod plugin;
mod proxy;

pub use plugin::BinanceSpotPlugin;
