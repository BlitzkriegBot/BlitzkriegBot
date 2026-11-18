//! `open_account_risk_db` is fail-closed on both the risk and the exit read
/// (issue #1199 P1-1 + fourth-audit P0). Moved verbatim from
/// `tests_risk_rounds.rs` (pure code motion).
use super::super::*;

use blitzkrieg_market_api::net::now_ms;

/// issue #1199 P1-1: `open_account_risk_db` is fail-closed at boot — a store
/// whose risk read cannot be served refuses boot instead of letting the env
/// global `.env` values keep trading with the operator none the wiser.
#[tokio::test]
async fn open_account_risk_db_read_failure_fail_closed() {
    let dir = std::env::temp_dir().join(format!("bk-1199-failclosed-{}", now_ms()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    // The env carries a WIDER global limit: a silent fallback would trade with
    // these widened values and the operator would not know (the P1-1 trap).
    std::fs::write(&env_path, "BK_RISK_MAX_POSITION_SIZE=999\n").unwrap();

    let cfg = CoreConfig {
        env_file_path: Some(env_path.to_string_lossy().into_owned()),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let mut c = Core::new(cfg.clone());
    c.set_balance(cfg.dry_seed_balance);

    // Not a SQLite database: the store cannot be opened and its risk rows
    // cannot be read.
    let db_path = dir.join("risk.sqlite");
    std::fs::write(&db_path, "not a sqlite database").unwrap();

    let result = c.open_account_risk_db(&db_path);
    assert!(
        result.is_err(),
        "a store whose risk read cannot be served must refuse boot"
    );
    assert!(
        !c.account_risk_db_open(),
        "no store is open after a refused boot (no env global allowed to leak in)"
    );
}

/// issue #1199 fourth audit P0: `open_account_risk_db` is fail-closed on the
/// EXIT read too — a store whose exit read cannot be served refuses boot
/// instead of letting the env global `.env` `BK_EXIT_*` values keep trading
/// with the operator none the wiser. Mirrors
/// `open_account_risk_db_read_failure_fail_closed` (P1-1, the risk side) on
/// the exit twin: with `load_exits` now returning `Result` and the boot
/// overlay using `?`, a refused boot leaks no env global into the cockpit.
#[tokio::test]
async fn open_account_risk_db_exit_read_failure_fail_closed() {
    let dir = std::env::temp_dir().join(format!("bk-1199-failclosed-exit-{}", now_ms()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    // The env carries WIDER exit discipline: a silent fallback would trade
    // with these values and the operator would not know (the P0 trap).
    std::fs::write(
        &env_path,
        "BK_EXIT_STOP_LOSS_PCT=99\nBK_EXIT_TAKE_PROFIT_PCT=90\n",
    )
    .unwrap();

    let cfg = CoreConfig {
        env_file_path: Some(env_path.to_string_lossy().into_owned()),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let mut c = Core::new(cfg.clone());
    c.set_balance(cfg.dry_seed_balance);

    // Not a SQLite database: the store cannot be opened and its exit rows
    // cannot be read.
    let db_path = dir.join("risk.sqlite");
    std::fs::write(&db_path, "not a sqlite database").unwrap();

    let result = c.open_account_risk_db(&db_path);
    assert!(
        result.is_err(),
        "a store whose exit read cannot be served must refuse boot"
    );
    assert!(
        !c.account_risk_db_open(),
        "no store is open after a refused boot (no env global allowed to leak in)"
    );
}
