#![cfg(test)]

//! #184 · OME 订单状态机的终态证据（falsification tests）
//!
//! 规格 `dev/tmp/gitea-issues-v043/p1-4-ome-fsm.md` 要求三条证伪判据。它们
//! 检查的不是"代码做了什么"，而是"没有证据时它绝不做什么"：
//!
//! 1. [`cancel_keeps_reservation_until_venue_cancel_ack`] — 撤单入队 ≠ venue
//!    已撤。在 venue 确认之前，预留必须仍在；确认之后迟到的成交仍被吸收，且
//!    不产生第二次扣款。
//! 2. [`place_timeout_probes_and_never_locally_cancels_without_evidence`] —
//!    POST 超时不能证明 venue 没有这张单。订单必须留在 live、预留不动、系统
//!    上报 Unknown。
//! 3. [`ghost_filled_release_error_aborts_instead_of_filling`] — 释放失败必须
//!    中止，不能吞掉错误后把订单落成 Filled（stray hold）。
//!
//! 这三条都用真实的 `Core` + 真实账本；没有任何 mock 顶替资金路径。

use super::*;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn live(balance: Decimal) -> Core {
    let mut c = Core::new(CoreConfig {
        mode: Mode::Live,
        risk: RiskConfig {
            max_order_notional: dec!(3),
            ..Default::default()
        },
        dry_seed_balance: Decimal::from(10_000),
        ..Default::default()
    });
    c.set_balance(balance);
    c
}

/// The same fixture with its event sink wired, so "the system REPORTED it" is
/// an observable fact rather than a log line nobody can assert on.
fn live_with_sink(balance: Decimal) -> (Core, mpsc::UnboundedReceiver<Event>) {
    let mut c = live(balance);
    let (tx, rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    (c, rx)
}

/// Drain the sink, keeping only RiskAlert text.
fn take_alerts(rx: &mut mpsc::UnboundedReceiver<Event>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            out.push(message);
        }
    }
    out
}

fn buy(price: Decimal, size: Decimal, key: &str) -> OrderRequest {
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

fn venue_fill(order_id: &str, trade: &str, size: Decimal, price: Decimal) -> Fill {
    Fill {
        order_id: order_id.into(),
        trade_id: Some(trade.into()),
        token_id: "tok".into(),
        side: Side::Buy,
        price,
        size,
        status: FillStatus::Confirmed,
        ts_ms: 250,
        tx_hash: None,
        maker: Some(true),
        account_id: default_account_id(),
    }
}

/// 判据 1：撤单保留预留，直到 venue 确认撤单；
/// 确认前到达的 250 ms 延迟成交被吸收，且账本只被扣一次。
#[test]
fn cancel_keeps_reservation_until_venue_cancel_ack() {
    let mut c = live(dec!(10));
    let (id, st) = c.place(buy(dec!(0.40), dec!(5), "k1"), 0, 1).unwrap();
    assert_eq!(st, OrderStatus::Pending);
    c.bind_venue(&id, "venue-1".into(), 2).unwrap();
    assert_eq!(c.ledger().reserved(), dec!(2), "0.40 x 5 reserved");

    // 撤单：DELETE 入队，但 venue 还没说话。
    c.cancel(&id, 3).unwrap();
    assert_eq!(
        c.take_pending_venue_cancels(),
        vec!["venue-1".to_string()],
        "the DELETE is queued for the executor"
    );
    let o = c.ome().get(&id).unwrap();
    assert_eq!(
        o.status,
        OrderStatus::Live,
        "FALSIFICATION: queued is not cancelled — no terminal without an ack"
    );
    assert!(o.terminal_evidence.is_none());
    assert_eq!(
        c.ledger().reserved(),
        dec!(2),
        "FALSIFICATION: the money is still committed while the cancel travels"
    );
    assert_eq!(c.ledger().available(), dec!(8));

    // 250 ms 内 venue 成交了 2 股 —— 这正是旧代码会超支的窗口。
    c.ingest_fill(venue_fill(&id, "trade-late-1", dec!(2), dec!(0.40)), 253)
        .unwrap();
    let o = c.ome().get(&id).unwrap();
    assert_eq!(
        o.filled_size,
        dec!(2),
        "the late fill is real and must be booked"
    );
    assert_eq!(o.status, OrderStatus::PartiallyFilled);
    // 2 股按 limit 0.40 释放预留，成交价 0.40 → 现金扣 0.80。
    assert_eq!(
        c.ledger().reserved(),
        dec!(1.2),
        "only the filled part's reservation left the table"
    );
    assert_eq!(
        c.ledger().balance(),
        dec!(9.2),
        "the venue charged exactly price x size, once"
    );

    // venue 现在确认撤单。
    c.confirm_venue_cancel(&id, "venue-1", 260).unwrap();
    let o = c.ome().get(&id).unwrap();
    assert_eq!(o.status, OrderStatus::Cancelled);
    assert_eq!(
        o.terminal_evidence.as_ref().map(|e| &e.source),
        Some(&crate::model::TerminalEvidenceSource::VenueCancelAck {
            venue_order_id: "venue-1".into()
        }),
        "the terminal row carries the venue's own acknowledgement"
    );
    assert_eq!(
        c.ledger().reserved(),
        Decimal::ZERO,
        "the unfilled remainder is released on the ack"
    );
    assert_eq!(
        c.ledger().balance(),
        dec!(9.2),
        "FALSIFICATION: the confirm must not debit a second time"
    );
    assert!(c.ledger().is_balanced());
}

/// 判据 2：place 超时只探测，绝不在没有证据时本地撤销；无应答 = Unknown 上报。
#[test]
fn place_timeout_probes_and_never_locally_cancels_without_evidence() {
    let (mut c, mut rx) = live_with_sink(dec!(10));
    let (id, _) = c.place(buy(dec!(0.40), dec!(5), "k2"), 0, 1).unwrap();
    c.note_place_timeout(&id);

    let events = take_alerts(&mut rx);
    assert!(
        events.iter().any(|m| m.contains("place timed out")),
        "the timeout itself is reported, not swallowed: {events:?}"
    );

    // 超过判定窗口，venue 依然没有任何信息。
    let report = c
        .reconcile(crate::reconcile::VenueSnapshot {
            open_order_ids: vec![],
            trades: vec![],
            now_ms: 1 + PLACE_TIMEOUT_VERDICT_MS,
        })
        .unwrap();

    let o = c.ome().get(&id).unwrap();
    assert_eq!(
        o.status,
        OrderStatus::Pending,
        "FALSIFICATION: a POST timeout is not a venue verdict"
    );
    assert!(
        o.terminal_evidence.is_none(),
        "FALSIFICATION: nothing may claim a terminal fact"
    );
    assert_eq!(
        o.probe_attempts, 1,
        "the sweep records that it asked and got no answer"
    );
    assert_eq!(
        c.ledger().reserved(),
        dec!(2),
        "FALSIFICATION: the reservation must not be released on a guess"
    );
    assert!(
        report.actions.is_empty(),
        "no terminal action may be invented"
    );

    let alerts = take_alerts(&mut rx);
    let unverified: Vec<&String> = alerts.iter().filter(|m| m.contains("UNVERIFIED")).collect();
    assert_eq!(
        unverified.len(),
        1,
        "FALSIFICATION: the unverifiable posture is REPORTED exactly once (Unknown), \
         never silent: {alerts:?}"
    );
    assert!(
        unverified[0].contains("reservation"),
        "the report names what is being held: {}",
        unverified[0]
    );

    // 留在 live、钱还在 → 必须同时挡住 re-submit：再签一次就是第二张真单。
    assert!(
        !c.pending_unbound().iter().any(|o| o.order_id == id),
        "FALSIFICATION: an uncertain place must never be handed to the executor again"
    );

    // 第二次 sweep：仍然不撤销，且探测计数递增。
    c.reconcile(crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![],
        now_ms: 1 + 2 * PLACE_TIMEOUT_VERDICT_MS,
    })
    .unwrap();
    let o = c.ome().get(&id).unwrap();
    assert_eq!(o.status, OrderStatus::Pending);
    assert_eq!(o.probe_attempts, 2);
    assert_eq!(c.ledger().reserved(), dec!(2));
    assert!(c.ome.terminal_without_evidence().is_empty());
}

/// 判据 3：ghost 的 Filled 分支释放失败必须中止整个转换，不能先落 Filled
/// 再留下 stray hold（旧代码 `ghosts.rs` 用 `let _ =` 把释放错误吞掉）。
///
/// 形态是一条**重启日志行**：进程在成交落到 OME 之后、状态改成 Filled 之前
/// 挂掉，orders.jsonl 留下 `Live` + `filled_size == size`，重启后 `restore`
/// 原样恢复 —— 正是 ghost sweep 的 Filled 分支存在的理由。账面同时带着一笔
/// 该单的残留预留（重启重放的漂移），于是释放金额与订单承诺不符。
#[test]
fn ghost_filled_release_error_aborts_instead_of_filling() {
    let mut c = live(dec!(10));
    let id = "live_9";
    // 崩溃窗口的快照：成交已满，状态还停在 Live。
    c.ome.restore(vec![crate::model::TrackedOrder {
        order_id: id.into(),
        internal_key: "k3".into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Maker,
        role: crate::model::OrderRole::Maker,
        price: dec!(0.40),
        size: dec!(5),
        filled_size: dec!(5),
        avg_fill_price: Some(dec!(0.40)),
        status: OrderStatus::Live,
        round_slot: 1,
        submitted_at_ms: 1,
        updated_at_ms: 20_000,
        decision_at_ms: 1,
        execute_at_ms: 1,
        report_at_ms: 20_000,
        venue_order_id: Some("venue-3".into()),
        place_uncertain: false,
        escalate_at_ms: None,
        maker_timeout_ms: 0,
        account_id: default_account_id(),
        venue: String::new(),
        round_duration_sec: 0,
        rejected_reason: None,
        cancel_requested: false,
        probe_attempts: 0,
        terminal_evidence: None,
    }]);
    // 账面漂移：该单还挂着一笔 1.00 的预留，而成交已满 → 承诺是 0。
    c.accounts
        .get_mut(&default_account_id())
        .unwrap()
        .reserve(id, dec!(1))
        .unwrap();

    // venue 不再持有它、也没有新成交 —— ghost 的 Filled 分支。
    let result = c.reconcile(crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![],
        now_ms: 20_000,
    });

    let err = result
        .expect_err("FALSIFICATION: a release mismatch must abort the sweep, not be swallowed");
    assert_eq!(err.code, CoreErrorCode::InvalidParams, "{err:?}");
    assert!(
        err.message.contains("release mismatch"),
        "the operator must see WHY: {err:?}"
    );

    // 关键断言：转换整体未发生（预检先于任何写入）。
    let o = c.ome().get(id).unwrap();
    assert_eq!(
        o.status,
        OrderStatus::Live,
        "FALSIFICATION: no terminal status may be stamped while its money book disagrees"
    );
    assert!(
        o.terminal_evidence.is_none(),
        "no evidence may be recorded for a transition that aborted"
    );
    assert_eq!(
        c.ledger().reserved(),
        dec!(1),
        "the drifted hold is still visible — nothing was silently released"
    );

    // 漂移修好后，同一次 sweep 能把转换完整做成：不是半个终态。
    c.accounts
        .get_mut(&default_account_id())
        .unwrap()
        .sync_buy_reservation(id, Decimal::ZERO);
    let report = c
        .reconcile(crate::reconcile::VenueSnapshot {
            open_order_ids: vec![],
            trades: vec![],
            now_ms: 20_001,
        })
        .unwrap();
    assert!(
        report
            .actions
            .iter()
            .any(|a| matches!(a, crate::reconcile::ReconcileAction::MarkedFilled { .. })),
        "the retry completes the transition: {:?}",
        report.actions
    );
    let o = c.ome().get(id).unwrap();
    assert_eq!(o.status, OrderStatus::Filled);
    assert_eq!(
        o.terminal_evidence.as_ref().map(|e| &e.source),
        Some(&crate::model::TerminalEvidenceSource::VenueFill {
            venue_order_id: "venue-3".into()
        }),
        "the terminal row carries the venue's fill as its proof"
    );
    assert_eq!(c.ledger().reserved(), Decimal::ZERO);
    assert!(c.ome.terminal_without_evidence().is_empty());
}
