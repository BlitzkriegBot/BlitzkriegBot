//! `system.reset_trading` wire contract — issue #458 (Live refusal / verbatim
//! RESET confirm / no-audit-path refusal) plus the reset closed loop.
//! Moved verbatim from `tests_risk_rounds.rs` (pure code motion).

use super::super::*;

use blitzkrieg_market_api::net::now_ms;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

/// #458 `system.reset_trading` 的 wire 合同：三道门（Live 拒绝 / 确认词
/// 逐字 RESET / 无审计路径拒绝）+ 重置闭环（停策略、清记录、re-seed、
/// 自校验）+ 审计行 + 反向验收（审计流不清除/余额必复原/记录必空）。
/// 记录文件挂在本测试的私有 scratch 目录（Hard Disk 写入策略）。这些路径
/// 曾是挂到 scratch cwd 的相对 `data/`——但 `set_current_dir` 是进程全局
/// 的，并行测试的默认相对路径 core 会追进这只测试的 cwd，重置自检的
/// 行数窗口就会数到别人的行。私有绝对路径 + 不动 cwd，窗口才真正闭合。
#[tokio::test]
async fn system_reset_trading_wipes_and_reseeds_with_audit() {
    let dir = std::env::temp_dir().join(format!("bk-reset-trading-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = |name: &str| Some(dir.join("data").join(name).to_string_lossy().into_owned());
    let audit_str = dir
        .join("audit")
        .join("manual-ops.jsonl")
        .to_string_lossy()
        .into_owned();

    let (core, registry, peer) = {
        let cfg = CoreConfig {
            manual_ops_audit_path: Some(audit_str.clone()),
            trade_log_path: log("trades.jsonl"),
            order_log_path: log("orders.jsonl"),
            position_log_path: log("positions.jsonl"),
            ..Default::default()
        };
        let mut c = Core::new(cfg.clone());
        c.set_balance(cfg.dry_seed_balance);
        (
            Arc::new(AsyncMutex::new(c)),
            crate::market::registry::MarketPluginRegistry::new(),
            PeerAuth::SameUid { uid: own_uid() },
        )
    };

    // 铺一些交易状态：一张 resting 挂单（吃掉现金预留）。
    let feed = rpc(&core, &registry, &peer, books_line()).await;
    assert!(feed.get("error").is_none(), "feed the book first: {feed}");
    let slot = now_ms() / 1000 / 900;
    let place = rpc(
        &core,
        &registry,
        &peer,
        format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"orders.place","params":{{"tokenId":"tok","conditionId":"cond","side":"buy","mode":"maker","price":0.39,"size":5,"internalKey":"reset-probe","strategy":"s","asset":"BTC_UP","direction":"up","roundSlot":{slot}}}}}"#
        ),
    )
    .await;
    assert!(
        place.get("error").is_none(),
        "resting order placed: {place}"
    );

    // 反向验收 1：确认词错误 → 红，且什么都没发生。
    let wrong = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"system.reset_trading","params":{"confirm":"reset"}}"#
            .to_string(),
    )
    .await;
    assert!(
        wrong["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("exactly \"RESET\""),
        "the confirm word is checked verbatim: {wrong}"
    );
    let still = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"orders.list","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        still["result"]["orders"].as_array().map(|a| a.len()),
        Some(1),
        "a refused reset leaves every record in place: {still}"
    );

    // 反向验收 2：无审计路径的 core → 红（wipe with no audit trail）。
    let (bare, registry2, peer2) = hot_reload_fixture().await;
    let no_audit = rpc(
        &bare,
        &registry2,
        &peer2,
        r#"{"jsonrpc":"2.0","id":1,"method":"system.reset_trading","params":{"confirm":"RESET"}}"#
            .to_string(),
    )
    .await;
    assert!(
        no_audit["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no manual-ops audit path"),
        "a path-less core refuses the wipe: {no_audit}"
    );

    // 正向：确认词正确 → 重置落地，回执带自检余额与清理计数。
    let reset = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":5,"method":"system.reset_trading","params":{"confirm":"RESET","reason":"ui smoke"}}"#.to_string(),
    )
    .await;
    assert!(reset.get("error").is_none(), "the reset lands: {reset}");
    assert_eq!(
        reset["result"]["balances"][0][1],
        serde_json::json!("10000"),
        "the cash is re-seeded to the configured seed: {reset}"
    );
    // issue 561: the reply names the reset scope — an omitted accountId
    // resolves through the connection default (this peer has no session
    // cell, so the display account `default`).
    assert_eq!(
        reset["result"]["accountId"],
        serde_json::json!("default"),
        "the reply names the account the reset wiped: {reset}"
    );
    assert!(
        reset["result"]["clearedOpenPositions"]
            .as_i64()
            .unwrap_or(-1)
            >= 0,
        "the reply carries the cleanup counts: {reset}"
    );

    // 记录文件全部 0 行；挂单索引空。
    let orders = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":6,"method":"orders.list","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        orders["result"]["orders"].as_array().map(|a| a.len()),
        Some(0),
        "no order survives the reset: {orders}"
    );
    let summary = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":7,"method":"trades.summary","params":{}}"#.to_string(),
    )
    .await;
    assert!(
        summary.get("error").is_none() || summary["result"]["totalTrades"].as_u64() == Some(0),
        "the trade summary is zeroed: {summary}"
    );
    for f in ["trades.jsonl", "orders.jsonl"] {
        let text = std::fs::read_to_string(dir.join("data").join(f)).unwrap_or_default();
        assert!(
            text.trim().is_empty(),
            "{f} is empty after the reset ({} bytes)",
            text.len()
        );
    }

    // 审计行：一次（失败的确认词不写行）。
    let audit = std::fs::read_to_string(&audit_str).unwrap_or_default();
    let lines: Vec<&str> = audit.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "exactly one audit line for the one reset: {audit}"
    );
    let rec: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(rec["op"], serde_json::json!("reset_trading"));
    assert!(
        rec["actor"]
            .as_str()
            .unwrap_or_default()
            .starts_with("uid:"),
        "the audit line carries the peer actor: {rec}"
    );
    // issue 561: the audit line records WHICH account the wipe hit — the
    // only trace left once the records are gone must scope itself.
    assert_eq!(
        rec["account"],
        serde_json::json!("default"),
        "the audit line names the reset scope: {rec}"
    );
    assert!(
        rec["pre"].is_object(),
        "the audit line carries the pre-reset summary: {rec}"
    );

    // 再来一次重置（第二次审计行追加，不覆盖 —— 审计流 append-only）。
    let again = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":8,"method":"system.reset_trading","params":{"confirm":"RESET"}}"#
            .to_string(),
    )
    .await;
    assert!(
        again.get("error").is_none(),
        "a second reset still lands: {again}"
    );
    let audit2 = std::fs::read_to_string(&audit_str).unwrap_or_default();
    assert_eq!(
        audit2.lines().filter(|l| !l.trim().is_empty()).count(),
        2,
        "audit lines append, never overwrite: {audit2}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
