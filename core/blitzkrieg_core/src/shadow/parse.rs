//! JSONL parsing for shadow and near-miss records. Moved verbatim from
//! `shadow.rs`.

use super::*;
use rust_decimal::Decimal;

/// Parse one Node-format shadow JSONL line (as written by the TS shadow engine)
/// into a `ShadowRecord`, so Rust walk-forward can run over existing datasets.
pub fn parse_shadow_line(line: &str) -> Option<ShadowRecord> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let entry = dec_from(&v["entryPrice"])?;
    let shares = dec_from(&v["shares"])?;
    let entered_at = v["enteredAt"].as_i64()?;
    let expires_at = v["expiredAt"].as_i64().unwrap_or(entered_at + 900_000);
    let own = parse_path(&v["own"])?;
    if own.is_empty() {
        return None;
    }
    let actual_net_pnl = v
        .get("actual")
        .and_then(|a| a.get("netPnlUsd"))
        .and_then(dec_from);
    Some(ShadowRecord {
        token_id: v["tokenId"].as_str().unwrap_or("").to_string(),
        asset: v["asset"].as_str().unwrap_or("").to_string(),
        direction: v["direction"].as_str().unwrap_or("up").to_string(),
        entry_price: entry,
        shares,
        was_maker_entry: v["wasMakerEntry"].as_bool().unwrap_or(false),
        entered_at_ms: entered_at,
        expires_at_ms: expires_at,
        own,
        opposite: parse_path(&v["opposite"]).unwrap_or_default(),
        actual_net_pnl,
        time_left_at_entry_sec: v
            .get("context")
            .and_then(|c| c.get("timeLeftSec"))
            .and_then(dec_from),
    })
}

fn parse_path(v: &serde_json::Value) -> Option<Vec<Sample>> {
    let samples = v.get("samples")?.as_array()?;
    Some(
        samples
            .iter()
            .filter_map(|s| {
                let t_sec = s.get("t")?.as_f64()?;
                let price = dec_from(&s["p"])?;
                Some(Sample {
                    t_ms: (t_sec * 1000.0) as i64,
                    price,
                    bid: s.get("b").and_then(dec_from),
                    ask: s.get("a").and_then(dec_from),
                })
            })
            .collect(),
    )
}

pub(super) fn dec_from(v: &serde_json::Value) -> Option<Decimal> {
    use std::str::FromStr;
    if let Some(n) = v.as_f64() {
        // Route through the shortest string form to avoid f64 representation
        // noise (0.59 must stay 0.59, not 0.58999999...).
        Decimal::from_str(&format!("{n}")).ok()
    } else if let Some(s) = v.as_str() {
        Decimal::from_str(s).ok()
    } else {
        None
    }
}
