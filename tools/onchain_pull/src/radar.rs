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
use crate::reverse::{FillSample, ReverseAnalysis, StrategyArchetype, analyze_fills};
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
        let Some(slug) = val_str(row, &["slug", "eventSlug", "marketSlug"]) else {
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
        let Some(condition_id) = val_str(row, &["conditionId", "condition_id"]) else {
            continue;
        };
        let price = val_f64(row, &["price"]);
        if price <= 0.0 || price >= 1.0 {
            continue;
        }
        let raw_size = val_f64(row, &["size", "shares"]);
        let usdc = val_f64(row, &["usdcSize"]);
        let size_usd = if usdc > 0.0 {
            usdc
        } else {
            raw_size * price
        };
        let mut ts = val_i64(row, &["timestamp", "ts"]);
        if ts > 10_000_000_000 {
            ts /= 1000;
        }
        let side = val_str(row, &["side"]).unwrap_or("BUY").to_uppercase();
        let outcome_index = val_i64(row, &["outcomeIndex", "outcome_index"]);
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

/// Pull recent trades for a single wallet and run [`analyze_fills`].
pub async fn analyze_wallet_trades(
    fetcher: &dyn JsonFetcher,
    wallet: &str,
    market: Option<&str>,
) -> Result<ReverseAnalysis, String> {
    let w = wallet.trim();
    if !w.starts_with("0x") || w.len() != 42 {
        return Err(format!("invalid wallet address: {w}"));
    }
    let url = format!("{DATA_API_BASE}/trades?user={w}&limit=500");
    let raw = fetcher.get_json(&url).await?;
    let rows = raw
        .as_array()
        .or_else(|| raw.get("data").and_then(Value::as_array))
        .ok_or_else(|| "unexpected /trades response shape (expected array)".to_string())?;
    let samples = parse_trades_to_samples(rows, market);
    Ok(analyze_fills(w, &samples))
}

/// Scan Polymarket Crypto leaderboard, inspect top wallets' 5m/15m trades,
/// classify archetypes, and return qualified candidates.
pub async fn scan_leaderboard(
    fetcher: &dyn JsonFetcher,
    req: &RadarScanRequest,
) -> Result<RadarScanResult, String> {
    let limit = req.limit.clamp(1, 30);
    let period = match req.time_period.trim().to_uppercase().as_str() {
        "DAY" | "WEEK" | "MONTH" | "ALL" => req.time_period.trim().to_uppercase(),
        _ => "WEEK".to_string(),
    };
    let url = format!(
        "{DATA_API_BASE}/v1/leaderboard?category=CRYPTO&timePeriod={period}&orderBy=PNL&limit={limit}"
    );
    let raw = fetcher.get_json(&url).await?;
    let entries = raw
        .as_array()
        .or_else(|| raw.get("data").and_then(Value::as_array))
        .ok_or_else(|| "unexpected /v1/leaderboard response shape".to_string())?;

    let mut candidates = Vec::new();
    let mut scanned = 0usize;
    let min_fills = req.min_buy_fills.max(1);

    for row in entries.iter().take(limit) {
        let Some(wallet) = val_str(row, &["proxyWallet", "wallet", "user", "address"]) else {
            continue;
        };
        if !wallet.starts_with("0x") || wallet.len() != 42 {
            continue;
        }
        scanned += 1;
        let label = val_str(row, &["userName", "name", "pseudonym"])
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}…{}", &wallet[..6], &wallet[wallet.len() - 4..]));
        let pnl = val_f64(row, &["pnl", "profit", "netPnl"]);
        let vol = val_f64(row, &["vol", "volume"]);

        let Ok(analysis) = analyze_wallet_trades(fetcher, wallet, req.market.as_deref()).await
        else {
            continue;
        };
        let fp = &analysis.fingerprint;
        if fp.buy_fills < min_fills || analysis.archetype == StrategyArchetype::Unclassifiable {
            continue;
        }
        candidates.push(RadarCandidate {
            wallet: wallet.to_string(),
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
        });
    }

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
            format!("{DATA_API_BASE}/v1/leaderboard?category=CRYPTO&timePeriod=WEEK&orderBy=PNL&limit=10"),
            serde_json::json!([
                { "proxyWallet": w1, "userName": "sniper_btc", "pnl": 4200.5, "vol": 98000.0 },
                { "proxyWallet": w2, "userName": "noise_trader", "pnl": 300.0, "vol": 1000.0 }
            ]),
        );

        let mut w1_trades = Vec::new();
        for i in 0..24 {
            let start = 1_795_670_000 + (i as i64) * 300;
            w1_trades.push(serde_json::json!({
                "slug": format!("btc-updown-5m-{start}"),
                "conditionId": format!("0xcond{i}"),
                "side": "BUY",
                "outcomeIndex": 0,
                "price": 0.90 + (i % 5) as f64 * 0.01,
                "size": 10.0,
                "timestamp": start + 260
            }));
        }
        // Non-updown trade must be ignored.
        w1_trades.push(serde_json::json!({
            "slug": "us-election-2028",
            "conditionId": "0xother",
            "side": "BUY",
            "outcomeIndex": 0,
            "price": 0.50,
            "size": 10.0,
            "timestamp": 1_795_670_000
        }));

        routes.insert(
            format!("{DATA_API_BASE}/trades?user={w1}&limit=500"),
            Value::Array(w1_trades),
        );
        routes.insert(
            format!("{DATA_API_BASE}/trades?user={w2}&limit=500"),
            serde_json::json!([
                {
                    "slug": "btc-updown-5m-1795670000",
                    "conditionId": "0xsmall",
                    "side": "BUY",
                    "outcomeIndex": 0,
                    "price": 0.91,
                    "size": 5.0,
                    "timestamp": 1_795_670_250
                }
            ]),
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
}
