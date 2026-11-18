//! `risk.limits` readout answers DB rows, never the stale `.env` overlay
//! (issue #1199 PR-2 A1/A2 asymmetry). Moved verbatim from
/// `tests_risk_rounds.rs` (pure code motion).
use super::super::*;

use blitzkrieg_market_api::net::now_ms;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

/// A core with a scratch `.env` carrying STALE cockpit keys, plus a SQLite
/// risk store whose rows say otherwise: `risk.limits` must answer the DB
/// rows (the boot overlay's values), never the file's stale overlay — and
/// `cockpit_env_rederive_allowed` is false, so no watcher re-derive can
/// clobber them from the file.
#[tokio::test]
async fn risk_limits_answers_db_rows_not_the_stale_env_overlay() {
    use rust_decimal_macros::dec;
    let dir = std::env::temp_dir().join(format!("bk-1199-db-readout-{}", now_ms()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    // The stale file: the operator's pre-DB spelling, never updated since
    // the DB took over (the exact residue the A1 asymmetry surfaced).
    std::fs::write(
        &env_path,
        "BK_RISK_MAX_POSITION_SIZE=999\nBK_EXIT_STOP_LOSS_PCT=99\n",
    )
    .unwrap();

    let (core, registry, peer) = {
        let cfg = CoreConfig {
            env_file_path: Some(env_path.to_string_lossy().into_owned()),
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        let mut c = Core::new(cfg.clone());
        c.set_balance(cfg.dry_seed_balance);
        // The DB rows (the panel write path's durable home): the default
        // account carries 5 shares / 8% stop — the boot overlay must put
        // them on the working copies and mirror them into the global faces.
        let db = crate::risk::account_store::AccountRiskDb::open(dir.join("risk.sqlite"))
            .expect("temp risk db");
        let row = crate::risk::limits::AccountRiskLimits {
            max_position_size: crate::risk::limits::Bound::new(
                dec!(5),
                crate::risk::limits::LimitSource::Flag,
            ),
            ..Default::default()
        };
        db.save_risk("default", &row).unwrap();
        let exit = crate::exit_policy::ExitConfig {
            stop_loss_pct: dec!(8),
            ..Default::default()
        };
        db.save_exit("default", &exit).unwrap();
        c.open_account_risk_db(dir.join("risk.sqlite")).unwrap();
        (
            Arc::new(AsyncMutex::new(c)),
            crate::market::registry::MarketPluginRegistry::new(),
            PeerAuth::SameUid { uid: own_uid() },
        )
    };

    // The readout: the DB rows win over the stale file keys.
    let read = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.limits","params":{}}"#.to_string(),
    )
    .await;
    assert!(read.get("error").is_none(), "readout must answer: {read}");
    assert_eq!(
        read["result"]["account"]["limits"]["maxPositionSize"]["value"],
        serde_json::json!("5"),
        "the DB row wins over the file's stale 999: {read}"
    );
    assert_eq!(
        read["result"]["exit"]["stopLossPct"],
        serde_json::json!(8.0),
        "the DB exit row wins over the file's stale 99: {read}"
    );

    // The watcher asymmetry is closed by construction: with the DB open the
    // cockpit re-derive is disabled (an unrelated .env write cannot clobber
    // the DB-sourced working copies with stale keys).
    {
        let c = core.lock().await;
        assert!(
            c.account_risk_db_open(),
            "precondition: the DB store is open"
        );
        assert!(
            !c.cockpit_env_rederive_allowed(),
            "the env-carrier re-derive must be off while the DB carries the cockpit"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
