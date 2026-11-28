//! Smart Money Radar (Stage 2):
//!
//! 1. [`scan_leaderboard`] — query Polymarket's Crypto leaderboard, pull recent
//!    fills for top traders via [`JsonFetcher`], filter strictly to 5m/15m
//!    Crypto up/down rounds (`parse_round_slug`), run [`analyze_fills`], and
//!    return ranked [`RadarCandidate`] entries.
//! 2. [`analyze_wallet_trades`] — pull a single wallet's recent fills from
//!    Polymarket Data API `/trades` and run [`analyze_fills`] + Blueprint
//!    synthesis.
//!
//! All network calls go through [`JsonFetcher`] so unit tests are 100%
//! deterministic and offline.

use crate::fetch::{DATA_API_BASE, JsonFetcher};
use crate::request::parse_round_slug;
use crate::reverse::{FillSample, ReverseAnalysis, StrategyArchetype};
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarScanRequest {
    /// `"5m"` | `"15m"` | `None` (both 5m and 15m).
    #[serde(default)]
    pub market: Option<String>,
    /// Minimum filtered Crypto up/down buy fills required (default 20).
    #[serde(default = "default_min_fills")]
    pub min_buy_fills: usize,
    /// Leaderboard wallets to inspect (default 15, capped at 30).
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// `"DAY"` | `"WEEK"` | `"MONTH"` (default `"WEEK"`).
    #[serde(default = "default_period")]
    pub time_period: String,
}

fn default_min_fills() -> usize {
    20
}
fn default_limit() -> usize {
    15
}
fn default_period() -> String {
    "WEEK".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarCandidate {
    pub wallet: String,
    pub label: String,
    pub leaderboard_pnl_usd: f64,
    pub leaderboard_vol_usd: f64,
    pub archetype: StrategyArchetype,
    pub reason: String,
    pub buy_fills: usize,
    pub unique_conditions: usize,
    pub assets: Vec<String>,
    pub durations_sec: Vec<i64>,
    pub entry_px_p25: f64,
    pub entry_px_p50: f64,
    pub entry_px_p90: f64,
    pub time_left_p10: i64,
    pub time_left_p50: i64,
    pub time_left_p90: i64,
    pub pair_ratio: f64,
    pub hold_to_settlement_ratio: f64,
    pub avg_notional_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blueprint: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarScanResult {
    pub scanned_wallets: usize,
    pub qualified_count: usize,
    pub candidates: Vec<RadarCandidate>,
}

fn val_f64(v: &Value, keys: &[&str]) -> f64 {
    for k in keys {
        match v.get(*k) {
            Some(Value::Number(n)) => {
                if let Some(x) = n.as_f64() {
                    return x;
                }
            }
            Some(Value::String(s)) => {
                if let Ok(x) = s.trim().parse::<f64>() {
                    return x;
                }
            }
            _ => {}
        }
    }
    0.0
}

fn val_i64(v: &Value, keys: &[&str]) -> i64 {
    for k in keys {
        match v.get(*k) {
            Some(Value::Number(n)) => {
                if let Some(x) = n.as_i64() {
                    return x;
                }
            }
            Some(Value::String(s)) => {
                if let Ok(x) = s.trim().parse::<i64>() {
                    return x;
                }
            }
            _ => {}
        }
    }
    0
}

fn val_str<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(Value::as_str).map(str::trim)
            && !s.is_empty()
        {
            return Some(s);
        }
    }
    None
}

fn target_duration_sec(market: Option<&str>) -> Option<i64> {
    match market.map(str::trim) {
        Some("5m") => Some(300),
        Some("15m") => Some(900),
        _ => None,
    }
}

/// Convert raw Data API `/trades` rows into [`FillSample`] filtered to 5m/15m
/// Crypto up/down markets.
pub fn parse_trades_to_samples(rows: &[Value], market_filter: Option<&str>) -> Vec<FillSample> {
    let want_dur = target_duration_sec(market_filter);
    let mut out = Vec::new();
    for row in rows {
        let Some(slug) = val_str(
            row,
            &[
                "slug",
                "event_slug",
                "market_slug",
                "eventSlug",
                "marketSlug",
            ],
        ) else {
            continue;
        };
        let Some(parsed) = parse_round_slug(slug) else {
            continue;
        };
        if parsed.duration_sec != 300 && parsed.duration_sec != 900 {
            continue;
        }
        if let Some(d) = want_dur
            && parsed.duration_sec != d
        {
            continue;
        }
        let Some(condition_id) = val_str(row, &["condition_id", "conditionId"]) else {
            continue;
        };
        let price = val_f64(row, &["price"]);
        if price <= 0.0 || price >= 1.0 {
            continue;
        }
        let raw_size = val_f64(row, &["size", "shares"]);
        let usdc = val_f64(row, &["usdc_size", "usdcSize"]);
        let size_usd = if usdc > 0.0 { usdc } else { raw_size * price };
        let mut ts = val_i64(row, &["timestamp", "ts"]);
        if ts > 10_000_000_000 {
            ts /= 1000;
        }
        let side = val_str(row, &["side"]).unwrap_or("BUY").to_uppercase();
        let outcome_index = val_i64(row, &["outcome_index", "outcomeIndex"]);
        out.push(FillSample {
            condition_id: condition_id.to_string(),
            asset: parsed.asset,
            duration_sec: parsed.duration_sec,
            end_ts_sec: parsed.start_sec + parsed.duration_sec,
            fill_ts_sec: ts,
            side,
            outcome_index,
            price,
            size_usd,
            winning_outcome: None,
        });
    }
    out
}

/// Pull recent trades for a single wallet and run [`analyze_fills_with_min`].
pub async fn analyze_wallet_trades(
    fetcher: &dyn JsonFetcher,
    wallet: &str,
    market: Option<&str>,
) -> Result<ReverseAnalysis, String> {
    let default_min = if target_duration_sec(market) == Some(900) {
        10
    } else {
        crate::reverse::MIN_BUY_FILLS
    };
    analyze_wallet_trades_with_min(fetcher, wallet, market, default_min).await
}

/// Pull recent trades for a single wallet with explicit `min_buy_fills` floor.
/// When filtering to `15m` rounds (3x lower frequency than `5m`), a single
/// 500-row page is often dominated by 5m fills; automatically follow up to 2
/// extra pages via `pagination.next_cursor` (or `offset`) until `min_buy_fills`
/// is reached or the stream ends.
pub async fn analyze_wallet_trades_with_min(
    fetcher: &dyn JsonFetcher,
    wallet: &str,
    market: Option<&str>,
    min_buy_fills: usize,
) -> Result<ReverseAnalysis, String> {
    let w = wallet.trim();
    if !w.starts_with("0x") || w.len() != 42 {
        return Err(format!("invalid wallet address: {w}"));
    }
    let min_fills = min_buy_fills.max(5);
    let max_pages: usize = if target_duration_sec(market) == Some(900) {
        3
    } else {
        1
    };

    let mut samples = Vec::new();
    let mut cursor: Option<String> = None;
    for page in 0..max_pages {
        let url = match &cursor {
            Some(c) => format!("{DATA_API_BASE}/v2/trades?user={w}&limit=500&cursor={c}"),
            None if page == 0 => format!("{DATA_API_BASE}/v2/trades?user={w}&limit=500"),
            None => format!(
                "{DATA_API_BASE}/v2/trades?user={w}&limit=500&offset={}",
                page * 500
            ),
        };
        let raw = match fetcher.get_json(&url).await {
            Ok(v) => v,
            Err(_) if page > 0 => break,
            Err(e) => return Err(e),
        };
        let rows = raw
            .get("data")
            .and_then(Value::as_array)
            .or_else(|| raw.as_array())
            .ok_or_else(|| {
                "unexpected /v2/trades response shape (expected data array)".to_string()
            })?;
        samples.extend(parse_trades_to_samples(rows, market));

        let buy_count = samples
            .iter()
            .filter(|s| !s.side.eq_ignore_ascii_case("SELL"))
            .count();
        if buy_count >= min_fills || rows.len() < 500 {
            break;
        }
        cursor = raw
            .get("pagination")
            .and_then(|p| p.get("next_cursor"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }

    Ok(crate::reverse::analyze_fills_with_min(
        w, &samples, min_fills,
    ))
}

/// Scan Polymarket Crypto leaderboard, inspect top wallets' 5m/15m trades,
/// classify archetypes, and return qualified candidates.
pub async fn scan_leaderboard(
    fetcher: &dyn JsonFetcher,
    req: &RadarScanRequest,
) -> Result<RadarScanResult, String> {
    let limit = req.limit.clamp(1, 50);
    let period = match req.time_period.trim().to_ascii_lowercase().as_str() {
        "day" => "day",
        "week" => "week",
        "month" => "month",
        "all" => "all",
        _ => "week",
    };
    let pnl_url = format!(
        "{DATA_API_BASE}/v2/leaderboard?category=crypto&time_period={period}&sort_by=PNL&limit={limit}"
    );
    let raw = fetcher.get_json(&pnl_url).await?;
    let mut entries: Vec<Value> = raw
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| raw.as_array())
        .ok_or_else(|| "unexpected /v2/leaderboard response shape".to_string())?
        .clone();

    // For 15m scans, many top-PNL crypto wallets only trade long-dated price
    // markets or 5m rounds; also blend in high-volume (`sort_by=VOL`) crypto
    // traders (best-effort, deduped below) to surface active 15m specialists.
    if target_duration_sec(req.market.as_deref()) == Some(900) {
        let vol_url = format!(
            "{DATA_API_BASE}/v2/leaderboard?category=crypto&time_period={period}&sort_by=VOL&limit={limit}"
        );
        if let Ok(vol_raw) = fetcher.get_json(&vol_url).await
            && let Some(vol_entries) = vol_raw
                .get("data")
                .and_then(Value::as_array)
                .or_else(|| vol_raw.as_array())
        {
            entries.extend(vol_entries.iter().cloned());
        }
    }

    let min_fills = req.min_buy_fills.max(5);
    let mut seen_wallets = std::collections::HashSet::new();
    let mut targets: Vec<(usize, String, String, f64, f64)> = Vec::new();
    for row in entries.iter() {
        if targets.len() >= limit * 2 {
            break;
        }
        let Some(wallet) = val_str(
            row,
            &[
                "user_id",
                "proxy_wallet",
                "proxyWallet",
                "wallet",
                "user",
                "address",
            ],
        ) else {
            continue;
        };
        if !wallet.starts_with("0x") || wallet.len() != 42 {
            continue;
        }
        let key = wallet.to_ascii_lowercase();
        if !seen_wallets.insert(key) {
            continue;
        }
        let idx = targets.len();
        let label = val_str(row, &["user_name", "userName", "name", "pseudonym"])
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}…{}", &wallet[..6], &wallet[wallet.len() - 4..]));
        let pnl = val_f64(row, &["pnl", "profit", "net_pnl", "netPnl"]);
        let vol = val_f64(row, &["volume", "vol"]);
        targets.push((idx, wallet.to_string(), label, pnl, vol));
    }
    let scanned = targets.len();
    let market = req.market.as_deref();

    // Fan out per-wallet trade fetches with bounded concurrency (8 in-flight)
    // instead of sequential round-trips, cutting scan wall time while
    // preserving leaderboard rank order.
    let mut ranked: Vec<(usize, RadarCandidate)> = stream::iter(targets)
        .map(|(idx, wallet, label, pnl, vol)| async move {
            let analysis = analyze_wallet_trades_with_min(fetcher, &wallet, market, min_fills)
                .await
                .ok()?;
            let fp = &analysis.fingerprint;
            if fp.buy_fills < min_fills || analysis.archetype == StrategyArchetype::Unclassifiable {
                return None;
            }
            Some((
                idx,
                RadarCandidate {
                    wallet,
                    label,
                    leaderboard_pnl_usd: (pnl * 100.0).round() / 100.0,
                    leaderboard_vol_usd: (vol * 100.0).round() / 100.0,
                    archetype: analysis.archetype,
                    reason: analysis.reason,
                    buy_fills: fp.buy_fills,
                    unique_conditions: fp.unique_conditions,
                    assets: fp.assets.clone(),
                    durations_sec: fp.durations_sec.clone(),
                    entry_px_p25: fp.entry_px_p25,
                    entry_px_p50: fp.entry_px_p50,
                    entry_px_p90: fp.entry_px_p90,
                    time_left_p10: fp.time_left_p10,
                    time_left_p50: fp.time_left_p50,
                    time_left_p90: fp.time_left_p90,
                    pair_ratio: fp.pair_ratio,
                    hold_to_settlement_ratio: fp.hold_to_settlement_ratio,
                    avg_notional_usd: fp.avg_notional_usd,
                    blueprint: analysis.blueprint,
                },
            ))
        })
        .buffer_unordered(8)
        .filter_map(|x| async move { x })
        .collect()
        .await;
    ranked.sort_by_key(|(idx, _)| *idx);
    let candidates: Vec<RadarCandidate> = ranked.into_iter().map(|(_, c)| c).collect();

    Ok(RadarScanResult {
        scanned_wallets: scanned,
        qualified_count: candidates.len(),
        candidates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;

    struct MockFetcher {
        routes: HashMap<String, Value>,
    }

    impl JsonFetcher for MockFetcher {
        fn get_json<'a>(
            &'a self,
            url: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
            let res = self
                .routes
                .get(url)
                .cloned()
                .ok_or_else(|| format!("unmocked url: {url}"));
            Box::pin(async move { res })
        }
    }

    #[tokio::test]
    async fn radar_scan_filters_5m_wallets_and_synthesizes_blueprint() {
        let w1 = "0x1111111111111111111111111111111111111111";
        let w2 = "0x2222222222222222222222222222222222222222";
        let mut routes = HashMap::new();
        routes.insert(
            format!(
                "{DATA_API_BASE}/v2/leaderboard?category=crypto&time_period=week&sort_by=PNL&limit=10"
            ),
            serde_json::json!({
                "data": [
                    { "rank": 1, "user_id": w1, "user_name": "sniper_btc", "pnl": 4200.5, "volume": 98000.0 },
                    { "rank": 2, "user_id": w2, "user_name": "noise_trader", "pnl": 300.0, "volume": 1000.0 }
                ],
                "pagination": { "next_cursor": null }
            }),
        );

        let mut w1_trades = Vec::new();
        for i in 0..24 {
            let start = 1_795_670_000 + (i as i64) * 300;
            w1_trades.push(serde_json::json!({
                "slug": format!("btc-updown-5m-{start}"),
                "condition_id": format!("0xcond{i}"),
                "side": "BUY",
                "outcome_index": 0,
                "price": 0.90 + (i % 5) as f64 * 0.01,
                "size": 10.0,
                "timestamp": start + 260
            }));
        }
        // Non-updown trade must be ignored.
        w1_trades.push(serde_json::json!({
            "slug": "us-election-2028",
            "condition_id": "0xother",
            "side": "BUY",
            "outcome_index": 0,
            "price": 0.50,
            "size": 10.0,
            "timestamp": 1_795_670_000
        }));

        routes.insert(
            format!("{DATA_API_BASE}/v2/trades?user={w1}&limit=500"),
            serde_json::json!({ "data": w1_trades, "pagination": { "next_cursor": null } }),
        );
        routes.insert(
            format!("{DATA_API_BASE}/v2/trades?user={w2}&limit=500"),
            serde_json::json!({
                "data": [
                    {
                        "slug": "btc-updown-5m-1795670000",
                        "condition_id": "0xsmall",
                        "side": "BUY",
                        "outcome_index": 0,
                        "price": 0.91,
                        "size": 5.0,
                        "timestamp": 1_795_670_250
                    }
                ],
                "pagination": { "next_cursor": null }
            }),
        );

        let fetcher = MockFetcher { routes };
        let res = scan_leaderboard(
            &fetcher,
            &RadarScanRequest {
                market: Some("5m".into()),
                min_buy_fills: 20,
                limit: 10,
                time_period: "WEEK".into(),
            },
        )
        .await
        .expect("scan succeeds");

        assert_eq!(res.scanned_wallets, 2);
        assert_eq!(res.qualified_count, 1);
        let c = &res.candidates[0];
        assert_eq!(c.wallet, w1);
        assert_eq!(c.label, "sniper_btc");
        assert_eq!(c.archetype, StrategyArchetype::LateFavorite);
        assert_eq!(c.buy_fills, 24);
        assert!(c.blueprint.is_some());
    }

    #[tokio::test]
    async fn radar_scan_15m_blends_vol_leaderboard_and_paginates_trades() {
        let w_pnl = "0x3333333333333333333333333333333333333333";
        let w_vol = "0x4444444444444444444444444444444444444444";
        let mut routes = HashMap::new();
        routes.insert(
            format!(
                "{DATA_API_BASE}/v2/leaderboard?category=crypto&time_period=week&sort_by=PNL&limit=10"
            ),
            serde_json::json!({
                "data": [
                    { "rank": 1, "user_id": w_pnl, "user_name": "long_bettor", "pnl": 9000.0, "volume": 50000.0 }
                ]
            }),
        );
        routes.insert(
            format!(
                "{DATA_API_BASE}/v2/leaderboard?category=crypto&time_period=week&sort_by=VOL&limit=10"
            ),
            serde_json::json!({
                "data": [
                    { "rank": 1, "user_id": w_vol, "user_name": "bot_15m", "pnl": 1800.0, "volume": 420000.0 }
                ]
            }),
        );
        routes.insert(
            format!("{DATA_API_BASE}/v2/trades?user={w_pnl}&limit=500"),
            serde_json::json!({
                "data": [{ "slug": "btc-100k-nov", "condition_id": "0xlong", "side": "BUY", "outcome_index": 0, "price": 0.6, "size": 100.0, "timestamp": 1_795_670_000 }]
            }),
        );
        // Page 1 for w_vol: 494 5m trades + 6 15m trades (full 500-row page, below min=10)
        let mut page1 = Vec::with_capacity(500);
        for i in 0..494 {
            let start = 1_795_670_000 + (i as i64) * 300;
            page1.push(serde_json::json!({
                "slug": format!("btc-updown-5m-{start}"),
                "condition_id": format!("0x5m_{i}"),
                "side": "BUY",
                "outcome_index": 0,
                "price": 0.90,
                "size": 10.0,
                "timestamp": start + 260
            }));
        }
        for i in 0..6 {
            let start = 1_795_670_000 + (i as i64) * 900;
            page1.push(serde_json::json!({
                "slug": format!("btc-updown-15m-{start}"),
                "condition_id": format!("0x15m_a_{i}"),
                "side": "BUY",
                "outcome_index": 0,
                "price": 0.87,
                "size": 10.0,
                "timestamp": start + 680
            }));
        }
        // Page 2 for w_vol: 6 more 15m trades via cursor=p2
        let mut page2 = Vec::new();
        for i in 0..6 {
            let start = 1_795_660_000 + (i as i64) * 900;
            page2.push(serde_json::json!({
                "slug": format!("eth-updown-15m-{start}"),
                "condition_id": format!("0x15m_b_{i}"),
                "side": "BUY",
                "outcome_index": 0,
                "price": 0.89,
                "size": 10.0,
                "timestamp": start + 700
            }));
        }
        routes.insert(
            format!("{DATA_API_BASE}/v2/trades?user={w_vol}&limit=500"),
            serde_json::json!({ "data": page1, "pagination": { "next_cursor": "p2" } }),
        );
        routes.insert(
            format!("{DATA_API_BASE}/v2/trades?user={w_vol}&limit=500&cursor=p2"),
            serde_json::json!({ "data": page2, "pagination": { "next_cursor": null } }),
        );

        let fetcher = MockFetcher { routes };
        let res = scan_leaderboard(
            &fetcher,
            &RadarScanRequest {
                market: Some("15m".into()),
                min_buy_fills: 10,
                limit: 10,
                time_period: "WEEK".into(),
            },
        )
        .await
        .expect("15m scan succeeds");

        assert_eq!(res.scanned_wallets, 2);
        assert_eq!(res.qualified_count, 1);
        let c = &res.candidates[0];
        assert_eq!(c.wallet, w_vol);
        assert_eq!(c.archetype, StrategyArchetype::LateFavorite);
        assert_eq!(c.buy_fills, 12);
        assert!(c.blueprint.is_some());
    }
}
