#![cfg(test)]

//! #1272 (网格池化) 的两条回归钉 —— 都钉**行为**，不钉实现：
//!
//! 1. `a_round_write_for_one_account_never_evicts_another_accounts_family`
//!    —— 账户 A 写 300s 之后，账户 B 的 900s 市场**仍然能入场**。单张全局
//!    网格时代这里会变成 `NoMarkets`（写入入口是 per-account，网格是全局），
//!    正是跨账户轮次串味。
//! 2. `a_round_write_survives_a_restart_with_the_same_account_grid`
//!    —— 写 300s → 重新装配（引擎重建，池只能由启动回填重建）→
//!    `round_duration.get` 的 `engineActiveSec` 仍是该账户的 300s。
//!    单张全局网格时代重启会把覆盖丢掉，读回进程默认的 900s。

use super::*;
use crate::model::CryptoMarket;
use crate::schema::SetRoundDurationParams;
use rust_decimal_macros::dec;

const NOW: i64 = 1_000_000;

fn market_row(venue: &str, stem: &str, dur: i64) -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: format!("cond-{venue}-{stem}-{dur}"),
        question_id: "q".into(),
        up_token_id: format!("{stem}-{dur}-up"),
        down_token_id: format!("{stem}-{dur}-down"),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: NOW / 1000 / dur * dur + dur * 1000,
        round_slot: NOW / 1000 / dur,
        round_duration_sec: dur,
        archive_verdict: false,
        venue: venue.into(),
        neg_risk: false,
        question: "?".into(),
    }
}

fn account(id: &str, venue: &str, rounds: Vec<i64>) -> crate::account::Account {
    crate::account::Account {
        id: crate::account::AccountId::from(id),
        name: id.into(),
        market_type: blitzkrieg_market_api::MarketType::Prediction,
        status: crate::account::AccountStatus::Active,
        credential_keys: crate::account::CredentialKeys::default(),
        venue: venue.into(),
        trading_enabled: true,
        strategies: Vec::new(),
        rounds,
        opening_equity: None,
        updated_at_ms: NOW,
    }
}

/// A dry core with `accounts` seeded and an engine whose scanner is a fresh
/// (empty pool) scanner — `dry_core_with` then runs the BOOT seeding, so every
/// test below starts from the production boot posture rather than a
/// hand-anchored pool.
fn dry_core_with(accounts: Vec<crate::account::Account>) -> Core {
    let book = crate::account::AccountLedgers::new(accounts, crate::model::default_account_id());
    let mut c = Core::with_account_book(
        CoreConfig {
            dry_seed_balance: dec!(1000),
            auto_exits_enabled: false,
            allowed_round_secs: vec![300, 900],
            default_round_duration_sec: 900,
            round_duration_sec: 900,
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        },
        book,
    );
    c.enable_engine(crate::engine::Engine::new(crate::engine::EngineConfig {
        scanner: crate::scanner::ScannerConfig {
            assets: vec!["BTC".into()],
            round_duration_sec: 900,
            min_round_age_sec: 0,
            min_time_left_sec: 0,
        },
        ..Default::default()
    }));
    // The boot seam's own grid seeding (`CoreConfig::install_engine` calls the
    // same function): the pool starts as the process default family plus every
    // family an account declares/overrides.
    let cfg = c.config().clone();
    c.seed_scanner_grids(&cfg);
    c
}

fn set_params(account_id: &str, duration_sec: i64) -> SetRoundDurationParams {
    SetRoundDurationParams {
        account_id: account_id.into(),
        duration_sec: Some(duration_sec),
        reset: false,
        reason: Some("1272 regression".into()),
    }
}

/// #1272：per-account 写入只往池里**增加**自己那个族，永远不改别的族。
///
/// 行为钉：账户 A（`default`，未绑定 venue）写 300s 之后，账户 B
/// （`sibling`，绑定 polymarket）的 900s 市场照常入场。
#[test]
fn a_round_write_for_one_account_never_evicts_another_accounts_family() {
    let mut c = dry_core_with(vec![
        account("default", "", vec![]),
        account("sibling", "polymarket", vec![900]),
    ]);
    // Both families' rows land: A's 300s and B's 900s.
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![
                market_row("polymarket", "a", 300),
                market_row("polymarket", "b", 900),
            ],
            now_ms: NOW,
        },
        NOW,
    );

    // Pre-write posture: the pool holds the boot families only, so the 300s
    // row is structurally refused (`NoMarkets`, never waivable).
    let pool_before: Vec<(String, i64)> = c.engine().expect("engine").scanner().grid_keys();
    assert!(
        !pool_before.contains(&(String::new(), 300)),
        "nothing anchors 300s before the write: {pool_before:?}"
    );
    assert_eq!(
        c.engine()
            .expect("engine")
            .scanner()
            .can_trade_reason_for(&market_row("polymarket", "a", 300), NOW),
        Err(crate::scanner::TimingBlock::NoMarkets),
        "an unanchored family is not enterable"
    );

    // Account A writes 300s.
    let update = c
        .apply_round_duration_set(&set_params("default", 300), "uid:1272", NOW)
        .expect("the per-account write lands");
    assert_eq!(update.next_session_sec, 300);
    assert!(!update.reset);

    // The pool gained 300s and KEPT 900s — the write is additive.
    let pool_after: Vec<(String, i64)> = c.engine().expect("engine").scanner().grid_keys();
    assert!(
        pool_after.contains(&(String::new(), 300)),
        "the writing account's family joins the pool: {pool_after:?}"
    );
    assert!(
        pool_after.contains(&(String::new(), 900)),
        "the process default family is never evicted by a per-account write: {pool_after:?}"
    );

    // The regression: the sibling account's 900s market STILL enters. Under a
    // single global grid this was `NoMarkets` — the cross-account bleed.
    assert!(
        c.engine()
            .expect("engine")
            .scanner()
            .can_trade_reason_for(&market_row("polymarket", "b", 900), NOW)
            .is_ok(),
        "a sibling account's 900s market keeps entering after A anchors 300s"
    );
    // And the writing account's own family enters too.
    assert!(
        c.engine()
            .expect("engine")
            .scanner()
            .can_trade_reason_for(&market_row("polymarket", "a", 300), NOW)
            .is_ok(),
        "the anchoring account's own family enters"
    );

    // The readout's engine clock follows the account that wrote (#1151).
    assert_eq!(
        crate::service::round_duration_readout(&c).engine_active_sec,
        300,
        "engineActiveSec reflects the DEFAULT account's resolved grid"
    );
}

/// #1272：per-account 覆盖必须活过重启。
///
/// 行为钉：写 300s → 重新装配（`install_engine` 重建引擎，池只能由启动
/// 回填从配置重建）→ `round_duration.get` 的 `engineActiveSec` 仍是 300s，
/// 且 300s 族仍在池里（入场判定不会把它判成 `NoMarkets`）。
#[test]
fn a_round_write_survives_a_restart_with_the_same_account_grid() {
    let mut c = dry_core_with(vec![account("default", "", vec![])]);
    c.apply_round_duration_set(&set_params("default", 300), "uid:1272", NOW)
        .expect("the per-account write lands");
    assert_eq!(
        crate::service::round_duration_readout(&c).engine_active_sec,
        300
    );

    // Restart: the SAME config (in production the override was read back from
    // SQLite at assembly) and a fresh account book — the engine is rebuilt, so
    // the grid pool can only come from the boot seeding.
    let cfg = c.config().clone();
    let accounts: Vec<crate::account::Account> =
        c.accounts().account_list().into_iter().cloned().collect();
    let mut restarted = Core::with_account_book(
        cfg.clone(),
        crate::account::AccountLedgers::new(accounts, crate::model::default_account_id()),
    );
    cfg.install_engine(&mut restarted)
        .expect("the boot seam installs the engine");

    assert_eq!(
        crate::service::round_duration_readout(&restarted).engine_active_sec,
        300,
        "the persisted per-account grid survives the restart"
    );
    assert!(
        restarted
            .engine()
            .expect("engine")
            .scanner()
            .admits_family("", 300),
        "the restarted pool carries the account's family: {:?}",
        restarted.engine().expect("engine").scanner().grid_keys()
    );
    // The process default family is still there too — the pool is a set.
    assert!(
        restarted
            .engine()
            .expect("engine")
            .scanner()
            .admits_family("", 900),
        "the restarted pool still carries the process default family"
    );
}
