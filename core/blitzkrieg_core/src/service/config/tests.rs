//! Config tests: share band, market plugin resolution and the issue-#176
//! unverified-expiry policy spelling.

use super::CoreConfig;
use super::{PolicyParseError, UnverifiedExpiryPolicy};
use rust_decimal::Decimal;

/// Issue #176 (R7.1 L4 fail-closed): the deleted `cash` policy must be a
/// REFUSAL, not a silent downgrade to `receivable`. A deployment that still
/// says `BK_UNVERIFIED_EXPIRY_POLICY=cash` gets a distinguishable error so the
/// boot path can refuse with a reason (and `parser` never yields a policy).
#[test]
fn the_removed_cash_policy_is_refused_not_downgraded() {
    let err = UnverifiedExpiryPolicy::parse("cash").expect_err("cash is gone");
    assert_eq!(
        err,
        PolicyParseError::RemovedCash,
        "the removed spelling gets its own error variant (explains WHY)"
    );
    // The message names the issue and the reason the model was removed.
    let msg = err.to_string();
    assert!(msg.contains("cash"), "{msg}");
    assert!(msg.contains("#176"), "{msg}");
    assert!(msg.contains("L1"), "{msg}");

    // Case/whitespace spelling variants are the same refusal.
    assert_eq!(
        UnverifiedExpiryPolicy::parse("  CASH ").unwrap_err(),
        PolicyParseError::RemovedCash
    );

    // A typo is a DIFFERENT error (never silently accepted either).
    assert_eq!(
        UnverifiedExpiryPolicy::parse("cashflow").unwrap_err(),
        PolicyParseError::Unknown("cashflow".to_string())
    );

    // The one surviving value parses, and it is the default.
    assert_eq!(
        UnverifiedExpiryPolicy::parse("receivable").unwrap(),
        UnverifiedExpiryPolicy::Receivable
    );
    assert_eq!(
        UnverifiedExpiryPolicy::default(),
        UnverifiedExpiryPolicy::Receivable
    );
    assert_eq!(UnverifiedExpiryPolicy::default().as_str(), "receivable");
}

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
