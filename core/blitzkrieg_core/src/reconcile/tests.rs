use super::audit::{
    AuditInput, AuditReport, CashIdentity, REALIGN_EXPLAIN_TTL_MS, audit_accounting,
};
use super::reconcile;
use super::types::{ReconcileAction, VenueSnapshot, VenueTrade};
use crate::model::*;
use crate::ome::SubmitParams;
use crate::service::{Core, CoreConfig};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn req(key: &str, size: rust_decimal::Decimal) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price: dec!(0.4),
        size,
        internal_key: key.into(),
        strategy: "s".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

fn new_core() -> Core {
    Core::new(CoreConfig::default())
}

fn live_order(core: &mut Core, key: &str, core_id: &str, venue_id: &str, now: i64) {
    core.ome
        .submit(SubmitParams {
            order_id: core_id.into(),
            request: req(key, dec!(10)),
            submitted_at_ms: now,
            decision_at_ms: now,
            execute_at_ms: now,
            report_at_ms: now,
            maker_timeout_ms: 0,
            round_duration_sec: 0,
        })
        .unwrap();
    core.ome.bind_venue(core_id, venue_id.into(), now).unwrap();
    core.ome.mark_live(core_id, now).unwrap();
}

#[test]
fn repairs_missed_fill_from_rest_trade() {
    let mut core = new_core();
    live_order(&mut core, "k1", "c1", "v1", 1);
    // WS gap: no fill applied locally. Venue reports a full 10 filled.
    let snap = VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![VenueTrade {
            venue_order_id: "v1".into(),
            trade_id: "t1".into(),
            token_id: "tok".into(),
            side: Side::Buy,
            size: dec!(10),
            price: dec!(0.4),
            ts_ms: 2,
            tx_hash: None,
            maker: Some(true),
        }],
        now_ms: 3,
    };
    let report = reconcile(&mut core, &snap).unwrap();
    assert!(
        report
            .actions
            .iter()
            .any(|a| matches!(a, ReconcileAction::FilledGap { .. }))
    );
    assert_eq!(core.ome.get("c1").unwrap().status, OrderStatus::Filled);
    assert_eq!(core.ome.get("c1").unwrap().filled_size, dec!(10));
}

#[test]
fn repairs_only_the_missing_increment_of_a_partially_known_fill() {
    let mut core = new_core();
    live_order(&mut core, "k1", "c1", "v1", 1);
    // The user-WS stream already booked trade t1: 4 of the order's 10
    // shares are locally known. A SELL, so the wrong increment would
    // settle phantom sale proceeds into the ledger.
    core.ome
        .apply_fill(
            Fill {
                order_id: "c1".into(),
                trade_id: Some("t1".into()),
                token_id: "tok".into(),
                side: Side::Sell,
                price: dec!(0.5),
                size: dec!(4),
                status: FillStatus::Confirmed,
                ts_ms: 2,
                tx_hash: None,
                maker: Some(true),
                account_id: crate::model::default_account_id(),
            },
            2,
        )
        .unwrap();
    // The REST sweep reports t1 (4) plus t2 (2): the order's cumulative
    // filled size is 6 — the missing increment is 2, not 6.
    let snap = VenueSnapshot {
        open_order_ids: vec!["v1".into()],
        trades: vec![
            VenueTrade {
                venue_order_id: "v1".into(),
                trade_id: "t1".into(),
                token_id: "tok".into(),
                side: Side::Sell,
                size: dec!(4),
                price: dec!(0.5),
                ts_ms: 2,
                tx_hash: None,
                maker: Some(true),
            },
            VenueTrade {
                venue_order_id: "v1".into(),
                trade_id: "t2".into(),
                token_id: "tok".into(),
                side: Side::Sell,
                size: dec!(2),
                price: dec!(0.5),
                ts_ms: 3,
                tx_hash: None,
                maker: Some(false),
            },
        ],
        now_ms: 5,
    };
    let report = reconcile(&mut core, &snap).unwrap();
    // Exactly one gap action, carrying exactly the missing increment.
    let gaps: Vec<&ReconcileAction> = report
        .actions
        .iter()
        .filter(|a| matches!(a, ReconcileAction::FilledGap { .. }))
        .collect();
    assert_eq!(gaps.len(), 1);
    if let ReconcileAction::FilledGap { delta, .. } = gaps[0] {
        assert_eq!(delta.delta, dec!(2));
    }
    assert_eq!(core.ome.get("c1").unwrap().filled_size, dec!(6));
    // A second identical sweep stays stable (no re-application).
    let again = reconcile(&mut core, &snap).unwrap();
    assert!(
        again
            .actions
            .iter()
            .all(|a| !matches!(a, ReconcileAction::FilledGap { .. }))
    );
    assert_eq!(core.ome.get("c1").unwrap().filled_size, dec!(6));
}

#[test]
fn detects_ghost_order_without_fill() {
    let mut core = new_core();
    live_order(&mut core, "k1", "c1", "v1", 1);
    // Venue no longer lists it open and there are no trades → ghost. Order
    // is well past the grace window, so #457 applies: the KERNEL closes it
    // as ForceClosed — "we gave up waiting", not "we withdrew".
    let snap = VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![],
        now_ms: 20_000,
    };
    let report = reconcile(&mut core, &snap).unwrap();
    assert_eq!(report.suspect_ghost_ids, vec!["c1".to_string()]);
    assert_eq!(core.ome.get("c1").unwrap().status, OrderStatus::ForceClosed);
    assert!(
        report
            .actions
            .iter()
            .any(|a| matches!(a, ReconcileAction::MarkedForceClosed { .. }))
    );
}

#[test]
fn leaves_open_orders_untouched_and_is_idempotent() {
    let mut core = new_core();
    live_order(&mut core, "k1", "c1", "v1", 1);
    let snap = VenueSnapshot {
        open_order_ids: vec!["v1".into()],
        trades: vec![],
        now_ms: 2,
    };
    let r1 = reconcile(&mut core, &snap).unwrap();
    assert!(r1.actions.is_empty());
    assert_eq!(core.ome.get("c1").unwrap().status, OrderStatus::Live);
    // Idempotent: still nothing on a second identical sweep.
    let r2 = reconcile(&mut core, &snap).unwrap();
    assert!(r2.actions.is_empty());
}

#[test]
fn dry_orders_without_venue_id_are_ignored() {
    let mut core = new_core();
    core.ome
        .submit(SubmitParams {
            order_id: "dry1".into(),
            request: req("k9", dec!(10)),
            submitted_at_ms: 1,
            decision_at_ms: 1,
            execute_at_ms: 1,
            report_at_ms: 1,
            maker_timeout_ms: 0,
            round_duration_sec: 0,
        })
        .unwrap();
    core.ome.mark_live("dry1", 1).unwrap();
    let snap = VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![],
        now_ms: 2,
    };
    let report = reconcile(&mut core, &snap).unwrap();
    assert!(report.actions.is_empty());
    assert_eq!(core.ome.get("dry1").unwrap().status, OrderStatus::Live);
}

/// A venue trade on an order the OME never issued (a manual close made
/// directly on the venue) must surface as an unknown fill for the caller
/// to fold into the position book — not be silently dropped, which left
/// ghost positions the exit rules kept managing forever.
#[test]
fn manual_close_trade_surfaces_as_unknown_fill() {
    let mut core = new_core();
    live_order(&mut core, "k1", "c1", "v1", 1);
    let snap = VenueSnapshot {
        open_order_ids: vec!["v1".into()],
        trades: vec![
            // Ours: known venue id → OME gap-fill path.
            VenueTrade {
                venue_order_id: "v1".into(),
                trade_id: "t1".into(),
                token_id: "tok".into(),
                side: Side::Sell,
                size: dec!(2),
                price: dec!(0.5),
                ts_ms: 10,
                tx_hash: None,
                maker: Some(false),
            },
            // Manual: an id the OME has never heard of.
            VenueTrade {
                venue_order_id: "manual-1".into(),
                trade_id: "t2".into(),
                token_id: "tok".into(),
                side: Side::Sell,
                size: dec!(10),
                price: dec!(0.6),
                ts_ms: 20,
                tx_hash: None,
                maker: Some(false),
            },
        ],
        now_ms: 30,
    };
    let report = reconcile(&mut core, &snap).unwrap();
    assert_eq!(
        report.unknown_fills.len(),
        1,
        "exactly the manual trade is unknown"
    );
    assert_eq!(report.unknown_fills[0].venue_order_id, "manual-1");
    assert_eq!(report.unknown_fills[0].size, dec!(10));
    // The known order got its gap fill; the OME is untouched by the manual one.
    assert_eq!(core.ome.get("c1").unwrap().filled_size, dec!(2));
}

/// A sweep can deliver a trade for an order that has already ended. The
/// report must say so (issue #178) — the fill is booked, the status keeps.
#[test]
fn sweep_reports_a_fill_absorbed_by_a_terminal_order() {
    let mut core = new_core();
    live_order(&mut core, "k1", "c1", "v1", 1);
    core.ome
        .mark_terminal("c1", OrderStatus::Cancelled, 2)
        .unwrap();
    let snap = VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![VenueTrade {
            venue_order_id: "v1".into(),
            trade_id: "t-late".into(),
            token_id: "tok".into(),
            side: Side::Buy,
            size: dec!(3),
            price: dec!(0.4),
            ts_ms: 3,
            tx_hash: None,
            maker: Some(true),
        }],
        now_ms: 4,
    };
    let report = reconcile(&mut core, &snap).unwrap();
    assert_eq!(report.late_fills.len(), 1);
    let late = &report.late_fills[0];
    assert_eq!(late.order_id, "c1");
    assert_eq!(late.absorbed_by, Some(OrderStatus::Cancelled));
    assert_eq!(late.size, dec!(3));
    assert!(!late.overfill);
    assert_eq!(core.ome.get("c1").unwrap().status, OrderStatus::Cancelled);
    assert_eq!(core.ome.get("c1").unwrap().filled_size, dec!(3));
}

// ── #55: ghost terminal transitions route through Core::cancel ─────────────

/// Issue #55: a ghost order the venue already filled is closed through
/// `Core::cancel` — its #7 terminal guard, reservation release and venue-cancel
/// queue run exactly like an explicit operator cancel. The order becomes
/// `Cancelled`, never `ForceClosed`, and a second sweep over an already
/// terminal order is a no-op on state.
#[test]
fn ghost_with_fill_is_cancelled_through_core_cancel() {
    let mut core = new_core();
    live_order(&mut core, "k1", "c1", "v1", 1);
    // Venue reports a partial fill and no longer lists the order open → ghost.
    // Step 1 applies the missing increment; step 2 cancels the order.
    let snap = VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![VenueTrade {
            venue_order_id: "v1".into(),
            trade_id: "t1".into(),
            token_id: "tok".into(),
            side: Side::Buy,
            size: dec!(4),
            price: dec!(0.4),
            ts_ms: 2,
            tx_hash: None,
            maker: Some(true),
        }],
        now_ms: 20_000,
    };
    let report = reconcile(&mut core, &snap).unwrap();
    assert_eq!(core.ome.get("c1").unwrap().status, OrderStatus::Cancelled);
    assert!(
        report
            .actions
            .iter()
            .any(|a| matches!(a, ReconcileAction::MarkedCancelled { .. }))
    );
    assert!(!report.suspect_ghost_ids.contains(&"c1".to_string()));
    // Core::cancel is idempotent: re-sweeping a terminal order changes nothing.
    let _ = reconcile(&mut core, &snap).unwrap();
    assert_eq!(core.ome.get("c1").unwrap().status, OrderStatus::Cancelled);
}

/// Issue #54's release guard on `mark_terminal` applies through reconcile:
/// an already-terminal ghost order is reported but never overwritten — the
/// sweep does not call `mark_terminal` on it a second time.
#[test]
fn already_terminal_ghost_is_not_overwritten() {
    let mut core = new_core();
    live_order(&mut core, "k1", "c1", "v1", 1);
    core.ome
        .mark_terminal("c1", OrderStatus::Filled, 2)
        .unwrap();
    let snap = VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![],
        now_ms: 20_000,
    };
    let report = reconcile(&mut core, &snap).unwrap();
    assert_eq!(core.ome.get("c1").unwrap().status, OrderStatus::Filled);
    // Terminal orders are not live ghost candidates: nothing is reported,
    // nothing is overwritten (#54's guard never even gets reached).
    assert!(report.actions.is_empty());
    assert_eq!(report.suspect_ghost_ids, Vec::<String>::new());
}

// ── #189: the audit itself ───────────────────────────────────────────────

fn clean_input() -> AuditInput {
    AuditInput {
        now_ms: 1_000,
        live: false,
        identity: CashIdentity {
            balance: dec!(100),
            realized: dec!(0),
            spent: Decimal::ZERO,
            received: Decimal::ZERO,
            receivable: Decimal::ZERO,
        },
        reserved: Decimal::ZERO,
        ledger_balanced: true,
        unfunded_reserved: Decimal::ZERO,
        open_buy_notional: Decimal::ZERO,
        open_buys: 0,
        stray_reservations: Vec::new(),
        venue_free: None,
        venue_free_at_ms: None,
        ledger_realigned: None,
        realign_explained_at: None,
        open_positions: 0,
        seen_trades: 0,
        has_trade_log: true,
        venue_simulated: false,
    }
}

#[test]
fn clean_books_pass_every_check() {
    let input = clean_input();
    let r = audit_accounting(&input, None);
    assert!(r.ok, "{:?}", r.checks);
    assert!(!r.structural_failure);
    assert!(!r.halt_entries);
    assert_eq!(r.drift_usd, Decimal::ZERO);
    assert_eq!(r.anchor, None);
    // Every check is named and present (the venue leg is skipped when not live).
    let names: Vec<&str> = r.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "cash_identity",
            "reservations_match_open_buys",
            "trade_ledger_identity"
        ]
    );
}

/// The invariant #181 restores: a reservation that outlives its order (the
/// old price-improvement residue) is a STRUCTURAL failure — it must halt new
/// entries on the first audit, because a phantom hold shrinks the capital
/// every later entry is gated against.
#[test]
fn reservation_residue_is_a_structural_failure() {
    let mut input = clean_input();
    input.reserved = dec!(0.5);
    input.stray_reservations = vec![("o-gone".into(), dec!(0.5))];
    let r = audit_accounting(&input, None);
    assert!(!r.ok);
    assert!(r.structural_failure);
    assert!(r.halt_entries, "structural failures halt immediately");
    let c = r
        .checks
        .iter()
        .find(|c| c.name == "reservations_match_open_buys")
        .unwrap();
    assert_eq!(c.drift, dec!(0.5));
    assert_eq!(c.offenders, vec!["o-gone=0.5"]);
}

/// A reservation total that disagrees with the live BUY commitments is caught
/// even when every key belongs to a tracked order (a stale amount, not a
/// stale key).
#[test]
fn reservation_total_mismatch_is_caught() {
    let mut input = clean_input();
    input.open_buys = 1;
    input.open_buy_notional = dec!(4);
    input.reserved = dec!(6);
    let r = audit_accounting(&input, None);
    let c = r
        .checks
        .iter()
        .find(|c| c.name == "reservations_match_open_buys")
        .unwrap();
    assert!(!c.ok);
    assert_eq!(c.drift, dec!(2));
}

/// The anchored cash identity: the trade records explain every movement of
/// the ledger since the anchor. Cash that moved with no trade behind it — the
/// classic missed fill, or the venue quietly healing a balance — shows up as
/// the residual.
#[test]
fn anchored_identity_catches_unexplained_cash() {
    let anchor = CashIdentity {
        balance: dec!(100),
        realized: dec!(0),
        spent: Decimal::ZERO,
        received: Decimal::ZERO,
        receivable: Decimal::ZERO,
    };
    // Since the anchor: a closed trade realized +4 (its entry cost and exit
    // proceeds live INSIDE `realized`; the two flow sums below belong to
    // positions still open — one paid 6 to enter, one partial exit returned
    // 10). 100 + 4 - 6 + 10 = 108.
    let mut input = clean_input();
    input.identity = CashIdentity {
        balance: dec!(108),
        realized: dec!(4),
        spent: dec!(6),
        received: dec!(10),
        receivable: Decimal::ZERO,
    };
    let r = audit_accounting(&input, Some(&anchor));
    assert!(r.ok, "{:?}", r.checks);

    // Now 5 USD leaves the ledger with no trade to explain it.
    input.identity.balance = dec!(103);
    let r = audit_accounting(&input, Some(&anchor));
    assert!(!r.ok);
    let c = r
        .checks
        .iter()
        .find(|c| c.name == "trade_ledger_identity")
        .unwrap();
    assert_eq!(c.expected, dec!(108));
    assert_eq!(c.actual, dec!(103));
    assert_eq!(c.drift, dec!(-5));
    assert_eq!(r.drift_usd, dec!(5));
    // Both records are local and read under one lock, so there is no fetch
    // race to forgive: a local inconsistency halts on the first audit.
    assert!(c.structural);
    assert!(r.structural_failure);
    assert!(r.halt_entries);
}

/// Without a trade log there is no record of realized PnL, so the trade leg
/// is explicitly not covered instead of reported as drift — otherwise every
/// close in a logless replay would look like cash that vanished.
#[test]
fn trade_leg_is_skipped_without_a_trade_log() {
    let anchor = CashIdentity {
        balance: dec!(100),
        realized: dec!(0),
        spent: Decimal::ZERO,
        received: Decimal::ZERO,
        receivable: Decimal::ZERO,
    };
    let mut input = clean_input();
    input.has_trade_log = false;
    // A close that moved cash but could never have been recorded.
    input.identity = CashIdentity {
        balance: dec!(104),
        realized: dec!(0),
        spent: Decimal::ZERO,
        received: Decimal::ZERO,
        receivable: Decimal::ZERO,
    };
    let r = audit_accounting(&input, Some(&anchor));
    assert!(r.ok, "{:?}", r.checks);
    let c = r
        .checks
        .iter()
        .find(|c| c.name == "trade_ledger_identity")
        .unwrap();
    assert!(c.detail.contains("not covered"));
}

/// The venue leg compares like with like: the ledger's free cash against the
/// venue's free cash, and it is only evaluated for a live core that has
/// actually reported a balance.
#[test]
fn venue_leg_compares_free_cash_when_live() {
    let mut input = clean_input();
    input.live = true;
    input.identity.balance = dec!(100);
    input.reserved = dec!(40);
    input.open_buys = 1;
    input.open_buy_notional = dec!(40);
    // Not reported yet: nothing to compare, no failure.
    let r = audit_accounting(&input, None);
    assert!(r.ok);
    assert!(r.checks.iter().any(|c| c.name == "venue_free_cash"
        && c.ok
        && c.detail.contains("no venue balance reported")));
    // Reported and matching (free = balance − reserved).
    input.venue_free = Some(dec!(60));
    input.venue_free_at_ms = Some(900);
    let r = audit_accounting(&input, None);
    assert!(r.ok, "{:?}", r.checks);
    // A cent of venue rounding is not drift: the two sides are computed by
    // different parties, so this leg carries its own (2-cent) tolerance.
    input.venue_free = Some(dec!(60.01));
    let r = audit_accounting(&input, None);
    assert!(r.ok, "{:?}", r.checks);
    // Reported and short by 5: cross-source failure, named with its drift.
    input.venue_free = Some(dec!(55));
    let r = audit_accounting(&input, None);
    assert!(!r.ok);
    let c = r
        .checks
        .iter()
        .find(|c| c.name == "venue_free_cash")
        .unwrap();
    assert_eq!(c.drift, dec!(-5));
    assert!(!c.structural);
    assert!(!r.structural_failure);
}

/// A SIMULATED venue's free-cash leg is ABSENT, not failing: the paper site
/// freezes its own reported free cash while a BUY rests and the ledger's
/// `balance − reserved` nets the commitment out, so the two figures disagree
/// by fee-level cents forever — the leg would falsely fail every audit and
/// two strikes halted new entries on a venue where nothing drifted. The
/// other checks still run; a real venue (`venue_simulated = false`) keeps
/// the leg.
#[test]
fn venue_leg_is_skipped_for_a_simulated_venue() {
    let mut input = clean_input();
    input.live = true;
    input.venue_simulated = true;
    input.identity.balance = dec!(93.558);
    input.reserved = dec!(4.35);
    input.open_buys = 1;
    input.open_buy_notional = dec!(4.35);
    // The paper site's frozen figure: balance minus what IT held for the
    // resting order, plus a fee-level cent it rounded away.
    input.venue_free = Some(dec!(89.11));
    input.venue_free_at_ms = Some(900);
    let r = audit_accounting(&input, None);
    assert!(
        r.ok,
        "the simulated-venue leg must not fail the audit: {:?}",
        r.checks
    );
    assert!(!r.halt_entries);
    assert!(
        !r.checks.iter().any(|c| c.name == "venue_free_cash"),
        "the leg is absent (skipped, not covered): {:?}",
        r.checks
    );

    // The same books on a REAL venue keep the leg — and its drift.
    input.venue_simulated = false;
    let r = audit_accounting(&input, None);
    assert!(!r.ok, "a real venue's 92-cent gap is still drift");
    let c = r
        .checks
        .iter()
        .find(|c| c.name == "venue_free_cash")
        .expect("the leg must be present for a real venue");
    let expected_drift = dec!(89.11) + dec!(4.35) - dec!(93.558);
    assert_eq!(c.drift, expected_drift);
}

/// A residual the VENUE itself created (the host sets the ledger's cash from
/// the venue's free cash when nothing rests) is not a local inconsistency: it
/// is downgraded to a cross-source failure — reported and persisted, but
/// halting on a repeat instead of on the first audit. An EXPLAINED residual is
/// still a failure: the trade record disagrees with the money.
#[test]
fn a_venue_correction_downgrades_the_trade_leg_to_cross_source() {
    let anchor = CashIdentity {
        balance: dec!(100),
        realized: dec!(0),
        spent: Decimal::ZERO,
        received: Decimal::ZERO,
        receivable: Decimal::ZERO,
    };
    let mut input = clean_input();
    // Cash is 5 short of what the trade records explain…
    input.identity.balance = dec!(95);
    let r = audit_accounting(&input, Some(&anchor));
    let c = |r: &AuditReport| {
        r.checks
            .iter()
            .find(|c| c.name == "trade_ledger_identity")
            .unwrap()
            .clone()
    };
    // …unexplained: structural, halts at once.
    assert!(!r.ok);
    assert!(r.structural_failure);
    assert!(c(&r).structural);

    // The same 5 USD, but the host set the ledger from the venue by exactly
    // that amount — fresh (now 1000, correction at 900): the correcting
    // party is external, so it is cross-source, and the excuse is CLAIMED
    // for this audit (issue #921): the report carries the attribution.
    input.now_ms = 1000;
    input.ledger_realigned = Some((900, dec!(-5)));
    let r = audit_accounting(&input, Some(&anchor));
    assert!(
        !r.ok,
        "still a failure: the trade record disagrees with the cash"
    );
    assert!(!r.structural_failure);
    assert!(!c(&r).structural);
    assert_eq!(c(&r).drift, dec!(-5));
    assert!(c(&r).detail.contains("venue's own ledger correction"));
    assert_eq!(r.realign_claimed_at, Some(900));

    // A realignment of a DIFFERENT size does not explain it.
    input.ledger_realigned = Some((900, dec!(-3)));
    let r = audit_accounting(&input, Some(&anchor));
    assert!(r.structural_failure);
    assert!(c(&r).structural);
    assert_eq!(r.realign_claimed_at, None);
}

/// Issue #921: the venue's own correction is a ONE-SHOT excuse. The first
/// audit that matches a residual to it claims the attribution (cross-source
/// downgrade, no instant halt); the NEXT audit still seeing the residual
/// gets the excuse REFUSED — the check comes back STRUCTURAL, entries halt
/// without waiting for a second cross-source strike that the standing
/// excuse would keep absorbing. A REFUSED excused-matcher is loud in the
/// detail text.
#[test]
fn a_venue_correction_explains_a_residual_exactly_once() {
    let anchor = CashIdentity {
        balance: dec!(100),
        realized: dec!(0),
        spent: Decimal::ZERO,
        received: Decimal::ZERO,
        receivable: Decimal::ZERO,
    };
    let mut input = clean_input();
    input.identity.balance = dec!(95);
    input.now_ms = 1000;
    input.ledger_realigned = Some((900, dec!(-5)));

    // First audit: explained, cross-source, attribution carried out.
    let r = audit_accounting(&input, Some(&anchor));
    assert!(!r.structural_failure);
    assert_eq!(r.realign_claimed_at, Some(900));

    // Next audit (same books, same correction — nothing overwrote it): the
    // excuse is spent. The residual is UNEXPLAINED → structural.
    input.realign_explained_at = Some(1000);
    let r = audit_accounting(&input, Some(&anchor));
    let c = r
        .checks
        .iter()
        .find(|c| c.name == "trade_ledger_identity")
        .unwrap();
    assert!(r.structural_failure, "{:?}", c.detail);
    assert!(c.structural);
    assert!(c.detail.contains("REFUSED"));
    assert!(c.detail.contains("already spent"));
    assert_eq!(r.realign_claimed_at, None);
}

/// Issue #921: a correction older than the fresh window cannot explain a
/// residual either — the venue reports on every sweep, so a standing truth
/// is always re-stamped in practice; only the snapshot kept by the
/// `last_realign` slot grows stale, and that snapshot must not act as a
/// permanent green light.
#[test]
fn a_stale_correction_cannot_explain_a_residual() {
    let anchor = CashIdentity {
        balance: dec!(100),
        realized: dec!(0),
        spent: Decimal::ZERO,
        received: Decimal::ZERO,
        receivable: Decimal::ZERO,
    };
    let mut input = clean_input();
    input.identity.balance = dec!(95);
    // Correction older than the TTL.
    input.now_ms = 900 + REALIGN_EXPLAIN_TTL_MS + 1;
    input.ledger_realigned = Some((900, dec!(-5)));
    let r = audit_accounting(&input, Some(&anchor));
    let c = r
        .checks
        .iter()
        .find(|c| c.name == "trade_ledger_identity")
        .unwrap();
    assert!(r.structural_failure);
    assert!(c.structural);
    assert!(c.detail.contains("REFUSED"));
    assert!(c.detail.contains("stale"));
    assert_eq!(r.realign_claimed_at, None);
}

/// A restarted core has no anchor: the first audit reports the structural
/// checks and says the cash identity is not yet defined, rather than
/// inventing a baseline.
#[test]
fn first_audit_defers_the_anchored_check() {
    let input = clean_input();
    let r = audit_accounting(&input, None);
    let c = r
        .checks
        .iter()
        .find(|c| c.name == "trade_ledger_identity")
        .unwrap();
    assert!(c.ok);
    assert!(c.detail.contains("anchor pending"));
    assert_eq!(r.anchor, None);
    // With an anchor the same books still pass and the anchor is echoed back.
    let anchor = input.identity;
    let r = audit_accounting(&input, Some(&anchor));
    assert!(r.ok);
    assert_eq!(r.anchor, Some(anchor));
}

/// A ledger whose reserved exceeds its balance fails the local identity.
#[test]
fn unbalanced_ledger_is_structural() {
    let mut input = clean_input();
    input.ledger_balanced = false;
    input.unfunded_reserved = dec!(10);
    let r = audit_accounting(&input, None);
    assert!(r.structural_failure);
    assert!(r.halt_entries);
    let c = r.checks.iter().find(|c| c.name == "cash_identity").unwrap();
    assert!(!c.ok);
    assert_eq!(c.drift, dec!(10));
}
