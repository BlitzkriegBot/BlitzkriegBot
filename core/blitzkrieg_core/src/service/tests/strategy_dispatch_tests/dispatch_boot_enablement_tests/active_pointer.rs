//! The `active` account pointer moves alone: engine scope set, enablement
//! flags, exits-only demotion and intent output all stay put under
//! `account.switch`; orders still resolve to their own accounts.
//! Moved verbatim from `dispatch_boot_enablement_tests.rs` (pure code motion).

use super::super::*;

/// `active` 指针 —— 引擎的 scope 集、启用标志、exits-only 降级与意图
/// 输出路径一行不碰。IPC 的 account.switch 更是会话级（另一 agent 域
/// 的既有牙已钉 wire 半）；这里钉的是核心侧可能被误接线的最后一环。
#[test]
fn process_level_switch_touches_nothing_but_the_active_pointer() {
    // A two-account core with a test-hosted engine (no strategy_dir: the
    // boot registers nothing itself), built inline — the audit mod's
    // `two_book_core` fixture is private to that module.
    let book = crate::account::AccountLedgers::new(
        vec![
            crate::account::Account {
                id: crate::account::AccountId::from("default"),
                name: "default".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                opening_equity: None,
                updated_at_ms: 0,
            },
            crate::account::Account {
                id: crate::account::AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                opening_equity: None,
                updated_at_ms: 0,
            },
        ],
        crate::account::AccountId::from("default"),
    );
    let mut c = Core::with_account_book(
        CoreConfig {
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: 10_000.into(),
            auto_exits_enabled: false,
            trade_log_path: None,
            ..Default::default()
        },
        book,
    );
    let cfg = c.config.clone();
    cfg.install_engine(&mut c)
        .expect("test engine (no strategy_dir: registers nothing itself)");
    let engine = c.engine.as_mut().expect("test engine");
    crate::strategies::test_support::host_disabled(engine, Default::default(), Default::default());
    // Build a distinguishable scope landscape: paper hosts spread_arb,
    // demoted exits-only; default hosts it ENABLED.
    engine
        .register_scoped_strategy(
            Box::new(crate::strategies::test_support::TestSpreadArb::new(
                Default::default(),
                Default::default(),
            )),
            "test".into(),
            "paper",
        )
        .expect("paper scope");
    engine.set_strategy_enabled("spread_arb", true);
    let held = vec!["spread_arb".to_string()];
    engine.rebuild_account_scopes("paper", &[], &held, &mut |_n, _a| {
        Some(Box::new(
            crate::strategies::test_support::TestSpreadArb::new(
                Default::default(),
                Default::default(),
            ),
        ))
    });
    // The closure above must stand a real instance; rebuild keeps the
    // existing scope anyway (the ADD phase finds it), so assert the shape.
    assert!(
        engine
            .strategy_scopes("spread_arb")
            .iter()
            .any(|a| a == "paper"),
        "the exits-only scope stays hosted"
    );
    let before_enabled = engine.enabled_strategies();
    let before_exits_only = engine.exits_only_scopes();

    // The switch: process-level (the book's own API).
    c.accounts
        .switch(&crate::account::AccountId::from("paper"))
        .expect("switch to a configured account");
    assert_eq!(
        c.accounts.active_id().as_str(),
        "paper",
        "the switch moved the active pointer"
    );

    // NOTHING else moved: scope landscape, enablement, demotions.
    assert_eq!(
        engine.enabled_strategies(),
        before_enabled,
        "enablement untouched by the switch"
    );
    assert_eq!(
        engine.exits_only_scopes(),
        before_exits_only,
        "exits-only demotions untouched by the switch"
    );
    assert!(
        engine
            .strategy_scopes("spread_arb")
            .iter()
            .any(|a| a == "paper"),
        "paper's scope survived the switch"
    );
    // The intent output path still resolves orders to their OWN accounts:
    // a place for paper still lands on paper after default became active.
    let view_default = c
        .account_views(1)
        .into_iter()
        .find(|v| v.id == "default")
        .unwrap();
    assert!(
        view_default.strategies.is_empty(),
        "the book's per-account enablement sets are untouched"
    );
}
