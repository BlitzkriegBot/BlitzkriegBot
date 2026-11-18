//! Boot-time assignment application: implicit default no longer migrates
//! into `strategies`; explicit keys win; production file shape boots
//! byte-identically; strategy-state.json never overrides an explicit key.
//! Moved verbatim from `dispatch_boot_enablement_tests.rs` (pure code motion).

use super::super::*;
use super::helpers::stored_strategy_set;

/// boot 分配应用（生产 `run()` 专属路径，回放不经过）：三条腿 ——
/// ① default 无键 + strategy-state 非空 → 一次性迁移物化进 accounts.toml
/// 且返回迁移集；② 已显式键的账户照文件执行（不再迁移）；③ 空键账户
/// boot 时零 scope（无 INHERIT）。scope 实例化是 Lua 源专属（test 适配器
/// 建不出来），所以 engine 侧断言钉在「分配即启用」的 rebuild 牙上，这里
/// 钉迁移与簿/文件拼写。
#[test]
fn boot_applies_assignments_and_migrates_the_implicit_default_once() {
    let dir = std::env::temp_dir().join(format!("bk-boot-assign-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default,probe2\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_PROBE2_NAME=\"probe two\"\nACCOUNT_PROBE2_STRATEGIES=\"spread_arb\"\n",
    )
    .expect("fixture");
    let state_path = dir.join("strategy-state.json");
    crate::strategy_state::save(&state_path, &["spread_arb".into(), "trend_follow".into()]);

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

    // ①+③: the default does NOT migrate — persisted enabled-set is no longer
    // folded into the default account's `strategies` (default strategies come
    // from `.env` ACCOUNT_DEFAULT_STRATEGIES / `account.strategies.set` only).
    // probe2's key is honored (its book spelling is already explicit).
    let migrated = c.apply_account_strategy_assignments_at_boot().unwrap();
    // 诊断信息不插值：CodeQL 把策略指派行当敏感信息，panic 消息里回显
    // 会被判 cleartext-logging（断言本身不动）。
    assert!(
        migrated.is_empty(),
        "no migration into the default account anymore"
    );
    let default_id = crate::account::default_account_id();
    assert!(
        c.accounts.strategy_assignment(&default_id).is_none(),
        "default account has no explicit strategies"
    );
    assert_eq!(
        stored_strategy_set(&dir, "default").as_deref(),
        None,
        "no STRATEGIES_SETTING row is materialized for the default account"
    );

    // Idempotence: a SECOND boot call still reports empty.
    let again = c.apply_account_strategy_assignments_at_boot().unwrap();
    assert!(again.is_empty(), "nothing was materialized for default");

    // ②: an account with NO key (and no persisted set to migrate — the
    // file now has one for default, so wipe the state file's role by
    // pointing the config away) boots zero scopes: no INHERIT.
    let mut c2 = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            strategy_state_path: None,
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
    let migrated2 = c2.apply_account_strategy_assignments_at_boot().unwrap();
    assert!(migrated2.is_empty(), "no state file, no migration");
    // default has no explicit strategies (.env ACCOUNT_DEFAULT_STRATEGIES unset)
    // and probe2's spread_arb is a non-Lua source: build_lua_scope declines,
    // so no scope is built for either account — nothing is enabled at boot.
    assert_eq!(
        c2.enabled_strategy_names(),
        Vec::<String>::new(),
        "default has no strategies; probe2's non-Lua strategy builds no scope"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The production upgrade shape pinned (账户↔venue 解耦 boot 兼容牙):
/// the shipped accounts.toml — `default` with
/// `strategies = ["pair_discount_arb","spread_arb"]` — boots EXACTLY the
/// scopes it names (spread_arb's scope lands enabled), the old global
/// toggle's strategy-state.json must not override an explicit key, and
/// unregistered names skip with a warning, never a panic.
#[test]
fn boot_keeps_the_production_default_enablement_shape_byte_identical() {
    let dir = std::env::temp_dir().join(format!("bk-prod-shape-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    // The production file's exact spelling (from the deployment tree).
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_DEFAULT_STRATEGIES=\"pair_discount_arb,spread_arb\"\n",
    )
    .expect("fixture");
    // The old global toggle's durable form: spread_arb off, the rest on —
    // the file's key must WIN (the migration never overrides an explicit
    // key, and the strategy-state.json read must not leak onto scopes).
    let state_path = dir.join("strategy-state.json");
    crate::strategy_state::save(&state_path, &["trend_follow".into()]);

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
    // The migration NEVER runs: the file's default row is already explicit.
    let migrated = c.apply_account_strategy_assignments_at_boot().unwrap();
    assert!(migrated.is_empty(), "an explicit key means no migration");
    // The book + file are untouched (byte-identical behaviour on upgrade).
    assert_eq!(
        c.accounts
            .strategy_assignment(&crate::account::default_account_id())
            .expect("the key rides"),
        &vec!["pair_discount_arb".to_string(), "spread_arb".to_string()],
    );
    let scopes = |name: &str| c.engine.as_ref().unwrap().strategy_scopes(name);
    assert_eq!(
        scopes("spread_arb"),
        vec!["default".to_string()],
        "the named scope lands (the test adapter is non-Lua, so only the \
         DEFAULT scope's pre-hosted instance is here — the ADD phase's \
         attempt at a second one warns and skips)"
    );
    assert!(
        scopes("pair_discount_arb").is_empty(),
        "an unregistered name is skipped with a warning, never a panic"
    );
    assert!(
        scopes("trend_follow").is_empty(),
        "strategy-state.json must NOT resurrect anything the file does not name"
    );
    // The engine's enabled set mirrors the file, not the old state file.
    assert_eq!(
        c.enabled_strategy_names(),
        vec!["spread_arb".to_string()],
        "the file's spread_arb entry is the enablement; trend_follow's \
         persisted global toggle is dead here"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
