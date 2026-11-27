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
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"accountId":"default","maxPositionSize":2,"maxSingleLossUsd":"0.75","reason":"persist me"}}"#.to_string(),
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
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"accountId":"default","sizePct":"2.5","reason":"webui"}}"#.to_string(),
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
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"accountId":"default","maxPositions":20,"minShares":"5","maxShares":"200","reason":"webui"}}"#.to_string(),
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
        r#"{"jsonrpc":"2.0","id":3,"method":"risk.setSystemic","params":{"accountId":"default","maxShares":"0","reason":"unlimited"}}"#.to_string(),
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

#[tokio::test]
async fn risk_set_systemic_and_exit_require_account_id() {
    let cfg = CoreConfig {
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
        ..Default::default()
    };
    let core = Arc::new(AsyncMutex::new(Core::new(cfg)));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    let sys_err = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"maxPositionSize":5}}"#
            .to_string(),
    )
    .await;
    assert!(
        sys_err.get("error").is_some(),
        "missing accountId must be rejected: {sys_err}"
    );

    let exit_err = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"risk.setExit","params":{"stopLossPct":"8"}}"#
            .to_string(),
    )
    .await;
    assert!(
        exit_err.get("error").is_some(),
        "missing accountId on risk.setExit must be rejected: {exit_err}"
    );
}

#[tokio::test]
async fn non_default_account_sizing_replays_on_boot_without_polluting_default() {
    use crate::account::{Account, AccountId, AccountLedgers, AccountStatus, CredentialKeys};
    use rust_decimal_macros::dec;
    let dir = std::env::temp_dir().join(format!("bk-acct-b-sizing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("risk.sqlite");
    let make_book = || {
        AccountLedgers::new(
            vec![
                Account {
                    id: AccountId::from("default"),
                    name: "default".into(),
                    market_type: blitzkrieg_market_api::MarketType::Prediction,
                    status: AccountStatus::Active,
                    credential_keys: CredentialKeys::default(),
                    venue: String::new(),
                    trading_enabled: true,
                    strategies: Vec::new(),
                    rounds: Vec::new(),
                    opening_equity: None,
                    updated_at_ms: 0,
                },
                Account {
                    id: AccountId::from("acct_b"),
                    name: "acct_b".into(),
                    market_type: blitzkrieg_market_api::MarketType::Prediction,
                    status: AccountStatus::Active,
                    credential_keys: CredentialKeys::default(),
                    venue: String::new(),
                    trading_enabled: true,
                    strategies: Vec::new(),
                    rounds: Vec::new(),
                    opening_equity: None,
                    updated_at_ms: 0,
                },
            ],
            AccountId::from("default"),
        )
    };
    let cfg = CoreConfig {
        dry_seed_balance: dec!(1000),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
        ..Default::default()
    };
    let mut c = Core::with_account_book(cfg.clone(), make_book());
    c.open_account_risk_db(&db_path)
        .expect("temp risk db opens");
    c.store = Some(Arc::new(
        crate::store::SqliteStore::open(&db_path).expect("temp store opens"),
    ));
    let core = Arc::new(AsyncMutex::new(c));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    let res = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"accountId":"acct_b","sizePct":"7.5","maxPositions":12,"minShares":"3","maxShares":"80"}}"#.to_string(),
    )
    .await;
    assert!(res.get("error").is_none(), "acct_b write failed: {res}");

    {
        let guard = core.lock().await;
        assert_eq!(guard.config().size_pct, dec!(0));
        assert_eq!(
            guard.config().account_size_pct.get("acct_b").copied(),
            Some(dec!(7.5))
        );
        assert_eq!(
            guard.config().account_max_positions.get("acct_b").copied(),
            Some(12)
        );
    }

    let mut c2 = Core::with_account_book(cfg, make_book());
    c2.open_account_risk_db(&db_path).expect("reopen risk db");
    assert_eq!(c2.config().size_pct, dec!(0));
    assert_eq!(
        c2.config().account_size_pct.get("acct_b").copied(),
        Some(dec!(7.5))
    );
    assert_eq!(
        c2.config().account_max_positions.get("acct_b").copied(),
        Some(12)
    );
    assert_eq!(
        c2.config().account_min_shares.get("acct_b").copied(),
        Some(dec!(3))
    );
    assert_eq!(
        c2.config().account_max_shares.get("acct_b").copied(),
        Some(dec!(80))
    );

    let core2 = Arc::new(AsyncMutex::new(c2));
    let limits_b = rpc(
        &core2,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"risk.limits","params":{"accountId":"acct_b"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(
        limits_b["result"]["account"]["id"],
        serde_json::json!("acct_b")
    );
    assert_eq!(limits_b["result"]["sizePct"], serde_json::json!("7.5"));
    assert_eq!(limits_b["result"]["maxPositions"], serde_json::json!(12));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #297 end-to-end dry proof:
/// 1. Same strategy, same market, same $1000 equity: `default` at 2% and
///    `acct_b` at 5% produce tickets matching their own thresholds (40 vs 100
///    shares at 0.50).
/// 2. `acct_b` with `maxPositions=1` rejects its 2nd open position while
///    `default` is unaffected.
/// 3. Live `risk.setSystemic` update on `acct_b` immediately changes `acct_b`'s
///    next order without restarting.
#[tokio::test]
async fn non_default_account_risk_set_systemic_takes_effect_immediately_on_orders_and_max_positions()
 {
    use crate::account::{Account, AccountId, AccountLedgers, AccountStatus, CredentialKeys};
    use crate::engine::DataEvent;
    use crate::model::{CryptoMarket, OrderbookSnapshot, SignalDirection};
    use crate::position::OpenParams;
    use crate::signal::TradeSignal;
    use crate::strategies::{EngineStrategy, StrategyCtx};
    use rust_decimal_macros::dec;

    struct AlwaysBuyUp {
        account: String,
    }
    impl EngineStrategy for AlwaysBuyUp {
        fn name(&self) -> &str {
            "issue_297_probe"
        }
        fn on_book(&mut self, _: &str, _: &OrderbookSnapshot, _: i64) {}
        fn on_round(&mut self, _: i64, _: i64, _: i64) {}
        fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
            ctx.markets()
                .iter()
                .map(|m| TradeSignal {
                    strategy: "issue_297_probe".into(),
                    account_id: self.account.clone(),
                    asset: m.asset.clone(),
                    direction: SignalDirection::Up,
                    token_id: m.up_token_id.clone(),
                    condition_id: m.condition_id.clone(),
                    price: dec!(0.50),
                    reason: "issue-297".into(),
                    shares: None,
                    pair: None,
                })
                .collect()
        }
    }

    let dir = std::env::temp_dir().join(format!("bk-issue-297-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("risk.sqlite");

    let book = AccountLedgers::new(
        vec![
            Account {
                id: AccountId::from("default"),
                name: "default".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                opening_equity: Some(dec!(1000)),
                updated_at_ms: 0,
            },
            Account {
                id: AccountId::from("acct_b"),
                name: "acct_b".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                opening_equity: Some(dec!(1000)),
                updated_at_ms: 0,
            },
        ],
        AccountId::from("default"),
    );

    let cfg = CoreConfig {
        dry_seed_balance: dec!(1000),
        risk: crate::risk::RiskConfig {
            max_order_notional: dec!(1000),
            ..Default::default()
        },
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
        ..Default::default()
    };
    let mut c = Core::with_account_book(cfg.clone(), book);
    c.set_balance(dec!(1000));
    c.open_account_risk_db(&db_path)
        .expect("temp risk db opens");
    c.store = Some(Arc::new(
        crate::store::SqliteStore::open(&db_path).expect("temp store opens"),
    ));

    let now_ms = 1_795_751_760_000i64;
    let mut eng = crate::engine::Engine::new(c.config().engine_config());
    eng.set_scanner_grids(vec![(String::new(), 300)]);
    eng.register_scoped_strategy(
        Box::new(AlwaysBuyUp {
            account: "default".into(),
        }),
        "test".into(),
        "default",
    )
    .unwrap();
    eng.register_scoped_strategy(
        Box::new(AlwaysBuyUp {
            account: "acct_b".into(),
        }),
        "test".into(),
        "acct_b",
    )
    .unwrap();
    eng.set_strategy_enabled("issue_297_probe", true);
    eng.set_account_trading("default", true);
    eng.set_account_trading("acct_b", true);
    c.enable_engine(eng);

    let mkt = CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond_297".into(),
        question_id: "q_297".into(),
        up_token_id: "tok_297_up".into(),
        down_token_id: "tok_297_down".into(),
        up_price: dec!(0.50),
        down_price: dec!(0.50),
        expires_at_ms: now_ms + 240_000,
        round_slot: 5_985_839,
        round_duration_sec: 300,
        archive_verdict: false,
        venue: String::new(),
        neg_risk: true,
        question: "BTC 5m".into(),
    };
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![mkt],
            now_ms,
        },
        now_ms,
    );

    let core = Arc::new(AsyncMutex::new(c));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    let res_a = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"accountId":"default","sizePct":"2","maxPositions":5,"minShares":"1","maxShares":"500"}}"#.to_string(),
    )
    .await;
    assert!(res_a.get("error").is_none(), "default set failed: {res_a}");

    let res_b = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"risk.setSystemic","params":{"accountId":"acct_b","sizePct":"5","maxPositions":1,"minShares":"1","maxShares":"500"}}"#.to_string(),
    )
    .await;
    assert!(res_b.get("error").is_none(), "acct_b set failed: {res_b}");

    // 1. Evaluate end-to-end: default gets 2% ($20 / 0.50 = 40 shares),
    //    acct_b gets 5% ($50 / 0.50 = 100 shares).
    {
        let mut guard = core.lock().await;
        let placed = guard.engine_evaluate(now_ms);
        assert_eq!(placed, 2, "both accounts should place one order");
        let orders = guard.list_orders();
        let size_a = orders
            .iter()
            .find(|o| o.account_id.as_str() == "default" && o.token_id == "tok_297_up")
            .map(|o| o.size);
        let size_b = orders
            .iter()
            .find(|o| o.account_id.as_str() == "acct_b" && o.token_id == "tok_297_up")
            .map(|o| o.size);
        assert_eq!(size_a, Some(dec!(40)), "default must size at 2%");
        assert_eq!(size_b, Some(dec!(100)), "acct_b must size at 5%");

        // 2. Open 1 live position on acct_b -> acct_b's 2nd position is rejected, default is unaffected.
        guard.positions_mut().open(
            OpenParams {
                strategy: "issue_297_probe".into(),
                asset: "BTC".into(),
                direction: SignalDirection::Up,
                token_id: "tok_297_up".into(),
                condition_id: "cond_297".into(),
                account_id: AccountId::from("acct_b"),
                venue: String::new(),
                partner_token_id: "tok_297_down".into(),
                entry_price: dec!(0.50),
                expires_at_ms: now_ms + 240_000,
                was_maker: false,
                target_exit_price: None,
                round_slot: 1_795_751_700,
                round_duration_sec: 300,
            },
            now_ms,
        );
        let err_b = guard
            .positions()
            .can_open("acct_b", Some("ETH"), Some(SignalDirection::Up), now_ms)
            .expect_err("acct_b must reject 2nd position at maxPositions=1");
        assert_eq!(err_b, "Max positions (1)");
        assert!(
            guard
                .positions()
                .can_open("default", Some("ETH"), Some(SignalDirection::Up), now_ms)
                .is_ok(),
            "default must be unaffected by acct_b's maxPositions=1"
        );
    }

    // 3. Live update on acct_b (sizePct=3, maxShares=30, maxPositions=5) takes
    //    effect immediately on the next order without restarting.
    let res_b2 = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"risk.setSystemic","params":{"accountId":"acct_b","sizePct":"3","maxShares":"30","maxPositions":5}}"#.to_string(),
    )
    .await;
    assert!(
        res_b2.get("error").is_none(),
        "acct_b live update failed: {res_b2}"
    );
    {
        let mut guard = core.lock().await;
        let next_ms = now_ms + 300_000;
        let mkt2 = CryptoMarket {
            asset: "ETH".into(),
            condition_id: "cond_297_2".into(),
            question_id: "q_297_2".into(),
            up_token_id: "tok_297_2_up".into(),
            down_token_id: "tok_297_2_down".into(),
            up_price: dec!(0.50),
            down_price: dec!(0.50),
            expires_at_ms: next_ms + 240_000,
            round_slot: 5_985_840,
            round_duration_sec: 300,
            archive_verdict: false,
            venue: String::new(),
            neg_risk: true,
            question: "ETH 5m".into(),
        };
        guard.engine_on_data(
            DataEvent::RoundMarkets {
                markets: vec![mkt2],
                now_ms: next_ms,
            },
            next_ms,
        );
        let placed = guard.engine_evaluate(next_ms);
        assert_eq!(placed, 2, "both accounts place on the next round");
        let orders = guard.list_orders();
        let size_a = orders
            .iter()
            .find(|o| o.account_id.as_str() == "default" && o.token_id == "tok_297_2_up")
            .map(|o| o.size);
        let size_b = orders
            .iter()
            .find(|o| o.account_id.as_str() == "acct_b" && o.token_id == "tok_297_2_up")
            .map(|o| o.size);
        assert_eq!(size_a, Some(dec!(40)), "default remains at 40 shares");
        assert_eq!(
            size_b,
            Some(dec!(30)),
            "acct_b immediately reflects updated maxShares=30"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
