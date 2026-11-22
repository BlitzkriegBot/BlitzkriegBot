//! Idempotent ledger identifier migration: rewrite persisted `roundSlot`
//! (or `round_slot`) from a scanner index to the venue start-Unix-ts when
//! the product is an unambiguous timestamp. Money fields are never touched.
//!
//! A first successful rewrite keeps a sibling `*.pre-venue-round-id` backup
//! of the original file. A second pass is a no-op. Rows whose duration is
//! missing, or whose product would stay below [`crate::round_id::VENUE_TS_FLOOR`],
//! are left as indices (the expiry helper still understands that form).

use crate::round_id::venue_round_id;
use serde_json::{Map, Value};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const BACKUP_SUFFIX: &str = ".pre-venue-round-id";
const TMP_SUFFIX: &str = ".tmp-venue-round-id";

/// Money / size keys that must be byte-identical across a rewrite (camel
/// or snake). Used for the fingerprint proof, not for rewriting.
const PROTECTED_KEYS: &[&str] = &[
    "pnl",
    "realizedPnl",
    "realized_pnl",
    "unrealizedPnl",
    "unrealized_pnl",
    "size",
    "filledSize",
    "filled_size",
    "price",
    "entryPrice",
    "entry_price",
    "exitPrice",
    "exit_price",
    "notional",
    "cash",
    "fee",
    "rebate",
    "amount",
    "cost",
    "proceeds",
    "shares",
    "balance",
    "qty",
    "quantity",
];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FileReport {
    pub path: PathBuf,
    pub records: u64,
    pub remapped: u64,
    pub already_venue: u64,
    pub left_as_index: u64,
    pub fingerprint_before: String,
    pub fingerprint_after: String,
    pub backed_up: bool,
}

impl FileReport {
    pub fn fingerprints_match(&self) -> bool {
        self.fingerprint_before == self.fingerprint_after
    }
}

#[derive(Debug, Default, Clone)]
pub struct MigrateReport {
    pub files: Vec<FileReport>,
}

impl MigrateReport {
    pub fn remapped(&self) -> u64 {
        self.files.iter().map(|f| f.remapped).sum()
    }
    pub fn fingerprints_match(&self) -> bool {
        self.files.iter().all(FileReport::fingerprints_match)
    }
}

/// Rewrite `roundSlot` / `round_slot` in one JSONL file. Missing files are
/// skipped (empty report, no error). Parse errors refuse the file.
pub fn migrate_jsonl_file(path: &Path) -> Result<FileReport, String> {
    let mut report = FileReport {
        path: path.to_path_buf(),
        ..FileReport::default()
    };
    if !path.is_file() {
        return Ok(report);
    }
    let raw = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut out_lines: Vec<String> = Vec::new();
    let mut fp_before = String::new();
    let mut fp_after = String::new();
    for (idx, line) in raw.split_inclusive('\n').enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            out_lines.push(line.to_string());
            continue;
        }
        let mut value: Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("parse {} line {}: {e}", path.display(), idx + 1))?;
        fp_before.push_str(&fingerprint_without_round_id(&value));
        fp_before.push('\n');
        if let Some(obj) = value.as_object_mut() {
            report.records += 1;
            match remap_object(obj) {
                Remap::Changed => report.remapped += 1,
                Remap::AlreadyVenue => report.already_venue += 1,
                Remap::LeftAsIndex => report.left_as_index += 1,
            }
        }
        fp_after.push_str(&fingerprint_without_round_id(&value));
        fp_after.push('\n');
        let mut encoded = serde_json::to_string(&value)
            .map_err(|e| format!("reserialize {} line {}: {e}", path.display(), idx + 1))?;
        if line.ends_with('\n') {
            encoded.push('\n');
        }
        out_lines.push(encoded);
    }
    report.fingerprint_before = fp_before;
    report.fingerprint_after = fp_after;
    if !report.fingerprints_match() {
        return Err(format!(
            "refusing to write {}: money/size fingerprint changed",
            path.display()
        ));
    }
    if report.remapped == 0 {
        return Ok(report);
    }
    let backup = backup_path(path);
    if !backup.exists() {
        fs::copy(path, &backup)
            .map_err(|e| format!("backup {} -> {}: {e}", path.display(), backup.display()))?;
        report.backed_up = true;
    }
    let tmp = tmp_path(path);
    write_atomically(&tmp, path, &out_lines)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(report)
}

enum Remap {
    Changed,
    AlreadyVenue,
    LeftAsIndex,
}

fn remap_object(obj: &mut Map<String, Value>) -> Remap {
    let (key, slot) = match slot_field(obj) {
        Some(v) => v,
        None => return Remap::LeftAsIndex,
    };
    if slot <= 0 {
        return Remap::LeftAsIndex;
    }
    let Some(dur) = duration_field(obj) else {
        return Remap::LeftAsIndex;
    };
    let new = venue_round_id(slot, dur);
    if new == slot {
        if slot >= crate::round_id::VENUE_TS_FLOOR {
            Remap::AlreadyVenue
        } else {
            Remap::LeftAsIndex
        }
    } else {
        obj.insert(key, Value::from(new));
        Remap::Changed
    }
}

fn slot_field(obj: &Map<String, Value>) -> Option<(String, i64)> {
    for key in ["roundSlot", "round_slot"] {
        if let Some(v) = obj.get(key).and_then(as_i64) {
            return Some((key.to_string(), v));
        }
    }
    None
}

fn duration_field(obj: &Map<String, Value>) -> Option<i64> {
    for key in [
        "roundDurationSec",
        "round_duration_sec",
        "durationSec",
        "duration_sec",
    ] {
        if let Some(v) = obj.get(key).and_then(as_i64)
            && v > 0
        {
            return Some(v);
        }
    }
    None
}

fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_u64().map(|u| u as i64)),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn fingerprint_without_round_id(v: &Value) -> String {
    let mut cloned = v.clone();
    strip_round_keys(&mut cloned);
    // Also pin protected money keys in a stable side channel so a rewrite
    // that somehow dropped a key still fails the compare.
    let mut money = Vec::new();
    collect_protected(&cloned, &mut money);
    money.sort();
    format!("{}|{}", cloned, money.join(";"))
}

fn strip_round_keys(v: &mut Value) {
    match v {
        Value::Object(map) => {
            map.remove("roundSlot");
            map.remove("round_slot");
            for child in map.values_mut() {
                strip_round_keys(child);
            }
        }
        Value::Array(items) => {
            for child in items {
                strip_round_keys(child);
            }
        }
        _ => {}
    }
}

fn collect_protected(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if PROTECTED_KEYS.iter().any(|p| p.eq_ignore_ascii_case(k)) {
                    out.push(format!("{k}={val}"));
                }
                collect_protected(val, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_protected(child, out);
            }
        }
        _ => {}
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(BACKUP_SUFFIX);
    PathBuf::from(s)
}

/// A per-call unique temp name beside `path`.
///
/// The suffix used to be a fixed `TMP_SUFFIX`, which made two concurrent
/// migrations of the same ledger write the same temp file: the first `rename`
/// moved it away and the second failed with `write <dest>: No such file or
/// directory`. That is not a hypothetical — `CoreConfig::default()` carries
/// cwd-relative ledger paths, so every test that boots a default core shares
/// one physical file, and the parallel lib suite hit the collision (#138).
/// pid + a process-wide counter keeps each writer's temp its own, so a
/// concurrent second pass either wins the rename or fails the overwrite
/// check, never a phantom ENOENT.
fn tmp_path(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut s = path.as_os_str().to_os_string();
    s.push(format!(
        "{TMP_SUFFIX}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    PathBuf::from(s)
}

fn write_atomically(tmp: &Path, dest: &Path, lines: &[String]) -> io::Result<()> {
    {
        let mut f = fs::File::create(tmp)?;
        for line in lines {
            f.write_all(line.as_bytes())?;
        }
        f.sync_all()?;
    }
    fs::rename(tmp, dest)?;
    Ok(())
}

/// The configured ledger path plus existing `.dry.jsonl` / `.live.jsonl`
/// siblings (and the unsuffixed sibling when the configured path is already
/// mode-suffixed). Missing files are not added.
pub fn ledger_paths_to_migrate(configured: Option<&str>) -> Vec<PathBuf> {
    let Some(raw) = configured.filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    let path = PathBuf::from(raw);
    let mut out = vec![path.clone()];
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let mut candidates = Vec::new();
        if let Some(stem) = name.strip_suffix(".jsonl") {
            if let Some(base) = stem.strip_suffix(".dry") {
                candidates.push(format!("{base}.jsonl"));
                candidates.push(format!("{base}.live.jsonl"));
            } else if let Some(base) = stem.strip_suffix(".live") {
                candidates.push(format!("{base}.jsonl"));
                candidates.push(format!("{base}.dry.jsonl"));
            } else {
                candidates.push(format!("{stem}.dry.jsonl"));
                candidates.push(format!("{stem}.live.jsonl"));
            }
        }
        for c in candidates {
            let p = parent.join(c);
            if p.is_file() && !out.iter().any(|e| e == &p) {
                out.push(p);
            }
        }
    }
    out
}

/// Migrate every configured orders / positions / trades jsonl (and siblings).
pub fn migrate_configured_ledgers(
    order_log_path: Option<&str>,
    position_log_path: Option<&str>,
    trade_log_path: Option<&str>,
) -> Result<MigrateReport, String> {
    let mut paths = Vec::new();
    for cfg in [order_log_path, position_log_path, trade_log_path] {
        for p in ledger_paths_to_migrate(cfg) {
            if !paths.iter().any(|e| e == &p) {
                paths.push(p);
            }
        }
    }
    let mut report = MigrateReport::default();
    for p in paths {
        report.files.push(migrate_jsonl_file(&p)?);
    }
    if !report.fingerprints_match() {
        return Err("money/size fingerprint changed during venue-round-id migration".into());
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "bk-venue-round-id-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn remaps_legacy_slot_and_keeps_money_totals() {
        let dir = tmp_dir();
        let path = dir.join("trades.jsonl");
        let body = concat!(
            r#"{"id":"t1","roundSlot":5984100,"roundDurationSec":300,"pnl":"1.25","size":"10","price":"0.55"}"#,
            "\n",
            r#"{"id":"t2","roundSlot":1994700,"round_duration_sec":900,"realizedPnl":-0.5,"filledSize":4}"#,
            "\n",
            // already a venue ts — left alone
            r#"{"id":"t3","roundSlot":1795230000,"roundDurationSec":300,"pnl":"2.00"}"#,
            "\n",
            // tiny test slot — left as index
            r#"{"id":"t4","roundSlot":2,"roundDurationSec":3600,"pnl":"9.00"}"#,
            "\n",
            // no duration — left alone
            r#"{"id":"t5","roundSlot":5984100,"pnl":"0.10"}"#,
            "\n",
        );
        fs::write(&path, body).unwrap();
        let r = migrate_jsonl_file(&path).unwrap();
        assert_eq!(r.records, 5);
        assert_eq!(r.remapped, 2);
        assert_eq!(r.already_venue, 1);
        assert_eq!(r.left_as_index, 2);
        assert!(r.fingerprints_match(), "money fields must be unchanged");
        assert!(r.backed_up);
        assert!(backup_path(&path).is_file());

        let lines: Vec<Value> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["roundSlot"], 1_795_230_000);
        assert_eq!(lines[0]["pnl"], "1.25");
        assert_eq!(lines[0]["size"], "10");
        assert_eq!(lines[1]["roundSlot"], 1_795_230_000);
        assert_eq!(lines[1]["realizedPnl"], -0.5);
        assert_eq!(lines[2]["roundSlot"], 1_795_230_000);
        assert_eq!(lines[3]["roundSlot"], 2);
        assert_eq!(lines[4]["roundSlot"], 5_984_100);

        // Idempotent second pass: no further remaps, original backup kept.
        let r2 = migrate_jsonl_file(&path).unwrap();
        assert_eq!(r2.remapped, 0);
        assert_eq!(r2.already_venue, 3);
        assert!(r2.fingerprints_match());
        assert!(!r2.backed_up);
        assert_eq!(
            fs::read_to_string(backup_path(&path)).unwrap(),
            body,
            "original backup must stay the pre-migration bytes"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// #138: two concurrent migrations of the SAME ledger must never trip over
    /// each other's temp file.
    ///
    /// The temp name used to be deterministic (`<ledger>.tmp-venue-round-id`),
    /// so writer B could `rename` after writer A had already moved that temp
    /// away and fail with `write <dest>: No such file or directory`. This pins
    /// the fix at the level the bug lived: distinct temp names, so both writers
    /// either succeed or fail the *content* check — never a phantom ENOENT on
    /// a name they both own.
    #[test]
    fn concurrent_migrations_of_one_ledger_do_not_collide_on_the_temp_file() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let dir = tmp_dir();
        let path = dir.join("orders.jsonl");
        // Legacy rows the migration will rewrite on both passes.
        let mut body = String::new();
        for i in 0..200 {
            body.push_str(&format!(
                "{{\"id\":\"o{i}\",\"roundSlot\":{},\"roundDurationSec\":300,\"size\":\"3\"}}\n",
                5_984_100 + i
            ));
        }
        fs::write(&path, &body).unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let path = path.clone();
                thread::spawn(move || {
                    // Line both writers up so their write windows overlap.
                    barrier.wait();
                    migrate_jsonl_file(&path)
                })
            })
            .collect();

        for h in handles {
            let r = h
                .join()
                .expect("a migration thread panicked; the collision must be a Result, not a panic");
            let r = r.unwrap_or_else(|e| {
                panic!(
                    "concurrent migration failed: {e} — a shared temp name is the \
                     deterministic-tmp-file bug (#138) coming back"
                )
            });
            assert!(
                r.fingerprints_match(),
                "money must stay byte-identical even under concurrency"
            );
        }

        // The ledger is left in the migrated (idempotent) state, and the
        // pre-migration bytes are still recoverable from the backup.
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains("\"roundSlot\":1795230000"), "{after}");
        assert!(backup_path(&path).is_file());
        // No writer may leave its temp behind.
        let leftovers: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-venue-round-id"))
            .collect();
        assert!(leftovers.is_empty(), "temp files survived: {leftovers:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn snake_case_field_names_are_preserved() {
        let dir = tmp_dir();
        let path = dir.join("orders.jsonl");
        fs::write(
            &path,
            r#"{"round_slot":5984100,"round_duration_sec":300,"size":"3"}"#,
        )
        .unwrap();
        migrate_jsonl_file(&path).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v.get("round_slot").is_some());
        assert!(v.get("roundSlot").is_none());
        assert_eq!(v["round_slot"], 1_795_230_000);
        assert_eq!(v["size"], "3");
        fs::remove_dir_all(&dir).ok();
    }
}
