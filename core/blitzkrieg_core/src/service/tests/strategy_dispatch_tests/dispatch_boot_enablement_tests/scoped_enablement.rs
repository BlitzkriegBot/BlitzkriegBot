//! Per-account enable/disable persists and boots back exactly; the default
//! account's scoped write and the global toggle sync bidirectionally and
//! boot never resurrects a disabled name from a stale state file (#537).
//! Moved verbatim from `dispatch_boot_enablement_tests.rs` (pure code motion).

use super::super::*;
use super::helpers::stored_strategy_set;

/// 启用集持久化 boot 回放牙：per-account enable/disable 的写路径落盘
/// （persist FIRST，一事务），重启（新 Core + 同一文件）后 scope 集精确
/// 复原 —— paper 的 scope 回来了、default 的没有，启用状态逐 scope 一致。
#[test]
fn scoped_enablement_persists_and_boots_back_exactly() {
    let dir = std::env::temp_dir().join(format!("bk-scope-enable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default,paper\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_PAPER_NAME=\"paper\"\n",
    )
    .expect("fixture");
    let state_path = dir.join("strategy-state.json");

    let mut c = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            strategy_state_path: Some(state_path.to_string_lossy().into_owned()),
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
    crate::strategies::test_support::host_disabled(
        c.engine.as_mut().unwrap(),
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    let paper = crate::account::AccountId::from("paper");

    // ENABLE trend_follow FOR paper: the set lands on disk.
    let (kept, set_now) = c
        .set_scoped_strategy_enabled(&paper, "trend_follow", true, "test", "panel on", 2)
        .expect("the scoped enable lands");
    assert!(kept.is_empty(), "nothing held → nothing demoted");
    assert_eq!(set_now, vec!["trend_follow".to_string()]);
    // The GLOBAL toggle is untouched (spread_arb's default scope state is
    // its own; trend_follow's global row does not exist to be touched).
    assert!(
        !c.enabled_strategy_names()
            .contains(&"trend_follow".to_string()),
        "the per-account write must not flip the global switch"
    );
    // strategy.list's accounts column reads the book.
    assert_eq!(
        c.strategy_enabled_accounts("trend_follow"),
        vec!["paper".to_string()],
        "paper is the only account whose enablement set names it"
    );
    assert_eq!(
        stored_strategy_set(&dir, "paper").as_deref(),
        Some("[\"trend_follow\"]"),
        "paper's enablement is its own row"
    );

    // DISABLE it again for paper (flat scope → teardown), then re-enable:
    // the whole dance is the panel's round-trip.
    let (kept, set_now) = c
        .set_scoped_strategy_enabled(&paper, "trend_follow", false, "test", "panel off", 3)
        .expect("the scoped disable lands");
    assert!(kept.is_empty() && set_now.is_empty());
    assert!(c.strategy_enabled_accounts("trend_follow").is_empty());
    let text = std::fs::read_to_string(&env_path).unwrap();
    assert!(
        !text.contains("ACCOUNT_PAPER_STRATEGIES"),
        "the empty set is spelled by absence: {text}"
    );

    // REBOOT: a fresh Core over the same file (paper re-enabled first so
    // the boot has something to restore), then boot-apply. The scope set
    // comes back EXACTLY as the file left it.
    let _ = c.set_scoped_strategy_enabled(
        &paper,
        "trend_follow",
        true,
        "test",
        "re-armed before reboot",
        4,
    );
    let mut c2 = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            strategy_state_path: Some(state_path.to_string_lossy().into_owned()),
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
    c2.open_account_risk_db(dir.join("store.sqlite"))
        .expect("settings store opens");
    c2.enable_engine(Engine::new(EngineConfig::default()));
    crate::strategies::test_support::host_disabled(
        c2.engine.as_mut().unwrap(),
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    let migrated = c2.apply_account_strategy_assignments_at_boot().unwrap();
    assert!(migrated.is_empty(), "paper's explicit key = no migration");
    // The book remembers the set; the reply echo of a fresh read matches.
    assert_eq!(
        c2.strategy_enabled_accounts("trend_follow"),
        vec!["paper".to_string()],
        "the reboot restores paper's enablement exactly"
    );
    assert_eq!(
        c2.strategy_enabled_accounts("spread_arb"),
        Vec::<String>::new(),
        "default has no key and no global toggle → nothing to report"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// #537 关不掉牙（操作者 2026-10-19 报告）：default 账户的启用集写在
/// accounts.toml 后，面板的全局开关（`strategy.enable` accountId=default
/// 的服务层形状）只翻引擎旗标 + 写 strategy-state.json —— boot 的
/// rebuild 会把文件集合里的 scope 无条件拉回 enabled，「关掉」活不过一次
/// 重启。合流后的合同：default 的 scoped 写与全局开关**双向同步**——
/// `set_scoped_strategy_enabled(default, …)` 同时落 accounts.toml 集合
/// 与 strategy-state.json，boot 读到的就是操作者最后一次的决定；文件
/// 集合里被移除的名字不再借陈旧 state 文件复活（migration 的输入与
/// 集合写入永远一致）。
#[test]
fn default_toggle_persists_and_boot_does_not_resurrect() {
    let dir = std::env::temp_dir().join(format!("bk-default-toggle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    // The pre-#537 production shape: the default account's enablement set
    // IS in the file (this is what made the toggle unable to win).
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default,paper\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_DEFAULT_STRATEGIES=\"trend_follow,spread_arb\"\nACCOUNT_PAPER_NAME=\"paper\"\n",
    )
    .expect("fixture");
    let state_path = dir.join("strategy-state.json");

    let mut c = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            strategy_state_path: Some(state_path.to_string_lossy().into_owned()),
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
    crate::strategies::test_support::host_disabled(
        c.engine.as_mut().unwrap(),
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    // The file is explicit: no migration.
    assert!(
        c.apply_account_strategy_assignments_at_boot()
            .unwrap()
            .is_empty()
    );
    let default_id = crate::account::default_account_id();

    // The operator turns spread_arb off (the scoped write is what the
    // panel's toggle now rides): the file's set loses the name AND the
    // state file follows.
    let (kept, set_now) = c
        .set_scoped_strategy_enabled(&default_id, "spread_arb", false, "test", "panel off", 2)
        .expect("the default disable lands");
    assert!(kept.is_empty(), "flat scope → nothing demoted");
    assert_eq!(set_now, vec!["trend_follow".to_string()]);
    assert_eq!(
        stored_strategy_set(&dir, "default").as_deref(),
        Some("[\"trend_follow\"]"),
        "the row lost spread_arb the moment the toggle did"
    );
    assert_eq!(
        crate::strategy_state::load(&state_path),
        vec!["trend_follow".to_string()],
        "the state file follows the scoped write"
    );

    // REBOOT: the boot's rebuild must not resurrect spread_arb — the file
    // is the only truth and the state file agrees with it.
    let mut c2 = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            strategy_state_path: Some(state_path.to_string_lossy().into_owned()),
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
    c2.open_account_risk_db(dir.join("store.sqlite"))
        .expect("settings store opens");
    c2.enable_engine(Engine::new(EngineConfig::default()));
    crate::strategies::test_support::host_disabled(
        c2.engine.as_mut().unwrap(),
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    let migrated = c2.apply_account_strategy_assignments_at_boot().unwrap();
    assert!(migrated.is_empty(), "explicit key ⇒ no migration");
    assert!(
        !c2.enabled_strategy_names()
            .contains(&"spread_arb".to_string()),
        "a disabled scope must not come back from the file or the state file"
    );
    assert_eq!(
        c2.enabled_strategy_names(),
        vec!["trend_follow".to_string()],
    );

    // The reverse direction: re-enabling through the same write lands the
    // name in BOTH stores again.
    let _ = c2.set_scoped_strategy_enabled(&default_id, "spread_arb", true, "test", "panel on", 3);
    let text = std::fs::read_to_string(&env_path).unwrap();
    assert!(
        text.contains("ACCOUNT_DEFAULT_STRATEGIES") && text.contains("spread_arb"),
        "the enable lands in the file too: {text}"
    );
    assert!(crate::strategy_state::load(&state_path).contains(&"spread_arb".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}
