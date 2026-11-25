//! The durable order-intent ledger (issue #185).
//!
//! # The defect this closes
//!
//! Idempotency used to be MEMORY state, so it evaporated exactly when it was
//! most needed:
//!
//! * the OME's `internal_key` dedup (`Ome::has_pending`) only refuses while the
//!   FIRST order is still `is_live()` — the moment it goes terminal (cancel,
//!   reject, force-close) the key is free again;
//! * a Lua package's own `quoted` / `slot_seen` tables live inside the loaded
//!   process, so `strategy.reload` (or any restart) starts them empty;
//! * the `orders` table could never have blocked a duplicate intent: its key
//!   was the locally-minted order id (and v2 dropped the table entirely — the
//!   runtime order truth is `data/orders.jsonl`).
//!
//! The combination that loses real money: cancel/reject + `strategy.reload` +
//! still inside the same 45–60 s round window → the same
//! `(strategy, asset, direction, slot)` intent places a SECOND order.
//!
//! # The structure
//!
//! One row per intent identity
//! `(account_id, strategy, condition_id, round_slot, direction)` — the
//! `uq_intent` unique index from the #185 contract. The claim happens BEFORE
//! any ledger reservation or OME registration, and it records the FIRST
//! submit's real outcome:
//!
//! * `Reserved` — the claim exists, the submit has not finished yet. A
//!   concurrent duplicate sees this and is refused (`#185`'s concurrency
//!   contract: exactly one submit actually places).
//! * `Ordered(order_id, status)` — the first submit placed this order. A
//!   duplicate REPLAYS that exact id + status; it never places again.
//! * `Consumed` / `Abandoned` — terminal forms for a row whose round is over
//!   or whose submit was abandoned before reaching the OME.
//!
//! # Capacity (bounded, explicit)
//!
//! Both bounds are enforced inside the claim transaction, so the table cannot
//! grow without limit:
//!
//! * [`INTENT_RETENTION_MS`] — rows older than this are deleted. It is far
//!   longer than any round window (the longest family is 1 day) and it is a
//!   dedup window, not an audit trail: the order log is the audit trail.
//! * [`INTENT_MAX_ROWS`] — a hard row cap; the OLDEST rows are deleted down
//!   to it (LRU by age). The newest intents are the ones a retry can still
//!   collide with, so age is the right eviction order.
//!
//! A prune failure never blocks a claim (a table above its cap is still
//! CORRECT, only large); it is logged and reported in the claim result so an
//! operator sees the degradation instead of a silent one (L4).
//!
//! # What this ledger is NOT
//!
//! It is not a money book. It carries an order id and a refusal reason — the
//! facts the first caller already received. Nothing here is ever read as cash,
//! shares or PnL, and losing every row costs idempotency, never accounting
//! truth.

use crate::db::Database;
use blitzkrieg_market_api::CoreErrorCode;
use rusqlite::{OptionalExtension, params};

/// How long an intent row is retained before the age window prunes it.
///
/// One day: longer than the longest shipped round family (86_400 s), so a
/// retry inside any live round window is still deduped, and short enough that
/// the table describes recent intents rather than all history.
pub const INTENT_RETENTION_MS: i64 = 86_400_000;

/// Hard row cap. Reached only by a kernel placing faster than the age window
/// retires: ~5k intents is weeks of ordinary round trading, and the oldest
/// rows are the ones past every live window.
pub const INTENT_MAX_ROWS: i64 = 5_000;

/// The lifecycle state of one claimed intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentState {
    /// Claimed; the submit has not recorded an outcome yet.
    Reserved,
    /// The first submit placed `order_id` and saw `status`. `rejection` is the
    /// kernel's own post-submit refusal, when the order came back Rejected —
    /// replaying it is what makes the duplicate's response byte-identical.
    ///
    /// A refusal that produced NO order never reaches the ledger at all: the
    /// ledger is claimed after every refusal gate, precisely so a gate-refused
    /// attempt leaves the intent retryable (a stale refusal must not lock a
    /// round window out).
    Ordered {
        order_id: String,
        status: String,
        rejection: Option<IntentRejection>,
    },
    /// The intent was superseded (an explicit #848 requote replaced its order).
    /// A later attempt may claim it again.
    Consumed,
    /// The claim was released without an outcome (a submit that never reached
    /// the OME). A later attempt may claim the intent again.
    Abandoned,
}

/// One replayed refusal: the structured code plus the exact message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentRejection {
    /// The `CoreErrorCode`'s wire spelling (`RISK_REJECTED`, …).
    pub code: String,
    pub message: String,
}

impl IntentState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Ordered { .. } => "ordered",
            Self::Consumed => "consumed",
            Self::Abandoned => "abandoned",
        }
    }
}

/// One intent identity: the `uq_intent` key from the #185 contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentKey {
    pub account_id: String,
    pub strategy: String,
    pub condition_id: String,
    pub round_slot: i64,
    pub direction: String,
}

impl IntentKey {
    pub fn new(
        account_id: impl Into<String>,
        strategy: impl Into<String>,
        condition_id: impl Into<String>,
        round_slot: i64,
        direction: impl Into<String>,
    ) -> Self {
        Self {
            account_id: account_id.into(),
            strategy: strategy.into(),
            condition_id: condition_id.into(),
            round_slot,
            direction: direction.into(),
        }
    }

    /// The intent identity as one human-greppable string, in the order log's
    /// `internal_key` shape where possible — an operator must be able to join
    /// the intent and the order by eye.
    pub fn internal_spelling(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            self.account_id, self.strategy, self.condition_id, self.round_slot, self.direction
        )
    }
}

/// An `OrderStatus`'s wire spelling (`SCREAMING_SNAKE_CASE`, the serde form).
pub fn order_status_wire(status: crate::model::OrderStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{status:?}"))
}

/// Parse a stored status back. `None` = a spelling this build does not know.
pub fn parse_order_status(raw: &str) -> Option<crate::model::OrderStatus> {
    serde_json::from_value(serde_json::Value::String(raw.to_string())).ok()
}

/// The result of a claim attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentClaim {
    /// This caller owns the intent; it must submit and then record the
    /// outcome ([`IntentLedger::record_ordered`] / [`IntentLedger::release`]).
    Claimed,
    /// Another submit already owns this intent and reached one of the replayable
    /// outcomes. The caller must answer with THIS — never a fresh response.
    Existing(IntentState),
}

/// The durable intent ledger over one SQLite connection.
#[derive(Clone)]
pub struct IntentLedger {
    db: Database,
}

impl IntentLedger {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub fn database(&self) -> &Database {
        &self.db
    }

    /// Claim the intent, pruning the table to its bounds in the same
    /// transaction.
    ///
    /// `INSERT OR IGNORE` against the unique key is the whole gate: exactly
    /// one concurrent caller can insert, everyone else reads the row that won.
    /// This is a real uniqueness constraint in ONE durable store, not a
    /// process-local `HashMap` — it holds across `strategy.reload` AND across
    /// a restart.
    ///
    /// Errors are returned as `Err(reason)`: a ledger that cannot answer must
    /// not be treated as "no existing intent" (§3/L4 — fail-closed, never a
    /// silent default). The caller decides the posture; the placement path
    /// refuses the entry.
    pub fn claim(&self, key: &IntentKey, now_ms: i64) -> Result<IntentClaim, String> {
        self.db
            .with_connection(|conn| -> Result<IntentClaim, String> {
                let tx = conn
                    .unchecked_transaction()
                    .map_err(|e| format!("intent claim: begin failed: {e}"))?;
                let inserted = tx
                    .execute(
                        "INSERT OR IGNORE INTO order_intents \
                         (account_id, strategy, condition_id, round_slot, direction, \
                          state, order_id, reject_code, reject_message, order_status, \
                          created_at_ms, updated_at_ms) \
                         VALUES (?1, ?2, ?3, ?4, ?5, 'reserved', NULL, NULL, NULL, NULL, ?6, ?6)",
                        params![
                            key.account_id,
                            key.strategy,
                            key.condition_id,
                            key.round_slot,
                            key.direction,
                            now_ms,
                        ],
                    )
                    .map_err(|e| format!("intent claim: insert failed: {e}"))?;
                let outcome = if inserted == 1 {
                    IntentClaim::Claimed
                } else {
                    let state = read_state(&tx, key)
                        .map_err(|e| format!("intent claim: read failed: {e}"))?
                        .ok_or_else(|| {
                            // The insert was ignored, so a row MUST exist: a
                            // vanished row would mean the unique key is not the
                            // one we read (or a concurrent DELETE). Say so
                            // instead of calling it free.
                            "intent claim: insert ignored but no row found — refusing to \
                             treat the intent as unclaimed"
                                .to_string()
                        })?;
                    match state {
                        // A released/superseded claim carries NO outcome, so
                        // the intent is claimable again — the same transaction
                        // re-arms the row (the unique key never leaves, so a
                        // concurrent duplicate still loses the race below).
                        IntentState::Consumed | IntentState::Abandoned => {
                            let n = tx
                                .execute(
                                    "UPDATE order_intents SET state = 'reserved', order_id = NULL, \
                                     reject_code = NULL, reject_message = NULL, order_status = NULL, \
                                     created_at_ms = ?6, updated_at_ms = ?6 \
                                     WHERE account_id = ?1 AND strategy = ?2 AND condition_id = ?3 \
                                       AND round_slot = ?4 AND direction = ?5 \
                                       AND state IN ('consumed', 'abandoned')",
                                    params![
                                        key.account_id,
                                        key.strategy,
                                        key.condition_id,
                                        key.round_slot,
                                        key.direction,
                                        now_ms,
                                    ],
                                )
                                .map_err(|e| format!("intent claim: re-arm failed: {e}"))?;
                            if n == 1 {
                                IntentClaim::Claimed
                            } else {
                                // Another caller re-armed or recorded it
                                // between our read and this update (a race the
                                // single connection makes unlikely, but the
                                // answer must stay honest): report the row as
                                // it stands now.
                                let now = read_state(&tx, key)
                                    .map_err(|e| format!("intent claim: read failed: {e}"))?
                                    .unwrap_or(IntentState::Reserved);
                                IntentClaim::Existing(now)
                            }
                        }
                        other => IntentClaim::Existing(other),
                    }
                };
                prune(&tx, now_ms)?;
                tx.commit()
                    .map_err(|e| format!("intent claim: commit failed: {e}"))?;
                Ok(outcome)
            })
    }

    /// Record the first submit's real result: the order it placed, plus the
    /// kernel's own post-submit refusal when it had one (a dry taker the book
    /// could not fill answers `Rejected` WITH a structured reason — a duplicate
    /// must replay that reason, not a hollow status).
    pub fn record_ordered(
        &self,
        key: &IntentKey,
        order_id: &str,
        status: &str,
        rejection: Option<(&str, &str)>,
        now_ms: i64,
    ) -> Result<(), String> {
        let (reject_code, reject_message) = match rejection {
            Some((c, m)) => (Some(c), Some(m)),
            None => (None, None),
        };
        self.db.with_connection(|conn| {
            let n = conn
                .execute(
                    "UPDATE order_intents SET state = 'ordered', order_id = ?6, \
                         order_status = ?7, reject_code = ?8, reject_message = ?9, \
                         updated_at_ms = ?10 \
                         WHERE account_id = ?1 AND strategy = ?2 AND condition_id = ?3 \
                           AND round_slot = ?4 AND direction = ?5 AND state = 'reserved'",
                    params![
                        key.account_id,
                        key.strategy,
                        key.condition_id,
                        key.round_slot,
                        key.direction,
                        order_id,
                        status,
                        reject_code,
                        reject_message,
                        now_ms,
                    ],
                )
                .map_err(|e| format!("intent record_ordered: {e}"))?;
            if n != 1 {
                // The row is not 'reserved': either it was already
                // recorded (a re-entrant call) or it was never claimed. The
                // first is harmless; the second means the caller skipped
                // the claim — say so, do not pretend.
                let state = read_state(conn, key).map_err(|e| e.to_string())?;
                return Err(format!(
                    "intent record_ordered: no reserved row to update (state: {state:?}) — \
                         the claim was not taken"
                ));
            }
            Ok(())
        })
    }

    /// Mark a claim SUPERSEDED: the order it recorded was retired by the #848
    /// replace, so the replacement must be recordable. Only an `ordered` row is
    /// consumed; a `refused` row stays (the refusal is still the first result
    /// of that intent until a new claim supersedes it).
    pub fn mark_consumed(&self, key: &IntentKey, now_ms: i64) -> Result<(), String> {
        self.db.with_connection(|conn| {
            conn.execute(
                "UPDATE order_intents SET state = 'consumed', updated_at_ms = ?6 \
                 WHERE account_id = ?1 AND strategy = ?2 AND condition_id = ?3 \
                   AND round_slot = ?4 AND direction = ?5 AND state = 'ordered'",
                params![
                    key.account_id,
                    key.strategy,
                    key.condition_id,
                    key.round_slot,
                    key.direction,
                    now_ms,
                ],
            )
            .map_err(|e| format!("intent mark_consumed: {e}"))?;
            Ok(())
        })
    }

    /// Release a claim that produced no outcome (the submit never reached the
    /// OME — a rolled-back reservation, a pre-submit error). A later attempt
    /// inside the same window may claim the intent again.
    ///
    /// Only a `reserved` row is released: an `ordered`/`refused` row is the
    /// first result and must stay replayable.
    pub fn release(&self, key: &IntentKey, now_ms: i64) -> Result<(), String> {
        self.db.with_connection(|conn| {
            conn.execute(
                "UPDATE order_intents SET state = 'abandoned', updated_at_ms = ?6 \
                 WHERE account_id = ?1 AND strategy = ?2 AND condition_id = ?3 \
                   AND round_slot = ?4 AND direction = ?5 AND state = 'reserved'",
                params![
                    key.account_id,
                    key.strategy,
                    key.condition_id,
                    key.round_slot,
                    key.direction,
                    now_ms,
                ],
            )
            .map_err(|e| format!("intent release: {e}"))?;
            Ok(())
        })
    }

    /// Read one intent's state (`None` = never claimed, or pruned away).
    pub fn state(&self, key: &IntentKey) -> Result<Option<IntentState>, String> {
        self.db
            .with_connection(|conn| read_state(conn, key).map_err(|e| e.to_string()))
    }

    /// Current row count (diagnostics / tests).
    ///
    /// `clippy::len_without_is_empty` is allowed rather than satisfied. The
    /// lint assumes a `len()` over an in-memory collection the same type can
    /// answer `is_empty()` for without I/O. Both calls here hit SQLite and
    /// both return `Result`, so an `is_empty()` would be a second round-trip
    /// that exists only to appease a heuristic; a caller who wants emptiness
    /// can compare this count. Adding it would be the exact shape this audit
    /// keeps flagging — an API written for the linter, not for the code.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> Result<i64, String> {
        self.db.with_connection(|conn| {
            conn.query_row("SELECT COUNT(*) FROM order_intents", [], |r| r.get(0))
                .map_err(|e| e.to_string())
        })
    }
}

/// The `CoreErrorCode`'s wire spelling (`RiskRejected` → `RISK_REJECTED`), so a
/// replay can rebuild the same code the first caller received.
pub fn wire_code(code: CoreErrorCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{code:?}"))
}

/// Parse a stored wire code back. `None` = not a code this build knows (a
/// downgrade or a corrupt cell); the caller reports that as unknown rather than
/// guessing a code (L4).
pub fn parse_wire_code(raw: &str) -> Option<CoreErrorCode> {
    serde_json::from_value(serde_json::Value::String(raw.to_string())).ok()
}

fn read_state(
    conn: &rusqlite::Connection,
    key: &IntentKey,
) -> rusqlite::Result<Option<IntentState>> {
    let row = conn
        .query_row(
            "SELECT state, order_id, order_status, reject_code, reject_message \
             FROM order_intents \
             WHERE account_id = ?1 AND strategy = ?2 AND condition_id = ?3 \
               AND round_slot = ?4 AND direction = ?5",
            params![
                key.account_id,
                key.strategy,
                key.condition_id,
                key.round_slot,
                key.direction,
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.and_then(|(state, order_id, order_status, code, message)| {
        match state.as_str() {
            "reserved" => Some(IntentState::Reserved),
            "ordered" => Some(IntentState::Ordered {
                order_id: order_id?,
                status: order_status.unwrap_or_else(|| "Pending".to_string()),
                rejection: match (code, message) {
                    (Some(code), Some(message)) => Some(IntentRejection { code, message }),
                    _ => None,
                },
            }),
            "consumed" => Some(IntentState::Consumed),
            "abandoned" => Some(IntentState::Abandoned),
            // An unknown state string is a schema/code mismatch, not "no
            // row": reporting it as Reserved keeps the intent BLOCKED (a
            // duplicate cannot submit) and the log line names the mismatch.
            other => {
                tracing::error!(
                    state = other,
                    "intent ledger holds an unknown state — treating the intent as still claimed"
                );
                Some(IntentState::Reserved)
            }
        }
    }))
}

/// Enforce the two capacity bounds. Runs inside the claim transaction so the
/// table can never exceed them by more than the rows this transaction added.
fn prune(conn: &rusqlite::Connection, now_ms: i64) -> Result<(), String> {
    conn.execute(
        "DELETE FROM order_intents WHERE created_at_ms < ?1",
        params![now_ms.saturating_sub(INTENT_RETENTION_MS)],
    )
    .map_err(|e| format!("intent prune (age): {e}"))?;
    conn.execute(
        "DELETE FROM order_intents WHERE rowid IN (
             SELECT rowid FROM order_intents
             ORDER BY created_at_ms ASC, rowid ASC
             LIMIT MAX(0, (SELECT COUNT(*) FROM order_intents) - ?1)
         )",
        params![INTENT_MAX_ROWS],
    )
    .map_err(|e| format!("intent prune (cap): {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::TempDb;

    fn ledger(_name: &str) -> (TempDb, IntentLedger) {
        let db = TempDb::new();
        let l = IntentLedger::new(db.db().clone());
        (db, l)
    }

    fn key() -> IntentKey {
        IntentKey::new("default", "late_favorite", "cond-1", 1_795_230_000, "up")
    }

    #[test]
    fn a_second_claim_of_the_same_intent_sees_the_first() {
        let (_db, l) = ledger("claim");
        assert_eq!(l.claim(&key(), 1_000).unwrap(), IntentClaim::Claimed);
        l.record_ordered(&key(), "dry_1", "Filled", None, 1_001)
            .unwrap();
        assert_eq!(
            l.claim(&key(), 1_002).unwrap(),
            IntentClaim::Existing(IntentState::Ordered {
                order_id: "dry_1".into(),
                status: "Filled".into(),
                rejection: None,
            })
        );
        assert_eq!(l.len().unwrap(), 1, "one intent, one row");
    }

    #[test]
    fn a_concurrent_claim_while_reserved_is_refused_not_claimed() {
        let (_db, l) = ledger("concurrent");
        assert_eq!(l.claim(&key(), 1_000).unwrap(), IntentClaim::Claimed);
        // The first submit has not recorded an outcome yet — the second caller
        // must NOT submit too.
        assert_eq!(
            l.claim(&key(), 1_001).unwrap(),
            IntentClaim::Existing(IntentState::Reserved)
        );
        assert_eq!(l.len().unwrap(), 1);
    }

    #[test]
    fn a_released_claim_can_be_claimed_again() {
        let (_db, l) = ledger("release");
        l.claim(&key(), 1_000).unwrap();
        l.release(&key(), 1_001).unwrap();
        assert_eq!(l.claim(&key(), 1_002).unwrap(), IntentClaim::Claimed);
    }

    #[test]
    fn different_directions_are_different_intents() {
        let (_db, l) = ledger("directions");
        let mut other = key();
        other.direction = "down".into();
        assert_eq!(l.claim(&key(), 1_000).unwrap(), IntentClaim::Claimed);
        assert_eq!(l.claim(&other, 1_000).unwrap(), IntentClaim::Claimed);
        assert_eq!(l.len().unwrap(), 2);
    }

    #[test]
    fn the_age_window_prunes_old_rows_outside_transaction() {
        let (_db, l) = ledger("age");
        let old = 1_000_000;
        assert_eq!(l.claim(&key(), old).unwrap(), IntentClaim::Claimed);
        // A claim a full retention window later prunes the old row.
        let later = old + INTENT_RETENTION_MS + 1;
        let mut fresh = key();
        fresh.round_slot += 1;
        assert_eq!(l.claim(&fresh, later).unwrap(), IntentClaim::Claimed);
        assert_eq!(l.len().unwrap(), 1, "the aged row left the window");
        assert_eq!(l.state(&key()).unwrap(), None);
    }
}
