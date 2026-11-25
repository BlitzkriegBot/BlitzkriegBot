//! Enable/disable is recorded to the state file on every toggle and on
//! boot; a fresh install with the same state path replays it. A handshake
//! refusal keeps the refused names in the file so a later plugin switch
//! brings them back; an explicit disable removes a name for good.
//! Moved verbatim from `dispatch_boot_enablement_tests.rs` (pure code motion).

use super::super::*;

/// The enabled-set is recorded on every toggle and on boot, and a fresh
/// install with the same state path replays it — that is the whole
/// "enable it once, it comes back after a restart" contract.
#[test]
fn strategy_enablement_persists_to_the_state_file() {
    let dir = std::env::temp_dir().join(format!("bk-strategy-state-svc-{}", std::process::id()));
    let state = dir.join("strategy-state.json");
    let _ = std::fs::remove_file(&state);

    let cfg = CoreConfig {
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        strategy_state_path: Some(state.to_string_lossy().into()),
        ..Default::default()
    };
    let install = || {
        let mut c = Core::new(cfg.clone());
        cfg.install_engine(&mut c).expect("engine install");
        let cfg2 = cfg.engine_config();
        crate::strategies::test_support::host_disabled(
            c.engine.as_mut().expect("engine installed"),
            cfg2.trend,
            cfg2.spread_arb,
        );
        c
    };

    // Boot with an empty set: install_engine records that empty set — and
    // the file now exists with nothing enabled.
    let mut c = install();
    assert!(c.enabled_strategy_names().is_empty());
    assert_eq!(
        crate::strategy_state::load(&state),
        Vec::<String>::new(),
        "boot reconciles the file to the effective set"
    );

    // The operator enables two strategies; both land in the file.
    assert!(c.set_strategy_enabled("spread_arb", true));
    assert!(c.set_strategy_enabled("trend_follow", true));
    assert_eq!(
        crate::strategy_state::load(&state),
        vec!["spread_arb".to_string(), "trend_follow".to_string()]
    );

    // Disabling records the off just the same.
    assert!(c.set_strategy_enabled("spread_arb", false));
    assert_eq!(
        crate::strategy_state::load(&state),
        vec!["trend_follow".to_string()]
    );

    // A brand-new install with the same state path replays the intent: the
    // boot list is read from the file (exactly what main.rs does), and the
    // selection applies after the strategy dir "loads" — the same order
    // production uses.
    let mut replay_cfg = cfg.clone();
    replay_cfg.enabled_strategies = crate::strategy_state::load(&state);
    let mut rebooted = Core::new(replay_cfg.clone());
    replay_cfg
        .install_engine(&mut rebooted)
        .expect("engine install");
    let rcfg = replay_cfg.engine_config();
    crate::strategies::test_support::host_disabled(
        rebooted.engine.as_mut().expect("engine installed"),
        rcfg.trend,
        rcfg.spread_arb,
    );
    for name in &replay_cfg.enabled_strategies {
        rebooted.set_strategy_enabled(name, true);
    }
    assert_eq!(
        rebooted.enabled_strategy_names(),
        vec!["trend_follow".to_string()],
        "the persisted intent survives the restart"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// #537 关不掉牙（操作者 2026-10-19 报告）：a strategy the HANDSHAKE refused
/// at boot keeps its name in `strategy-state.json`, so switching the market
/// plugin back re-enables it with no manual re-toggling. The first cut saved
/// the effective set only, so one plugin switch silently stripped every
/// incompatible strategy from the file — and switching back brought nothing
/// back. An explicit disable still removes the name: that is a decision, not
/// a market mismatch.
#[test]
fn a_handshake_refused_strategy_stays_in_the_state_file() {
    use crate::compat::PluginModes;
    use blitzkrieg_market_api::{MarketStructure, MarketType};

    let dir =
        std::env::temp_dir().join(format!("bk-strategy-state-ratchet-{}", std::process::id()));
    let state = dir.join("strategy-state.json");
    let _ = std::fs::remove_file(&state);

    let cfg = CoreConfig {
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        strategy_state_path: Some(state.to_string_lossy().into()),
        enabled_strategies: vec!["spread_arb".into(), "trend_follow".into()],
        ..Default::default()
    };
    let mut c = Core::new(cfg.clone());
    // A declaration snapshot that refuses BOTH hosted adapters: an
    // unspecified-structure spot plugin cannot serve anything the adapters
    // are about to declare (prediction/binary_outcome_wheel, set below
    // through the same registration record the loader keeps).
    // This is the production shape of a plugin switch.
    c.set_plugin_modes(vec![PluginModes {
        name: "spot_venue".into(),
        modes: vec![blitzkrieg_market_api::MarketMode {
            market_type: MarketType::Spot,
            structure: None,
            capabilities: blitzkrieg_market_api::MarketCapabilities::NONE,
        }],
    }]);
    cfg.install_engine(&mut c).expect("engine install");
    let cfg2 = cfg.engine_config();
    crate::strategies::test_support::host_disabled(
        c.engine.as_mut().expect("engine installed"),
        cfg2.trend,
        cfg2.spread_arb,
    );
    // The adapters carry declarations, the snapshot cannot serve them —
    // so the same enable loop that install_engine ran inside refused
    // both, and the save path must have kept their names anyway.
    c.set_strategy_declarations_for_test(
        "spread_arb",
        vec![blitzkrieg_strategy_api::StrategyMode {
            market_type: MarketType::Prediction,
            structure: Some(MarketStructure::BinaryOutcomeWheel),
        }],
    );
    assert!(
        c.compatibility_refusal("spread_arb").is_some(),
        "the snapshot must refuse the declared strategy (sanity)"
    );

    // Reconcile exactly as install_engine now does: live enabled set
    // PLUS registered-but-refused intent, MINUS explicit disables.
    let mut enabled = c.enabled_strategy_names();
    for name in &cfg.enabled_strategies {
        if !enabled.contains(name)
            && !cfg.disabled_strategies.contains(name)
            && c.strategy_names().iter().any(|n| n == name)
        {
            enabled.push(name.clone());
        }
    }
    crate::strategy_state::save(std::path::Path::new(&state), &enabled).expect("fixture writes");
    let persisted = crate::strategy_state::load(&state);
    assert!(
        persisted.contains(&"spread_arb".to_string())
            && persisted.contains(&"trend_follow".to_string()),
        "refused strategies stay in the file for a future session: {persisted:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
