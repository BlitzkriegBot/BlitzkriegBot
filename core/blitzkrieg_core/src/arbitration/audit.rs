//! The audit trail (DEV_V0_3 §3.4): every suggestion, approved or rejected,
//! lands as ONE line in `data/audit/intents.jsonl`.
//!
//! Rules the design fixes here:
//! * one JSON line per intent, appended with `O_APPEND` so concurrent
//!   appenders can never fuse two records (#386); a failed write costs one
//!   `tracing::warn!`, never a trade;
//! * rotation at 256 MB per segment: the full segment is renamed to a
//!   UTC-stamped sibling (`intents.jsonl` → `intents.20261116T210000Z.jsonl`)
//!   and the next intent starts a fresh file, so the log cannot grow without
//!   bound (#1199 P1-11). Rotated segments are kept on disk; the audit sink
//!   itself has no disk floor — the event archive owns deletion policy;
//! * `INTENT_DECISION` pushes are THROTTLED, per `(strategy, gate, reason)`
//!   key, to at most one per second — a rejection storm can produce thousands
//!   of intents a second, and the push is folded, never the audit (the audit
//!   stays one line per intent).

use super::{Decision, GateTrace};
use crate::data_source::next_segment_path;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;

/// 256 MB per intent-audit segment — mirrors the event archive's segment
/// discipline (`config.event_archive_rotate_mb`, DEV §5.3). Fixed, not a knob:
/// an un-rotated 566 MB audit log is exactly what #1199 P1-11 found in
/// production.
const INTENT_ROTATE_BYTES: u64 = 256 * 1024 * 1024;

/// One audited suggestion, exactly the §3.4 shape. The serde spelling here is
/// load-bearing: `intent.audit.tail` filters on `accountId` / `strategy` /
/// `decision.status`, and the panel reads `gates[].detail` verbatim (§13.4).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntentAuditRecord {
    pub ts_ms: i64,
    pub account_id: String,
    pub strategy: String,
    pub intent_id: String,
    /// The suggestion as submitted (reserved keys already stripped, §2.4).
    pub intent: serde_json::Value,
    pub decision: Decision,
    pub gates: Vec<GateTrace>,
    pub latency_us: u64,
}

/// Where the pushes fold. `throttle` returns `Some(folded_count)` when the
/// caller may emit ONE push for the key now — every suppressed decision is
/// still audited; only the push is folded.
#[derive(Debug)]
struct ThrottleState {
    /// `i64::MIN` = never sent (0 is a legitimate timestamp, and a test that
    /// starts its clock at 0 must still fold).
    last_sent_ms: i64,
    folded: u64,
}

/// The audit sink: sequence numbers, the JSONL writer, and the push throttle.
/// Owned by `Core`; `enabled = false` (`--no-intent-audit`) silences only the
/// WRITES — the gates themselves keep running (P1 proves both settings hit
/// the same economic baseline).
#[derive(Debug)]
pub struct AuditSink {
    pub enabled: bool,
    pub path: PathBuf,
    pub account_id: String,
    next_seq: u64,
    throttles: HashMap<(String, String, String), ThrottleState>,
    /// Size of the segment currently being written, in bytes. Lazily seeded
    /// from the actual file on the first write (a pre-oversized file rotates
    /// on the very first record), and updated on every successful append.
    segment_bytes: u64,
    /// Whether `segment_bytes` has been seeded from disk yet.
    segment_seeded: bool,
    /// Per-segment ceiling: 256 MB (see `INTENT_ROTATE_BYTES`).
    rotate_bytes: u64,
}

/// The one push the throttle lets through, ready for the `INTENT_DECISION`
/// event (§12.3).
#[derive(Debug, Clone)]
pub struct DecisionPush {
    pub strategy: String,
    pub intent_id: String,
    pub status: String,
    pub gate: String,
    pub detail: String,
    pub ts_ms: i64,
    /// How many decisions this single push summarizes (≥ 1). Additive field
    /// on top of the §12.3 example — the fold has to be observable.
    pub count: u64,
}

impl AuditSink {
    pub fn new(account_id: impl Into<String>) -> Self {
        Self {
            enabled: true,
            path: PathBuf::from("data/audit/intents.jsonl"),
            account_id: account_id.into(),
            next_seq: 0,
            throttles: HashMap::new(),
            segment_bytes: 0,
            segment_seeded: false,
            rotate_bytes: INTENT_ROTATE_BYTES,
        }
    }

    /// One intent id per audited suggestion (unique within the process).
    fn next_intent_id(&mut self) -> String {
        self.next_seq += 1;
        format!("i{:08}", self.next_seq)
    }

    /// Build the record for one arbitrated suggestion and append it. Returns
    /// the record (the caller emits the throttled push from the same fields);
    /// a failed append warns ONCE and never blocks the trade.
    ///
    /// E28: `account_id` is the account the SUGGESTION itself names (§9.2 —
    /// the record attributes the decision to the account whose money was
    /// judged), stamped per call rather than fixed at sink construction.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        ts_ms: i64,
        account_id: &str,
        strategy: &str,
        intent: &serde_json::Value,
        decision: &Decision,
        gates: &[GateTrace],
        latency_us: u64,
    ) -> IntentAuditRecord {
        let record = IntentAuditRecord {
            ts_ms,
            account_id: account_id.to_string(),
            strategy: strategy.to_string(),
            intent_id: self.next_intent_id(),
            intent: intent.clone(),
            decision: decision.clone(),
            gates: gates.to_vec(),
            latency_us,
        };
        if self.enabled {
            let mut buf = match serde_json::to_vec(&record) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        target: "strategy",
                        error = %e,
                        "intent audit: record failed to serialize (trading continues)"
                    );
                    return record;
                }
            };
            buf.push(b'\n');
            let n = buf.len() as u64;
            // Lazily seed the segment size from the actual file before this
            // write (a pre-oversized segment rotates on the very first record);
            // seeding in `new` would stat the default relative path, which a
            // test may override afterwards.
            if !self.segment_seeded {
                self.segment_bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
                self.segment_seeded = true;
            }
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let appended = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .and_then(|mut f| f.write_all(&buf));
            if let Err(e) = appended {
                tracing::warn!(
                    target: "strategy",
                    error = %e,
                    path = %self.path.display(),
                    "intent audit append failed (trading continues): {}",
                    self.path.display()
                );
                return record;
            }
            // Same O_APPEND single-write contract as `crate::jsonl::append`
            // (#386): one write() lands at the current end atomically. We write
            // directly instead of via `jsonl::append` so the byte count feeds
            // the rotation ceiling below without a second serialization.
            // Lazily seed the segment size from the actual file on the first
            // write (a pre-oversized segment rotates on the very first record);
            // seeding in `new` would stat the default relative path, which a
            // test may override afterwards.
            if !self.segment_seeded {
                self.segment_bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
                self.segment_seeded = true;
            }
            self.segment_bytes += n;
            if self.rotate_bytes > 0 && self.segment_bytes >= self.rotate_bytes {
                self.rotate(ts_ms);
            }
        }
        record
    }

    /// Close the current segment under a UTC-stamped sibling name and let the
    /// next record start a fresh `path`. The rotated file is never altered — it
    /// is only renamed away.
    fn rotate(&mut self, stamp_ms: i64) {
        let rotated = next_segment_path(&self.path, stamp_ms);
        if let Err(e) = std::fs::rename(&self.path, &rotated) {
            tracing::warn!(
                path = %self.path.display(),
                target = %rotated.display(),
                error = %e,
                "intent audit: rotate failed; keeping the current segment"
            );
            return;
        }
        let wrote = self.segment_bytes;
        self.segment_bytes = 0;
        tracing::info!(
            path = %self.path.display(),
            rotated = %rotated.display(),
            bytes = wrote,
            "intent audit rotated"
        );
    }

    /// Fold pushes for `(strategy, gate, reason)`: at most ONE per second per
    /// key. `Some(count)` = emit now (count decisions folded into it).
    pub fn throttle(
        &mut self,
        strategy: &str,
        gate: &str,
        reason: &str,
        now_ms: i64,
    ) -> Option<u64> {
        const WINDOW_MS: i64 = 1000;
        let key = (strategy.to_string(), gate.to_string(), reason.to_string());
        let state = self.throttles.entry(key).or_insert(ThrottleState {
            last_sent_ms: i64::MIN,
            folded: 0,
        });
        if now_ms.saturating_sub(state.last_sent_ms) < WINDOW_MS {
            state.folded += 1;
            return None;
        }
        let count = state.folded + 1;
        state.last_sent_ms = now_ms;
        state.folded = 0;
        Some(count)
    }

    /// The push for a decision, with the gate/detail the §12.3 event carries:
    /// the REJECTING gate for a rejection, the last gate otherwise.
    pub fn push_for(record: &IntentAuditRecord, count: u64) -> DecisionPush {
        let (gate, detail) = match &record.decision {
            Decision::Rejected { gate, detail, .. } => {
                (format!("{gate:?}").to_uppercase(), detail.clone())
            }
            _ => record
                .gates
                .last()
                .map(|g| (format!("{:?}", g.gate).to_uppercase(), g.detail.clone()))
                .unwrap_or_else(|| ("UNKNOWN".into(), String::new())),
        };
        DecisionPush {
            strategy: record.strategy.clone(),
            intent_id: record.intent_id.clone(),
            status: record.decision.status().to_string(),
            gate,
            detail,
            ts_ms: record.ts_ms,
            count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitration::{GateId, GateOutcome, GateTrace, RejectReason};
    use rust_decimal_macros::dec;

    fn rejected_record(ts_ms: i64) -> IntentAuditRecord {
        IntentAuditRecord {
            ts_ms,
            account_id: "default".into(),
            strategy: "probe".into(),
            intent_id: "i00000001".into(),
            intent: serde_json::json!({"token": "UP", "price": "1.5"}),
            decision: Decision::Rejected {
                reason: RejectReason::OutOfPriceBand,
                gate: GateId::Legality,
                detail: "prediction price must be in (0,1]".into(),
            },
            gates: vec![GateTrace {
                gate: GateId::Legality,
                outcome: GateOutcome::Reject,
                detail: "prediction price must be in (0,1]".into(),
            }],
            latency_us: 3,
        }
    }

    /// The rejection-storm acceptance (§14.1 E25): 4000 rejections in one
    /// second are audited LINE BY LINE (nothing dropped), while the pushes for
    /// the same `(strategy, gate, reason)` fold to ≤ 1 per second.
    #[test]
    fn a_rejection_storm_audits_every_intent_and_folds_the_pushes() {
        let mut sink = AuditSink::new("default");
        let dir = std::env::temp_dir().join(format!("bk-e25-audit-{}", std::process::id()));
        sink.path = dir.join("intents.jsonl");
        let _ = std::fs::remove_file(&sink.path);

        let mut pushes = 0u64;
        for i in 0..4000i64 {
            // All 4000 inside the SAME throttle second (ts 0..1000).
            let ts = i / 4;
            let record = sink.record(
                ts,
                "default",
                "probe",
                &serde_json::json!({"token": "UP", "price": "1.5"}),
                &rejected_record(ts).decision,
                &rejected_record(ts).gates,
                3,
            );
            if sink
                .throttle(&record.strategy, "LEGALITY", "OUT_OF_PRICE_BAND", ts)
                .is_some()
            {
                pushes += 1;
            }
        }
        assert_eq!(pushes, 1, "one second, one key, ONE push");

        let lines = std::fs::read_to_string(&sink.path).unwrap().lines().count();
        assert_eq!(lines, 4000, "the audit is never folded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A disabled sink (`--no-intent-audit`) writes nothing — and that is the
    /// ONLY thing it changes (P1: both settings hit the same baseline).
    #[test]
    fn a_disabled_sink_writes_nothing() {
        let mut sink = AuditSink::new("default");
        sink.enabled = false;
        let dir = std::env::temp_dir().join(format!("bk-e25-audit-off-{}", std::process::id()));
        sink.path = dir.join("intents.jsonl");
        let record = sink.record(
            0,
            "default",
            "probe",
            &serde_json::json!({}),
            &rejected_record(0).decision,
            &[],
            0,
        );
        assert_eq!(record.intent_id, "i00000001", "ids keep flowing");
        assert!(!sink.path.exists(), "no file is written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The push carries the §12.3 fields verbatim; the DECIMAL intent values
    /// survive as strings (prices are decimals, never floats — same rule as
    /// the rest of the wire).
    #[test]
    fn push_carries_the_rejecting_gate_and_verbatim_detail() {
        let record = rejected_record(1758888000123);
        let push = AuditSink::push_for(&record, 3);
        assert_eq!(push.status, "REJECTED");
        assert_eq!(push.gate, "LEGALITY");
        assert_eq!(push.detail, "prediction price must be in (0,1]");
        assert_eq!(push.count, 3);
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["accountId"], "default");
        assert_eq!(json["decision"]["status"], "REJECTED");
        assert_eq!(json["decision"]["reason"], "OUT_OF_PRICE_BAND");
        assert_eq!(json["gates"][0]["gate"], "LEGALITY");
        assert!(json["latencyUs"].is_u64());
    }

    /// Sanity: an approved decision's push names the LAST gate (physics), and
    /// the throttle re-opens after the one-second window.
    #[test]
    fn throttle_reopens_after_the_window() {
        let mut sink = AuditSink::new("default");
        assert_eq!(sink.throttle("s", "LEGALITY", "R", 0), Some(1));
        assert_eq!(sink.throttle("s", "LEGALITY", "R", 500), None);
        assert_eq!(sink.throttle("s", "LEGALITY", "R", 999), None);
        assert_eq!(
            sink.throttle("s", "LEGALITY", "R", 1000),
            Some(3),
            "the folded count surfaces (3 decisions, one push)"
        );
        // A different key never folds into the first.
        assert_eq!(sink.throttle("s", "RISK", "R", 1000), Some(1));
    }

    /// The probe strategy's decimal book prices survive the audit round-trip:
    /// `serde_json::Value` intent + decimal-string prices read back exactly.
    #[test]
    fn decimal_prices_stay_exact_strings_in_the_record() {
        let mut sink = AuditSink::new("default");
        let dir = std::env::temp_dir().join(format!("bk-e25-audit-dec-{}", std::process::id()));
        sink.path = dir.join("intents.jsonl");
        sink.record(
            0,
            "default",
            "probe",
            &serde_json::json!({"price": dec!(0.40).to_string()}),
            &rejected_record(0).decision,
            &[],
            1,
        );
        let line = std::fs::read_to_string(&sink.path).unwrap();
        assert!(line.contains("\"price\":\"0.40\""), "{line}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1199 P1-11: a segment that reaches the per-segment ceiling is rotated
    /// to a UTC-stamped sibling and the next record starts a fresh file; no
    /// intent line is lost and ids keep flowing across the rotation.
    #[test]
    fn audit_rotates_the_full_segment_and_streams_keep_flowing() {
        let mut sink = AuditSink::new("default");
        sink.rotate_bytes = 400; // force rotation inside the test
        // Unique scratch dir: `a_rejection_storm` uses `bk-e25-audit-{pid}`,
        // tests in the same binary run in parallel and must not share it.
        let dir = std::env::temp_dir().join(format!("bk-e25-audit-rot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        sink.path = dir.join("intents.jsonl");
        let base: i64 = 1_757_851_200_000;
        let mut ids = Vec::new();
        for i in 0..7u64 {
            let ts = base + (i as i64) * 1000;
            let record = sink.record(
                ts,
                "default",
                "probe",
                &serde_json::json!({"token": "UP", "i": i}),
                &rejected_record(ts).decision,
                &rejected_record(ts).gates,
                3,
            );
            ids.push(record.intent_id.clone());
        }
        // The live file plus three rotated siblings (a rotation happens every
        // other record at this size).
        let mut segs: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        segs.sort();
        assert_eq!(
            segs.len(),
            4,
            "live file + three rotated segments: {segs:?}"
        );
        let mut live = String::new();
        let mut all = String::new();
        for seg in &segs {
            let content = std::fs::read_to_string(seg).unwrap();
            all.push_str(&content);
            if *seg == sink.path {
                live = content;
            }
        }
        assert!(!live.is_empty(), "the fresh segment receives new records");
        assert_eq!(
            all.lines().count(),
            7,
            "no intent line is lost across rotation"
        );
        for id in &ids {
            assert!(
                all.contains(&format!("\"intentId\":\"{id}\"")),
                "intent id {id} survived rotation"
            );
        }
        // The tail contract names the live path, which exists right after
        // rotation (ipc/server/handlers/intent.rs reads via jsonl::tail_filtered).
        assert!(sink.path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
