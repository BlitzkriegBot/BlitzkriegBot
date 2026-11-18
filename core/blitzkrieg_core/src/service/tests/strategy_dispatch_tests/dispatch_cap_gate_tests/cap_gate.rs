//! Per-strategy caps: position cap, notional cap, portfolio notional cap,
//! the #1258 clamp-before-Gate-2 move, and the same-cycle `cycle_placed`
//! backstop (#966 / #967). Moved verbatim from `dispatch_cap_gate_tests.rs`
//! (pure code motion).

use super::super::*;
use crate::risk::limits::{Bound, LimitSource};

#[test]
fn default_config_has_no_limits_and_places_the_entry() {
    let mut c = core_with_engine(HashMap::new());
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        1,
        "no caps configured must not change behaviour"
    );
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 0);

    // Fill the resting maker entry so exposure is live, then read the ledger.
    // The tuned 0.88 discount rests the entry bid at round2(0.44*0.88)
    // = 0.39, so the crossing ask is 0.39 (was 0.42 under the 0.98 factor).
    c.book_snapshot(
        "up",
        vec![(dec!(0.38), dec!(100))],
        vec![(dec!(0.39), dec!(100))],
        now + 13_000,
    );
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(s["enabled"], true);
    // The kernel has no `builtin` class any more (PR-B): the test adapter is
    // hosted by the test itself, and a production session would report the
    // library's `dylib:<path>` provenance here instead.
    assert_eq!(s["source"], "test");
    assert_eq!(s["ordersPlaced"], 1);
    assert_eq!(s["openPositions"], 1);
    assert_eq!(dec_of(&s["openNotionalUsd"]), dec!(3.90));
}

#[test]
fn strategy_position_cap_rejects_the_entry_and_is_counted() {
    let mut limits = HashMap::new();
    limits.insert(
        "spread_arb".to_string(),
        StrategyLimit {
            max_open_positions: Some(0),
            ..Default::default()
        },
    );
    let mut c = core_with_engine(limits);
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        0,
        "position cap 0 must block the entry"
    );
    assert!(
        c.ome().live_orders().is_empty(),
        "no order may reach the OME"
    );
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(s["limitRejected"], 1);
    assert_eq!(s["ordersPlaced"], 0);
    assert_eq!(s["openPositions"], 0);
}

#[test]
fn rejection_causes_report_position_cap_bucket() {
    // E9-c: the limit rejection lands in a machine-parseable bucket next
    // to the live counters, so an operator can see WHY without logs.
    let mut limits = HashMap::new();
    limits.insert(
        "spread_arb".to_string(),
        StrategyLimit {
            max_open_positions: Some(0),
            ..Default::default()
        },
    );
    let mut c = core_with_engine(limits);
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 12_000), 0);
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(
        s["rejectionCauses"]["limit.positionCap"], 1,
        "position-cap rejection must be bucketed: {s}"
    );
    assert_eq!(
        s["ordersRejected"], 0,
        "cap rejections are not place rejections"
    );
    // A strategy that never placed has NO causes object at all.
    let clean_core = core_with_engine(HashMap::new());
    let clean_stats = clean_core.strategy_stats();
    let clean = strategy_entry(&clean_stats, "spread_arb");
    assert!(clean["rejectionCauses"].is_null());
}

#[test]
fn an_over_cap_entry_is_clamped_to_the_cap_and_approved() {
    // #1258: the #1199 silent clamp used to sit AFTER the arbitration
    // Rejected `continue`, where Gate 2's own per-order-cap refusal made it
    // unreachable dead code — every over-cap entry was rejected ACCOUNT_LIMIT
    // and the clamp never ran. Moved before `from_request`, Gate 2 now sees
    // the already clamped request: the 3.90 entry (0.39 × 10; the strategy
    // prices at 88% of the 0.44 mid) on a 1.00 cap is sized down to
    // floor(1.00/0.39, 2dp) = 2.56 shares and APPROVED.
    let mut c = core_with_engine(HashMap::new());
    let now = 1_000_000i64;
    let dir = std::env::temp_dir().join(format!("bk-1258-clamp-{}", std::process::id()));
    c.intent_audit.path = dir.join("intents.jsonl");
    let _ = std::fs::remove_file(&c.intent_audit.path);
    feed_entry_setup(&mut c, now);
    c.risk_config_mut().systemic.account.max_order_notional_usd =
        Bound::new(dec!(1), LimitSource::Flag);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        1,
        "the clamped entry must place, not be refused"
    );
    let orders = c.list_orders();
    let o = orders.first().expect("the clamped entry is on the OME");
    assert_eq!(o.size, dec!(2.56), "size == cap floored to the 2-dp grid");
    assert_eq!(o.price, dec!(0.39));
    let notional = o.price * o.size;
    assert!(
        notional <= dec!(1),
        "clamped notional {notional} must not exceed the cap"
    );
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(s["ordersPlaced"], 1);
    assert_eq!(s["ordersRejected"], 0);

    // The intent audit carries the CLAMPED request as an APPROVED decision —
    // the intents.jsonl sample #1258 asks for (previously these were
    // REJECTED ACCOUNT_LIMIT lines and the APPROVED-clamped shape never
    // existed).
    let audit = std::fs::read_to_string(&c.intent_audit.path).expect("audit written");
    let rec: serde_json::Value = audit
        .lines()
        .last()
        .expect("one audit record")
        .parse()
        .expect("json record");
    assert_eq!(rec["decision"]["status"], "APPROVED", "{audit}");
    assert_eq!(
        rec["intent"]["size"], 2.56,
        "the audited intent is the clamped one"
    );
    assert_eq!(rec["strategy"], "spread_arb");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejection_causes_report_per_order_cap_risk_bucket() {
    // E9-c: a per-order-cap refusal that survives the clamp is bucketed as
    // risk.perOrderCap. The clamp floors the size to the venue's 2-dp share
    // grid, so a cap that cannot afford ONE share at 2dp (0.002 < 0.39 ×
    // 0.01) cannot be clamped — the over-cap request reaches Gate 2 and is
    // refused, and the bucket must name the cap that bit.
    let mut c = core_with_engine(HashMap::new());
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    c.risk_config_mut().systemic.account.max_order_notional_usd =
        Bound::new(dec!(0.002), LimitSource::Flag);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        0,
        "a cap below one 2-dp share must reject the 3.90 entry"
    );
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(s["ordersRejected"], 1);
    assert_eq!(
        s["rejectionCauses"]["risk.perOrderCap"], 1,
        "must count and bucket the risk reason: {s}"
    );
}

#[test]
fn strategy_notional_cap_blocks_below_and_passes_above_the_entry() {
    // Entry notional is 0.43 * 10 = 4.30: a 1.00 cap rejects, a 10.00 cap passes.
    let mut tight = HashMap::new();
    tight.insert(
        "spread_arb".to_string(),
        StrategyLimit {
            max_open_notional_usd: Some(dec!(1)),
            ..Default::default()
        },
    );
    let mut c = core_with_engine(tight);
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        0,
        "notional cap 1.00 must block a 4.30 entry"
    );
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);

    let mut loose = HashMap::new();
    loose.insert(
        "spread_arb".to_string(),
        StrategyLimit {
            max_open_notional_usd: Some(dec!(10)),
            ..Default::default()
        },
    );
    let mut c2 = core_with_engine(loose);
    feed_entry_setup(&mut c2, now);
    assert_eq!(
        c2.engine_evaluate(now + 12_000),
        1,
        "cap above the entry notional must not block"
    );
    assert_eq!(c2.engine_stats()["strategyLimitRejected"], 0);
}

/// E16/#98: the portfolio-level open-notional cap bounds the TOTAL open
/// commitment across all strategies (0 = off = shipped behaviour).
#[test]
fn portfolio_notional_cap_bounds_total_open_exposure() {
    // Entry notional is 0.43 * 10 = 4.30: a 2.00 portfolio cap rejects it
    // even though no per-strategy limit is configured at all.
    let mut c = core_with_engine(HashMap::new());
    c.risk_config_mut().max_open_notional_usd = dec!(2);
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        0,
        "a 4.30 entry breaches a 2.00 portfolio cap"
    );
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(
        s["rejectionCauses"]["limit.portfolioNotionalCap"], 1,
        "the portfolio cap must be bucketed so tooling can answer why: {s}"
    );

    // 0 = off: the same entry goes through untouched.
    let mut c2 = core_with_engine(HashMap::new());
    feed_entry_setup(&mut c2, now);
    assert_eq!(c2.engine_evaluate(now + 12_000), 1);
    assert_eq!(c2.engine_stats()["strategyLimitRejected"], 0);
}

/// #966: same-cycle concurrent entries see each other through `cycle_placed`.
/// Three same-account Buy intents in one cycle under `open_positions >= 2 →
/// skip`: the first two place (cycle-placed count 0, then 1), the third is
/// refused (count 2 → open_for + already_placed = 0 + 2 ≥ 2). This pins the
/// 11-05 same-millisecond three-leg bypass fix for both the real cycle and
/// the replay fast path.
#[test]
fn cycle_placed_backstop_stops_at_two_same_cycle() {
    let dir = std::env::temp_dir().join(format!("bk-cycle-backstop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Issue #967: the policy file is retired, so the backstop arrives the only
    // way a boot accepts it — the `POLICY_RULES` key in `.env` (see
    // `.env.example`; the string `when` form is the readable one the issue's
    // default file used).
    std::fs::write(
        dir.join(".env"),
        "POLICY_RULES=[{\"name\":\"max_two\",\"priority\":100,\"enabled\":true,\
         \"when\":\"open_positions >= 2\",\"then\":{\"action\":\"skip\"}}]\n",
    )
    .unwrap();

    let mut c = Core::new(CoreConfig {
        env_file_path: Some(dir.join(".env").to_string_lossy().into_owned()),
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        round_duration_sec: 900,
        auto_exits_enabled: false,
        assets: vec!["BTC".into(), "ETH".into(), "SOL".into()],
        positions: crate::position::PositionConfig {
            max_positions: 5,
            exit: crate::exit_policy::ExitConfig {
                min_time_left_sec: 0,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    });
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    crate::strategies::test_support::host(&mut eng, cfg.trend, cfg.spread_arb);
    eng.register_user_strategy(
        Box::new(TargetDip {
            name: "dip".into(),
            buy_below: dec!(0.45),
            assets: vec!["BTC".into(), "ETH".into(), "SOL".into()],
            gates: crate::strategies::GateExemptions::none(),
            scope: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        }),
        "test".into(),
    )
    .unwrap();
    assert!(eng.set_strategy_enabled("dip", true));
    c.enable_engine(eng);

    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    for asset in ["BTC", "ETH", "SOL"] {
        let token = format!("{}_up", asset.to_lowercase());
        c.engine_on_data(
            DataEvent::Book {
                token_id: token,
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: now + 1_000,
            },
            now + 1_000,
        );
    }
    // 3 same-account intents in one cycle: 2 place, the 3rd is refused by the
    // cycle-placed backstop (open_for=0 + already_placed=2 → 2 >= 2 → skip).
    let placed = c.engine_evaluate(now + 2_000);
    assert_eq!(
        placed, 2,
        "backstop must allow exactly 2 of 3 same-cycle entries"
    );
    assert_eq!(
        c.ome().live_orders().len(),
        2,
        "exactly two orders reach the OME"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
