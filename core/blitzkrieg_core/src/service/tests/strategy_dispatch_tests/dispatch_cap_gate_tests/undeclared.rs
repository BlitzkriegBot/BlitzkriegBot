//! Undeclared-strategy bypass guard: a strategy armed in `virtual_limits`
//! but not declaring `atomic_batch` takes the historical path byte-identically
//! (no pot, no reservation, no new counters). Moved verbatim from
//! `dispatch_cap_gate_tests.rs` (pure code motion).

use super::super::*;

#[test]
fn undeclared_strategy_keeps_the_per_order_path_byte_identically() {
    // Test 5 (bypass guard): a strategy that did NOT declare
    // `atomic_batch` but IS armed in virtual_limits (and one that is
    // neither) must take the historical path: no pot, no reservation,
    // no new counters — the same entry the default core places.
    // Reference run: NO virtual layer at all.
    let mut reference = core_with_dips(HashMap::new(), &[("plain", &["BTC"])], 5);
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut reference, now);
    let ref_placed = reference.engine_evaluate(now + 2_000);
    let ref_stats = reference.strategy_stats();
    let ref_row = strategy_entry(&ref_stats, "plain").clone();

    // Armed pot, undeclared strategy: same outcome, empty pot.
    let mut limits = HashMap::new();
    limits.insert(
        "plain".to_string(),
        VirtualAccountLimit {
            max_batch_notional_usd: Some(dec!(1)),
            accounts: vec![blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string()],
        },
    );
    let mut armed = core_with_dips(HashMap::new(), &[("plain", &["BTC"])], 5);
    armed.set_virtual_limits_for_test(limits);
    feed_three_asset_dip(&mut armed, now);
    assert_eq!(
        armed.engine_evaluate(now + 2_000),
        ref_placed,
        "an undeclared strategy ignores the pot entirely"
    );
    let armed_stats = armed.strategy_stats();
    let armed_row = strategy_entry(&armed_stats, "plain");
    assert_eq!(
        armed_row["ordersPlaced"], ref_row["ordersPlaced"],
        "byte-identical placement outcome"
    );
    assert!(
        armed_row["rejectionCauses"].is_null()
            || armed_row["rejectionCauses"] == ref_row["rejectionCauses"],
        "no new rejection bucket may appear: {armed_row}"
    );
    assert_eq!(
        armed.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "plain"),
        Decimal::ZERO,
        "the pot must stay empty — reservations are the declared path only"
    );
}
