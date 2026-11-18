//! Config tests: share band and market plugin resolution. Moved verbatim
//! from `config.rs` (pure code motion, no logic edits).

use super::CoreConfig;
use rust_decimal::Decimal;

/// Issue #1189 (PR-D): the account row wins; a missing row falls back to
/// the global band.
#[test]
fn share_band_for_account_rows_win() {
    let mut config = CoreConfig::default();
    config
        .account_min_shares
        .insert("A".to_string(), Decimal::from(10));
    config
        .account_max_shares
        .insert("A".to_string(), Decimal::from(20));
    config
        .account_min_shares
        .insert("B".to_string(), Decimal::from(5));
    config
        .account_max_shares
        .insert("B".to_string(), Decimal::from(50));
    assert_eq!(
        config.share_band_for("A"),
        (Decimal::from(10), Decimal::from(20))
    );
    assert_eq!(
        config.share_band_for("B"),
        (Decimal::from(5), Decimal::from(50))
    );
    // No row for C: the global band stands.
    assert_eq!(
        config.share_band_for("C"),
        (config.min_shares, config.max_shares)
    );
}

/// Issue #1189 (PR-D): the account row wins; a missing row falls back to
/// the caller-supplied fallback.
#[test]
fn market_plugin_for_account_rows_win() {
    let mut config = CoreConfig::default();
    config
        .account_market_plugins
        .insert("A".to_string(), "polymarket".to_string());
    config
        .account_market_plugins
        .insert("B".to_string(), "kalshi".to_string());
    assert_eq!(config.market_plugin_for("A", "fallback"), "polymarket");
    assert_eq!(config.market_plugin_for("B", "fallback"), "kalshi");
    // No row for C: the fallback stands.
    assert_eq!(config.market_plugin_for("C", "fallback"), "fallback");
}

/// Issue #1189 (PR-D): empty maps resolve every account to the global
/// band — the historical single-account behaviour, byte for byte.
#[test]
fn share_band_for_empty_map_matches_global() {
    let config = CoreConfig::default();
    assert_eq!(
        config.share_band_for("any"),
        (config.min_shares, config.max_shares)
    );
    assert!(config.account_min_shares.is_empty());
    assert!(config.account_max_shares.is_empty());
    assert!(config.account_market_plugins.is_empty());
}
