// Issue #188 — the enabled strategy set has ONE authority.
//
// THE DEFECT THIS PINS
//   The same "enabled strategy set" was written to two carriers: SQLite
//   `account_settings.strategies` AND the legacy `strategy-state.json`
//   mirror, from four production call sites. `strategy_state::save`
//   reported failure with a `warn!` and returned `()` unconditionally, so a
//   mirror that never landed was indistinguishable from one that did — the
//   two carriers could diverge silently and the operator would only learn
//   about it at the next boot (R7.1 L4, silent degradation).
//
// WHAT IS PINNED HERE
//   1. `strategy.enable` on an account lands the set in SQLite and the
//      engine scope agrees with the row — the authority is the row, not a
//      side file.
//   2. A boot with NO `strategy-state.json` at all still restores the set
//      from the SQLite row. This is the load-bearing consequence of
//      "SQLite is the authority": if boot ever regressed to reading only the
//      legacy mirror, this goes red.
//   3. A mirror write that cannot land SURFACES an error instead of being
//      swallowed by a `warn!`.
//
// Runs under `#![cfg(feature = "test-support")]` — the CI matrix passes
// `--features blitzkrieg-core/test-support`; the default-feature build
// compiles this target out, same as `account_decouple_check.rs`.
#![cfg(feature = "test-support")]

use blitzkrieg_core::account::{Account, AccountId, AccountLedgers, AccountStatus};
use blitzkrieg_core::risk::account_store::{AccountRiskDb, STRATEGIES_SETTING};
use blitzkrieg_core::service::{Core, CoreConfig};
use blitzkrieg_market_api::net::now_ms;

/// A scratch directory under the target tree (never /tmp, never the repo
/// root) so a panicking test cannot leave debris inside the workspace.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("issue-188")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn one_account_book(strategies: Vec<String>) -> AccountLedgers {
    AccountLedgers::new(
        vec![Account {
            id: AccountId::from("default"),
            name: "default".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: AccountStatus::Active,
            credential_keys: blitzkrieg_core::account::CredentialKeys::default(),
            venue: String::new(),
            trading_enabled: true,
            strategies,
            rounds: Vec::new(),
            opening_equity: None,
            updated_at_ms: now_ms(),
        }],
        AccountId::from("default"),
    )
}

/// Build a core over a REAL SQLite file with the two test adapters hosted
/// (both DISABLED — the zero-default shape a fresh boot has). The legacy
/// mirror path is deliberately NOT configured: the whole point is that the
/// authority needs no mirror.
fn core_with_store(dir: &std::path::Path, strategies: Vec<String>) -> Core {
    let cfg = CoreConfig {
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        strategy_state_path: None,
        db_path: Some(dir.join("store.sqlite")),
        ..Default::default()
    };
    let mut core = Core::with_account_book(cfg, one_account_book(strategies));
    let engine_cfg = core.config().engine_config();
    let mut engine = blitzkrieg_core::engine::Engine::new(engine_cfg.clone());
    blitzkrieg_core::strategies::test_support::host_disabled(
        &mut engine,
        engine_cfg.trend,
        engine_cfg.spread_arb,
    );
    core.enable_engine(engine);
    core
}

/// The authoritative row, read from the database FILE — not through the
/// in-memory book (which would only restate what the write just did).
fn stored_set(dir: &std::path::Path) -> Option<String> {
    let db = AccountRiskDb::open(dir.join("store.sqlite")).expect("reopen store");
    db.load_settings()
        .expect("settings read")
        .get("default")
        .and_then(|keys| keys.get(STRATEGIES_SETTING).cloned())
}

// ── 1. the authority and the scope move together ───────────────────────────

/// `strategy.enable` on `default` must move the SQLite row and the engine
/// scope together: the row is the authority, and the scope agrees with it.
#[test]
fn strategy_enable_writes_the_authority_and_the_scope_agrees() {
    let dir = scratch("enable");
    let mut core = core_with_store(&dir, Vec::new());

    let (kept, set_now) = core
        .set_scoped_strategy_enabled(
            &AccountId::from("default"),
            "spread_arb",
            true,
            "test",
            "issue #188 pin",
            now_ms(),
        )
        .expect("the default-account enable lands");

    assert!(kept.is_empty(), "a fresh scope has no held positions");
    assert_eq!(set_now, vec!["spread_arb".to_string()]);

    // The authority (the SQLite row) carries the set.
    assert_eq!(
        stored_set(&dir).as_deref(),
        Some("[\"spread_arb\"]"),
        "the SQLite row is the authority and must carry the enabled set"
    );
    // The book (what the next boot derives from) agrees.
    assert_eq!(
        core.accounts()
            .strategy_assignment(&AccountId::from("default")),
        Some(&vec!["spread_arb".to_string()]),
        "the in-memory book agrees with the row"
    );
    // The engine scope is enabled — assignment IS enablement.
    assert!(
        core.enabled_strategy_names()
            .contains(&"spread_arb".to_string()),
        "the scope is enabled in the live dispatch"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ── 2. deleting the legacy mirror loses nothing ────────────────────────────

/// THE KEY TOOTH of "SQLite is the authority": a boot with NO
/// `strategy-state.json` at all still restores the set from the SQLite row.
/// If boot ever regressed to reading only the legacy mirror, this goes red.
#[test]
fn deleting_the_state_file_still_boots_the_set_from_sqlite() {
    let dir = scratch("no-mirror");

    // Boot 1: the operator enables a strategy. Only the SQLite row exists —
    // no mirror path is configured, so no JSON file is ever created.
    {
        let mut core = core_with_store(&dir, Vec::new());
        core.set_scoped_strategy_enabled(
            &AccountId::from("default"),
            "spread_arb",
            true,
            "test",
            "issue #188 pin",
            now_ms(),
        )
        .expect("the enable lands");
    }
    assert!(
        !dir.join("strategy-state.json").exists(),
        "precondition: the authority path never writes the legacy mirror"
    );
    assert_eq!(
        stored_set(&dir).as_deref(),
        Some("[\"spread_arb\"]"),
        "precondition: the row survived the core's drop"
    );

    // Boot 2: a FRESH core over the same store file. The row is the only
    // carrier that exists; the boot must apply it.
    let mut rebooted = core_with_store(&dir, Vec::new());
    rebooted
        .apply_account_strategy_assignments_at_boot()
        .expect("boot applies the assignments");
    assert_eq!(
        rebooted.enabled_strategy_names(),
        vec!["spread_arb".to_string()],
        "the SQLite row alone restores the set — no JSON mirror needed"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ── 3. the authority is written EXACTLY once per toggle ────────────────────

/// A real SQLite trigger counts every write to `account_settings.strategies`.
/// Exists so the "exactly once" claim is MEASURED, never asserted from a
/// reading of the call graph: the run below produces the number.
fn install_write_counter(dir: &std::path::Path) {
    let db = blitzkrieg_core::db::Database::open(dir.join("store.sqlite")).expect("counter db");
    db.with_connection(|conn| {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS i188_log (value TEXT NOT NULL);
             CREATE TRIGGER IF NOT EXISTS i188_ins AFTER INSERT ON account_settings
               WHEN NEW.key = 'strategies'
               BEGIN INSERT INTO i188_log(value) VALUES (NEW.value); END;
             CREATE TRIGGER IF NOT EXISTS i188_upd AFTER UPDATE ON account_settings
               WHEN NEW.key = 'strategies'
               BEGIN INSERT INTO i188_log(value) VALUES (NEW.value); END;",
        )
        .expect("install the write-counting triggers");
    });
}

fn counted_writes(dir: &std::path::Path) -> Vec<String> {
    let db = blitzkrieg_core::db::Database::open(dir.join("store.sqlite")).expect("counter db");
    db.with_connection(|conn| {
        let mut stmt = conn
            .prepare("SELECT value FROM i188_log ORDER BY rowid")
            .expect("prepare");
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect();
        rows
    })
}

/// `strategy.enable` must write the AUTHORITY once — not once per carrier.
/// The pre-#188 shape had no single owner: the account write and the legacy
/// mirror each carried their own `save` call, so one operator action could
/// touch the durable row more than once.
#[test]
fn strategy_enable_writes_sqlite_exactly_once() {
    let dir = scratch("once");
    let mut core = core_with_store(&dir, Vec::new());
    install_write_counter(&dir);

    core.set_scoped_strategy_enabled(
        &AccountId::from("default"),
        "spread_arb",
        true,
        "test",
        "issue #188 exactly-once",
        now_ms(),
    )
    .expect("the enable lands");

    let writes = counted_writes(&dir);
    assert_eq!(
        writes.len(),
        1,
        "one toggle must write account_settings.strategies exactly once, got: {writes:?}"
    );
    assert_eq!(
        writes[0], "[\"spread_arb\"]",
        "the single write carries the new set"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ── 3b. the FULL `strategy.enable` handler sequence ────────────────────────

/// The `strategy.enable` default-account branch runs TWO service calls:
/// `set_strategy_enabled_checked` (the global/local toggle) and then
/// `set_scoped_strategy_enabled` (the account-scoped write). Issue #188
/// asserted this could write SQLite twice; this MEASURES it instead of
/// trusting either the audit or a reading of the call graph.
///
/// The scope write is the only one that owns the durable row, so the
/// handler sequence must still produce exactly ONE `account_settings`
/// write — a second one would mean two owners of the same fact.
#[test]
fn the_full_enable_handler_sequence_writes_sqlite_exactly_once() {
    let dir = scratch("handler-seq");
    let mut core = core_with_store(&dir, Vec::new());
    install_write_counter(&dir);

    // Step 1: the branch's first call (handshake + toggle + mirror).
    let (_found, _applied, refusal) = core.set_strategy_enabled_checked("spread_arb", true);
    assert!(
        refusal.is_none(),
        "the test adapters declare no modes to refuse"
    );
    // Step 2: the branch's scoped write (the durable half).
    core.set_scoped_strategy_enabled(
        &AccountId::from("default"),
        "spread_arb",
        true,
        "test",
        "issue #188 handler sequence",
        now_ms(),
    )
    .expect("the scoped write lands");

    let writes = counted_writes(&dir);
    assert_eq!(
        writes.len(),
        1,
        "the handler sequence must write the authority exactly once; \
         more than one means a second writer owns the same fact: {writes:?}"
    );
    assert_eq!(writes[0], "[\"spread_arb\"]");

    let _ = std::fs::remove_dir_all(&dir);
}

// ── 3c. the legacy mirror ends on the AUTHORITY-derived set ───────────────

/// The `strategy.enable` default branch writes the legacy mirror TWICE, from
/// two different call sites carrying two different facts:
///
///   1. `set_strategy_enabled_checked` → the GLOBAL dispatch's enabled set;
///   2. `set_scoped_strategy_enabled` → the ACCOUNT's enablement set (the one
///      SQLite row also carries).
///
/// One file, two facts, last writer wins. This pins that the ORDER is the
/// safe one: the account-scoped write (whose fact is the authority's) runs
/// last, so the mirror ends agreeing with the SQLite row rather than with a
/// global set the durable carrier never recorded. If a future edit reorders
/// these two calls, the mirror starts disagreeing with the authority and this
/// goes red — which is exactly the divergence #188 is about.
#[test]
fn the_legacy_mirror_ends_on_the_set_the_authority_carries() {
    let dir = scratch("mirror-order");
    let mirror = dir.join("strategy-state.json");

    // A core WITH the legacy mirror configured (the production shape for an
    // upgraded deployment).
    let cfg = CoreConfig {
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        strategy_state_path: Some(mirror.to_string_lossy().into_owned()),
        db_path: Some(dir.join("store.sqlite")),
        ..Default::default()
    };
    let mut core = Core::with_account_book(cfg, one_account_book(Vec::new()));
    let engine_cfg = core.config().engine_config();
    let mut engine = blitzkrieg_core::engine::Engine::new(engine_cfg.clone());
    blitzkrieg_core::strategies::test_support::host_disabled(
        &mut engine,
        engine_cfg.trend,
        engine_cfg.spread_arb,
    );
    core.enable_engine(engine);

    // The production handler sequence, in the handler's order.
    let (_found, _applied, refusal) = core.set_strategy_enabled_checked("spread_arb", true);
    assert!(refusal.is_none());
    core.set_scoped_strategy_enabled(
        &AccountId::from("default"),
        "spread_arb",
        true,
        "test",
        "issue #188 mirror order",
        now_ms(),
    )
    .expect("the scoped write lands");

    let on_disk = blitzkrieg_core::strategy_state::load(&mirror);
    let authority_like = stored_set(&dir).expect("the authority row exists");
    assert_eq!(
        on_disk,
        serde_json::from_str::<Vec<String>>(&authority_like).expect("the row is a JSON array"),
        "the mirror must end agreeing with the authoritative SQLite row"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ── 4. a mirror write that cannot land is surfaced ─────────────────────────

/// #188 (R7.1 L4): the legacy mirror's failure must be reported, not
/// swallowed by a `warn!` (the pre-#188 `save` returned `()` on every path).
/// The parent path here is a regular FILE, so creating the state directory
/// cannot succeed.
#[test]
fn state_file_write_failure_is_surfaced_not_swallowed() {
    let dir = scratch("mirror-fail");
    let blocker = dir.join("blocked");
    std::fs::write(&blocker, b"not a directory").expect("blocker file");
    let path = blocker.join("strategy-state.json");

    let err = blitzkrieg_core::strategy_state::save(&path, &["spread_arb".to_string()])
        .expect_err("a mirror that cannot be written must report why");
    assert!(
        err.contains("cannot create strategy state dir"),
        "the failure names the step that failed: {err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
