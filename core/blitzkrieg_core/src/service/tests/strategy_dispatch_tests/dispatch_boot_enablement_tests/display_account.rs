//! `display.account.set`: valid id flips memory + durable kernel_meta row
//! atomically (persist first, nothing moves on failure), unknown id refused
//! plan-first, frozen id legal after issue 561, no persist path refused
//! wholesale. `.env` never gains an ACCOUNT_DISPLAY key.
//! Moved verbatim from `dispatch_boot_enablement_tests.rs` (pure code motion).

use super::super::*;

/// 显示账户配置化（display.account.set 的事务半）：合法切位把内存指针
/// 与 accounts.toml 顶层 `display_account` 键一次写齐（persist 先行，
/// 失败两个存储都不动）；未知 id 被计划先行拒绝且文件字节不动；持久
/// 化关（无 config 路径）整体拒绝。纯展示指针：active 账户/记账不受
/// 任何影响 —— 内存的 display 单元独立于 active 单元。
/// issue 561（2026-10-20 裁决）：校验放宽为「账户存在即可」，frozen
/// 账户也是合法 display 目标（旧断言钉住的 Active-only 已随裁决废除）。
#[test]
fn display_account_set_persists_and_validates_plan_first() {
    let dir = std::env::temp_dir().join(format!("bk-display-set-{}", std::process::id()));
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
            // 持久化后端是 SQLite（`.env` 只读）：display 指针落 accounts 行。
            db_path: Some(dir.join("store.sqlite")),
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
    // Boot default: the loader's first-ACTIVE rule points at `default`.
    assert_eq!(c.accounts.display_id(), "default");

    // The legal re-point: memory moves immediately; the durable half lands
    // in kernel_meta (SQL)，绝不写 `.env`。
    c.set_display_account_persisting(&crate::account::AccountId::from("paper"), "test", "")
        .expect("the re-point lands");
    assert_eq!(c.accounts.display_id(), "paper");
    let db = crate::risk::account_store::AccountRiskDb::open(dir.join("store.sqlite"))
        .expect("store re-opens");
    assert_eq!(
        db.load_kernel_meta(crate::db::META_DISPLAY_ACCOUNT)
            .expect("kernel_meta readable"),
        Some("paper".to_string()),
        "the display pointer is durable: paper"
    );
    // `.env` 逐字节不动。
    let text = std::fs::read_to_string(&env_path).unwrap();
    assert!(
        !text.contains("ACCOUNT_DISPLAY"),
        "env stays display-free: {text}"
    );
    assert!(text.contains("ACCOUNT_LIST=default,paper"), "{text}");

    // UNKNOWN id: refused plan-first, memory and store untouched.
    let before_meta = db
        .load_kernel_meta(crate::db::META_DISPLAY_ACCOUNT)
        .expect("kernel_meta readable");
    let err = c
        .set_display_account_persisting(&crate::account::AccountId::from("ghost"), "test", "")
        .expect_err("unknown id");
    assert!(err.message.contains("unknown account"), "{}", err.message);
    assert_eq!(c.accounts.display_id(), "paper");
    assert_eq!(
        db.load_kernel_meta(crate::db::META_DISPLAY_ACCOUNT)
            .expect("kernel_meta readable"),
        before_meta,
        "a refused write leaves the durable pointer untouched"
    );

    // issue 561（2026-10-20 裁决）改写旧断言：frozen 账户此前被
    // Active-only 校验拒绝，现在 display 是面板唯一可信账户源、切观察
    // 户必须生效 —— 「存在即可」，frozen1 合法落位，内存与文件同步。
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default,frozen1\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_FROZEN1_NAME=\"f\"\nACCOUNT_FROZEN1_STATUS=\"frozen\"\n",
    )
    .expect("fixture two");
    let mut c2 = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            // c2 用独立 store：c 已把 default/paper 写进 store.sqlite，而 frozen1 只在 c2 的 env 里，
            // 须 materialize 进 c2 自己的表后才能被 display 指向。
            db_path: Some(dir.join("store-c2.sqlite")),
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
    c2.set_display_account_persisting(&crate::account::AccountId::from("frozen1"), "test", "")
        .expect("a frozen account is a legal display target (issue 561)");
    assert_eq!(
        c2.accounts.display_id(),
        "frozen1",
        "display follows a frozen book — view-only, posture gates the orders"
    );
    // Durable across boot：同 store-c2.sqlite 再开一核心，display 指针从
    // kernel_meta 复原为 frozen1（env 里已无 ACCOUNT_DISPLAY 键）。
    let fresh_book =
        crate::account::AccountLedgers::new(Vec::new(), crate::account::AccountId::from("default"));
    let c4 = crate::service::Core::with_account_book(
        crate::service::CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            db_path: Some(dir.join("store-c2.sqlite")),
            ..Default::default()
        },
        fresh_book,
    );
    assert_eq!(
        c4.accounts.display_id(),
        "frozen1",
        "display survives boot from store: kernel_meta is the durable home"
    );

    // 持久化关（无 accounts_config_path 的内核 — 回测/进程内测试）：整体
    // 拒绝，内存指针也不动（与 account.trading.set 同一姿态）。
    let mut c3 = Core::new(CoreConfig::default());
    let err = c3
        .set_display_account_persisting(&crate::account::AccountId::from("default"), "test", "")
        .expect_err("no persist path");
    assert!(
        err.message.contains("no accounts config path"),
        "{}",
        err.message
    );
    let _ = std::fs::remove_dir_all(&dir);
}
