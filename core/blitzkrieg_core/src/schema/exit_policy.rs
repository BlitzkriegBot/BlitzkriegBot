//! Exit-policy write types: `risk.setExit`. Moved verbatim from
//! `schema/mod.rs` (pure code motion).

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// `risk.setExit` params: the exit triple Gate 4 binds per entry, all fields
/// optional; `accountId` picks the `account_exit_policy` row (absent = default).
/// Percentages are decimal STRINGS; `forceExitSec` is seconds.
///
/// Calibration guard: `forceExitSec: 0` DISABLES the guillotine and
/// `stopLossPct: 0` would stop on entry tick, so the percentages are refused
/// at `<= 0` while the deadline accepts `0` as the documented off switch —
/// the same posture the ExitConfig default documents (`force_exit_sec 0 = the
/// guillotine is OFF`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SetExitParams {
    #[serde(default)]
    pub account_id: Option<String>,
    /// 止损（入场价百分比）：跌破即触发保护性离场。出厂 12。
    #[serde(default, with = "crate::decimal::opt")]
    pub stop_loss_pct: Option<Decimal>,
    /// 止盈（入场价百分比）：兜底入袋规则。出厂 100（近确定二元的胜利入袋）。
    #[serde(default, with = "crate::decimal::opt")]
    pub take_profit_pct: Option<Decimal>,
    /// 价格天花板止盈（绝对价格，0 = 关闭，出厂 0.98）。
    #[serde(default, with = "crate::decimal::opt")]
    pub ceiling_tp_price: Option<Decimal>,
    /// 持有到期策略保护止损带宽（%，出厂 50）。
    #[serde(default, with = "crate::decimal::opt")]
    pub holder_stop_pct: Option<Decimal>,
    /// 强平秒数（轮剩余时间降到该值即硬性离场）。0 = 关闭强平。
    #[serde(default)]
    pub force_exit_sec: Option<i64>,
    /// 轮次最小剩余时间退出（秒，0 = 仅到期结算）。
    #[serde(default)]
    pub min_time_left_sec: Option<i64>,
    /// 简单退出模式开关（出厂 true）。
    #[serde(default)]
    pub simple_exit_enabled: Option<bool>,
    /// 动态收紧止损开关（出厂 true）。
    #[serde(default)]
    pub dynamic_stop_enabled: Option<bool>,
    /// 动态止损开始收紧的剩余秒数（出厂 300）。
    #[serde(default)]
    pub stop_tighten_start_sec: Option<i64>,
    /// 动态止损收紧下限（%，出厂 10）。
    #[serde(default, with = "crate::decimal::opt")]
    pub stop_min_pct: Option<Decimal>,
    /// 移动止盈总开关（出厂 false）。
    #[serde(default)]
    pub trailing_enabled: Option<bool>,
    /// 移动止盈启动所需最高浮盈（%，出厂 15）。
    #[serde(default, with = "crate::decimal::opt")]
    pub trailing_min_high_pct: Option<Decimal>,
    /// 移动止盈最小回撤宽度（%，出厂 8）。
    #[serde(default, with = "crate::decimal::opt")]
    pub min_trail_pct: Option<Decimal>,
    /// 比例回撤移动止盈开关（出厂 true）。
    #[serde(default)]
    pub proportional_trail_enabled: Option<bool>,
    /// 比例回撤系数（%，出厂 15）。
    #[serde(default, with = "crate::decimal::opt")]
    pub proportional_trail_pct: Option<Decimal>,
    /// 启用比例回撤的最低最高浮盈（%，出厂 15）。
    #[serde(default, with = "crate::decimal::opt")]
    pub proportional_trail_min_pct: Option<Decimal>,
    /// 比例回撤最小允许回吐（%，出厂 3）。
    #[serde(default, with = "crate::decimal::opt")]
    pub proportional_trail_min_giveback_pct: Option<Decimal>,
    /// 紧止损开关（完整模式，出厂 true）。
    #[serde(default)]
    pub tight_stop_enabled: Option<bool>,
    /// 紧止损百分比（完整模式，出厂 12）。
    #[serde(default, with = "crate::decimal::opt")]
    pub tight_stop_pct: Option<Decimal>,
    /// 棘轮保底止盈开关（完整模式，出厂 false）。
    #[serde(default)]
    pub ratchet_enabled: Option<bool>,
    /// 棘轮高点确认 tick 数（出厂 3）。
    #[serde(default)]
    pub ratchet_confirm_ticks: Option<u32>,
    /// 棘轮高点确认容差（%，出厂 0.5）。
    #[serde(default, with = "crate::decimal::opt")]
    pub ratchet_confirm_tolerance_pct: Option<Decimal>,
    /// 防插针买一偏离中价上限（%，出厂 8）。
    #[serde(default, with = "crate::decimal::opt")]
    pub max_bid_wick_pct: Option<Decimal>,
    /// 记忆价判定有效期（秒，出厂 30）。
    #[serde(default)]
    pub max_last_price_age_sec: Option<i64>,
    /// 滞涨止盈浮盈阈值（完整模式，%，出厂 20）。
    #[serde(default, with = "crate::decimal::opt")]
    pub stale_profit_pct: Option<Decimal>,
    /// 滞涨止盈买一不变时长（秒，出厂 10）。
    #[serde(default)]
    pub stale_profit_bid_unchanged_sec: Option<i64>,
    /// 停滞止盈浮盈阈值（完整模式，%，出厂 5）。
    #[serde(default, with = "crate::decimal::opt")]
    pub stagnant_profit_pct: Option<Decimal>,
    /// 停滞止盈无进展时长（秒，出厂 30）。
    #[serde(default)]
    pub stagnant_duration_sec: Option<i64>,
    /// 深度塌陷止盈阈值（完整模式，%，出厂 60）。
    #[serde(default, with = "crate::decimal::opt")]
    pub depth_collapse_threshold_pct: Option<Decimal>,
    /// 完整模式开仓宽限期（秒，出厂 3）。
    #[serde(default)]
    pub exit_grace_sec: Option<i64>,
    /// 仅止盈走挂单出场（完整模式，出厂 false）。
    #[serde(default)]
    pub maker_exits_for_tp_only: Option<bool>,
    /// 固定止盈优先挂单（出厂 true）。
    #[serde(default)]
    pub maker_first_exit_enabled: Option<bool>,
    /// 策略出场 maker-first 总开关（出厂 false）。
    #[serde(default)]
    pub strategy_exit_maker_first: Option<bool>,
    /// 挂单出场追单超时（ms，出厂 250；0 = 沿用入场默认，负数 = 永不追单）。
    #[serde(default)]
    pub exit_maker_timeout_ms: Option<i64>,
    /// 盘口定价有效期（秒，出厂 60）。
    #[serde(default)]
    pub max_book_age_sec: Option<i64>,
    /// Free-text note from the caller, echoed into the audit line.
    #[serde(default)]
    pub reason: Option<String>,
}

impl SetExitParams {
    /// True when the patch names no field — refused, not answered with an
    /// empty record (the same posture as `risk.setSystemic`).
    pub fn is_empty(&self) -> bool {
        self.stop_loss_pct.is_none()
            && self.take_profit_pct.is_none()
            && self.ceiling_tp_price.is_none()
            && self.holder_stop_pct.is_none()
            && self.force_exit_sec.is_none()
            && self.min_time_left_sec.is_none()
            && self.simple_exit_enabled.is_none()
            && self.dynamic_stop_enabled.is_none()
            && self.stop_tighten_start_sec.is_none()
            && self.stop_min_pct.is_none()
            && self.trailing_enabled.is_none()
            && self.trailing_min_high_pct.is_none()
            && self.min_trail_pct.is_none()
            && self.proportional_trail_enabled.is_none()
            && self.proportional_trail_pct.is_none()
            && self.proportional_trail_min_pct.is_none()
            && self.proportional_trail_min_giveback_pct.is_none()
            && self.tight_stop_enabled.is_none()
            && self.tight_stop_pct.is_none()
            && self.ratchet_enabled.is_none()
            && self.ratchet_confirm_ticks.is_none()
            && self.ratchet_confirm_tolerance_pct.is_none()
            && self.max_bid_wick_pct.is_none()
            && self.max_last_price_age_sec.is_none()
            && self.stale_profit_pct.is_none()
            && self.stale_profit_bid_unchanged_sec.is_none()
            && self.stagnant_profit_pct.is_none()
            && self.stagnant_duration_sec.is_none()
            && self.depth_collapse_threshold_pct.is_none()
            && self.exit_grace_sec.is_none()
            && self.maker_exits_for_tp_only.is_none()
            && self.maker_first_exit_enabled.is_none()
            && self.strategy_exit_maker_first.is_none()
            && self.exit_maker_timeout_ms.is_none()
            && self.max_book_age_sec.is_none()
    }
}

/// One field's resolution inside a `risk.setExit` reply. All three fields are
/// **live** for every NEW entry (Gate 4 binds from the live config each
/// intent) and the exit sweep reads the same live config, so a change also
/// steers already-open positions' protective rules — the receipt says so.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitSetChange {
    /// The wire (camelCase) name of the field.
    pub field: &'static str,
    /// Previous effective value, decimal string.
    pub from: String,
    /// New effective value, decimal string.
    pub to: String,
    /// Always `live` today: every exit rule is judged per tick from the live
    /// config. A separate class exists so a future next-session knob does not
    /// reuse the string silently.
    pub effect: &'static str,
}

/// `risk.setExit` result: the audited change list, in the
/// `SystemicLimitUpdate` shape. `persisted` is `true` when the durable carrier
/// took the write — the SQLite `account_exit_policy` row (`StoreApi::save_exit`;
/// the only backend since #1199 — the env-only backend was deleted, so the
/// restart re-resolves the same values from it) — and
/// `false` otherwise (no carrier wired — backtester, most tests — or the
/// write failed, which is logged with the path). The panel also receives
/// `persistence: "store" | "none"` beside it, spelled by the write face.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitSetUpdate {
    pub applied: Vec<ExitSetChange>,
    pub at_ms: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub persisted: bool,
    pub note: &'static str,
}

/// [`ExitSetUpdate::note`] — one spelling, used by the reply and the log.
pub const EXIT_SET_UPDATE_NOTE: &str = "live for every new entry (Gate 4 binds per intent) and for the exit \
     sweep of already-open positions; persisted to the kernel's store (SQLite \
     when the DB is open, the .env BK_EXIT_* keys otherwise) — in-memory \
     only when no carrier is wired";
