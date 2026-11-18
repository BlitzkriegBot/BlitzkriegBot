//! Near-miss recorder + replay: tracking blocked signals and running exit
//! policy over recorded paths. Moved verbatim from `shadow.rs`.

use super::*;
use crate::exit_policy::{
    BookScalarView, ExitConfig, ExitState, ExitTickInput, decide_exit, executable_bid,
    update_exit_state,
};
use crate::model::OrderbookSnapshot;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::path::Path;

struct NearTracked {
    rec: NearMissRecord,
    last_sample_at: i64,
}

pub struct NearMissRecorder {
    tracked: HashMap<String, NearTracked>,
    sample_min_interval_ms: i64,
    window_ms: i64,
    /// Cap on concurrently tracked tokens (bounded memory).
    max_tracked: usize,
}

impl NearMissRecorder {
    pub fn new(sample_min_interval_ms: i64, window_ms: i64) -> Self {
        Self {
            tracked: HashMap::new(),
            sample_min_interval_ms,
            window_ms,
            max_tracked: 64,
        }
    }

    /// Begin tracking a blocked candidate. Dedup: one record per token at a time
    /// (a token that stays blocked across ticks should not spawn N records).
    #[allow(clippy::too_many_arguments)]
    pub fn on_blocked(
        &mut self,
        token_id: &str,
        asset: &str,
        direction: &str,
        entry_price: Decimal,
        mid_at_block: Decimal,
        reason: NearMissReason,
        time_left_sec: i64,
        round_slot: i64,
        now_ms: i64,
    ) {
        if self.tracked.contains_key(token_id) || self.tracked.len() >= self.max_tracked {
            return;
        }
        self.tracked.insert(
            token_id.to_string(),
            NearTracked {
                rec: NearMissRecord {
                    token_id: token_id.to_string(),
                    asset: asset.to_string(),
                    direction: direction.to_string(),
                    blocked_at_ms: now_ms,
                    entry_price,
                    mid_at_block,
                    reason,
                    time_left_sec,
                    round_slot,
                    path: Vec::new(),
                    window_end_ms: now_ms + self.window_ms,
                },
                last_sample_at: 0,
            },
        );
    }

    pub fn on_book(&mut self, token_id: &str, book: &OrderbookSnapshot, now_ms: i64) {
        let Some(t) = self.tracked.get_mut(token_id) else {
            return;
        };
        if now_ms < t.rec.blocked_at_ms || now_ms > t.rec.window_end_ms {
            return;
        }
        if now_ms - t.last_sample_at < self.sample_min_interval_ms {
            return;
        }
        t.last_sample_at = now_ms;
        t.rec.path.push(Sample {
            t_ms: now_ms - t.rec.blocked_at_ms,
            price: book.mid_price,
            bid: Some(book.best_bid),
            ask: Some(book.best_ask),
        });
    }

    /// Finalize records whose window elapsed; returns them for persistence.
    pub fn tick(&mut self, now_ms: i64) -> Vec<NearMissRecord> {
        let ready: Vec<String> = self
            .tracked
            .iter()
            .filter(|(_, t)| now_ms >= t.rec.window_end_ms)
            .map(|(k, _)| k.clone())
            .collect();
        ready
            .into_iter()
            .filter_map(|k| self.tracked.remove(&k).map(|t| t.rec))
            .collect()
    }

    pub fn pending(&self) -> usize {
        self.tracked.len()
    }

    /// Take all pending records regardless of window (used on shutdown).
    pub fn flush(&mut self) -> Vec<NearMissRecord> {
        let keys: Vec<String> = self.tracked.keys().cloned().collect();
        keys.into_iter()
            .filter_map(|k| self.tracked.remove(&k).map(|t| t.rec))
            .collect()
    }
}

/// Hypothetical replay of a blocked near-miss: treat the blocked bid as an
/// entry and run the shared exit policy over the recorded path.
///
/// IMPORTANT SEMANTICS: the exit policy's own `min_time_left_sec` still applies.
/// A near-miss blocked by the timing gate has `time_left` below that threshold,
/// so replaying it as-is exits almost immediately at the entry price — which
/// models "took the trade but kept the same exit timing". To evaluate *actually*
/// relaxing the gate you must also relax the matching exit-side time gate; the
/// caller can pass a modified `ExitConfig`. This coupling is itself the key
/// finding: relaxing entry timing without relaxing exit timing buys nothing.
///
/// Taker fee on exit is charged conservatively (a maker-first TP can only do
/// better).
pub fn replay_near_miss(rec: &NearMissRecord, cfg: &ExitConfig) -> ReplayResult {
    if rec.path.is_empty() {
        return ReplayResult {
            pnl: Decimal::ZERO,
            exit_reason: crate::model::ExitReason::Manual,
            exit_price: rec.entry_price,
        };
    }
    let entry = rec.entry_price;
    let shares = dec!(10); // nominal 10-share clip, matching the live sizing default
    let mut state = ExitState::new(entry, 0);
    // A resting maker entry would pay no entry fee.
    let entry_fee = Decimal::ZERO;

    let mut exit_price = rec
        .path
        .last()
        .map(|s| s.bid.unwrap_or(s.price))
        .unwrap_or(entry);
    let mut exit_reason = crate::model::ExitReason::ForceExit;

    for s in &rec.path {
        let now = rec.blocked_at_ms + s.t_ms;
        let book = OrderbookSnapshot::from_levels(
            rec.token_id.clone(),
            vec![(s.bid.unwrap_or(s.price), dec!(100))],
            vec![(s.ask.unwrap_or(s.price), dec!(100))],
            now,
        );
        update_exit_state(&mut state, entry, Some(&book), now, cfg);
        // Time left shrinks from the block moment.
        let time_left = rec.time_left_sec - (s.t_ms / 1000);
        let d = decide_exit(ExitTickInput {
            entry_price: entry,
            book: Some(&book as &dyn BookScalarView),
            fallback_price: Some(s.price),
            time_left_sec: time_left,
            hold_sec: s.t_ms / 1000,
            state: &state,
            now_ms: now,
            cfg,
        });
        if let Some(d) = d {
            exit_price = executable_bid(Some(&book));
            exit_reason = d.reason;
            break;
        }
    }

    let exit_fee =
        crate::exit_policy::taker_fee_pct(exit_price) / Decimal::ONE_HUNDRED * exit_price * shares;
    let pnl = shares * (exit_price - entry) - entry_fee - exit_fee;
    ReplayResult {
        pnl,
        exit_reason,
        exit_price,
    }
}

/// Summary of replaying every blocked near-miss: what relaxing the gate would
/// have earned (or cost) over the recorded sample.
#[derive(Debug, Clone, Default)]
pub struct NearMissSummary {
    pub n: usize,
    pub total_pnl: Decimal,
    pub wins: usize,
    pub avg_pnl: Decimal,
    /// Count by block reason.
    pub timing_n: usize,
    pub momentum_n: usize,
    pub total_feeable_notional: Decimal,
}

pub fn evaluate_near_misses(records: &[NearMissRecord], cfg: &ExitConfig) -> NearMissSummary {
    let mut s = NearMissSummary::default();
    for r in records {
        let res = replay_near_miss(r, cfg);
        s.n += 1;
        s.total_pnl += res.pnl;
        if res.pnl > Decimal::ZERO {
            s.wins += 1;
        }
        s.total_feeable_notional += r.entry_price * dec!(10);
        match r.reason {
            NearMissReason::Timing => s.timing_n += 1,
            NearMissReason::Momentum => s.momentum_n += 1,
        }
    }
    if s.n > 0 {
        s.avg_pnl = s.total_pnl / Decimal::from(s.n);
    }
    s
}

/// JSONL line for a near-miss record (includes the path for later replay).
pub fn near_miss_to_json(r: &NearMissRecord) -> serde_json::Value {
    serde_json::json!({
        "tokenId": r.token_id,
        "asset": r.asset,
        "direction": r.direction,
        "blockedAt": r.blocked_at_ms,
        "entryPrice": r.entry_price,
        "midAtBlock": r.mid_at_block,
        "reason": match r.reason { NearMissReason::Timing => "timing", NearMissReason::Momentum => "momentum" },
        "timeLeftSec": r.time_left_sec,
        "roundSlot": r.round_slot,
        "windowSec": (r.window_end_ms - r.blocked_at_ms) as f64 / 1000.0,
        "path": r.path.iter().map(|s| serde_json::json!({ "t": s.t_ms as f64 / 1000.0, "p": s.price, "b": s.bid, "a": s.ask })).collect::<Vec<_>>(),
    })
}

pub fn parse_near_miss_line(line: &str) -> Option<NearMissRecord> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let entry_price = dec_from(&v["entryPrice"])?;
    let blocked_at = v["blockedAt"].as_i64()?;
    let path = v
        .get("path")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    Some(Sample {
                        t_ms: (s.get("t")?.as_f64()? * 1000.0) as i64,
                        price: dec_from(&s["p"])?,
                        bid: s.get("b").and_then(dec_from),
                        ask: s.get("a").and_then(dec_from),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let window_sec = v["windowSec"].as_f64().unwrap_or(0.0);
    let reason = match v["reason"].as_str() {
        Some("momentum") => NearMissReason::Momentum,
        _ => NearMissReason::Timing,
    };
    Some(NearMissRecord {
        token_id: v["tokenId"].as_str().unwrap_or("").to_string(),
        asset: v["asset"].as_str().unwrap_or("").to_string(),
        direction: v["direction"].as_str().unwrap_or("up").to_string(),
        blocked_at_ms: blocked_at,
        entry_price,
        mid_at_block: dec_from(&v["midAtBlock"]).unwrap_or(entry_price),
        reason,
        time_left_sec: v["timeLeftSec"].as_i64().unwrap_or(0),
        round_slot: v["roundSlot"].as_i64().unwrap_or(0),
        path,
        window_end_ms: blocked_at + (window_sec * 1000.0) as i64,
    })
}

pub fn persist_near_misses(path: &Path, records: &[NearMissRecord]) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for r in records {
        writeln!(f, "{}", near_miss_to_json(r))?;
    }
    Ok(())
}
