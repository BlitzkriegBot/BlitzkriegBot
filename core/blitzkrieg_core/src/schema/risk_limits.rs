//! Systemic risk readout and hot-write types: `risk.limits`, `risk.setLimits`,
//! `risk.setSystemic`. Moved verbatim from `schema/mod.rs` (pure code motion).

use crate::account::default_account_id;
use crate::exit_policy::ExitConfig;
use crate::risk::limits::{AccountRiskLimits, SystemicRiskLimits};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// The whole `risk.limits` readout: WHAT each systemic limit is and WHERE it
/// came from, plus the exit resolution the entry pipeline binds at Gate 4.
/// Assembled per answer from the LIVE risk config — `risk.setLimits` and
/// `risk.setSystemic` hot-write the matrix, so a boot-frozen snapshot would
/// answer with limits that are no longer in force (the arm takes the Core
/// lock and reads `risk_config()` for exactly this reason).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskLimitsResult {
    /// Readout contract version (§4.4).
    pub version: String,
    /// The per-account matrix, shown for the kernel's default account
    /// (§4.2: every account runs under the SAME configured matrix).
    pub account: RiskAccountLimitsView,
    /// The exit triple Gate 4 binds per entry.
    pub exit: RiskExitView,
    /// Equity-relative per-entry sizing percentage (`BK_SIZE_PCT`, 0 = off).
    /// Additive optional field — omitted when `None` to keep the frozen wire
    /// golden byte-identical; old panels ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_pct: Option<Decimal>,
    /// Concurrent open-position cap (`0` = off / unlimited). Additive optional
    /// field — omitted when `None` to keep the frozen wire golden byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_positions: Option<usize>,
    /// Per-entry share band (`min_shares` / `max_shares`). Additive optional
    /// fields — omitted when `None` to keep the frozen wire golden byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_shares: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_shares: Option<Decimal>,
    /// #277: durability fault state for the target account (ledger journal,
    /// order log, applied-fill idempotency log). When `active == true`, new
    /// entries are refused fail-closed with `RiskRejected` while exits keep
    /// flowing. Additive optional field — omitted when `None` to keep the
    /// frozen wire golden byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence_fault: Option<PersistenceFaultView>,
}

/// #277: per-account persistence durability status exposed by `risk.limits`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PersistenceFaultView {
    pub active: bool,
    pub journal_write_failed: bool,
    pub order_log_write_failed: bool,
    pub applied_fill_log_write_failed: bool,
}

impl RiskLimitsResult {
    /// The live readout for the default account: systemic limits + exit
    /// resolution exactly as they stand in the risk gate RIGHT NOW.
    pub fn snapshot(systemic: &SystemicRiskLimits, exit: &ExitConfig) -> Self {
        Self::snapshot_for(default_account_id().as_str(), systemic, exit)
    }

    /// #271: per-account live readout so `risk.limits` echoes the requested
    /// account's own limits and `account.id` instead of hardcoding `default`.
    pub fn snapshot_for(
        account_id: &str,
        systemic: &SystemicRiskLimits,
        exit: &ExitConfig,
    ) -> Self {
        Self {
            version: "1.1".into(),
            account: RiskAccountLimitsView {
                id: account_id.to_owned(),
                limits: systemic.account_for(account_id).clone(),
            },
            exit: RiskExitView {
                stop_loss_pct: exit.stop_loss_pct,
                take_profit_pct: exit.take_profit_pct,
                force_exit_sec: exit.force_exit_sec,
                trailing_enabled: exit.trailing_enabled,
                holder_stop_pct: exit.holder_stop_pct,
                strategy_exit_maker_first: exit.strategy_exit_maker_first,
            },
            size_pct: None,
            max_positions: None,
            min_shares: None,
            max_shares: None,
            persistence_fault: None,
        }
    }
}

/// One account's slice of the readout.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskAccountLimitsView {
    pub id: String,
    pub limits: AccountRiskLimits,
}

/// The exit resolution Gate 4 binds: stop/take percentages of the entry fill
/// and the hard force-exit deadline. Plain numbers — the IPC contract's usual
/// Decimal convention — NOT the `Bound` envelope: these are behaviour knobs
/// with factory values, not operator limits with provenance.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskExitView {
    #[serde(with = "crate::decimal")]
    pub stop_loss_pct: Decimal,
    #[serde(with = "crate::decimal")]
    pub take_profit_pct: Decimal,
    pub force_exit_sec: i64,
    /// Trailing-stop arming switch, same live semantics as the triple
    /// (hot-writable via `risk.setExit`). Incremental field — old panels
    /// ignore it.
    pub trailing_enabled: bool,
    /// #935 fix 3: the stop width a hold-to-settlement strategy's positions
    /// actually run on (`holder_stop_pct`) — the panel's 12% readout does not
    /// bind holders, and showing only it misstates the kernel's posture.
    /// Read-only display field (the 50% semantics are calibrated, funadmental
    /// change is a separate ruling); old panels ignore it.
    #[serde(with = "crate::decimal")]
    pub holder_stop_pct: Decimal,
    /// #935 fix 2: strategy-exit maker-first gate's current state — the
    /// panel's toggle row arms on this (undefined = old kernel → the Vue
    /// gate keeps the row honestly disabled). Same hot-writable-via-
    /// `risk.setExit` semantics as `trailing_enabled` above; old panels
    /// ignore it.
    #[serde(default)]
    pub strategy_exit_maker_first: bool,
}

/// Params of `risk.setLimits` — the ONLY knobs an operator may change without a
/// restart, and the whole of the safe subset.
///
/// `deny_unknown_fields` is what makes the boundary structural rather than
/// advisory: a field outside this set cannot even deserialize into the type that
/// carries the patch, so no downstream code path can apply one. The named
/// refusals ([`crate::risk::hot_reload_refusal`]) exist on top of it to say WHY
/// for the knobs an operator actually reaches for (breakers, exit thresholds,
/// credentials), instead of a bare "unknown field".
///
/// Every field is optional and every field means "set to this value"; an absent
/// field is left alone. Setting one to `0` is literal, not "disabled" — see each
/// field below.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetRiskLimitsParams {
    /// Absolute cap on ONE order's notional (USD). Read on every order by the
    /// risk gate, so the next order is judged by the new value. `0` binds hard:
    /// every BUY is refused (closing SELLs are exempt by construction, #174).
    #[serde(default, with = "crate::decimal::opt")]
    pub max_order_notional: Option<Decimal>,
    /// The same cap as a percentage of the account's cash equity. `0` = off.
    #[serde(default, with = "crate::decimal::opt")]
    pub max_order_notional_pct: Option<Decimal>,
    /// Cap on TOTAL open notional across all strategies (USD). `0` = off.
    #[serde(default, with = "crate::decimal::opt")]
    pub max_open_notional_usd: Option<Decimal>,
    /// Lower bound of the per-entry share lot.
    #[serde(default, with = "crate::decimal::opt")]
    pub min_shares: Option<Decimal>,
    /// Upper bound of the per-entry share lot.
    #[serde(default, with = "crate::decimal::opt")]
    pub max_shares: Option<Decimal>,
    /// Free-text note from the caller, echoed into the audit line so a log
    /// reader gets the intent beside the numbers.
    #[serde(default)]
    pub reason: Option<String>,
}

impl SetRiskLimitsParams {
    /// True when the patch carries no settable field (a request that would audit
    /// nothing is refused rather than answered with an empty record).
    pub fn is_empty(&self) -> bool {
        self.max_order_notional.is_none()
            && self.max_order_notional_pct.is_none()
            && self.max_open_notional_usd.is_none()
            && self.min_shares.is_none()
            && self.max_shares.is_none()
    }
}

/// One audited field change: old value → new value.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskLimitChange {
    /// The wire (camelCase) name of the field, as the caller spelled it.
    pub field: &'static str,
    #[serde(with = "crate::decimal")]
    pub from: Decimal,
    #[serde(with = "crate::decimal")]
    pub to: Decimal,
}

/// The record an applied `risk.setLimits` returns. The SAME facts go to the run
/// log (one line per changed field, `target: "risk"`), because an audit that
/// only exists in a reply is gone as soon as the caller disconnects.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskLimitUpdate {
    pub applied: Vec<RiskLimitChange>,
    pub at_ms: i64,
    /// Who asked — the peer's uid as recorded by the kernel, not a claim.
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `true` when the kernel has a risk-state file (`risk_state_path`) and
    /// the write landed there — the update recovers across a restart (#861).
    /// `false` for cores without one (backtester, most tests): the update is
    /// then genuinely memory-only, and a restart re-applies the startup
    /// flags/env/TOML. "I changed it" reads as "it stuck" only when this
    /// says so.
    pub persisted: bool,
    /// The same statement in words, for a panel or a log reader.
    pub note: &'static str,
}

/// [`RiskLimitUpdate::note`] — one spelling, used by the reply and the log.
pub const RISK_LIMIT_UPDATE_NOTE: &str = "hot-applied; persisted to the risk-state file when the kernel has one, \
     in-memory only otherwise";

/// `risk.setSystemic` params: the six per-account systemic limits (issue
/// #983: risk follows the account's capital — no global caps); `accountId`
/// picks the `account_risk` row (absent = default). USD / shares / counts /
/// minutes, decimal-STRING wire; `0` = off.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetSystemicLimitsParams {
    #[serde(default)]
    pub account_id: Option<String>,
    /// 单笔最大亏损（USD）：Gate 2 从止损价推导，超额入场缩到帽内。
    #[serde(default, with = "crate::decimal::opt")]
    pub max_single_loss_usd: Option<Decimal>,
    /// 当日最大回撤（USD）：与既有的日亏熔断是同一个预算。**下次会话生效**
    /// —— 当日已发生的已实现亏损账本不能被一个中途改动的帽追溯改判。
    #[serde(default, with = "crate::decimal::opt")]
    pub max_daily_drawdown_usd: Option<Decimal>,
    /// 单仓上限（股）：整数股，超出缩到帽内。
    #[serde(default, with = "crate::decimal::opt")]
    pub max_position_size: Option<Decimal>,
    /// 连亏熔断（次）：**下次会话生效** —— 计数器已在跑，新阈值会追溯改判
    /// 在案连亏是否熔断（HOT_RELOAD_REFUSED 的理由）。
    #[serde(default, with = "crate::decimal::opt")]
    pub max_consecutive_losses: Option<Decimal>,
    /// 熔断冷静期（分钟）：**下次会话生效** —— 与连亏熔断同一裁决。
    #[serde(default, with = "crate::decimal::opt")]
    pub cooldown_minutes: Option<Decimal>,
    /// 单笔下单帽（USD）：账户级，落在 SQLite `account_risk` 行的同名金额
    /// 字段（`StoreApi::save_risk_bounds`；`BK_RISK_MAX_ORDER_NOTIONAL_USD`
    /// 只是重启导入时的读侧拼写），热载入立即生效，重启从库恢复。取代进程级的
    /// `--max-order-notional` CLI 标语（issue #983：风控跟随账户资本）。
    #[serde(default, with = "crate::decimal::opt")]
    pub max_order_notional_usd: Option<Decimal>,
    /// 单笔权益比例（`BK_SIZE_PCT`，百分比 `0..=100`，`0` = 关闭走固定股数/金额）。
    /// 热写入立即生效，持久化至 SQLite `account_settings` 键 `BK_SIZE_PCT`。
    #[serde(default, with = "crate::decimal::opt")]
    pub size_pct: Option<Decimal>,
    /// 最大并发持仓数（`0` = 关闭/不限）。热写入立即生效，持久化至 SQLite
    /// `account_settings` 键 `positions.max_positions`。
    #[serde(default)]
    pub max_positions: Option<usize>,
    /// 单笔最小/最大份额。热写入立即生效，持久化至 SQLite `account_settings`。
    #[serde(default, with = "crate::decimal::opt")]
    pub min_shares: Option<Decimal>,
    #[serde(default, with = "crate::decimal::opt")]
    pub max_shares: Option<Decimal>,
    /// Free-text note from the caller, echoed into the audit line.
    #[serde(default)]
    pub reason: Option<String>,
}

impl SetSystemicLimitsParams {
    /// True when the patch names no field — refused, not answered with an
    /// empty record (the same posture as `risk.setLimits`).
    pub fn is_empty(&self) -> bool {
        self.max_single_loss_usd.is_none()
            && self.max_daily_drawdown_usd.is_none()
            && self.max_position_size.is_none()
            && self.max_consecutive_losses.is_none()
            && self.cooldown_minutes.is_none()
            && self.max_order_notional_usd.is_none()
            && self.size_pct.is_none()
            && self.max_positions.is_none()
            && self.min_shares.is_none()
            && self.max_shares.is_none()
    }
}

/// One field's resolution inside a `risk.setSystemic` reply: old value, new
/// value, and WHEN it takes effect. The effect classes are real, not
/// documentation: the sizing bounds (single loss, position size) are judged
/// per entry LIVE; the drawdown budget and the breaker pair arm at the NEXT
/// KERNEL SESSION (a live change would either rewrite what the day already
/// spent or retroactively judge a running streak).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemicLimitChange {
    /// The wire (camelCase) name of the field.
    pub field: &'static str,
    /// Previous effective value ("0" = off), decimal string.
    pub from: String,
    /// New effective value ("0" = off), decimal string.
    pub to: String,
    /// `live` (judged on the next entry) or `next_session` (armed at boot of
    /// a future kernel run).
    pub effect: &'static str,
}

/// `risk.setSystemic` result: the audited change list, in the
/// `RiskLimitUpdate` shape. `persisted` is `true` when the durable carrier
/// took the write — the SQLite `account_risk` row (`StoreApi::save_risk_bounds`;
/// the only backend since #1199 — the env-only backend was deleted) — and
/// `false` otherwise (no carrier wired — backtester, most
/// tests — or the write failed, which is logged with the path). The panel
/// also receives `persistence: "store" | "none"` beside it, spelled by the
/// write face: `"store"` = the durable carrier took it (carrier-neutral since
/// #1199 PR-2 — the assembly decides which one), `"none"` = nothing landed
/// durably.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemicLimitUpdate {
    pub applied: Vec<SystemicLimitChange>,
    pub at_ms: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub persisted: bool,
    pub note: &'static str,
}

/// [`SystemicLimitUpdate::note`] — one spelling, used by the reply and the log.
pub const SYSTEMIC_LIMIT_UPDATE_NOTE: &str = "single-loss + position-size live, drawdown budget + breaker pair arm \
     next session; persisted to the kernel's store (SQLite when the DB is \
     open, the .env BK_RISK_* keys otherwise) — in-memory only when no \
     carrier is wired";
