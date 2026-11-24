#![cfg(test)]

//! Release correctness for terminal transitions (issue #54:
//! `ome.mark_terminal` release 版无源状态校验, 终态可被静默覆写).
//!
//! When a BUY order reaches a terminal state, the notional released from the
//! ledger must equal the order's own outstanding commitment
//! (`price * remaining`) — never more (cash created out of thin air) and
//! never less (a stray hold the audit halts on) — and the transition must
//! abort with `Err` instead of completing silently.
//!
//! #184 changed WHEN that release happens: on a live venue the reservation is
//! held until the venue acknowledges the cancel
//! ([`Core::confirm_venue_cancel`]), because a fill can still land in the
//! window. These tests therefore drive the release through the confirm, not
//! through `cancel` — the invariants they pin are unchanged and, if anything,
//! now guard the only path that can legitimately move the money.

use super::*;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn buy_order(price: Decimal, size: Decimal, key: &str) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Maker,
        price,
        size,
        internal_key: key.into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: default_account_id(),
    }
}

fn acct() -> AccountId {
    default_account_id()
}

#[test]
fn release_ok_releases_exactly_outstanding_commitment() {
    let mut c = live_core(dec!(10));
    let (id, st) = c.place(buy_order(dec!(0.40), dec!(5), "r1"), 0, 1).unwrap();
    assert_eq!(st, OrderStatus::Pending);

    let ledger = c.accounts.get_mut(&acct()).unwrap();
    assert_eq!(ledger.reserved(), dec!(2)); // 0.40 x 5
    assert_eq!(ledger.available(), dec!(8));

    // A venue id is what makes a confirm possible: the release is driven by it.
    c.bind_venue(&id, "v-r1".into(), 1).unwrap();
    c.cancel(&id, 2).unwrap();

    // #184: the withdrawal is in flight — NOT terminal, NOT released.
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Live);
    assert!(c.ome().get(&id).unwrap().cancel_requested);
    assert_eq!(c.accounts.get_mut(&acct()).unwrap().reserved(), dec!(2));

    c.confirm_venue_cancel(&id, "v-r1", 3).unwrap();

    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Cancelled);
    let ledger = c.accounts.get_mut(&acct()).unwrap();
    assert_eq!(ledger.reserved(), dec!(0));
    assert_eq!(ledger.available(), dec!(10)); // full refund, no residue
    assert!(ledger.is_balanced());
}

#[test]
fn release_amount_mismatch_is_err() {
    let mut c = live_core(dec!(10));
    let (id, _) = c.place(buy_order(dec!(0.40), dec!(5), "r2"), 0, 1).unwrap();
    c.bind_venue(&id, "v-r2".into(), 1).unwrap();
    // Ledger drift: the table no longer matches the order's commitment.
    c.accounts
        .get_mut(&acct())
        .unwrap()
        .sync_buy_reservation(&id, dec!(7));

    let err = c.confirm_venue_cancel(&id, "v-r2", 2).unwrap_err();
    assert_eq!(err.code, CoreErrorCode::InvalidParams);
}

#[test]
fn release_under_release_is_explicit_err() {
    let mut c = live_core(dec!(10));
    let (id, _) = c.place(buy_order(dec!(0.40), dec!(5), "r3"), 0, 1).unwrap();
    c.bind_venue(&id, "v-r3".into(), 1).unwrap();
    // The ledger still holds 1.00 while the order commits 2.00 (stale fill
    // settlement drew the rest): releasing returns 1.00, not 2.00.
    c.accounts
        .get_mut(&acct())
        .unwrap()
        .sync_buy_reservation(&id, dec!(1));

    let err = c.confirm_venue_cancel(&id, "v-r3", 2).unwrap_err();
    assert_eq!(err.code, CoreErrorCode::InvalidParams);
}

#[test]
fn mark_terminal_rejects_non_terminal_status() {
    let mut c = live_core(dec!(10));
    let (id, _) = c.place(buy_order(dec!(0.40), dec!(5), "r4"), 0, 1).unwrap();

    let err = c.ome.mark_terminal(&id, OrderStatus::Live, 2).unwrap_err();
    assert_eq!(err.code, CoreErrorCode::InvalidOrderState);

    // The order is untouched.
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Pending);
}

/// #184: the evidence-free stamp refuses `PlaceNeverAccepted{Unknown}` — a
/// probe with no answer is not evidence of absence.
#[test]
fn unknown_probe_is_refused_as_terminal_evidence() {
    let mut c = live_core(dec!(10));
    let (id, _) = c.place(buy_order(dec!(0.40), dec!(5), "r5"), 0, 1).unwrap();

    let err = c
        .ome
        .mark_terminal_with_evidence(
            &id,
            OrderStatus::Cancelled,
            crate::model::TerminalEvidenceSource::PlaceNeverAccepted {
                probe: crate::model::ProbeOutcome::Unknown,
            },
            2,
        )
        .unwrap_err();
    assert_eq!(err.code, CoreErrorCode::InvalidOrderState);
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Pending);
    assert!(c.ome().get(&id).unwrap().terminal_evidence.is_none());
}
