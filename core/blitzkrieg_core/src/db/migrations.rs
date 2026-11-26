//! Migration runner for the SQLite store (issue #1189 §2/§5).
//!
//! The schema version is tracked in `schema_migrations` — one row per applied
//! migration — so a later wave can add a v2 without guessing what a database
//! on disk already contains. Migrations are applied in ascending order, each
//! inside its own transaction: a failure rolls that migration back and leaves
//! the previous version intact, so the next boot retries the same step.
//!
//! [`SCHEMA_V1`](crate::db::schema::SCHEMA_V1) is idempotent (`IF NOT EXISTS`
//! everywhere), so a partially applied v1 is completed by re-running it.

use rusqlite::Connection;

use crate::db::schema;

/// One schema migration: a version number, a human name, and the SQL that
/// brings a database to that version.
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

/// Every migration this build knows, ascending. Append-only: an existing
/// version's SQL is never edited, a correction is a new version.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "v1: accounts, credentials, risk, runtime, ledger",
        sql: schema::SCHEMA_V1,
    },
    Migration {
        // #1199 PR-0.5 (T-02, drop-table ruling): the five ledger tables
        // (orders/fills/positions/trades/ledger_entries) had zero runtime
        // writers — the live truth is the JSONL files. The v2 drop makes the
        // schema stop advertising storage nothing reads.
        version: 2,
        name: "v2: drop zero-runtime-write ledger tables (orders/fills/positions/trades/ledger_entries, #1199 T-02)",
        sql: schema::SCHEMA_V2,
    },
    Migration {
        // #185: the durable order-intent ledger. Before v3, dedup was memory
        // state (the OME's live-only key + Lua process tables), so a
        // cancel/reject plus reload/restart inside the same round window
        // placed a second order. The unique intent index is the hard gate;
        // the row replays the first submit's real outcome.
        //
        // Literal 3, NOT `schema::SCHEMA_VERSION`: a migration's version is a
        // historical fact, and reading the live constant here would make every
        // future bump rewrite this entry (so two entries would claim the same
        // number and `validate` would reject the list).
        version: 3,
        name: "v3: order_intents durable intent ledger + uq_intent (#185)",
        sql: schema::SCHEMA_V3,
    },
    Migration {
        // v4: the durable closed-trade log. Reverses ONE of v2's five drops
        // (`trades`): it is the only one with a runtime consumer through the
        // store seam. `mode` is part of the row address because one database
        // file serves every mode while the JSONL era kept dry and live trades
        // in two separate files.
        version: schema::SCHEMA_VERSION,
        name: "v4: trades durable closed-trade log (mode-scoped, decimal legs)",
        sql: schema::SCHEMA_V4,
    },
];

/// The version ledger's own DDL. Created before the first read so a brand-new
/// database can be inspected without a chicken-and-egg step;
/// [`schema::SCHEMA_V1`] repeats it to stay a complete standalone schema.
const BOOTSTRAP: &str = "CREATE TABLE IF NOT EXISTS schema_migrations (
  version       INTEGER PRIMARY KEY,
  applied_at_ms INTEGER NOT NULL
);";

/// Milliseconds since the Unix epoch, or 0 if the clock is before it.
fn now_ms() -> i64 {
    crate::db::now_ms()
}

/// Validate the migration list before touching a database: versions must be
/// unique, contiguous and start at 1. A gap would let a database skip a step
/// and a duplicate would apply one twice.
pub fn validate(migrations: &[Migration]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !migrations.is_empty(),
        "migration list is empty — a database with no v1 is not a database"
    );
    for (index, m) in migrations.iter().enumerate() {
        let expected = index as i64 + 1;
        anyhow::ensure!(
            m.version == expected,
            "migration versions must be contiguous from 1: expected {expected}, found {} ({})",
            m.version,
            m.name
        );
    }
    Ok(())
}

/// The versions already recorded in `schema_migrations`.
pub fn applied_versions(conn: &Connection) -> anyhow::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT version FROM schema_migrations ORDER BY version")?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// The highest applied version, or 0 on a fresh database.
pub fn current_version(conn: &Connection) -> anyhow::Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |r| r.get(0),
    )?)
}

/// The column names of `table`, or `None` when the table does not exist.
fn columns_of(conn: &Connection, table: &str) -> anyhow::Result<Option<Vec<String>>> {
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Ok(None);
    }
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
    let cols = stmt
        .query_map([table], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(cols))
}

/// v4 reverses v2's `DROP TABLE trades`, but a database can still carry v2's
/// drop-and-resurrect casualty: the pre-#1199 boot re-ran `SCHEMA_V1` on every
/// start, so the importer's four-column `trades` (`id, account_id,
/// payload_json, exit_time` — no `mode`) came back AFTER v2 was recorded as
/// applied, and v2 never runs again to drop it (#1199 third audit P0-2).
///
/// `SCHEMA_V4`'s `CREATE TABLE IF NOT EXISTS` would then silently keep the old
/// shape and the very next index statement (`ON trades(mode, ...)`) fails with
/// `no such column: mode` — the kernel refuses to boot on a REAL, in-service
/// production database.
///
/// Retiring that corpse is only honest when it holds no rows: it is a table no
/// runtime path writes (the JSONL files were the truth in that era), and this
/// migration's job is to replace it. A NON-EMPTY table of that shape is a
/// closed-trade book we cannot read — a genuine collision between a claim and
/// the schema — so it is refused loudly rather than dropped. Losing a ledger
/// row to convenience is the one thing R7.1 forbids outright.
fn retire_v1_trades_corpse(conn: &Connection) -> anyhow::Result<()> {
    let Some(cols) = columns_of(conn, "trades")? else {
        return Ok(()); // fresh database: v4 creates the table itself
    };
    if cols.iter().any(|c| c == "mode") {
        return Ok(()); // already the v4 address, nothing to retire
    }
    let rows: i64 = conn.query_row("SELECT COUNT(*) FROM trades", [], |r| r.get(0))?;
    anyhow::ensure!(
        rows == 0,
        "the `trades` table on disk predates schema v4 (columns: {cols:?}, no `mode`) and holds \
         {rows} row(s). This migration cannot reinterpret a populated ledger of that shape, and it \
         will not drop one: every close in it would vanish from the book. Export the rows, delete \
         the file, or restore it from backup before booting this build."
    );
    conn.execute_batch("DROP TABLE trades;")?;
    tracing::warn!(
        "retired the empty pre-v4 `trades` table (v2's drop was undone by the old \
         every-boot SCHEMA_V1 re-run, #1199 P0-2); v4 recreates it with the mode-scoped address"
    );
    Ok(())
}

/// Apply every migration the database has not seen yet. Safe to call on every
/// open: applied versions are skipped.
pub fn run(conn: &Connection) -> anyhow::Result<()> {
    validate(MIGRATIONS)?;
    conn.execute_batch(BOOTSTRAP)?;
    let applied = applied_versions(conn)?;
    // v4 needs this BEFORE its own SQL: `CREATE TABLE IF NOT EXISTS` cannot
    // repair a table that exists with the wrong shape.
    if !applied.contains(&4) {
        retire_v1_trades_corpse(conn)?;
    }
    for migration in MIGRATIONS {
        if applied.contains(&migration.version) {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(migration.sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at_ms) VALUES (?1, ?2)",
            rusqlite::params![migration.version, now_ms()],
        )?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::db::test_support::TempDb;

    /// A database file that has v1..v3 recorded and carries the importer's
    /// four-column `trades`, i.e. the exact state the live `data/blitzkrieg.sqlite`
    /// was in. Built with a bare connection on purpose: `Database::open` would
    /// run v4 first and create the right-shaped table.
    fn db_with_v1_trades_corpse(rows: usize) -> TempDb {
        let tmp = TempDb::new();
        // TempDb opens through Database::open (so v4 exists). Start over from a
        // bare file: drop everything and lay down the historical shape.
        {
            let conn = tmp.connection();
            conn.execute_batch(
                "DROP TABLE IF EXISTS trades_archive;
                 DROP TABLE IF EXISTS trades;
                 DROP INDEX IF EXISTS idx_trades_mode_account_exit;
                 DROP INDEX IF EXISTS idx_trades_mode_id;
                 DELETE FROM schema_migrations;",
            )
            .expect("prior shape is cleared");
            seed_v1_corpse(&conn, rows);
            for version in 1..=3 {
                conn.execute(
                    "INSERT INTO schema_migrations (version, applied_at_ms) VALUES (?1, 0)",
                    [version],
                )
                .expect("prior version recorded");
            }
        }
        tmp
    }

    /// The exact production failure this guard exists for: a database that ran
    /// v1→v3 while carrying the importer's four-column `trades` (the old
    /// every-boot `SCHEMA_V1` re-run resurrected it after v2 dropped it), so
    /// `CREATE TABLE IF NOT EXISTS` in v4 silently kept the old shape and
    /// `ON trades(mode, ...)` failed with `no such column: mode`. Observed on a
    /// copy of the live `data/blitzkrieg.sqlite`: the kernel refused to boot.
    fn seed_v1_corpse(conn: &Connection, rows: usize) {
        conn.execute_batch(
            "CREATE TABLE trades (
               id           TEXT PRIMARY KEY,
               account_id   TEXT NOT NULL,
               payload_json TEXT NOT NULL,
               exit_time    INTEGER NOT NULL
             );",
        )
        .expect("v1 trades corpse is created");
        for n in 0..rows {
            conn.execute(
                "INSERT INTO trades (id, account_id, payload_json, exit_time) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![format!("hft-{n}"), "default", r#"{"id":"hft-0"}"#, 1_700_000_000_000i64],
            )
            .expect("row lands in the corpse");
        }
    }

    /// An EMPTY pre-v4 `trades` table is a table nothing writes — retiring it
    /// is how the migration completes, and v4 must then own the address.
    #[test]
    fn an_empty_pre_v4_trades_table_is_retired_and_v4_lands() {
        let tmp = db_with_v1_trades_corpse(0);
        // A fresh Database::open runs the real migration list from that state.
        let reopened =
            Database::open(tmp.path()).expect("v4 upgrades a database with an empty corpse");
        let conn = reopened.lock();
        assert_eq!(current_version(&conn).unwrap(), schema::SCHEMA_VERSION);
        let cols = columns_of(&conn, "trades").unwrap().expect("trades exists");
        assert!(
            cols.iter().any(|c| c == "mode"),
            "v4 must own the address: {cols:?}"
        );
    }

    /// A POPULATED table of the pre-v4 shape is a closed-trade book this
    /// migration cannot reinterpret. Dropping it would erase real closes, so it
    /// must refuse — loudly — and leave every row in place (R7.1: a number you
    /// cannot carry is never silently replaced by nothing).
    #[test]
    fn a_populated_pre_v4_trades_table_refuses_instead_of_dropping_rows() {
        let tmp = db_with_v1_trades_corpse(2);
        // `Database` carries no Debug (it owns a live connection), so match
        // instead of `expect_err`; the Ok arm is the failure we are asserting
        // against.
        let err = match Database::open(tmp.path()) {
            Ok(_) => panic!("a populated ledger of the old shape must not boot"),
            Err(e) => e,
        };
        let message = format!("{err:#}");
        assert!(
            message.contains("holds 2 row(s)"),
            "the refusal must name what it refused to drop: {message}"
        );
        // The rows are still there: the guard refused, it did not wipe.
        let conn = Connection::open(tmp.path()).expect("reopen for inspection");
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM trades", [], |r| r.get(0))
            .expect("the corpse is still readable");
        assert_eq!(rows, 2, "no close may be dropped to make boot succeed");
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('trades')")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            !cols.iter().any(|c| c == "mode"),
            "shape untouched on refusal"
        );
    }

    #[test]
    fn migration_list_is_valid() {
        validate(MIGRATIONS).expect("shipped migration list is contiguous from 1");
    }

    #[test]
    fn validate_rejects_gaps_and_duplicates() {
        let one = Migration {
            version: 1,
            name: "one",
            sql: "SELECT 1",
        };
        let three = Migration {
            version: 3,
            name: "three",
            sql: "SELECT 1",
        };
        assert!(validate(&[one, three]).is_err(), "gap must be refused");
        assert!(
            validate(&[
                Migration {
                    version: 1,
                    name: "a",
                    sql: ""
                },
                Migration {
                    version: 1,
                    name: "b",
                    sql: ""
                }
            ])
            .is_err(),
            "duplicate version must be refused"
        );
        assert!(
            validate(&[Migration {
                version: 2,
                name: "two",
                sql: ""
            }])
            .is_err(),
            "must start at 1"
        );
    }

    #[test]
    fn fresh_database_records_the_current_version() {
        // A raw connection with NO migrations applied: version 0.
        let dir = std::env::temp_dir().join(format!(
            "blitzkrieg-mig-fresh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = Connection::open(dir.join("fresh.sqlite")).unwrap();
        raw.execute_batch(BOOTSTRAP).unwrap();
        assert_eq!(current_version(&raw).unwrap(), 0);
        run(&raw).unwrap();
        assert_eq!(current_version(&raw).unwrap(), schema::SCHEMA_VERSION);
        // Every migration from 1..=SCHEMA_VERSION is recorded, in order.
        let expected: Vec<i64> = (1..=schema::SCHEMA_VERSION).collect();
        assert_eq!(applied_versions(&raw).unwrap(), expected);
        drop(raw);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rerunning_is_idempotent() {
        let db = TempDb::new();
        run(&db.connection()).unwrap();
        run(&db.connection()).unwrap();
        run(&db.connection()).unwrap();
        let expected: Vec<i64> = (1..=schema::SCHEMA_VERSION).collect();
        assert_eq!(
            applied_versions(&db.connection()).unwrap(),
            expected,
            "an applied version is never recorded twice"
        );
    }

    #[test]
    fn every_table_and_index_exists() {
        let db = TempDb::new();
        run(&db.connection()).unwrap();
        let conn = db.connection();
        for table in schema::TABLES {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "table {table} must exist exactly once");
        }
        for index in schema::INDEXES {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                    [index],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "index {index} must exist exactly once");
        }
    }

    #[test]
    fn every_table_is_queryable() {
        let db = TempDb::new();
        run(&db.connection()).unwrap();
        for table in schema::TABLES {
            let sql = format!("SELECT COUNT(*) FROM {table}");
            db.connection()
                .query_row(&sql, [], |r| r.get::<_, i64>(0))
                .unwrap_or_else(|e| panic!("table {table} must be queryable: {e}"));
        }
    }

    /// The upgrade path a real deployment takes (#1199 T-02): a database
    /// created by the v1-only importer (five ledger tables populated) is
    /// migrated in place — the rows go with the tables, the live tables stay,
    /// and the version ledger records v2. A second run changes nothing.
    #[test]
    fn v1_database_upgrades_in_place_and_loses_the_five_tables() {
        let dir = std::env::temp_dir().join(format!(
            "blitzkrieg-mig-v1up-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("v1.sqlite")).unwrap();
        // Apply ONLY v1: the shape the one-shot importer leaves behind.
        conn.execute_batch(schema::SCHEMA_V1).unwrap();
        conn.execute_batch(BOOTSTRAP).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at_ms) VALUES (1, 0)",
            [],
        )
        .unwrap();
        // Rows in the doomed tables (the importer's legacy folds).
        conn.execute(
            "INSERT INTO orders (account_id, order_id, payload_json, updated_at_ms) \
             VALUES ('default', 'o1', '{}', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ledger_entries (account_id, kind, amount, payload_json, ts_ms) \
             VALUES ('default', 'seed', '1', '{}', 0)",
            [],
        )
        .unwrap();
        // One row in a live table, which must SURVIVE the upgrade.
        conn.execute(
            "INSERT INTO accounts (id, name, market_type, status, trading_enabled, updated_at_ms) \
             VALUES ('default', 'D', 'prediction', 'active', 1, 0)",
            [],
        )
        .unwrap();

        run(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), schema::SCHEMA_VERSION);
        assert_eq!(
            applied_versions(&conn).unwrap(),
            (1..=schema::SCHEMA_VERSION).collect::<Vec<_>>(),
            "every migration recorded"
        );

        // v2 dropped all five… (historical fact, asserted on the DDL above).
        // …but v4 RE-CREATES `trades`, so on a fully migrated database only
        // FOUR of them are still absent. The v1 `trades` shape is gone for
        // good: the v4 table is a different table that happens to share a name.
        for (kind, name) in [
            ("table", "orders"),
            ("table", "fills"),
            ("table", "positions"),
            ("table", "ledger_entries"),
            ("index", "idx_orders_account"),
            ("index", "idx_fills_account"),
            ("index", "idx_positions_account"),
            ("index", "idx_trades_account"),
            ("index", "idx_ledger_account"),
        ] {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
                    [kind, name],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                found, 0,
                "{kind} {name} must be gone after v2 (and not revived)"
            );
        }
        // `trades` exists again — created by v4, NOT by the surviving v1 DDL.
        // Its v1 index must stay absent (the v4 indexes replace it), which is
        // the check above; this asserts the table itself and its new identity.
        let trades_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'trades'",
                [],
                |r| r.get(0),
            )
            .expect("v4 re-creates the trades table");
        assert!(
            trades_sql.contains("PRIMARY KEY (mode, account_id, id, exit_time)"),
            "the surviving trades table must be the v4 shape (mode-scoped identity), \
             not the v1 shape: {trades_sql}"
        );
        // The live tables are untouched and the account row survived.
        let accounts: i64 = conn
            .query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(accounts, 1, "accounts table survives with its rows");
        for table in schema::TABLES {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "table {table} must exist after the upgrade");
        }

        // Re-running the migration is a no-op (idempotent upgrade).
        run(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), schema::SCHEMA_VERSION);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
