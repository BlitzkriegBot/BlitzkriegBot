//! The durable closed-trade log (schema v4) — the SQL side of the trades domain.
//!
//! # Why this module exists
//!
//! Before v4 every closed trade lived in `data/trades/trades.jsonl`: written by
//! `TradeDb::record`, read whole by `trade_db::read_page`, and re-folded in
//! memory for every summary. The store seam (`crate::store::StoreApi`) had **no
//! trade method at all**, so this domain could not be carried by anything but a
//! file. [`TradeRead`] / [`TradeWrite`] froze the seam; this module is its SQL
//! implementation, and [`crate::store::SqliteStore`] is the adapter that exposes
//! it.
//!
//! # The row address
//!
//! `(mode, account_id, id, exit_time)`. Three things about that are load-bearing:
//!
//! * **`mode` is in the key.** One database file serves every mode, while the
//!   JSONL era kept dry and live trades in two separate FILES
//!   (`trades.jsonl` / `trades-live.jsonl`). Dropping `mode` would merge the two
//!   books — a live panel rendering dry-era rows is exactly the defect that
//!   split existed to prevent, and it stays closed here.
//! * **`net_pnl_usd` is NOT in the key.** The JSONL version had to include that
//!   moving field because a file line has no stable identity, which is why
//!   `amend_net_pnl` could not address a row it had already amended. Here the
//!   amendment guards its precondition with an explicit compare-and-swap.
//! * **`id` alone is not enough.** The position book reuses `hft-N` ids across
//!   restarts (#942: nine collisions observed in production), so a reissued id
//!   can legitimately meet an old close — at a DIFFERENT `exit_time`. Same id
//!   at the same exit time is the same close twice.
//!
//! # What this module does not decide
//!
//! It does not decide what a trade is worth. Every money leg is copied verbatim
//! out of the [`TradeRecord`] the caller supplied; the totals query sums those
//! copies. Nothing here adjusts, floors, caps or reclassifies a PnL — R7.1
//! L1–L3 territory is upstream, and this is deliberately a dumb carrier.
//!
//! # SQL text is a constant, never assembled
//!
//! Every statement below is a `const &str` with positional parameters. Nothing
//! interpolates a caller-supplied value into SQL text: the two scoped variants
//! of each query are two literal constants chosen by `match`, and every value
//! (mode, account, id, limit) is bound. This is the acceptance rule
//! 「所有外部输入必须使用参数绑定」 taken literally — there is no `format!`
//! anywhere in this file.

use rusqlite::{OptionalExtension, params};
use rust_decimal::Decimal;
use std::str::FromStr;

use crate::contracts::metrics::Tally;
use crate::contracts::store::{AccountScope, StoreError, StoreTx, WriteOutcome};
use crate::contracts::trades::{
    TradeIdentity, TradeRead, TradeRecord, TradeRetraction, TradeScope, TradeTotals, TradeWrite,
    UnverifiedTrades,
};
use crate::db::Database;
use crate::metrics::Outcome;
use crate::model::{AccountId, Mode};

/// The value written to `trades.mode`.
///
/// A plain string column rather than an enum column because SQLite has no enum
/// and the mapping must be readable by an operator running `sqlite3` by hand.
/// `live` and `dry` must never share a value: that is the whole point of the
/// column, and the JSONL era got the same separation from two filenames.
pub fn mode_key(mode: Mode) -> &'static str {
    match mode {
        Mode::Dry => "dry",
        Mode::Live => "live",
        Mode::ReadOnly => "readonly",
    }
}

/// The trades carrier for ONE book: its database plus the mode it addresses.
///
/// The mode is a field rather than a per-call argument because it is not the
/// caller's choice — a kernel assembled for `Live` must never write into the
/// `dry` book, and a parameter means every call site can get it wrong.
/// [`crate::store::SqliteStore::trade_log`] supplies it from the boot mode.
#[derive(Clone)]
pub struct TradeLog {
    db: Database,
    mode: Mode,
}

impl std::fmt::Debug for TradeLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TradeLog")
            .field("path", &self.db.path())
            .field("mode", &mode_key(self.mode))
            .finish()
    }
}

// ---------------------------------------------------------------------------
// SQL constants. No format!, no interpolation — see the module docs.
// ---------------------------------------------------------------------------

const INSERT_TRADE: &str = "\
INSERT INTO trades (
    mode, account_id, id, exit_time, payload_json,
    net_pnl_usd, gross_pnl_usd, fees_usd, hold_time_sec,
    source, written_at_ms
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)";

const SELECT_ONE_PAYLOAD: &str = "\
SELECT payload_json FROM trades
WHERE mode = ?1 AND account_id = ?2 AND id = ?3 AND exit_time = ?4";

const DELETE_ONE: &str = "\
DELETE FROM trades
WHERE mode = ?1 AND account_id = ?2 AND id = ?3 AND exit_time = ?4";

const UPDATE_AMENDED: &str = "\
UPDATE trades SET payload_json = ?5, net_pnl_usd = ?6, gross_pnl_usd = ?7, written_at_ms = ?8
WHERE mode = ?1 AND account_id = ?2 AND id = ?3 AND exit_time = ?4";

const PAGE_ALL: &str = "\
SELECT payload_json FROM trades
WHERE mode = ?1
ORDER BY exit_time DESC, id DESC
LIMIT ?2 OFFSET ?3";

const PAGE_ACCOUNT: &str = "\
SELECT payload_json FROM trades
WHERE mode = ?1 AND account_id = ?2
ORDER BY exit_time DESC, id DESC
LIMIT ?3 OFFSET ?4";

const LEGS_ALL: &str = "\
SELECT net_pnl_usd, gross_pnl_usd, fees_usd, hold_time_sec, source FROM trades WHERE mode = ?1";

const LEGS_ACCOUNT: &str = "\
SELECT net_pnl_usd, gross_pnl_usd, fees_usd, hold_time_sec, source FROM trades
WHERE mode = ?1 AND account_id = ?2";

const ID_TAILS: &str = "SELECT id FROM trades WHERE mode = ?1 AND id LIKE 'hft-%'";

const EXISTING_KEYS: &str = "SELECT account_id, id, exit_time FROM trades WHERE mode = ?1";

const ARCHIVE_ACCOUNT: &str = "\
INSERT INTO trades_archive (
    mode, account_id, id, exit_time, payload_json,
    net_pnl_usd, gross_pnl_usd, fees_usd, hold_time_sec,
    source, written_at_ms, archived_at_ms, archive_reason
)
SELECT mode, account_id, id, exit_time, payload_json,
       net_pnl_usd, gross_pnl_usd, fees_usd, hold_time_sec,
       source, written_at_ms, ?3, ?4
FROM trades WHERE mode = ?1 AND account_id = ?2";

const DELETE_ACCOUNT: &str = "DELETE FROM trades WHERE mode = ?1 AND account_id = ?2";

impl TradeLog {
    pub fn new(db: Database, mode: Mode) -> Self {
        Self { db, mode }
    }

    /// The database this log writes through (composition roots that already hold
    /// a handle must not open a second one on the same file).
    pub fn database(&self) -> &Database {
        &self.db
    }

    /// The book this log addresses.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    // -- standalone writes ---------------------------------------------------
    //
    // Each opens and commits its own transaction. The [`TradeWrite`] trait
    // methods are the same operations inside a CALLER-owned transaction; these
    // exist for call sites whose unit of work is exactly one trade fact (a
    // close, a genuine retraction). Anything that must be atomic with another
    // write uses the trait form.

    /// Append one closed trade, committing immediately.
    pub fn append(&self, record: &TradeRecord) -> Result<WriteOutcome, StoreError> {
        let now = crate::db::now_ms();
        self.write(|tx| {
            let scope = AccountScope {
                account: &record.account_id,
            };
            self.append_in(tx, scope, record, now)
                .map(|()| WriteOutcome::Persisted { at_ms: now })
        })
    }

    /// Remove one recorded trade, committing immediately.
    ///
    /// Takes the record (not an identity) because every caller has the closed
    /// position in hand; deriving the address happens here, in one place.
    /// `now_ms` is the caller's clock — the receipt's `at_ms` is the moment the
    /// caller believes the withdrawal happened, not a second read of the wall
    /// clock inside the store.
    pub fn retract(&self, record: &TradeRecord, now_ms: i64) -> Result<WriteOutcome, StoreError> {
        let identity = TradeIdentity {
            id: record.id.clone(),
            exit_time: record.exit_time,
        };
        let scope = AccountScope {
            account: &record.account_id,
        };
        self.write(|tx| {
            if self.retract_in(tx, scope, &identity)? {
                Ok(WriteOutcome::Persisted { at_ms: now_ms })
            } else {
                Err(StoreError::NotFound)
            }
        })
    }

    /// Move one trade's net PnL to venue truth, committing immediately.
    pub fn amend_net_pnl(
        &self,
        account: &AccountId,
        identity: &TradeIdentity,
        expected_net: Decimal,
        delta: Decimal,
    ) -> Result<WriteOutcome, StoreError> {
        let scope = AccountScope { account };
        let now = crate::db::now_ms();
        self.write(
            |tx| match self.amend_in(tx, scope, identity, expected_net, delta, now)? {
                AmendOutcome::Amended => Ok(WriteOutcome::Persisted { at_ms: now }),
                AmendOutcome::NotFound => Err(StoreError::NotFound),
                AmendOutcome::StaleBookedValue { stored } => Err(StoreError::Refused(format!(
                    "trade {} at {} carries net {} but the amendment expected {} — \
                         another amendment already moved it; nothing was written",
                    identity.id, identity.exit_time, stored, expected_net
                ))),
            },
        )
    }

    /// Erase one account's closed-trade history, committing immediately.
    pub fn retract_account(&self, account: &AccountId) -> Result<TradeRetraction, StoreError> {
        let scope = AccountScope { account };
        let now = crate::db::now_ms();
        self.write(|tx| self.retract_account_in(tx, scope, now))
    }

    /// Persist `source = "reconciled"` for fallback-closed rows matching
    /// `condition_id` so the durable SQLite ledger stays aligned with
    /// `PositionManager::backfill_fallback_to_venue` even across restarts.
    /// Keeps `exit_price` and `original_fallback_price` unchanged (issue #991).
    pub fn mark_fallback_reconciled_by_condition(
        &self,
        condition_id: &str,
        now_ms: i64,
    ) -> Result<usize, StoreError> {
        crate::store::mark_fallback_trades_reconciled(
            &self.db,
            self.mode,
            |rec| rec.condition_id == condition_id,
            now_ms,
        )
    }

    /// Persist `source = "reconciled"` for fallback-closed rows whose
    /// `token_id` is in `tokens`.
    pub fn mark_fallback_reconciled_by_tokens(
        &self,
        tokens: &std::collections::HashSet<&str>,
        now_ms: i64,
    ) -> Result<usize, StoreError> {
        crate::store::mark_fallback_trades_reconciled(
            &self.db,
            self.mode,
            |rec| tokens.contains(rec.token_id.as_str()),
            now_ms,
        )
    }

    // -- write helpers, all inside a caller-owned transaction -----------------

    fn append_in(
        &self,
        tx: &StoreTx<'_>,
        scope: AccountScope<'_>,
        record: &TradeRecord,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        // The scope must address the record's own account. A mismatch means a
        // caller is writing one book's trade into another's scope — the
        // #1221/#1223 class of defect, refused rather than "fixed up" by
        // trusting either side.
        if scope.account.as_str() != record.account_id.as_str() {
            return Err(StoreError::Invalid(format!(
                "trade scope mismatch: scope is `{}`, record carries `{}` — a close must \
                 be written into its own book",
                scope.account.as_str(),
                record.account_id.as_str()
            )));
        }
        let payload = serde_json::to_string(record).map_err(|e| {
            StoreError::Invalid(format!(
                "trade record could not be serialized for storage ({e}) — refusing to \
                 write a row the read face could not reproduce"
            ))
        })?;
        tx.connection()
            .execute(
                INSERT_TRADE,
                params![
                    mode_key(self.mode),
                    record.account_id.as_str(),
                    record.id,
                    record.exit_time,
                    payload,
                    record.net_pnl_usd.to_string(),
                    record.gross_pnl_usd.to_string(),
                    record.fees_usd.to_string(),
                    record.hold_time_sec,
                    record.source,
                    now_ms,
                ],
            )
            .map_err(|e| match e {
                // A unique-constraint violation here is the duplicate-address
                // case, not a backend failure: name WHICH close collided so the
                // caller can report it instead of a generic SQL error.
                rusqlite::Error::SqliteFailure(err, _)
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    StoreError::Refused(format!(
                        "trade {} on account `{}` at exit_time {} ({}) already exists — a \
                         close is an event; refusing to overwrite it",
                        record.id,
                        record.account_id.as_str(),
                        record.exit_time,
                        mode_key(self.mode)
                    ))
                }
                other => StoreError::Backend(format!("trades insert failed: {other}")),
            })?;
        Ok(())
    }

    fn retract_in(
        &self,
        tx: &StoreTx<'_>,
        scope: AccountScope<'_>,
        identity: &TradeIdentity,
    ) -> Result<bool, StoreError> {
        let removed = tx
            .connection()
            .execute(
                DELETE_ONE,
                params![
                    mode_key(self.mode),
                    scope.account.as_str(),
                    identity.id,
                    identity.exit_time,
                ],
            )
            .map_err(|e| StoreError::Backend(format!("trades delete failed: {e}")))?;
        Ok(removed > 0)
    }

    fn amend_in(
        &self,
        tx: &StoreTx<'_>,
        scope: AccountScope<'_>,
        identity: &TradeIdentity,
        expected_net: Decimal,
        delta: Decimal,
        now_ms: i64,
    ) -> Result<AmendOutcome, StoreError> {
        // Read the payload inside the caller's transaction: the amendment must
        // rewrite the JSON record AND the aggregate legs so the two describe the
        // same row.
        let current: Option<String> = tx
            .connection()
            .query_row(
                SELECT_ONE_PAYLOAD,
                params![
                    mode_key(self.mode),
                    scope.account.as_str(),
                    identity.id,
                    identity.exit_time,
                ],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| StoreError::Backend(format!("trades amend read failed: {e}")))?;
        let Some(payload) = current else {
            return Ok(AmendOutcome::NotFound);
        };
        let mut record: TradeRecord = serde_json::from_str(&payload).map_err(|e| {
            StoreError::Backend(format!(
                "stored trade payload for {} at {} is unreadable ({e}) — refusing to amend \
                 a row that cannot be reproduced",
                identity.id, identity.exit_time
            ))
        })?;
        if record.net_pnl_usd != expected_net {
            return Ok(AmendOutcome::StaleBookedValue {
                stored: record.net_pnl_usd,
            });
        }
        // Only the payout was estimated, not the fees (issue #991): `delta` moves
        // net and gross together, the fee leg stays put.
        record.net_pnl_usd += delta;
        record.gross_pnl_usd += delta;
        let rewritten = serde_json::to_string(&record).map_err(|e| {
            StoreError::Backend(format!(
                "amended trade payload could not be serialized: {e}"
            ))
        })?;
        let updated = tx
            .connection()
            .execute(
                UPDATE_AMENDED,
                params![
                    mode_key(self.mode),
                    scope.account.as_str(),
                    identity.id,
                    identity.exit_time,
                    rewritten,
                    record.net_pnl_usd.to_string(),
                    record.gross_pnl_usd.to_string(),
                    now_ms,
                ],
            )
            .map_err(|e| StoreError::Backend(format!("trades amend update failed: {e}")))?;
        if updated == 0 {
            // The row vanished between the read and the write inside our own
            // transaction — impossible under SQLite's snapshot, but reporting
            // "not found" rather than success keeps the caller honest.
            return Ok(AmendOutcome::NotFound);
        }
        Ok(AmendOutcome::Amended)
    }

    fn retract_account_in(
        &self,
        tx: &StoreTx<'_>,
        scope: AccountScope<'_>,
        now_ms: i64,
    ) -> Result<TradeRetraction, StoreError> {
        let reason = "account trade history reset";
        // Archive FIRST, in the same transaction (issue #1253): a wipe that
        // leaves no trace of what it removed is an unarchived erasure. Same
        // transaction means a crash cannot leave one without the other.
        let archived = tx
            .connection()
            .execute(
                ARCHIVE_ACCOUNT,
                params![mode_key(self.mode), scope.account.as_str(), now_ms, reason],
            )
            .map_err(|e| {
                StoreError::Refused(format!(
                    "refusing to erase account `{}` trades: the archive write failed ({e}) — \
                     an unarchived erase is not a reset",
                    scope.account.as_str()
                ))
            })?;
        let removed = tx
            .connection()
            .execute(
                DELETE_ACCOUNT,
                params![mode_key(self.mode), scope.account.as_str()],
            )
            .map_err(|e| StoreError::Backend(format!("trades reset delete failed: {e}")))?;
        if removed as u64 != archived as u64 {
            // Both statements are in one transaction over one predicate, so a
            // mismatch means the archive and the delete disagree about which
            // rows are in scope. Refusing (rather than committing a partial
            // erase) is the only honest outcome.
            return Err(StoreError::Backend(format!(
                "trade reset archived {archived} rows but deleted {removed} — the archive \
                 and the erase disagree; nothing was committed"
            )));
        }
        Ok(TradeRetraction {
            removed: removed as u64,
            archived_to: format!(
                "trades_archive(mode={}, account={}, archived_at_ms={})",
                mode_key(self.mode),
                scope.account.as_str(),
                now_ms
            ),
            at_ms: now_ms,
        })
    }

    /// Run `f` inside one transaction on this log's connection, then commit.
    ///
    /// `f` gets a `&mut StoreTx`, so a domain method cannot tell a standalone
    /// write from one inside a larger unit of work.
    fn write<T>(
        &self,
        f: impl FnOnce(&mut StoreTx<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.db.with_connection(|conn| {
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| StoreError::Backend(format!("trades transaction failed: {e}")))?;
            let out = {
                let mut stx = StoreTx::new(&tx);
                f(&mut stx)?
            };
            tx.commit()
                .map_err(|e| StoreError::Backend(format!("trades commit failed: {e}")))?;
            Ok(out)
        })
    }

    // -- read helpers --------------------------------------------------------

    fn page_in(
        &self,
        scope: TradeScope<'_>,
        from_newest: u64,
        limit: u32,
    ) -> Result<Vec<TradeRecord>, StoreError> {
        self.db.with_connection(|conn| {
            let payloads: Vec<String> = match scope {
                TradeScope::Account(s) => {
                    let mut stmt = conn.prepare(PAGE_ACCOUNT).map_err(|e| {
                        StoreError::Backend(format!("trades page prepare failed: {e}"))
                    })?;
                    let mapped = stmt
                        .query_map(
                            params![
                                mode_key(self.mode),
                                s.account.as_str(),
                                i64::from(limit),
                                from_newest as i64
                            ],
                            |r| r.get::<_, String>(0),
                        )
                        .map_err(|e| {
                            StoreError::Backend(format!("trades page query failed: {e}"))
                        })?;
                    collect_strings(mapped)?
                }
                TradeScope::AllAccounts => {
                    let mut stmt = conn.prepare(PAGE_ALL).map_err(|e| {
                        StoreError::Backend(format!("trades page prepare failed: {e}"))
                    })?;
                    let mapped = stmt
                        .query_map(
                            params![mode_key(self.mode), i64::from(limit), from_newest as i64],
                            |r| r.get::<_, String>(0),
                        )
                        .map_err(|e| {
                            StoreError::Backend(format!("trades page query failed: {e}"))
                        })?;
                    collect_strings(mapped)?
                }
            };
            // SQL gave us the window from the newest end; the contract promises
            // chronological order INSIDE the window, so reverse it.
            let mut records = Vec::with_capacity(payloads.len());
            for payload in payloads.into_iter().rev() {
                records.push(serde_json::from_str::<TradeRecord>(&payload).map_err(|e| {
                    StoreError::Backend(format!(
                        "a stored trade payload is unreadable ({e}) — the row exists but \
                         cannot be reproduced"
                    ))
                })?);
            }
            Ok(records)
        })
    }

    fn totals_in(&self, scope: TradeScope<'_>, as_of_ms: i64) -> Result<TradeTotals, StoreError> {
        let account: Option<String> = match scope {
            TradeScope::Account(s) => Some(s.account.as_str().to_string()),
            TradeScope::AllAccounts => None,
        };
        self.db.with_connection(|conn| {
            let mut acc = TotalsAccumulator::default();
            // The two scopes have different parameter shapes, so the fold is
            // spelled once per branch rather than abstracted over a params build.
            if let Some(account) = &account {
                let mut stmt = conn.prepare(LEGS_ACCOUNT).map_err(|e| {
                    StoreError::Backend(format!("trades totals prepare failed: {e}"))
                })?;
                let mapped = stmt
                    .query_map(params![mode_key(self.mode), account], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, String>(4)?,
                        ))
                    })
                    .map_err(|e| StoreError::Backend(format!("trades totals query failed: {e}")))?;
                for row in mapped {
                    let (net, gross, fees, hold, source) = row.map_err(|e| {
                        StoreError::Backend(format!("trades totals row failed: {e}"))
                    })?;
                    acc.push(&net, &gross, &fees, hold, &source)?;
                }
            } else {
                let mut stmt = conn.prepare(LEGS_ALL).map_err(|e| {
                    StoreError::Backend(format!("trades totals prepare failed: {e}"))
                })?;
                let mapped = stmt
                    .query_map(params![mode_key(self.mode)], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, String>(4)?,
                        ))
                    })
                    .map_err(|e| StoreError::Backend(format!("trades totals query failed: {e}")))?;
                for row in mapped {
                    let (net, gross, fees, hold, source) = row.map_err(|e| {
                        StoreError::Backend(format!("trades totals row failed: {e}"))
                    })?;
                    acc.push(&net, &gross, &fees, hold, &source)?;
                }
            }
            Ok(acc.finish(account.map(|a| AccountId::from(a.as_str())), as_of_ms))
        })
    }

    fn max_id_tail_in(&self) -> Result<Option<u64>, StoreError> {
        self.db.with_connection(|conn| {
            let mut stmt = conn
                .prepare(ID_TAILS)
                .map_err(|e| StoreError::Backend(format!("trades id-tail prepare failed: {e}")))?;
            let mapped = stmt
                .query_map(params![mode_key(self.mode)], |r| r.get::<_, String>(0))
                .map_err(|e| StoreError::Backend(format!("trades id-tail query failed: {e}")))?;
            let mut max: Option<u64> = None;
            for row in mapped {
                let id = row
                    .map_err(|e| StoreError::Backend(format!("trades id-tail row failed: {e}")))?;
                if let Some(tail) = id.strip_prefix("hft-")
                    && let Ok(n) = tail.parse::<u64>()
                {
                    max = Some(max.map_or(n, |m| m.max(n)));
                }
            }
            Ok(max)
        })
    }

    /// Import one JSONL trade log into this book, refusing the WHOLE import on
    /// any `(account, id, exit_time)` collision.
    ///
    /// # Why refusal, not upsert
    ///
    /// A collision means the same close would exist twice. Silently collapsing it
    /// would delete a real row from the books (R7.1 L2); silently keeping both is
    /// impossible under the primary key. There is no honest automatic
    /// resolution, so the importer reports every collision and writes NOTHING —
    /// the operator cleans the history and re-runs.
    ///
    /// The whole import is one transaction: a failure halfway leaves the database
    /// exactly as it was, and `Ok` with a non-empty `refused` means not one row
    /// landed.
    pub fn import_jsonl(&self, path: &std::path::Path) -> Result<ImportReport, StoreError> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            StoreError::Backend(format!(
                "trade log {} could not be read: {e}",
                path.display()
            ))
        })?;
        // Parse everything first: nothing touches the database until every line
        // has been accounted for.
        let mut records: Vec<(usize, TradeRecord)> = Vec::new();
        let mut refused: Vec<ImportDefect> = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<TradeRecord>(trimmed) {
                Ok(rec) => records.push((index + 1, rec)),
                Err(e) => refused.push(ImportDefect {
                    line: index + 1,
                    reason: format!("unparseable trade row: {e}"),
                    account: String::new(),
                    id: String::new(),
                    exit_time: 0,
                }),
            }
        }

        // Duplicates WITHIN the file.
        let mut first_seen: std::collections::HashMap<(String, String, i64), usize> =
            std::collections::HashMap::new();
        for (line, rec) in &records {
            let key = (
                rec.account_id.as_str().to_string(),
                rec.id.clone(),
                rec.exit_time,
            );
            if let Some(previous) = first_seen.get(&key) {
                refused.push(ImportDefect {
                    line: *line,
                    reason: format!(
                        "duplicate (account, id, exit_time) — first seen at line {previous}; the \
                         primary key cannot hold both and collapsing them would delete a real close"
                    ),
                    account: rec.account_id.as_str().to_string(),
                    id: rec.id.clone(),
                    exit_time: rec.exit_time,
                });
            } else {
                first_seen.insert(key, *line);
            }
        }

        // Collisions with rows ALREADY in the ledger. Reported, never skipped
        // silently — re-running a completed import is a refusal, not a no-op.
        let existing = self.existing_keys()?;
        for (line, rec) in &records {
            let key = (
                rec.account_id.as_str().to_string(),
                rec.id.clone(),
                rec.exit_time,
            );
            if existing.contains(&key) {
                refused.push(ImportDefect {
                    line: *line,
                    reason: format!(
                        "a trade with (account, id, exit_time) = ({}, {}, {}) is already in the \
                         {} ledger",
                        key.0,
                        key.1,
                        key.2,
                        mode_key(self.mode)
                    ),
                    account: key.0,
                    id: key.1,
                    exit_time: key.2,
                });
            }
        }

        if !refused.is_empty() {
            return Ok(ImportReport {
                imported: 0,
                refused,
                mode: mode_key(self.mode),
            });
        }

        let now = crate::db::now_ms();
        self.write(|tx| {
            for (_, rec) in &records {
                let scope = AccountScope {
                    account: &rec.account_id,
                };
                self.append_in(tx, scope, rec, now)?;
            }
            Ok(())
        })?;
        Ok(ImportReport {
            imported: records.len() as u64,
            refused: Vec::new(),
            mode: mode_key(self.mode),
        })
    }

    /// Every `(account, id, exit_time)` currently in this book.
    fn existing_keys(
        &self,
    ) -> Result<std::collections::HashSet<(String, String, i64)>, StoreError> {
        self.db.with_connection(|conn| {
            let mut stmt = conn
                .prepare(EXISTING_KEYS)
                .map_err(|e| StoreError::Backend(format!("trades key scan prepare failed: {e}")))?;
            let mapped = stmt
                .query_map(params![mode_key(self.mode)], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .map_err(|e| StoreError::Backend(format!("trades key scan query failed: {e}")))?;
            let mut out = std::collections::HashSet::new();
            for row in mapped {
                out.insert(row.map_err(|e| {
                    StoreError::Backend(format!("trades key scan row failed: {e}"))
                })?);
            }
            Ok(out)
        })
    }

    /// Export this book back to the Node-compatible JSONL format.
    ///
    /// The DEMOTED direction: JSONL is an export an operator or an offline script
    /// asks for, never the kernel's carrier. The file is written whole and
    /// atomically because it is a snapshot, not a log.
    pub fn export_jsonl(&self, path: &std::path::Path) -> Result<u64, StoreError> {
        let payloads: Vec<String> = self.db.with_connection(|conn| {
            let mut stmt = conn
                .prepare(PAGE_ALL)
                .map_err(|e| StoreError::Backend(format!("trades export prepare failed: {e}")))?;
            // `-1` is SQLite's "no limit"; the export is the one caller that
            // legitimately wants the whole book, and it says so explicitly
            // rather than passing 0 and inheriting an unbounded read.
            let mapped = stmt
                .query_map(params![mode_key(self.mode), -1i64, 0i64], |r| {
                    r.get::<_, String>(0)
                })
                .map_err(|e| StoreError::Backend(format!("trades export query failed: {e}")))?;
            collect_strings(mapped)
        })?;
        // PAGE_ALL is newest-first; a log is chronological.
        let mut body = String::new();
        for payload in payloads.iter().rev() {
            body.push_str(payload);
            body.push('\n');
        }
        // `rewrite_atomic` reports failure as `false` (it has already cleaned up
        // its temp file). Returning `Ok` here would claim an export that left the
        // previous generation on disk — an export an operator trusts is worse
        // than no export (R7.1 L4).
        if !crate::jsonl::rewrite_atomic(path, body.as_bytes()) {
            return Err(StoreError::Backend(format!(
                "trade log export to {} failed: the atomic rewrite left the previous file in \
                 place (see the kernel log for the io error)",
                path.display()
            )));
        }
        Ok(payloads.len() as u64)
    }
}

impl TradeRead for TradeLog {
    fn load_trades(
        &self,
        scope: TradeScope<'_>,
        from_newest: u64,
        limit: std::num::NonZeroU32,
    ) -> Result<Vec<TradeRecord>, StoreError> {
        self.page_in(scope, from_newest, limit.get())
    }

    fn trade_totals(&self, scope: TradeScope<'_>) -> Result<TradeTotals, StoreError> {
        // The carrier's own clock: event time, never "now" sampled by a caller.
        self.totals_in(scope, crate::db::now_ms())
    }

    fn max_trade_id_tail(&self) -> Result<Option<u64>, StoreError> {
        self.max_id_tail_in()
    }
}

impl TradeWrite for TradeLog {
    fn append_trade(
        &self,
        tx: &mut StoreTx<'_>,
        scope: AccountScope<'_>,
        record: &TradeRecord,
    ) -> Result<WriteOutcome, StoreError> {
        let now = crate::db::now_ms();
        self.append_in(tx, scope, record, now)?;
        Ok(WriteOutcome::Persisted { at_ms: now })
    }

    fn retract_trade(
        &self,
        tx: &mut StoreTx<'_>,
        scope: AccountScope<'_>,
        identity: &TradeIdentity,
        _now_ms: i64,
    ) -> Result<WriteOutcome, StoreError> {
        if self.retract_in(tx, scope, identity)? {
            Ok(WriteOutcome::Persisted {
                at_ms: crate::db::now_ms(),
            })
        } else {
            // "already gone" is a fact the caller distinguishes from a carrier
            // failure — the JSONL version could not (it returned `()`).
            Err(StoreError::NotFound)
        }
    }

    fn amend_trade_net_pnl(
        &self,
        tx: &mut StoreTx<'_>,
        scope: AccountScope<'_>,
        identity: &TradeIdentity,
        expected_net: Decimal,
        delta: Decimal,
        now_ms: i64,
    ) -> Result<WriteOutcome, StoreError> {
        match self.amend_in(tx, scope, identity, expected_net, delta, now_ms)? {
            AmendOutcome::Amended => Ok(WriteOutcome::Persisted { at_ms: now_ms }),
            AmendOutcome::NotFound => Err(StoreError::NotFound),
            AmendOutcome::StaleBookedValue { stored } => Err(StoreError::Refused(format!(
                "trade {} at {} carries net {} but the amendment expected {} — another \
                 amendment already moved it; nothing was written",
                identity.id, identity.exit_time, stored, expected_net
            ))),
        }
    }

    fn retract_account_trades(
        &self,
        tx: &mut StoreTx<'_>,
        scope: AccountScope<'_>,
        now_ms: i64,
    ) -> Result<TradeRetraction, StoreError> {
        self.retract_account_in(tx, scope, now_ms)
    }
}

/// What one amendment actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmendOutcome {
    /// The row moved to the new net.
    Amended,
    /// No row at that identity.
    NotFound,
    /// A row exists but its stored net is not what the caller expected — another
    /// amendment already moved it. Nothing was written.
    StaleBookedValue { stored: Decimal },
}

/// One refused row during an import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportDefect {
    /// 1-based line number in the source file.
    pub line: usize,
    /// Why it cannot be imported. Operator-facing text.
    pub reason: String,
    /// The offending identity, when the row had one.
    pub account: String,
    pub id: String,
    pub exit_time: i64,
}

/// The result of one import attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// How many rows landed. Always 0 when `refused` is non-empty.
    pub imported: u64,
    /// Every defect found. Non-empty means NOTHING was written.
    pub refused: Vec<ImportDefect>,
    /// Which book this report is about.
    pub mode: &'static str,
}

impl ImportReport {
    pub fn is_refusal(&self) -> bool {
        !self.refused.is_empty()
    }

    /// One operator-readable line per collision. This is what the CLI/panel
    /// prints: 「拒绝导入，明细如下」.
    pub fn refusal_lines(&self) -> Vec<String> {
        self.refused
            .iter()
            .map(|d| {
                format!(
                    "line {}: {} (account={}, id={}, exit_time={})",
                    d.line, d.reason, d.account, d.id, d.exit_time
                )
            })
            .collect()
    }
}

/// The running fold behind [`TradeTotals`].
///
/// A struct rather than loose locals so the two scoped queries share one fold —
/// two copies is how the two口径 forks this contract settles got created in the
/// first place.
#[derive(Default)]
struct TotalsAccumulator {
    wins: u64,
    losses: u64,
    flats: u64,
    total: u64,
    net: Decimal,
    gross: Decimal,
    fees: Decimal,
    hold_total: i64,
    best: Option<Decimal>,
    worst: Option<Decimal>,
    unverified_count: u64,
    unverified_net: Decimal,
}

impl TotalsAccumulator {
    fn push(
        &mut self,
        net_raw: &str,
        gross_raw: &str,
        fees_raw: &str,
        hold: i64,
        source: &str,
    ) -> Result<(), StoreError> {
        let net = decimal_of(net_raw, "net_pnl_usd")?;
        self.gross += decimal_of(gross_raw, "gross_pnl_usd")?;
        self.fees += decimal_of(fees_raw, "fees_usd")?;
        self.total += 1;
        self.hold_total += hold;
        self.net += net;
        match Outcome::of_net(net) {
            Outcome::Win => self.wins += 1,
            Outcome::Loss => self.losses += 1,
            Outcome::Flat => self.flats += 1,
        }
        self.best = Some(self.best.map_or(net, |b| b.max(net)));
        self.worst = Some(self.worst.map_or(net, |w| w.min(net)));
        // R7.1「未核实敞口」单列: a `fallback` close has no venue answer for its
        // exit price. It is COUNTED INSIDE `net` above and reported separately
        // here — never subtracted from the headline (that would be L3).
        if source == "fallback" {
            self.unverified_count += 1;
            self.unverified_net += net;
        }
        Ok(())
    }

    fn finish(self, account: Option<AccountId>, as_of_ms: i64) -> TradeTotals {
        TradeTotals {
            account,
            as_of_ms,
            tally: Tally {
                wins: u32::try_from(self.wins).unwrap_or(u32::MAX),
                losses: u32::try_from(self.losses).unwrap_or(u32::MAX),
                flats: u32::try_from(self.flats).unwrap_or(u32::MAX),
            },
            total_trades: self.total,
            gross_pnl: self.gross,
            fees: self.fees,
            net_pnl: self.net,
            unverified: (self.unverified_count > 0).then_some(UnverifiedTrades {
                trades: self.unverified_count,
                net_pnl: self.unverified_net,
            }),
            // Mean over ALL closed trades — the contract's one answer (fork 1).
            avg_hold_time_sec: (self.total > 0)
                .then_some(self.hold_total as f64 / self.total as f64),
            best_trade_pnl: self.best,
            worst_trade_pnl: self.worst,
        }
    }
}

/// Drain a `query_map` of one `String` column.
fn collect_strings(
    mapped: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<String>>,
) -> Result<Vec<String>, StoreError> {
    let mut out = Vec::new();
    for row in mapped {
        out.push(row.map_err(|e| StoreError::Backend(format!("trades row failed: {e}")))?);
    }
    Ok(out)
}

/// Parse one decimal leg from the carrier, refusing rather than zeroing.
///
/// A stored leg that will not parse means the row is corrupt; folding it as zero
/// would report a smaller loss than reality (R7.1 L4). The error names the
/// column so an operator can find the row.
fn decimal_of(raw: &str, column: &str) -> Result<Decimal, StoreError> {
    Decimal::from_str(raw).map_err(|e| {
        StoreError::Backend(format!(
            "stored {column} value `{raw}` is not a decimal ({e}) — refusing to fold a \
             corrupt money leg as zero"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::TempDb;
    use crate::model::{ExitReason, OrderRole, SignalDirection};
    use crate::position::ClosedPosition;
    use rust_decimal_macros::dec;

    fn log(db: &TempDb, mode: Mode) -> TradeLog {
        TradeLog::new(db.db().clone(), mode)
    }

    fn close(id: &str, exit_time: i64, net: Decimal) -> TradeRecord {
        let c = ClosedPosition {
            id: id.into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: SignalDirection::Up,
            token_id: "tok".into(),
            condition_id: "cond".into(),
            entry_price: dec!(0.40),
            exit_price: dec!(0.60),
            shares: dec!(10),
            cost_usd: dec!(4),
            was_maker_entry: true,
            was_maker_exit: false,
            entry_fee_pct: Decimal::ZERO,
            exit_fee_pct: dec!(0.5),
            maker_rebate_usd: Decimal::ZERO,
            pnl_usd: dec!(2),
            pnl_pct: dec!(50),
            net_pnl_usd: net,
            net_pnl_pct: dec!(49.25),
            high_pnl_pct: dec!(52),
            low_pnl_pct: dec!(-3),
            hold_time_sec: 64,
            exit_reason: ExitReason::TakeProfit,
            entered_at_ms: exit_time - 64_000,
            exited_at_ms: exit_time,
            entry_role: OrderRole::Maker,
            exit_role: OrderRole::Taker,
            dust_shares: Decimal::ZERO,
            account_id: crate::model::default_account_id(),
            venue: String::new(),
            source: String::new(),
            original_fallback_price: None,
            price_source: String::new(),
        };
        TradeRecord::from_closed(&c)
    }

    fn close_for(account: &str, id: &str, exit_time: i64, net: Decimal) -> TradeRecord {
        let mut rec = close(id, exit_time, net);
        rec.account_id = AccountId::from(account);
        // `from_closed` derives gross from net + fees; set it explicitly here so
        // a totals assertion can never pass because of a fee-side coincidence.
        rec.gross_pnl_usd = net;
        rec
    }

    fn append(l: &TradeLog, rec: &TradeRecord) -> Result<WriteOutcome, StoreError> {
        l.write(|tx| {
            let scope = AccountScope {
                account: &rec.account_id,
            };
            l.append_trade(tx, scope, rec)
        })
    }

    #[test]
    fn append_then_read_roundtrips_every_field() {
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        let rec = close("hft-1", 65_000, dec!(1.97));
        append(&l, &rec).expect("append lands");

        let rows = l
            .load_trades(
                TradeScope::AllAccounts,
                0,
                std::num::NonZeroU32::new(10).unwrap(),
            )
            .expect("read back");
        assert_eq!(rows.len(), 1);
        // The payload is stored verbatim, so a whole-value compare is the only
        // honest check that the read face reproduces the write.
        let roundtrip = serde_json::to_value(&rows[0]).unwrap();
        let original = serde_json::to_value(&rec).unwrap();
        assert_eq!(roundtrip, original, "every field survives the carrier");
    }

    #[test]
    fn dry_and_live_books_do_not_mix() {
        // The defect this guards: one database file, two modes. If `mode` were
        // not part of the row address a live panel would render dry rows.
        let db = TempDb::new();
        let dry = log(&db, Mode::Dry);
        let live = log(&db, Mode::Live);
        append(&dry, &close("hft-1", 1_000, dec!(1))).unwrap();
        append(&live, &close("hft-2", 2_000, dec!(2))).unwrap();

        let dry_rows = dry
            .load_trades(
                TradeScope::AllAccounts,
                0,
                std::num::NonZeroU32::new(10).unwrap(),
            )
            .unwrap();
        let live_rows = live
            .load_trades(
                TradeScope::AllAccounts,
                0,
                std::num::NonZeroU32::new(10).unwrap(),
            )
            .unwrap();
        assert_eq!(dry_rows.len(), 1);
        assert_eq!(live_rows.len(), 1);
        assert_eq!(dry_rows[0].id, "hft-1");
        assert_eq!(live_rows[0].id, "hft-2");

        // Totals are per book too, not a fold over both.
        assert_eq!(
            dry.trade_totals(TradeScope::AllAccounts).unwrap().net_pnl,
            dec!(1)
        );
        assert_eq!(
            live.trade_totals(TradeScope::AllAccounts).unwrap().net_pnl,
            dec!(2)
        );
    }

    #[test]
    fn the_same_close_cannot_be_appended_twice() {
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        let rec = close("hft-1", 5_000, dec!(1));
        append(&l, &rec).unwrap();
        let err = append(&l, &rec).expect_err("a duplicate close is refused");
        let text = format!("{err:?}");
        assert!(
            text.contains("already exists"),
            "the refusal must name the collision, got: {text}"
        );
        assert_eq!(
            l.trade_totals(TradeScope::AllAccounts)
                .unwrap()
                .total_trades,
            1,
            "a refused insert must not have written anything"
        );
    }

    #[test]
    fn a_reissued_id_at_a_new_exit_time_is_a_different_close() {
        // #942: the position book reuses hft-N across restarts, so the same id
        // at a different exit_time is a legitimate second close.
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        append(&l, &close("hft-1", 1_000, dec!(1))).unwrap();
        append(&l, &close("hft-1", 2_000, dec!(2)))
            .expect("same id, different close time is a new row");
        assert_eq!(
            l.trade_totals(TradeScope::AllAccounts)
                .unwrap()
                .total_trades,
            2
        );
    }

    #[test]
    fn scope_mismatch_is_refused_rather_than_written() {
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        let rec = close_for("alpha", "hft-1", 1_000, dec!(1));
        let other = AccountId::from("beta");
        let err = l
            .write(|tx| l.append_trade(tx, AccountScope { account: &other }, &rec))
            .expect_err("writing alpha's close into beta's scope must be refused");
        assert!(format!("{err:?}").contains("scope mismatch"));
        assert_eq!(
            l.trade_totals(TradeScope::AllAccounts)
                .unwrap()
                .total_trades,
            0
        );
    }

    #[test]
    fn totals_split_wins_losses_flats_and_keep_flats_out_of_the_rate() {
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        append(&l, &close_for("alpha", "hft-1", 1_000, dec!(5))).unwrap();
        append(&l, &close_for("alpha", "hft-2", 2_000, dec!(-3))).unwrap();
        append(&l, &close_for("alpha", "hft-3", 3_000, Decimal::ZERO)).unwrap();

        let t = l
            .trade_totals(TradeScope::Account(AccountScope {
                account: &AccountId::from("alpha"),
            }))
            .unwrap();
        assert_eq!(t.total_trades, 3);
        assert_eq!(t.tally.wins, 1);
        assert_eq!(t.tally.losses, 1);
        assert_eq!(t.tally.flats, 1);
        // flats excluded from the denominator (Tally::win_rate, one formula)
        assert_eq!(t.tally.win_rate(), Some(0.5));
        assert_eq!(t.net_pnl, dec!(2));
        assert_eq!(t.gross_pnl, dec!(2));
        assert_eq!(t.best_trade_pnl, Some(dec!(5)));
        assert_eq!(t.worst_trade_pnl, Some(dec!(-3)));
        // Mean hold over ALL closes, not only the decided ones (fork 1).
        assert_eq!(t.avg_hold_time_sec, Some(64.0));
        assert_eq!(t.account.as_ref().map(|a| a.as_str()), Some("alpha"));
    }

    #[test]
    fn an_account_with_no_trades_has_no_fabricated_best_or_rate() {
        // The fork this settles: `fold_into` started best at 0.0, so an account
        // with no trades reported a best of 0.00 — a trade it never made.
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        let t = l
            .trade_totals(TradeScope::Account(AccountScope {
                account: &AccountId::from("nobody"),
            }))
            .unwrap();
        assert_eq!(t.total_trades, 0);
        assert_eq!(t.best_trade_pnl, None);
        assert_eq!(t.worst_trade_pnl, None);
        assert_eq!(t.avg_hold_time_sec, None);
        assert_eq!(
            t.tally.win_rate(),
            None,
            "no decided trades = unknown, not 0%"
        );
        assert_eq!(
            t.unverified, None,
            "no unverified closes is a fact, not zero"
        );
        assert_eq!(t.net_pnl, Decimal::ZERO);
    }

    #[test]
    fn fallback_closes_are_counted_in_the_headline_and_broken_out_separately() {
        // R7.1: the unverified slice is a SUBSET of net, never subtracted —
        // subtracting it would make the book look better than it is (L3).
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        let mut fallback = close_for("alpha", "hft-1", 1_000, dec!(-10));
        fallback.source = "fallback".into();
        fallback.price_source = "unobserved".into();
        append(&l, &fallback).unwrap();
        append(&l, &close_for("alpha", "hft-2", 2_000, dec!(4))).unwrap();

        let t = l
            .trade_totals(TradeScope::Account(AccountScope {
                account: &AccountId::from("alpha"),
            }))
            .unwrap();
        assert_eq!(
            t.net_pnl,
            dec!(-6),
            "the fallback loss is inside the headline"
        );
        let u = t.unverified.expect("the unverified slice is reported");
        assert_eq!(u.trades, 1);
        assert_eq!(u.net_pnl, dec!(-10));
        // It is a subset: subtracting it gives 4, which is NOT the book's
        // realized PnL and must never be presented as one.
        assert_eq!(t.net_pnl - u.net_pnl, dec!(4));
    }

    #[test]
    fn page_returns_the_newest_window_in_chronological_order() {
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        for i in 1..=5 {
            append(&l, &close(&format!("hft-{i}"), i * 1_000, dec!(1))).unwrap();
        }
        let page = l
            .load_trades(
                TradeScope::AllAccounts,
                0,
                std::num::NonZeroU32::new(2).unwrap(),
            )
            .unwrap();
        // Newest window = hft-5, hft-4; inside the window oldest→newest.
        assert_eq!(
            page.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["hft-4", "hft-5"]
        );
        let next = l
            .load_trades(
                TradeScope::AllAccounts,
                2,
                std::num::NonZeroU32::new(2).unwrap(),
            )
            .unwrap();
        assert_eq!(
            next.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["hft-2", "hft-3"]
        );
        // Past the oldest row is an empty page, NOT an error.
        let past = l
            .load_trades(
                TradeScope::AllAccounts,
                99,
                std::num::NonZeroU32::new(2).unwrap(),
            )
            .unwrap();
        assert!(past.is_empty());
    }

    #[test]
    fn account_scope_pages_only_that_books_rows() {
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        append(&l, &close_for("alpha", "hft-1", 1_000, dec!(1))).unwrap();
        append(&l, &close_for("beta", "hft-2", 2_000, dec!(2))).unwrap();
        append(&l, &close_for("alpha", "hft-3", 3_000, dec!(3))).unwrap();

        let alpha = l
            .load_trades(
                TradeScope::Account(AccountScope {
                    account: &AccountId::from("alpha"),
                }),
                0,
                std::num::NonZeroU32::new(10).unwrap(),
            )
            .unwrap();
        assert_eq!(
            alpha.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["hft-1", "hft-3"]
        );
    }

    #[test]
    fn retract_removes_exactly_one_row_and_reports_absence() {
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        append(&l, &close_for("alpha", "hft-1", 1_000, dec!(1))).unwrap();
        let identity = TradeIdentity {
            id: "hft-1".into(),
            exit_time: 1_000,
        };
        let scope = AccountId::from("alpha");
        l.write(|tx| l.retract_trade(tx, AccountScope { account: &scope }, &identity, 0))
            .expect("first retraction finds the row");
        // Second time: absent is a fact the caller can tell apart from a carrier
        // failure (StoreError::NotFound, not a silent ()).
        let err = l
            .write(|tx| l.retract_trade(tx, AccountScope { account: &scope }, &identity, 0))
            .expect_err("a second retraction has nothing to remove");
        assert!(matches!(err, StoreError::NotFound), "got {err:?}");
        assert_eq!(
            l.trade_totals(TradeScope::AllAccounts)
                .unwrap()
                .total_trades,
            0
        );
    }

    #[test]
    fn retract_cannot_remove_another_accounts_trade() {
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        append(&l, &close_for("alpha", "hft-1", 1_000, dec!(1))).unwrap();
        let beta = AccountId::from("beta");
        let identity = TradeIdentity {
            id: "hft-1".into(),
            exit_time: 1_000,
        };
        let err = l
            .write(|tx| l.retract_trade(tx, AccountScope { account: &beta }, &identity, 0))
            .expect_err("scoped retraction must not reach into another book");
        assert!(matches!(err, StoreError::NotFound));
        assert_eq!(
            l.trade_totals(TradeScope::AllAccounts)
                .unwrap()
                .total_trades,
            1,
            "alpha's row survives"
        );
    }

    #[test]
    fn amend_moves_net_and_gross_and_leaves_fees_alone() {
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        let mut rec = close_for("alpha", "hft-1", 1_000, dec!(-10));
        rec.fees_usd = dec!(0.5);
        append(&l, &rec).unwrap();

        let scope = AccountId::from("alpha");
        let identity = TradeIdentity {
            id: "hft-1".into(),
            exit_time: 1_000,
        };
        l.write(|tx| {
            l.amend_trade_net_pnl(
                tx,
                AccountScope { account: &scope },
                &identity,
                dec!(-10),
                dec!(7), // venue said the real payout was -3
                9_999,
            )
        })
        .expect("the amendment applies");

        let rows = l
            .load_trades(
                TradeScope::AllAccounts,
                0,
                std::num::NonZeroU32::new(5).unwrap(),
            )
            .unwrap();
        assert_eq!(rows[0].net_pnl_usd, dec!(-3));
        assert_eq!(rows[0].gross_pnl_usd, dec!(-3));
        assert_eq!(rows[0].fees_usd, dec!(0.5), "only the payout was estimated");
        // The aggregate leg moved with the payload (they must not disagree).
        assert_eq!(
            l.trade_totals(TradeScope::AllAccounts).unwrap().net_pnl,
            dec!(-3)
        );
    }

    #[test]
    fn amend_refuses_when_the_booked_value_moved() {
        // The compare-and-swap that replaces the JSONL match triple: a second
        // amendment against a stale expectation must not compound.
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        append(&l, &close_for("alpha", "hft-1", 1_000, dec!(-10))).unwrap();
        let scope = AccountId::from("alpha");
        let identity = TradeIdentity {
            id: "hft-1".into(),
            exit_time: 1_000,
        };
        l.write(|tx| {
            l.amend_trade_net_pnl(
                tx,
                AccountScope { account: &scope },
                &identity,
                dec!(-10),
                dec!(7),
                1,
            )
        })
        .unwrap();
        // Second call still believes the booked net is -10.
        let err = l
            .write(|tx| {
                l.amend_trade_net_pnl(
                    tx,
                    AccountScope { account: &scope },
                    &identity,
                    dec!(-10),
                    dec!(7),
                    2,
                )
            })
            .expect_err("a stale expectation is refused");
        let text = format!("{err:?}");
        assert!(
            text.contains("another amendment already moved it"),
            "{text}"
        );
        assert_eq!(
            l.trade_totals(TradeScope::AllAccounts).unwrap().net_pnl,
            dec!(-3),
            "the refused amendment wrote nothing"
        );
    }

    #[test]
    fn amend_of_a_missing_row_is_not_found() {
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        let scope = AccountId::from("alpha");
        let identity = TradeIdentity {
            id: "hft-9".into(),
            exit_time: 1,
        };
        let err = l
            .write(|tx| {
                l.amend_trade_net_pnl(
                    tx,
                    AccountScope { account: &scope },
                    &identity,
                    dec!(1),
                    dec!(1),
                    0,
                )
            })
            .expect_err("no row to amend");
        assert!(matches!(err, StoreError::NotFound));
    }

    #[test]
    fn retract_account_archives_what_it_erases_and_spares_other_accounts() {
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        append(&l, &close_for("alpha", "hft-1", 1_000, dec!(1))).unwrap();
        append(&l, &close_for("alpha", "hft-2", 2_000, dec!(2))).unwrap();
        append(&l, &close_for("beta", "hft-3", 3_000, dec!(3))).unwrap();

        let alpha = AccountId::from("alpha");
        let receipt = l
            .write(|tx| l.retract_account_trades(tx, AccountScope { account: &alpha }, 42))
            .expect("the wipe reports its receipt");
        assert_eq!(receipt.removed, 2);
        assert_eq!(receipt.at_ms, 42);
        assert!(
            receipt.archived_to.contains("trades_archive"),
            "the receipt must say WHERE the rows went, got {}",
            receipt.archived_to
        );

        // The live book lost alpha's rows and kept beta's.
        assert_eq!(
            l.trade_totals(TradeScope::AllAccounts)
                .unwrap()
                .total_trades,
            1
        );
        // The archive kept every erased row (issue #1253).
        let archived: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM trades_archive", [], |r| r.get(0))
            .unwrap();
        assert_eq!(archived, 2, "both erased rows are auditable");
        let archived_net: String = db
            .connection()
            .query_row(
                "SELECT net_pnl_usd FROM trades_archive WHERE id = 'hft-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(archived_net, "1", "the archived row keeps its booked value");
    }

    #[test]
    fn retracting_an_account_that_never_traded_is_a_zero_receipt() {
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        let empty = AccountId::from("never-traded");
        let receipt = l
            .write(|tx| l.retract_account_trades(tx, AccountScope { account: &empty }, 7))
            .expect("an empty reset is a success");
        assert_eq!(receipt.removed, 0);
    }

    #[test]
    fn max_id_tail_sees_every_account_and_ignores_non_hft_ids() {
        // #942: it seeds the position counter for the whole book, so it must not
        // be account-scoped, and a non-hft id must not poison it.
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        append(&l, &close_for("alpha", "hft-7", 1_000, dec!(1))).unwrap();
        append(&l, &close_for("beta", "hft-42", 2_000, dec!(1))).unwrap();
        append(&l, &close_for("beta", "manual-99", 3_000, dec!(1))).unwrap();
        assert_eq!(l.max_trade_id_tail().unwrap(), Some(42));

        // A different book has its own memory.
        let live = log(&db, Mode::Live);
        assert_eq!(live.max_trade_id_tail().unwrap(), None);
    }

    // -- the importer: refusal is the whole point ---------------------------

    fn write_jsonl(
        dir: &std::path::Path,
        name: &str,
        records: &[TradeRecord],
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut body = String::new();
        for rec in records {
            body.push_str(&serde_json::to_string(rec).unwrap());
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bk-{tag}-{}-{}",
            std::process::id(),
            crate::db::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_clean_jsonl_import_lands_every_row_in_one_transaction() {
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        let recs = vec![
            close_for("alpha", "hft-1", 1_000, dec!(1)),
            close_for("alpha", "hft-2", 2_000, dec!(2)),
            close_for("beta", "hft-3", 3_000, dec!(3)),
        ];
        let dir = temp_dir("import-clean");
        let path = write_jsonl(&dir, "trades.jsonl", &recs);

        let report = l.import_jsonl(&path).expect("clean import succeeds");
        assert_eq!(report.imported, 3);
        assert!(report.refused.is_empty());
        assert!(!report.is_refusal());
        assert_eq!(
            l.trade_totals(TradeScope::AllAccounts)
                .unwrap()
                .total_trades,
            3
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_colliding_jsonl_import_writes_nothing_and_names_the_collision() {
        // The measured shape of the real data/trades/trades.jsonl: the same
        // (account, id, exit_time) repeated many times. A naive import hits the
        // primary key here, so the operator gets a refusal list, not a partial
        // write.
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        let mut recs = Vec::new();
        for _ in 0..6 {
            recs.push(close_for("paper", "hft-1", 1_700_000_003_500, dec!(-0.01)));
        }
        recs.push(close_for("paper", "hft-2", 1_700_000_003_500, dec!(0.01)));
        let dir = temp_dir("import-collide");
        let path = write_jsonl(&dir, "trades.jsonl", &recs);

        let report = l.import_jsonl(&path).expect("a refusal is not an io error");
        assert!(report.is_refusal(), "the collision must be reported");
        assert_eq!(report.imported, 0, "NOT ONE row lands on a collision");
        assert_eq!(
            report.refused.len(),
            5,
            "every duplicate after the first is named"
        );
        let lines = report.refusal_lines();
        assert!(
            lines
                .iter()
                .all(|l| l.contains("duplicate (account, id, exit_time)")),
            "the detail must say what collided: {lines:?}"
        );
        // The decisive assertion: the ledger is untouched.
        assert_eq!(
            l.trade_totals(TradeScope::AllAccounts)
                .unwrap()
                .total_trades,
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reimporting_an_already_imported_file_is_refused_not_silently_skipped() {
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        let recs = vec![close_for("alpha", "hft-1", 1_000, dec!(1))];
        let dir = temp_dir("import-rerun");
        let path = write_jsonl(&dir, "trades.jsonl", &recs);

        assert_eq!(l.import_jsonl(&path).unwrap().imported, 1);
        let second = l.import_jsonl(&path).expect("re-run reports, not errors");
        assert!(second.is_refusal());
        assert_eq!(second.imported, 0);
        assert!(
            second.refused[0]
                .reason
                .contains("already in the dry ledger"),
            "{}",
            second.refused[0].reason
        );
        assert_eq!(
            l.trade_totals(TradeScope::AllAccounts)
                .unwrap()
                .total_trades,
            1,
            "the ledger still holds exactly one row"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unparseable_line_refuses_the_whole_import() {
        // A half-import is worse than no import: the operator cannot tell which
        // history is missing.
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        let dir = temp_dir("import-broken");
        let path = dir.join("trades.jsonl");
        let good = serde_json::to_string(&close_for("alpha", "hft-1", 1_000, dec!(1))).unwrap();
        std::fs::write(&path, format!("{good}\n{{not json\n")).unwrap();

        let report = l.import_jsonl(&path).unwrap();
        assert!(report.is_refusal());
        assert_eq!(report.imported, 0);
        assert_eq!(report.refused[0].line, 2);
        assert!(report.refused[0].reason.contains("unparseable"));
        assert_eq!(
            l.trade_totals(TradeScope::AllAccounts)
                .unwrap()
                .total_trades,
            0,
            "the parseable line did NOT land on its own"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_writes_a_chronological_node_compatible_file_and_reimports() {
        let db = TempDb::new();
        let l = log(&db, Mode::Dry);
        append(&l, &close_for("alpha", "hft-1", 1_000, dec!(1))).unwrap();
        append(&l, &close_for("alpha", "hft-2", 2_000, dec!(2))).unwrap();
        let dir = temp_dir("export");
        let path = dir.join("export.jsonl");
        let n = l.export_jsonl(&path).expect("export lands");
        assert_eq!(n, 2);

        let body = std::fs::read_to_string(&path).unwrap();
        let mut ids = Vec::new();
        for line in body.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            // camelCase wire shape, unchanged from the JSONL era
            assert!(v.get("netPnlUsd").is_some(), "Node-compatible shape");
            ids.push(v["id"].as_str().unwrap().to_string());
        }
        assert_eq!(ids, vec!["hft-1", "hft-2"], "chronological, oldest first");

        // And the export is re-importable into a fresh book (round trip).
        let db2 = TempDb::new();
        let l2 = log(&db2, Mode::Dry);
        let report = l2.import_jsonl(&path).unwrap();
        assert_eq!(report.imported, 2);
        assert_eq!(
            l2.trade_totals(TradeScope::AllAccounts).unwrap().net_pnl,
            dec!(3)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_fallback_reconciled_persists_to_sqlite_without_mutating_pnl() {
        let db = TempDb::new();
        let l = log(&db, Mode::Live);
        let mut fb_cond = close_for("polysimulator", "hft-10", 1_000, dec!(-8.59));
        fb_cond.condition_id = "cond-a".into();
        fb_cond.token_id = "tok-a".into();
        fb_cond.source = "fallback".into();
        let mut fb_tok = close_for("polysimulator", "hft-11", 2_000, dec!(-3.33));
        fb_tok.condition_id = "cond-b".into();
        fb_tok.token_id = "tok-b".into();
        fb_tok.source = "fallback".into();
        let mut venue_row = close_for("polysimulator", "hft-12", 3_000, dec!(1.20));
        venue_row.condition_id = "cond-a".into();
        venue_row.token_id = "tok-a".into();
        venue_row.source = "venue".into();
        append(&l, &fb_cond).unwrap();
        append(&l, &fb_tok).unwrap();
        append(&l, &venue_row).unwrap();

        let flipped_cond = l
            .mark_fallback_reconciled_by_condition("cond-a", 9_000)
            .unwrap();
        assert_eq!(flipped_cond, 1);
        assert_eq!(
            l.mark_fallback_reconciled_by_condition("cond-a", 9_001)
                .unwrap(),
            0
        );

        let mut tokens = std::collections::HashSet::new();
        tokens.insert("tok-b");
        let flipped_tok = l
            .mark_fallback_reconciled_by_tokens(&tokens, 9_002)
            .unwrap();
        assert_eq!(flipped_tok, 1);

        let rows = l
            .load_trades(
                TradeScope::AllAccounts,
                0,
                std::num::NonZeroU32::new(10).unwrap(),
            )
            .unwrap();
        let by_id: std::collections::HashMap<&str, &TradeRecord> =
            rows.iter().map(|r| (r.id.as_str(), r)).collect();
        assert_eq!(by_id["hft-10"].source, "reconciled");
        assert_eq!(by_id["hft-10"].net_pnl_usd, dec!(-8.59));
        assert_eq!(by_id["hft-11"].source, "reconciled");
        assert_eq!(by_id["hft-12"].source, "venue");
        assert_eq!(
            l.trade_totals(TradeScope::AllAccounts).unwrap().unverified,
            None,
            "reconciled rows are no longer counted as unverified fallback"
        );
    }
}
