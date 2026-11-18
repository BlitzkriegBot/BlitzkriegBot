//! Round-duration read/write types: `round_duration.get` / `round_duration.set`.
//! Moved verbatim from `schema/mod.rs` (pure code motion).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `round_duration.get` result: the effective round length, where it came
/// from, the switchable whitelist, and per-account round durations.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoundDurationResult {
    /// The effective seconds the DEFAULT account runs on (fallback for
    /// accounts without an explicit `ACCOUNT_<ID>_ROUND_DURATION_SEC`).
    pub effective_sec: i64,
    /// 引擎扫描器当前实际运行的轮长网格（倒计时钟真实跑的值）。与
    /// `effective_sec` 的 default 网格分离：本字段是 DEFAULT 账户解析后的族
    /// （它自己的 per-account 覆盖，没有覆盖就是进程默认网格），#1272 之后
    /// 由 per-account 记录直接导出而不是一次全局写入 —— 重启后它从库里读回
    /// 同一个值（#1151 的「无 split-brain」契约）。
    pub engine_active_sec: i64,
    /// One of `cli` / `env` / `runtime` / `toml` / `default` — the boot
    /// resolution's own report for the default grid.
    pub source: &'static str,
    /// Per-account round durations: `account_id → seconds`. Accounts absent
    /// from this map fall back to `effective_sec`.
    pub per_account: BTreeMap<String, i64>,
    /// The switchable whitelist (labels come from the plugins' declared
    /// `round_labels`, aggregated at boot — issue 648).
    pub allowed: Vec<RoundDurationOption>,
    /// What the next session will run on for the default account.
    pub next_session_sec: i64,
    /// Whether the persisted cell holds a value for the default account.
    pub persisted: bool,
}

/// One row of the whitelist.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoundDurationOption {
    pub sec: i64,
    /// `5m` / `15m` / `1h` / `4h` / `daily` — null for a non-standard value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<&'static str>,
}

/// One account's round duration entry.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRoundDuration {
    pub account_id: String,
    pub duration_sec: i64,
}

/// `round_duration.set` params: `account_id` names the account whose grid is
/// being set; `duration_sec` must be one of the whitelist values;
/// `reset` clears that account's override (falls back to default). Exactly
/// one of `duration_sec` or `reset` must be given.
///
/// 2026-11-13 操作者指令：切换即「秒切秒换」，不再有 `immediate` 开关。
/// 每次写入一律先撤该账户在途轮市场的全部挂单、按市价真实强平其在途
/// 仓位（PnL 按真实成交价），再把扫描器网格切入新周期；任一步骤
/// 失败 fail-closed，网格不切。仓库内已无客户端发 `immediate` 字段（前端
/// 残留已在 #1235 清除）；旧客户端若仍发，serde 容忍未知字段并按立即切换
/// 语义处理。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetRoundDurationParams {
    /// The account whose round duration is being set. Required.
    pub account_id: String,
    #[serde(default)]
    pub duration_sec: Option<i64>,
    #[serde(default)]
    pub reset: bool,
    /// Audit trail only — echoed in the reply and the log line.
    pub reason: Option<String>,
}

/// `round_duration.set` result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoundDurationUpdate {
    /// The account whose grid was set.
    pub account_id: String,
    /// What the account will run on after this write (for `reset`, the default grid).
    pub next_session_sec: i64,
    /// `reset` clears the account's override (falls back to boot resolution) instead.
    pub reset: bool,
    /// `true` when durably persisted to SQLite (#1199: `.env` is read-only).
    pub persisted: bool,
    /// Real count of in-transit positions market-flattened by this write
    /// (from `cancel_and_flatten_account`); cancelled orders not counted.
    pub flattened_positions: usize,
    pub at_ms: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub note: &'static str,
}

/// [`RoundDurationUpdate::note`] — one spelling, used by the reply and the log.
pub const ROUND_DURATION_SET_NOTE: &str = "per-account round grid: in-transit orders are cancelled \
     and positions are market-flattened BEFORE the grid switches; the switch is fail-closed \
     (the grid does not move on any failure); the value persists for the next session; an explicit --round-sec / BK_ROUND_SEC still outranks this setting at boot";
