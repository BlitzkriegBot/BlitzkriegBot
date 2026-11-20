//! Pair orphan sweep: governed scopes flatten a naked leg after the 5 s
//! landing grace; ungoverned strategies see zero alerts and zero flatten
//! orders. Moved verbatim from `dispatch_cap_gate_tests.rs` (pure code
//! motion).

use super::super::*;
use super::helpers::{
    confirm_cross_venue_mapping, core_with_cross_venue_arb, cross_venue_markets,
    feed_cross_venue_books,
};

#[test]
fn pair_orphan_sweep_flattens_a_naked_leg_after_the_grace_window() {
    // The compensation watch: leg ONE lands, leg TWO's order dies before
    // it can (the starved ledger below is one real way — the #457 expiry
    // sweep, a manual withdraw and a venue reject all share the same
    // terminal shape), and after the 5 s landing grace the LIVE tick sweep
    // alerts and flattens the naked leg.
    //
    // Device: the ledger runs dry between the legs. pm-up (0.44 × 250 = 110)
    // just fits; kx-down (0.42 × 250 = 105) does not.
    let mut c = core_with_cross_venue_arb(Some(dec!(1000)));
    {
        // Drain the default book to 110: pm-up (0.44 × 250 = 110) just
        // fits; kx-down (0.42 × 250 = 105) does not.
        let _ = c.accounts_mut().active_ledger_mut().set_balance(dec!(110));
    }
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
    assert_eq!(c.engine_evaluate(now + 2_000), 1, "only leg one lands");

    let held: Vec<String> = c
        .positions()
        .open_positions()
        .iter()
        .map(|p| p.token_id.clone())
        .collect();
    assert_eq!(
        held,
        vec!["pm-up".to_string()],
        "pm-up filled, kx-down never placed"
    );

    // Inside the grace window the sweep holds its hand (the two legs of
    // one batch land serially inside one evaluate cycle).
    c.tick(now + 4_000).expect("grace-window tick");
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "the grace window protects the just-filled leg"
    );

    // Past the window, the LIVE tick sweep flattens the naked leg. The
    // alert names the SAME-market partner token (`partner_of` stamps the
    // condition's other outcome — the dry settlement ladder's key), not
    // the cross-venue leg that died.
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    c.book_snapshot(
        "pm-up",
        vec![(dec!(0.42), dec!(600))],
        vec![(dec!(0.42), dec!(600))],
        now + 12_000,
    );
    c.tick(now + 12_000).expect("sweep tick");
    let mut alerts = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            alerts.push(message);
        }
    }
    assert!(
        alerts.iter().any(|m| m.contains("pair leg orphaned")
            && m.contains("cross_venue_arb")
            && m.contains("partner_missing=pm-down")),
        "the orphan alert must name the strategy and the missing leg: {alerts:?}"
    );
    assert!(
        c.positions().open_positions().is_empty(),
        "the naked leg is flattened: remaining = {:?}",
        c.positions()
            .open_positions()
            .iter()
            .map(|p| p.token_id.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn pair_orphan_sweep_leaves_ungoverned_strategies_alone() {
    // The zero-config guard: the SAME naked-leg shape (one fill, the
    // partner leg dead) under a strategy the pot book does NOT govern
    // must see ZERO alerts and ZERO flatten orders — orphan compensation
    // is a governed-atomic-scope posture, never a global behaviour.
    let mut c = core_with_cross_venue_arb(None);
    {
        // Same starved-ledger device as the governed test: pm-up lands,
        // kx-down's place dies on the reserve.
        let _ = c.accounts_mut().active_ledger_mut().set_balance(dec!(110));
    }
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
        1,
        "leg one lands the same way"
    );
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "leg one fills the same way"
    );

    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    c.book_snapshot(
        "pm-up",
        vec![(dec!(0.42), dec!(600))],
        vec![(dec!(0.42), dec!(600))],
        now + 12_000,
    );
    c.tick(now + 12_000).expect("sweep tick");
    let mut alerts = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            alerts.push(message);
        }
    }
    assert!(
        alerts.iter().all(|m| !m.contains("pair leg orphaned")),
        "an ungoverned scope is invisible to the orphan sweep: {alerts:?}"
    );
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "the naked leg of an ungoverned strategy stays in the book"
    );
    assert!(
        c.ome()
            .live_orders()
            .into_iter()
            .all(|o| o.side != Side::Sell),
        "no flatten SELL was spawned"
    );
}
