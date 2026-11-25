//! `round_duration.get` / `round_duration.set` wire contract — per-account
//! grids, `.env` vs SQLite carriers, runtime discovery mount. Moved verbatim
//! from `tests_risk_rounds.rs` (pure code motion).

use super::super::*;

use crate::risk::account_store::ROUND_DURATION_SETTING;
use crate::service::{Core, CoreConfig};
use blitzkrieg_market_api::net::now_ms;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

/// 轮次时间周期可设置（`round_duration.get`/`round_duration.set`）的
/// wire 合同：readout 回答「本会话生效值 + 来源 + 白名单 + 下次会话将
/// 解析到什么」；写路径把白名单内的值落进 SQLite `account_settings` 的
/// `round.duration_sec` 格（`nextSessionSec` 如实回报；无 store 的内核
/// 只在本会话生效）、白名单外的值拒绝且格不动、`reset` 清格、两者都给
/// 拒绝、无 store 的 core（回测/进程内测试）只在本会话生效。
/// **next_session 语义** 由构造保证（写路径只碰持久格），测试同时钉住
/// 本会话的 `round_duration_sec` 一个字节不动。
#[tokio::test]
async fn round_duration_set_writes_the_cell_and_get_reads_it_back() {
    // Per-account round duration: `round_duration.set` now names an account
    // and lands the value in the SQLite `account_settings` cell
    // (`round.duration_sec`; the `.env` spelling is only the first-import
    // read side since 操作者裁决 2026-11-14). The readout
    // reports `perAccount` plus the default effective duration.
    let dir = std::env::temp_dir().join(format!("bk-round-dur-wire-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    let env_str = env_path.to_string_lossy().into_owned();

    let (core, registry, peer) = {
        let cfg = CoreConfig {
            env_file_path: Some(env_str.clone()),
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
            round_labels: [
                (300, "5m"),
                (900, "15m"),
                (3600, "1h"),
                (14400, "4h"),
                (86400, "daily"),
            ]
            .into_iter()
            .collect(),
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

    // The readout before any write: effective 900 from the compiled default,
    // empty per-account map, the declared union with its labels.
    let read = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"round_duration.get","params":{}}"#.to_string(),
    )
    .await;
    assert!(read.get("error").is_none(), "readout must answer: {read}");
    assert_eq!(read["result"]["effectiveSec"], serde_json::json!(900));
    assert_eq!(read["result"]["source"], serde_json::json!("default"));
    assert_eq!(
        read["result"]["perAccount"],
        serde_json::json!({}),
        "no per-account overrides yet: {read}"
    );
    assert_eq!(
        read["result"]["nextSessionSec"],
        serde_json::json!(900),
        "next session re-resolves to the effective value: {read}"
    );
    let labels: Vec<(i64, &str)> = read["result"]["allowed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            (
                o["sec"].as_i64().unwrap(),
                o["label"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert!(
        labels.contains(&(300, "5m")) && labels.contains(&(900, "15m")),
        "the whitelist carries the plugins' declared labels: {labels:?}"
    );

    // The write path: set account "default" to 300s. Immediate effect — the
    // running grid for that account moves now, not at restart.
    let set = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"round_duration.set","params":{"accountId":"default","durationSec":300,"reason":"faster rounds"}}"#.to_string(),
    )
    .await;
    assert!(set.get("error").is_none(), "the set must land: {set}");
    assert_eq!(
        set["result"]["accountId"],
        serde_json::json!("default"),
        "the reply names the account: {set}"
    );
    assert_eq!(
        set["result"]["nextSessionSec"],
        serde_json::json!(300),
        "the reply names the account's value: {set}"
    );
    assert_eq!(set["result"]["reset"], serde_json::json!(false));
    assert_eq!(
        set["result"]["actor"],
        serde_json::json!(format!("uid:{}", own_uid()))
    );
    // In-memory map reflects the write immediately.
    {
        let c = core.lock().await;
        assert_eq!(
            c.config().account_round_durations.get("default").copied(),
            Some(300),
            "in-memory per-account map updated immediately",
        );
    }

    // The readout now reports the per-account override.
    let read2 = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"round_duration.get","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        read2["result"]["perAccount"]["default"],
        serde_json::json!(300),
        "per-account map shows the override: {read2}"
    );
    assert_eq!(
        read2["result"]["effectiveSec"],
        serde_json::json!(900),
        "default effective stays 900 (the account override is separate): {read2}"
    );
    assert_eq!(
        read2["result"]["engineActiveSec"],
        serde_json::json!(300),
        "the running default-account grid is the override: {read2}"
    );
    assert_eq!(
        read2["result"]["nextSessionSec"],
        serde_json::json!(300),
        "nextSessionSec is what the default account will run, not the factory 900: {read2}"
    );

    // An off-whitelist value is refused.
    let bad = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"round_duration.set","params":{"accountId":"default","durationSec":60}}"#
            .to_string(),
    )
    .await;
    assert!(
        bad["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not declared by any market plugin"),
        "an off-whitelist value is refused by name: {bad}"
    );

    // durationSec + reset together is refused.
    let both = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":5,"method":"round_duration.set","params":{"accountId":"default","durationSec":300,"reset":true}}"#.to_string(),
    )
    .await;
    assert!(
        both["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not both"),
        "an ambiguous write is refused: {both}"
    );

    // reset clears the account's override.
    let reset = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":6,"method":"round_duration.set","params":{"accountId":"default","reset":true}}"#
            .to_string(),
    )
    .await;
    assert!(reset.get("error").is_none(), "the reset must land: {reset}");
    assert_eq!(reset["result"]["reset"], serde_json::json!(true));
    assert_eq!(
        reset["result"]["accountId"],
        serde_json::json!("default"),
        "the reply names the account: {reset}"
    );
    assert_eq!(
        reset["result"]["nextSessionSec"],
        serde_json::json!(900),
        "after a reset the account falls back to the default: {reset}"
    );
    // In-memory map no longer has the account's override.
    {
        let c = core.lock().await;
        assert!(
            !c.config().account_round_durations.contains_key("default"),
            "in-memory per-account map cleared after reset",
        );
    }

    // A core with NO env file path (backtests, in-process tests): the write
    // still lands in memory (seconds-cut, no persistence), it just never
    // touches the filesystem.
    let (bare, registry2, peer2) = hot_reload_fixture().await;
    let accepted = rpc(
        &bare,
        &registry2,
        &peer2,
        r#"{"jsonrpc":"2.0","id":1,"method":"round_duration.set","params":{"accountId":"default","durationSec":300}}"#
            .to_string(),
    )
    .await;
    assert!(
        accepted.get("error").is_none(),
        "a path-less core allows in-memory write (no persistence): {accepted}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 操作者裁决 2026-11-14（落库 + 启动回读）：`round_duration.set` 写的是
/// SQLite 的 `account_settings` 行，不是 `.env`；下一个会话（新 Core、同一个
/// 库）读回同一个网格 —— 写进去的和读出来的是同一个事实，`.env` 里连键都
/// 不该出现。
#[tokio::test]
async fn round_duration_lands_in_sqlite_and_the_next_boot_reads_it_back() {
    let dir = std::env::temp_dir().join(format!("bk-round-sql-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("store.sqlite");
    let env_path = dir.join(".env");
    let cfg = || CoreConfig {
        env_file_path: Some(env_path.to_string_lossy().into_owned()),
        allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
        round_labels: [(300, "5m"), (900, "15m")].into_iter().collect(),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    // 第一个会话：写入 300。
    {
        let c = cfg();
        let mut core = Core::new(c.clone());
        core.set_balance(c.dry_seed_balance);
        core.open_account_risk_db(&db_path).expect("store opens");
        let core = Arc::new(AsyncMutex::new(core));
        let set = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":1,"method":"round_duration.set","params":{"accountId":"default","durationSec":300,"reason":"sql round trip"}}"#
                .to_string(),
        )
        .await;
        assert!(set.get("error").is_none(), "the set must land: {set}");
    }

    // 库里有那一行；`.env` 里没有任何轮次键。
    let db = crate::risk::account_store::AccountRiskDb::open(&db_path).expect("reopen");
    let stored = db
        .load_settings()
        .expect("settings readable")
        .get("default")
        .and_then(|keys| keys.get(ROUND_DURATION_SETTING))
        .cloned();
    assert_eq!(
        stored.as_deref(),
        Some("300"),
        "the grid is an account_settings row"
    );
    let env_text = std::fs::read_to_string(&env_path).unwrap_or_default();
    assert!(
        !env_text.contains("ROUND_DURATION_SEC"),
        "the round grid must never be written to .env: {env_text}"
    );

    // 第二个会话：同一个库 → 读回 300。默认账户的行同时是进程默认网格。
    {
        let c = cfg();
        let mut core = Core::new(c.clone());
        core.set_balance(c.dry_seed_balance);
        core.open_account_risk_db(&db_path).expect("store re-opens");
        let core = Arc::new(AsyncMutex::new(core));
        let read = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":2,"method":"round_duration.get","params":{}}"#.to_string(),
        )
        .await;
        assert_eq!(
            read["result"]["perAccount"]["default"],
            serde_json::json!(300),
            "the next boot reads the stored anchor back: {read}"
        );
        assert_eq!(
            read["result"]["effectiveSec"],
            serde_json::json!(900),
            "per-account override is separate from the process default grid: {read}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// #1165 (L2/L3 契约，验收用例 A)：`round_duration.set` 切到一个本会话
/// 从未挂载 discovery 实例的族时，写路径必须当场把该族挂进来 —— 只有
/// scanner 网格变了而数据面没跟上，账户就吃不到新族的任何行情（live
/// 事故的「no live PairBook entry」刷屏正是这条链）。重写同族幂等零新
/// 挂载；reset 落回 boot 全局族同样零新挂载。挂载收到的 config 必须
/// 是 boot 快照（assets 不是零）—— kalshi 按 (asset, round_duration_sec)
/// 选 series，零配置永远发现不了任何东西（#535 的形状）。
///
/// 并行安全（CI rust-check 的教训）：`VENUE_LIVE_DISCOVERY_CFG` 是进程级
/// 静态，挂载测试们并行运行——本用例的计数窗口曾被别的用例的挂载插入
/// （本地绿 CI 红的调度差），且 assert 时的持锁 panic 毒化了静态互斥、
/// 级联放倒三个无辜测试。修法：本用例的插件/发现 tag 用进程内唯一名，
/// 所有计数按该 tag 过滤——并行插入的条目带别的 tag，天然不可见；锁卫
/// 全部 `into_inner` 去毒，计数先绑定后断言，panic 不持锁。
#[tokio::test]
async fn round_duration_set_mounts_the_new_familys_discovery_at_runtime() {
    // Unique in-process: every cfg entry THIS test produces carries it, and
    // no parallel test can append an entry with it (their plugins use their
    // own tags) — the counting below is interference-free by construction.
    let tag = "venuelive-mount-idem";
    let cfg_of_tag = |v: &Vec<(String, blitzkrieg_market_api::DiscoveryConfig)>| {
        v.iter()
            .filter(|(t, _)| t == tag)
            .map(|(_, cfg)| cfg.clone())
            .collect::<Vec<_>>()
    };
    let dir = std::env::temp_dir().join(format!("bk-round-dur-mount-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    let env_str = env_path.to_string_lossy().into_owned();

    let (core, registry, peer) = {
        let now = now_ms();
        let book = crate::account::AccountLedgers::new(
            vec![crate::account::Account {
                id: crate::account::AccountId::from("default"),
                name: "main".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(), // unbound: speaks for every mounted venue
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                opening_equity: None,
                updated_at_ms: now,
            }],
            crate::account::AccountId::from("default"),
        );
        let cfg = CoreConfig {
            env_file_path: Some(env_str.clone()),
            engine_enabled: true,
            round_duration_sec: 900,
            allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        let registry = crate::market::registry::MarketPluginRegistry::new();
        registry.register(Box::new(VenueLivePlugin {
            name: tag.to_string(),
            discovery: VenueLiveDiscovery {
                tag: tag.to_string(),
            },
        }));
        (
            Arc::new(AsyncMutex::new(Core::with_account_book(cfg, book))),
            registry,
            PeerAuth::SameUid { uid: own_uid() },
        )
    };
    // Boot shape: the venue active on the GLOBAL family only — 300s has no
    // instance polling it, exactly the live incident's pre-state.
    let host: Arc<dyn blitzkrieg_market_api::MarketHost> =
        Arc::new(crate::market::host::CoreHost::new(core.clone()));
    let mut mounted = std::collections::BTreeSet::new();
    mounted.insert((tag.to_string(), 900));
    let data_plane = DataPlaneRuntime {
        feed_ws_enabled: true,
        discovery_enabled: true,
        discovery_assets: vec!["BTC".to_string()],
        discovery_round_duration_sec: 900,
        discovery_mounted: Arc::new(std::sync::Mutex::new(mounted)),
        soft_unloaded_polling_instances: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        feed_reuse: None,
        last_dropped_batches: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::BTreeMap::new(),
        )),
    };

    // Override to the unmounted family: the write must mount it live.
    let set = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":1,"method":"round_duration.set","params":{"accountId":"default","durationSec":300}}"#
            .to_string(),
    )
    .await;
    assert!(set.get("error").is_none(), "the set must land: {set}");
    assert_eq!(set["result"]["nextSessionSec"], serde_json::json!(300));
    assert!(
        data_plane.discovery_mounted_at(tag, 300),
        "the override family's discovery must be mounted: {:?}",
        data_plane
            .discovery_mounted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    );
    let starts = cfg_of_tag(
        &VENUE_LIVE_DISCOVERY_CFG
            .lock()
            .unwrap_or_else(|p| p.into_inner()),
    );
    assert_eq!(
        starts.len(),
        1,
        "exactly one discovery start for the unique tag: {starts:?}"
    );
    assert_eq!(
        starts[0].round_duration_sec, 300,
        "the new instance polls the OVERRIDE family, not the global one"
    );
    assert_eq!(
        starts[0].assets,
        vec!["BTC".to_string()],
        "the mount hands the boot's asset set, never zeroes"
    );

    // Idempotence: re-writing the SAME family starts nothing new.
    let again = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":2,"method":"round_duration.set","params":{"accountId":"default","durationSec":300,"reason":"idempotence"}}"#
            .to_string(),
    )
    .await;
    assert!(
        again.get("error").is_none(),
        "the re-write must land: {again}"
    );
    let after_rewrite = cfg_of_tag(
        &VENUE_LIVE_DISCOVERY_CFG
            .lock()
            .unwrap_or_else(|p| p.into_inner()),
    );
    assert_eq!(
        after_rewrite.len(),
        1,
        "an already-mounted family must not restart: {after_rewrite:?}"
    );

    // Reset → back to the global family, mounted since boot: still no new
    // start, and the runtime grid the account resolves to is 900 again.
    let reset = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":3,"method":"round_duration.set","params":{"accountId":"default","reset":true}}"#
            .to_string(),
    )
    .await;
    assert!(reset.get("error").is_none(), "the reset must land: {reset}");
    let after_reset = cfg_of_tag(
        &VENUE_LIVE_DISCOVERY_CFG
            .lock()
            .unwrap_or_else(|p| p.into_inner()),
    );
    assert_eq!(
        after_reset.len(),
        1,
        "the boot-mounted global family must not restart on reset: {after_reset:?}"
    );
}
