//! Smart Money Reverse-to-Strategy engine (Phase 1 skeleton):
//!
//! 1. [`analyze_fills`] — extract a deterministic [`WalletFingerprint`] from a
//!    wallet's normalized fill samples (entry-price quantiles, time-left window,
//!    pair ratio, hold-to-settlement ratio, realized win rate / PnL / PF).
//! 2. [`classify_archetype`] — classify the wallet into an actionable strategy
//!    archetype (`LateFavorite`, `PairArb`, `MidRoundMomentum`,
//!    `CheapOptionHunter`, or fail-closed `Unclassifiable`).
//! 3. [`synthesize_blueprint`] — emit a valid v1 Blueprint JSON document (only
//!    using whitelisted fields & operators) that compiles directly through
//!    `blitzkrieg_core::blueprint::compile_with_tunables` into sandboxed Lua
//!    with evolvable `tunables`.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Minimum buy fills required before reverse-engineering is attempted.
/// Below this floor the sample is too noisy and fails closed (`Unclassifiable`).
pub const MIN_BUY_FILLS: usize = 20;

/// One normalized wallet fill aligned with its round's market metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FillSample {
    pub condition_id: String,
    pub asset: String,
    pub duration_sec: i64,
    /// Round close timestamp (epoch seconds).
    pub end_ts_sec: i64,
    /// Fill execution timestamp (epoch seconds).
    pub fill_ts_sec: i64,
    /// `"BUY"` or `"SELL"`.
    pub side: String,
    /// `0` = Up, `1` = Down.
    pub outcome_index: i64,
    /// Execution price in `(0, 1)`.
    pub price: f64,
    /// Fill notional in USD (`price * shares`).
    pub size_usd: f64,
    /// Optional resolved winner (`Some(0)` = Up, `Some(1)` = Down).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winning_outcome: Option<i64>,
}

/// Deterministic behavioral fingerprint of a Polymarket wallet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WalletFingerprint {
    pub wallet: String,
    pub total_fills: usize,
    pub buy_fills: usize,
    pub sell_fills: usize,
    pub unique_conditions: usize,
    pub assets: Vec<String>,
    pub durations_sec: Vec<i64>,
    pub entry_px_min: f64,
    pub entry_px_p10: f64,
    pub entry_px_p25: f64,
    pub entry_px_p50: f64,
    pub entry_px_p75: f64,
    pub entry_px_p90: f64,
    pub entry_px_max: f64,
    pub time_left_p10: i64,
    pub time_left_p50: i64,
    pub time_left_p90: i64,
    /// Fraction of bought conditions where the wallet bought BOTH outcomes (0 & 1).
    pub pair_ratio: f64,
    /// Fraction of bought conditions with zero pre-expiry SELL fills.
    pub hold_to_settlement_ratio: f64,
    pub avg_notional_usd: f64,
    pub resolved_rounds: usize,
    pub wins: usize,
    pub win_rate_pct: f64,
    pub net_pnl_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profit_factor: Option<f64>,
}

/// Strategy skeleton inferred from a wallet's behavioral fingerprint.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StrategyArchetype {
    LateFavorite,
    PairArb,
    MidRoundMomentum,
    CheapOptionHunter,
    Unclassifiable,
}

/// Full output of reverse-engineering one wallet.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReverseAnalysis {
    pub fingerprint: WalletFingerprint,
    pub archetype: StrategyArchetype,
    pub reason: String,
    /// Synthesized v1 Blueprint JSON when a single-sided rule can be safely
    /// compiled; `None` for `Unclassifiable` (fail-closed) or `PairArb`
    /// (requires two-leg pair template rather than single-sided Blueprint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blueprint: Option<serde_json::Value>,
}

fn quantile_f64(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn quantile_i64(sorted: &[i64], q: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Compute a deterministic [`WalletFingerprint`], classify its
/// [`StrategyArchetype`], and synthesize a v1 Blueprint JSON when classifiable.
pub fn analyze_fills(wallet: &str, samples: &[FillSample]) -> ReverseAnalysis {
    let mut buy_prices: Vec<f64> = Vec::new();
    let mut time_lefts: Vec<i64> = Vec::new();
    let mut assets: BTreeSet<String> = BTreeSet::new();
    let mut durations: BTreeSet<i64> = BTreeSet::new();
    let mut buy_outcomes_by_cond: BTreeMap<&str, BTreeSet<i64>> = BTreeMap::new();
    let mut sold_conds: BTreeSet<&str> = BTreeSet::new();
    let mut all_conds: BTreeSet<&str> = BTreeSet::new();
    let mut buy_notional_sum = 0.0f64;
    let mut sell_fills = 0usize;

    // Track per-condition realized settlement PnL (simple buy-to-settlement view).
    let mut resolved_rounds = 0usize;
    let mut wins = 0usize;
    let mut gross_profit = 0.0f64;
    let mut gross_loss = 0.0f64;
    let mut cond_pnl: BTreeMap<&str, (f64, bool)> = BTreeMap::new();

    for f in samples {
        all_conds.insert(f.condition_id.as_str());
        if !f.asset.is_empty() {
            assets.insert(f.asset.to_uppercase());
        }
        if f.duration_sec > 0 {
            durations.insert(f.duration_sec);
        }
        if f.side.eq_ignore_ascii_case("SELL") {
            sell_fills += 1;
            if f.fill_ts_sec < f.end_ts_sec {
                sold_conds.insert(f.condition_id.as_str());
            }
            continue;
        }
        if f.price > 0.0 && f.price < 1.0 {
            buy_prices.push(f.price);
            buy_notional_sum += f.size_usd.max(0.0);
            let tl = (f.end_ts_sec - f.fill_ts_sec).max(0);
            if f.end_ts_sec > 0 && f.fill_ts_sec > 0 && tl <= f.duration_sec.max(86_400) {
                time_lefts.push(tl);
            }
            buy_outcomes_by_cond
                .entry(f.condition_id.as_str())
                .or_default()
                .insert(f.outcome_index);
            if let Some(winner) = f.winning_outcome {
                let shares = if f.price > 0.0 {
                    f.size_usd / f.price
                } else {
                    0.0
                };
                let payout = if winner == f.outcome_index { shares } else { 0.0 };
                let pnl = payout - f.size_usd;
                let entry = cond_pnl.entry(f.condition_id.as_str()).or_insert((0.0, true));
                entry.0 += pnl;
            }
        }
    }

    for (pnl, _) in cond_pnl.values() {
        resolved_rounds += 1;
        if *pnl >= 0.0 {
            wins += 1;
            gross_profit += *pnl;
        } else {
            gross_loss += pnl.abs();
        }
    }

    buy_prices.sort_by(|a, b| a.total_cmp(b));
    time_lefts.sort_unstable();

    let buy_fills = buy_prices.len();
    let bought_cond_count = buy_outcomes_by_cond.len();
    let paired_conds = buy_outcomes_by_cond
        .values()
        .filter(|outs| outs.len() >= 2)
        .count();
    let held_conds = buy_outcomes_by_cond
        .keys()
        .filter(|cid| !sold_conds.contains(**cid))
        .count();

    let pair_ratio = if bought_cond_count > 0 {
        round4(paired_conds as f64 / bought_cond_count as f64)
    } else {
        0.0
    };
    let hold_to_settlement_ratio = if bought_cond_count > 0 {
        round4(held_conds as f64 / bought_cond_count as f64)
    } else {
        0.0
    };
    let avg_notional_usd = if buy_fills > 0 {
        round2(buy_notional_sum / buy_fills as f64)
    } else {
        0.0
    };
    let win_rate_pct = if resolved_rounds > 0 {
        round2(wins as f64 / resolved_rounds as f64 * 100.0)
    } else {
        0.0
    };
    let net_pnl_usd = round2(gross_profit - gross_loss);
    let profit_factor = if gross_loss > 0.0 {
        Some(round2(gross_profit / gross_loss))
    } else if gross_profit > 0.0 {
        None
    } else {
        Some(0.0)
    };

    let fp = WalletFingerprint {
        wallet: wallet.trim().to_string(),
        total_fills: samples.len(),
        buy_fills,
        sell_fills,
        unique_conditions: all_conds.len(),
        assets: assets.into_iter().collect(),
        durations_sec: durations.into_iter().collect(),
        entry_px_min: round4(buy_prices.first().copied().unwrap_or(0.0)),
        entry_px_p10: round4(quantile_f64(&buy_prices, 0.10)),
        entry_px_p25: round4(quantile_f64(&buy_prices, 0.25)),
        entry_px_p50: round4(quantile_f64(&buy_prices, 0.50)),
        entry_px_p75: round4(quantile_f64(&buy_prices, 0.75)),
        entry_px_p90: round4(quantile_f64(&buy_prices, 0.90)),
        entry_px_max: round4(buy_prices.last().copied().unwrap_or(0.0)),
        time_left_p10: quantile_i64(&time_lefts, 0.10),
        time_left_p50: quantile_i64(&time_lefts, 0.50),
        time_left_p90: quantile_i64(&time_lefts, 0.90),
        pair_ratio,
        hold_to_settlement_ratio,
        avg_notional_usd,
        resolved_rounds,
        wins,
        win_rate_pct,
        net_pnl_usd,
        profit_factor,
    };

    let (archetype, reason) = classify_archetype(&fp);
    let blueprint = synthesize_blueprint(wallet, &fp, archetype);
    ReverseAnalysis {
        fingerprint: fp,
        archetype,
        reason,
        blueprint,
    }
}

/// Deterministic archetype classifier (fail-closed on small/noisy samples).
pub fn classify_archetype(fp: &WalletFingerprint) -> (StrategyArchetype, String) {
    if fp.buy_fills < MIN_BUY_FILLS {
        return (
            StrategyArchetype::Unclassifiable,
            format!(
                "样本不足（仅 {} 笔买入，需 ≥ {} 笔），拒绝逆向以防过拟合",
                fp.buy_fills, MIN_BUY_FILLS
            ),
        );
    }
    if fp.time_left_p90 <= 0 {
        return (
            StrategyArchetype::Unclassifiable,
            "缺少有效的轮次剩余时间样本，无法推断入场窗口".to_string(),
        );
    }
    if fp.pair_ratio >= 0.40 {
        return (
            StrategyArchetype::PairArb,
            format!(
                "双边配对占比 {:.1}%（≥ 40%），属于同轮双腿套利骨架，建议走 pair_discount_arb / maker_pair 模板",
                fp.pair_ratio * 100.0
            ),
        );
    }
    if fp.entry_px_p50 >= 0.80 && fp.time_left_p50 <= 120 && fp.hold_to_settlement_ratio >= 0.80 {
        return (
            StrategyArchetype::LateFavorite,
            format!(
                "尾盘顺势持有到期：入场中位价 {:.2}、剩余时间中位数 {}s、持有到期率 {:.1}%",
                fp.entry_px_p50,
                fp.time_left_p50,
                fp.hold_to_settlement_ratio * 100.0
            ),
        );
    }
    if (0.55..0.85).contains(&fp.entry_px_p50) && fp.time_left_p50 > 60 {
        return (
            StrategyArchetype::MidRoundMomentum,
            format!(
                "中段动量顺势：入场中位价 {:.2}、剩余时间中位数 {}s",
                fp.entry_px_p50, fp.time_left_p50
            ),
        );
    }
    if fp.entry_px_p50 <= 0.45 {
        return (
            StrategyArchetype::CheapOptionHunter,
            format!(
                "低价非对称筹码：入场中位价 {:.2}（≤ 0.45）",
                fp.entry_px_p50
            ),
        );
    }
    (
        StrategyArchetype::Unclassifiable,
        format!(
            "入场分布较离散（中位价 {:.2}、剩余时间 {}s），暂不自动合成规则",
            fp.entry_px_p50, fp.time_left_p50
        ),
    )
}

/// Synthesize a valid v1 Blueprint JSON document for single-sided classifiable
/// archetypes. Returns `None` for `Unclassifiable` and `PairArb` (fail-closed).
pub fn synthesize_blueprint(
    wallet: &str,
    fp: &WalletFingerprint,
    archetype: StrategyArchetype,
) -> Option<serde_json::Value> {
    if matches!(
        archetype,
        StrategyArchetype::Unclassifiable | StrategyArchetype::PairArb
    ) {
        return None;
    }

    let min_px = round4(fp.entry_px_p25.clamp(0.02, 0.97));
    let max_px = round4(fp.entry_px_p90.max(min_px + 0.01).clamp(0.03, 0.99));
    let min_tl = fp.time_left_p10.max(5);
    let max_tl = fp.time_left_p90.max(min_tl + 5);

    // Safe alphanumeric blueprint name (never carries forbidden substrings).
    let short_wallet: String = wallet
        .trim()
        .trim_start_matches("0x")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    let tag = match archetype {
        StrategyArchetype::LateFavorite => "late_fav",
        StrategyArchetype::MidRoundMomentum => "mid_mom",
        StrategyArchetype::CheapOptionHunter => "cheap_opt",
        StrategyArchetype::PairArb | StrategyArchetype::Unclassifiable => unreachable!(),
    };
    let name = if short_wallet.is_empty() {
        format!("sm_{tag}")
    } else {
        format!("sm_{tag}_{short_wallet}")
    };

    Some(serde_json::json!({
        "version": 1,
        "name": name,
        "nodes": [
            { "id": "c_px_min", "type": "condition", "params": { "field": "tick.price", "op": ">=", "value": min_px } },
            { "id": "c_px_max", "type": "condition", "params": { "field": "tick.price", "op": "<=", "value": max_px } },
            { "id": "c_tl_min", "type": "condition", "params": { "field": "tick.time_left_sec", "op": ">=", "value": min_tl } },
            { "id": "c_tl_max", "type": "condition", "params": { "field": "tick.time_left_sec", "op": "<=", "value": max_tl } },
            { "id": "g_px", "type": "logic_and", "params": {} },
            { "id": "g_tl", "type": "logic_and", "params": {} },
            { "id": "g_all", "type": "logic_and", "params": {} },
            { "id": "a_buy", "type": "action_buy", "params": { "price": max_px } }
        ],
        "edges": [
            { "from": "c_px_min", "to": "g_px" },
            { "from": "c_px_max", "to": "g_px" },
            { "from": "c_tl_min", "to": "g_tl" },
            { "from": "c_tl_max", "to": "g_tl" },
            { "from": "g_px", "to": "g_all" },
            { "from": "g_tl", "to": "g_all" },
            { "from": "g_all", "to": "a_buy" }
        ]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_fill(cond: usize, side: &str, outcome: i64, price: f64, tl: i64, winner: Option<i64>) -> FillSample {
        let end = 1_795_670_000 + (cond as i64) * 300;
        FillSample {
            condition_id: format!("0xcond{cond}"),
            asset: "BTC".into(),
            duration_sec: 300,
            end_ts_sec: end,
            fill_ts_sec: end - tl,
            side: side.into(),
            outcome_index: outcome,
            price,
            size_usd: 10.0,
            winning_outcome: winner,
        }
    }

    #[test]
    fn too_few_fills_fails_closed_as_unclassifiable() {
        let samples: Vec<FillSample> = (0..10)
            .map(|i| mk_fill(i, "BUY", 0, 0.92, 45, Some(0)))
            .collect();
        let out = analyze_fills("0xabc12345", &samples);
        assert_eq!(out.archetype, StrategyArchetype::Unclassifiable);
        assert!(out.blueprint.is_none());
    }

    #[test]
    fn late_favorite_wallet_synthesizes_valid_blueprint() {
        let samples: Vec<FillSample> = (0..30)
            .map(|i| mk_fill(i, "BUY", 0, 0.88 + (i % 8) as f64 * 0.01, 20 + (i % 5) as i64 * 10, Some(0)))
            .collect();
        let out = analyze_fills("0xdeadbeef0011", &samples);
        assert_eq!(out.archetype, StrategyArchetype::LateFavorite);
        assert_eq!(out.fingerprint.win_rate_pct, 100.0);
        let bp = out.blueprint.expect("blueprint synthesized");
        assert_eq!(bp["version"], 1);
        assert_eq!(bp["name"], "sm_late_fav_deadbeef");
        assert_eq!(bp["nodes"].as_array().unwrap().len(), 8);
    }

    #[test]
    fn pair_arb_wallet_is_detected_and_not_forced_into_single_sided_blueprint() {
        let mut samples = Vec::new();
        for i in 0..25 {
            samples.push(mk_fill(i, "BUY", 0, 0.48, 120, Some(0)));
            samples.push(mk_fill(i, "BUY", 1, 0.48, 118, Some(0)));
        }
        let out = analyze_fills("0xpair0001", &samples);
        assert_eq!(out.archetype, StrategyArchetype::PairArb);
        assert!(out.fingerprint.pair_ratio >= 0.99);
        assert!(out.blueprint.is_none());
    }
}
