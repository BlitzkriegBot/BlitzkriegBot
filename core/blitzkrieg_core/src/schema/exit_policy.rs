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
    /// 强平秒数（轮剩余时间降到该值即硬性离场）。**0 = 关闭强平**（出厂
    /// 校准值：旧的 120s 截止切掉了后来赔付的轮）。负值拒绝。
    #[serde(default)]
    pub force_exit_sec: Option<i64>,
    /// 移动止盈总开关（trailing stop 是否武装）。**出厂 false** —— 10-02
    /// #356 校准裁决（「操盘手从不卖进利润，全部持有到期 REDEEM」），但
    /// spread_arb 一类「入场→坐山车→止损」的价差策略正是它该管的形态：
    /// 单边持仓到期特征暴露了旧校准的局限，故升格为热配项。true/false
    /// 都是合法值（bool 没有「非法值」可拒）。
    #[serde(default)]
    pub trailing_enabled: Option<bool>,
    /// #935 fix 2：策略出场 maker-first 总开关。**出厂 false**（字节保持）。
    /// on 时：策略出场建议里带 `use_maker` 且 reason 是利润侧
    /// （take_profit / trailing_stop）的平仓先挂 maker（`maker_timeout_ms`
    /// 期满升 taker）；保护侧（stop_loss / time_exit）永不转 maker
    /// （#915 反事实：挂单止损对崩腿 0/2 无效）。
    #[serde(default)]
    pub strategy_exit_maker_first: Option<bool>,
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
            && self.force_exit_sec.is_none()
            && self.trailing_enabled.is_none()
            && self.strategy_exit_maker_first.is_none()
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
/// took the write — the SQLite `account_exit_policy` row when the DB is open
/// (the default since #1199 PR-2; the restart re-resolves the same values
/// from it), the `.env` `BK_EXIT_*` keys on the env-only backend — and
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
