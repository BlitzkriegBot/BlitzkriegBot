//! Chainlink 60-second TWAP reference-price feed.
//!
//! Primary transport: Polymarket PolyBolt (`wss://ws-live-v2.polymarket.com/ws`),
//! channel `price.crypto.twap` with `window_seconds = 60` (the official Chainlink
//! 60-second time-weighted average price used by binary crypto up/down market
//! settlement). Authenticated with Polymarket L2 CLOB credentials from env
//! (`POLYMARKET_API_KEY` / `POLYMARKET_SECRET` / `POLYMARKET_PASSPHRASE`, or
//! `CLOB_API_KEY` / `CLOB_SECRET` / `CLOB_PASSPHRASE`).
//!
//! Credential-free fallback: when no CLOB L2 credentials are present (e.g.
//! dry-run without keys), connects to Polymarket public RTDS
//! (`wss://ws-live-data.polymarket.com`), subscribing to
//! `crypto_prices_twap_sixty` (native 60s Chainlink TWAP) and
//! `crypto_prices_chainlink` (raw Chainlink ticks fed through a local 60-second
//! step-function TWAP window when native TWAP frames are absent).

use blitzkrieg_bk_contract::{MarketHost, SpotUpdate};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::str::FromStr;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

/// Official Polymarket PolyBolt endpoint carrying Chainlink 60s TWAP.
pub const POLYBOLT_WS_URL: &str = "wss://ws-live-v2.polymarket.com/ws";
/// Public RTDS fallback endpoint for credential-free dry sessions.
pub const RTDS_WS_URL: &str = "wss://ws-live-data.polymarket.com";
/// Settlement TWAP window in seconds.
pub const TWAP_WINDOW_SECS: u64 = 60;
const TWAP_WINDOW_MS: i64 = (TWAP_WINDOW_SECS as i64) * 1_000;

const RECONNECT_MIN_SEC: u64 = 2;
const RECONNECT_MAX_SEC: u64 = 30;
const DRAIN_RECONNECT_SEC: u64 = 3;
const READ_IDLE_TIMEOUT_SEC: u64 = 45;
const POLYBOLT_PING_SEC: u64 = 30;
const RTDS_PING_SEC: u64 = 5;
const WS_SILENCE_LOG_MS: u128 = 5_000;
/// Prefer native RTDS `crypto_prices_twap_sixty` when it ticked within this window.
const NATIVE_TWAP_PRECEDENCE_MS: i64 = 10_000;
/// PolyBolt connection limit: 64 channel+filter pairs per socket.
const MAX_SUBSCRIPTIONS_PER_CONN: usize = 64;

/// Parsed Chainlink price tick ready for `MarketHost::on_spot`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainlinkTick {
    pub asset: String,
    pub price: Decimal,
    pub trade_time: i64,
    pub source: Option<String>,
}

/// Polymarket L2 CLOB credentials for PolyBolt authentication.
#[derive(Clone)]
struct ClobCreds {
    api_key: String,
    secret: String,
    passphrase: String,
}

impl ClobCreds {
    fn from_env() -> Option<Self> {
        let first_non_empty = |keys: &[&str]| -> Option<String> {
            keys.iter().find_map(|k| {
                std::env::var(k)
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
            })
        };
        let api_key = first_non_empty(&["POLYMARKET_API_KEY", "CLOB_API_KEY"])?;
        let secret = first_non_empty(&["POLYMARKET_SECRET", "CLOB_SECRET"])?;
        let passphrase = first_non_empty(&["POLYMARKET_PASSPHRASE", "CLOB_PASSPHRASE"])?;
        Some(Self {
            api_key,
            secret,
            passphrase,
        })
    }
}

/// Normalize a PolyBolt (`btcusd`) or RTDS (`btc/usd`) symbol into the kernel's
/// uppercase base asset (`BTC`).
pub fn normalize_asset(symbol: &str) -> Option<String> {
    let clean = symbol.trim().to_ascii_uppercase();
    if clean.is_empty() {
        return None;
    }
    let base = clean
        .split_once('/')
        .map(|(b, _)| b)
        .or_else(|| clean.strip_suffix("USDT"))
        .or_else(|| clean.strip_suffix("USD"))
        .unwrap_or(clean.as_str())
        .trim();
    if base.is_empty() {
        None
    } else {
        Some(base.to_string())
    }
}

/// Format an uppercase asset (`BTC`) into PolyBolt's lowercase USD symbol (`btcusd`).
pub fn polybolt_symbol(asset: &str) -> String {
    let base = normalize_asset(asset).unwrap_or_else(|| asset.trim().to_ascii_uppercase());
    format!("{}usd", base.to_ascii_lowercase())
}

/// Format an uppercase asset (`BTC`) into RTDS Chainlink's slash symbol (`btc/usd`).
pub fn rtds_symbol(asset: &str) -> String {
    let base = normalize_asset(asset).unwrap_or_else(|| asset.trim().to_ascii_uppercase());
    format!("{}/usd", base.to_ascii_lowercase())
}

/// Build the PolyBolt L2 auth frame.
pub fn build_polybolt_auth_frame(api_key: &str, secret: &str, passphrase: &str) -> String {
    json!({
        "op": "auth",
        "rid": "a1",
        "auth": {
            "apiKey": api_key,
            "secret": secret,
            "passphrase": passphrase,
        }
    })
    .to_string()
}

/// Build a single PolyBolt batch subscribe frame for 60s Chainlink TWAP.
pub fn build_polybolt_twap_subscribe(assets: &[String]) -> String {
    let subs: Vec<Value> = assets
        .iter()
        .take(MAX_SUBSCRIPTIONS_PER_CONN)
        .map(|a| {
            json!({
                "channel": "price.crypto.twap",
                "filter": {
                    "symbol": polybolt_symbol(a),
                    "window_seconds": TWAP_WINDOW_SECS,
                }
            })
        })
        .collect();
    json!({
        "op": "subscribe",
        "rid": "s1",
        "subscriptions": subs,
    })
    .to_string()
}

/// Build an RTDS subscribe frame for `crypto_prices_twap_sixty` and
/// `crypto_prices_chainlink`.
pub fn build_rtds_subscribe(assets: &[String]) -> String {
    let mut subs = Vec::with_capacity(assets.len() * 2);
    for a in assets {
        let filter = json!({ "symbol": rtds_symbol(a) }).to_string();
        subs.push(json!({
            "topic": "crypto_prices_twap_sixty",
            "type": "*",
            "filters": filter,
        }));
        subs.push(json!({
            "topic": "crypto_prices_chainlink",
            "type": "*",
            "filters": filter,
        }));
    }
    json!({
        "action": "subscribe",
        "subscriptions": subs,
    })
    .to_string()
}

/// Parse a decimal from either a JSON string or JSON number. If `allow_e18` is
/// true and the parsed value is >= 10^15 (RTDS E18 fixed-point TWAP), scale it
/// down by 10^18.
fn parse_decimal_value(v: &Value, allow_e18: bool) -> Option<Decimal> {
    let raw = match v {
        Value::String(s) => Decimal::from_str(s.trim()).ok()?,
        Value::Number(n) => Decimal::from_str(&n.to_string()).ok()?,
        _ => return None,
    };
    if raw <= Decimal::ZERO {
        return None;
    }
    if allow_e18 {
        // RTDS `crypto_prices_twap_sixty` encoded prices as 1e18 integers.
        let e15 = Decimal::from(1_000_000_000_000_000i64);
        let e18 = Decimal::from(1_000_000_000_000_000_000i64);
        if raw >= e15 {
            return Some((raw / e18).normalize());
        }
    }
    Some(raw.normalize())
}

/// Parse a PolyBolt data frame (`v == 1`, `channel == "price.crypto.twap"`).
///
/// For snapshot frames (`snapshot == true`), returns the latest point in
/// `payload.data`. For live frames, prefers `full_accuracy_value` over float
/// `value` as documented by Polymarket PolyBolt.
pub fn parse_polybolt_frame(text: &str) -> Option<ChainlinkTick> {
    let v: Value = serde_json::from_str(text).ok()?;
    if v.get("v").and_then(Value::as_u64) != Some(1) {
        return None;
    }
    if v.get("channel").and_then(Value::as_str) != Some("price.crypto.twap") {
        return None;
    }
    let payload = v.get("payload")?;
    let asset = normalize_asset(payload.get("symbol")?.as_str()?)?;
    let source = payload
        .get("source")
        .and_then(Value::as_str)
        .map(str::to_string);

    if v.get("snapshot").and_then(Value::as_bool) == Some(true) {
        let data = payload.get("data")?.as_array()?;
        let latest = data
            .iter()
            .filter_map(|pt| {
                let ts = pt.get("timestamp")?.as_i64()?;
                let px = pt
                    .get("full_accuracy_value")
                    .and_then(|x| parse_decimal_value(x, false))
                    .or_else(|| pt.get("value").and_then(|x| parse_decimal_value(x, false)))?;
                Some((ts, px))
            })
            .max_by_key(|(ts, _)| *ts)?;
        return Some(ChainlinkTick {
            asset,
            price: latest.1,
            trade_time: latest.0,
            source,
        });
    }

    let price = payload
        .get("full_accuracy_value")
        .and_then(|x| parse_decimal_value(x, false))
        .or_else(|| {
            payload
                .get("value")
                .and_then(|x| parse_decimal_value(x, false))
        })?;
    let trade_time = payload
        .get("timestamp")
        .and_then(Value::as_i64)
        .or_else(|| v.get("ts").and_then(Value::as_i64))?;
    Some(ChainlinkTick {
        asset,
        price,
        trade_time,
        source,
    })
}

/// RTDS message classification: native 60s TWAP vs raw Chainlink tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtdsTick {
    Twap60(ChainlinkTick),
    RawChainlink(ChainlinkTick),
}

/// Parse a Polymarket RTDS `crypto_prices_twap_sixty` or
/// `crypto_prices_chainlink` frame.
pub fn parse_rtds_frame(text: &str) -> Option<RtdsTick> {
    let v: Value = serde_json::from_str(text).ok()?;
    let topic = v.get("topic")?.as_str()?;
    let is_twap = match topic {
        "crypto_prices_twap_sixty" => true,
        "crypto_prices_chainlink" => false,
        _ => return None,
    };
    let payload = v.get("payload")?;
    let asset = normalize_asset(payload.get("symbol")?.as_str()?)?;
    let price = payload
        .get("full_accuracy_value")
        .and_then(|x| parse_decimal_value(x, is_twap))
        .or_else(|| {
            payload
                .get("value")
                .and_then(|x| parse_decimal_value(x, is_twap))
        })?;
    let trade_time = payload
        .get("timestamp")
        .and_then(Value::as_i64)
        .or_else(|| v.get("timestamp").and_then(Value::as_i64))?;
    let tick = ChainlinkTick {
        asset,
        price,
        trade_time,
        source: Some("chainlink".to_string()),
    };
    if is_twap {
        Some(RtdsTick::Twap60(tick))
    } else {
        Some(RtdsTick::RawChainlink(tick))
    }
}

/// Step-function 60-second time-weighted average price (TWAP) tracker.
/// Used only as a local fallback when raw `crypto_prices_chainlink` ticks arrive
/// without native `crypto_prices_twap_sixty` frames.
#[derive(Debug, Default)]
pub struct TwapWindow {
    samples: VecDeque<(i64, Decimal)>,
}

impl TwapWindow {
    pub fn push(&mut self, ts_ms: i64, price: Decimal) {
        if let Some((last_ts, _)) = self.samples.back()
            && ts_ms < *last_ts
        {
            return;
        }
        self.samples.push_back((ts_ms, price));
        let cutoff = ts_ms.saturating_sub(TWAP_WINDOW_MS);
        // Keep one sample at or before `cutoff` to anchor the left boundary.
        while self.samples.len() >= 2 && self.samples[1].0 <= cutoff {
            self.samples.pop_front();
        }
    }

    /// Compute the time-weighted average over `[now_ms - 60_000, now_ms]`.
    pub fn twap_60s(&self, now_ms: i64) -> Option<Decimal> {
        if self.samples.is_empty() {
            return None;
        }
        if self.samples.len() == 1 {
            return Some(self.samples[0].1);
        }
        let start = now_ms.saturating_sub(TWAP_WINDOW_MS);
        let mut weighted_sum = Decimal::ZERO;
        let mut total_ms: i64 = 0;

        for i in 0..self.samples.len() {
            let (t0, px) = self.samples[i];
            let t1 = if i + 1 < self.samples.len() {
                self.samples[i + 1].0
            } else {
                now_ms
            };
            let seg_start = t0.max(start);
            let seg_end = t1.min(now_ms);
            if seg_end > seg_start {
                let dt = seg_end - seg_start;
                weighted_sum += px * Decimal::from(dt);
                total_ms += dt;
            }
        }
        if total_ms > 0 {
            Some((weighted_sum / Decimal::from(total_ms)).round_dp(8).normalize())
        } else {
            self.samples.back().map(|(_, px)| *px)
        }
    }
}

/// Run the Chainlink 60s TWAP feed loop forever.
///
/// Emits each 60-second average price update to `MarketHost::on_spot` with the
/// oracle timestamp as `trade_time`.
pub async fn chainlink_twap_loop(host: std::sync::Arc<dyn MarketHost>, assets: Vec<String>) -> ! {
    let mut attempt: u32 = 0;
    loop {
        let creds = ClobCreds::from_env();
        let outcome = if let Some(c) = creds {
            let url = std::env::var("CHAINLINK_WS_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| POLYBOLT_WS_URL.to_string());
            run_polybolt_session(&host, &url, &assets, &c).await
        } else {
            let url = std::env::var("CHAINLINK_RTDS_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| RTDS_WS_URL.to_string());
            tracing::info!(
                target: "chainlink",
                "CLOB L2 credentials absent — using public RTDS fallback ({url}); set POLYMARKET_API_KEY/POLYMARKET_SECRET/POLYMARKET_PASSPHRASE for PolyBolt price.crypto.twap"
            );
            run_rtds_session(&host, &url, &assets).await
        };

        match outcome {
            SessionEnd::AuthRejected(reason) => {
                tracing::error!(
                    target: "chainlink",
                    "PolyBolt rejected credentials/policy ({reason}); pausing 60s before re-checking env"
                );
                tokio::time::sleep(Duration::from_secs(60)).await;
                attempt = 0;
            }
            SessionEnd::ServerDrain => {
                tracing::info!(
                    target: "chainlink",
                    "server draining (4003) — reconnecting in {DRAIN_RECONNECT_SEC}s"
                );
                tokio::time::sleep(Duration::from_secs(DRAIN_RECONNECT_SEC)).await;
                attempt = 0;
            }
            SessionEnd::HealthyThenDisconnected => {
                attempt = 0;
                tokio::time::sleep(Duration::from_secs(RECONNECT_MIN_SEC)).await;
            }
            SessionEnd::Error => {
                let pause = (RECONNECT_MIN_SEC.saturating_mul(1u64 << attempt.min(4)))
                    .min(RECONNECT_MAX_SEC);
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(Duration::from_secs(pause)).await;
            }
        }
    }
}

enum SessionEnd {
    AuthRejected(String),
    ServerDrain,
    HealthyThenDisconnected,
    Error,
}

async fn emit_connected_marker(host: &std::sync::Arc<dyn MarketHost>) {
    host.on_spot(SpotUpdate {
        asset: "__connect__".into(),
        price: Decimal::ZERO,
        ts_ms: blitzkrieg_bk_contract::net::now_ms(),
        trade_time: None,
    })
    .await;
}

async fn run_polybolt_session(
    host: &std::sync::Arc<dyn MarketHost>,
    url: &str,
    assets: &[String],
    creds: &ClobCreds,
) -> SessionEnd {
    let started = Instant::now();
    let (ws, _) = match crate::proxy::connect(url).await {
        Ok(ok) => ok,
        Err(e) => {
            tracing::warn!(target: "chainlink", "polybolt connect failed: {e}");
            return SessionEnd::Error;
        }
    };
    let (mut write, mut read) = ws.split();

    let auth_frame = build_polybolt_auth_frame(&creds.api_key, &creds.secret, &creds.passphrase);
    if let Err(e) = write.send(Message::text(auth_frame)).await {
        tracing::warn!(target: "chainlink", "polybolt auth send failed: {e}");
        return SessionEnd::Error;
    }

    // Wait for {"op":"authed"} within 10s.
    let auth_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let Some(rem) = auth_deadline.checked_duration_since(Instant::now()) else {
            tracing::warn!(target: "chainlink", "polybolt auth timed out");
            return SessionEnd::Error;
        };
        match tokio::time::timeout(rem, read.next()).await {
            Ok(Some(Ok(Message::Text(txt)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                    match v.get("op").and_then(Value::as_str) {
                        Some("authed") => break,
                        Some("error") => {
                            let code = v
                                .get("code")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown");
                            if code == "auth_unavailable" {
                                tracing::warn!(target: "chainlink", "polybolt auth_unavailable — retrying on fresh socket");
                                return SessionEnd::Error;
                            }
                            return SessionEnd::AuthRejected(code.to_string());
                        }
                        _ => {}
                    }
                }
            }
            Ok(Some(Ok(Message::Ping(p)))) => {
                let _ = write.send(Message::Pong(p)).await;
            }
            Ok(Some(Ok(Message::Close(f)))) => {
                let code = f.as_ref().map(|c| u16::from(c.code));
                if matches!(code, Some(4001 | 4008)) {
                    return SessionEnd::AuthRejected(format!("close {}", code.unwrap()));
                }
                return SessionEnd::Error;
            }
            Ok(Some(Err(e))) => {
                tracing::warn!(target: "chainlink", "polybolt auth read error: {e}");
                return SessionEnd::Error;
            }
            Ok(None) | Err(_) => return SessionEnd::Error,
            _ => {}
        }
    }

    let sub_frame = build_polybolt_twap_subscribe(assets);
    if let Err(e) = write.send(Message::text(sub_frame)).await {
        tracing::warn!(target: "chainlink", "polybolt subscribe send failed: {e}");
        return SessionEnd::Error;
    }

    emit_connected_marker(host).await;
    tracing::info!(
        target: "chainlink",
        "connected to PolyBolt price.crypto.twap ({TWAP_WINDOW_SECS}s) for {} asset(s)",
        assets.len().min(MAX_SUBSCRIPTIONS_PER_CONN)
    );

    let mut ping = tokio::time::interval(Duration::from_secs(POLYBOLT_PING_SEC));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping.tick().await;

    let mut last_rx = Instant::now();
    let mut saw_tick = false;

    loop {
        let idle = Duration::from_secs(READ_IDLE_TIMEOUT_SEC)
            .checked_sub(last_rx.elapsed())
            .unwrap_or(Duration::ZERO);
        tokio::select! {
            _ = ping.tick() => {
                if let Err(e) = write.send(Message::text(r#"{"op":"ping"}"#)).await {
                    tracing::warn!(target: "chainlink", "polybolt ping failed: {e}");
                    break;
                }
            }
            _ = tokio::time::sleep(idle) => {
                tracing::warn!(
                    target: "chainlink",
                    "polybolt read idle timeout after {READ_IDLE_TIMEOUT_SEC}s — reconnecting"
                );
                break;
            }
            next = read.next() => {
                match next {
                    Some(Ok(Message::Text(txt))) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                            if let Some(op) = v.get("op").and_then(Value::as_str) {
                                match op {
                                    "subscribed" => {
                                        let ch = v.get("channel").and_then(Value::as_str).unwrap_or("");
                                        let prov = v.get("provider").and_then(Value::as_str).unwrap_or("chainlink");
                                        tracing::info!(target: "chainlink", "subscribed channel={ch} provider={prov}");
                                        last_rx = Instant::now();
                                        continue;
                                    }
                                    "pong" => {
                                        last_rx = Instant::now();
                                        continue;
                                    }
                                    "error" => {
                                        let code = v.get("code").and_then(Value::as_str).unwrap_or("unknown");
                                        tracing::warn!(target: "chainlink", "polybolt server error: {code}");
                                        if matches!(code, "auth_invalid" | "bad_op" | "bad_channel" | "bad_filter" | "sub_limit" | "rate_limited" | "auth_attempts") {
                                            return SessionEnd::AuthRejected(code.to_string());
                                        }
                                        continue;
                                    }
                                    _ => {}
                                }
                            }
                            if let Some(dropped) = v.get("dropped").and_then(Value::as_u64)
                                && dropped > 0
                            {
                                tracing::warn!(target: "chainlink", "polybolt dropped={dropped} frames");
                            }
                        }
                        if let Some(tick) = parse_polybolt_frame(&txt) {
                            let silence_ms = last_rx.elapsed().as_millis();
                            if saw_tick && silence_ms >= WS_SILENCE_LOG_MS {
                                tracing::warn!(
                                    target: "chainlink",
                                    "ws gap silence_ms={silence_ms} asset={}",
                                    tick.asset
                                );
                            }
                            saw_tick = true;
                            last_rx = Instant::now();
                            host.on_spot(SpotUpdate {
                                asset: tick.asset,
                                price: tick.price,
                                ts_ms: blitzkrieg_bk_contract::net::now_ms(),
                                trade_time: Some(tick.trade_time),
                            })
                            .await;
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        last_rx = Instant::now();
                        let _ = write.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {
                        last_rx = Instant::now();
                    }
                    Some(Ok(Message::Close(f))) => {
                        let code = f.as_ref().map(|c| u16::from(c.code));
                        tracing::warn!(target: "chainlink", "polybolt closed code={code:?}");
                        return match code {
                            Some(4001 | 4008) => SessionEnd::AuthRejected(format!("close {}", code.unwrap())),
                            Some(4003) => SessionEnd::ServerDrain,
                            _ => if started.elapsed() > Duration::from_secs(60) {
                                SessionEnd::HealthyThenDisconnected
                            } else {
                                SessionEnd::Error
                            },
                        };
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::warn!(target: "chainlink", "polybolt ws error: {e}");
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    if started.elapsed() > Duration::from_secs(60) {
        SessionEnd::HealthyThenDisconnected
    } else {
        SessionEnd::Error
    }
}

async fn run_rtds_session(
    host: &std::sync::Arc<dyn MarketHost>,
    url: &str,
    assets: &[String],
) -> SessionEnd {
    let started = Instant::now();
    let (ws, _) = match crate::proxy::connect(url).await {
        Ok(ok) => ok,
        Err(e) => {
            tracing::warn!(target: "chainlink", "rtds connect failed: {e}");
            return SessionEnd::Error;
        }
    };
    let (mut write, mut read) = ws.split();
    let sub_frame = build_rtds_subscribe(assets);
    if let Err(e) = write.send(Message::text(sub_frame)).await {
        tracing::warn!(target: "chainlink", "rtds subscribe send failed: {e}");
        return SessionEnd::Error;
    }

    emit_connected_marker(host).await;
    tracing::info!(
        target: "chainlink",
        "connected to RTDS fallback (crypto_prices_twap_sixty + crypto_prices_chainlink) for {} asset(s)",
        assets.len()
    );

    let mut ping = tokio::time::interval(Duration::from_secs(RTDS_PING_SEC));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping.tick().await;

    let mut last_rx = Instant::now();
    let mut saw_tick = false;
    let mut twap_windows: HashMap<String, TwapWindow> = HashMap::new();
    let mut last_native_twap_ms: HashMap<String, i64> = HashMap::new();

    loop {
        let idle = Duration::from_secs(READ_IDLE_TIMEOUT_SEC)
            .checked_sub(last_rx.elapsed())
            .unwrap_or(Duration::ZERO);
        tokio::select! {
            _ = ping.tick() => {
                if let Err(e) = write.send(Message::text("PING")).await {
                    tracing::warn!(target: "chainlink", "rtds PING failed: {e}");
                    break;
                }
            }
            _ = tokio::time::sleep(idle) => {
                tracing::warn!(
                    target: "chainlink",
                    "rtds read idle timeout after {READ_IDLE_TIMEOUT_SEC}s — reconnecting"
                );
                break;
            }
            next = read.next() => {
                match next {
                    Some(Ok(Message::Text(txt))) => {
                        if txt == "PONG" || txt == "PING" {
                            last_rx = Instant::now();
                            continue;
                        }
                        if let Some(parsed) = parse_rtds_frame(&txt) {
                            let now = blitzkrieg_bk_contract::net::now_ms();
                            let emit = match parsed {
                                RtdsTick::Twap60(tick) => {
                                    last_native_twap_ms.insert(tick.asset.clone(), now);
                                    Some(tick)
                                }
                                RtdsTick::RawChainlink(tick) => {
                                    let win = twap_windows.entry(tick.asset.clone()).or_default();
                                    win.push(tick.trade_time, tick.price);
                                    let native_fresh = last_native_twap_ms
                                        .get(&tick.asset)
                                        .is_some_and(|&t| now.saturating_sub(t) < NATIVE_TWAP_PRECEDENCE_MS);
                                    if native_fresh {
                                        None
                                    } else {
                                        win.twap_60s(tick.trade_time).map(|avg| ChainlinkTick {
                                            asset: tick.asset,
                                            price: avg,
                                            trade_time: tick.trade_time,
                                            source: tick.source,
                                        })
                                    }
                                }
                            };
                            if let Some(tick) = emit {
                                let silence_ms = last_rx.elapsed().as_millis();
                                if saw_tick && silence_ms >= WS_SILENCE_LOG_MS {
                                    tracing::warn!(
                                        target: "chainlink",
                                        "rtds gap silence_ms={silence_ms} asset={}",
                                        tick.asset
                                    );
                                }
                                saw_tick = true;
                                last_rx = Instant::now();
                                host.on_spot(SpotUpdate {
                                    asset: tick.asset,
                                    price: tick.price,
                                    ts_ms: now,
                                    trade_time: Some(tick.trade_time),
                                })
                                .await;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        last_rx = Instant::now();
                        let _ = write.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {
                        last_rx = Instant::now();
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }

    if started.elapsed() > Duration::from_secs(60) {
        SessionEnd::HealthyThenDisconnected
    } else {
        SessionEnd::Error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_polybolt_and_rtds_symbols() {
        assert_eq!(normalize_asset("btcusd").as_deref(), Some("BTC"));
        assert_eq!(normalize_asset("eth/usd").as_deref(), Some("ETH"));
        assert_eq!(normalize_asset("SOL/USD").as_deref(), Some("SOL"));
        assert_eq!(normalize_asset("xrpusdt").as_deref(), Some("XRP"));
        assert_eq!(normalize_asset(""), None);
        assert_eq!(polybolt_symbol("BTC"), "btcusd");
        assert_eq!(rtds_symbol("ETH"), "eth/usd");
    }

    #[test]
    fn parses_polybolt_live_twap_frame_with_exact_decimal() {
        let msg = r#"{"v":1,"channel":"price.crypto.twap","seq":2,"ts":1792143001050,"payload":{"symbol":"btcusd","value":64131.5,"full_accuracy_value":"64131.50001234","timestamp":1792143001000,"source":"chainlink"}}"#;
        let tick = parse_polybolt_frame(msg).expect("live twap frame");
        assert_eq!(tick.asset, "BTC");
        assert_eq!(tick.price, Decimal::from_str("64131.50001234").unwrap());
        assert_eq!(tick.trade_time, 1_792_143_001_000);
        assert_eq!(tick.source.as_deref(), Some("chainlink"));
    }

    #[test]
    fn parses_polybolt_snapshot_picking_latest_point() {
        let msg = r#"{"v":1,"channel":"price.crypto.twap","seq":1,"ts":1792143000120,"snapshot":true,"payload":{"symbol":"ethusd","source":"chainlink","data":[{"timestamp":1792142999000,"value":3100.0,"full_accuracy_value":"3100.10000000"},{"timestamp":1792143000000,"value":3102.25,"full_accuracy_value":"3102.25000000"}]}}"#;
        let tick = parse_polybolt_frame(msg).expect("snapshot twap frame");
        assert_eq!(tick.asset, "ETH");
        assert_eq!(tick.price, Decimal::from_str("3102.25").unwrap());
        assert_eq!(tick.trade_time, 1_792_143_000_000);
    }

    #[test]
    fn ignores_non_twap_polybolt_channels() {
        let msg = r#"{"v":1,"channel":"price.crypto","seq":2,"ts":1792143001050,"payload":{"symbol":"btcusd","full_accuracy_value":"64131.5","timestamp":1792143001000,"source":"chainlink"}}"#;
        assert!(parse_polybolt_frame(msg).is_none());
    }

    #[test]
    fn parses_rtds_twap_e18_and_decimal_and_raw_chainlink() {
        // E18 fixed-point TWAP (64125.50 * 1e18)
        let e18 = r#"{"topic":"crypto_prices_twap_sixty","type":"update","timestamp":1792143000000,"payload":{"symbol":"btc/usd","timestamp":1792143000000,"value":"64125500000000000000000"}}"#;
        match parse_rtds_frame(e18) {
            Some(RtdsTick::Twap60(t)) => {
                assert_eq!(t.asset, "BTC");
                assert_eq!(t.price, Decimal::from_str("64125.5").unwrap());
                assert_eq!(t.trade_time, 1_792_143_000_000);
            }
            other => panic!("expected Twap60, got {other:?}"),
        }

        // Plain decimal raw Chainlink tick
        let raw = r#"{"topic":"crypto_prices_chainlink","type":"update","timestamp":1792143001000,"payload":{"symbol":"sol/usd","timestamp":1792143001000,"value":182.45}}"#;
        match parse_rtds_frame(raw) {
            Some(RtdsTick::RawChainlink(t)) => {
                assert_eq!(t.asset, "SOL");
                assert_eq!(t.price, Decimal::from_str("182.45").unwrap());
            }
            other => panic!("expected RawChainlink, got {other:?}"),
        }
    }

    #[test]
    fn local_twap_window_computes_60s_time_weighted_average() {
        let mut win = TwapWindow::default();
        // t=0: price 100 for 20s, t=20s: price 130 for 40s -> (100*20 + 130*40)/60 = 120
        win.push(1_000_000, Decimal::from(100));
        win.push(1_020_000, Decimal::from(130));
        assert_eq!(win.twap_60s(1_060_000), Some(Decimal::from(120)));

        // At t=120s, the t=0 sample is fully outside [60s, 120s], so average is 130.
        win.push(1_120_000, Decimal::from(130));
        assert_eq!(win.twap_60s(1_120_000), Some(Decimal::from(130)));
    }

    #[test]
    fn builds_polybolt_60s_twap_subscribe_frame() {
        let frame = build_polybolt_twap_subscribe(&["BTC".into(), "ETH".into()]);
        let v: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["op"], "subscribe");
        let subs = v["subscriptions"].as_array().unwrap();
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0]["channel"], "price.crypto.twap");
        assert_eq!(subs[0]["filter"]["symbol"], "btcusd");
        assert_eq!(subs[0]["filter"]["window_seconds"], 60);
    }
}
