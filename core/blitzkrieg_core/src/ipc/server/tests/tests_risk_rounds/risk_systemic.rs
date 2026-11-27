//! `risk.setSystemic` wire contract — issue #191 acceptance (a) and (b).
//! Moved verbatim from `tests_risk_rounds.rs` (pure code motion).

use super::super::*;

/// #191 acceptance (a): one `risk.setSystemic` call moves the per-account
/// notional cap without a restart, answers with the audit record
/// (old → new, actor, instant), and the very NEXT order is judged by the NEW
/// value.
#[tokio::test]
async fn risk_set_systemic_binds_next_order_without_restart() {
    let (core, registry, peer) = hot_reload_fixture().await;
    let feed = rpc(&core, &registry, &peer, books_line()).await;
    assert!(feed.get("error").is_none(), "feed the book first: {feed}");

    // 2.00 USD against the startup default (cap off): MUST be admitted.
    let before = rpc(
        &core,
        &registry,
        &peer,
        order_line(2, "probe-before", "BTC", "up"),
    )
    .await;
    assert!(
        before.get("error").is_none(),
        "2.00 must pass with no cap set: {before}"
    );
    assert_eq!(
        before["result"]["status"],
        serde_json::json!("FILLED"),
        "the probe must be a real, filled order: {before}"
    );

    let applied = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"risk.setSystemic","params":{"accountId":"default","maxOrderNotionalUsd":1,"reason":"tighten after drawdown"}}"#.to_string(),
    )
    .await;
    assert!(
        applied.get("error").is_none(),
        "the safe subset must be accepted: {applied}"
    );
    let audit = &applied["result"];
    assert_eq!(
        audit["actor"],
        serde_json::json!(format!("uid:{}", own_uid())),
        "who did it must be on the record: {audit}"
    );
    assert_eq!(audit["reason"], serde_json::json!("tighten after drawdown"));
    assert!(
        audit["atMs"].as_i64().unwrap_or(0) > 0,
        "when it happened must be on the record: {audit}"
    );
    let change = |field: &str| {
        audit["applied"]
            .as_array()
            .expect("an applied list")
            .iter()
            .find(|c| c["field"] == serde_json::json!(field))
            .cloned()
            .unwrap_or_else(|| panic!("{field} must be in the audit record: {audit}"))
    };
    assert_eq!(
        change("maxOrderNotionalUsd")["from"],
        serde_json::json!("0"),
        "the audit keeps the OLD value (factory off)"
    );
    assert_eq!(change("maxOrderNotionalUsd")["to"], serde_json::json!("1"));

    // `engine.stats.sizing` reads live off the systemic matrix — the
    // no-restart half: the new cap is in force immediately.
    let stats = rpc(&core, &registry, &peer, stats_line(4)).await;
    let sizing = &stats["result"]["sizing"];
    assert_eq!(sizing["maxOrderNotionalUsd"], serde_json::json!(1.0));

    // The SAME 2.00 ticket, on another asset (so the only possible reason is
    // the cap, never the one-position-per-asset gate), judged by the NEW cap.
    let after = rpc(
        &core,
        &registry,
        &peer,
        order_line(5, "probe-after", "ETH", "down"),
    )
    .await;
    assert_eq!(
        after["error"]["data"]["coreCode"],
        serde_json::json!("RISK_REJECTED"),
        "the next order must be judged by the new cap: {after}"
    );
    assert!(
        after["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("exceeds per-order cap 1"),
        "the refusal must name the NEW cap: {after}"
    );
}

/// 生效风控可编辑（`risk.setSystemic`）：写路径的 wire 合同 —— patch
/// 落进 matrix（readout 立即读回 flag 来源的新值）、回执逐项带 effect
/// （live / next_session）与 old→new、空 patch 拒绝、负值拒绝且原子
/// （同行 ride-along 的合法字段也不落）。下一笔入场按新值裁决由
/// `engine_evaluate` → `process_intent` 的 §4.2 测试矩阵覆盖（arbitration/
/// pipeline.rs 的 judge_entry 用例），这里不重复架一台引擎。
#[tokio::test]
async fn risk_set_systemic_writes_the_matrix_and_names_each_effect() {
    let (core, registry, peer) = hot_reload_fixture().await;

    // Arm ONE live bound + the drawdown row, which carries live (issue #1199 P1-6).
    let applied = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"accountId":"default","maxPositionSize":1,"maxDailyDrawdownUsd":50,"reason":"live arm"}}"#.to_string(),
    )
    .await;
    assert!(
        applied.get("error").is_none(),
        "the systemic patch must be accepted: {applied}"
    );
    let audit = &applied["result"];
    assert_eq!(audit["persisted"], serde_json::json!(false));
    assert_eq!(
        audit["persistence"],
        serde_json::json!("none"),
        "the receipt names the carrier: no durable carrier wired → none: {audit}"
    );
    assert_eq!(
        audit["actor"],
        serde_json::json!(format!("uid:{}", own_uid())),
        "who did it must be on the record: {audit}"
    );
    let effect_of = |field: &str| {
        audit["applied"]
            .as_array()
            .expect("an applied list")
            .iter()
            .find(|c| c["field"] == serde_json::json!(field))
            .map(|c| c["effect"].as_str().expect("effect").to_string())
    };
    assert_eq!(
        effect_of("maxPositionSize").as_deref(),
        Some("live"),
        "the size cap is a live bound: {audit}"
    );
    assert_eq!(
        effect_of("maxDailyDrawdownUsd").as_deref(),
        Some("live"),
        "the drawdown budget must state its live arming: {audit}"
    );
    let from_of = |field: &str| {
        audit["applied"]
            .as_array()
            .expect("an applied list")
            .iter()
            .find(|c| c["field"] == serde_json::json!(field))
            .map(|c| c["from"].as_str().expect("from").to_string())
    };
    assert_eq!(
        from_of("maxPositionSize").as_deref(),
        Some("0"),
        "the old value (0 = off) is on the record: {audit}"
    );

    // The readout answers with the new values, flag-sourced.
    let read = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"risk.limits","params":{}}"#.to_string(),
    )
    .await;
    assert!(read.get("error").is_none(), "readout must answer: {read}");
    assert_eq!(
        read["result"]["account"]["limits"]["maxPositionSize"]["value"],
        serde_json::json!("1")
    );
    assert_eq!(
        read["result"]["account"]["limits"]["maxPositionSize"]["source"],
        serde_json::json!("flag")
    );
    assert_eq!(
        read["result"]["account"]["limits"]["maxDailyDrawdownUsd"]["value"],
        serde_json::json!("50")
    );

    // Empty patch is refused.
    let empty = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"risk.setSystemic","params":{}}"#.to_string(),
    )
    .await;
    assert!(
        empty["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("at least one"),
        "an empty patch is refused: {empty}"
    );

    // A negative value is refused with the field named — and the atomicity
    // means the legal field riding along did NOT land either.
    let negative = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"risk.setSystemic","params":{"accountId":"default","maxSingleLossUsd":"-5","maxConsecutiveLosses":10}}"#.to_string(),
    )
    .await;
    assert!(
        negative["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("must be >= 0"),
        "a negative bound is refused with the field named: {negative}"
    );
    let read2 = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":5,"method":"risk.limits","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        read2["result"]["account"]["limits"]["maxConsecutiveLosses"]["value"],
        serde_json::json!("0"),
        "a refused patch leaves no trace: {read2}"
    );
}

/// #191 acceptance (b): a knob OUTSIDE the safe subset is refused BY NAME,
/// with the reason and with what to do instead — and the refusal is atomic,
/// so an allowed field riding along in the same patch is not applied either.
#[tokio::test]
async fn risk_set_limits_refuses_a_forbidden_field_and_applies_nothing() {
    let (core, registry, peer) = hot_reload_fixture().await;

    let feed = rpc(&core, &registry, &peer, books_line()).await;
    assert!(feed.get("error").is_none(), "feed the book first: {feed}");
    let before = rpc(&core, &registry, &peer, stats_line(2)).await;

    // A double carrier: `maxOrderNotionalPct` is inside the safe subset,
    // `stopLossPct` is an exit threshold. The whole request must be refused.
    let refused = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"risk.setLimits","params":{"maxOrderNotionalPct":1,"stopLossPct":"0.50"}}"#.to_string(),
    )
    .await;
    assert_eq!(
        refused["error"]["code"],
        serde_json::json!(-32602),
        "a refused patch is INVALID_PARAMS, not a silent no-op: {refused}"
    );
    let msg = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("stopLossPct"),
        "the refusal must name the field: {refused}"
    );
    assert!(
        msg.contains("not hot-reloadable"),
        "and say what is wrong with it: {refused}"
    );
    assert!(
        msg.contains("a restart applies it"),
        "and how to get it: {refused}"
    );
    assert!(
        msg.contains("maxOrderNotionalPct"),
        "and what CAN be changed live: {refused}"
    );

    // Nothing moved — neither the refused knob nor the allowed one that came
    // with it. `sizing` is the live view of every number this call could have
    // touched.
    let after = rpc(&core, &registry, &peer, stats_line(4)).await;
    assert_eq!(
        before["result"]["sizing"], after["result"]["sizing"],
        "a refused patch must leave the live limits exactly as they were"
    );

    // ...and the order path agrees: this 2.00 ticket still passes, so the
    // `maxOrderNotional: 1` that rode along was NOT applied (the same ticket
    // IS refused once that field really is applied — see the test above).
    let probe = rpc(
        &core,
        &registry,
        &peer,
        order_line(5, "probe-after-refusal", "BTC", "up"),
    )
    .await;
    assert!(
        probe.get("error").is_none(),
        "the cap must still be the startup 3 USD after a refused patch: {probe}"
    );
    assert_eq!(
        probe["result"]["status"],
        serde_json::json!("FILLED"),
        "the probe must be a real, filled order: {probe}"
    );
}
