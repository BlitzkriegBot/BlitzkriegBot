//! Empty strategy assignment = the account runs nothing (INHERIT abolished
//! by operator ruling: explicit assignment is the only truth source).
//! Moved verbatim from `dispatch_boot_enablement_tests.rs` (pure code motion).

use super::super::*;
use super::helpers::stored_strategy_set;

/// 空分配 = 该账户零策略（INHERIT 已按操作者裁决废除：显式分配是唯一
/// 真相源，default 账户一视同仁）。旧行为把 `{default, []}` 解析成
/// 「继承 boot 已装集」，DROP 阶段反而因此漏拆 —— 新语义下空分配把
/// 无持仓的 scope 全数拆掉，文件同样按 absence 拼写「不跑任何策略」。
#[test]
fn strategies_set_empty_on_default_runs_nothing_explicitly() {
    let dir = std::env::temp_dir().join(format!("bk-inherit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default,paper\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_PAPER_NAME=\"paper\"\n",
    )
    .expect("fixture");
    let mut c = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path,
            Some(&env_path),
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    c.open_account_risk_db(dir.join("store.sqlite"))
        .expect("settings store opens");
    c.enable_engine(Engine::new(EngineConfig::default()));
    crate::strategies::test_support::host(
        c.engine.as_mut().unwrap(),
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    // Boot shape: one enabled default scope (what strategy-state.json replays).
    assert!(
        c.engine
            .as_mut()
            .unwrap()
            .set_strategy_enabled("spread_arb", true)
    );

    // The empty write is explicit "run nothing": every default scope
    // drops outright — the enabled one included. The trim is a decision
    // (durable exposure would ride the exits-only DEMOTE had positions
    // been held; none are).
    let kept = c
        .set_account_strategies_persisting(
            &crate::account::AccountId::from("default"),
            Vec::new(),
            "test",
            "explicit stop",
            2,
        )
        .expect("the empty write lands");
    assert!(
        kept.is_empty(),
        "no positions are held, so nothing is promoted exits-only"
    );
    let e = c.engine.as_ref().unwrap();
    // `strategy_registered` is the ASSIGNABLE set now (sticky provenance,
    // teardown-proof); scope liveness is `strategy_scopes`.
    let scopes = |name: &str| e.strategy_scopes(name);
    assert!(
        scopes("spread_arb").is_empty()
            && scopes("trend_follow").is_empty()
            && scopes("mean_reversion").is_empty(),
        "an empty assignment runs nothing — every flat scope drops, enabled included"
    );
    assert!(
        e.strategy_registered("spread_arb"),
        "the package stays assignable after the teardown (sticky provenance)"
    );
    assert!(
        e.enabled_strategies().is_empty(),
        "nothing enabled remains after the empty write"
    );
    // The persisted file spells the empty assignment by absence (the
    // config half's own牙).
    let text = std::fs::read_to_string(&env_path).unwrap();
    assert!(
        !text.contains("_STRATEGIES"),
        "empty spelled by absence: {text}"
    );

    // A NAMED assignment restores exactly one scope. The rebuild's ADD
    // phase (incl. "assignment IS enablement") is pinned by the engine
    // tests (`rebuild_drops_an_enabled_scope_the_assignment_omits`); the
    // in-process test adapters are not Lua packages, so a rebuilt scope
    // here would need the source seam — the FILE half is what this test
    // adds on top of the engine teeth.
    c.set_account_strategies_persisting(
        &crate::account::AccountId::from("default"),
        vec!["spread_arb".into()],
        "test",
        "named restore",
        3,
    )
    .expect("the named write lands (build fn declines a non-Lua source: warn, no panic)");
    let book_assignment = c
        .accounts
        .strategy_assignment(&crate::account::AccountId::from("default"))
        .expect("the named assignment is in the book");
    assert_eq!(
        book_assignment,
        &vec!["spread_arb".to_string()],
        "the book spells the named assignment"
    );
    assert_eq!(
        stored_strategy_set(&dir, "default").as_deref(),
        Some("[\"spread_arb\"]"),
        "the assignment lands as an account_settings row"
    );
    let text = std::fs::read_to_string(&env_path).unwrap_or_default();
    assert!(
        !text.contains("ACCOUNT_DEFAULT_STRATEGIES"),
        "the strategy set never lands in .env: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
