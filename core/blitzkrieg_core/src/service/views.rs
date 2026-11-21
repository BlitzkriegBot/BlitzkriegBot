//! Read-only view structs the diagnostics panels consume, moved verbatim
//! from `service.rs` (pure code motion, no logic edits). `service.rs`
//! re-exports everything (`pub use views::*;`), so `crate::service::RoundView`
//! and friends resolve exactly as before the split.

use rust_decimal::Decimal;

/// Compact round info for the UI.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoundView {
    pub slot: i64,
    pub age_sec: i64,
    pub time_left_sec: i64,
    pub markets: usize,
    pub can_trade: bool,
    /// Per-asset live prices (from the local book mid, falling back to the
    /// scanner's last price). For the UI Prices section.
    pub market_prices: Vec<MarketPriceView>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketPriceView {
    pub asset: String,
    pub up: Decimal,
    pub down: Decimal,
}

/// 盘口深度 view (E8-c): one round asset with both token books, as the UI depth
/// chart renders them. Levels are best-first and capped per side by the caller.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetBooksView {
    /// 行自身的 venue 标签（镜像盘显示簿合成行，2026-10-25）：server 端
    /// 按它打标签，不再按 token 反查 —— 镜像行与被镜像所共享 token，
    /// `venue_of` 反查会首中被镜像所，标签就错了。
    #[serde(rename = "venue")]
    pub venue: String,
    pub asset: String,
    /// The row's own family duration in seconds (0 = legacy / undeclared).
    /// The panel filters depth the same way it filters price cards.
    #[serde(rename = "durationSec", default)]
    pub duration_sec: i64,
    /// E29 (§12.2): the aggregator keys prediction-token bars by TOKEN id, so
    /// the panel needs the same key `feed_klines` was fed to ask
    /// `kline.history` for the right series.
    pub up_token_id: String,
    pub down_token_id: String,
    pub up: BookSideView,
    pub down: BookSideView,
}

/// One token's live book. Every metric is `null` when no book has arrived for
/// the token — never a zero stand-in, which the panel must not read as a real
/// quote (from_levels' empty-book sentinels stay internal to the strategy
/// layer, they are not observables).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookSideView {
    pub bids: Vec<BookLevelView>,
    pub asks: Vec<BookLevelView>,
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
    pub mid_price: Option<Decimal>,
    /// Order-book imbalance (bid_depth − ask_depth) / (bid_depth + ask_depth).
    pub obi: Option<Decimal>,
    pub spread: Option<Decimal>,
    pub spread_pct: Option<Decimal>,
}

impl BookSideView {
    pub(super) fn empty() -> Self {
        Self {
            bids: Vec::new(),
            asks: Vec::new(),
            best_bid: None,
            best_ask: None,
            mid_price: None,
            obi: None,
            spread: None,
            spread_pct: None,
        }
    }
}

/// One account's current venue round — the single source of truth the
/// panel's market section (countdown, round id, elapsed, progress bar,
/// price cards, order book) must render. Duration is the account's
/// anchored family, never a reconstructed expires/slot or the process
/// default grid. `start_ms`/`end_ms` are the real window (a daily
/// America/New_York noon-to-noon can be 23h or 25h on DST days).
/// `round_id` is the venue slug start Unix timestamp.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentRoundView {
    pub account_id: String,
    pub venue: String,
    /// The account's anchored family duration (300 / 900 / 3600 / 86400).
    pub duration_sec: i64,
    pub slot: i64,
    /// Venue start-Unix-ts identifier (the exchange slug's start).
    pub round_id: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub age_sec: i64,
    pub time_left_sec: i64,
    pub can_trade: bool,
    pub prices: Vec<CurrentRoundPriceView>,
    pub books: Vec<AssetBooksView>,
}

/// One live market in the account's current family, including the token
/// ids the depth / k-line cards share with the price cards.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentRoundPriceView {
    pub asset: String,
    pub venue: String,
    pub duration_sec: i64,
    pub up: Decimal,
    pub down: Decimal,
    pub up_token_id: String,
    pub down_token_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub condition_id: String,
    /// `snapshot` (placeholder REST), `live` (high-freq subscription), or
    /// `unreliable` (a book exists but the mid is not a tradeable reference,
    /// e.g. a near-expiry 0.01/1.00 stub whose mid is 0.505). Empty when the
    /// kernel has not stamped this card — never invent `live`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// Venue-or-kernel time the mid was fetched. 0 when unknown; omitted.
    #[serde(default, skip_serializing_if = "crate::service::views::is_zero_i64")]
    pub fetched_at_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookLevelView {
    pub price: Decimal,
    pub size: Decimal,
}

pub(crate) fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}
