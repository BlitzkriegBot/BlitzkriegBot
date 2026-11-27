#![cfg(test)]

use super::*;
use rust_decimal_macros::dec;

// ── #trades-mode-split: live 与 dry 的成交账本互不可见 ───────────────────

/// The end-to-end isolation the bug report asked for: a LIVE core must not
/// see rows in the DRY ledger. Since schema v4 the two books share ONE
/// database file and are separated by `mode` in the row identity, so the
/// isolation that used to be a path split is now an address split — every
/// read predicate carries the mode, and the shared file makes the two books
/// *harder* to leak between, not easier. Mirrors production: the dry book
/// already holds a dry-era probe row when live boots.
#[test]
fn live_recent_trades_do_not_read_the_dry_ledger() {
    let dir = reset_audit_dir("trades-split");
    let db_path = dir.join("store.sqlite");

    // The dry era's leftover: one probe row in the DRY book of the shared DB.
    let dry_closed = crate::position::ClosedPosition {
        id: "hft-probe-1".into(),
        strategy: "kline_probe".into(),
        asset: "BTC".into(),
        direction: crate::model::SignalDirection::Up,
        token_id: "tok".into(),
        condition_id: "cond".into(),
        entry_price: Decimal::ONE / Decimal::from(2),
        exit_price: Decimal::ONE,
        shares: Decimal::from(10),
        cost_usd: Decimal::from(5),
        was_maker_entry: true,
        was_maker_exit: false,
        entry_fee_pct: Decimal::ZERO,
        exit_fee_pct: Decimal::ZERO,
        maker_rebate_usd: Decimal::ZERO,
        pnl_usd: Decimal::from(5),
        pnl_pct: Decimal::from(100),
        net_pnl_usd: Decimal::ONE,
        net_pnl_pct: Decimal::from(20),
        high_pnl_pct: Decimal::ZERO,
        low_pnl_pct: Decimal::ZERO,
        hold_time_sec: 0,
        exit_reason: crate::model::ExitReason::TakeProfit,
        entered_at_ms: 0,
        exited_at_ms: 1,
        entry_role: crate::model::OrderRole::Maker,
        exit_role: crate::model::OrderRole::Taker,
        dust_shares: Decimal::ZERO,
        account_id: crate::model::default_account_id(),
        venue: "polymarket".into(),
        source: String::new(),
        original_fallback_price: None,
        price_source: String::new(),
    };
    // The dry book already carries its probe row — written through a dry-mode
    // core onto the SAME database file live is about to open, which is the
    // deployment shape the bug report describes (one store, two books).
    let mut dry = Core::new(CoreConfig {
        mode: Mode::Dry,
        db_path: Some(db_path.clone()),
        manual_ops_audit_path: Some(dir.join("manual-ops.jsonl").to_string_lossy().into_owned()),
        dry_seed_balance: Decimal::from(1_000),
        ..Default::default()
    });
    dry.set_balance(Decimal::from(1_000));
    dry.trade_log
        .as_ref()
        .expect("a core with db_path carries a trade ledger")
        .append(&crate::trade::TradeRecord::from_closed(&dry_closed))
        .expect("the dry probe row lands");

    // A live core on the same DB — no explicit --trade-log, exactly the
    // deployment shape from the bug report.
    let mut live = Core::new(CoreConfig {
        mode: Mode::Live,
        db_path: Some(db_path.clone()),
        manual_ops_audit_path: Some(dir.join("manual-ops.jsonl").to_string_lossy().into_owned()),
        dry_seed_balance: Decimal::from(1_000),
        ..Default::default()
    });
    live.set_balance(Decimal::from(1_000));

    // Boot view: the dry row is invisible. The fallback (in-memory closed
    // positions) is empty too — the panel shows nothing, not dry history.
    assert!(
        live.recent_trades(50).is_empty(),
        "live core saw the dry book's probe row: {:?}",
        live.recent_trades(50)
    );

    // One live trade books into the LIVE book only.
    let mut live_closed = dry_closed.clone();
    live_closed.id = "live-1".into();
    live_closed.strategy = "spread_arb".into();
    live.trade_log
        .as_ref()
        .expect("a core with db_path carries a trade ledger")
        .append(&crate::trade::TradeRecord::from_closed(&live_closed))
        .expect("the live row lands");
    let rows = live.recent_trades(50);
    assert_eq!(rows.len(), 1, "exactly the live row: {rows:?}");
    assert_eq!(rows[0]["id"], serde_json::json!("live-1"));

    // The dry book is untouched by the live run: same file, different address.
    let dry_rows = dry.recent_trades(50);
    assert_eq!(
        dry_rows.len(),
        1,
        "dry book must stay one row: {dry_rows:?}"
    );
    assert_eq!(dry_rows[0]["id"], serde_json::json!("hft-probe-1"));
}

#[test]
fn reset_trading_is_refused_in_live_mode() {
    let mut c = live_core(dec!(10));
    let err = c
        .reset_trading(&reset_params("RESET"), "uid:t", 1)
        .expect_err("live refuses the wipe");
    assert_eq!(err.code, CoreErrorCode::RiskRejected);
    assert!(
        err.message.contains("append-only evidence"),
        "the refusal names why: {err}"
    );
}

#[test]
fn reset_trading_requires_the_verbatim_confirm_word() {
    let mut c = dry_core(dec!(10));
    for bad in ["", "reset", "RESET ", "confirm"] {
        let err = c
            .reset_trading(&reset_params(bad), "uid:t", 1)
            .expect_err("a wrong confirm word is refused");
        assert_eq!(err.code, CoreErrorCode::InvalidParams, "{bad}: {err}");
    }
}

#[test]
fn reset_trading_refuses_without_an_audit_path_and_wipes_with_one() {
    let mut c = dry_core(dec!(10));
    let err = c
        .reset_trading(&reset_params("RESET"), "uid:t", 1)
        .expect_err("no audit path = no wipe");
    assert!(
        err.message.contains("no manual-ops audit path"),
        "the refusal names the missing audit: {err}"
    );

    // 有审计路径 → 重置落地：记录清空、余额复原、审计行存在。The three
    // log paths are PRIVATE absolute paths: the reset self-check counts
    // the log files and parallel tests trade on the default RELATIVE
    // data/ paths (in a cwd this test family used to hijack), so a
    // shared path would count a neighbour's lines as a failed reset.
    // `db_path` is what makes the trade ledger exist at all now — the reset's
    // own post-wipe verifier reads the SQL book, so a core without a store
    // would skip that leg of the check entirely.
    let dir = reset_audit_dir("happy");
    let audit = dir.join("manual-ops.jsonl");
    let log = |name: &str| Some(dir.join(name).to_string_lossy().into_owned());
    let mut c = Core::new(CoreConfig {
        manual_ops_audit_path: Some(audit.to_string_lossy().into_owned()),
        db_path: Some(dir.join("store.sqlite")),
        trade_log_path: log("trades.jsonl"),
        order_log_path: log("orders.jsonl"),
        position_log_path: log("positions.jsonl"),
        dry_seed_balance: Decimal::from(5_000),
        ..Default::default()
    });
    c.set_balance(Decimal::from(5_000));
    let r = c.reset_trading(&reset_params("RESET"), "uid:t", 7).unwrap();
    assert_eq!(
        r.balances,
        vec![("default".to_string(), Decimal::from(5_000))]
    );
    assert_eq!(c.trade_summary(None)["totalTrades"], serde_json::json!(0));
    let text = std::fs::read_to_string(&audit).unwrap();
    let rec: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(rec["op"], serde_json::json!("reset_trading"));
    assert_eq!(rec["pre"]["balance"], serde_json::json!("5000"));
}

/// 重置必须连记账审计的状态一起清：账本归零后，带着旧 realized 的
/// 锚点会让 trade_ledger_identity 永久假红（真实事故 2026-10-16：重置后
/// 漂移 94.72，halt_entries 拦下之后每一笔意图——策略页看着在下单，
/// 行情页一支挂单都没有）。重置后下一次审计必须在干净账目上重新取锚、
/// 不 halt；而不是把锚点永远钉在过去。
#[test]
fn reset_trading_re_anchors_the_accounting_audit_instead_of_halting_forever() {
    let dir = reset_audit_dir("reanchor");
    let log = |name: &str| Some(dir.join(name).to_string_lossy().into_owned());
    let mut c = Core::new(CoreConfig {
        manual_ops_audit_path: Some(dir.join("manual-ops.jsonl").to_string_lossy().into_owned()),
        db_path: Some(dir.join("store.sqlite")),
        trade_log_path: log("trades.jsonl"),
        order_log_path: log("orders.jsonl"),
        position_log_path: log("positions.jsonl"),
        dry_seed_balance: Decimal::from(1_000),
        ..Default::default()
    });
    c.set_balance(Decimal::from(1_000));

    // 一个已平仓的交易把 realized 写进账本（重置前的事实）。
    let closed = crate::position::ClosedPosition {
        id: "hft-r1".into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: crate::model::SignalDirection::Up,
        token_id: "tok".into(),
        condition_id: "cond".into(),
        entry_price: Decimal::ONE / Decimal::from(2),
        exit_price: Decimal::ONE,
        shares: Decimal::from(10),
        cost_usd: Decimal::from(5),
        was_maker_entry: true,
        was_maker_exit: false,
        entry_fee_pct: Decimal::ZERO,
        exit_fee_pct: Decimal::ZERO,
        maker_rebate_usd: Decimal::ZERO,
        pnl_usd: Decimal::from(5),
        pnl_pct: Decimal::from(100),
        net_pnl_usd: Decimal::from(94),
        net_pnl_pct: Decimal::from(100),
        high_pnl_pct: Decimal::ZERO,
        low_pnl_pct: Decimal::ZERO,
        hold_time_sec: 0,
        exit_reason: crate::model::ExitReason::TakeProfit,
        entered_at_ms: 0,
        exited_at_ms: 1,
        entry_role: crate::model::OrderRole::Maker,
        exit_role: crate::model::OrderRole::Taker,
        dust_shares: Decimal::ZERO,
        account_id: crate::model::default_account_id(),
        venue: "polymarket".into(),
        source: String::new(),
        original_fallback_price: None,
        price_source: String::new(),
    };
    c.trade_log
        .as_ref()
        .expect("a core with db_path carries a trade ledger")
        .append(&crate::trade::TradeRecord::from_closed(&closed))
        .expect("the pre-reset close lands");
    // 锚点在旧账目上取下：realized=94 成为本轮基线。
    let anchored = c.run_accounting_audit(2);
    assert!(anchored.ok, "pre-reset audit on consistent books is clean");

    // 重置：记录归零、现金重播种——审计状态必须跟着归零。
    c.reset_trading(&reset_params("RESET"), "uid:t", 3).unwrap();

    // 修复断言：下一次审计不带旧锚点——realized 0 对 0，干净取锚不 halt。
    let _after_report = c.run_accounting_audit(4);
    let v = c.accounting_audit_view();
    assert_eq!(
        v["halted"],
        serde_json::json!(false),
        "reset must clear the audit halt: old anchor vs wiped books is not drift; \
         entries must not stay blocked"
    );
    assert_eq!(
        v["anchored"],
        serde_json::json!(true),
        "the post-reset audit re-anchors on the clean books"
    );
}

/// #537 重置漏清牙：reset_trading 清了账本/positions/OME，但
/// 策略页盈亏的源（`strategy_accounting` 的 per-(strategy,account) 切片
/// —— closed_trades/wins/net_pnl_usd）留在了内存里。症状是总览的
/// 「真实余额 = 本金 + 净利」继续按已抹掉的历史加减（操作者 2026-10-19
/// 报告：reset 后余额回 1000，总览却跟着策略页旧盈亏走）。重置后每一个
/// 计数器必须从零开始。
#[test]
fn reset_trading_clears_the_strategy_pnl_counters() {
    let dir = reset_audit_dir("strategy-counters");
    // All three log paths are PRIVATE absolute paths: this reset runs its
    // post-wipe self-check against the log files, and the default
    // cwd-relative `data/…` spellings trade with every parallel test in
    // the suite — a neighbour's lines read as a failed reset.
    let log = |name: &str| Some(dir.join(name).to_string_lossy().into_owned());
    let mut c = Core::new(CoreConfig {
        manual_ops_audit_path: Some(dir.join("manual-ops.jsonl").to_string_lossy().into_owned()),
        db_path: Some(dir.join("store.sqlite")),
        trade_log_path: log("trades.jsonl"),
        order_log_path: log("orders.jsonl"),
        position_log_path: log("positions.jsonl"),
        dry_seed_balance: Decimal::from(1_000),
        ..Default::default()
    });
    c.set_balance(Decimal::from(1_000));

    // A closed trade folds into BOTH stores: the trade ledger and the
    // strategy_accounting slice the strategy page reads.
    let closed = crate::position::ClosedPosition {
        id: "hft-r1".into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: crate::model::SignalDirection::Up,
        token_id: "tok".into(),
        condition_id: "cond".into(),
        entry_price: Decimal::ONE / Decimal::from(2),
        exit_price: Decimal::ONE,
        shares: Decimal::from(10),
        cost_usd: Decimal::from(5),
        was_maker_entry: true,
        was_maker_exit: false,
        entry_fee_pct: Decimal::ZERO,
        exit_fee_pct: Decimal::ZERO,
        maker_rebate_usd: Decimal::ZERO,
        pnl_usd: Decimal::from(5),
        pnl_pct: Decimal::from(100),
        net_pnl_usd: Decimal::from(7),
        net_pnl_pct: Decimal::from(100),
        high_pnl_pct: Decimal::ZERO,
        low_pnl_pct: Decimal::ZERO,
        hold_time_sec: 0,
        exit_reason: crate::model::ExitReason::TakeProfit,
        entered_at_ms: 0,
        exited_at_ms: 1,
        entry_role: crate::model::OrderRole::Maker,
        exit_role: crate::model::OrderRole::Taker,
        dust_shares: Decimal::ZERO,
        account_id: crate::model::default_account_id(),
        venue: "polymarket".into(),
        source: String::new(),
        original_fallback_price: None,
        price_source: String::new(),
    };
    c.trade_log
        .as_ref()
        .expect("a core with db_path carries a trade ledger")
        .append(&crate::trade::TradeRecord::from_closed(&closed))
        .expect("the close lands");
    c.on_position_closed(&closed, 1);
    let pre = c.strategy_stats();
    assert!(
        pre.iter().any(|r| r["netPnlUsd"] == serde_json::json!(7.0)),
        "the counter is live before the reset: {pre:?}"
    );

    c.reset_trading(&reset_params("RESET"), "uid:t", 3).unwrap();

    let post = c.strategy_stats();
    assert!(
        post.iter().all(|r| r["netPnlUsd"] != serde_json::json!(7.0)
            && r["closedTrades"] == serde_json::json!(0)
            && r["wins"] == serde_json::json!(0)),
        "no strategy counter may survive the wipe: {post:?}"
    );
}

#[test]
fn reset_trading_revokes_persisted_strategies_across_restart() {
    // Gitea #285 Option B: reset_trading permanently revokes the target
    // account's strategies in SQLite `account_settings.strategies` so a
    // subsequent boot stays disarmed rather than silently re-arming.
    let dir = reset_audit_dir("reset-revokes-strategies");
    let db_path = dir.join("store.sqlite");
    let audit_path = dir.join("manual-ops.jsonl");
    let log = |name: &str| Some(dir.join(name).to_string_lossy().into_owned());

    let db = crate::risk::account_store::AccountRiskDb::open(&db_path).expect("open risk db");
    db.save_setting(
        "default",
        crate::risk::account_store::STRATEGIES_SETTING,
        Some(r#"["late_favorite"]"#),
    )
    .expect("seed strategies");

    let cfg = CoreConfig {
        manual_ops_audit_path: Some(audit_path.to_string_lossy().into_owned()),
        db_path: Some(db_path.clone()),
        trade_log_path: log("trades.jsonl"),
        order_log_path: log("orders.jsonl"),
        position_log_path: log("positions.jsonl"),
        ..CoreConfig::default()
    };
    let mut c = Core::new(cfg.clone());
    c.account_risk_db = Some(db.clone());
    c.accounts
        .set_strategies(
            &crate::model::default_account_id(),
            vec!["late_favorite".into()],
            1,
        )
        .expect("seed in-memory strategies");

    c.reset_trading(&reset_params("RESET"), "uid:t", 3)
        .expect("reset succeeds");

    // 1. Persisted SQLite setting is explicitly revoked to "[]".
    let settings = db.load_settings().expect("load settings");
    let persisted = settings
        .get("default")
        .and_then(|m| m.get(crate::risk::account_store::STRATEGIES_SETTING))
        .map(String::as_str);
    assert_eq!(
        persisted,
        Some("[]"),
        "reset_trading must persist revoked strategies as [] in SQLite"
    );
    // 2. In-memory account book matches SQLite.
    let in_mem = c
        .accounts
        .account(&crate::model::default_account_id())
        .map(|a| a.strategies.clone())
        .unwrap_or_default();
    assert!(
        in_mem.is_empty(),
        "in-memory account strategies must be empty after reset: {in_mem:?}"
    );

    // 3. Simulated restart: a fresh Core reading the same SQLite DB must boot
    //    with 0 strategies armed for `default`.
    let db2 = crate::risk::account_store::AccountRiskDb::open(&db_path).expect("reopen risk db");
    let mut c2 = Core::new(cfg);
    c2.account_risk_db = Some(db2.clone());
    let settings2 = db2.load_settings().expect("reload settings after restart");
    let reloaded = settings2
        .get("default")
        .and_then(|m| m.get(crate::risk::account_store::STRATEGIES_SETTING))
        .map(String::as_str);
    assert_eq!(reloaded, Some("[]"));
    let parsed: Vec<String> = serde_json::from_str(reloaded.unwrap_or("[]")).unwrap();
    c2.accounts
        .set_strategies(&crate::model::default_account_id(), parsed, 4)
        .expect("hydrate strategies on restart");
    let boot_armed = c2
        .apply_account_strategy_assignments_at_boot()
        .expect("boot assignment succeeds");
    assert!(
        boot_armed.is_empty(),
        "restart after reset_trading must keep the account disarmed: {boot_armed:?}"
    );
}
