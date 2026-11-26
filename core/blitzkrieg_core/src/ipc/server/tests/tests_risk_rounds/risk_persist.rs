//! `risk.setSystemic` persistence round trip — issue #965 acceptance → SQLite
//! is the durable carrier, `.env` untouched, boot overlay re-arms on start.
//! Moved verbatim from `tests_risk_rounds.rs` (pure code motion).

use super::super::*;

use crate::service::{Core, CoreConfig};
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

/// 生效风控的持久化回路（issue #965 验收一 → 操作者裁决 2026-11-14）：
/// 面板保存 → `BK_RISK_*` 落进 SQLite（回执 `persisted=true` +
/// `persistence=store` —— DB 是唯一载体，`.env` 对内核只读、逐字节不动）
/// → 「重启」（boot overlay 读的同一张表，fresh `load_risk` 就是启动链的
/// 回读）→ 限额仍武装。持久化载体与回读一致 —— 一条载体链，没有第二个
/// flag 标注的影子载体（issue #965 把 risk-state 文件里的 systemic 格
/// 退役了；本轮把 env 载体一并退役）。
#[tokio::test]
async fn risk_set_systemic_persists_to_the_db_and_the_boot_rearms_it() {
    use rust_decimal_macros::dec;
    let dir = std::env::temp_dir().join(format!("bk-risk-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    let cfg = CoreConfig {
        risk: crate::risk::RiskConfig {
            max_order_notional: dec!(3),
            ..Default::default()
        },
        dry_seed_balance: dec!(100),
        env_file_path: Some(env_path.to_string_lossy().into_owned()),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
        ..Default::default()
    };
    let mut c = Core::new(cfg.clone());
    c.set_balance(cfg.dry_seed_balance);
    c.open_account_risk_db(dir.join("risk.sqlite"))
        .expect("temp risk db opens");
    c.store = Some(Arc::new(
        crate::store::SqliteStore::open(dir.join("risk.sqlite")).expect("temp store opens"),
    ));
    cfg.install_engine(&mut c).expect("engine install");
    let core = Arc::new(AsyncMutex::new(c));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };
    // The kernel never writes the .env (operator ruling 2026-11-14): the
    // whole write must leave this byte-for-byte (here: absent) unchanged.
    let env_before = std::fs::read(&env_path).ok();

    let applied = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"maxPositionSize":2,"maxSingleLossUsd":"0.75","reason":"persist me"}}"#.to_string(),
    )
    .await;
    assert!(
        applied.get("error").is_none(),
        "the patch must land: {applied}"
    );
    let audit = &applied["result"];
    assert_eq!(
        audit["persisted"],
        serde_json::json!(true),
        "the store is wired, so the write must land: {audit}"
    );
    assert_eq!(
        audit["persistence"],
        serde_json::json!("store"),
        "the receipt names the carrier: the store took the write: {audit}"
    );

    // The durable half: the merged row lands in the SQLite risk table —
    // exactly the row the boot overlay reads back on the next start.
    let db = crate::risk::account_store::AccountRiskDb::open(dir.join("risk.sqlite"))
        .expect("reopen the db for the read-back");
    let rows = db.load_risk().expect("the risk rows load");
    assert_eq!(
        rows["default"].max_position_size.value(),
        dec!(2),
        "the DB row carries the new position size: {rows:?}"
    );
    assert_eq!(
        rows["default"].max_single_loss_usd.value(),
        dec!(0.75),
        "the DB row carries the new single-loss cap: {rows:?}"
    );

    // 「重启」：the boot overlay reads that same table (a fresh `load_risk`
    // IS the restart read-back) — the durable bound is armed on start.

    // `.env` 逐字节不动（不存在也照实）—— 内核唯一不写它。
    assert_eq!(
        std::fs::read(&env_path).ok(),
        env_before,
        "the kernel must never write the .env"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn risk_set_systemic_persists_size_pct_and_reloads_on_boot() {
    use rust_decimal_macros::dec;
    let dir = std::env::temp_dir().join(format!("bk-size-pct-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("risk.sqlite");
    let cfg = CoreConfig {
        dry_seed_balance: dec!(1000),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
        ..Default::default()
    };
    let mut c = Core::new(cfg.clone());
    c.set_balance(cfg.dry_seed_balance);
    c.open_account_risk_db(&db_path)
        .expect("temp risk db opens");
    c.store = Some(Arc::new(
        crate::store::SqliteStore::open(&db_path).expect("temp store opens"),
    ));
    cfg.install_engine(&mut c).expect("engine install");
    let core = Arc::new(AsyncMutex::new(c));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    let applied = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"sizePct":"2.5","reason":"webui"}}"#.to_string(),
    )
    .await;
    assert!(
        applied.get("error").is_none(),
        "sizePct set failed: {applied}"
    );
    assert_eq!(applied["result"]["persisted"], serde_json::json!(true));

    let limits = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"risk.limits","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(limits["result"]["sizePct"], serde_json::json!("2.5"));

    // Boot reload: a fresh Core reading the same SQLite DB must restore size_pct = 2.5.
    let mut c2 = Core::new(cfg.clone());
    c2.open_account_risk_db(&db_path).expect("reopen risk db");
    assert_eq!(c2.config().size_pct, dec!(2.5));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn risk_set_systemic_persists_max_positions_and_share_band_on_boot() {
    use rust_decimal_macros::dec;
    let dir = std::env::temp_dir().join(format!("bk-max-pos-band-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("risk.sqlite");
    let cfg = CoreConfig {
        dry_seed_balance: dec!(1000),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
        ..Default::default()
    };
    let mut c = Core::new(cfg.clone());
    c.set_balance(cfg.dry_seed_balance);
    c.open_account_risk_db(&db_path)
        .expect("temp risk db opens");
    c.store = Some(Arc::new(
        crate::store::SqliteStore::open(&db_path).expect("temp store opens"),
    ));
    cfg.install_engine(&mut c).expect("engine install");
    let core = Arc::new(AsyncMutex::new(c));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    let applied = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"maxPositions":20,"minShares":"5","maxShares":"200","reason":"webui"}}"#.to_string(),
    )
    .await;
    assert!(applied.get("error").is_none(), "set failed: {applied}");
    assert_eq!(applied["result"]["persisted"], serde_json::json!(true));

    let limits = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"risk.limits","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(limits["result"]["maxPositions"], serde_json::json!(20));
    assert_eq!(limits["result"]["minShares"], serde_json::json!("5"));
    assert_eq!(limits["result"]["maxShares"], serde_json::json!("200"));

    let mut c2 = Core::new(cfg.clone());
    c2.open_account_risk_db(&db_path).expect("reopen risk db");
    assert_eq!(c2.config().positions.max_positions, 20);
    assert_eq!(c2.config().min_shares, dec!(5));
    assert_eq!(c2.config().max_shares, dec!(200));

    // maxShares = 0 means unlimited and must persist + reload cleanly even when minShares > 0.
    let unlimited = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"risk.setSystemic","params":{"maxShares":"0","reason":"unlimited"}}"#.to_string(),
    )
    .await;
    assert!(
        unlimited.get("error").is_none(),
        "maxShares=0 failed: {unlimited}"
    );
    let mut c3 = Core::new(cfg.clone());
    c3.open_account_risk_db(&db_path).expect("reopen risk db");
    assert_eq!(c3.config().min_shares, dec!(5));
    assert_eq!(c3.config().max_shares, dec!(0));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn boot_reconciles_legacy_size_pct_and_policy_budget_ratio_fail_closed() {
    use rust_decimal_macros::dec;
    let dir = std::env::temp_dir().join(format!("bk-fail-closed-pct-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("risk.sqlite");

    // Seed both legacy BK_SIZE_PCT=10 (10%) and POLICY_BUDGET_RATIO=0.04 (4%):
    // Fail-Closed migration must pick the tighter 4% bound and never widen.
    use crate::store::StoreApi;
    let store = crate::store::SqliteStore::open(&db_path).expect("temp store opens");
    let default_id = crate::account::AccountId::from("default");
    let scope = crate::store::AccountScope::new(&default_id);
    store
        .put_setting(
            scope,
            crate::risk::account_store::SIZE_PCT_SETTING,
            Some("10"),
        )
        .unwrap();
    store
        .put_setting(scope, "POLICY_BUDGET_RATIO", Some("0.04"))
        .unwrap();
    drop(store);

    let cfg = CoreConfig {
        dry_seed_balance: dec!(1000),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
        ..Default::default()
    };
    let mut c = Core::new(cfg);
    c.open_account_risk_db(&db_path).expect("reopen risk db");
    assert_eq!(
        c.config().size_pct,
        dec!(4),
        "must pick tighter 4% instead of widening to 10%"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
