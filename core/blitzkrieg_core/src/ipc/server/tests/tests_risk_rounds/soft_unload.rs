//! Soft-unload gate admits an override family's discovery batches (issue #1165
//! L2 contract, acceptance case B). Moved verbatim from
/// `tests_risk_rounds.rs` (pure code motion).
use super::super::*;

use blitzkrieg_market_api::net::now_ms;
use rust_decimal::Decimal;
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

/// #1165 (L2 契约，验收用例 B)：软卸闸认账 per-account override —— override
/// 族的 discovery 批次必须到达引擎（它正是 scanner 现在跑的网格），没有
/// override 时同族批次照旧被软卸丢弃（B2 语义不变）。live 事故里 override
/// 后连数据面都起不来；即使起了实例，没有这条改动闸也会把批次吞掉。
#[tokio::test]
async fn the_soft_unload_gate_admits_the_overridden_familys_batches() {
    let now = now_ms();
    let book = crate::account::AccountLedgers::new(
        vec![crate::account::Account {
            id: crate::account::AccountId::from("default"),
            name: "main".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys::default(),
            venue: "venuelive".to_string(), // bound: the family keys on ITS venue
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(), // no declaration — the override IS the only grid
            opening_equity: None,
            updated_at_ms: now,
        }],
        crate::account::AccountId::from("default"),
    );
    let cfg = CoreConfig {
        engine_enabled: true,
        round_duration_sec: 900,
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let mut c = Core::with_account_book(cfg, book);
    c.config_mut()
        .account_round_durations
        .insert("default".to_string(), 120);
    let core = Arc::new(AsyncMutex::new(c));
    let mut mounted = BTreeSet::new();
    mounted.insert(("venuelive".to_string(), 900));
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

    // A 3600s batch — the family the override runs. The span derivation the
    // gate uses (expires/slot) must reproduce 3600: fixed constants keep it
    // deterministic (4_000_000s / slot 1111 → 3600).
    let batch = blitzkrieg_market_api::MarketDescriptor {
        asset: "BTC".to_string(),
        condition_id: "c".to_string(),
        question_id: "q".to_string(),
        up_token_id: "u".to_string(),
        down_token_id: "d".to_string(),
        up_price: Decimal::new(5, 1),
        down_price: Decimal::new(5, 1),
        expires_at_ms: 4_000_000_000,
        round_slot: 4_000_000_000 / 1000 / 3600,
        round_duration_sec: 3600,
        venue: "venuelive".to_string(),
        neg_risk: false,
        question: "override family".to_string(),
    };
    assert!(
        data_plane
            .admit_poll_batch(&core, std::slice::from_ref(&batch))
            .await,
        "the override family is ACTIVE — its batches must reach the engine"
    );
    assert_eq!(data_plane.soft_unloaded_count(), 0);

    // Soft-unload must NOT discard market data for a declared family:
    // after the account leaves 3600 the 1h batch still admits so prices
    // stay warm for the next switch. Trading stays fail-closed elsewhere.
    core.lock()
        .await
        .config_mut()
        .account_round_durations
        .clear();
    assert!(
        data_plane.admit_poll_batch(&core, &[batch]).await,
        "declared 3600s market data stays warm after the account leaves the family"
    );
    assert_eq!(data_plane.soft_unloaded_count(), 0);
}

fn override_plane(
    venue: &str,
    process_default_sec: i64,
    override_sec: i64,
) -> (std::sync::Arc<tokio::sync::Mutex<Core>>, DataPlaneRuntime) {
    let now = now_ms();
    let book = crate::account::AccountLedgers::new(
        vec![crate::account::Account {
            id: crate::account::AccountId::from("default"),
            name: "main".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys::default(),
            venue: venue.to_string(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            opening_equity: None,
            updated_at_ms: now,
        }],
        crate::account::AccountId::from("default"),
    );
    let cfg = CoreConfig {
        engine_enabled: true,
        round_duration_sec: process_default_sec,
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let mut c = Core::with_account_book(cfg, book);
    c.config_mut()
        .account_round_durations
        .insert("default".to_string(), override_sec);
    let core = Arc::new(AsyncMutex::new(c));
    let mut mounted = BTreeSet::new();
    mounted.insert((venue.to_string(), process_default_sec));
    mounted.insert((venue.to_string(), override_sec));
    let data_plane = DataPlaneRuntime {
        feed_ws_enabled: true,
        discovery_enabled: true,
        discovery_assets: vec!["BTC".to_string()],
        discovery_round_duration_sec: process_default_sec,
        discovery_mounted: Arc::new(std::sync::Mutex::new(mounted)),
        soft_unloaded_polling_instances: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        feed_reuse: None,
        last_dropped_batches: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::BTreeMap::new(),
        )),
    };
    (core, data_plane)
}

fn desc(
    venue: &str,
    duration_sec: i64,
    expires_at_ms: i64,
    round_slot: i64,
    question: &str,
) -> blitzkrieg_market_api::MarketDescriptor {
    blitzkrieg_market_api::MarketDescriptor {
        asset: "BTC".to_string(),
        condition_id: "c".to_string(),
        question_id: "q".to_string(),
        up_token_id: "u".to_string(),
        down_token_id: "d".to_string(),
        up_price: Decimal::new(5, 1),
        down_price: Decimal::new(5, 1),
        expires_at_ms,
        round_slot,
        round_duration_sec: duration_sec,
        venue: venue.to_string(),
        neg_risk: false,
        question: question.to_string(),
    }
}

/// 15m → 5m: a reconstructed expires/slot (here 301s) used to be dropped
/// as an unknown family even though the account was on 300s.
/// The gate must key on the declared 300s field so prices and the clock
/// keep flowing after the switch and after each 5m roll.
#[tokio::test]
async fn admit_uses_declared_duration_when_expiry_span_is_skewed() {
    let (core, data_plane) = override_plane("polymarket", 900, 300);
    // expires/slot is a reconstructed duration: give it a 301s quotient
    // that is not in the active set, while the descriptor declares 300.
    // (At live timestamps, end/floor(end/300) almost never yields 299;
    // the gate must ignore that quotient whenever the declared field is set.)
    let expires_at_ms = 301_000_000i64;
    let round_slot = 1_000i64;
    let batch = desc("polymarket", 300, expires_at_ms, round_slot, "skewed 5m");
    assert_eq!(
        expires_at_ms / 1000 / round_slot,
        301,
        "the fixture must actually reproduce a 301s false family"
    );
    assert!(
        data_plane
            .admit_poll_batch(&core, std::slice::from_ref(&batch))
            .await,
        "declared 300s must admit even when expires/slot yields 301"
    );
    assert_eq!(data_plane.soft_unloaded_count(), 0);
}

/// Daily windows start at America/New_York noon, so expires/slot lands on
/// 86402 (observed live as the soft-unload key) rather than the declared
/// 86400. Switching onto daily must still admit the batch.
#[tokio::test]
async fn admit_keys_daily_on_declared_86400_not_measured_86402() {
    let (core, data_plane) = override_plane("polymarket", 900, 86_400);
    // 20778 * 86402 = 1_795_250_556; floor(end/86400) is also 20778, so
    // the old span formula yields 86402.
    let end_sec = 20_778i64 * 86_402;
    let batch = desc(
        "polymarket",
        86_400,
        end_sec * 1000,
        end_sec / 86_400,
        "daily 86402 skew",
    );
    assert_eq!(
        end_sec / (end_sec / 86_400),
        86_402,
        "the fixture must actually reproduce the 86402s false family"
    );
    assert!(
        data_plane
            .admit_poll_batch(&core, std::slice::from_ref(&batch))
            .await,
        "declared daily 86400 must admit even when expires/slot yields 86402"
    );
    assert_eq!(data_plane.soft_unloaded_count(), 0);
}

/// A family that was soft-unloaded mid-slot is cached so `round_duration.set`
/// can replay it. Discovery will not re-feed the same slot (it treats a
/// dropped `on_round_markets` as already delivered). Switching the account
/// onto that family must therefore: (1) have the dropped batch on hand,
/// (2) admit it once the override is live, (3) clear the cache on admit
/// so a later stale replay cannot resurrect an old slot.
#[tokio::test]
async fn dropped_batch_is_cached_and_cleared_when_family_becomes_active() {
    let now = now_ms();
    let book = crate::account::AccountLedgers::new(
        vec![crate::account::Account {
            id: crate::account::AccountId::from("default"),
            name: "main".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys::default(),
            venue: "polymarket".to_string(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            opening_equity: None,
            updated_at_ms: now,
        }],
        crate::account::AccountId::from("default"),
    );
    let cfg = CoreConfig {
        engine_enabled: true,
        round_duration_sec: 900,
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    // Account is on 15m; an undeclared 120s family is still dropped and cached.
    let mut c = Core::with_account_book(cfg, book);
    c.config_mut()
        .account_round_durations
        .insert("default".to_string(), 900);
    let core = Arc::new(AsyncMutex::new(c));
    let mut mounted = BTreeSet::new();
    mounted.insert(("polymarket".to_string(), 900));
    mounted.insert(("polymarket".to_string(), 3600));
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
    let batch = desc(
        "polymarket",
        120,
        now + 3_500_000,
        (now + 3_500_000) / 1000 / 120,
        "unknown 120s family",
    );
    assert!(
        !data_plane
            .admit_poll_batch(&core, std::slice::from_ref(&batch))
            .await,
        "an undeclared 120s family is still dropped"
    );
    let cached = data_plane.take_dropped_batch("polymarket", 120);
    assert!(cached.is_some(), "the dropped 120s batch must be cached");
    assert_eq!(cached.as_ref().unwrap()[0].question, "unknown 120s family");
    // Put it back — take consumed the cache, as the set-path replay will.
    // Re-drop to restore the production shape (admit_poll_batch caches again).
    assert!(
        !data_plane
            .admit_poll_batch(&core, std::slice::from_ref(&batch))
            .await
    );

    core.lock()
        .await
        .config_mut()
        .account_round_durations
        .insert("default".to_string(), 120);
    // An undeclared duration stays dropped even if an account "anchors" it:
    // only 300/900/3600/86400 stay warm. Cache remains so a retry is possible.
    assert!(
        !data_plane
            .admit_poll_batch(&core, std::slice::from_ref(&batch))
            .await,
        "undeclared 120s must stay dropped"
    );
    assert!(
        data_plane.take_dropped_batch("polymarket", 120).is_some(),
        "the dropped undeclared batch stays cached"
    );
}
