//! Strategy-level virtual accounts (E-VA): per-strategy quota isolation,
//! atomic batch all-or-nothing, fill-proportional release, and cancelled
//! order hold release. Moved verbatim from `dispatch_cap_gate_tests.rs`
//! (pure code motion).

use super::super::*;

// ── E-VA: strategy-level virtual accounts ───────────────────────────────
//
// Test 1 (strategy isolation): A's big entry must NOT consume B's
// headroom — each governed scope reserves against its OWN pot.
// Test 2 (atomic batch): a pair batch whose sum exceeds the pot places
// ZERO legs; the legs cancel together through the existing StrategyLimit
// rejection accounting.
// Test 3 (precise release): a partial fill converts exactly the filled
// shares' share of the hold; the rest stays outstanding.
// Test 4 (terminal release): a cancelled order's hold leaves the pot.
// Test 5 (bypass guard): undeclared strategy + armed pot = the historical
// path, byte-identically (no pot, no counters).

/// A dip strategy that declares `atomic_batch` and emits N equal-share
/// legs per cycle (one per configured asset) — the pair-batch shape the
/// virtual-account path exists for.
struct AtomicDip {
    name: String,
    assets: Vec<String>,
}
impl crate::strategies::EngineStrategy for AtomicDip {
    fn name(&self) -> &str {
        &self.name
    }
    fn on_book(&mut self, _t: &str, _s: &crate::model::OrderbookSnapshot, _n: i64) {}
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    // Pair semantics: legs hold to settlement (the EngineStrategy doc's
    // complete-set rationale). Also the release posture the cancel test
    // exercises — a cancelled leg's token re-arms for a re-attempt.
    fn holds_to_settlement(&self) -> bool {
        true
    }
    fn atomic_batch(&self) -> bool {
        true
    }
    fn find_candidates(
        &mut self,
        ctx: &crate::strategies::StrategyCtx<'_>,
    ) -> Vec<crate::signal::TradeSignal> {
        let mut out = Vec::new();
        for market in ctx.markets() {
            if !self.assets.contains(&market.asset) {
                continue;
            }
            for (token_id, direction) in [
                (&market.up_token_id, crate::model::SignalDirection::Up),
                (&market.down_token_id, crate::model::SignalDirection::Down),
            ] {
                let Some(book) = ctx.fresh_book(token_id) else {
                    continue;
                };
                if book.mid_price <= dec!(0.45) {
                    out.push(crate::signal::TradeSignal {
                        strategy: self.name.clone(),
                        account_id: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
                        asset: market.asset.clone(),
                        direction,
                        token_id: token_id.clone(),
                        condition_id: market.condition_id.clone(),
                        price: book.mid_price,
                        reason: "atomic dip".into(),
                        shares: Some(dec!(10)),
                        pair: None,
                    });
                }
            }
        }
        out
    }
}

/// A core with one enabled `AtomicDip` on the given assets (the kernel
/// ships no strategies; the test hosts its own, PR-B posture).
fn core_with_atomic_dip(virtual_cap: Option<Decimal>, assets: &[&str]) -> Core {
    let mut virtual_limits = HashMap::new();
    if let Some(cap) = virtual_cap {
        virtual_limits.insert(
            "pair_dip".to_string(),
            VirtualAccountLimit {
                max_batch_notional_usd: Some(cap),
                accounts: vec![blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string()],
            },
        );
    }
    let mut c = Core::new(CoreConfig {
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
        virtual_limits,
        ..Default::default()
    });
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    eng.register_user_strategy(
        Box::new(AtomicDip {
            name: "pair_dip".to_string(),
            assets: assets.iter().map(|a| (*a).to_string()).collect(),
        }),
        "test".into(),
    )
    .unwrap();
    assert!(eng.set_strategy_enabled("pair_dip", true));
    c.enable_engine(eng);
    c
}

#[test]
fn virtual_quota_is_per_strategy_a_cannot_starve_b() {
    // Test 1: two strategies share one account. A holds a big RESERVATION
    // (a placed, unfilled entry); B's entry must still place — B's quota
    // is its own pot, and the REAL ledger was never touched by A's hold
    // (the pot is a risk-layer book, not cash).
    let mut c = core_with_atomic_dip(Some(dec!(100)), &["BTC"]);
    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        DataEvent::Book {
            token_id: "btc_up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 1_000,
        },
        now + 1_000,
    );
    // A (pair_dip) places its 10-share leg at 0.44: 4.40 USD held.
    assert_eq!(c.engine_evaluate(now + 2_000), 1);
    assert_eq!(
        c.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "pair_dip"),
        dec!(4.40)
    );

    // B (a plain dip, NOT declared atomic) places against the SAME
    // account while A's hold is outstanding: B's entry is judged on the
    // real ledger (untouched) — it places.
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    eng.register_user_strategy(
        Box::new(TargetDip {
            name: "plain_b".into(),
            buy_below: dec!(0.45),
            assets: vec!["ETH".into()],
            gates: crate::strategies::GateExemptions::none(),
            scope: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        }),
        "test".into(),
    )
    .unwrap();
    assert!(eng.set_strategy_enabled("plain_b", true));
    // The engine is replaced wholesale — re-push the round/book state the
    // new engine needs, then evaluate B's candidate in the same cycle.
    c.enable_engine(eng);
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        DataEvent::Book {
            token_id: "eth_up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 3_000,
        },
        now + 3_000,
    );
    let placed = c.engine_evaluate(now + 4_000);
    assert_eq!(
        placed, 1,
        "B's entry must place: A's virtual hold is A's own"
    );
    let stats = c.strategy_stats();
    let b = strategy_entry(&stats, "plain_b");
    assert_eq!(b["ordersPlaced"], 1, "B must not be starved: {b}");
    // A's hold is untouched by B's placement.
    assert_eq!(
        c.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "pair_dip"),
        dec!(4.40)
    );
}

#[test]
fn atomic_batch_over_cap_cancels_the_whole_batch() {
    // Test 2: two legs at 0.44 x 10 shares = 4.40 each, batch 8.80. A pot
    // of 8.00 covers leg 1 alone but not the SUM: zero legs may place.
    let mut c = core_with_atomic_dip(Some(dec!(8)), &["BTC", "ETH"]);
    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    for (token, t) in [("btc_up", now + 1_000), ("eth_up", now + 1_100)] {
        c.engine_on_data(
            DataEvent::Book {
                token_id: token.into(),
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: t,
            },
            t,
        );
    }
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        0,
        "an over-cap batch must place ZERO legs (no naked first leg)"
    );
    assert!(c.ome().live_orders().is_empty(), "no leg may reach the OME");
    // The refusal rides the EXISTING StrategyLimit accounting.
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "pair_dip");
    assert_eq!(
        s["rejectionCauses"]["limit.virtualCap"], 2,
        "both legs bucketed: {s}"
    );
    assert_eq!(s["ordersPlaced"], 0);
    assert_eq!(
        c.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "pair_dip"),
        Decimal::ZERO,
        "a cancelled batch holds nothing"
    );

    // A pot of 20.00 fits the whole batch: both legs place.
    let mut c2 = core_with_atomic_dip(Some(dec!(20)), &["BTC", "ETH"]);
    c2.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    for (token, t) in [("btc_up", now + 1_000), ("eth_up", now + 1_100)] {
        c2.engine_on_data(
            DataEvent::Book {
                token_id: token.into(),
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: t,
            },
            t,
        );
    }
    assert_eq!(c2.engine_evaluate(now + 2_000), 2);
    assert_eq!(
        c2.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "pair_dip"),
        dec!(8.80),
        "both legs' holds outstanding after placement"
    );
}

#[test]
fn virtual_release_is_fill_proportional() {
    // Test 3: hold 0.44 x 10 = 4.40; fill 1.2 shares → exactly 1.2 x 0.44
    // = 0.528 released, 3.872 still outstanding.
    let mut c = core_with_atomic_dip(Some(dec!(100)), &["BTC"]);
    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        DataEvent::Book {
            token_id: "btc_up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 1_000,
        },
        now + 1_000,
    );
    assert_eq!(c.engine_evaluate(now + 2_000), 1);
    let acct = blitzkrieg_market_api::DEFAULT_ACCOUNT_ID;
    assert_eq!(c.virtual_used(acct, "pair_dip"), dec!(4.40));

    // Fill PART of the 10 resting shares via a crossing book. The identity
    // fill model takes min(remaining, crossing depth) — the ask depth of
    // 1.2 fills exactly 1.2 shares, the rest keeps resting.
    c.book_snapshot(
        "btc_up",
        vec![(dec!(0.42), dec!(1))],
        vec![(dec!(0.42), dec!(1.2))],
        now + 3_000,
    );
    let used_after = c.virtual_used(acct, "pair_dip");
    let released = dec!(4.40) - used_after;
    assert_eq!(
        used_after,
        dec!(3.872),
        "release = 1.2 x 0.44 exactly: {released}"
    );
    assert!(
        c.positions()
            .open_positions()
            .iter()
            .any(|p| p.shares == dec!(1.2)),
        "the 1.2 filled shares are position occupancy"
    );
}

#[test]
fn cancelled_order_releases_its_virtual_hold() {
    // Test 4: a placed entry holds 4.40; cancelling it must leave the
    // pot empty (today's real-ledger release has no virtual twin — that
    // is the hole this closes).
    let mut c = core_with_atomic_dip(Some(dec!(100)), &["BTC"]);
    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        DataEvent::Book {
            token_id: "btc_up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 1_000,
        },
        now + 1_000,
    );
    assert_eq!(c.engine_evaluate(now + 2_000), 1);
    let acct = blitzkrieg_market_api::DEFAULT_ACCOUNT_ID;
    assert_eq!(c.virtual_used(acct, "pair_dip"), dec!(4.40));
    let id = c
        .ome()
        .live_orders()
        .first()
        .expect("the entry rests")
        .order_id
        .clone();
    c.cancel(&id, now + 3_000).unwrap();
    assert_eq!(
        c.virtual_used(acct, "pair_dip"),
        Decimal::ZERO,
        "the cancelled order's hold must release"
    );
    // The freed pot funds the next batch (the pre-check re-passes).
    let mut limits = HashMap::new();
    limits.insert(
        "pair_dip".to_string(),
        VirtualAccountLimit {
            max_batch_notional_usd: Some(dec!(100)),
            accounts: vec![acct.to_string()],
        },
    );
    let _ = limits; // cap unchanged; the re-place below proves the release
    c.book_snapshot(
        "btc_up",
        vec![(dec!(0.43), dec!(100))],
        vec![(dec!(0.45), dec!(100))],
        now + 4_000,
    );
    assert_eq!(
        c.engine_evaluate(now + 5_000),
        1,
        "the freed pot funds the next batch"
    );
}
