//! `risk.setExit` wire contract — Gate 4 exit triple, live and atomic.
//! Moved verbatim from `tests_risk_rounds.rs` (pure code motion).

use super::super::*;

/// 退出纪律可编辑（`risk.setExit`）：写路径的 wire 合同 —— patch 落进
/// Gate 4 绑定的活配置（readout 立即读回新值）、回执逐项带 old→new 与
/// effect=live、空 patch 拒绝、非法值拒绝且原子（同行 ride-along 的合法
/// 字段也不落）、`forceExitSec: 0` 是文档化的「关闭强平」开关照常接受。
/// 下一笔入场按新值绑定由 arbitration/pipeline.rs 的 Gate 4 用例覆盖。
#[tokio::test]
async fn risk_set_exit_writes_the_triple_and_names_each_change() {
    let (core, registry, peer) = hot_reload_fixture().await;

    // Arm all three fields at once; the reply names each old→new.
    // trailingEnabled rides the same patch: the calibration default is
    // OFF (issue 356: the proven operator never sells into profit), and
    // the hot-write face is where an operator re-arms it without a
    // restart — the single-entry behaviour this round's spread_arb
    // losses exposed (peak +46.7% given back to a -13% stop).
    let applied = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setExit","params":{"accountId":"default","stopLossPct":"8","takeProfitPct":"90","forceExitSec":120,"trailingEnabled":true,"reason":"tighten"}}"#.to_string(),
    )
    .await;
    assert!(
        applied.get("error").is_none(),
        "the exit patch must be accepted: {applied}"
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
    let change_of = |field: &str| {
        audit["applied"]
            .as_array()
            .expect("an applied list")
            .iter()
            .find(|c| c["field"] == serde_json::json!(field))
            .map(|c| {
                (
                    c["from"].as_str().expect("from").to_string(),
                    c["to"].as_str().expect("to").to_string(),
                    c["effect"].as_str().expect("effect").to_string(),
                )
            })
    };
    // The factory calibration: SL 12, TP 100, guillotine OFF (0).
    assert_eq!(
        change_of("stopLossPct"),
        Some(("12".into(), "8".into(), "live".into())),
        "the stop row carries old→new and live: {audit}"
    );
    assert_eq!(
        change_of("takeProfitPct"),
        Some(("100".into(), "90".into(), "live".into())),
        "the take-profit row carries old→new and live: {audit}"
    );
    assert_eq!(
        change_of("forceExitSec"),
        Some(("0".into(), "120".into(), "live".into())),
        "the guillotine row carries old→new and live: {audit}"
    );
    assert_eq!(
        change_of("trailingEnabled"),
        Some(("false".into(), "true".into(), "live".into())),
        "the trailing row carries old→new and live — the calibration default OFF re-arms hot: {audit}"
    );

    // The readout answers with the new values.
    let read = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"risk.limits","params":{}}"#.to_string(),
    )
    .await;
    assert!(read.get("error").is_none(), "readout must answer: {read}");
    // The exit view crosses as NUMBERS (finite decimals serialize as
    // numbers), not the Bound strings the systemic matrix uses.
    assert_eq!(
        read["result"]["exit"]["stopLossPct"],
        serde_json::json!(8.0)
    );
    assert_eq!(
        read["result"]["exit"]["takeProfitPct"],
        serde_json::json!(90.0)
    );
    assert_eq!(
        read["result"]["exit"]["forceExitSec"],
        serde_json::json!(120)
    );
    assert_eq!(
        read["result"]["exit"]["trailingEnabled"],
        serde_json::json!(true),
        "the readout carries the arming switch the panel renders"
    );

    // The write path is live, not just the receipt: the arming switch the
    // exit sweep reads every tick moved with it (a flipped trailing flag
    // without a config move would be a receipt lie).
    {
        let c = core.lock().await;
        assert!(
            c.config().positions.exit.trailing_enabled,
            "the write phase must move the LIVE exit config"
        );
    }

    // forceExitSec: 0 is the documented OFF switch, accepted.
    let off = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"risk.setExit","params":{"accountId":"default","forceExitSec":0}}"#
            .to_string(),
    )
    .await;
    assert!(
        off.get("error").is_none(),
        "0 = guillotine OFF must be accepted: {off}"
    );

    // A zero percentage is refused — and atomicity means the legal field
    // riding along did NOT land either.
    let zero = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"risk.setExit","params":{"accountId":"default","stopLossPct":"0","takeProfitPct":"50"}}"#.to_string(),
    )
    .await;
    assert!(
        zero["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("must be > 0"),
        "a zero percentage is refused with the field named: {zero}"
    );
    let read2 = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":5,"method":"risk.limits","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        read2["result"]["exit"]["takeProfitPct"],
        serde_json::json!(90.0),
        "a refused patch leaves no trace: {read2}"
    );

    // Empty patch is refused.
    let empty = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":6,"method":"risk.setExit","params":{}}"#.to_string(),
    )
    .await;
    assert!(
        empty["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("at least one"),
        "an empty patch is refused: {empty}"
    );
}
