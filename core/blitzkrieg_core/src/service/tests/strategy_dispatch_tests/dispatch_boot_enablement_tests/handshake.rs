//! E27 multi-plugin handshake: ANY active plugin can serve a declaration —
//! one satisfiable plugin loads the strategy, all-refused diagnostics name
//! every plugin, a single refusal keeps the historical message byte-identical,
//! and an empty snapshot leaves the handshake inert.
//! Moved verbatim from `dispatch_boot_enablement_tests.rs` (pure code motion).

use super::super::*;

// ── E27 multi-plugin handshake: ANY active plugin can serve ─────────────

fn prediction_plugin_modes(name: &str) -> crate::compat::PluginModes {
    crate::compat::PluginModes {
        name: name.into(),
        modes: vec![blitzkrieg_market_api::MarketMode {
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            structure: Some(blitzkrieg_market_api::MarketStructure::BinaryOutcomeWheel),
            capabilities: blitzkrieg_market_api::MarketCapabilities::NONE,
        }],
    }
}

fn spot_plugin_modes(name: &str) -> crate::compat::PluginModes {
    crate::compat::PluginModes {
        name: name.into(),
        modes: vec![blitzkrieg_market_api::MarketMode {
            market_type: blitzkrieg_market_api::MarketType::Spot,
            structure: None,
            capabilities: blitzkrieg_market_api::MarketCapabilities::NONE,
        }],
    }
}

fn prediction_declaration() -> Vec<blitzkrieg_strategy_api::StrategyMode> {
    vec![blitzkrieg_strategy_api::StrategyMode {
        market_type: blitzkrieg_market_api::MarketType::Prediction,
        structure: Some(blitzkrieg_market_api::MarketStructure::BinaryOutcomeWheel),
    }]
}

#[test]
fn a_declaration_served_by_any_active_plugin_loads() {
    // polymarket can serve the prediction declaration; kalshi (spot-shaped
    // here, only to make a second distinct plugin) cannot. Any one
    // satisfiable plugin loads the strategy — a venue the strategy cannot
    // serve must not block the venues it can.
    let mut c = Core::new(CoreConfig::default());
    c.set_plugin_modes(vec![
        prediction_plugin_modes("polymarket"),
        spot_plugin_modes("kalshi"),
    ]);
    c.set_strategy_declarations_for_test("served", prediction_declaration());
    assert!(
        c.compatibility_refusal("served").is_none(),
        "one satisfiable plugin is enough: {:?}",
        c.compatibility_refusal("served")
    );
}

#[test]
fn a_declaration_no_plugin_serves_names_every_plugin() {
    // A FUTURES declaration against a prediction plugin and a spot plugin:
    // both refuse, and the diagnostic names BOTH plugins so the operator
    // can see the full mismatch surface.
    let mut c = Core::new(CoreConfig::default());
    c.set_plugin_modes(vec![
        prediction_plugin_modes("polymarket"),
        spot_plugin_modes("kalshi"),
    ]);
    c.set_strategy_declarations_for_test(
        "unserved",
        vec![blitzkrieg_strategy_api::StrategyMode {
            market_type: blitzkrieg_market_api::MarketType::Futures,
            structure: None,
        }],
    );
    let refusal = c.compatibility_refusal("unserved").expect("all refused");
    assert!(refusal.contains("polymarket"), "{refusal}");
    assert!(refusal.contains("kalshi"), "{refusal}");
    assert!(refusal.contains("futures"), "{refusal}");
}

#[test]
fn a_single_plugin_refusal_keeps_the_historical_message() {
    // One plugin, refused: the message must be byte-identical to the
    // pre-multi-plugin wording (no per-plugin prefix, no join separator).
    let mut c = Core::new(CoreConfig::default());
    c.set_plugin_modes(vec![spot_plugin_modes("spot_venue")]);
    c.set_strategy_declarations_for_test("s", prediction_declaration());
    let refusal = c.compatibility_refusal("s").expect("refused");
    assert!(
        refusal.starts_with("incompatible modes: strategy 's' wants ["),
        "the historical format, unprefixed: {refusal}"
    );
    assert_eq!(refusal.matches("incompatible modes:").count(), 1);
}

#[test]
fn an_empty_plugin_snapshot_leaves_the_handshake_inert() {
    // Backtester / tests: no snapshot injected, nothing refuses.
    let mut c = Core::new(CoreConfig::default());
    c.set_plugin_modes(Vec::new());
    c.set_strategy_declarations_for_test("s", prediction_declaration());
    assert!(c.compatibility_refusal("s").is_none());
}
