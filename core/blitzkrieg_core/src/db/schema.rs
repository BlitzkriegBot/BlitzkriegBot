//! SQLite schema (issue #1189, §2; v2 = #1199 PR-0.5 T-02).
//!
//! One database holds both account state and kernel metadata. Every monetary
//! value is a **decimal string** (`TEXT`), never a SQLite `REAL` — the kernel
//! speaks `rust_decimal`, and a binary float would silently lose the precision
//! the ledger's cash identity depends on.
//!
//! The SQL here is the single definition of the schema. [`crate::db::migrations`]
//! owns the version number and the runner; this module owns the DDL and the
//! table/index inventory the tests check against.
//!
//! # v2 (#1199 PR-0.5, T-02): the five zero-runtime-write tables are dropped
//!
//! `orders` / `fills` / `positions` / `trades` / `ledger_entries` never had a
//! runtime writer: the live truth for all five lives in the `data/*.jsonl`
//! files (`order_db.rs` / `position_db.rs` / `ledger.rs`), and
//! the only rows the SQLite tables ever carried came from the one-shot
//! importer. Keeping them made the schema a lying surface — it advertised
//! storage nothing reads. [`SCHEMA_V2`] drops them (and their indexes);
//! [`SCHEMA_V1`] below stays byte-identical (append-only: a v1 database on
//! disk is upgraded by the new migration, never by editing the old DDL).

/// The schema version this module defines. Bumped whenever [`SCHEMA_V1`] is
/// superseded by a new migration (never edited in place).
pub const SCHEMA_VERSION: i64 = 4;

/// Every table the CURRENT schema creates, in dependency order (parents first).
///
/// The test suite asserts this list against `sqlite_master` so a table that is
/// renamed or dropped cannot pass unnoticed. Since v2 (#1199 T-02) this is the
/// post-migration target state: the five ledger tables the importer alone ever
/// wrote are dropped, and the JSONL files are their only home.
///
/// v4 reverses that for ONE of the five: `trades` comes back, because the
/// closed-trade log is the only money fact the panel's headline number comes
/// from and v2's reasoning ("nothing reads it") stopped being true the moment
/// the store seam grew a trades domain. See [`SCHEMA_V4`] — and note the table
/// is NOT the v1 shape (that one had no account/exit index and no mode column).
pub const TABLES: &[&str] = &[
    "schema_migrations",
    "kernel_meta",
    "accounts",
    "account_credentials",
    "account_risk",
    "account_exit_policy",
    "account_settings",
    "strategy_assignments",
    "venues",
    "markets",
    "rounds",
    "arb_pairs",
    "audit_log",
    // v3 (#185): the durable order-intent ledger.
    "order_intents",
    // v4: the durable closed-trade log (the one v2 drop that is reversed).
    "trades",
    // v4: where an account-history wipe preserves what it erased (#1253).
    // A SEPARATE table, not archived rows inside `trades`: mixing them would
    // put an "is this row live?" predicate on every read, and a reader that
    // forgot the predicate would count erased trades in the headline.
    "trades_archive",
];

/// Every index the CURRENT schema creates. Since v2 (#1199 T-02) the five
/// per-account ledger indexes went with their tables; v3 (#185) adds the
/// intent ledger's unique intent index plus the age index its capacity
/// policy prunes by. v4 adds the trades page index and its ledger-wide id
/// scan index.
pub const INDEXES: &[&str] = &[
    "idx_audit_ts",
    "idx_rounds_account",
    "idx_markets_venue",
    "uq_intent",
    "idx_intents_created",
    // v4 trades: the page query's (mode, account, exit_time DESC) cover, plus
    // the unscoped ledger-wide index the id-tail memory reads.
    "idx_trades_mode_account_exit",
    "idx_trades_mode_id",
    // v4 archive: "what did that wipe remove from this account" reads
    // newest-archive-first.
    "idx_trades_archive_account_time",
];

/// The complete v1 schema.
///
/// Idempotent by construction (`IF NOT EXISTS` everywhere) so a partially
/// applied file can be re-run safely. Amounts, prices and share counts are
/// `TEXT`; `INTEGER` columns carry only counts, flags and epoch milliseconds.
pub const SCHEMA_V1: &str = r#"
-- Schema version ledger. The migration runner owns this table's bootstrap;
-- it is repeated here so SCHEMA_V1 alone is a complete v1 database.
CREATE TABLE IF NOT EXISTS schema_migrations (
  version       INTEGER PRIMARY KEY,
  applied_at_ms INTEGER NOT NULL
);

-- Kernel metadata (import marker, active/display account, update + runtime
-- cells). `dry_run` is deliberately NOT storable here: whether the process
-- simulates is decided by the environment alone (issue #1189 §2/§4).
CREATE TABLE IF NOT EXISTS kernel_meta (
  key           TEXT PRIMARY KEY,
  value         TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  CHECK (lower(key) <> 'dry_run')
);

CREATE TABLE IF NOT EXISTS accounts (
  id              TEXT PRIMARY KEY,
  name            TEXT NOT NULL,
  market_type     TEXT NOT NULL,          -- prediction / spot / ...
  status          TEXT NOT NULL,          -- active / read_only / frozen / suspended
  status_reason   TEXT,
  venue           TEXT NOT NULL DEFAULT '',
  trading_enabled INTEGER NOT NULL,
  rounds_json     TEXT NOT NULL DEFAULT '[]',
  opening_equity  TEXT,                   -- decimal string, never REAL
  updated_at_ms   INTEGER NOT NULL
);

-- Credential ENV VAR NAMES only, plus the AES-256-GCM sealed payload
-- (nonce || ciphertext || tag) and the name of the env var holding the
-- master key. The plaintext value never lands in this table.
CREATE TABLE IF NOT EXISTS account_credentials (
  account_id         TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  api_key_env        TEXT,
  secret_env         TEXT,
  passphrase_env     TEXT,
  private_key_env    TEXT,
  encrypted_payload  BLOB,
  encryption_key_env TEXT,
  updated_at_ms      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS account_risk (
  account_id             TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  max_single_loss_usd    TEXT NOT NULL,
  max_daily_drawdown_usd TEXT NOT NULL,
  max_position_size      TEXT NOT NULL,
  max_consecutive_losses TEXT NOT NULL,
  cooldown_minutes       TEXT NOT NULL,
  source                 TEXT NOT NULL,   -- default / imported_env / imported_toml / panel / flag
  updated_at_ms          INTEGER NOT NULL
);

-- The full ExitConfig as JSON; `source` as in account_risk.
CREATE TABLE IF NOT EXISTS account_exit_policy (
  account_id    TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  policy_json   TEXT NOT NULL,
  source        TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL
);

-- Execution policy, sizing ratios, daily loss, round anchor, hot caps, the
-- shadow-evolution copy. Money and ratios are decimal strings; POLICY_RULES
-- values are JSON text.
CREATE TABLE IF NOT EXISTS account_settings (
  account_id    TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  key           TEXT NOT NULL,
  value         TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  PRIMARY KEY (account_id, key)
);

CREATE TABLE IF NOT EXISTS strategy_assignments (
  account_id         TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  strategy           TEXT NOT NULL,
  enabled            INTEGER NOT NULL,
  params_json        TEXT,
  round_duration_sec INTEGER,             -- STRATEGY_<NAME>_ROUND_DURATION_SEC
  updated_at_ms      INTEGER NOT NULL,
  PRIMARY KEY (account_id, strategy)
);

CREATE TABLE IF NOT EXISTS venues (
  id          TEXT PRIMARY KEY,
  kind        TEXT NOT NULL DEFAULT '',
  config_json TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS markets (
  id         TEXT PRIMARY KEY,
  venue_id   TEXT NOT NULL,
  account_id TEXT,
  meta_json  TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS rounds (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id    TEXT NOT NULL,
  venue_id      TEXT,
  market_id     TEXT,
  duration_sec  INTEGER,
  started_at_ms INTEGER,
  ends_at_ms    INTEGER,
  state_json    TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS orders (
  account_id    TEXT NOT NULL,
  order_id      TEXT NOT NULL,
  payload_json  TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  PRIMARY KEY (account_id, order_id)
);

CREATE TABLE IF NOT EXISTS fills (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id   TEXT NOT NULL,
  order_id     TEXT,
  payload_json TEXT NOT NULL,
  ts_ms        INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS positions (
  account_id    TEXT NOT NULL,
  position_id   TEXT NOT NULL,
  payload_json  TEXT NOT NULL,
  open          INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  PRIMARY KEY (account_id, position_id)
);

CREATE TABLE IF NOT EXISTS trades (
  id           TEXT PRIMARY KEY,
  account_id   TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  exit_time    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS ledger_entries (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id   TEXT NOT NULL,
  kind         TEXT NOT NULL,
  amount       TEXT NOT NULL,             -- decimal string
  payload_json TEXT NOT NULL,
  ts_ms        INTEGER NOT NULL
);

-- Cross-account arbitrage legs. v1 holds exactly one row (id = 1); it belongs
-- to two accounts, so it is NOT folded into a single account's settings.
CREATE TABLE IF NOT EXISTS arb_pairs (
  id            INTEGER PRIMARY KEY CHECK (id = 1),
  leg_a_venue   TEXT NOT NULL,
  leg_a_account TEXT NOT NULL,
  leg_b_venue   TEXT NOT NULL,
  leg_b_account TEXT NOT NULL,
  pairs_json    TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS audit_log (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  ts_ms       INTEGER NOT NULL,
  account_id  TEXT,
  actor       TEXT NOT NULL,
  action      TEXT NOT NULL,
  detail_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_fills_account     ON fills(account_id, ts_ms);
CREATE INDEX IF NOT EXISTS idx_ledger_account    ON ledger_entries(account_id, ts_ms);
CREATE INDEX IF NOT EXISTS idx_audit_ts          ON audit_log(ts_ms);
CREATE INDEX IF NOT EXISTS idx_rounds_account    ON rounds(account_id);
CREATE INDEX IF NOT EXISTS idx_orders_account    ON orders(account_id, updated_at_ms);
CREATE INDEX IF NOT EXISTS idx_positions_account ON positions(account_id, open);
CREATE INDEX IF NOT EXISTS idx_trades_account    ON trades(account_id, exit_time);
CREATE INDEX IF NOT EXISTS idx_markets_venue     ON markets(venue_id);
"#;

/// The v2 migration (#1199 PR-0.5, T-02 — the "write-matrix lockdown" ruling):
/// drop the five tables that never had a runtime writer.
///
/// `orders` / `fills` / `positions` / `trades` / `ledger_entries` were written
/// ONLY by the one-shot legacy importer; every runtime read and write of those
/// facts goes to the `data/*.jsonl` files. The empty tables made the schema
/// advertise storage nothing consumes, so v2 removes them — a database that
/// boots through the migration runner converges on the same shape the runtime
/// actually uses.
///
/// Indexes drop with their tables (SQLite would remove them implicitly, but
/// the explicit `DROP INDEX IF EXISTS` keeps the intent readable and the
/// statements order-independent). No foreign keys point at these tables, so the
/// drop order is free; the child-before-parent spelling is defensive habit.
/// Idempotent (`IF EXISTS` everywhere), so a partially applied file re-runs
/// safely — same discipline as [`SCHEMA_V1`].
pub const SCHEMA_V2: &str = r#"
DROP INDEX IF EXISTS idx_fills_account;
DROP INDEX IF EXISTS idx_ledger_account;
DROP INDEX IF EXISTS idx_orders_account;
DROP INDEX IF EXISTS idx_positions_account;
DROP INDEX IF EXISTS idx_trades_account;
DROP TABLE IF EXISTS fills;
DROP TABLE IF EXISTS ledger_entries;
DROP TABLE IF EXISTS positions;
DROP TABLE IF EXISTS trades;
DROP TABLE IF EXISTS orders;
"#;

/// The v3 migration (issue #185 — order-intent idempotency): the durable
/// intent ledger.
///
/// # Why this table exists
///
/// Before v3, idempotency was implemented as MEMORY state: the OME's
/// `internal_key` dedup (`has_pending`) only refuses while the first order is
/// still `is_live()`, and a Lua package's own `quoted` / `slot_seen` tables
/// live inside the process. A cancel/reject followed by `strategy.reload` (or
/// any restart) inside the same 45–60 s round window therefore let the SAME
/// `(strategy, asset, direction, slot)` intent place a second order. The
/// `orders` table could never have blocked it: its key was the locally-minted
/// order id (and since v2 that table is gone entirely).
///
/// `order_intents` is that missing structure: ONE row per intent identity,
/// claimed BEFORE any ledger/order-state write, carrying the FIRST submit's
/// outcome so a duplicate replays it instead of placing again.
///
/// # Capacity (bounded, with an explicit policy)
///
/// The table is bounded by BOTH an age window and a hard row cap; the policy
/// lives in `crate::db::intents` and is pruned on every claim transaction:
///
/// * rows older than [`INTENT_RETENTION_MS`] are deleted — the registry is a
///   dedup window, not an audit trail (the order log is the audit trail), and
///   a round slot is dead long before that window closes;
/// * if more than [`INTENT_MAX_ROWS`] rows remain, the OLDEST are deleted
///   down to the cap (LRU-by-age: the newest intents are the ones a retry can
///   still collide with).
///
/// Pruning only ever removes rows that can no longer be retried inside a live
/// round window, so it can never open a duplicate-placement hole; and a
/// failed prune is fail-open for the CLAIM (a too-large table is still
/// correct, only unbounded — the log line says so).
///
/// # States
///
/// `state` carries the first submit's lifecycle:
/// `reserved` (claimed, submit in flight), `ordered` (order id persisted),
/// `refused` (first submit was refused — code + message replayed),
/// `consumed`/`abandoned` are terminal for rows whose round is over.
pub const SCHEMA_V3: &str = r#"
-- Issue #185: the durable order-intent ledger. One row per intent identity
-- (account, strategy, condition, round slot, direction). The UNIQUE index is
-- the hard gate: a duplicate insert fails, and the caller replays the stored
-- first outcome instead of placing a second order.
CREATE TABLE IF NOT EXISTS order_intents (
  account_id     TEXT NOT NULL,
  strategy       TEXT NOT NULL,
  condition_id   TEXT NOT NULL,
  round_slot     INTEGER NOT NULL,
  direction      TEXT NOT NULL,
  -- IntentState as lowercase text: reserved | ordered | refused | consumed | abandoned
  state          TEXT NOT NULL,
  -- The FIRST submit's order id (ordered/consumed only). NULL while reserved.
  order_id       TEXT,
  -- The FIRST submit's refusal (refused only), replayed byte-identically.
  reject_code    TEXT,
  reject_message TEXT,
  -- The first submit's order status at claim time (ordered only), so a
  -- duplicate answers the same status the first caller saw.
  order_status   TEXT,
  created_at_ms  INTEGER NOT NULL,
  updated_at_ms  INTEGER NOT NULL,
  PRIMARY KEY (account_id, strategy, condition_id, round_slot, direction)
);

-- The contract's unique intent index (the PRIMARY KEY above already enforces
-- it; this named index is what a schema reader greps for and what a future
-- ALTER can rely on). It is declared explicitly per the #185 contract.
CREATE UNIQUE INDEX IF NOT EXISTS uq_intent
  ON order_intents(account_id, strategy, condition_id, round_slot, direction);

-- Capacity policy's prune cursor: oldest-first deletes scan this index.
CREATE INDEX IF NOT EXISTS idx_intents_created
  ON order_intents(created_at_ms);
"#;

/// v4: the durable closed-trade log — one row per realized close.
///
/// # Why this reverses one of v2's five drops
///
/// v2 dropped five ledger tables because "nothing reads them": the runtime
/// truth lived in `data/*.jsonl` and keeping the tables made the schema
/// advertise storage nobody used. That reasoning was correct for FOUR of them
/// and wrong for `trades`: the closed-trade log is where the panel's headline
/// realized-PnL number comes from, and it is now consumed through the store
/// seam ([`crate::contracts::TradeRead`] / [`crate::contracts::TradeWrite`]),
/// which v2 knew nothing about. The other four stay dropped.
///
/// # Shape
///
/// `payload_json` carries the full [`crate::trade::TradeRecord`] verbatim so
/// the wire shape has exactly one definition (`TradeRecord`'s serde) rather
/// than a second column-per-field mapping that would drift from it. The
/// columns beside it exist only for what the seam QUERIES on:
///
/// * `mode` — dry and live keep SEPARATE books. The JSONL era spelled this as
///   two filenames (`trades.jsonl` vs `trades-live.jsonl`) precisely so a live
///   panel could not render dry-era rows; a single table without this column
///   would silently merge them, which is a worse bug than the one being fixed.
/// * `account_id` — every read is account-scoped (#1221/#1223).
/// * `exit_time` — the page order and the second half of the row identity.
/// * `net_pnl_usd` / `gross_pnl_usd` / `fees_usd` / `hold_time_sec` /
///   `source` — the aggregate legs. Stored as decimal strings (never REAL:
///   [`SCHEMA_V1`]'s rule applies to every version) so `SUM` runs on exact
///   decimals rather than IEEE floats.
///
/// # Identity
///
/// `PRIMARY KEY (mode, account_id, id, exit_time)` is the row address the
/// contract calls [`crate::contracts::TradeIdentity`]. It deliberately excludes
/// `net_pnl_usd`: the JSONL version had to include that moving field because a
/// file line had no stable identity, which is why `amend_net_pnl` could not
/// address a row it had already amended. In a table the uniqueness is a
/// constraint, so the amendment guards its precondition with an explicit
/// compare-and-swap instead.
///
/// A duplicate `(mode, account, id, exit_time)` is a REAL COLLISION: the
/// position book reuses `hft-N` ids across restarts (#942 records nine such
/// collisions in production), so a reissued id can legitimately meet an old
/// close at a different time — but the same id at the same exit time is the
/// same close twice, and the importer refuses it rather than collapsing it.
pub const SCHEMA_V4: &str = r#"
-- The durable closed-trade log. See this constant's doc for why v2's drop of
-- `trades` is reversed here and why `mode` is part of the row address.
CREATE TABLE IF NOT EXISTS trades (
  -- The book this close belongs to: 'dry' | 'live' | 'readonly'. Two modes
  -- share one database file (db_path is single), so the separation the JSONL
  -- era got from two filenames has to be a column here.
  mode         TEXT NOT NULL,
  account_id   TEXT NOT NULL,
  -- The position book's id at close (`hft-N`), reused across restarts.
  id           TEXT NOT NULL,
  -- The close's own timestamp, epoch milliseconds. Part of the identity:
  -- it does not move under an amendment, and two closes of one id cannot
  -- share it.
  exit_time    INTEGER NOT NULL,
  -- The full TradeRecord, verbatim (camelCase wire shape). One definition of
  -- the trade row, not a second column-per-field mapping to keep in sync.
  payload_json TEXT NOT NULL,
  -- Aggregate legs, copied out of the payload so SUM/COUNT run in SQL.
  -- Decimal strings, never REAL: a float SUM would lose the precision the
  -- cash identity depends on.
  net_pnl_usd    TEXT NOT NULL,
  gross_pnl_usd  TEXT NOT NULL,
  fees_usd       TEXT NOT NULL,
  hold_time_sec  INTEGER NOT NULL,
  -- The close's provenance: 'venue' (a real venue verdict) or 'fallback'
  -- (bookkeeping value with no venue answer). R7.1 requires the unverified
  -- slice to be reportable as its own line, so it is a queryable column, not
  -- a string buried in JSON.
  source         TEXT NOT NULL DEFAULT '',
  -- When this row landed, epoch milliseconds (carrier commit time).
  written_at_ms  INTEGER NOT NULL,
  PRIMARY KEY (mode, account_id, id, exit_time)
);

-- The page query's cover: newest-first within one account's book.
CREATE INDEX IF NOT EXISTS idx_trades_mode_account_exit
  ON trades(mode, account_id, exit_time);

-- The ledger-wide id memory (max hft-N tail, issue #942) is unscoped by
-- account on purpose: it seeds the position counter for the whole book.
CREATE INDEX IF NOT EXISTS idx_trades_mode_id
  ON trades(mode, id);

-- Where an account-history wipe preserves what it erased (issue #1253: a
-- reset that leaves no trace of the rows it removed is an unarchived
-- erasure). A SEPARATE table rather than archived rows inside `trades`, and
-- seeded by copy-then-delete in the SAME transaction, so a crash cannot
-- leave a wipe without its archive or an archive without the wipe.
--
-- It carries the same payload columns as `trades` because an archived row is
-- the same fact, just retired; and `restored_at_ms = 0` marks "not restored",
-- which is the only state today. The archive is append-only: nothing in this
-- kernel deletes from it, so a mistaken wipe stays auditable.
CREATE TABLE IF NOT EXISTS trades_archive (
  mode            TEXT NOT NULL,
  account_id      TEXT NOT NULL,
  id              TEXT NOT NULL,
  exit_time       INTEGER NOT NULL,
  payload_json    TEXT NOT NULL,
  net_pnl_usd     TEXT NOT NULL,
  gross_pnl_usd   TEXT NOT NULL,
  fees_usd        TEXT NOT NULL,
  hold_time_sec   INTEGER NOT NULL,
  source          TEXT NOT NULL DEFAULT '',
  -- When the row first landed (copied from trades.written_at_ms).
  written_at_ms   INTEGER NOT NULL,
  -- When the wipe that retired it committed.
  archived_at_ms  INTEGER NOT NULL,
  -- Who/what caused the wipe (the caller's own label), for the audit trail.
  archive_reason  TEXT NOT NULL DEFAULT '',
  -- Reserved: non-zero once a restore path exists. Nothing sets it yet, so an
  -- operator can tell "never restored" from "restored" without guessing.
  restored_at_ms  INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (mode, account_id, id, exit_time, archived_at_ms)
);

-- An operator asking "what did the 2026-11-26 wipe remove from account X"
-- reads newest-archive-first, so the index leads with the account.
CREATE INDEX IF NOT EXISTS idx_trades_archive_account_time
  ON trades_archive(mode, account_id, archived_at_ms);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// The DDL must not declare a floating-point column anywhere: every amount
    /// in the kernel is a decimal string, and a REAL column would invite an
    /// implicit float round-trip.
    #[test]
    fn schema_declares_no_real_columns() {
        // Strip `--` line comments first: the prose explains WHY floats are
        // banned, and the words REAL/FLOAT appear in that prose.
        let ddl: String = SCHEMA_V1
            .lines()
            .map(|line| line.split("--").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
            .to_uppercase();
        for forbidden in ["REAL", "FLOAT", "DOUBLE", "NUMERIC"] {
            assert!(
                !ddl.contains(forbidden),
                "schema v1 must not declare {forbidden} columns"
            );
        }
    }

    /// The version constant and the migration list must agree.
    #[test]
    fn version_matches_migration_list() {
        assert_eq!(
            crate::db::migrations::MIGRATIONS
                .iter()
                .map(|m| m.version)
                .max(),
            Some(SCHEMA_VERSION)
        );
    }

    /// v2 (#1199 T-02) drops exactly the five zero-runtime-write tables and
    /// their five indexes — no more, no less. A typo in the DROP list would
    /// either leave a lying table behind or take a live one down.
    ///
    /// v4 re-creates ONE of them (`trades`) with a DIFFERENT shape. That is not
    /// a hole in this assertion: the historical fact is "v2's SQL drops these
    /// five", and the current-state fact is checked right below it. Keeping
    /// both means neither can silently regress — a v2 edit that stopped
    /// dropping a table would still red here, and a v4 that forgot to re-create
    /// `trades` would red on the inventory assertion.
    #[test]
    fn v2_drops_exactly_the_five_ledger_tables() {
        const DROPPED_TABLES: &[&str] =
            &["orders", "fills", "positions", "trades", "ledger_entries"];
        const DROPPED_INDEXES: &[&str] = &[
            "idx_orders_account",
            "idx_fills_account",
            "idx_positions_account",
            "idx_trades_account",
            "idx_ledger_account",
        ];
        for table in DROPPED_TABLES {
            assert!(
                SCHEMA_V2.contains(&format!("DROP TABLE IF EXISTS {table};")),
                "SCHEMA_V2 must drop table {table}"
            );
        }
        for index in DROPPED_INDEXES {
            assert!(
                SCHEMA_V2.contains(&format!("DROP INDEX IF EXISTS {index};")),
                "SCHEMA_V2 must drop index {index}"
            );
            assert!(
                !INDEXES.contains(index),
                "{index} is dropped by v2 — it must not be in the INDEXES inventory"
            );
        }
        // The v1 DDL itself is never edited (append-only): the tables it
        // creates must still be declared there, v2 is what removes them.
        for table in DROPPED_TABLES {
            assert!(
                SCHEMA_V1.contains(&format!("CREATE TABLE IF NOT EXISTS {table} (")),
                "SCHEMA_V1 keeps its historical DDL: {table} must still be created there"
            );
        }
    }

    /// v4 re-creates `trades` and NOTHING else from v2's drop list, with a
    /// strictly stronger address than the v1 table had.
    ///
    /// Why this assertion exists next to the one above: the pair together pin
    /// the whole decision — "four stay dropped, one comes back, and it comes
    /// back mode-scoped". A future edit that also resurrected `orders` (or
    /// dropped `mode` from the key, silently merging dry and live books) reds
    /// here.
    #[test]
    fn v4_recreates_only_trades_and_mode_is_part_of_its_identity() {
        for dropped in ["orders", "fills", "positions", "ledger_entries"] {
            assert!(
                !TABLES.contains(&dropped),
                "{dropped} stays dropped: v2's reasoning (no runtime consumer) still holds for it"
            );
        }
        assert!(
            TABLES.contains(&"trades"),
            "v4 re-creates `trades` — the closed-trade log is the store seam's trades domain"
        );
        assert!(
            SCHEMA_V4.contains("CREATE TABLE IF NOT EXISTS trades ("),
            "the table must be created by SCHEMA_V4 (SCHEMA_V1's shape is historical)"
        );
        // The identity must be mode-scoped. One database file serves dry and
        // live; the JSONL era got its separation from two filenames, and a
        // table without `mode` in the key would merge the two books.
        assert!(
            SCHEMA_V4.contains("PRIMARY KEY (mode, account_id, id, exit_time)"),
            "a trades row is addressed by (mode, account, id, exit_time): \
             `mode` keeps dry and live books apart, and `net_pnl_usd` must NOT \
             be in the key (amend_trade_net_pnl mutates it)"
        );
        assert!(
            !SCHEMA_V4.contains("PRIMARY KEY (id)"),
            "the v1 single-column key must not come back: it cannot represent \
             two modes or a reissued id"
        );
        // Money is a decimal string in EVERY version, never REAL.
        let ddl: String = SCHEMA_V4
            .lines()
            .map(|line| line.split("--").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
            .to_uppercase();
        for forbidden in ["REAL", "FLOAT", "DOUBLE", "NUMERIC"] {
            assert!(
                !ddl.contains(forbidden),
                "schema v4 must not declare {forbidden} columns: money is a decimal string"
            );
        }
    }
}
