#![cfg(test)]

use super::*;

// ── #363: the execution policy surface, at the wire ─────────────────────

/// #386 — the fixture contract that killed the flake: two cores NEVER
/// share an audit journal. The CI failure (run 37262356151) had two
/// parallel tests appending to the ONE fixed `data/audit/…` file; their
/// records fused into a single unparseable line and BOTH history counts
/// came back empty (`left: 0, right: 1`). Each fixture now carries its
/// own journal inside its own scratch root — this test pins that a set
/// through one core lands in that core's journal alone, and the other
/// core's history stays empty.
#[tokio::test]
async fn policy_journals_are_private_per_fixture() {
    let (a, reg_a, peer_a, path_a) = policy_fixture().await;
    let (b, reg_b, peer_b, path_b) = policy_fixture().await;
    assert_ne!(path_a, path_b, "each fixture gets its own scratch root");

    let v = rpc(&a, &reg_a, &peer_a,
        r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.set","params":{"accountId":"acct-x","budgetRatio":"0.05","maxBudgetUsd":"20"}}"#.into())
        .await;
    assert!(v.get("error").is_none(), "set on core A answers: {v}");
    let v = rpc(&b, &reg_b, &peer_b,
        r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.set","params":{"accountId":"acct-y","budgetRatio":"0.06","maxBudgetUsd":"25"}}"#.into())
        .await;
    assert!(v.get("error").is_none(), "set on core B answers: {v}");

    // Each journal carries exactly its own core's record — no bleed.
    let ja = a.lock().await.execution_policy_audit();
    assert_eq!(ja.len(), 1, "core A's journal: {ja:?}");
    assert_eq!(ja[0].account_id, "acct-x");
    let jb = b.lock().await.execution_policy_audit();
    assert_eq!(jb.len(), 1, "core B's journal: {jb:?}");
    assert_eq!(jb[0].account_id, "acct-y");

    // And the journals are physically inside each fixture's scratch root
    // (the cleanup at the tests' end removes them with the policy file).
    for path in [&path_a, &path_b] {
        let journal = path
            .parent()
            .expect("scratch root")
            .join("data/audit/execution_policy.jsonl");
        assert!(journal.is_file(), "private journal exists");
    }
    let _ = std::fs::remove_dir_all(path_a.parent().unwrap());
    let _ = std::fs::remove_dir_all(path_b.parent().unwrap());
}

/// 2026-11-15 操作者裁决（#1236 追加）：全新安装**不再自举生成**
/// `execution_policy.toml`。这条测试原断言"内核模板首次保存被 sweep"——
/// 生成能力删除后它失去对象。改为钉住新语义：一份磁盘上遗留的 toml
/// （无论来源）是死数据——写入放行、文件原样不动、段落落 store。
#[tokio::test]
async fn a_set_proceeds_past_a_leftover_policy_toml_and_leaves_it_untouched() {
    let (core, registry, peer, env_path) = policy_fixture().await;
    let toml_path = {
        let c = core.lock().await;
        std::path::PathBuf::from(c.config().execution_policy_path.clone().unwrap())
    };
    std::fs::write(
        &toml_path,
        "version = 1\n\n[defaults]\nbudget_ratio = 0.10\n",
    )
    .expect("seed a leftover policy toml");

    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.set","params":{"accountId":"acct-t","budgetRatio":"0.05","maxBudgetUsd":"20"}}"#.into())
        .await;
    assert!(
        v.get("error").is_none(),
        "dead data must not block the write: {v}"
    );
    assert!(
        toml_path.exists(),
        "a proceeding write never deletes the file"
    );
    // 操作者裁决 2026-11-14：段落在 SQLite 的 account_settings 行里。
    let rows = crate::risk::account_store::AccountRiskDb::open(
        env_path.parent().unwrap().join("store.sqlite"),
    )
    .expect("the store re-opens")
    .load_settings()
    .expect("settings readable");
    assert_eq!(
        rows.get("acct-t")
            .and_then(|s| s.get("POLICY_BUDGET_RATIO_ACCT_T"))
            .map(String::as_str),
        Some("0.05"),
        "the section landed in the store the loader reads: {rows:?}"
    );
    let _ = std::fs::remove_dir_all(env_path.parent().unwrap());
}

/// Issue #965's guard half, UPDATED for the SQLite-only loader (#1199 第六轮 +
/// 2026-11-15 操作者实机报告): an EDITED (real, 0.4.1-era) toml at the policy
/// path is DEAD DATA — the loader reads the store's POLICY_* rows only, so the
/// file cannot form a second source. Refusing here blocked EVERY panel save
/// forever ("完全无法保存"); the write now proceeds with a loud warn, the file
/// is left untouched (never deleted without the operator's say-so), and the
/// section lands in the store the loader reads.
#[tokio::test]
async fn a_set_proceeds_past_an_operator_edited_policy_file_with_a_warn() {
    let (core, registry, peer, env_path) = policy_fixture().await;
    let toml_path = {
        let c = core.lock().await;
        std::path::PathBuf::from(c.config().execution_policy_path.clone().unwrap())
    };
    std::fs::write(
        &toml_path,
        "version = 1\n\n[defaults]\nbudget_ratio = 0.9\n",
    )
    .expect("seed an operator-edited file");

    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.set","params":{"accountId":"acct-o","budgetRatio":"0.05"}}"#.into())
        .await;
    assert!(
        v.get("error").is_none(),
        "dead data must not block the store write: {v}"
    );
    assert!(
        toml_path.exists(),
        "a proceeding write never deletes the operator's file"
    );
    // 操作者裁决 2026-11-14：段落在 SQLite 的 account_settings 行里。
    let rows = crate::risk::account_store::AccountRiskDb::open(
        env_path.parent().unwrap().join("store.sqlite"),
    )
    .expect("the store re-opens")
    .load_settings()
    .expect("settings readable");
    assert_eq!(
        rows.get("acct-o")
            .and_then(|s| s.get("POLICY_BUDGET_RATIO_ACCT_O"))
            .map(String::as_str),
        Some("0.05"),
        "the section landed in the store despite the dead toml: {rows:?}"
    );
    let _ = std::fs::remove_dir_all(env_path.parent().unwrap());
}

/// #1253 acceptance: a `maxBudgetUsd` at the "cap disables itself" magnitude —
/// the exact value found in production `account_settings` — is refused on the
/// wire, and no POLICY_ row lands in the store. A typo must not be able to
/// raise the ceiling to `≈ unlimited`.
#[tokio::test]
async fn an_absurd_budget_cap_is_refused_and_the_store_is_untouched() {
    let (core, registry, peer, env_path) = policy_fixture().await;
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.set","params":{"accountId":"acct-big","maxBudgetUsd":"10222221221331331"}}"#.into())
        .await;
    assert!(
        v.get("error").is_some(),
        "the absurd cap must be refused: {v}"
    );

    let rows = crate::risk::account_store::AccountRiskDb::open(
        env_path.parent().unwrap().join("store.sqlite"),
    )
    .expect("the store re-opens")
    .load_settings()
    .expect("settings readable");
    assert!(
        !rows
            .get("acct-big")
            .map(|s| s.keys().any(|k| k.starts_with("POLICY_")))
            .unwrap_or(false),
        "no POLICY_ row may land for the refused section: {rows:?}"
    );
    let _ = std::fs::remove_dir_all(env_path.parent().unwrap());
}

/// #1253: the set reply carries `written` (the accepted section) and
/// `readBack` (the reloaded view the kernel judges under) — the caller
/// verifies the save without a follow-up get.
#[tokio::test]
async fn a_set_reply_carries_written_and_read_back() {
    let (core, registry, peer, env_path) = policy_fixture().await;
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.set","params":{"accountId":"acct-r","budgetRatio":"0.05","maxBudgetUsd":"20"}}"#.into())
        .await;
    assert!(v.get("error").is_none(), "set answers: {v}");
    let res = v.get("result").expect("success reply");
    assert_eq!(res["accountId"], "acct-r");
    assert!(
        res.get("written").is_some_and(|w| !w.is_null()),
        "written names the accepted section: {res}"
    );
    assert!(
        res.get("readBack").is_some_and(|r| !r.is_null()),
        "readBack is the reloaded view: {res}"
    );
    let _ = std::fs::remove_dir_all(env_path.parent().unwrap());
}
