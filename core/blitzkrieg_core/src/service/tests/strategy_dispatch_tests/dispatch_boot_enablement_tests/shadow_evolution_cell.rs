//! Shadow Evolution keeps a disabled strategy's evolved cell across a
//! toggle — keying the evolvable set off `enabled` would throw away
//! parameters and rollback anchor on disable (E2-c regression guard).
//! Moved verbatim from `dispatch_boot_enablement_tests.rs` (pure code motion).

use super::super::*;

/// Shadow Evolution sees every evolvable strategy the engine hosts, whether it
/// is enabled or not (E2-c): the knob declaration is read off the live
/// instance, and `register_strategies` drops the cell of anything it is not
/// handed — so keying the set off `enabled` would make a runtime disable throw
/// away that strategy's evolved parameters and rollback anchor.
///
/// This is the regression guard for exactly that: toggle the chase leg off and
/// back on, and the value applied to it must still be in force.
#[test]
fn evolution_keeps_the_cell_of_a_disabled_strategy_across_a_toggle() {
    let (tuning, dir) = scratch("toggle-cell");
    let base = CoreConfig {
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        shadow_evolution_tuning: tuning,
        ..Default::default()
    };
    let mut c = Core::new(base.clone());
    base.install_engine(&mut c).expect("engine install");
    // The kernel registers nothing itself (PR-B); host the adapters so the
    // evolution manager has strategies to build units for.
    let ecfg = base.engine_config();
    crate::strategies::test_support::host(
        c.engine.as_mut().expect("engine installed"),
        ecfg.trend,
        ecfg.spread_arb,
    );
    c.shadow_evolution_enable(0);
    // Registered from the start even though it starts disabled.
    assert_eq!(
        c.shadow_evolution().strategy_names(),
        vec![
            "spread_arb".to_string(),
            "trend_follow".to_string(),
            "mean_reversion".to_string()
        ],
    );
    assert!(c.shadow_evolution_status_for("trend_follow", 0).is_some());
    // Its declared knobs are the strategy's own, not an invented set.
    let names: Vec<String> = c
        .shadow_evolution()
        .declared_knobs("trend_follow")
        .into_iter()
        .map(|k| k.name)
        .collect();
    assert_eq!(
        names,
        vec![
            "momentum_window_sec".to_string(),
            "min_move_pct".to_string(),
            "min_confirm_price".to_string(),
            "break_price".to_string(),
            "max_entry_price".to_string(),
            "max_spread_pct".to_string(),
        ]
    );

    assert!(c.set_strategy_enabled("trend_follow", true));
    assert!(
        c.shadow_evolution_apply(one_param("trend_follow", "min_move_pct", dec!(3.1)), 1_200)
            .is_ok()
    );
    let evolved = c
        .shadow_evolution()
        .params_for("trend_follow")
        .unwrap()
        .get("min_move_pct");
    assert_eq!(evolved, Some(dec!(3.1)));

    // Off and on again: same cell, same value — a toggle is not a reset.
    assert!(c.set_strategy_enabled("trend_follow", false));
    assert!(c.set_strategy_enabled("trend_follow", true));
    assert_eq!(
        c.shadow_evolution()
            .params_for("trend_follow")
            .unwrap()
            .get("min_move_pct"),
        Some(dec!(3.1)),
        "a runtime toggle must not discard the strategy's evolved value"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
