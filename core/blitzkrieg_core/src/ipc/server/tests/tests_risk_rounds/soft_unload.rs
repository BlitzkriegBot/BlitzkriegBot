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
        .insert("default".to_string(), 3600);
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

    // Without the override the same batch is soft-unloaded (B2 unchanged):
    // no account declares 3600 any more, the gate drops it and counts.
    core.lock()
        .await
        .config_mut()
        .account_round_durations
        .clear();
    assert!(
        !data_plane.admit_poll_batch(&core, &[batch]).await,
        "an undeclared, un-overridden family stays soft-unloaded"
    );
    assert_eq!(data_plane.soft_unloaded_count(), 1);
}
