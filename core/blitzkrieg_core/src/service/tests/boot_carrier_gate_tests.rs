#![cfg(test)]
//! issue #1199 audit batch E, T1 + T3 — the boot carrier gates, pinned as
//! behaviour (not implementation):
//!
//!  * T1 (P1-1): `cockpit_env_rederive_allowed` — with the account risk DB
//!    open the DB is the cockpit carrier, and an env-file re-derive would
//!    clobber the DB-sourced working copies with the file's stale `BK_*` /
//!    `BK_EXIT_*` keys on ANY unrelated `.env` write. The gate pins the boot
//!    order fact both ways: DB open → re-derive refused; no DB (the
//!    backtester posture) → the re-derive stays armed, because there the env
//!    file IS the carrier and the re-derive is its freshness mechanism.
//!  * T3 (P1-3): `apply_account_strategy_assignments_at_boot` — a SQLite
//!    read failure (table gone, file unreadable) is an `Err`, never a silent
//!    "no rows" that the boot chain would read as "use the env default" and
//!    keep trading with widened limits.

use super::*;
use crate::account::config::{TEST_ALLOWED_ROUNDS, load_account_ledgers_single_source};
use crate::account::default_account_id;
use crate::risk::account_store::ROUND_DURATION_SETTING;

/// A private scratch dir per test (the dispatch-boot fixture shape): the
/// legacy accounts path is never written, the `.env` fixture is the loader's
/// only source, and `store.sqlite` starts empty.
fn boot_scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("bk-boot-carrier-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The one-account book (the zero-config deployment shape), loaded through
/// the same single-source loader the boot uses — no process env, no repo
/// files: the fixture `.env` is the only source this loader reads.
fn one_default_account(dir: &std::path::Path) -> crate::account::AccountLedgers {
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(&env_path, "ACCOUNT_LIST=default\n").expect("fixture");
    load_account_ledgers_single_source(&accounts_path, Some(&env_path), 1, TEST_ALLOWED_ROUNDS)
        .expect("book loads")
}

/// T1 (P1-1): the DB-opened core refuses the cockpit env re-derive; the
/// no-DB core (backtester posture) allows it. Calling the `pub(crate)`
/// predicate directly — the boot chain's only gate for that call.
#[test]
fn db_open_refuses_the_cockpit_env_rederive_no_db_allows_it() {
    // The DB-opened core: the re-derive would clobber DB-sourced working
    // copies with stale `.env` keys — refused.
    let dir = boot_scratch("t1-db");
    let mut with_db = Core::with_account_book(CoreConfig::default(), one_default_account(&dir));
    assert!(
        !with_db.account_risk_db_open() && with_db.cockpit_env_rederive_allowed(),
        "precondition: a freshly assembled core has no store — the env \
         re-derive is allowed"
    );
    with_db
        .open_account_risk_db(dir.join("store.sqlite"))
        .expect("account risk db opens");
    assert!(
        with_db.account_risk_db_open(),
        "the account risk DB is open on this core"
    );
    assert!(
        !with_db.cockpit_env_rederive_allowed(),
        "P1-1 gate: with the DB open the cockpit carrier is the DB — an env \
         re-derive must never run, or any unrelated .env write clobbers the \
         DB-sourced working copies"
    );

    // The no-DB core (backtester / in-process test posture): the env file IS
    // the carrier, so the re-derive (its freshness mechanism) stays allowed.
    let dir2 = boot_scratch("t1-nodb");
    let without_db = Core::with_account_book(CoreConfig::default(), one_default_account(&dir2));
    assert!(!without_db.account_risk_db_open(), "no store on this core");
    assert!(
        without_db.cockpit_env_rederive_allowed(),
        "no DB: the env re-derive is the env-carrier backend's freshness \
         mechanism and stays allowed"
    );
}

/// T3 (P1-3): the carrier corrupted under an open DB — the
/// `account_settings` table dropped after boot opened it — makes the boot
/// assignment apply return `Err`, never a silent `Ok` that the boot chain
/// would read as "no rows, use the env default".
#[test]
fn corrupted_account_settings_table_is_an_err_not_a_silent_env_fallback() {
    let dir = boot_scratch("t3");
    let mut c = Core::with_account_book(CoreConfig::default(), one_default_account(&dir));
    c.open_account_risk_db(dir.join("store.sqlite"))
        .expect("account risk db opens");

    // One real settings row before the corruption: the read that fails below
    // is the row-carrying query, not a vacuum read of an empty table.
    let default_id = default_account_id();
    let db = c.account_risk_db.as_ref().expect("store installed");
    db.save_setting(default_id.as_str(), ROUND_DURATION_SETTING, Some("300"))
        .expect("settings row lands");
    assert!(
        db.load_settings()
            .expect("settings read back")
            .contains_key(default_id.as_str()),
        "the round-duration row is on the table before the corruption"
    );

    // The corruption: the table itself is gone (manual surgery, a partial
    // migration, an operator with a sqlite shell). The NEXT read must fail
    // loudly instead of resolving to "no rows".
    db.db().with_connection(|conn| {
        conn.execute("DROP TABLE account_settings", [])
            .expect("drop the account_settings table")
    });

    let applied = c.apply_account_strategy_assignments_at_boot();
    assert!(
        applied.is_err(),
        "P1-3: a settings read failure is an Err — never a silent 'no row' \
         that would fall back to the env default: {applied:?}"
    );
}
