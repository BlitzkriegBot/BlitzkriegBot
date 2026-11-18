//! Toggle edge cases without a state path (persist nothing), and boot
//! refusal for an explicit request that resolves to no loaded strategy
//! (#265 — refusal, not warning, `--allow-zero-strategies` is the opt-out).
//! Moved verbatim from `dispatch_boot_enablement_tests.rs` (pure code motion).

use super::super::*;

/// No `strategy_state_path` (backtests, most tests) — a toggle works but
/// writes nothing anywhere.
#[test]
fn toggles_without_a_state_path_persist_nothing() {
    let cfg = CoreConfig {
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        ..Default::default()
    };
    let mut c = Core::new(cfg.clone());
    cfg.install_engine(&mut c).expect("engine install");
    let cfg2 = cfg.engine_config();
    crate::strategies::test_support::host_disabled(
        c.engine.as_mut().expect("engine installed"),
        cfg2.trend,
        cfg2.spread_arb,
    );
    assert!(c.set_strategy_enabled("trend_follow", true));
    assert_eq!(c.enabled_strategy_names(), vec!["trend_follow".to_string()]);
}

/// #265: naming strategies that do not exist is a REFUSAL, not a warning. A
/// kernel that boots with a healthy-looking banner and then trades nothing
/// because the library it was told to run was never loaded is exactly the
/// failure this guards; `--allow-zero-strategies` is the acknowledged
/// opt-out, and a name the operator also disabled explicitly was never
/// requested in the first place.
#[test]
fn an_explicit_request_that_resolves_to_nothing_refuses_the_boot() {
    // The kernel registers nothing itself (PR-B), so in-process every
    // requested name is unresolved — which is precisely the production
    // shape of "the strategy library was not found".
    let boot = |requested: &[&str], disabled: &[&str], allow: bool| {
        let cfg = CoreConfig {
            dry_seed_balance: dec!(1000),
            engine_enabled: true,
            requested_strategies: requested.iter().map(|s| (*s).to_string()).collect(),
            disabled_strategies: disabled.iter().map(|s| (*s).to_string()).collect(),
            allow_zero_strategies: allow,
            ..Default::default()
        };
        let mut core = Core::new(cfg.clone());
        cfg.install_engine(&mut core)
    };

    // Nothing was asked for: the ordinary zero-strategy boot (tests, the
    // backtester, a session that only hosts builtins) is untouched.
    assert!(
        boot(&[], &[], false).is_ok(),
        "an unrequested empty set must never refuse"
    );

    // An explicit request that resolves to nothing refuses, and names both
    // what was asked for and the door out.
    let err = boot(&["alpha_strategy"], &[], false)
        .expect_err("a request that resolves to nothing must refuse the boot");
    assert!(
        err.contains("refusing to start") && err.contains("alpha_strategy"),
        "the refusal must name the request, got: {err}"
    );
    assert!(
        err.contains("--allow-zero-strategies"),
        "the refusal must name the opt-out, got: {err}"
    );
    // The message states the ENGINE's live set, not the request twice: the
    // first draft filled this slot with the enabled list under the label
    // "Requested:", which a probe read as "Requested: ." — a refusal that
    // cannot say what it refused is not a diagnosis.
    assert!(
        err.contains("the engine's live set is []"),
        "the refusal must state the live set, got: {err}"
    );

    // The door is not welded shut: the flag acknowledges the empty set.
    assert!(
        boot(&["alpha_strategy"], &[], true).is_ok(),
        "--allow-zero-strategies must start normally"
    );

    // `--enable-strategy x --disable-strategy x` asked for nothing, so it is
    // not a request that failed — the same rule the toggle order follows.
    assert!(
        boot(&["alpha_strategy"], &["alpha_strategy"], false).is_ok(),
        "an explicitly disabled name is not part of the request"
    );

    // Several names: the refusal counts and lists them all.
    let err = boot(&["alpha_strategy", "cat_strategy"], &[], false).expect_err("still a refusal");
    assert!(
        err.contains("alpha_strategy") && err.contains("cat_strategy"),
        "got: {err}"
    );
}
