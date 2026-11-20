//! The SQLite-backed store (issue #1199 PR-2): the DB backend behind
//! [`StoreApi`](super::StoreApi).
//!
//! One connection: this store owns the same [`AccountRiskDb`] handle the
//! risk/exit DB-first branch uses — one file, one connection, the `db`
//! module's single-writer discipline (every statement serialised through the
//! connection's mutex). Opening the store opens (creating/migrating if
//! needed) the database; a failure is the caller's fail-closed boot refusal.
//!
//! Domain coverage (the DB-carried domains vs the pending ones):
//!
//!   * **risk / exit** — the DB-carried pair. The boot overlay
//!     (`Core::open_account_risk_db`) reads their rows onto the working
//!     copies, so a write here IS the restart-durable value: the merge
//!     (fields onto the stored row) mirrors the write path's in-memory merge.
//!     The production write port (`risk.setSystemic` / `risk.setExit`) keeps
//!     the whole-row `AccountRiskDb::save_risk` / `save_exit` write; SQLite
//!     is the default assembly's durable backend — the ExitConfig carries
//!     calibration fields the wire audit never names, so the full-row write
//!     is the exact pre-seam semantics (parity-proven against the raw pair:
//!     the `account_risk` six-field row, the `account_settings`
//!     order-notional key, `source = 'panel'`, and the exit `policy_json`).
//!   * **settings family** (execution-policy sections, per-account round
//!     duration, the account-book knobs, the arb legs) — the rows land in
//!     the tables the importer fills (`account_settings`, `accounts`,
//!     `strategy_assignments`, `arb_pairs`, `kernel_meta`), which is the
//!     right durable shape.
//!   * **credentials** — per-account credential ENV VAR NAMES plus the
//!     AES-256-GCM sealed payload, durably landed in `kernel_meta`
//!     (`credential.<VAR>`); the boot injects the plaintext into the process
//!     env only (never to disk). Values never land in plaintext anywhere.
//!
//! Install state (issue #1199 r2, the honest ledger): this backend has passed
//! the two-backend parity proof
//! (`tests/store_backend_parity.rs`: one business path — account
//! create/venue/rounds/status/trading/strategies/remove, the settings cell,
//! the risk/exit round-trip — asserted fact-for-fact identical across
//! MemoryStore / SqliteStore, with the legitimate differences
//! enumerated, not silenced).
//!
//! Fail-closed posture (issue #1199 acceptance): the store exists only with
//! a live database — construction IS the open, and an open failure is an
//! error. There is no store-without-a-database state, so no write can
//! silently fall back to a second carrier from inside this backend.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rust_decimal::Decimal;

use crate::account::config::CrossVenueArbConfig;
use crate::account::{AccountStatus, CredentialKeys};
use crate::db::{META_RUNTIME_CONFIG_JSON, account_setting_remove, account_setting_set};
use crate::model::{CoreError, CoreErrorCode};
use crate::position::OpenPosition;
use crate::risk::account_store::AccountRiskDb;
use crate::risk::limits::{AccountRiskLimits, Bound, LimitSource};
use crate::store::{AccountScope, FieldChanges, MoneyFact, WriteOutcome};

/// The `account_settings` key carrying the per-account round-length grid
/// (the account-level form of the env's `ACCOUNT_<ID>_ROUND_DURATION_SEC`;
/// the table is already account-keyed, so the key carries no id).
pub const ROUND_DURATION_SETTING: &str = "round.duration_sec";

/// The reserved `defaults` account selector (env_state.rs): its row exists
/// only so the globals `POLICY_*` rows satisfy the `account_settings` FK —
/// scaffolding, never an account. Marked with [`DEFAULTS_STATUS_REASON`] so
/// no reader mistakes the empty venue for an unbound account (#1253).
pub const DEFAULTS_ACCOUNT_ID: &str = "defaults";
/// `status_reason` written on the `defaults` row (and healed on pre-existing
/// rows) in [`SqliteStore::ensure_account`] — the FK scaffold's own label,
/// store-layer internal convention.
pub const DEFAULTS_STATUS_REASON: &str =
    "reserved-policy-selector (FK scaffold for the globals policy section)";

/// The SQLite-backed [`StoreApi`](super::StoreApi) implementation.
///
/// See the module doc for the domain coverage and the install gate.
#[derive(Clone)]
pub struct SqliteStore {
    risk: AccountRiskDb,
    /// The AES-256-GCM manager built from `BLITZKRIEG_DB_KEY` at open.
    /// `None` = the key is missing/malformed on this boot: the credential
    /// write face refuses (the honest error — the operator must set the key),
    /// and the boot injection keeps the env spelling (see
    /// `credentials::inject_credentials_into_process_env`).
    ///
    /// Debug deliberately does NOT print the manager (it holds the master key
    /// material); only its presence is observable.
    credentials: Option<std::sync::Arc<crate::db::credentials::CredentialsManager>>,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("risk", &self.risk)
            .field("credentials", &self.credentials.is_some())
            .finish()
    }
}

impl SqliteStore {
    /// Open (creating/migrating if needed) the store at `path`.
    ///
    /// The open is the only constructor: a failure is an error (fail-closed
    /// — the caller refuses to boot rather than run without a store).
    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        // The credential master key is read ONCE, here (the same posture as
        // `CredentialsManager::new`: missing/malformed refuses the credential
        // domain but never the whole store — the rest of the rows are plain).
        let credentials = match std::env::var(crate::db::credentials::MASTER_KEY_ENV) {
            Ok(key) if !key.trim().is_empty() => {
                match crate::db::credentials::CredentialsManager::new(&key) {
                    Ok(m) => Some(std::sync::Arc::new(m)),
                    Err(e) => {
                        tracing::warn!(
                            error = %e.message,
                            "credential encryption unavailable — market.credentials.set \
                             will refuse until the master key is fixed"
                        );
                        None
                    }
                }
            }
            _ => {
                tracing::warn!(
                    "BLITZKRIEG_DB_KEY is not set — the credential store is unavailable; \
                     UI-submitted credentials refuse and boot injection falls back to env"
                );
                None
            }
        };
        let store = Self {
            risk: AccountRiskDb::open(path)?,
            credentials,
        };
        // #1278: 资金事实的追加-only 日志，一行一事实，永不更新、永不删除，
        // 以便重启后审计四个资金写面到底落了哪些行。与 accounts/account_risk
        // 同库同连接，同一套单写入口；IF NOT EXISTS 对已有库是幂等的，
        // 不走 migrations 表（纯加表，不改既有版本）。
        store
            .risk
            .db()
            .with_connection(|conn| -> anyhow::Result<()> {
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS money_facts (
                        id          INTEGER PRIMARY KEY AUTOINCREMENT,
                        kind        TEXT NOT NULL,
                        account_id  TEXT,
                        at_ms       INTEGER NOT NULL,
                        payload     TEXT NOT NULL,
                        created_at_ms INTEGER NOT NULL
                    )",
                )?;
                Ok(())
            })
            .map_err(|e| anyhow::anyhow!("money_facts table init failed: {e}"))?;
        Ok(store)
    }

    /// The risk/exit store behind this backend (the same connection — the
    /// boot overlay and the DB-first write branch use it without opening a
    /// second handle on the file).
    pub fn risk_db(&self) -> &AccountRiskDb {
        &self.risk
    }

    /// The path this store's database was opened from.
    pub fn path(&self) -> &std::path::Path {
        self.risk.path()
    }
}

impl SqliteStore {
    /// Merge wire field changes onto an existing risk row (the stored row,
    /// or the factory posture when the account has none) — the DB-side twin
    /// of the service layer's in-memory merge (`install_account_risk_row`).
    /// An unknown field is skipped (a future wire field without a DB column
    /// must not crash the write — the pre-seam rule, kept).
    /// Read-modify-write base row for a risk patch. A store read failure is
    /// propagated (fail-closed): merging onto a silently-empty base could wipe
    /// a stored row the operator can't see.
    fn merge_risk_fields(
        &self,
        account_id: &str,
        fields: &FieldChanges,
    ) -> Result<AccountRiskLimits, String> {
        let mut row = self
            .risk
            .load_risk()
            .map_err(|e| format!("account_risk read failed: {e}"))?
            .get(account_id)
            .cloned()
            .unwrap_or_default();
        for (field, value) in fields {
            let Ok(v) = value.trim().parse::<Decimal>() else {
                tracing::warn!(
                    account = %account_id,
                    field = %field,
                    raw = %value,
                    "risk bound value unparseable — field stays"
                );
                continue;
            };
            let bound = Bound::new(v, LimitSource::Flag);
            match field.as_str() {
                "maxSingleLossUsd" => row.max_single_loss_usd = bound,
                "maxDailyDrawdownUsd" => row.max_daily_drawdown_usd = bound,
                "maxPositionSize" => row.max_position_size = bound,
                "maxConsecutiveLosses" => row.max_consecutive_losses = bound,
                "cooldownMinutes" => row.cooldown_minutes = bound,
                "maxOrderNotionalUsd" => row.max_order_notional_usd = bound,
                other => {
                    tracing::warn!(
                        account = %account_id,
                        field = %other,
                        "unknown risk field — skipped (no DB column)"
                    );
                }
            }
        }
        Ok(row)
    }

    /// Merge wire field changes onto an existing exit-policy row (the stored
    /// JSON row, or the factory posture when the account has none) — the
    /// DB-side twin of `install_account_exit_row`'s merge. A store read
    /// failure is propagated (fail-closed): merging onto a silently-empty
    /// base could wipe a stored row the operator can't see (mirrors
    /// `merge_risk_fields`, issue #1199 fourth audit P0).
    fn merge_exit_fields(
        &self,
        account_id: &str,
        fields: &FieldChanges,
    ) -> Result<crate::exit_policy::ExitConfig, String> {
        let mut exit = self
            .risk
            .load_exits()
            .map_err(|e| format!("account_exit_policy read failed: {e}"))?
            .get(account_id)
            .cloned()
            .unwrap_or_default();
        for (field, value) in fields {
            match field.as_str() {
                "stopLossPct" => {
                    if let Ok(v) = value.trim().parse::<Decimal>() {
                        exit.stop_loss_pct = v;
                    }
                }
                "takeProfitPct" => {
                    if let Ok(v) = value.trim().parse::<Decimal>() {
                        exit.take_profit_pct = v;
                    }
                }
                "forceExitSec" => {
                    if let Ok(v) = value.trim().parse::<i64>() {
                        exit.force_exit_sec = v;
                    }
                }
                "trailingEnabled" => {
                    if let Ok(v) = value.trim().parse::<bool>() {
                        exit.trailing_enabled = v;
                    }
                }
                "strategyExitMakerFirst" => {
                    if let Ok(v) = value.trim().parse::<bool>() {
                        exit.strategy_exit_maker_first = v;
                    }
                }
                other => {
                    tracing::warn!(
                        account = %account_id,
                        field = %other,
                        "unknown exit field — skipped (no DB column)"
                    );
                }
            }
        }
        Ok(exit)
    }

    /// The settings family's write tail shared by the policy-section and
    /// round-duration methods: each `(key, value)` lands as an
    /// `account_settings` row (a `None` value is the sweep — key gone =
    /// inherit the globals, the env grammar's spelling).
    fn write_settings(
        &self,
        account_id: &str,
        rows: &[(&str, Option<&str>)],
    ) -> Result<(), CoreError> {
        self.risk
            .db()
            .with_connection(|conn| -> anyhow::Result<()> {
                for (key, value) in rows {
                    match value {
                        Some(value) => account_setting_set(conn, account_id, key, value)?,
                        None => account_setting_remove(conn, account_id, key)?,
                    }
                }
                Ok(())
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account settings write failed: {e}"),
                )
            })
    }

    /// Ensure the account row exists (the FK precondition the settings and
    /// account-book writes share; INSERT OR IGNORE — a real row is never
    /// overwritten).
    fn ensure_account(&self, account_id: &str, name: Option<&str>) -> Result<(), CoreError> {
        // `defaults` is the reserved policy-section selector (env_state.rs):
        // its row exists only so the globals POLICY_* rows satisfy the
        // account_settings FK — scaffolding, never an account. It is marked
        // here (and healed on pre-existing rows) so no reader mistakes the
        // empty venue for an unbound account (#1253); boot's
        // ledger_book_from_rows already refuses to materialize it.
        let reserved = account_id.eq_ignore_ascii_case(DEFAULTS_ACCOUNT_ID);
        let marker = DEFAULTS_STATUS_REASON;
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "INSERT OR IGNORE INTO accounts
                            (id, name, market_type, status, status_reason, trading_enabled, updated_at_ms)
                         VALUES (?1, ?2, 'prediction', 'active', ?3, 1, ?4)",
                    rusqlite::params![
                        account_id,
                        name.unwrap_or(account_id),
                        reserved.then_some(marker),
                        crate::db::now_ms(),
                    ],
                )?;
                if reserved {
                    conn.execute(
                        "UPDATE accounts SET status_reason = ?2 WHERE id = ?1 AND status_reason IS NULL",
                        rusqlite::params![account_id, marker],
                    )?;
                }
                Ok::<(), rusqlite::Error>(())
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account ensure failed: {e}"),
                )
            })
    }
}

impl super::StoreApi for SqliteStore {
    fn account_writes_enabled(&self) -> bool {
        // The DB is always live here (the open is the constructor); account
        // writes are accepted.
        true
    }

    fn save_account_venue(&self, account_id: &str, venue: &str) -> Result<(), CoreError> {
        self.ensure_account(account_id, None)?;
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE accounts SET venue = ?2, updated_at_ms = ?3 WHERE id = ?1",
                    rusqlite::params![account_id, venue, crate::db::now_ms()],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account venue write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_account_create(
        &self,
        account_id: &str,
        name: Option<&str>,
        venue: Option<&str>,
        credential_keys: &CredentialKeys,
    ) -> Result<(), CoreError> {
        self.ensure_account(account_id, name)?;
        let venue = venue.unwrap_or("");
        if !venue.is_empty() {
            self.save_account_venue(account_id, venue)?;
        }
        // The credential ENV VAR NAMES ride the account row's credential
        // table (values belong to the credentials domain, never here).
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO account_credentials
                            (account_id, api_key_env, secret_env, passphrase_env,
                             private_key_env, encryption_key_env, updated_at_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                         ON CONFLICT(account_id) DO UPDATE SET
                            api_key_env = excluded.api_key_env,
                            secret_env = excluded.secret_env,
                            passphrase_env = excluded.passphrase_env,
                            private_key_env = excluded.private_key_env,
                            encryption_key_env = excluded.encryption_key_env,
                            updated_at_ms = excluded.updated_at_ms",
                    rusqlite::params![
                        account_id,
                        credential_keys.api_key_env,
                        credential_keys.secret_env,
                        credential_keys.passphrase_env,
                        credential_keys.private_key_env,
                        Option::<&str>::None,
                        crate::db::now_ms(),
                    ],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account credentials names write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn remove_account(&self, account_id: &str) -> Result<(), CoreError> {
        // The FK cascades sweep every child row (risk / exit / settings /
        // strategies / credentials) in the same statement.
        let removed = self
            .risk
            .db()
            .with_connection(|conn| {
                conn.execute("DELETE FROM accounts WHERE id = ?1", [account_id])
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account remove failed: {e}"),
                )
            })?;
        if removed == 0 {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("the store names no account `{account_id}`"),
            ));
        }
        // 写删除墓碑：boot 时 .env 补账不得把已删除的账户复活。
        // 墓碑与 accounts 表在同一 SQLite 文件，单写入口，不跨库。
        self.risk
            .save_kernel_meta(
                &format!("removed_account.{}", account_id),
                &crate::db::now_ms().to_string(),
            )
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account remove tombstone write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_account_trading(&self, account_id: &str, enabled: bool) -> Result<(), CoreError> {
        self.ensure_account(account_id, None)?;
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE accounts SET trading_enabled = ?2, updated_at_ms = ?3 WHERE id = ?1",
                    rusqlite::params![account_id, enabled as i64, crate::db::now_ms()],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account trading write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_account_status(
        &self,
        account_id: &str,
        status: &AccountStatus,
    ) -> Result<(), CoreError> {
        self.ensure_account(account_id, None)?;
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE accounts SET status = ?2, updated_at_ms = ?3 WHERE id = ?1",
                    rusqlite::params![account_id, status.to_string(), crate::db::now_ms()],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account status write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_account_active(&self, account_id: &str) -> Result<(), CoreError> {
        // ONE write: Active status AND trading-on are both carried by the
        // same row (no half-state is expressible — the pre-seam two-removal
        // window cannot exist here at all).
        self.ensure_account(account_id, None)?;
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE accounts SET status = 'active', trading_enabled = 1,
                            updated_at_ms = ?2
                     WHERE id = ?1",
                    rusqlite::params![account_id, crate::db::now_ms()],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account unfreeze write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_display_account(&self, account_id: &str) -> Result<(), CoreError> {
        // The display pointer is pure presentation — a kernel_meta cell, not
        // an accounting axis (the same split the env grammar spells with
        // ACCOUNT_DISPLAY).
        self.risk
            .db()
            .with_connection(|conn| {
                crate::db::kernel_meta_set(conn, crate::db::META_DISPLAY_ACCOUNT, account_id)
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("display account write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_account_strategies(
        &self,
        account_id: &str,
        strategies: &[String],
    ) -> Result<(), CoreError> {
        self.ensure_account(account_id, None)?;
        self.risk
            .db()
            .with_connection(|conn| {
                // Sweep-then-write: the enablement set is the row set; an
                // empty list clears every assignment (absence = no
                // restriction, the env grammar's spelling).
                conn.execute(
                    "DELETE FROM strategy_assignments WHERE account_id = ?1",
                    [account_id],
                )?;
                for strategy in strategies {
                    conn.execute(
                        "INSERT INTO strategy_assignments
                                (account_id, strategy, enabled, updated_at_ms)
                             VALUES (?1, ?2, 1, ?3)
                             ON CONFLICT(account_id, strategy) DO UPDATE SET
                                enabled = excluded.enabled,
                                updated_at_ms = excluded.updated_at_ms",
                        rusqlite::params![account_id, strategy, crate::db::now_ms()],
                    )?;
                }
                Ok::<(), rusqlite::Error>(())
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account strategies write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_account_rounds(&self, account_id: &str, rounds: &[i64]) -> Result<(), CoreError> {
        self.ensure_account(account_id, None)?;
        let rounds_json = serde_json::to_string(rounds).unwrap_or_else(|_| "[]".to_string());
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE accounts SET rounds_json = ?2, updated_at_ms = ?3 WHERE id = ?1",
                    rusqlite::params![account_id, rounds_json, crate::db::now_ms()],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account rounds write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_account_opening_equity(
        &self,
        account_id: &str,
        equity: Decimal,
        at_ms: i64,
    ) -> Result<(), CoreError> {
        self.ensure_account(account_id, None)?;
        // Write-once: an already-recorded principal is authoritative and is
        // never revised by a later venue report (the env face's rule, kept —
        // the ledger's cash identity depends on it).
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE accounts SET opening_equity = ?2, updated_at_ms = ?3
                     WHERE id = ?1 AND opening_equity IS NULL",
                    rusqlite::params![account_id, equity.to_string(), at_ms],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account opening equity write failed: {e}"),
                )
            })?;
        Ok(())
    }

    /// #77: mirror the open-position book into SQLite — one `positions` row
    /// per open position, upserted by id, all in one transaction. The table is
    /// (re)ensured idempotently on every call (migration v2 dropped the
    /// runtime tables from the baseline schema, so `CREATE TABLE IF NOT
    /// EXISTS` here is the only guarantee its container exists). The byte-
    /// identical JSONL write in [`crate::service::persistence::persist_positions`]
    /// stays; this write is the additional StoreApi landing. A failure comes
    /// back as `Err` at the call site, which logs it and keeps the round
    /// running — the in-memory book stays the source of truth either way.
    fn save_positions(&self, positions: &[OpenPosition]) -> Result<bool, CoreError> {
        let payloads: Vec<(String, String, String)> = positions
            .iter()
            .map(|p| {
                serde_json::to_string(p)
                    .map(|payload| (p.id.clone(), p.account_id.as_str().to_owned(), payload))
            })
            .collect::<Result<Vec<_>, serde_json::Error>>()
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("save_positions: serialize failed ({e})"),
                )
            })?;
        let mut conn = self.risk.db().lock();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS positions (
                id           TEXT PRIMARY KEY,
                account_id   TEXT NOT NULL,
                payload      TEXT NOT NULL,
                updated_at_ms INTEGER NOT NULL
            )",
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!("save_positions: table ensure failed ({e})"),
            )
        })?;
        let tx = conn.transaction().map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!("save_positions: transaction begin failed ({e})"),
            )
        })?;
        for (id, account_id, payload) in &payloads {
            tx.execute(
                "INSERT OR REPLACE INTO positions
                        (id, account_id, payload, updated_at_ms)
                     VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, account_id, payload, crate::db::now_ms()],
            )
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("save_positions: upsert failed ({e})"),
                )
            })?;
        }
        tx.commit().map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!("save_positions: commit failed ({e})"),
            )
        })?;
        Ok(true)
    }

    fn save_risk_bounds(
        &self,
        account_id: Option<&str>,
        fields: &FieldChanges,
    ) -> Result<bool, CoreError> {
        let target = account_id
            .map(str::to_string)
            .unwrap_or_else(|| crate::model::default_account_id().as_str().to_string());
        let row = self.merge_risk_fields(&target, fields).map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!("the account_risk store read failed ({e})"),
            )
        })?;
        self.risk.save_risk(&target, &row).map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!("the account_risk store write failed ({e})"),
            )
        })?;
        Ok(true)
    }

    fn save_exit(
        &self,
        account_id: Option<&str>,
        fields: &FieldChanges,
    ) -> Result<bool, CoreError> {
        let target = account_id
            .map(str::to_string)
            .unwrap_or_else(|| crate::model::default_account_id().as_str().to_string());
        let exit = self.merge_exit_fields(&target, fields).map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!("the account_exit_policy store read failed ({e})"),
            )
        })?;
        self.risk.save_exit(&target, &exit).map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!("the account_exit_policy store write failed ({e})"),
            )
        })?;
        Ok(true)
    }

    fn save_arb_config(&self, cfg: &CrossVenueArbConfig) -> Result<(), CoreError> {
        let pairs_json = serde_json::to_string(&cfg.pairs).unwrap_or_else(|_| "[]".to_string());
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO arb_pairs
                            (id, leg_a_venue, leg_a_account, leg_b_venue, leg_b_account,
                             pairs_json, updated_at_ms)
                     VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(id) DO UPDATE SET
                        leg_a_venue = excluded.leg_a_venue,
                        leg_a_account = excluded.leg_a_account,
                        leg_b_venue = excluded.leg_b_venue,
                        leg_b_account = excluded.leg_b_account,
                        pairs_json = excluded.pairs_json,
                        updated_at_ms = excluded.updated_at_ms",
                    rusqlite::params![
                        cfg.leg_a_venue,
                        cfg.leg_a_account,
                        cfg.leg_b_venue,
                        cfg.leg_b_account,
                        pairs_json,
                        crate::db::now_ms(),
                    ],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("arb config write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn clear_arb_config(&self) -> Result<(), CoreError> {
        self.risk
            .db()
            .with_connection(|conn| conn.execute("DELETE FROM arb_pairs WHERE id = 1", []))
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("arb config clear failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn load_arb_config(&self) -> Result<Option<crate::account::CrossVenueArbConfig>, CoreError> {
        self.risk
            .db()
            .with_connection(|conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT leg_a_venue, leg_a_account, leg_b_venue, leg_b_account, pairs_json \
                         FROM arb_pairs WHERE id = 1",
                    )
                    .map_err(|e| CoreError::new(CoreErrorCode::Internal, format!("{e}")))?;
                let row = stmt.query_row([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                });
                match row {
                    Ok((leg_a_venue, leg_a_account, leg_b_venue, leg_b_account, pairs_json)) => {
                        // NULL pairs_json is the legal "never configured / cleared"
                        // state (save_arb_config always serializes a parseable
                        // payload). A non-NULL payload that fails to parse is row
                        // corruption: it still reads as an empty pairs list — a
                        // damaged row must not panic the boot — but it must be
                        // loud, or corruption silently reads as "unconfigured"
                        // (issue #1199 P2-11).
                        let pairs: Vec<String> = match pairs_json.as_deref() {
                            None => Vec::new(),
                            Some(raw) => match serde_json::from_str(raw) {
                                Ok(pairs) => pairs,
                                Err(e) => {
                                    tracing::warn!(
                                        "arb_pairs row id=1 holds malformed pairs_json — the \
                                         pairs list is ignored (row corruption, NOT the legal \
                                         unconfigured NULL): {e}"
                                    );
                                    Vec::new()
                                }
                            },
                        };
                        Ok(Some(crate::account::CrossVenueArbConfig {
                            leg_a_venue,
                            leg_a_account,
                            leg_b_venue,
                            leg_b_account,
                            pairs,
                        }))
                    }
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(CoreError::new(
                        CoreErrorCode::Internal,
                        format!("arb config read failed: {e}"),
                    )),
                }
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("arb config read failed: {e}"),
                )
            })
    }

    fn save_policy_section(
        &self,
        account_id: &str,
        state: &BTreeMap<String, Option<String>>,
    ) -> Result<(), String> {
        // The section's whole footprint, spelled by the env grammar the
        // importer and the policy loader share (the key set is the contract;
        // a `None` value is the sweep — key gone = inherit the globals).
        // 操作者裁决 2026-11-14：reset（footprint 全 None）继承原 EnvStore 的
        // 段存在性探针——`account_settings` 里没有任何 POLICY_ 行 = 没有可
        // 撤销的段，按名如实拒绝（未知 id 不许静默 ok，也不许 ensure 出一行）。
        let is_reset = state.values().all(|v| v.is_none());
        if is_reset {
            let has_section = self
                .risk
                .load_settings()
                .map_err(|e| format!("policy section probe failed: {e}"))?
                .get(account_id)
                .map(|keys| keys.keys().any(|k| k.starts_with("POLICY_")))
                .unwrap_or(false);
            if !has_section {
                return Err(format!(
                    "execution policy reset: no policy section exists for `{account_id}`"
                ));
            }
        } else {
            self.ensure_account(account_id, None)
                .map_err(|e| e.message)?;
        }
        let rows: Vec<(&str, Option<&str>)> = state
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_deref()))
            .collect();
        self.write_settings(account_id, &rows)
            .map_err(|e| e.message)
    }

    fn save_account_round_duration(
        &self,
        account_id: &str,
        seconds: Option<i64>,
    ) -> Result<(), CoreError> {
        self.ensure_account(account_id, None)?;
        match seconds {
            Some(sec) => self.write_settings(
                account_id,
                &[(ROUND_DURATION_SETTING, Some(sec.to_string().as_str()))],
            ),
            None => self.write_settings(account_id, &[(ROUND_DURATION_SETTING, None)]),
        }
    }

    /// [`StoreApi::put_setting`]: `Some(value)` upserts into `account_settings`;
    /// `None` sweeps the key (key gone → inherit the globals). The account
    /// arrives typed via `AccountScope` — never written onto a guessed id
    /// (#1221/#1223 bug class). A backend failure is carried verbatim, never
    /// silently degraded.
    fn put_setting(
        &self,
        scope: AccountScope<'_>,
        key: &str,
        value: Option<&str>,
    ) -> Result<WriteOutcome, CoreError> {
        self.ensure_account(scope.account.as_str(), None)?;
        self.write_settings(scope.account.as_str(), &[(key, value)])?;
        Ok(WriteOutcome::Persisted)
    }

    fn save_credentials(
        &self,
        vars: &BTreeMap<String, String>,
        reset_keys: &[&str],
    ) -> Result<(), CoreError> {
        // 操作者裁决 2026-11-14（#1199 第六轮）：凭证的家是数据库——每个值
        // AES-256-GCM 封装后落 `kernel_meta.credential.<VAR>`（nonce ||
        // ciphertext || tag，base64）。`.env` 对内核只读；明文值除了加密
        // 行之外不落任何盘上位置。reset 分支按 plugin 声明表删行。
        let manager = self.credentials.as_ref().ok_or_else(|| {
            CoreError::new(
                CoreErrorCode::NotAuthenticated,
                "market.credentials.set: no credential master key — set BLITZKRIEG_DB_KEY \
                 (base64 of 32 random bytes) and restart; nothing was written",
            )
        })?;
        for (name, value) in vars {
            let blob = manager.encrypt(value.as_bytes()).map_err(|e| {
                CoreError::new(
                    CoreErrorCode::NotAuthenticated,
                    format!("market.credentials.set: encrypt failed ({})", e.message),
                )
            })?;
            let b64 = crate::db::credentials::encode_blob(&blob);
            self.risk
                .save_kernel_meta(
                    &format!(
                        "{}{name}",
                        crate::risk::account_store::CREDENTIAL_META_PREFIX
                    ),
                    &b64,
                )
                .map_err(|e| {
                    CoreError::new(
                        CoreErrorCode::Internal,
                        format!("market.credentials.set: SQLite write failed: {e}"),
                    )
                })?;
        }
        for name in reset_keys {
            self.risk
                .delete_kernel_meta(&format!(
                    "{}{name}",
                    crate::risk::account_store::CREDENTIAL_META_PREFIX
                ))
                .map_err(|e| {
                    CoreError::new(
                        CoreErrorCode::Internal,
                        format!("market.credentials.set: SQLite delete failed: {e}"),
                    )
                })?;
        }
        Ok(())
    }

    /// #1278: 把一条资金事实落入 `money_facts`（追加-only，幂等于 INSERT
    /// —— 同一事实重复写就是两行，由调用方保证幂等键，本层只管耐久。
    /// `Ok(WriteOutcome::Persisted)` 只在 INSERT 真落盘后返回；序列化或
    /// SQLite 失败返回 `Err`，由四个资金写面如实记日志、交易继续。
    fn append_money_fact(&self, fact: &MoneyFact<'_>) -> Result<WriteOutcome, CoreError> {
        let (kind, account_id, at_ms, payload) = match fact {
            MoneyFact::Order(o) => (
                "order",
                o.account_id.as_str(),
                o.updated_at_ms,
                serde_json::to_string(o).map_err(|e| {
                    CoreError::new(
                        CoreErrorCode::Internal,
                        format!("money fact (order) serialize failed: {e}"),
                    )
                })?,
            ),
            MoneyFact::Fill(f) => (
                "fill",
                f.account_id.as_str(),
                f.exit_time,
                serde_json::to_string(f).map_err(|e| {
                    CoreError::new(
                        CoreErrorCode::Internal,
                        format!("money fact (fill) serialize failed: {e}"),
                    )
                })?,
            ),
            MoneyFact::Ledger(e) => (
                "ledger",
                "",
                e.at_ms,
                serde_json::to_string(e).map_err(|e| {
                    CoreError::new(
                        CoreErrorCode::Internal,
                        format!("money fact (ledger) serialize failed: {e}"),
                    )
                })?,
            ),
            MoneyFact::Settlement(s) => (
                "settlement",
                if s.account_id.is_empty() {
                    ""
                } else {
                    s.account_id.as_str()
                },
                s.applied_at_ms,
                serde_json::to_string(s).map_err(|e| {
                    CoreError::new(
                        CoreErrorCode::Internal,
                        format!("money fact (settlement) serialize failed: {e}"),
                    )
                })?,
            ),
        };
        self.risk
            .db()
            .with_connection(|conn| -> anyhow::Result<()> {
                conn.execute(
                    "INSERT INTO money_facts
                            (kind, account_id, at_ms, payload, created_at_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        kind,
                        if account_id.is_empty() {
                            None
                        } else {
                            Some(account_id)
                        },
                        at_ms,
                        payload,
                        crate::db::now_ms(),
                    ],
                )?;
                Ok(())
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("money fact append failed: {e}"),
                )
            })?;
        Ok(WriteOutcome::Persisted)
    }

    /// issue #47: config.json 直写替换为 SQLite persist 路径。market_plugin
    /// 单元以 JSON 字符串落 `kernel_meta.runtime_config_json`，与导入时
    /// `db_import.rs` 的 `runtime_config_json` 单元格同一格式；读者通过
    /// `load_market_plugin` 反序列化取回 `marketPlugin`。
    fn save_market_plugin(&self, plugin: Option<&str>) -> Result<(), CoreError> {
        let doc = serde_json::json!({
            "marketPlugin": plugin,
            "marketPluginActor": plugin.map(|_| "system"),
            "marketPluginAtMs": blitzkrieg_market_api::net::now_ms(),
        });
        let value = serde_json::to_string(&doc).map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!("market.plugin.set: serialize failed: {e}"),
            )
        })?;
        self.risk
            .save_kernel_meta(META_RUNTIME_CONFIG_JSON, &value)
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("market.plugin.set: SQLite write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn load_market_plugin(&self) -> Result<Option<String>, CoreError> {
        let value = self
            .risk
            .load_kernel_meta(META_RUNTIME_CONFIG_JSON)
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("market.plugin.get: SQLite read failed: {e}"),
                )
            })?;
        let Some(text) = value else {
            return Ok(None);
        };
        let doc: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!("market.plugin.get: parse failed: {e}"),
            )
        })?;
        let plugin = doc
            .get("marketPlugin")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_string());
        Ok(plugin)
    }
}

#[cfg(test)]
mod tests {
    use super::super::StoreApi;
    use super::*;
    use crate::risk::limits::LimitSource;
    use rust_decimal_macros::dec;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// A private on-disk store (the WAL/migration posture is part of the
    /// contract under test — a `:memory:` file would skip both).
    fn temp_store() -> (SqliteStore, std::path::PathBuf) {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("bk-sqlite-store-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SqliteStore::open(dir.join("store.sqlite")).expect("temp store opens");
        (store, dir)
    }

    fn fields(pairs: &[(&str, &str)]) -> FieldChanges {
        pairs
            .iter()
            .map(|(f, v)| (f.to_string(), v.to_string()))
            .collect()
    }

    /// #1253: the reserved `defaults` selector is FK scaffolding — its row
    /// carries the marker (fresh and healed), a real account's does not, and
    /// the ensure stays idempotent across repeated calls.
    #[test]
    fn defaults_row_is_marked_reserved_not_unbound() {
        let (store, dir) = temp_store();
        store.ensure_account("defaults", None).unwrap();
        store.ensure_account("defaults", None).unwrap();
        store.ensure_account("paper", None).unwrap();
        let reason_of = |store: &SqliteStore, id: &str| -> Option<String> {
            store
                .risk
                .db()
                .with_connection(|conn| {
                    conn.query_row(
                        "SELECT status_reason FROM accounts WHERE id = ?1",
                        rusqlite::params![id],
                        |r| r.get::<_, Option<String>>(0),
                    )
                })
                .unwrap()
        };
        assert!(
            reason_of(&store, "defaults").is_some(),
            "the reserved selector's row carries the FK-scaffold marker (#1253)"
        );
        assert_eq!(
            reason_of(&store, "paper"),
            None,
            "a real account is never marked"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The risk-bounds round-trip: a field set merges onto the stored row,
    /// a second set upserts (no duplicate row), and a sibling account never
    /// sees the other's row.
    #[test]
    fn risk_bounds_round_trip_and_isolate_siblings() {
        let (store, dir) = temp_store();
        assert!(
            store
                .save_risk_bounds(Some("paper"), &fields(&[("maxPositionSize", "5")]))
                .unwrap()
        );
        let rows = store.risk.load_risk().unwrap();
        assert_eq!(rows["paper"].max_position_size.value(), dec!(5));
        assert_eq!(
            rows["paper"].max_position_size.source(),
            LimitSource::Env,
            "load_risk spells a stored row's source as env (the read face's \
             provenance label — the row has no source column)"
        );
        // A partial second write merges onto the stored row (the first value
        // survives — the merge is read-modify-write, not replace).
        store
            .save_risk_bounds(Some("paper"), &fields(&[("cooldownMinutes", "30")]))
            .unwrap();
        let rows = store.risk.load_risk().unwrap();
        assert_eq!(rows["paper"].max_position_size.value(), dec!(5));
        assert_eq!(rows["paper"].cooldown_minutes.value(), dec!(30));
        // The default-account face (None) is its own row.
        store
            .save_risk_bounds(None, &fields(&[("maxSingleLossUsd", "25")]))
            .unwrap();
        let rows = store.risk.load_risk().unwrap();
        assert_eq!(
            rows["default"].max_single_loss_usd.value(),
            dec!(25),
            "absent account = the process default"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The exit round-trip: the five cockpit fields land in the stored JSON
    /// row, and a partial second write merges (the first values survive).
    #[test]
    fn exit_fields_round_trip_and_merge() {
        let (store, dir) = temp_store();
        store
            .save_exit(Some("paper"), &fields(&[("stopLossPct", "8")]))
            .unwrap();
        let rows = store.risk.load_exits().unwrap();
        assert_eq!(rows["paper"].stop_loss_pct, dec!(8));
        store
            .save_exit(
                Some("paper"),
                &fields(&[("takeProfitPct", "90"), ("trailingEnabled", "true")]),
            )
            .unwrap();
        let rows = store.risk.load_exits().unwrap();
        assert_eq!(rows["paper"].stop_loss_pct, dec!(8));
        assert_eq!(rows["paper"].take_profit_pct, dec!(90));
        assert!(rows["paper"].trailing_enabled);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The settings family: policy sections sweep-then-write (a reset field
    /// removes the row), the round duration upserts/clears, and both live in
    /// `account_settings` under the env spelling the importer uses.
    #[test]
    fn policy_section_and_round_duration_land_in_settings() {
        let (store, dir) = temp_store();
        let state = BTreeMap::from([
            (
                "POLICY_BUDGET_RATIO_PAPER".to_string(),
                Some("0.25".to_string()),
            ),
            ("POLICY_MIN_BUDGET_USD_PAPER".to_string(), None),
        ]);
        store.save_policy_section("paper", &state).unwrap();
        let budget = store
            .risk
            .db()
            .with_connection(|conn| {
                crate::db::account_setting_get(conn, "paper", "POLICY_BUDGET_RATIO_PAPER")
            })
            .unwrap();
        assert_eq!(budget.as_deref(), Some("0.25"));
        // A None value is a sweep: the key is gone (inherit the globals).
        let swept = store
            .risk
            .db()
            .with_connection(|conn| {
                crate::db::account_setting_get(conn, "paper", "POLICY_MIN_BUDGET_USD_PAPER")
            })
            .unwrap();
        assert_eq!(swept, None);

        store
            .save_account_round_duration("paper", Some(300))
            .unwrap();
        let dur = store
            .risk
            .db()
            .with_connection(|conn| {
                crate::db::account_setting_get(conn, "paper", ROUND_DURATION_SETTING)
            })
            .unwrap();
        assert_eq!(dur.as_deref(), Some("300"));
        store.save_account_round_duration("paper", None).unwrap();
        let dur = store
            .risk
            .db()
            .with_connection(|conn| {
                crate::db::account_setting_get(conn, "paper", ROUND_DURATION_SETTING)
            })
            .unwrap();
        assert_eq!(dur, None, "the reset removes the key");
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The account-book family: the row updates carry the env grammar's
    /// meaning (trading/status as columns, the strategies set as its row
    /// set, rounds as the JSON array, the opening equity write-once), and
    /// the remove sweep cascades.
    #[test]
    fn account_book_writes_round_trip_and_cascade() {
        let (store, dir) = temp_store();
        store.save_account_venue("paper", "kalshi").unwrap();
        store.save_account_trading("paper", false).unwrap();
        store
            .save_account_status("paper", &AccountStatus::Frozen)
            .unwrap();
        store
            .save_account_strategies("paper", &["spread_arb".to_string()])
            .unwrap();
        store.save_account_rounds("paper", &[900, 3600]).unwrap();
        store
            .save_account_opening_equity("paper", dec!(1234.5), 7)
            .unwrap();
        // Write-once: the second report is ignored.
        store
            .save_account_opening_equity("paper", dec!(999), 8)
            .unwrap();
        store.save_display_account("paper").unwrap();

        let (venue, trading, status, rounds_json, equity) =
            store.risk.db().with_connection(|conn| {
                let venue: String = conn
                    .query_row("SELECT venue FROM accounts WHERE id = 'paper'", [], |r| {
                        r.get(0)
                    })
                    .unwrap();
                let trading: i64 = conn
                    .query_row(
                        "SELECT trading_enabled FROM accounts WHERE id = 'paper'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                let status: String = conn
                    .query_row("SELECT status FROM accounts WHERE id = 'paper'", [], |r| {
                        r.get(0)
                    })
                    .unwrap();
                let rounds_json: String = conn
                    .query_row(
                        "SELECT rounds_json FROM accounts WHERE id = 'paper'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                let equity: Option<String> = conn
                    .query_row(
                        "SELECT opening_equity FROM accounts WHERE id = 'paper'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                (venue, trading, status, rounds_json, equity)
            });
        assert_eq!(venue, "kalshi");
        assert_eq!(trading, 0, "trading=false is a column value");
        assert_eq!(status, "frozen");
        assert_eq!(rounds_json, "[900,3600]");
        assert_eq!(
            equity.as_deref(),
            Some("1234.5"),
            "write-once kept the first"
        );

        let display: String = store
            .risk
            .db()
            .with_connection(|conn| {
                crate::db::kernel_meta_get(conn, crate::db::META_DISPLAY_ACCOUNT)
            })
            .unwrap()
            .unwrap();
        assert_eq!(display, "paper");

        let strategies: i64 = store
            .risk
            .db()
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM strategy_assignments WHERE account_id = 'paper'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(strategies, 1);

        // The remove sweep cascades (risk / exit / settings / strategies /
        // credentials all ride the FK).
        store.remove_account("paper").unwrap();
        let left: i64 = store
            .risk
            .db()
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM accounts WHERE id = 'paper'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(left, 0);
        // An unknown remove refuses by name.
        let err = store.remove_account("paper").unwrap_err();
        assert_eq!(err.code, CoreErrorCode::InvalidParams);
        assert!(err.message.contains("paper"), "{err}");
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// #77: save_positions mirrors the open-position book — one `positions`
    /// row per position, upserted by id (repeat saves stay one row), account
    /// ids isolated, payloads deserialize back to an equal OpenPosition, and
    /// an empty set still ensures the table without touching any row.
    #[test]
    fn save_positions_mirrors_the_open_book() {
        use crate::account::AccountId;
        use crate::exit_policy::ExitState;
        use crate::model::{OrderRole, SignalDirection};
        use crate::position::CashFlows;

        let (store, dir) = temp_store();
        let now = crate::db::now_ms();
        let make = |id: &str, account: &str| OpenPosition {
            id: id.to_string(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            token_id: "tok".into(),
            condition_id: "cond".into(),
            entry_price: dec!(100),
            current_price: dec!(101),
            prev_price: dec!(100),
            shares: dec!(10),
            cost_usd: dec!(1000),
            was_maker_entry: true,
            entry_fee_pct: dec!(5),
            entry_role: OrderRole::Pending,
            exit_role: OrderRole::Pending,
            direction: SignalDirection::Up,
            target_exit_price: None,
            entered_at_ms: now,
            expires_at_ms: now + 3_600_000,
            account_id: AccountId::from(account),
            venue: "kalshi".into(),
            partner_token_id: "partner".into(),
            last_book_ts: now,
            round_duration_sec: 0,
            round_slot: 0,
            state: ExitState::new(dec!(100), now),
            flows: CashFlows::default(),
        };
        let a = make("a1", "alice");
        let b = make("a2", "bob");
        assert!(store.save_positions(&[a.clone(), b.clone()]).unwrap());

        let rows: Vec<(String, String)> = store
            .risk
            .db()
            .with_connection(|conn| -> rusqlite::Result<Vec<(String, String)>> {
                let mut stmt = conn.prepare("SELECT id, account_id FROM positions ORDER BY id")?;
                let rows: Vec<(String, String)> = stmt
                    .query_map([], |r| -> rusqlite::Result<(String, String)> {
                        Ok((r.get(0)?, r.get(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("a1".to_string(), "alice".to_string()),
                ("a2".to_string(), "bob".to_string())
            ],
            "exactly one row per position, keyed by id"
        );

        // Idempotent re-save of a1 with a changed price: still one row per id,
        // the payload carries the new value.
        let mut a2 = a.clone();
        a2.current_price = dec!(150);
        assert!(store.save_positions(&[a2.clone()]).unwrap());
        let count: i64 = store
            .risk
            .db()
            .with_connection(|conn| {
                conn.query_row("SELECT COUNT(*) FROM positions", [], |r| r.get(0))
            })
            .unwrap();
        assert_eq!(count, 2, "upsert by id never duplicates");
        let payload: String = store
            .risk
            .db()
            .with_connection(|conn| {
                conn.query_row("SELECT payload FROM positions WHERE id = 'a1'", [], |r| {
                    r.get(0)
                })
            })
            .unwrap();
        let back: OpenPosition = serde_json::from_str(&payload).unwrap();
        assert_eq!(back.current_price, dec!(150));
        assert_eq!(back.account_id, AccountId::from("alice"));

        // An empty set: the table is ensured, no row is touched.
        assert!(store.save_positions(&[]).unwrap());
        let count: i64 = store
            .risk
            .db()
            .with_connection(|conn| {
                conn.query_row("SELECT COUNT(*) FROM positions", [], |r| r.get(0))
            })
            .unwrap();
        assert_eq!(count, 2, "empty save must not delete existing rows");

        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The arb legs: the single-row upsert and the clear.
    #[test]
    fn arb_config_upserts_and_clears() {
        let (store, dir) = temp_store();
        store
            .save_arb_config(&CrossVenueArbConfig {
                leg_a_venue: "kalshi".into(),
                leg_a_account: "paper".into(),
                leg_b_venue: "polymarket".into(),
                leg_b_account: "paper".into(),
                pairs: vec!["BTC".into()],
            })
            .unwrap();
        let pairs_json: String = store
            .risk
            .db()
            .with_connection(|conn| {
                conn.query_row("SELECT pairs_json FROM arb_pairs WHERE id = 1", [], |r| {
                    r.get(0)
                })
            })
            .unwrap();
        assert_eq!(pairs_json, "[\"BTC\"]");
        store.clear_arb_config().unwrap();
        let left: i64 = store
            .risk
            .db()
            .with_connection(|conn| {
                conn.query_row("SELECT COUNT(*) FROM arb_pairs", [], |r| r.get(0))
            })
            .unwrap();
        assert_eq!(left, 0);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// put_setting: `Some(value)` upserts and reads back; `None` sweeps the
    /// key (reads back as None = inherit the globals); and writing account A
    /// never touches account B — the row lands under the AccountScope account,
    /// never a guessed id (#1221/#1223 bug class).
    #[test]
    fn put_setting_upsert_sweep_and_account_isolation() {
        let (store, dir) = temp_store();
        let alice = crate::account::AccountId::from("alice");
        let bob = crate::account::AccountId::from("bob");

        assert!(
            matches!(
                store.put_setting(AccountScope::new(&alice), "round.duration_sec", Some("300")),
                Ok(WriteOutcome::Persisted)
            ),
            "put_setting(Some) must report Persisted"
        );
        let got = store
            .risk
            .db()
            .with_connection(|conn| {
                crate::db::account_setting_get(conn, "alice", "round.duration_sec")
            })
            .unwrap();
        assert_eq!(got.as_deref(), Some("300"));

        // Repeat upsert overwrites (no duplicate row).
        store
            .put_setting(AccountScope::new(&alice), "round.duration_sec", Some("900"))
            .unwrap();
        let got = store
            .risk
            .db()
            .with_connection(|conn| {
                crate::db::account_setting_get(conn, "alice", "round.duration_sec")
            })
            .unwrap();
        assert_eq!(got.as_deref(), Some("900"));

        // None sweeps the key: gone = inherit the globals.
        store
            .put_setting(AccountScope::new(&alice), "round.duration_sec", None)
            .unwrap();
        let got = store
            .risk
            .db()
            .with_connection(|conn| {
                crate::db::account_setting_get(conn, "alice", "round.duration_sec")
            })
            .unwrap();
        assert_eq!(got, None, "swept key must read back as None");

        // Cross-account isolation: bob's write never resurrects/touches alice's row.
        store
            .put_setting(AccountScope::new(&bob), "round.duration_sec", Some("100"))
            .unwrap();
        let got_bob = store
            .risk
            .db()
            .with_connection(|conn| {
                crate::db::account_setting_get(conn, "bob", "round.duration_sec")
            })
            .unwrap();
        assert_eq!(got_bob.as_deref(), Some("100"));
        let got_alice = store
            .risk
            .db()
            .with_connection(|conn| {
                crate::db::account_setting_get(conn, "alice", "round.duration_sec")
            })
            .unwrap();
        assert_eq!(got_alice, None, "alice's row stays swept after bob's write");

        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The credentials domain refuses with a typed error when the master key
    /// is missing (操作者裁决 2026-11-14：写入面是加密的 kernel_meta 行；无键
    /// 是诚实的 NotAuthenticated，绝不静默明文）。
    #[test]
    fn credentials_refuse_with_a_typed_error() {
        let _lock = crate::db::credentials::ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Deterministic: strip the var for the open + write, restore after.
        struct Restore(Option<String>);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(v) => std::env::set_var(crate::db::credentials::MASTER_KEY_ENV, v),
                        None => std::env::remove_var(crate::db::credentials::MASTER_KEY_ENV),
                    }
                }
            }
        }
        let saved = std::env::var(crate::db::credentials::MASTER_KEY_ENV).ok();
        unsafe { std::env::remove_var(crate::db::credentials::MASTER_KEY_ENV) };
        let _restore = Restore(saved);

        let (store, dir) = temp_store();
        let err = store
            .save_credentials(
                &BTreeMap::from([("KALSHI_API_KEY".to_string(), "k".to_string())]),
                &[],
            )
            .unwrap_err();
        assert_eq!(err.code, CoreErrorCode::NotAuthenticated);
        assert!(
            err.message.contains("BLITZKRIEG_DB_KEY"),
            "the refusal names the master key, never a value: {err}"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The open is the constructor: an unopenable path is an error (the
    /// fail-closed posture — no store-without-a-database state exists).
    #[test]
    fn open_failure_is_an_error() {
        // A path whose parent is a regular FILE can never be created.
        let dir =
            std::env::temp_dir().join(format!("bk-sqlite-store-open-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        assert!(SqliteStore::open(blocker.join("store.sqlite")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
