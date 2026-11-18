//! cross_venue_arb driven through the real Lua package (manifest-declared
//! `atomic_batch`, kernel fee schedule injected, pair book stats stamped).
//! Over-pot refusal + both-legs-place acceptance twin. Moved verbatim from
//! `dispatch_cap_gate_tests.rs` (pure code motion).

use super::super::*;
use super::helpers::{
    confirm_cross_venue_mapping, core_with_cross_venue_arb, cross_venue_markets,
    feed_cross_venue_books,
};

#[test]
fn cross_venue_arb_over_pot_refuses_the_whole_pair_through_the_real_package() {
    // Two legs at 0.44 / 0.42 × 250 declared shares ≈ 215 notional. A pot
    // of 200 covers neither pair whole... it refuses the WHOLE batch:
    // zero legs reach the OME, and the refusal rides the existing
    // StrategyLimit accounting (`limit.virtualCap`, one per leg).
    // Two legs at 0.44 / 0.42 × 250 declared shares ≈ 215 notional. A pot
    // of 200 covers neither pair whole... it refuses the WHOLE batch:
    // zero legs reach the OME, and the refusal rides the existing
    // StrategyLimit accounting (`limit.virtualCap`, one per leg).
    let mut c = core_with_cross_venue_arb(Some(dec!(200)));
    let now = 1_700_000_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: cross_venue_markets(now),
            now_ms: now,
        },
        now,
    );
    confirm_cross_venue_mapping(&mut c, now);
    feed_cross_venue_books(&mut c, now + 1_000);
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        0,
        "an over-cap pot must place ZERO legs through the real package"
    );
    assert!(c.ome().live_orders().is_empty(), "no leg reaches the OME");
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "cross_venue_arb");
    assert_eq!(
        s["rejectionCauses"]["limit.virtualCap"], 2,
        "both legs bucketed: {s}"
    );
    assert_eq!(
        c.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "cross_venue_arb"),
        Decimal::ZERO,
        "a refused pair holds nothing"
    );

    // The same books under a pot that fits: BOTH legs place — the
    // acceptance twin proving the refusal above was the cap, not the
    // package. The package quotes at its own asks, so each leg's
    // MakerThenTaker POST crosses and the leg lands inside its own place
    // call: the pot's holds convert to position occupancy within the
    // cycle, and the committed 215 reads off the POSITION book (the pot's
    // own `batch_fits` arithmetic: used + committed_elsewhere).
    let mut c2 = core_with_cross_venue_arb(Some(dec!(1000)));
    c2.engine_on_data(
        DataEvent::RoundMarkets {
            markets: cross_venue_markets(now),
            now_ms: now,
        },
        now,
    );
    confirm_cross_venue_mapping(&mut c2, now);
    feed_cross_venue_books(&mut c2, now + 1_000);
    assert_eq!(c2.engine_evaluate(now + 2_000), 2, "both legs place");
    let committed: Decimal = c2
        .positions()
        .open_positions()
        .iter()
        .map(|p| p.cost_usd)
        .sum();
    assert_eq!(
        committed,
        dec!(215),
        "250 × (0.44 + 0.42) committed as ONE ticket"
    );
    assert_eq!(
        c2.positions().open_positions().len(),
        2,
        "the pair is whole: one position per venue leg"
    );
}
