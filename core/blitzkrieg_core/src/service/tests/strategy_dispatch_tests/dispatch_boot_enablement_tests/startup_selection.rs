//! Startup `enabled_strategies`/`disabled_strategies` selection goes through
//! the same toggle as runtime `strategy.enable`/`disable`: unknown names are
//! silent no-ops, disable wins over enable, nothing is enabled by default.
//! Moved verbatim from `dispatch_boot_enablement_tests.rs` (pure code motion).

use super::super::*;

/// Installing an engine with a selection puts the session in exactly the state
/// an operator would reach with `strategy.enable`/`strategy.disable`: the
/// selection goes through the same toggle (so Shadow Evolution is rewired the
/// same way), an unknown name is ignored rather than fatal, and disabling wins
/// over enabling when both name the same strategy.
#[test]
fn startup_selection_matches_a_runtime_toggle() {
    // `install_engine` loads only what `strategy_dir` holds (PR-B: the
    // kernel registers nothing itself), so the test hosts the three
    // adapters right after it — the same registration the loader performs
    // for a real library. Nothing is enabled unless the config says so.
    let install = |cfg: CoreConfig| {
        let mut c = Core::new(cfg.clone());
        // No `requested_strategies` here, so the startup self-check has
        // nothing to insist on and install cannot refuse.
        cfg.install_engine(&mut c).expect("engine install");
        let cfg2 = cfg.engine_config();
        crate::strategies::test_support::host_disabled(
            c.engine.as_mut().expect("engine installed"),
            cfg2.trend,
            cfg2.spread_arb,
        );
        // The startup selection is applied after the strategy dir loads, in
        // the same order production uses — so a name asked for at startup
        // lands on the just-registered adapter (an unknown name stays a
        // silent no-op, exactly what install_engine does).
        for name in &cfg.enabled_strategies {
            c.set_strategy_enabled(name, true);
        }
        for name in &cfg.disabled_strategies {
            c.set_strategy_enabled(name, false);
        }
        c
    };
    let base = CoreConfig {
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        ..Default::default()
    };

    // Default: ZERO strategies enabled (the kernel couples to none). The
    // hosted adapters all register, all disabled.
    let plain = install(base.clone());
    assert!(
        plain.enabled_strategy_names().is_empty(),
        "a fresh boot must enable nothing: {:?}",
        plain.enabled_strategy_names()
    );
    assert_eq!(
        plain.strategy_names(),
        vec![
            "mean_reversion".to_string(),
            "spread_arb".to_string(),
            "trend_follow".to_string()
        ],
        "registered but disabled (sorted: the per-NAME projection dedupes across account scopes)"
    );

    // Opt in by flag: same result as toggling it at runtime.
    let by_flag = install(CoreConfig {
        enabled_strategies: vec!["trend_follow".into()],
        ..base.clone()
    });
    let mut by_toggle = install(base.clone());
    assert!(by_toggle.set_strategy_enabled("trend_follow", true));

    assert_eq!(
        by_flag.enabled_strategy_names(),
        vec!["trend_follow".to_string()]
    );
    assert_eq!(
        by_flag.enabled_strategy_names(),
        by_toggle.enabled_strategy_names()
    );

    // An explicit "off" wins over an "on" for the same name, and a name the
    // engine does not host is ignored without taking the session down.
    let both = install(CoreConfig {
        enabled_strategies: vec!["trend_follow".into(), "alpha_strategy".into()],
        disabled_strategies: vec!["trend_follow".into()],
        ..base.clone()
    });
    assert!(
        both.enabled_strategy_names().is_empty(),
        "enable then disable the same name nets to off: {:?}",
        both.enabled_strategy_names()
    );
    assert_eq!(
        both.strategy_names(),
        vec![
            "mean_reversion".to_string(),
            "spread_arb".to_string(),
            "trend_follow".to_string()
        ],
        "an unknown name must not register anything"
    );
}
