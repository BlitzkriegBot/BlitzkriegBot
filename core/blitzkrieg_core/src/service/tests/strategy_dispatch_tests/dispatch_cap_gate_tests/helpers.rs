//! Shared fixtures for the cap-gate domain children: the real cross_venue_arb
//! core builder plus its market / mapping / book feeds. Moved verbatim from
//! `dispatch_cap_gate_tests.rs` (pure code motion).

use super::super::*;

pub(super) fn core_with_cross_venue_arb(virtual_cap: Option<Decimal>) -> Core {
    // The REAL cross_venue_arb Lua package, hosted the way `install_engine`
    // hosts one: package loaded from its worktree dir, `holds_to_settlement`
    // and `atomic_batch` stamped from the manifest, the kernel's ONE fee
    // schedule injected (the shipped legacy_quadratic — `bk.fees()` nil
    // would refuse everything). Registration rides
    // `register_user_strategy` (the `load_lua_package_inner` route minus the
    // engine plumbing), DISABLED — the test enables it explicitly, the same
    // opt-in an operator makes.
    let package_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/cross_venue_arb");
    let mut virtual_limits = HashMap::new();
    if let Some(cap) = virtual_cap {
        virtual_limits.insert(
            "cross_venue_arb".to_string(),
            VirtualAccountLimit {
                max_batch_notional_usd: Some(cap),
                accounts: vec![blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string()],
            },
        );
    }
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(10000),
            ..Default::default()
        },
        dry_seed_balance: dec!(100000),
        engine_enabled: true,
        round_duration_sec: 900,
        auto_exits_enabled: false,
        min_round_age_sec: 0,
        // The pair's legs are DECLARED at the strategy's capacity (the
        // thin leg's depth); the kernel band must never clamp below it.
        min_shares: dec!(1),
        max_shares: dec!(10000),
        // The pair's two legs are TWO live positions on one asset with
        // DIFFERENT condition ids (one per venue) — the #363 per-asset
        // count must cover them, exactly as an operator running this
        // package would configure it (`--max-positions-per-asset 2`).
        max_positions_per_asset: Some(4),
        // cwd-relative logs are the historical three-copies pollution:
        // a test writes nothing to the process cwd.
        order_log_path: None,
        trade_log_path: None,
        position_log_path: None,
        execution_policy_path: None,
        positions: crate::position::PositionConfig {
            // Same shape as the per-asset count above, on the position
            // manager itself: two legs = two open positions.
            max_positions: 8,
            ..Default::default()
        },
        virtual_limits,
        ..Default::default()
    });
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    crate::strategies::test_support::host_disabled(&mut eng, cfg.trend, cfg.spread_arb);
    let loaded = crate::strategy_engine::lua_loader::load_lua_package(&package_dir)
        .expect("cross_venue_arb package must load");
    assert!(
        loaded.atomic_batch,
        "the fixture premise: the manifest declares atomic_batch"
    );
    let sched = crate::exit_policy::fee_schedule();
    let adapter =
        crate::strategy_engine::lua_loader::LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
            .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
            .declare_atomic_batch(loaded.atomic_batch)
            // RFC-645 A-1: this fixture hosts the REAL arbiter package — stamp
            // the statistics declaration the production loader stamps (the
            // acceptance predicate still requires atomic_batch + the live
            // PairBook entry the fixture's evaluate produces).
            .declare_pair_book_stats(true);
    adapter.set_fee_schedule(blitzkrieg_lua_runtime::FeeScheduleView {
        name: sched.name.to_string(),
        rate: sched.rate.to_string(),
        exponent: sched.exponent,
    });
    eng.register_user_strategy(Box::new(adapter), format!("lua:{}", package_dir.display()))
        .expect("cross_venue_arb registers");
    assert!(eng.set_strategy_enabled("cross_venue_arb", true));
    c.enable_engine(eng);
    // Issue #1127: the zero-config `default` account is born bound to
    // polymarket (DEFAULT_SEED_VENUE). These dispatch/cap-gate fixtures,
    // however, judge the pre-v2 single-account batch shape (pot accounting,
    // orphan sweep, naked-leg degradation) and are venue-agnostic, so restore
    // `default` to unbound here — both legs (polymarket + kalshi) can then
    // ride the same account and the pot is computed exactly as before. This
    // only affects the test fixture, not the #1127 business behaviour.
    c.accounts
        .account_mut(&blitzkrieg_market_api::default_account_id())
        .unwrap()
        .venue = "".into();
    // 跨所套利弹窗 v2: the pairing is CONFIG-DRIVEN (`arb.config.set` →
    // the adapter's params bag); unconfigured ⇒ the strategy pairs
    // nothing (fail-closed). The fixture wires both legs onto the
    // DEFAULT account — the pre-v2 single-account batch shape these
    // dispatch tests judge (pot accounting, orphan sweep, naked-leg
    // degradation ride the same (account, strategy) scope as before).
    c.arb_config_set(&crate::account::CrossVenueArbConfig {
        leg_a_venue: "polymarket".into(),
        leg_a_account: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        leg_b_venue: "kalshi".into(),
        leg_b_account: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        pairs: vec!["BTC".into()],
    })
    .expect("the leg configuration lands (memory-only core)");
    c
}

/// The two-venue round the package prices: pm quotes the cheaper UP ask,
/// kx the cheaper DOWN ask. `expires_at_ms` rides the 900 s grid (the
/// #354 cadence the config runs).
pub(super) fn cross_venue_markets(now: i64) -> Vec<CryptoMarket> {
    let slot = now / 1000 / 900;
    let market = |venue: &str, cond: &str, up: &str, down: &str| CryptoMarket {
        asset: "BTC".into(),
        condition_id: cond.into(),
        question_id: "q".into(),
        up_token_id: up.into(),
        down_token_id: down.into(),
        up_price: dec!(0.48),
        down_price: dec!(0.45),
        expires_at_ms: (slot + 1) * 900 * 1000,
        round_slot: slot,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: venue.into(),
        neg_risk: false,
        question: "BTC up or down".into(),
    };
    vec![
        market("polymarket", "pm-cond", "pm-up", "pm-down"),
        market("kalshi", "kx-cond", "kx-up", "kx-down"),
    ]
}

/// The confirmed mapping binding the two listings into one bet (the human
/// gate: `unified_confirm`'s own ledger route — the exact path production
/// clicks).
pub(super) fn confirm_cross_venue_mapping(c: &mut Core, now: i64) {
    let listing =
        |venue: blitzkrieg_market_api::unified::Venue, cond: &str, up: &str, down: &str| {
            blitzkrieg_market_api::unified::PlatformListing {
                venue,
                condition_id: cond.into(),
                up_token_id: up.into(),
                down_token_id: down.into(),
                asset: Some("bitcoin".into()),
                title: "Bitcoin Up or Down".into(),
                settlement_rules: "Binance 1m candle".into(),
                settlement_source: blitzkrieg_market_api::unified::SettlementSource::Chainlink,
                expires_at_ms: now + 900_000,
                status: blitzkrieg_market_api::unified::ListingStatus::Active,
            }
        };
    c.unified_confirm(
        "Bitcoin Up or Down",
        "uid:test",
        vec![
            listing(
                blitzkrieg_market_api::unified::Venue::Polymarket,
                "pm-cond",
                "pm-up",
                "pm-down",
            ),
            listing(
                blitzkrieg_market_api::unified::Venue::Kalshi,
                "kx-cond",
                "kx-up",
                "kx-down",
            ),
        ],
        now,
    )
    .expect("the two-listing mapping confirms");
}

/// Feed the pair books: pm up ask 0.44 / depth 600, kx down ask 0.42 /
/// depth 250 (the thin leg), the other legs mid-priced. Cross-venue pair
/// cost ≈ 0.875 + fees ≤ 0.98 → the package fires both legs.
pub(super) fn feed_cross_venue_books(c: &mut Core, now: i64) {
    let mut book = |token: &str, ask: Decimal, depth: Decimal| {
        c.engine_on_data(
            DataEvent::Book {
                token_id: token.into(),
                bids: vec![(ask - dec!(0.01), depth)],
                asks: vec![(ask, depth)],
                now_ms: now,
            },
            now,
        );
    };
    book("pm-up", dec!(0.44), dec!(600));
    book("pm-down", dec!(0.50), dec!(600));
    book("kx-up", dec!(0.48), dec!(9000));
    book("kx-down", dec!(0.42), dec!(250));
}
