//! `data/runtime/config.json` — the runtime settings file holding ONE live
//! cell: the market-plugin override (`marketPlugin`). The WebUI writes it
//! through `market.plugin.set` (via `persist_market_plugin_to`); the boot
//! resolution and the `market.plugin.get` readout read it back (via
//! `load_market_plugin`). Same model as `data/update/state.json`: a small
//! durable JSON doc that survives restarts. It outranks the
//! first-registered fallback but never an explicit `--market-plugin` or the
//! accounts' venue bindings.
//!
//! The file's other historical cell — the round-duration override — is
//! retired: `round_duration.set` persists per-account to SQLite
//! (`account_settings`, key `round.duration_sec`) and boot reads it back
//! from there, never from this file. The round-duration reader below is the
//! retired cell's read-back path, kept only for its tests.
//!
//! Next-session semantics by construction: a write to this file cannot move
//! the running session's feed/discovery/executor — they were all built
//! around the plugin choice at ITS boot (the same reason the retired round
//! grid could not move: the grid is a coordinate system — `round_slot` is a
//! UTC grid index derived with the session's duration — and live
//! positions/orders were slotted under the old length).

use std::path::PathBuf;

use crate::model::CoreError;
use crate::store::StoreApi;

/// cwd-relative, like every other `data/` writer.
fn state_path() -> PathBuf {
    PathBuf::from("data/runtime/config.json")
}

/// What the boot resolution needs to know about the cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoundDurationCell {
    pub duration_sec: i64,
}

/// Read the persisted round duration, if the cell holds one. A missing or
/// broken file is the normal first-boot case → `None`, no drama (the same
/// posture as `version::load_from`).
///
/// Which durations are legal is NOT this layer's business: the declared union
/// of the market plugins' `supported_round_secs` (`MarketPluginRegistry::
/// allowed_round_secs`) owns that, and the config layer cannot see the
/// registry — the market-plugin cell below follows the same split. This
/// reader therefore only reports what the cell holds; the boot caller
/// (`main.rs`) passes the union in as `allowed` and applies the admission
/// rule itself (out-of-union values are ignored with a warning — a
/// hand-edited cell must not be able to arm a duration no plugin declared).
pub fn load_round_duration() -> Option<RoundDurationCell> {
    load_round_duration_from(&state_path(), &[])
}

pub fn load_round_duration_from(
    path: &std::path::Path,
    allowed: &[i64],
) -> Option<RoundDurationCell> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "{} is not valid JSON — the round-duration cell is ignored: {e}",
                path.display()
            );
            return None;
        }
    };
    let sec = v.get("roundDurationSec").and_then(|s| s.as_i64());
    let Some(sec) = sec else {
        return None; // cell present but holds no round duration — not an error
    };
    if !allowed.contains(&sec) {
        tracing::warn!(
            "{} holds roundDurationSec={sec}, which is not in the market plugins' \
             declared round-duration union {allowed:?} — the cell is ignored",
            path.display()
        );
        return None;
    }
    Some(RoundDurationCell { duration_sec: sec })
}

/// The market-plugin cell the boot resolution reads back. issue #47:
/// 读取来自 SQLite `kernel_meta.runtime_config_json`（通过 `StoreApi`），
/// 不再读文件。调用方负责把名字同 `MarketPluginRegistry::names()` 做
/// 注册表校验；本层只报告 cell 里存了什么。
#[derive(Debug, Clone, PartialEq)]
pub struct MarketPluginCell {
    pub plugin: String,
}

pub fn load_market_plugin() -> Option<MarketPluginCell> {
    // 无 store 注入时（回测、多数进程内测试）持久格不可读 → None，
    // boot 回落到 flag/绑定/首个注册者。与 `persist_market_plugin` 的
    // `None`-store 拒绝姿态对称。
    None
}

pub fn load_market_plugin_from(
    store: &dyn StoreApi,
) -> Result<Option<MarketPluginCell>, CoreError> {
    match store.load_market_plugin()? {
        Some(plugin) => Ok(Some(MarketPluginCell { plugin })),
        None => Ok(None),
    }
}

/// Write (or clear) the persisted market-plugin choice. issue #47:
/// config.json 直写替换为 SQLite persist 路径 —— 通过 `StoreApi` 落
/// `kernel_meta.runtime_config_json`，不再触碰文件系统。
pub fn persist_market_plugin(
    store: &dyn StoreApi,
    plugin: Option<&str>,
    _actor: &str,
    _now_ms: i64,
) -> Result<(), CoreError> {
    store.save_market_plugin(plugin)
}

/// Write (or clear) the persisted market-plugin choice. 与 `persist_market_plugin`
/// 同义，保留命名以便调用方迁移；issue #47 后所有写路径均经 SQLite。
pub fn persist_market_plugin_to(
    store: &dyn StoreApi,
    plugin: Option<&str>,
    _actor: &str,
    _now_ms: i64,
) -> Result<(), CoreError> {
    persist_market_plugin(store, plugin, _actor, _now_ms)
}

/// #458: append one JSONL record for a manual, destructive operation
/// (`system.reset_trading`; #456's force-close rides the same file). The
/// manual-ops audit is the ONLY trace a reset leaves — the wiped records are
/// gone — so a write failure is an ERROR, not a warn-and-continue: an
/// unaudited reset must fail loudly (the caller surfaces it as a refusal).
///
/// Path convention: the config carries the explicit path (production
/// `data/audit/manual-ops.jsonl`); the config also has `manual_ops_audit_path`
/// in the shape of the other audit paths. Appending to an existing file is the
/// whole model — audit streams are append-only, never cleared by anything.
pub fn append_manual_op(path: &std::path::Path, record: &serde_json::Value) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let line = serde_json::to_string(record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

/// Count non-empty lines of a JSONL record file — the pre-reset summary's
/// "how much is about to be deleted" figure. A missing file counts as 0.
pub fn count_jsonl_lines(path: &std::path::Path) -> u64 {
    std::fs::read_to_string(path)
        .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;

    /// A five-family historical whitelist used as a test fixture (wider
    /// than the current Polymarket declaration of 5m/15m). Test-fixture
    /// only: the assertion semantics stay exactly what they were.
    const TEST_ALLOWED: &[i64] = &[300, 900, 3600, 14400, 86400];

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bk-runtime-cfg-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn store_at(p: &std::path::Path) -> SqliteStore {
        SqliteStore::open(p).expect("test store opens")
    }

    #[test]
    fn a_missing_file_loads_as_none() {
        let d = tmpdir("missing");
        assert_eq!(
            load_round_duration_from(&d.join("state.json"), TEST_ALLOWED),
            None
        );
    }

    #[test]
    fn an_off_whitelist_value_is_ignored_not_armed() {
        let d = tmpdir("whitelist");
        let p = d.join("config.json");
        std::fs::write(
            &p,
            r#"{"roundDurationSec": 60, "roundDurationActor": "hand-edit"}"#,
        )
        .unwrap();
        assert_eq!(load_round_duration_from(&p, TEST_ALLOWED), None);
        // An EMPTY allowed set (no plugin declared anything) refuses every
        // value — fail-closed, never "allow everything".
        std::fs::write(
            &p,
            r#"{"roundDurationSec": 300, "roundDurationActor": "hand-edit"}"#,
        )
        .unwrap();
        assert_eq!(load_round_duration_from(&p, &[]), None);
    }

    #[test]
    fn manual_ops_appends_and_counts_lines() {
        let d = tmpdir("manualops");
        let p = d.join("audit").join("manual-ops.jsonl");
        append_manual_op(&p, &serde_json::json!({ "op": "reset" })).unwrap();
        append_manual_op(&p, &serde_json::json!({ "op": "force_close" })).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().all(|l| l.starts_with('{')));
        assert_eq!(count_jsonl_lines(&p), 2);
        // A missing file counts as 0, not an error.
        assert_eq!(count_jsonl_lines(&d.join("nope.jsonl")), 0);
    }

    #[test]
    fn the_market_plugin_cell_round_trips() {
        let d = tmpdir("mktrip");
        let p = d.join("config.json");
        let store = store_at(&p);
        persist_market_plugin_to(&store, Some("kalshi"), "uid:test", 1_000).unwrap();
        assert_eq!(
            load_market_plugin_from(&store).unwrap(),
            Some(MarketPluginCell {
                plugin: "kalshi".into()
            })
        );
    }

    #[test]
    fn an_empty_or_whitespace_plugin_name_loads_as_none() {
        let d = tmpdir("blankplugin");
        let p = d.join("config.json");
        let store = store_at(&p);
        persist_market_plugin_to(&store, Some("   "), "uid:test", 1_000).unwrap();
        assert_eq!(load_market_plugin_from(&store).unwrap(), None);
    }

    #[test]
    fn a_missing_file_yields_no_plugin_cell() {
        let d = tmpdir("noplugin");
        let p = d.join("config.json");
        let store = store_at(&p);
        assert_eq!(load_market_plugin_from(&store).unwrap(), None);
    }
}
