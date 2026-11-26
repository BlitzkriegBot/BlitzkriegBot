#![cfg(test)]

use super::*;
use rust_decimal_macros::dec;

/// One minimal closed-trade row for the pagination fixtures: every field the
/// carrier's payload round-trip needs, and nothing the test does not assert on.
fn bounds_row(i: usize) -> crate::position::ClosedPosition {
    crate::position::ClosedPosition {
        id: format!("t{i}"),
        strategy: "bounds".into(),
        asset: "BTC".into(),
        direction: crate::model::SignalDirection::Up,
        token_id: "tok".into(),
        condition_id: "cond".into(),
        entry_price: dec!(0.4),
        exit_price: dec!(0.5),
        shares: dec!(1),
        cost_usd: dec!(0.4),
        was_maker_entry: true,
        was_maker_exit: false,
        entry_fee_pct: rust_decimal::Decimal::ZERO,
        exit_fee_pct: rust_decimal::Decimal::ZERO,
        maker_rebate_usd: rust_decimal::Decimal::ZERO,
        pnl_usd: dec!(0.1),
        pnl_pct: dec!(100),
        net_pnl_usd: dec!(0.1),
        net_pnl_pct: dec!(100),
        high_pnl_pct: rust_decimal::Decimal::ZERO,
        low_pnl_pct: rust_decimal::Decimal::ZERO,
        hold_time_sec: 0,
        exit_reason: crate::model::ExitReason::TakeProfit,
        entered_at_ms: 0,
        exited_at_ms: i as i64 + 1,
        entry_role: crate::model::OrderRole::Maker,
        exit_role: crate::model::OrderRole::Taker,
        dust_shares: rust_decimal::Decimal::ZERO,
        account_id: crate::model::default_account_id(),
        venue: String::new(),
        source: String::new(),
        original_fallback_price: None,
        price_source: String::new(),
    }
}

// ── #186: the read verbs answer pages, never the whole table ─────────────
//
// The defect this pins: `trades.history`'s `limit` went straight to
// `trade_db::read_page`, where `0` means UNCAPPED, and `orders.list` had no
// parameter at all. Both tables only grow (the trade log is append-only; the
// order table is "one row per order ever placed, never removed"), so an
// unbounded reply is a read whose cost climbs for the life of the process.
// Three of the UI's snapshot embedders passed that `0`.

use crate::ipc::server::handlers::trades::{
    TRADES_DEFAULT_LIMIT, TRADES_MAX_LIMIT, trades_page_limit,
};
use crate::service::{ORDERS_DEFAULT_LIMIT, orders_page_keeps, orders_page_window};

/// `limit: 0` is "no page size was asked for", not "give me everything".
/// This is the whole fix: a zero must never buy an unbounded read.
#[test]
fn trades_limit_zero_is_the_default_page_not_uncapped() {
    assert_eq!(
        trades_page_limit(&serde_json::json!({ "limit": 0 })),
        TRADES_DEFAULT_LIMIT,
        "0 must resolve to the default page"
    );
    assert_eq!(
        trades_page_limit(&serde_json::json!({})),
        TRADES_DEFAULT_LIMIT,
        "an absent limit keeps the historical default"
    );
}

/// An explicit in-range ask is honoured verbatim — the fix bounds the reply,
/// it does not shrink what callers already asked for.
#[test]
fn trades_limit_within_bounds_is_honoured() {
    assert_eq!(trades_page_limit(&serde_json::json!({ "limit": 200 })), 200);
    assert_eq!(trades_page_limit(&serde_json::json!({ "limit": 1 })), 1);
    assert_eq!(
        trades_page_limit(&serde_json::json!({ "limit": TRADES_MAX_LIMIT })),
        TRADES_MAX_LIMIT,
        "the ceiling itself is a legal ask"
    );
}

/// Above the ceiling is clamped, not refused: the sibling read verbs
/// (`account.journal` clamps to 500, `kline.history` to 1,000) answer a
/// bounded page rather than teaching callers a new error code.
#[test]
fn trades_limit_above_the_ceiling_is_clamped() {
    assert_eq!(
        trades_page_limit(&serde_json::json!({ "limit": u64::MAX })),
        TRADES_MAX_LIMIT,
        "u64::MAX must land on the ceiling"
    );
    assert_eq!(
        trades_page_limit(&serde_json::json!({ "limit": TRADES_MAX_LIMIT + 1 })),
        TRADES_MAX_LIMIT
    );
}

/// The window maths, pinned directly: cut from the newest (tail) end, offset
/// walking back, `0` uncapped, offset past the oldest row an empty window.
#[test]
fn orders_page_window_cuts_from_the_newest_end() {
    assert_eq!(orders_page_window(100, 10, 0), (90, 100), "newest page");
    assert_eq!(
        orders_page_window(100, 10, 10),
        (80, 90),
        "offset 10 = the page BEHIND the newest 10"
    );
    assert_eq!(
        orders_page_window(100, 0, 0),
        (0, 100),
        "limit 0 stays uncapped in-process"
    );
    assert_eq!(
        orders_page_window(100, 10, 95),
        (0, 5),
        "a window running off the oldest end is short, not an error"
    );
    assert_eq!(
        orders_page_window(100, 10, 100),
        (0, 0),
        "offset past the oldest row is an empty page"
    );
}

/// Live orders are never paged out. `orders.force_close` only accepts a live
/// order, so a ghost stuck since a venue outage is exactly the old row a
/// newest-first window drops first — hiding the row the operator came to act
/// on. Terminal rows page; live rows always answer.
#[test]
fn a_live_order_survives_a_window_that_excludes_its_age() {
    let window = orders_page_window(4, 2, 0); // newest two rows
    assert_eq!(window, (2, 4), "the window itself excludes index 0..2");
    assert!(
        !orders_page_keeps(0, window, false),
        "an old TERMINAL row is paged out"
    );
    assert!(
        orders_page_keeps(0, window, true),
        "the same old row, still LIVE, must survive the page"
    );
    assert!(orders_page_keeps(3, window, false), "a windowed row stays");
}

/// Paging a long table must not silently shrink the answer: the size of the
/// whole table rides with the page.
#[test]
fn the_page_reports_the_full_table_size_not_its_own_length() {
    let total = ORDERS_DEFAULT_LIMIT + 250;
    let window = orders_page_window(total, ORDERS_DEFAULT_LIMIT, 0);
    let page_len = window.1 - window.0;
    assert_eq!(
        page_len, ORDERS_DEFAULT_LIMIT,
        "the page is one default page long"
    );
    assert_ne!(
        page_len, total,
        "and is therefore NOT the whole table — which is why `total` must ride along"
    );
}

/// At the wire: `trades.history {limit: 0}` over a book holding more rows than
/// one default page must answer ONE page. This is the regression that the
/// three `snapshot(0)` embedders hit on every poll.
///
/// The rows are seeded into the SQL book the core was assembled with (schema
/// v4) rather than into a JSONL file: the page limit is a property of the WIRE
/// verb, and the point of the test is that it holds no matter how many rows
/// the carrier holds — so the carrier has to actually hold them.
#[tokio::test]
async fn trades_history_at_the_wire_answers_one_page_for_limit_zero() {
    let dir = std::env::temp_dir().join(format!(
        "bk-trades-bounds-{}-{}",
        std::process::id(),
        now_ms()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let (core, registry, peer) = hot_reload_fixture_with_db(&dir).await;
    let total = TRADES_DEFAULT_LIMIT + 1;
    {
        let c = core.lock().await;
        let log = c
            .trade_log()
            .expect("the fixture wires a store, so the ledger must exist");
        for i in 0..total {
            log.append(&crate::trade::TradeRecord::from_closed(&bounds_row(i)))
                .expect("seed row lands");
        }
    }

    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"trades.history","params":{"limit":0}}"#.to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the verb must answer: {reply}"
    );
    assert_eq!(
        reply["result"]["trades"].as_array().map(|a| a.len()),
        Some(TRADES_DEFAULT_LIMIT),
        "limit:0 must answer ONE default page, not the whole book: {reply}"
    );
    assert_eq!(
        reply["result"]["trades"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|t| t["id"].as_str()),
        Some("t1"),
        "the page is the OLDEST window of the newest-first read"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// At the wire: `orders.list` carries both the page and the table size, so a
/// caller can tell a complete answer from a truncated one.
#[tokio::test]
async fn orders_list_at_the_wire_carries_the_page_and_the_table_size() {
    let (core, registry, peer) = hot_reload_fixture().await;
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"orders.list","params":{}}"#.to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the verb must answer: {reply}"
    );
    // A fresh core has no orders; the shape still carries both fields, so a
    // caller can tell "empty table" from "field absent" (older core).
    assert_eq!(
        reply["result"]["orders"].as_array().map(|a| a.len()),
        Some(0)
    );
    assert_eq!(
        reply["result"]["total"],
        serde_json::json!(0),
        "the size rides with the page even when the table is empty"
    );
}
