//! Walk-forward evaluation: expanding-window parameter selection over recorded
//! shadow paths. Moved verbatim from `shadow.rs`.

use super::*;
use crate::exit_policy::ExitConfig;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// One candidate exit configuration in the walk-forward grid.
#[derive(Debug, Clone)]
pub struct GridPoint {
    pub name: String,
    pub stop_loss_pct: Decimal,
    pub min_trail_pct: Decimal,
    pub trailing_min_high_pct: Decimal,
}

fn cfg_for(base: &ExitConfig, g: &GridPoint) -> ExitConfig {
    let mut c = base.clone();
    c.stop_loss_pct = g.stop_loss_pct;
    c.min_trail_pct = g.min_trail_pct;
    c.trailing_min_high_pct = g.trailing_min_high_pct;
    c
}

/// Default grid for the exit walk-forward study. The first point is the OLD
/// shipped config (SL50, the wide stop the pre-tuning binary ran); the second is
/// the ACTUAL current shipped default (`ExitConfig::default`: SL12 + trail8, see
/// exit_policy.rs). The remaining points probe the neighbouring stop/trail space.
///
/// `shipped` MUST stay in sync with `ExitConfig::default`: an earlier grid
/// mislabelled an SL15/trail10 point as "shipped" and omitted the real SL12/trail8
/// cell, so the offline study never scored what production actually trades and a
/// stale wide-stop binary went unnoticed.
pub fn default_grid() -> Vec<GridPoint> {
    vec![
        GridPoint {
            name: "old-SL50/trail10".into(),
            stop_loss_pct: dec!(50),
            min_trail_pct: dec!(10),
            trailing_min_high_pct: dec!(15),
        },
        GridPoint {
            name: "shipped-SL12/trail8".into(),
            stop_loss_pct: dec!(12),
            min_trail_pct: dec!(8),
            trailing_min_high_pct: dec!(15),
        },
        GridPoint {
            name: "SL12/trail10".into(),
            stop_loss_pct: dec!(12),
            min_trail_pct: dec!(10),
            trailing_min_high_pct: dec!(15),
        },
        GridPoint {
            name: "SL15/trail8".into(),
            stop_loss_pct: dec!(15),
            min_trail_pct: dec!(8),
            trailing_min_high_pct: dec!(15),
        },
        GridPoint {
            name: "SL10/trail8".into(),
            stop_loss_pct: dec!(10),
            min_trail_pct: dec!(8),
            trailing_min_high_pct: dec!(15),
        },
        GridPoint {
            name: "SL20/trail10".into(),
            stop_loss_pct: dec!(20),
            min_trail_pct: dec!(10),
            trailing_min_high_pct: dec!(15),
        },
    ]
}

/// One cell's train/test score in the frozen holdout study.
#[derive(Debug, Clone)]
pub struct HoldoutRow {
    pub name: String,
    pub train_pnl: Decimal,
    pub train_wins: usize,
    pub test_pnl: Decimal,
    pub test_wins: usize,
}

/// Result of choosing an exit cell ONCE on the early window and applying it
/// frozen (no per-record re-optimization) to the later window — a stricter
/// overfit check than the adaptive expanding-window `walk_forward`.
#[derive(Debug, Clone)]
pub struct HoldoutResult {
    pub rows: Vec<HoldoutRow>,
    /// Cell with the highest training PnL.
    pub train_best: String,
    /// That cell's PnL over the unseen test window.
    pub frozen_test_pnl: Decimal,
    pub train_n: usize,
    pub test_n: usize,
}

/// Strict chronological holdout. `records` MUST be time-ordered (ascending); the
/// first `train_frac` fraction is the training window, the rest the test window.
pub fn frozen_holdout(
    records: &[ShadowRecord],
    base: &ExitConfig,
    grid: &[GridPoint],
    train_frac: f64,
) -> HoldoutResult {
    let cut = ((records.len() as f64) * train_frac).round() as usize;
    let cut = cut.clamp(1, records.len().saturating_sub(1));
    let (train, test) = records.split_at(cut);

    let mut rows = Vec::with_capacity(grid.len());
    for g in grid {
        let cfg = cfg_for(base, g);
        let mut train_pnl = Decimal::ZERO;
        let mut train_wins = 0usize;
        for r in train {
            let p = replay(r, &cfg).pnl;
            train_pnl += p;
            if p > Decimal::ZERO {
                train_wins += 1;
            }
        }
        let mut test_pnl = Decimal::ZERO;
        let mut test_wins = 0usize;
        for r in test {
            let p = replay(r, &cfg).pnl;
            test_pnl += p;
            if p > Decimal::ZERO {
                test_wins += 1;
            }
        }
        rows.push(HoldoutRow {
            name: g.name.clone(),
            train_pnl,
            train_wins,
            test_pnl,
            test_wins,
        });
    }

    let best_idx = rows
        .iter()
        .enumerate()
        .max_by_key(|(_, row)| row.train_pnl)
        .map(|(i, _)| i)
        .unwrap_or(0);
    HoldoutResult {
        train_best: rows[best_idx].name.clone(),
        frozen_test_pnl: rows[best_idx].test_pnl,
        rows,
        train_n: train.len(),
        test_n: test.len(),
    }
}

#[derive(Debug, Clone)]
pub struct WalkForwardResult {
    /// PnL of each grid point fit on all records (optimistic upper bound).
    pub in_sample: Vec<(String, Decimal)>,
    /// Records tested out-of-sample.
    pub oos_count: usize,
    /// Expanding-window walk-forward PnL (params chosen on the past, applied once).
    pub oos_pnl: Decimal,
    pub min_train: usize,
}

/// Expanding-window walk-forward: pick the best grid point on the past, apply
/// it once to record k. Guards against curve-fitting a handful of paths.
///
/// F9: "the past" is not `records[..k]`. A training path may only vote when
/// its OUTCOME was already knowable at the validation trade's entry time —
/// its path's last event happened at or before that entry. Selection by
/// entry order alone leaked outcomes that happened after the validation
/// entry (a path entered earlier but resolved later), so training slices are
/// filtered by knowable-result time before any grid scoring.
pub fn walk_forward(
    records: &[ShadowRecord],
    base: &ExitConfig,
    grid: &[GridPoint],
    min_train: usize,
) -> WalkForwardResult {
    let in_sample: Vec<(String, Decimal)> = grid
        .iter()
        .map(|g| {
            let total = records
                .iter()
                .map(|r| replay(r, &cfg_for(base, g)).pnl)
                .sum();
            (g.name.clone(), total)
        })
        .collect();

    let mut oos_pnl = Decimal::ZERO;
    let mut oos_count = 0usize;
    if records.len() > min_train {
        for k in min_train..records.len() {
            let train = knowable_train(records, k);
            if train.len() < min_train {
                continue; // not enough KNOWN history to choose params honestly
            }
            let best = grid
                .iter()
                .max_by_key(|g| {
                    train
                        .iter()
                        .map(|r| replay(r, &cfg_for(base, g)).pnl)
                        .sum::<Decimal>()
                })
                .or_else(|| grid.first());
            let Some(best) = best else { continue };
            oos_pnl += replay(&records[k], &cfg_for(base, best)).pnl;
            oos_count += 1;
        }
    }
    WalkForwardResult {
        in_sample,
        oos_count,
        oos_pnl,
        min_train,
    }
}

/// The moment a record's OUTCOME became knowable. A `ShadowRecord` carries no
/// explicit close time, so the bound is its own path's LAST event: entry time
/// plus the newest sample's offset (an unclosed path is only knowable at
/// entry, and contributes nothing to training anyway).
pub(super) fn outcome_known_ms(rec: &ShadowRecord) -> i64 {
    rec.entered_at_ms + rec.own.last().map(|s| s.t_ms).unwrap_or(0)
}

/// The training candidates for validating `records[idx]`: earlier records
/// whose result was already knowable when this validation trade was entered
/// (F9). Overlapping paths — an earlier ENTRY whose result lands after the
/// validation entry — are excluded; they are the future leaking backwards.
pub(super) fn knowable_train(records: &[ShadowRecord], idx: usize) -> Vec<&ShadowRecord> {
    let Some(t_k) = records.get(idx).map(|r| r.entered_at_ms) else {
        return Vec::new();
    };
    records[..idx]
        .iter()
        .filter(|r| outcome_known_ms(r) <= t_k)
        .collect()
}

/// Read a shadow JSONL file and run walk-forward over its records.
pub fn walk_forward_file(
    path: &std::path::Path,
    base: &ExitConfig,
    grid: &[GridPoint],
    min_train: usize,
) -> std::io::Result<WalkForwardResult> {
    let text = std::fs::read_to_string(path)?;
    let mut records: Vec<ShadowRecord> = text.lines().filter_map(parse_shadow_line).collect();
    records.sort_by_key(|r| r.entered_at_ms);
    Ok(walk_forward(&records, base, grid, min_train))
}
