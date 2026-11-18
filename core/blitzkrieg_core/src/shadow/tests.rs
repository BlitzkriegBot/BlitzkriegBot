//! Shadow tests: replay, walk-forward, holdout, near-miss tracking, parsing.
//! Moved verbatim from `shadow.rs`.

use super::walk_forward::{knowable_train, outcome_known_ms};
use super::*;
use crate::exit_policy::ExitConfig;
use crate::model::OrderbookSnapshot;
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal_macros::dec;

fn rec_with_path(entry: Decimal, prices: &[(i64, f64)]) -> ShadowRecord {
    let own = prices
        .iter()
        .map(|(t, p)| Sample {
            t_ms: *t,
            price: Decimal::from_f64(*p).unwrap(),
            bid: Some(Decimal::from_f64(*p).unwrap()),
            ask: Some(Decimal::from_f64(*p).unwrap()),
        })
        .collect();
    ShadowRecord {
        token_id: "t".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        entry_price: entry,
        shares: dec!(10),
        was_maker_entry: true,
        entered_at_ms: 0,
        expires_at_ms: 900_000,
        own,
        opposite: vec![],
        actual_net_pnl: None,
        time_left_at_entry_sec: None,
    }
}

#[test]
fn replay_takes_profit_on_a_run_up() {
    let r = rec_with_path(
        dec!(0.40),
        &[(0, 0.40), (1000, 0.50), (2000, 1.0), (3000, 0.99)],
    );
    let cfg = ExitConfig::default(); // TP 100%
    let out = replay(&r, &cfg);
    // +150% mid but bid 1.0 → capped by TP at ~100%; fee small → positive.
    assert!(out.pnl > Decimal::ZERO, "expected profit, got {}", out.pnl);
}

#[test]
fn replay_stops_out_on_a_drop() {
    let r = rec_with_path(dec!(0.40), &[(0, 0.40), (1000, 0.20), (2000, 0.10)]);
    let cfg = ExitConfig::default();
    let out = replay(&r, &cfg);
    assert!(out.pnl < Decimal::ZERO);
}

#[test]
fn walk_forward_reports_oos_and_in_sample() {
    // Five records: mostly losers, one big winner. Each entry is spaced
    // past every earlier path's last event (10s apart, paths end at 2s),
    // so every earlier outcome is knowable when the later OOS checks run
    // (the F9 knowability filter).
    let recs: Vec<ShadowRecord> = vec![
        rec_with_path(dec!(0.40), &[(0, 0.40), (1000, 0.30), (2000, 0.20)]),
        rec_with_path(dec!(0.40), &[(0, 0.40), (1000, 0.35), (2000, 0.25)]),
        rec_with_path(dec!(0.40), &[(0, 0.40), (1000, 0.60), (2000, 1.0)]),
        rec_with_path(dec!(0.40), &[(0, 0.40), (1000, 0.38), (2000, 0.30)]),
        rec_with_path(dec!(0.40), &[(0, 0.40), (1000, 0.50), (2000, 0.45)]),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, mut r)| {
        r.entered_at_ms = i as i64 * 10_000;
        r
    })
    .collect();
    let out = walk_forward(&recs, &ExitConfig::default(), &default_grid(), 3);
    assert_eq!(out.in_sample.len(), default_grid().len());
    assert_eq!(out.oos_count, 2);
    // The in-sample list is named and ordered by the grid.
    assert_eq!(out.in_sample[0].0, default_grid()[0].name);
}

/// F9: a path entered EARLIER but resolved LATER must not train a
/// validation trade that happened in between. A entered at 0s and only
/// exits at 180s; B enters at 60s. At B's entry A's result is still in
/// the future, so B gets no honest training slice and is not scored;
/// once the validation window advances past 180s, A becomes training
/// material again.
#[test]
fn walk_forward_does_not_train_on_outcomes_from_the_future() {
    let mut a = rec_with_path(dec!(0.40), &[(0, 0.40), (180_000, 1.0)]);
    a.entered_at_ms = 0;
    a.token_id = "a".into();
    // B's own path ends at 150s (entered 60s + 90s of samples).
    let mut b = rec_with_path(dec!(0.40), &[(0, 0.40), (90_000, 0.50)]);
    b.entered_at_ms = 60_000;
    b.token_id = "b".into();
    let mut c = rec_with_path(dec!(0.40), &[(0, 0.40), (10_000, 0.45)]);
    c.entered_at_ms = 200_000;
    c.token_id = "c".into();
    let recs = vec![a, b, c]; // already entry-ordered

    assert_eq!(outcome_known_ms(&recs[0]), 180_000);
    // B's validation entry at 60s: A's outcome (180s) is not knowable yet
    // → the training slice must not contain A — here it is empty entirely.
    let train_b = knowable_train(&recs, 1);
    assert!(
        train_b.iter().all(|r| r.token_id != "a"),
        "A's 180s result leaked into B's 60s training slice"
    );
    // And with nothing knowable at all, walk_forward must not invent a
    // choice: B is skipped instead of scored on fabricated params.
    let out = walk_forward(&recs, &ExitConfig::default(), &default_grid(), 1);
    assert_eq!(out.oos_count, 1, "only C is scored out-of-sample");

    // The window advanced past A's resolution (180s) and B's (150s):
    // both are now knowable and DO train C's 200s validation entry.
    let train_c = knowable_train(&recs, 2);
    assert_eq!(train_c.len(), 2);
    assert!(train_c.iter().any(|r| r.token_id == "a"));
    assert!(train_c.iter().any(|r| r.token_id == "b"));
}

#[test]
fn frozen_holdout_picks_on_train_and_scores_test_once() {
    let recs: Vec<ShadowRecord> = (0..6)
        .map(|i| {
            // Alternating loser/winner paths, time-ordered via entered_at.
            let end = if i % 2 == 0 { 0.30 } else { 0.62 };
            let mut r = rec_with_path(dec!(0.40), &[(0, 0.40), (1000, end)]);
            r.entered_at_ms = i as i64 * 1000;
            r
        })
        .collect();
    let grid = default_grid();
    let out = frozen_holdout(&recs, &ExitConfig::default(), &grid, 0.5);
    assert_eq!(out.train_n, 3);
    assert_eq!(out.test_n, 3);
    assert_eq!(out.rows.len(), grid.len());
    // The reported frozen score must equal the train-best row's test score.
    let best = out.rows.iter().find(|r| r.name == out.train_best).unwrap();
    assert_eq!(out.frozen_test_pnl, best.test_pnl);
    // The shipped cell is present and scored (regression: it used to be absent).
    assert!(out.rows.iter().any(|r| r.name == "shipped-SL12/trail8"));
}

#[test]
fn parses_node_format_line() {
    let line = r#"{"id":"hft-1","strategy":"spread_arb","asset":"ETH","direction":"down","tokenId":"t1","entryPrice":0.45,"shares":10,"wasMakerEntry":true,"enteredAt":1000,"expiredAt":901000,"own":{"entryRef":0.45,"samples":[{"t":0,"p":0.45},{"t":5,"p":0.6,"b":0.59,"a":0.61}],"maxPrice":0.6,"minPrice":0.45},"oppositeTokenId":null,"opposite":null,"closed":true,"actual":{"exitReason":"take_profit","netPnlUsd":1.5}}"#;
    let r = parse_shadow_line(line).unwrap();
    assert_eq!(r.asset, "ETH");
    assert_eq!(r.direction, "down");
    assert_eq!(r.shares, dec!(10));
    assert_eq!(r.own.len(), 2);
    assert_eq!(r.own[1].bid, Some(dec!(0.59)));
    assert_eq!(r.actual_net_pnl, Some(dec!(1.5)));
    // And it replays without panicking.
    let _ = replay(&r, &ExitConfig::default());
}

#[test]
fn near_miss_records_path_and_replays() {
    let mut rec = NearMissRecorder::new(0, 1000);
    // time_left must be > min_time_left(180) for a relaxed gate to be able to
    // hold the trade; a block recorded below that is a pure timing exit.
    rec.on_blocked(
        "tok",
        "BTC",
        "up",
        dec!(0.43),
        dec!(0.44),
        NearMissReason::Timing,
        190,
        7,
        0,
    );
    // Dedup: a second block for the same token does not spawn a new record.
    rec.on_blocked(
        "tok",
        "BTC",
        "up",
        dec!(0.43),
        dec!(0.44),
        NearMissReason::Timing,
        190,
        7,
        100,
    );
    assert_eq!(rec.pending(), 1);

    let up = OrderbookSnapshot::from_levels(
        "tok",
        vec![(dec!(0.43), dec!(1))],
        vec![(dec!(0.45), dec!(1))],
        100,
    );
    rec.on_book("tok", &up, 100);
    let big = OrderbookSnapshot::from_levels(
        "tok",
        vec![(dec!(0.90), dec!(1))],
        vec![(dec!(0.92), dec!(1))],
        500,
    );
    rec.on_book("tok", &big, 500);
    let done = rec.tick(2000);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].path.len(), 2);

    // Hypothetical replay from the blocked bid: a run-up should profit.
    let res = replay_near_miss(&done[0], &ExitConfig::default());
    assert!(
        res.pnl > Decimal::ZERO,
        "run-up near-miss should replay positive, got {}",
        res.pnl
    );
}

#[test]
fn near_miss_summary_counts_reasons_and_pnl() {
    let mk = |token: &str, reason: NearMissReason, up: bool| NearMissRecord {
        token_id: token.into(),
        asset: "BTC".into(),
        direction: "up".into(),
        blocked_at_ms: 0,
        entry_price: dec!(0.43),
        mid_at_block: dec!(0.44),
        reason,
        time_left_sec: 190,
        round_slot: 7,
        path: if up {
            vec![
                Sample {
                    t_ms: 100,
                    price: dec!(0.43),
                    bid: Some(dec!(0.43)),
                    ask: Some(dec!(0.45)),
                },
                Sample {
                    t_ms: 500,
                    price: dec!(0.95),
                    bid: Some(dec!(0.95)),
                    ask: Some(dec!(0.97)),
                },
            ]
        } else {
            vec![
                Sample {
                    t_ms: 100,
                    price: dec!(0.43),
                    bid: Some(dec!(0.43)),
                    ask: Some(dec!(0.45)),
                },
                Sample {
                    t_ms: 500,
                    price: dec!(0.10),
                    bid: Some(dec!(0.10)),
                    ask: Some(dec!(0.12)),
                },
            ]
        },
        window_end_ms: 1000,
    };
    let recs = vec![
        mk("a", NearMissReason::Timing, true),
        mk("b", NearMissReason::Momentum, false),
    ];
    let s = evaluate_near_misses(&recs, &ExitConfig::default());
    assert_eq!(s.n, 2);
    assert_eq!(s.timing_n, 1);
    assert_eq!(s.momentum_n, 1);
    assert_eq!(s.wins, 1);
    // One winner (~+$5) + one loser (~-$3.3) → net positive here; the point is
    // the summary is populated and reason-bucketed.
    assert!(s.total_pnl != Decimal::ZERO);
}

#[test]
fn near_miss_json_round_trips() {
    let mut rec = NearMissRecorder::new(0, 1000);
    rec.on_blocked(
        "tok",
        "BTC",
        "up",
        dec!(0.43),
        dec!(0.44),
        NearMissReason::Timing,
        150,
        7,
        0,
    );
    let b = OrderbookSnapshot::from_levels(
        "tok",
        vec![(dec!(0.43), dec!(1))],
        vec![(dec!(0.45), dec!(1))],
        100,
    );
    rec.on_book("tok", &b, 100);
    let done = rec.tick(2000);
    let json = near_miss_to_json(&done[0]);
    let line = serde_json::to_string(&json).unwrap();
    let back = parse_near_miss_line(&line).unwrap();
    assert_eq!(back.token_id, "tok");
    assert_eq!(back.reason, NearMissReason::Timing);
    assert_eq!(back.entry_price, dec!(0.43));
    assert_eq!(back.path.len(), 1);
}
