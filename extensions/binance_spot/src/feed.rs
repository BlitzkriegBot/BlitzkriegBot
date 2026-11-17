//! Binance spot feed — raw `tokio-tungstenite` connection to the combined
//! `@trade` stream, parsed into per-asset spot ticks for the momentum filter.
//!
//! This is the loop that used to live inside `extensions/polymarket/src/feed.rs`
//! (`binance_spot_loop`). It has moved here so that Binance spot and Polymarket
//! are **two independent data sources** with independent feed tasks, independent
//! reconnect policies, and no shared channel.

use blitzkrieg_bk_contract::{MarketHost, SpotUpdate};
use futures_util::StreamExt;
use rust_decimal::Decimal;
use std::str::FromStr;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// How long to wait before dialing Binance again after a transport blip.
const RECONNECT_PAUSE_SEC: u64 = 5;

/// Read-side idle timeout. If `read.next()` yields nothing (no frame, not even
/// a Ping/Pong) for this long, the socket is half-dead (proxy tunnel silent,
/// or the peer stopped forwarding). We break out and reconnect rather than
/// hanging forever — this is the fix for the 283 s / 39 s spot freezes.
const READ_IDLE_TIMEOUT_SEC: u64 = 20;

/// Silence longer than this is logged as a gap. Same shape as the old
/// `WS_SILENCE_LOG_MS` (3500 ms) — observation only, no behaviour change.
const WS_SILENCE_LOG_MS: u128 = 3_500;

/// Run the Binance spot feed loop forever. Each trade tick is forwarded to the
/// host via `MarketHost::on_spot` carrying the upstream trade stamp `T` as
/// `trade_time`, so downstream can measure `now_ms - trade_time` for staleness.
pub async fn binance_spot_loop(host: std::sync::Arc<dyn MarketHost>, assets: Vec<String>) -> ! {
    let streams: Vec<String> = assets
        .iter()
        .map(|a| format!("{}@trade", a.to_lowercase() + "usdt"))
        .collect();
    let url = format!("wss://stream.binance.com:9443/ws/{}", streams.join("/"));

    let mut last_rx = std::time::Instant::now();
    let mut saw_trade = false;
    loop {
        match crate::proxy::connect(&url).await {
            Ok((ws, _)) => {
                host.on_spot(SpotUpdate {
                    asset: "__connect__".into(),
                    price: Decimal::ZERO,
                    ts_ms: blitzkrieg_bk_contract::net::now_ms(),
                    trade_time: None,
                })
                .await;
                // Marker event so the host knows the socket (re)connected. The
                // engine ignores unknown assets in the spot buffer.
                tracing::info!(target: "binance_spot", "connected");
                let (_write, mut read) = ws.split();
                loop {
                    // READ-SIDE IDLE TIMEOUT: if no frame arrives within the
                    // budget, the socket is half-dead. Reconnect instead of
                    // hanging until the TCP stack times out (minutes).
                    let next = tokio::time::timeout(
                        Duration::from_secs(READ_IDLE_TIMEOUT_SEC),
                        read.next(),
                    );
                    match next.await {
                        Ok(Some(Ok(Message::Text(txt)))) => {
                            if let Some((symbol, price, trade_time)) = parse_binance_trade(&txt) {
                                let silence_ms = last_rx.elapsed().as_millis();
                                if saw_trade && silence_ms >= WS_SILENCE_LOG_MS {
                                    tracing::warn!(
                                        target: "binance_spot",
                                        "ws gap silence_ms={silence_ms} asset={symbol}"
                                    );
                                }
                                saw_trade = true;
                                last_rx = std::time::Instant::now();
                                let asset = symbol
                                    .trim_end_matches("USDT")
                                    .trim_end_matches("USD")
                                    .to_string();
                                let now = blitzkrieg_bk_contract::net::now_ms();
                                host.on_spot(SpotUpdate {
                                    asset,
                                    price,
                                    ts_ms: now,
                                    trade_time: Some(trade_time),
                                })
                                .await;
                            }
                        }
                        Ok(Some(Ok(Message::Ping(_)))) | Ok(Some(Ok(Message::Pong(_)))) => {}
                        Ok(Some(Ok(Message::Close(_)))) => {
                            tracing::warn!(
                                target: "binance_spot",
                                "ws close after {}ms silence",
                                last_rx.elapsed().as_millis()
                            );
                            break;
                        }
                        Ok(Some(Err(e))) => {
                            tracing::warn!(
                                target: "binance_spot",
                                "ws error: {e}"
                            );
                            break;
                        }
                        Ok(None) => {
                            // Stream ended cleanly.
                            break;
                        }
                        Err(_elapsed) => {
                            // Read idle timeout fired — socket half-dead.
                            tracing::warn!(
                                target: "binance_spot",
                                "read idle timeout after {READ_IDLE_TIMEOUT_SEC}s — reconnecting"
                            );
                            break;
                        }
                        Ok(Some(Ok(_))) => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!(target: "binance_spot", "connect: {e}");
            }
        }
        tokio::time::sleep(Duration::from_secs(RECONNECT_PAUSE_SEC)).await;
    }
}

/// Parse a Binance combined `@trade` payload into (symbol, price, trade_time).
/// Format: {"e":"trade","s":"BTCUSDT","p":"60000.00","T":1234567890,...}
pub fn parse_binance_trade(text: &str) -> Option<(String, Decimal, i64)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("e").and_then(|e| e.as_str()) != Some("trade") {
        return None;
    }
    let symbol = v.get("s")?.as_str()?.to_string();
    let price = Decimal::from_str(v.get("p")?.as_str()?).ok()?;
    let trade_time = v.get("T")?.as_i64()?;
    Some((symbol, price, trade_time))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binance_trade_with_trade_time() {
        let msg = r#"{"e":"trade","s":"BTCUSDT","p":"60000.50","q":"0.01","T":1700000000000}"#;
        let (sym, px, t) = parse_binance_trade(msg).unwrap();
        assert_eq!(sym, "BTCUSDT");
        assert_eq!(px, Decimal::from_str("60000.50").unwrap());
        assert_eq!(t, 1_700_000_000_000);
    }

    #[test]
    fn ignores_non_trade_messages() {
        assert!(parse_binance_trade(r#"{"e":"24hrTicker","s":"BTCUSDT"}"#).is_none());
        assert!(parse_binance_trade("not json").is_none());
    }

    #[test]
    fn missing_trade_time_is_rejected() {
        // Binance trade frames always carry T; a frame without it is malformed.
        assert!(parse_binance_trade(r#"{"e":"trade","s":"BTCUSDT","p":"60000.50"}"#).is_none());
    }
}
