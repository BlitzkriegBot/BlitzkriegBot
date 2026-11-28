//! Durable replay records: the OME's applied-fill idempotency journal and
//! the in-kernel accounting audit state. Moved verbatim from `config.rs`
//! (pure code motion, no logic edits).

use crate::ome::AppliedFillRecord;
use crate::reconcile::{AuditReport, CashIdentity};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::path::PathBuf;

/// Append-only journal of the OME's applied-fill idempotency table (issue #178).
///
/// The table used to be memory-only, so a restart forgot which venue trade ids
/// had already been booked and the venue's own re-delivery (a WS replay or the
/// REST sweep's recent-trade window) booked the same fill a second time — the
/// position doubled and the cash moved twice. Rebuilding it from `trades.jsonl`
/// is not possible: that log stores ONE AGGREGATED record per CLOSED position and
/// carries no venue trade ids at all, so there is nothing to fold a fill key from.
///
/// Same shape as the order log: one line per MUTATION (a key's applied size, or
/// a tombstone when it is evicted), folded latest-wins on load. The file lives
/// beside the order log because it is the OME's state, and it is compacted to the
/// live table on restore so it cannot grow across restarts.
pub struct AppliedFillLog {
    pub path: PathBuf,
    write_failed: std::sync::atomic::AtomicBool,
}

impl AppliedFillLog {
    pub fn new(path: impl AsRef<std::path::Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            write_failed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Append the mutations to disk. Fail-closed (#277): returns `false` and
    /// latches `write_failed` if the file cannot be opened or written, so the
    /// caller refuses booking the fill and blocks further entries.
    pub fn append(&self, records: &[AppliedFillRecord]) -> bool {
        if records.is_empty() {
            return true;
        }
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut buf = String::new();
        for r in records {
            match serde_json::to_string(r) {
                Ok(line) => {
                    buf.push_str(&line);
                    buf.push('\n');
                }
                Err(e) => {
                    self.write_failed
                        .store(true, std::sync::atomic::Ordering::Release);
                    tracing::error!(error = %e, "cannot serialize applied-fill record — durability fault latched");
                    return false;
                }
            }
        }
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            if let Err(e) = f.write_all(buf.as_bytes()) {
                self.write_failed
                    .store(true, std::sync::atomic::Ordering::Release);
                tracing::error!(error = %e, path = %self.path.display(), "applied-fill log append failed — durability fault latched");
                return false;
            }
            true
        } else {
            self.write_failed
                .store(true, std::sync::atomic::Ordering::Release);
            tracing::error!(path = %self.path.display(), "cannot open applied-fill log — durability fault latched");
            false
        }
    }

    pub fn write_failed(&self) -> bool {
        self.write_failed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Load the mutations in file order (the OME folds them). Unparseable lines
    /// are skipped, never fatal: a corrupt tail must not stop the core from
    /// starting, and every record kept is a duplicate the table will refuse.
    pub fn load(&self) -> Vec<AppliedFillRecord> {
        crate::jsonl::load::<AppliedFillRecord>(
            &self.path,
            "applied-fill log: skipped unparseable lines",
        )
    }

    /// Rewrite the log as the current table (one line per live key). Called after
    /// a restore, so the file is the table rather than every mutation ever made.
    pub fn compact(&self, snapshot: &[AppliedFillRecord]) {
        let mut buf = String::new();
        for r in snapshot {
            match serde_json::to_string(r) {
                Ok(line) => {
                    buf.push_str(&line);
                    buf.push('\n');
                }
                Err(e) => tracing::warn!(error = %e, "cannot serialize applied-fill record"),
            }
        }
        // #463 / #277: atomic rewrite — latch durability fault if rewrite fails.
        if !crate::jsonl::rewrite_atomic(&self.path, buf.as_bytes()) {
            self.write_failed
                .store(true, std::sync::atomic::Ordering::Release);
            tracing::error!(path = %self.path.display(), "applied-fill log compact failed — durability fault latched");
        }
    }
}

/// In-kernel accounting audit state (issue #189). Session-scoped: the anchor and
/// the strike counter describe THIS run's observations, and the durable record is
/// the audit JSONL.
#[derive(Debug, Clone, Default)]
pub struct AuditRuntime {
    /// Last audit instant (0 = never).
    pub last_at_ms: i64,
    /// Last audit that was written to the log even though it passed (heartbeat).
    pub last_heartbeat_ms: i64,
    /// The baseline the anchored cash identity is measured from (active account).
    /// Taken at the first audit whose structural checks pass and NEVER moved
    /// while the books are wrong — re-anchoring would hide the drift it exists
    /// to expose.
    pub anchor: Option<CashIdentity>,
    /// Per-account baselines so `run_accounting_audit` audits EVERY active
    /// account (not only `active_id` — e.g. `polysimulator` when `active_id`
    /// is an empty `default` book).
    pub anchors_by_account: HashMap<String, CashIdentity>,
    pub report: Option<AuditReport>,
    /// Consecutive cross-source-only failures (active account); two in a row
    /// halt (a single one can be a stale venue read).
    pub cross_source_failures: u32,
    /// Per-account consecutive cross-source-only failure counts.
    pub cross_source_failures_by_account: HashMap<String, u32>,
    /// New entries are currently blocked by a failing audit (any account).
    pub halted: bool,
    pub halt_reason: String,
    /// Per-account audit halts (`account_id -> halt_reason`).
    pub halted_accounts: HashMap<String, String>,
    pub runs: u64,
    pub failures: u64,
    /// Where each audit appends its verdict (None = no persistence).
    pub audit_log_path: Option<String>,
    /// The last time each venue's cash was SET from that venue rather than
    /// moved by a fill: venue → `(at_ms, delta)`. The plugin host does this
    /// deliberately when nothing rests, so the trade-leg check can tell the
    /// venue's own correction from a local inconsistency instead of halting
    /// entries over it. Keyed by venue: a realign explains a residual only on
    /// the book that venue's cash belongs to.
    pub last_realign: HashMap<String, (i64, Decimal)>,
    /// Issue #921: the audit-instant at which a venue's standing correction was
    /// last CLAIMED as the explanation for a trade-leg residual (venue →
    /// at_ms). Present = the correction is spent: it cannot excuse a second
    /// residual until the venue overwrites the slot with a fresher correction
    /// (which clears this entry below in `realign_venue_cash`). Mirrors
    /// `AuditInput.realign_explained_at` into the per-venue runtime state.
    pub realign_explained: HashMap<String, i64>,
}
