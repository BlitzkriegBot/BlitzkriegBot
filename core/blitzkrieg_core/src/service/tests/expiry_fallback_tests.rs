#![cfg(test)]

//! #684 两层修复：LIVE 模式下持有到期的轮次持仓永远无法结算 —— 场地的
//! updown 列表只载未来 ~20h 的未到期轮次，已过期 conditionId 永远不会出现
//! 在结算应答里。
//!
//! 第一层（内核自动，#1236 操作者裁决 2026-11-15）：轮次过期超过
//! [`EXPIRY_ESTIMATE_SETTLE_AFTER_MS`]（10s）→ 按兜底价（binary 市场把
//! 最后已知 mid 二值化：`> 0.5` → 1，`<= 0.5` → 0；`0`/无 feed → 0 并
//! 标 `price_source=unobserved`，绝不套入场价）**强制平仓入历史**，
//! trade record 带 `unverifiable_expiry` 标签，**不建 redemption claim**
//! —— 裁决未经验证，赎回一个发明出来的赢家是错的；场地现金真值
//! 由既有 reconcile/realign sweep 跟平。
//!
//! 第二层（操作员）：`positions.force_close` —— 持仓存在 + 已过期 +
//! manual-ops 审计路径三道门；审计行、warn、RiskAlert；同样不建 claim。

use super::*;
use crate::model::{FillPolicy, OrderStatus, Side};
use crate::position::OpenParams;
use crate::service::config::UnverifiedExpiryPolicy;
use rust_decimal_macros::dec;

/// 900s 网格上 slot 1 的终点：`place` 在 now=1、round_slot=1 落进
/// [900s, 1800s)，持仓到期即此 —— 所有判定时刻都锚在这个真实网格值上。
const EXPIRES_MS: i64 = 1_800_000;

/// Issue #176: plant a REAL book observation on a position — the only input
/// the fallback estimator accepts. There is no way to spell "entry price as a
/// mark" any more: `PriceState` has no such constructor.
fn observe(p: &mut crate::position::OpenPosition, price: Decimal) {
    p.price_state = crate::price_state::PriceState::observed_bid(price, EXPIRES_MS - 1_000);
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bk-expiry-fallback-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 一个 LIVE 形态的 dry 核（dry 让 tick 自驱结算 pass；判定逻辑两层共享），
/// 持久日志全部落私有目录，manual-ops 审计可选 —— 第二层测试用它。
fn fallback_core(dir: &std::path::Path, with_audit: bool) -> Core {
    let path = |name: &str| Some(dir.join(name).to_string_lossy().to_string());
    let mut c = Core::new(CoreConfig {
        mode: Mode::Dry,
        round_duration_sec: 900,
        risk: crate::risk::RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        position_log_path: path("positions.jsonl"),
        trade_log_path: path("trades.jsonl"),
        settlement_log_path: path("settlements.jsonl"),
        order_log_path: path("orders.jsonl"),
        manual_ops_audit_path: with_audit.then(|| path("manual-ops.jsonl")).flatten(),
        ..Default::default()
    });
    c.set_balance(dec!(1000));
    c
}

fn entry_order(price: Decimal, size: Decimal) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price,
        size,
        internal_key: "k1".into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

/// 开一个 5 股 @0.40 的持仓，市场在 [`EXPIRES_MS`] 过期，入场时现货记录在案。
/// 返回开好的持仓快照。
fn open_expiring_position(c: &mut Core) -> crate::position::OpenPosition {
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let (_id, status) = c.place(entry_order(dec!(0.40), dec!(5)), 0, 1).unwrap();
    assert_eq!(status, OrderStatus::Filled);
    let p = c.positions().open_positions()[0].clone();
    assert_eq!(p.expires_at_ms, EXPIRES_MS);
    p
}

/// 把 watch 表直接注入一条「已发出、零应答、first_sent_ms = sent」的查询。
/// 不走 take_queries 的真实节拍（那是用例 7 的活），判定用的可观测状态
/// 与线上完全一致：answers == 0 && first_sent_ms > 0。
fn stage_unanswered_query(c: &mut Core, condition_id: &str, token_id: &str, first_sent_ms: i64) {
    let positions: Vec<crate::position::OpenPosition> = c
        .positions()
        .open_positions()
        .iter()
        .filter(|p| p.condition_id == condition_id)
        .cloned()
        .collect();
    assert!(!positions.is_empty(), "fixture needs the position first");
    c.settlement.track_markets(&positions, first_sent_ms);
    c.settlement.take_queries(first_sent_ms);
    // take_queries stamps first_sent_ms = first_sent_ms exactly (fresh query);
    // unanswered_since needs now > first_sent, hence the +1.
    let unanswered = c.settlement.unanswered_since(0, first_sent_ms + 1);
    assert!(
        unanswered.iter().any(|(cid, _)| cid == condition_id),
        "fixture: the query must stage as unanswered: {unanswered:?}"
    );
    let _ = token_id;
}

/// 用例 1：过期超过 10s 强制结算窗（无论查询是否零应答）→ 兜底平仓，
/// trade record 带 `unverifiable_expiry`，**不贷可花现金**，审计锚全程为真。
///
/// Issue #176: the old form of this test forced `UnverifiedExpiryPolicy::Cash`
/// to pin the #684 spendable-cash credit. That arm is deleted (it minted cash
/// from a price nobody observed — R7.1 L1), so the test now pins the
/// RECEIVABLE model in the same scenario: the position still closes with the
/// unverifiable label, the claim carries the notional, and the ledger does not
/// move — the assertion REPLACES the deleted behavior rather than relaxing it.
#[test]
fn an_expired_never_answered_position_falls_back_closed() {
    let dir = scratch("fallback-closes");
    let mut c = fallback_core(&dir, false);
    // 0.97 > 0.5 → 二值化结算价 1（旧语义会错按原 mid 0.40 记账，少回笼 3%）。
    let mut p = open_expiring_position(&mut c);
    observe(&mut p, dec!(0.97));
    c.positions_mut().restore_open(vec![p], None);
    let cash_before = c.accounts().active_ledger().balance();

    // 过期瞬间 watch 接管（真路径），查询在过期后 100ms 首次发出、零应答。
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);
    assert_eq!(c.positions().open_positions().len(), 1, "still held");

    // 10s 强制结算界限之后的第一拍：兜底 sweep 平仓。
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    c.tick(bound + 1_000).unwrap();

    assert!(
        c.positions().open_positions().is_empty(),
        "the ghost position is closed by the fallback sweep"
    );
    let closed = &c.positions().closed_positions()[0];
    // 标签：不是 settlement —— 没有裁决就永远不能读成裁决。
    assert_eq!(
        closed.exit_reason,
        crate::model::ExitReason::UnverifiableExpiry,
        "the record must carry the unverifiable-expiry label"
    );
    // 0.97 > 0.5 → 二值化结算价 1（旧语义会错按 0.40 记账，少回笼 3%）。
    assert_eq!(closed.exit_price, dec!(1));
    assert_eq!(
        c.recent_trades(10)[0]["priceSource"],
        serde_json::json!("binary_threshold"),
        "trade record names the threshold source"
    );
    // Operator decision 2026-11-27: default Cash policy credits 1.00 × 5 = 5.00 directly to cash.
    let stats = c.engine_stats_at(bound + 2_000)["settlement"].clone();
    assert_eq!(
        stats["fallbackReceivableUsd"],
        serde_json::json!(0.0),
        "cash policy books no receivable claim"
    );
    assert_eq!(
        stats["pendingFallbackClaims"],
        serde_json::json!(0),
        "no pending fallback claims under cash policy"
    );
    assert_eq!(
        c.accounts().active_ledger().balance(),
        cash_before + dec!(5.0),
        "the fallback close credits 1.00 × 5 directly to spendable cash"
    );
    assert_eq!(stats["pendingRedemptions"], serde_json::json!(0));
    assert!(c.take_pending_redemptions(i64::MAX / 2).is_empty());
    // 结算日志（journal）里没有为它记过 settlement —— book 从未被调用。
    let journal = std::fs::read_to_string(dir.join("settlements.jsonl")).unwrap_or_default();
    assert!(
        !journal.contains("\"kind\":\"settlement\""),
        "the fallback must never book a settlement: {journal}"
    );
    // trade record 自带标签（面板/审计读的就是这一行）。
    let trades = c.recent_trades(10);
    assert_eq!(trades.len(), 1);
    assert_eq!(
        trades[0]["exitReason"],
        serde_json::json!("unverifiable_expiry")
    );
    // 记账恒等式在兜底平仓后依然成立。
    let audit = c.run_accounting_audit(bound + 3_000);
    // CodeQL rust/cleartext-logging 会把插值进消息的 summary()（含 balance 字样）
    // 当成敏感数据落日志；测试断言消息不插值，失败时 panic 定位靠 git/blame 与
    // audit 字段本身（#679 同族驳回先例：内部记账数值不是凭据）。
    assert!(
        audit.ok,
        "audit after fallback close failed (see AuditReport fields)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 用例 2：未到 10s 强制结算界限 → 不平。10s 内的等待仍是正确姿态
/// （feed 的最后报价可能仍在向 1/0 收敛）。
#[test]
fn inside_the_silence_bound_the_position_stays_open() {
    let dir = scratch("inside-bound");
    let mut c = fallback_core(&dir, false);
    open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);

    // 界限前 1s（10s 强制结算窗未到）：什么都不发生。
    c.tick(sent + 9_000).unwrap();
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "inside the 10s force-settle bound the fallback must not fire"
    );
    assert!(c.positions().closed_positions().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 用例 3（#1236 对齐）：场地应答过（answers > 0）且过期在 10s 强制结算
/// 窗口之内 → 不平（还在等 resolved=true；"not resolved yet" 的应答也算数
/// —— 场地活着）。
#[test]
fn an_answered_market_inside_the_ten_second_bound_stays_open() {
    let dir = scratch("answered-stays");
    let mut c = fallback_core(&dir, false);
    open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);

    // 场地应答过一次（未到期，resolved=false —— 正是 updown 载未来轮次的
    // 场地会给出的诚实回答）。answers 从 0 变 1。
    c.settlement.note_answer("cond", false, sent + 100);
    assert!(c.settlement.unanswered_since(0, sent + 200).is_empty());

    // 过期后 9s：10s 强制结算窗未到 —— 原地不动。
    c.tick(sent + 9_000).unwrap();
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "inside the 10s force-settle bound an answered market keeps waiting \
         for its verdict"
    );
    assert!(c.positions().closed_positions().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 用例 3b（#1236 操作者裁决 2026-11-15）：场地应答过但过期超过 600s 估计
/// 结算窗 → 按最后报价（binary 市场到期前已收敛到 1/0，实机 Cur = 0.99 /
/// 0.01 / 0.97）兜底平仓，trade record 带 `unverifiable_expiry`。裁决
/// 晚到的真实现金仍由 reconcile 对平 —— 等待不再以幽灵仓位为代价。
#[test]
fn an_answered_market_past_the_estimate_bound_falls_back_closed() {
    let dir = scratch("answered-slow-falls");
    let mut c = fallback_core(&dir, false);
    // 0.97 > 0.5 → 二值化结算价 1，验证兜底 sweep 在超估计窗后仍会平仓。
    let mut p = open_expiring_position(&mut c);
    observe(&mut p, dec!(0.97));
    c.positions_mut().restore_open(vec![p], None);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);

    // 场地一直有应答（resolved=false 的"未裁决"回答），但过期超过 10s
    // 强制结算窗仍未给出 verdict —— 平台侧结算早已完成（操作者裁决）。
    c.settlement.note_answer("cond", false, sent + 100);
    let past_bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS + 1_000;
    c.tick(past_bound).unwrap();
    assert!(
        c.positions().open_positions().is_empty(),
        "past the estimate bound the slow venue no longer strands the position"
    );
    let closed = c.positions().closed_positions();
    assert_eq!(closed.len(), 1, "exactly one fallback close");
    assert_eq!(
        closed[0].exit_price,
        dec!(1),
        "mid 0.97 settles 1 (binary threshold), not 0.40"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 用例 4：兜底后 settlement.tracked_markets() 归零 —— 最后一个持仓离场，
/// watch 通过 sync_settlement_watch 自然忘掉该市场。
#[test]
fn after_the_fallback_the_market_leaves_the_watch_list() {
    let dir = scratch("watch-forgets");
    let mut c = fallback_core(&dir, false);
    open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);
    let stats = c.engine_stats_at(sent + 100)["settlement"].clone();
    assert_eq!(
        stats["trackedMarkets"],
        serde_json::json!(1),
        "precondition: watched"
    );

    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    c.tick(bound + 1_000).unwrap();
    assert!(c.positions().open_positions().is_empty());
    let stats = c.engine_stats_at(bound + 2_000)["settlement"].clone();
    assert_eq!(
        stats["trackedMarkets"],
        serde_json::json!(0),
        "the watch forgets a market with nothing left to settle"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 用例 5：`positions.force_close` 三道门 —— 过期持仓平 + 审计行存在 +
/// RiskAlert；活轮持仓拒；无审计路径拒。平仓同样不建 claim。
#[test]
fn force_close_gates_and_audit() {
    let dir = scratch("force-close");
    let audit = dir.join("manual-ops.jsonl");

    // (a) 无审计路径 → 拒（回测/进程内测试不下药）。
    let mut c = fallback_core(&dir, false);
    let p = open_expiring_position(&mut c);
    let err = c
        .force_close_expired_position(&p.id, "uid:op", EXPIRES_MS + 1_000)
        .expect_err("no audit path = refused");
    assert_eq!(err.code, CoreErrorCode::InvalidParams, "{err}");
    assert!(err.message.contains("no manual-ops audit path"), "{err}");
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "refusal moves nothing"
    );
    drop(c);

    // (b) 活轮持仓 → 拒（那是 positions.exit 的活）。
    let mut c = fallback_core(&dir, true);
    let p = open_expiring_position(&mut c);
    let err = c
        .force_close_expired_position(&p.id, "uid:op", 1_500)
        .expect_err("a live round is refused");
    assert_eq!(err.code, CoreErrorCode::InvalidOrderState, "{err}");
    assert!(err.message.contains("still live"), "{err}");
    assert!(
        err.message.contains("positions.exit"),
        "the refusal points at the right verb"
    );
    assert_eq!(c.positions().open_positions().len(), 1);

    // (c) 过期持仓 + 审计路径在 → 平。兜底价与扫掠同源（0.5 二值化）：0.40 → 0；
    // 无 claim；审计行在。
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    let closed = c
        .force_close_expired_position(&p.id, "uid:op", EXPIRES_MS + 1_000)
        .expect("expired + audited = closed");
    assert_eq!(
        closed.exit_reason,
        crate::model::ExitReason::UnverifiableExpiry
    );
    assert_eq!(
        closed.exit_price,
        dec!(0),
        "0.40 并阈二值化为 0，与扫掠同源 (issue #1273)"
    );
    assert!(c.positions().open_positions().is_empty());
    assert!(
        c.take_pending_redemptions(EXPIRES_MS + 1_100).is_empty(),
        "no claim is booked"
    );
    assert_eq!(
        c.engine_stats_at(EXPIRES_MS + 1_200)["settlement"]["receivableUsd"],
        serde_json::json!(0.0)
    );
    // 审计行：op/actor/position/价格全名。
    let text = std::fs::read_to_string(&audit).unwrap();
    let rec: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(rec["op"], serde_json::json!("positions.force_close"));
    assert_eq!(rec["actor"], serde_json::json!("uid:op"));
    assert_eq!(rec["positionId"], serde_json::json!(closed.id));
    assert_eq!(rec["conditionId"], serde_json::json!("cond"));
    assert_eq!(rec["exitReason"], serde_json::json!("unverifiable_expiry"));
    // 操作员听得见响（RiskAlert）。
    let mut alerts = 0;
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            assert!(message.contains("force-closed"), "{message}");
            alerts += 1;
        }
    }
    assert!(alerts >= 1, "the operator hears the manual close");
    // 记账恒等式成立。
    let report = c.run_accounting_audit(EXPIRES_MS + 2_000);
    assert!(
        report.ok,
        "audit after force-close failed (see AuditReport fields)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 用例 5b（issue 733）：非零 round_slot 的过期门按**持仓自身族长**判定，
/// 不是全局格。持仓戳 slot=2、自身族长 3600s（快照直存），全局 900s：
/// 轮尾在 (2+1)·3600 = 10_800_000 —— 全局钟会把 [5_400_000, 10_800_000)
/// 误判成已过期（slot×900=2_700_000 更早），自身钟在 5_400_000 仍判活轮
/// 拒、在 10_800_000 放行。老快照（slot=0）回落 expires_at_ms 倒计时。
#[test]
fn force_close_expiry_uses_the_positions_own_family_clock() {
    let dir = scratch("force-close-family-clock");
    let mut c = fallback_core(&dir, true);
    let mut p = open_expiring_position(&mut c);
    p.round_slot = 2;
    p.round_duration_sec = 3600;
    // 直接把带戳的持仓放回账本（开仓路径的戳来自成交行，这里测判定门本身）。
    c.positions_mut().restore_open(vec![p.clone()], None);

    // 自身钟下仍在轮内：(slot+1)·3600 = 10_800_000，此刻 5_400_001 → 拒。
    let err = c
        .force_close_expired_position(&p.id, "uid:op", 5_400_001)
        .expect_err("the 3600s round is still live at slot midpoint");
    assert_eq!(err.code, CoreErrorCode::InvalidOrderState, "{err}");
    assert!(err.message.contains("still live"), "{err}");
    assert_eq!(c.positions().open_positions().len(), 1);

    // 全局钟的误判点（slot×900 早已过去）也不放行 —— 判定不用全局格。
    let err = c
        .force_close_expired_position(&p.id, "uid:op", 2_700_001)
        .expect_err("the global-grid arithmetic must not decide");
    assert_eq!(err.code, CoreErrorCode::InvalidOrderState, "{err}");

    // 自身钟轮尾已过 → 平。
    let closed = c
        .force_close_expired_position(&p.id, "uid:op", 10_800_001)
        .expect("past the slot's own end = closable");
    assert_eq!(
        closed.exit_reason,
        crate::model::ExitReason::UnverifiableExpiry
    );
    assert!(c.positions().open_positions().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 用例 6：幂等 —— 已平仓的 position_id 再调 force_close → 拒
/// （InvalidParams），不重复记账（只有一条 trade record）。
#[test]
fn force_close_is_idempotent_on_an_already_closed_position() {
    let dir = scratch("force-close-idem");
    let mut c = fallback_core(&dir, true);
    let p = open_expiring_position(&mut c);
    c.force_close_expired_position(&p.id, "uid:op", EXPIRES_MS + 1_000)
        .expect("first close lands");
    assert_eq!(c.positions().closed_positions().len(), 1);

    let err = c
        .force_close_expired_position(&p.id, "uid:op", EXPIRES_MS + 2_000)
        .expect_err("a second close is refused");
    assert_eq!(err.code, CoreErrorCode::InvalidParams, "{err}");
    assert!(err.message.contains("no open position"), "{err}");
    assert_eq!(
        c.positions().closed_positions().len(),
        1,
        "no double booking"
    );
    assert_eq!(c.trade_summary(None)["totalTrades"], serde_json::json!(1));
    // 未知 id 同拒。
    let err = c
        .force_close_expired_position("hft-ghost", "uid:op", EXPIRES_MS + 3_000)
        .expect_err("unknown id is refused");
    assert_eq!(err.code, CoreErrorCode::InvalidParams);

    let _ = std::fs::remove_dir_all(&dir);
}

/// #1238 / #176: a position with NO book observation (`PriceState::Unobserved`)
/// is NOT priced at the entry price. The fallback books `0` at
/// `price_source=unobserved`; the notional rides the receivable pool, and venue
/// truth redeems it later — no invented cost recovery.
#[test]
fn zero_feed_fallback_is_unobserved_no_entry_price_substitute() {
    let dir = scratch("zero-feed-unobserved");
    let mut c = fallback_core(&dir, false);
    let p = open_expiring_position(&mut c);
    // Issue #176: a freshly opened position carries NO observation at all —
    // the pre-#176 fixture had to zero `current_price` to model this; now the
    // unobserved state cannot be confused with a quote in the first place.
    let dead = p.clone();
    assert!(dead.price_state.is_unobserved(), "fixture: never observed");
    c.positions.restore_open(vec![dead], None);
    assert!(c.positions().open_positions()[0].mark().is_none());
    // 真路径：过期后 watch 接管、查询打点。
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    assert_eq!(c.settlement.unanswered_since(0, sent + 100).len(), 1);

    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    c.tick(bound + 1_000).unwrap();
    assert!(c.positions().open_positions().is_empty());
    let closed = &c.positions().closed_positions()[0];
    assert_eq!(
        closed.exit_reason,
        crate::model::ExitReason::UnverifiableExpiry
    );
    assert_eq!(
        closed.exit_price,
        dec!(0),
        "dead feed books 0, never the entry price"
    );
    let trades = c.recent_trades(10);
    assert_eq!(
        trades[0]["priceSource"],
        serde_json::json!("unobserved"),
        "price source names the unobserved feed"
    );
    // No spendable cash is credited: `amount = 0 × dust_shares = 0` so the
    // cash leg applies nothing and writes no ExpiryFallback journal row.
    let journal = c.ledger().journal_snapshot();
    assert!(
        !journal
            .iter()
            .any(|e| matches!(e.kind, crate::ledger::JournalKind::ExpiryFallback)),
        "no ExpiryFallback cash credit for an unobserved close: {journal:?}"
    );
    assert!(
        closed.net_pnl_usd <= Decimal::ZERO,
        "cost of an unobserved close is never booked as profit"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// #1238 binary threshold: an observation `<= 0.5` settles `0` (raw mid
/// `0.01` → `0`, no floor); exactly `0.5` → `0` (tie falls to zero); no
/// observation at all → `0` at `unobserved`.
#[test]
fn binary_threshold_books_zero_at_and_below_half() {
    // Sub-case 1: 0.01 — above zero but at/below the 0.5 threshold → 0.
    {
        let dir = scratch("binary-001");
        let mut c = fallback_core(&dir, false);
        let p = open_expiring_position(&mut c);
        let mut low = p.clone();
        observe(&mut low, dec!(0.01));
        c.positions.restore_open(vec![low], None);

        let sent = EXPIRES_MS + 100;
        c.tick(sent).unwrap();
        stage_unanswered_query(&mut c, "cond", "tok", sent);
        let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
        c.tick(bound + 1_000).unwrap();

        let closed = &c.positions().closed_positions()[0];
        assert_eq!(closed.exit_price, dec!(0), "mid 0.01 settles 0, not 0.01");
        assert_eq!(
            c.recent_trades(10)[0]["priceSource"],
            serde_json::json!("binary_threshold"),
            "threshold-priced at or below 0.5"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Sub-case 2: exactly 0.5 → 0 (the tie falls to zero).
    {
        let dir = scratch("binary-tie-050");
        let mut c = fallback_core(&dir, false);
        let p = open_expiring_position(&mut c);
        let mut tie = p.clone();
        observe(&mut tie, dec!(0.5));
        c.positions.restore_open(vec![tie], None);

        let sent = EXPIRES_MS + 100;
        c.tick(sent).unwrap();
        stage_unanswered_query(&mut c, "cond", "tok", sent);
        let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
        c.tick(bound + 1_000).unwrap();

        let closed = &c.positions().closed_positions()[0];
        assert_eq!(closed.exit_price, dec!(0), "the exact-0.5 tie settles 0");
        assert_eq!(
            c.recent_trades(10)[0]["priceSource"],
            serde_json::json!("binary_threshold"),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Sub-case 3: no observation at all → unobserved (covered by name-sake
    // test too, kept here as the threshold family's zero corner).
    let dir = scratch("binary-zero");
    let mut c = fallback_core(&dir, false);
    let p = open_expiring_position(&mut c);
    let dead = p.clone();
    assert!(dead.price_state.is_unobserved(), "fixture: never observed");
    c.positions.restore_open(vec![dead], None);

    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    c.tick(bound + 1_000).unwrap();

    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.exit_price, dec!(0));
    assert_eq!(
        c.recent_trades(10)[0]["priceSource"],
        serde_json::json!("unobserved"),
        "zero feed is unobserved, not threshold-priced"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// OpenParams 构造的补充面：test-seam 开出的旧快照（entered > expires +
/// EXPIRY_TRUST_MS）不进 watch（track_markets 拒收），因此兜底也永远不碰它
/// —— 那条路是 #175 的发明防护，本修复不越它半步。
#[test]
fn an_untrackable_position_is_outside_the_fallback() {
    let dir = scratch("untrackable");
    let mut c = fallback_core(&dir, false);
    c.positions.open(
        OpenParams {
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: crate::model::SignalDirection::Up,
            token_id: "tok".into(),
            condition_id: "cond".into(),
            account_id: crate::model::default_account_id(),
            venue: String::new(),
            partner_token_id: String::new(),
            entry_price: dec!(0.40),
            // expires 在入场前 10 分钟 —— 不是市场终点（EXPIRY_TRUST_MS 防护）。
            expires_at_ms: 1_000,
            was_maker: false,
            target_exit_price: None,
            round_duration_sec: 0,
            round_slot: 0,
        },
        700_000,
    );
    c.tick(3_000_000).unwrap();
    assert_eq!(
        c.settlement.tracked_markets(),
        0,
        "the untrackable position never enters the watch"
    );
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "and the fallback never closes what it does not watch"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// #987: a fallback close carries `source == "fallback"` and records the
/// original fallback price in `original_fallback_price`. The trade record
/// (TradeDb) mirrors both fields. Issue 991: `venue` closes carry NEITHER —
/// provenance and price stay in lockstep.
#[test]
fn fallback_close_carries_source_fallback() {
    let dir = scratch("source-fallback");
    let mut c = fallback_core(&dir, true);
    let p = open_expiring_position(&mut c);
    let closed = c
        .force_close_expired_position(&p.id, "uid:op", EXPIRES_MS + 1_000)
        .expect("expired + audited = closed");
    assert_eq!(closed.source, "fallback", "fallback close must be labeled");
    // 0.40 并阈二值化为 0：兜底价随扫掠同源 (issue #1273)。
    assert_eq!(
        closed.original_fallback_price,
        Some(dec!(0)),
        "original fallback price preserved for audit"
    );
    // The trade record mirrors the closed position's provenance.
    let rec = &c.positions().closed_positions()[0];
    assert_eq!(rec.source, "fallback");
    assert_eq!(rec.original_fallback_price, Some(dec!(0)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// #987, issue 991: reconcile backfill marks fallback-closed positions
/// `source == "reconciled"` when venue cash truth arrives, while the row's
/// exit price and `original_fallback_price` KEEP the fallback value — issue
/// 991's acceptance shape: the venue fill price (0.55) is deliberately
/// DIFFERENT from the fallback price (0.40) so the two are provably
/// distinguishable after the backfill, and an estimated price is never
/// re-labeled `venue`.
#[test]
fn reconcile_backfill_flips_fallback_to_venue() {
    let dir = scratch("reconcile-backfill");
    let mut c = fallback_core(&dir, false);
    // 0.97 > 0.5 → 二值化结算价 1，作为 fallback_price；venue 后以 0.55 到账，两者可区分。
    let mut p = open_expiring_position(&mut c);
    observe(&mut p, dec!(0.97));
    c.positions_mut().restore_open(vec![p], None);

    // First close via the fallback sweep (zero venue answers past the silence bound).
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    c.tick(bound + 1_000).unwrap();
    assert!(c.positions().open_positions().is_empty());
    let closed_before = &c.positions().closed_positions()[0];
    assert_eq!(
        closed_before.source, "fallback",
        "precondition: source is fallback"
    );
    let fallback_price = closed_before.exit_price;
    assert_eq!(
        fallback_price,
        dec!(1),
        "fixture: binary 0.97 settles 1 (the fallback price)"
    );
    assert_eq!(
        closed_before.original_fallback_price,
        Some(fallback_price),
        "precondition: the fallback row carries its own price for audit"
    );

    // Venue cash truth arrives: a SELL fill on the same token via reconcile,
    // at a price that is NOT the fallback price (issue 991: the old fixture
    // used 0.40 == the fallback price, masking the label/price gap).
    let venue_price = dec!(0.55);
    assert_ne!(venue_price, fallback_price, "fixture must not mask the gap");
    let snap = crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![crate::reconcile::VenueTrade {
            venue_order_id: "ven:1".into(),
            trade_id: "tr:1".into(),
            token_id: "tok".into(),
            side: crate::model::Side::Sell,
            size: dec!(5),
            price: venue_price,
            ts_ms: bound + 2_000,
            tx_hash: None,
            maker: None,
            // #175: the row names the account whose cash moved. The position is
            // already fallback-closed, so the fold parks it `unknown_position`
            // (nothing to reduce); the #991 backfill below is what flips the
            // closed row — it is token-scoped on purpose.
            account: Some(crate::model::default_account_id()),
        }],
        now_ms: bound + 2_000,
    };
    let report = c.reconcile(snap).expect("reconcile succeeds");
    // The backfill should have marked the fallback row.
    assert!(
        !report
            .actions
            .iter()
            .any(|a| matches!(a, crate::reconcile::ReconcileAction::FilledGap { .. })),
        "no gap fill expected for an already-closed position"
    );
    let closed_after = &c.positions().closed_positions()[0];
    assert_eq!(
        closed_after.source, "reconciled",
        "reconcile backfill marks the row reconciled (issue 991: never `venue` while the price is still the estimate)"
    );
    assert_eq!(
        closed_after.original_fallback_price,
        Some(fallback_price),
        "original fallback price preserved for audit, not overwritten"
    );
    assert_eq!(
        closed_after.exit_price, fallback_price,
        "exit price unchanged: the venue's 0.55 fill must NOT be copied onto the row (no invented price)"
    );
    assert_ne!(
        closed_after.exit_price, venue_price,
        "acceptance: fallback price and venue price stay distinguishable"
    );
    // The row is still on the unverified exposure report, flagged reconciled.
    let exposure = c.unverified_exposure_report();
    assert_eq!(exposure.total_positions, 1, "still unverified exposure");
    assert_eq!(exposure.entries[0].source, "reconciled");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue 991, acceptance: the `on_market_resolution` path backfills too — even
/// when NO open position remains (the empty-book early return used to skip the
/// backfill entirely, so a fallback row whose market later resolved stayed
/// `fallback` forever). The verdict's `payout_per_share` (1.00) is NOT copied
/// onto the row: the row's unsold remainder was already written off at the
/// fallback price and the settlement claim belongs to tokens still held; the
/// row is marked `reconciled` with its price untouched.
#[test]
fn market_resolution_backfills_a_fallback_row_with_no_open_position() {
    let dir = scratch("resolution-backfill");
    let mut c = fallback_core(&dir, false);
    let _p = open_expiring_position(&mut c);

    // Fallback close first (zero venue answers past the silence bound).
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    c.tick(bound + 1_000).unwrap();
    let closed_before = &c.positions().closed_positions()[0];
    assert_eq!(closed_before.source, "fallback", "precondition");
    let fallback_price = closed_before.exit_price;

    // The market's resolution arrives AFTER the fallback closed everything:
    // the open book is empty for this condition (the early-return branch).
    let resolution = blitzkrieg_market_api::MarketResolution {
        condition_id: "cond".into(),
        resolved: true,
        payouts: vec![("tok".to_string(), Decimal::ONE)],
        neg_risk: false,
        source: "test".into(),
    };
    c.on_market_resolution(resolution, bound + 3_000);

    let closed_after = &c.positions().closed_positions()[0];
    assert_eq!(
        closed_after.source, "reconciled",
        "the resolution marks the fallback row reconciled even with an empty open book"
    );
    assert_eq!(
        closed_after.exit_price, fallback_price,
        "the verdict's payout price is NOT copied onto the already-written-off row"
    );
    assert_eq!(
        closed_after.original_fallback_price,
        Some(fallback_price),
        "the fallback price stays in the audit field"
    );
    // Idempotent: a re-delivered resolution cannot double-mark or change it.
    let resolution2 = blitzkrieg_market_api::MarketResolution {
        condition_id: "cond".into(),
        resolved: true,
        payouts: vec![("tok".to_string(), Decimal::ONE)],
        neg_risk: false,
        source: "test".into(),
    };
    c.on_market_resolution(resolution2, bound + 4_000);
    let closed_final = &c.positions().closed_positions()[0];
    assert_eq!(closed_final.source, "reconciled", "still reconciled");
    assert_eq!(closed_final.exit_price, fallback_price, "price still kept");
    let _ = std::fs::remove_dir_all(&dir);
}

/// #987, issue 991: the unverified exposure report lists fallback-closed (and,
/// once backfilled, reconciled) rows only — a `venue` close never appears, and
/// never merges into realized PnL. The fixture has no reconcile, so the entry
/// stays plain `"fallback"`.
#[test]
fn unverified_exposure_report_lists_only_fallback_closes() {
    let dir = scratch("exposure-report");
    let mut c = fallback_core(&dir, true);

    // One fallback close: over-expiry, zero venue answers.
    let _p1 = open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    c.tick(bound + 1_000).unwrap();
    assert!(c.positions().open_positions().is_empty());
    let fallback_cost = c.positions().closed_positions()[0].cost_usd;

    // One venue close: force-close via the settlement path would be "fallback",
    // so instead simulate a venue close directly through the book's close() with
    // source="venue". Use a second condition/token so the fallback row isn't
    // flipped by reconcile.
    c.book_snapshot("tok2", vec![], vec![(dec!(0.50), dec!(100))], bound + 3_000);
    let (_id, status) = c
        .place(
            OrderRequest {
                token_id: "tok2".into(),
                condition_id: "cond2".into(),
                side: Side::Buy,
                mode: FillPolicy::Taker,
                price: dec!(0.50),
                size: dec!(3),
                internal_key: "k2".into(),
                strategy: "spread_arb".into(),
                asset: "ETH".into(),
                direction: "up".into(),
                round_slot: 1,
                account_id: crate::model::default_account_id(),
            },
            0,
            1,
        )
        .unwrap();
    assert_eq!(status, crate::model::OrderStatus::Filled);
    let p2 = c.positions().open_positions()[0].clone();
    // Close it as a venue close (exit path).
    let closed2 = c
        .positions
        .close(
            &p2.id,
            dec!(0.50),
            crate::model::ExitReason::TakeProfit,
            true,
            bound + 4_000,
            "venue",
        )
        .expect("venue close lands");
    assert_eq!(closed2.source, "venue");

    // Report: only the fallback row.
    let report = c.unverified_exposure_report();
    assert_eq!(
        report.total_positions, 1,
        "only one fallback-closed position"
    );
    assert_eq!(
        report.entries[0].source, "fallback",
        "with no reconcile the entry carries its plain fallback provenance"
    );
    assert_eq!(
        report.entries[0].cost_usd, fallback_cost,
        "report carries the fallback position's cost"
    );
    assert_eq!(
        report.total_cost_usd, fallback_cost,
        "total cost is just the fallback position's cost"
    );
    assert!(
        report.total_percentage_of_closed_cost > Decimal::ZERO,
        "unverified share of closed cost is nonzero"
    );
    assert_eq!(
        report.entries[0].net_pnl_usd,
        c.positions().closed_positions()[0].net_pnl_usd,
        "report mirrors the closed row's net pnl"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Kept for regression: an unobserved close still nets ≤ 0 (never profit), at
/// `price_source=unobserved` — no entry-price cost recovery exists any more
/// (issue #176 deleted that basis).
#[test]
fn zero_feed_fallback_never_books_profit() {
    let dir = scratch("zero-feed-no-profit");
    let mut c = fallback_core(&dir, false);
    let p = open_expiring_position(&mut c);
    // A dead feed: nothing was ever observed (#176's first-class state).
    let dead = p.clone();
    assert!(dead.price_state.is_unobserved(), "fixture: never observed");
    c.positions.restore_open(vec![dead], None);

    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    c.tick(bound + 1_000).unwrap();

    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.exit_price, dec!(0));
    assert_eq!(
        c.recent_trades(10)[0]["priceSource"],
        serde_json::json!("unobserved"),
    );
    assert!(
        closed.net_pnl_usd <= Decimal::ZERO,
        "cost recovery never books profit"
    );
    assert_eq!(
        closed.source, "fallback",
        "zero-feed close is labeled fallback"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// #1238 binary threshold: an observation `> 0.5` settles `1` — 0.97 → 1,
/// 0.6 → 1 — and the notional (`1 × dust_shares` = 5 unsold shares @ 1 = 5
/// USDC) rides the RECEIVABLE pool.
///
/// Issue #176: this test used to force `UnverifiedExpiryPolicy::Cash` and pin
/// the `ExpiryFallback` journal credit. That arm is deleted (it minted
/// spendable cash from an estimate), so the assertion now pins the receivable
/// booking AND the absence of any cash credit — a strictly stronger statement
/// than the deleted one, on the same price facts.
#[test]
fn binary_above_half_settles_one_and_books_the_receivable() {
    for price in [dec!(0.97), dec!(0.6)] {
        let dir_name = format!("binary-{}", price);
        let dir = scratch(&dir_name);
        let mut c = fallback_core(&dir, false);
        let p = open_expiring_position(&mut c);
        let mut hi = p.clone();
        observe(&mut hi, price);
        c.positions.restore_open(vec![hi], None);
        let cash_before = c.accounts().active_ledger().balance();

        let sent = EXPIRES_MS + 100;
        c.tick(sent).unwrap();
        stage_unanswered_query(&mut c, "cond", "tok", sent);
        let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
        c.tick(bound + 1_000).unwrap();

        let closed = &c.positions().closed_positions()[0];
        assert_eq!(closed.exit_price, dec!(1), "mid {price} settles 1",);
        assert_eq!(
            c.recent_trades(10)[0]["priceSource"],
            serde_json::json!("binary_threshold"),
        );
        // Under default Cash policy, `1 × dust_shares` credits directly to spendable cash.
        assert_eq!(
            c.engine_stats_at(bound + 2_000)["settlement"]["fallbackReceivableUsd"],
            serde_json::json!(0.0),
            "cash policy books no receivable claim: price={price}",
        );
        assert_eq!(
            c.accounts().active_ledger().balance(),
            cash_before + dec!(5.0),
            "1 USDC × 5 unsold shares credits directly to spendable cash: price={price}"
        );
        let credited: Decimal = c
            .ledger()
            .journal_snapshot()
            .iter()
            .filter(|e| matches!(e.kind, crate::ledger::JournalKind::ExpiryFallback))
            .map(|e| e.amount)
            .sum();
        assert_eq!(
            credited,
            dec!(5.0),
            "ExpiryFallback journal row records the direct cash credit: price={price}",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// #1238: an observation at-or-below 0.5 — 0.4 → 0 (0.6's near-twin with the
/// opposite settlement: 0.2 notional swing flips 1 vs 0). No cash credit
/// (0 × dust_shares = 0); balance moves only by realized PnL.
#[test]
fn binary_at_or_below_half_settles_zero_no_credit() {
    let dir = scratch("binary-040");
    let mut c = fallback_core(&dir, false);
    let p = open_expiring_position(&mut c);
    let mut lo = p.clone();
    observe(&mut lo, dec!(0.4));
    c.positions.restore_open(vec![lo], None);

    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    c.tick(bound + 1_000).unwrap();

    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.exit_price, dec!(0), "mid 0.4 settles 0");
    assert_eq!(
        c.recent_trades(10)[0]["priceSource"],
        serde_json::json!("binary_threshold"),
    );
    // 0 × dust_shares = 0 → no ExpiryFallback journal row, no cash credit.
    let journal = c.ledger().journal_snapshot();
    assert!(
        !journal
            .iter()
            .any(|e| matches!(e.kind, crate::ledger::JournalKind::ExpiryFallback)),
        "no ExpiryFallback cash credit for a 0 settlement"
    );
    // net_pnl reflects the realized loss; no fallback credit inflates the books.
    assert!(
        closed.net_pnl_usd <= Decimal::ZERO,
        "a 0 settlement books a loss, never profit"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #1273: the manual force close and the expiry sweep price from the
/// same function — the same position facts yield identical `exit_price` and
/// `price_source` on both paths (previously 0.55 priced at the raw mid on
/// force close but binary 1 on the sweep).
#[test]
fn force_close_and_sweep_price_converge() {
    let mark = dec!(0.55); // > 0.5 → binary 1 on both paths

    // Path A: 手动强平（operator force close，审计路径配齐）。
    let dir_a = scratch("conv-force");
    let mut c_a = fallback_core(&dir_a, true);
    let mut p_a = open_expiring_position(&mut c_a);
    observe(&mut p_a, mark);
    c_a.positions_mut().restore_open(vec![p_a.clone()], None);
    let closed_a = c_a
        .force_close_expired_position(&p_a.id, "uid:op", EXPIRES_MS + 1_000)
        .expect("force close lands");

    // Path B: 兜底 sweep（过期后 10s 估计结算界限之外，场地零应答）。
    let dir_b = scratch("conv-sweep");
    let mut c_b = fallback_core(&dir_b, false);
    let mut p_b = open_expiring_position(&mut c_b);
    observe(&mut p_b, mark);
    c_b.positions_mut().restore_open(vec![p_b], None);
    let sent = EXPIRES_MS + 100;
    c_b.tick(sent).unwrap();
    stage_unanswered_query(&mut c_b, "cond", "tok", sent);
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    c_b.tick(bound + 1_000).unwrap();
    assert!(c_b.positions().open_positions().is_empty());
    let closed_b = &c_b.positions().closed_positions()[0];

    assert_eq!(
        closed_a.exit_price, closed_b.exit_price,
        "force close and sweep converge on exit price"
    );
    assert_eq!(
        c_a.recent_trades(10)[0]["priceSource"],
        c_b.recent_trades(10)[0]["priceSource"],
        "force close and sweep converge on price source"
    );
    assert_eq!(closed_a.exit_price, dec!(1), "0.55 并阈结算为 1");
    assert_eq!(
        closed_a.price_source, "binary_threshold",
        "price source names the binary threshold"
    );

    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

/// Issue #1273: `Receivable` 政策下的兜底平仓不贷可花现金。0.55 并阈结算
/// 为 1，名义过入应收池，现金账分文不入。
#[test]
fn receivable_policy_credits_no_cash_on_fallback_close() {
    let dir = scratch("receivable-no-cash");
    let mut c = fallback_core(&dir, false);
    // 默认已是 Receivable；此处显式点名，让断言对缺省策略不敏感。
    c.config.unverified_expiry_policy = UnverifiedExpiryPolicy::Receivable;

    let mut p = open_expiring_position(&mut c);
    observe(&mut p, dec!(0.55));
    c.positions_mut().restore_open(vec![p], None);

    let before = c.accounts().active_ledger().balance();

    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    c.tick(bound + 1_000).unwrap();

    assert!(c.positions().open_positions().is_empty());
    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.exit_price, dec!(1), "0.55 > 0.5 结算为 1");
    assert_eq!(
        c.recent_trades(10)[0]["priceSource"],
        serde_json::json!("binary_threshold"),
    );
    // 1 × dust_shares = 5 过入应收池，现金账不增一分；审计恒等式仍成立。
    assert_eq!(
        c.engine_stats_at(bound + 2_000)["settlement"]["receivableUsd"],
        serde_json::json!(5.0),
    );
    assert_eq!(
        c.accounts().active_ledger().balance(),
        before,
        "Receivable 兜底平仓不贷可花现金",
    );
    let audit = c.run_accounting_audit(bound + 3_000);
    assert!(
        audit.ok,
        "audit after receivable fallback close failed (see AuditReport fields)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #1273 / #176: an UNOBSERVED expiry (dead feed / never quoted) never
/// credits spendable cash. Issue #176 deleted the `Cash` policy, so this pins
/// the surviving (and only) model: the close books `0` at
/// `price_source=unobserved`, a zero-amount claim rides the pool, and the
/// ledger does not move at all.
#[test]
fn unobserved_expiry_credits_no_cash() {
    let dir = scratch("unobserved-no-cash");
    let mut c = fallback_core(&dir, false);
    // Only one policy exists now; named explicitly so the test does not depend
    // on the default.
    c.config.unverified_expiry_policy = UnverifiedExpiryPolicy::Receivable;

    let p = open_expiring_position(&mut c);
    // No observation was ever made — #176's first-class "unknown" state.
    let dead = p.clone();
    assert!(dead.price_state.is_unobserved(), "fixture: never observed");
    c.positions_mut().restore_open(vec![dead], None);

    let before = c.accounts().active_ledger().balance();

    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    c.tick(bound + 1_000).unwrap();

    assert!(c.positions().open_positions().is_empty());
    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.exit_price, dec!(0), "an unobserved close books 0");
    assert_eq!(
        c.recent_trades(10)[0]["priceSource"],
        serde_json::json!("unobserved"),
        "price source names the unobserved feed"
    );
    assert_eq!(
        c.accounts().active_ledger().balance(),
        before,
        "an unobserved fallback credits no cash"
    );
    // The claim exists (that IS the receivable model) but carries zero — the
    // notional is never invented.
    let (booked, shares) = {
        let claims: Vec<_> = c.settlement.fallback_claims().collect();
        assert_eq!(claims.len(), 1, "one claim records the unknown close");
        (claims[0].booked_usd(), claims[0].shares)
    };
    assert_eq!(
        booked,
        Decimal::ZERO,
        "the claim books no invented notional"
    );
    assert_eq!(shares, dec!(5), "the share count is the real remainder");

    let _ = std::fs::remove_dir_all(&dir);
}
