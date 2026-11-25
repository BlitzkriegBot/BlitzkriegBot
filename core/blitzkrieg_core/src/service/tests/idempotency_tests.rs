#![cfg(test)]
//! #185 falsification tests: the durable order-intent gate.
//!
//! The five unit tests in `db::intents` prove the LEDGER behaves; these prove
//! the KERNEL consults it. That distinction is the whole issue: idempotency was
//! memory state, so the defect only appears across a cancel/reject plus a
//! reload/restart inside one round window. A ledger that the placement path
//! never reads would pass every unit test and fix nothing.
//!
//! Every test here uses a REAL `Core` wired to a REAL SQLite file
//! (`db_path: Some(..)`) with the real reservation/OME path — no mock stands in
//! for the duplicate guard, because the guard is exactly what is under test.
//!
//! # Why the store-less case is asserted, not just mentioned
//!
//! `Core::new` without `db_path` has no ledger and `intent_gate` answers
//! `Open`. That is a REAL boundary (the backtester and most in-process tests),
//! so one test pins it: a duplicate there must still be caught by the OME's
//! live-key dedup, and the test says so instead of implying durability exists.

use super::*;
use crate::model::OrderStatus;
use rust_decimal_macros::dec;

/// A Core with a real on-disk store, so `intent_ledger` is `Some`. The DB lives
/// under a per-test temp dir that the caller removes.
fn stored_core(root: &std::path::Path, config: CoreConfig) -> Core {
    std::fs::create_dir_all(root).expect("temp root");
    Core::new(CoreConfig {
        db_path: Some(root.join("store.sqlite")),
        dry_seed_balance: dec!(1000),
        round_duration_sec: 900,
        auto_exits_enabled: false,
        ..config
    })
}

fn buy(strategy: &str, condition: &str, slot: i64, direction: &str, key: &str) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: condition.into(),
        side: Side::Buy,
        mode: FillPolicy::Maker,
        price: dec!(0.40),
        size: dec!(5),
        internal_key: key.into(),
        strategy: strategy.into(),
        asset: "BTC".into(),
        direction: direction.into(),
        round_slot: slot,
        account_id: crate::model::default_account_id(),
    }
}

/// SPEC TEST 1 — 同 `(account, strategy, condition, slot, direction)` 连提两次：
/// 第二次必须返回**既有 order id**，且账本/OME 里只有一单。
///
/// This is the headline contract. Note the two submits use DIFFERENT
/// `internal_key`s, because that is what the real defect looks like: the Lua
/// package's own `quoted` table is what used to supply the key, and it is empty
/// after a reload. If the gate only deduped on `internal_key` this test fails.
#[test]
fn two_submits_same_intent_key_yield_one_row() {
    let root = std::env::temp_dir().join(format!("bk-185-dup-{}", std::process::id()));
    let mut c = stored_core(&root, CoreConfig::default());

    let first = c
        .place(
            buy("late_favorite", "cond-185", 1_795_230_000, "up", "quote-a"),
            0,
            1_000,
        )
        .expect("the first submit places");
    let (first_id, first_status) = first;

    // A DIFFERENT internal key, same intent identity — the reload/cancel shape.
    let second = c
        .place(
            buy("late_favorite", "cond-185", 1_795_230_000, "up", "quote-b"),
            0,
            1_100,
        )
        .expect("the duplicate is answered, not refused with an error");

    assert_eq!(
        second.0, first_id,
        "the duplicate must replay the FIRST order id, never mint a second"
    );
    assert_eq!(
        second.1, first_status,
        "the duplicate replays the first submit's status byte-for-byte"
    );

    // The order book and the intent ledger each hold exactly one row.
    let live_ids: Vec<String> = c.ome().all().iter().map(|o| o.order_id.clone()).collect();
    assert_eq!(
        live_ids,
        vec![first_id.clone()],
        "exactly one order exists in the OME: {live_ids:?}"
    );
    let rows = c
        .intent_ledger
        .as_ref()
        .expect("a store-wired core has the ledger")
        .len()
        .expect("intent rows readable");
    assert_eq!(rows, 1, "exactly one intent row for one intent identity");

    let _ = std::fs::remove_dir_all(&root);
}

/// SPEC TEST 2 — 撤单后同窗口再评估：不得再发第二单。
///
/// The ladder the issue describes is `cancel → reload → re-evaluate`. The
/// reload itself is exercised elsewhere (the Lua process tables are the thing
/// that empties); what the GATE must guarantee is the part the reload cannot:
/// the intent identity is still claimed after the first order went terminal.
/// The first order is cancelled for real (`Core::cancel`), which releases the
/// OME's live key — the historical hole — and the second submit must still be
/// answered from the ledger.
#[test]
fn cancel_then_resubmit_inside_the_window_is_replayed() {
    let root = std::env::temp_dir().join(format!("bk-185-cancel-{}", std::process::id()));
    let mut c = stored_core(&root, CoreConfig::default());

    let (first_id, _) = c
        .place(
            buy("late_favorite", "cond-185c", 1_795_230_100, "up", "q1"),
            0,
            2_000,
        )
        .expect("first submit");

    // Terminal-ize the order the way the defect does: cancel it, so
    // `Ome::has_pending` no longer sees the key (its `is_live()` gate).
    c.cancel(&first_id, 2_100).expect("cancel lands");
    assert!(
        c.ome().get(&first_id).is_some(),
        "precondition: the cancelled order is still in the OME as terminal"
    );
    assert_eq!(
        c.ome().get(&first_id).map(|o| o.status),
        Some(OrderStatus::Cancelled),
        "precondition: the OME's live key is free again — this is the hole"
    );

    // Same intent, fresh internal key (post-reload), same round window.
    let second = c
        .place(
            buy("late_favorite", "cond-185c", 1_795_230_100, "up", "q2"),
            0,
            2_200,
        )
        .expect("the duplicate is answered from the ledger");

    assert_eq!(
        second.0, first_id,
        "after a cancel the intent must still replay the first order, not place a second"
    );
    let rows = c
        .intent_ledger
        .as_ref()
        .expect("ledger")
        .len()
        .expect("rows");
    assert_eq!(
        rows, 1,
        "one intent, one row — the cancel did not free the row"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// SPEC TEST 3 — 终态订单 + 重建 Core（重启）后同槽必须被拒。
///
/// This is the durability half and the reason a `HashMap` cannot do the job:
/// a NEW `Core` over the SAME database file must still see the claim. The test
/// builds the second core from the same `db_path`, exactly like a process
/// restart, and asserts the duplicate replays the FIRST process's order id.
#[test]
fn restart_after_terminal_same_slot_is_refused() {
    let root = std::env::temp_dir().join(format!("bk-185-restart-{}", std::process::id()));

    let (first_id, first_status) = {
        let mut c = stored_core(&root, CoreConfig::default());
        let (id, status) = c
            .place(
                buy("late_favorite", "cond-185r", 1_795_230_200, "up", "q1"),
                0,
                3_000,
            )
            .expect("first submit");
        c.cancel(&id, 3_100).expect("cancel");
        (id, status)
    };

    // A fresh process over the same store: nothing in memory survives.
    let mut restarted = stored_core(&root, CoreConfig::default());
    let rows = restarted
        .intent_ledger
        .as_ref()
        .expect("the restarted core reads the same ledger")
        .len()
        .expect("rows readable");
    assert_eq!(rows, 1, "the intent row survived the restart");

    let second = restarted
        .place(
            buy(
                "late_favorite",
                "cond-185r",
                1_795_230_200,
                "up",
                "q2-after-restart",
            ),
            0,
            3_200,
        )
        .expect("the duplicate is answered from the durable row");

    assert_eq!(
        second.0, first_id,
        "a restart must not free the intent: the duplicate replays the first order id"
    );
    assert_eq!(second.1, first_status, "and the same status");

    let _ = std::fs::remove_dir_all(&root);
}

/// A CLOSE must never be gated — trapping exposure is far worse than the
/// duplicate this ledger prevents.
///
/// The gate's own doc says closes and escalations are excluded; this pins the
/// exit half with a real sell against a real book, because a stale intent row
/// silently blocking an exit would be the worst possible regression to ship.
#[test]
fn closes_are_never_gated_by_a_stale_intent() {
    let root = std::env::temp_dir().join(format!("bk-185-close-{}", std::process::id()));
    let mut c = stored_core(&root, CoreConfig::default());

    // Open a real position so there is something to close.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    c.place(
        OrderRequest {
            mode: FillPolicy::Taker,
            ..buy("late_favorite", "cond-185x", 1_795_230_300, "up", "entry")
        },
        0,
        1,
    )
    .expect("entry fills");

    // The exit is a SELL with a close-intent key. If the gate inspected it, the
    // entry's row (same condition/slot/direction) would answer it.
    let exit = OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond-185x".into(),
        side: Side::Sell,
        mode: FillPolicy::Taker,
        price: dec!(0.40),
        size: dec!(5),
        internal_key: "exit:cond-185x".into(),
        strategy: "late_favorite".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1_795_230_300,
        account_id: crate::model::default_account_id(),
    };
    let before = c.ome().all().len();
    let (_id, status) = c.place(exit, 0, 2).expect("the close must be allowed");
    assert_ne!(
        status,
        OrderStatus::Pending,
        "the close reached the matcher (not answered by an intent replay)"
    );
    assert!(
        c.ome().all().len() > before,
        "the close minted its own order instead of replaying the entry's"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// SPEC-ADJACENT — 无 store 的 Core 没有意图账本，`intent_gate` 必须如实
/// 返回 `Open`，此时唯一守卫是 OME 的 live-key 去重（行为与修复前一致）。
///
/// Pinned because the boundary is load-bearing: a kernel shape that silently
/// pretended to persist would be the L4 class the rules forbid. The assertion
/// is that the SECOND submit is refused by the OME's own live-key guard, which
/// proves the gate really was open (not silently claiming).
#[test]
fn store_less_core_has_no_ledger_and_the_ome_guard_is_the_only_gate() {
    let mut c = dry_core(dec!(10));
    assert!(
        c.intent_ledger.is_none(),
        "a core without db_path must not have an intent ledger"
    );

    let (id, _) = c
        .place(
            buy(
                "late_favorite",
                "cond-185s",
                1_795_230_400,
                "up",
                "same-key",
            ),
            0,
            1,
        )
        .expect("first submit");

    // Same internal key, still live: the OME's dedup refuses it (unchanged
    // pre-#185 behaviour). The message names the key, proving it came from the
    // OME guard and not from an intent replay.
    let err = c
        .place(
            buy(
                "late_favorite",
                "cond-185s",
                1_795_230_400,
                "up",
                "same-key",
            ),
            0,
            2,
        )
        .expect_err("the OME's live-key dedup must refuse the duplicate");
    assert!(
        err.message.contains("same-key"),
        "the refusal comes from the OME's live-key guard: {}",
        err.message
    );
    assert_eq!(
        c.ome().all().len(),
        1,
        "one order — the store-less shape behaves exactly as before (#185 is a store property)"
    );
    assert!(c.ome().get(&id).is_some());
}
