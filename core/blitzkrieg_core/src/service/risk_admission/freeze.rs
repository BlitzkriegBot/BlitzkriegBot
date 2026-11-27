use super::*;
use crate::account::AccountStatus;
use crate::contracts::{Halt, HaltScope, VenueId};
use serde::Serialize;

/// 场地级 halt（#187）：一个 venue 的健康失败只能停这个 venue。
///
/// **废除**的是「venue 故障 = 系统故障」这条单 venue 时代的映射。审计实测：
/// `on_venue_self_check` / `on_venue_reconcile_failed` 在
/// `account_ids_bound_to_venue` 为空时调 `freeze_trading` →
/// `risk.kill + GLOBAL_KILL_ACCOUNT`，于是一个 venue 的网络抖动停掉**全部**
/// venue 的全部账户的全部新入场（已有仓位仍可平），直到操作员 `risk.resume`
/// 或重启。这是用最粗的刀切最细的问题 —— 跨账户日亏熔断早已按
/// `(strategy, account)` 隔离，venue 健康失败却是全局。
///
/// 用 [`Halt`] + [`HaltScope::Venue`] 取代：作用域是**必填字段**，构造一个
/// 「没有作用域」的 halt 在类型上不可能。没有任何账户绑定该 venue 时，正确
/// 输出是一条 unattributed 告警，**不是**升级为全局。
///
/// `GlobalOperator` 仍然存在，且只能由操作员的 `risk.kill` 产生 —— 人工
/// 总闸不能被这次收窄带走。
///
/// 与 `account_gates` 的关系：这张表是 **venue 轴**，`account_gates` 是
/// **账户轴**，两者都在 `account_gate_permits` 里读。分开存是因为 venue
/// halt 必须能在「没有任何账户绑定该 venue」时存在（那种情况下没有任何
/// 账户键可以承载它）。
///
/// **判定用的是「这笔单将在哪个 venue 执行」，不是「账户绑定在哪个 venue」。**
/// 两者在「账户无绑定」时不同：`refuse_cross_venue_order` 对空绑定直接放行，
/// 随后 `execution_venue_of` 按 token 的唯一候选把它们路由到**真实**的 venue。
/// 若闸门只看绑定，一个未绑定账户的单子会绕过故障 venue 的 halt —— 而它恰恰
/// 要去那里下单。所以 `account_gate_permits` 读的是执行 venue，未绑定账户的
/// 订单在归因不到 venue 时才按「无 venue 身份」放行（未知维度不用于拒绝，
/// 与 #174 同向）。
#[derive(Debug, Clone, Default)]
pub(crate) struct VenueHalts {
    /// venue → 现行 halt。键是 venue 名，**不是**账户：一个 venue 一份。
    halts: HashMap<String, ActiveVenueHalt>,
}

/// 表内一行：[`Halt`]（契约读出面）加上柜面自用的上锁时点。
///
/// `since_ms` 不是 `Halt` 的字段（契约冻结，且 `until_ms` 是「何时自行失效」，
/// 另一个意思）。它有两个用途，都不是装饰：操作员要看到「这个 venue 坏了
/// 多久」，以及场地恢复后的自愈去抖（`FREEZE_AUTO_LIFT_MIN_MS`，与账户轴
/// 同一条纪律 —— 一个抖动的 venue 不该每个周期都开合一次）。
#[derive(Debug, Clone)]
struct ActiveVenueHalt {
    halt: Halt,
    since_ms: i64,
}

impl VenueHalts {
    /// 记录一条 venue 级 halt。返回 `true` 当这是新上锁（0 → 1），调用方
    /// 据此决定是否告警 —— 一场持续的 venue 故障不该每次探测都重印一行。
    ///
    /// 只接受 [`HaltScope::Venue`]：venue 轴的表里放不下别的作用域，类型上
    /// 就挡住「venue 故障升级为全局」这条路径。
    ///
    /// venue halt 没有时钟驱动的失效：它只由「恢复健康」或操作员解除。
    /// 所以 `until_ms` 恒为 `None` —— 一个 venue 的故障不会自己变好，给个
    /// 倒计时反而是编造一个没人观测过的恢复时间。
    pub(crate) fn halt(&mut self, venue: &str, reason: String, now_ms: i64) -> bool {
        if venue.is_empty() {
            // 空 venue 不是「未知 venue 的 halt」—— 它是一个没有身份的失败，
            // 落进这张表会让下一笔真实 venue 的判定查询到它。调用方必须先
            // 证明手里有 venue id（`venue_of`/报告自带），否则这是调用方的
            // 编程错误：不写入，由调用方的测试/断言去钉住。这里**不** panic：
            // 一个空串不该让内核在运行时崩掉，它只要不生效就够了。
            return false;
        }
        let is_new = !self.halts.contains_key(venue);
        // 同一 venue 再次失败时保留首次的原因与时点（与 `set_account_gate` 同
        // 纪律：重复上锁不覆盖根因；时点也要保留，否则自愈去抖会被每次失败
        // 重新计时，一个持续故障就永远等不到 debounce 满）。
        self.halts
            .entry(venue.to_string())
            .or_insert(ActiveVenueHalt {
                halt: Halt {
                    scope: HaltScope::Venue(VenueId(venue.to_string())),
                    reason,
                    until_ms: None,
                },
                since_ms: now_ms,
            });
        is_new
    }

    /// 该 venue 当前是否被 halt。过期视为未 halt（自动到期，无需清理动作）。
    pub(crate) fn is_halted(&self, venue: &str, now_ms: i64) -> bool {
        if venue.is_empty() {
            // 空 venue = 未声明（旧语料、engine 未起）。没有身份就没有 halt
            // 判定，如实放行 —— 这正是 #174 类缺陷的反面：不要用一个未知
            // 维度去拒绝。
            return false;
        }
        self.halts
            .get(venue)
            .is_some_and(|e| e.halt.until_ms.is_none_or(|until| now_ms < until))
    }

    /// 该 venue 的现行 halt（含原因），供面板与回执读。过期的不返回。
    pub(crate) fn active(&self, venue: &str, now_ms: i64) -> Option<&Halt> {
        let e = self.halts.get(venue)?;
        if e.halt.until_ms.is_some_and(|until| now_ms >= until) {
            return None;
        }
        Some(&e.halt)
    }

    /// 上锁时点（自愈去抖用）。未上锁返回 `None`。
    pub(crate) fn since_ms(&self, venue: &str) -> Option<i64> {
        self.halts.get(venue).map(|e| e.since_ms)
    }

    /// 解除一个 venue 的 halt（venue 恢复健康，或操作员显式解除）。
    /// 返回被清除的 halt。
    pub(crate) fn clear(&mut self, venue: &str) -> Option<Halt> {
        self.halts.remove(venue).map(|e| e.halt)
    }

    /// 解除全部 halt（操作员 `risk.resume`：总闸解除时，自动 venue halt 也
    /// 一并让路 —— 否则 #187 之前的「操作员能把内核弄回可用」这条杠杆就没了）。
    /// 返回被清除的条数。
    pub(crate) fn clear_all(&mut self) -> usize {
        let n = self.halts.len();
        self.halts.clear();
        n
    }

    /// 全部**生效中**的 halt。按 venue 名排序：来源是 `HashMap`，遍历序每进程
    /// 随机，会抖的报告没法逐行 diff。过期条目不出现在回执里。
    ///
    /// 今日只有内核测试读它：面板读出要另开 issue（`engine.stats` 是带字节级
    /// fixture 的冻结契约，本 PR 不许加键）。故这里 `cfg(test)` —— 非测试构建
    /// 不留未用访问器（R8：新增债务为 0）。加面板面时连同 `Core::venue_halt`
    /// 一起去掉这个门控。
    #[cfg(test)]
    pub(crate) fn active_halts(&self, now_ms: i64) -> Vec<&Halt> {
        let mut live: Vec<(&String, &Halt)> = self
            .halts
            .iter()
            .filter(|(_, e)| e.halt.until_ms.is_none_or(|until| now_ms < until))
            .map(|(venue, e)| (venue, &e.halt))
            .collect();
        live.sort_by(|a, b| a.0.cmp(b.0));
        live.into_iter().map(|(_, h)| h).collect()
    }

    /// 测试与诊断用：表内条目数（含已过期待清理项）。
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.halts.len()
    }
}

/// 账户入场闸门（#1045）：每账户一份，**纯内存、永不落盘**，进程重启即清，
/// 唯一由 `account.freeze` / `account.unfreeze`、日内亏损熔断和场地自愈三条
/// 路由改写。所有入场路径只读 `gate.blocked`；出场永不阻断。
///
/// 取代旧 `FreezeBook` 里的 `account_freezes` + `auto_frozen` 两张持久表：
/// 不再有任何冻结姿态随进程重启而重播。`realized_pnl_usd` 继续持久化（资金
/// 真相），重启只清闸门；`tripped` 故不持久化，重启不自动重触发日内
/// 熔断，下一笔使亏损超限的成交才会重新上 `DailyLoss` 闸门。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccountGate {
    pub(crate) blocked: bool,
    /// 运营可读的原因文案（算法给定：如 "daily loss limit reached" / 手动冻结
    /// 的 operator 原因 / 场地拒单原因）。
    pub(crate) reason: String,
    /// 原因种类，决定优先级与谁可以自解。
    pub(crate) reason_kind: GateReason,
    /// 闸门首次上锁的时点（ms），供 UI 倒计时与审计。
    pub(crate) since_ms: i64,
    /// 自动到期点（ms）：`DailyLoss` 是下一个 UTC 日零点；`None` = 仅由解冻
    /// 操作或配置改写清除（`Manual`、`ConfigReadOnly`、`ConfigSuspended`）。
    pub(crate) expires_at_ms: Option<i64>,
    pub(crate) actor: String,
    /// 导致该闸门的场地（VenueRejection 时有值，其他闸门为 None）。
    /// 用于场地自愈时只解除该场地造成的冻结，避免误解除其他场地导致的冻结。
    pub(crate) venue: Option<String>,
}

/// 闸门原因，声明顺序即优先级从低到高：
/// `VenueRejection < DailyLoss < ConfigReadOnly < ConfigSuspended <
/// TradingDisabled < Manual < KillSwitch`。
///
/// 更高优先级的闸门可覆盖更低的；较低优先级的路由永远不会静默解除更高
/// 优先级的闸门（场地健康探针只解 `VenueRejection`，碰不着 `Manual`/`DailyLoss`/`KillSwitch`）。
///
/// `ConfigReadOnly` / `ConfigSuspended` 由账户配置态（read_only /
/// suspended）产生；`TradingDisabled` 由交易开关 `trading_enabled = false`
/// 产生；`Manual` 由操作员 `account.freeze {frozen: true}` 产生；
/// `KillSwitch` 是进程级全局熔断（`risk.kill` / `risk.resume`）。配置态
/// 闸门只在启动时从 `.env` 再推导（#1057），`unfreeze` 只清运行时
/// 路由设置的闸门（Manual / DailyLoss / VenueRejection），不清配置态。
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum GateReason {
    /// 场地拒单/自检失败/sweep 连击造成的自动冻结：场地健康探针可自愈。
    VenueRejection,
    /// 日内亏损熔断：UTC 日边界自动到期，无需操作。
    DailyLoss,
    /// 账户配置为 read_only：交易开关关，新单拒、平仓放行。
    ConfigReadOnly,
    /// 账户配置为 suspended：同上，且带运营原因。
    ConfigSuspended,
    /// 账户交易开关关闭：新单拒、平仓放行；由 `account.trading.set` 写
    /// 回 `.env` 后重启再生，运行时仅反映当前开关状态。
    TradingDisabled,
    /// 操作员 `account.freeze {frozen: true}` 手动冻结：仅 `account.unfreeze`
    /// / 手动解冻可解，健康探针与日内熔断均不得碰。
    Manual,
    /// 进程级全局熔断（`risk.kill`）：对全账户拒新单、放平仓，纯内存、
    /// 重启即清，`risk.resume` 或 `unfreeze {target: "global"}` 解除。
    KillSwitch,
    /// 启动时风险态文件不可读，按失败关闭冻结全部账户：系统级 fail-closed，
    /// 绝不由操作员/场地自愈自解，需 `risk.resume` 或下次文件可读的 boot
    /// 恢复（issue #861 的 corrupt-boot 条款，不复用 `Manual`）。
    BootUnverifiable,
}

impl GateReason {
    pub(crate) fn priority(&self) -> u8 {
        match self {
            GateReason::VenueRejection => 0,
            GateReason::DailyLoss => 1,
            GateReason::ConfigReadOnly => 2,
            GateReason::ConfigSuspended => 3,
            GateReason::TradingDisabled => 4,
            GateReason::Manual => 5,
            GateReason::KillSwitch => 6,
            GateReason::BootUnverifiable => 7,
        }
    }

    /// 统一措辞，供面板与回执文案；前端不得据此推断 `kind` 之外的语义，
    /// 也不得编造恢复时间--恢复时间只读内核 `expires_at_ms`。
    pub(crate) fn label(&self) -> &'static str {
        match self {
            GateReason::VenueRejection => "venue rejection",
            GateReason::DailyLoss => "daily loss limit",
            GateReason::ConfigReadOnly => "account read-only",
            GateReason::ConfigSuspended => "account suspended",
            GateReason::TradingDisabled => "trading disabled",
            GateReason::Manual => "manual freeze",
            GateReason::KillSwitch => "kill switch",
            GateReason::BootUnverifiable => "risk state unverifiable",
        }
    }
}

/// 进程级全局熔断闸门的账户键：不与任何配置账户 id 冲突，仅供
/// `risk.kill` / `risk.resume` / `unfreeze {target: "global"}` 使用。
pub(crate) const GLOBAL_KILL_ACCOUNT: &str = "global";

/// 解冻操作的内核回执（#1045 API：`account.unfreeze`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UnfreezeResult {
    pub(crate) unfrozen: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) previous_reason: Option<String>,
    /// 解冻后该账户是否可开新单（本闸已清且无更高优先级闸门）。
    pub(crate) entries_allowed: bool,
}

/// 冻结/熔断剩余状态簿（#834 收拢后的上帝结构一环）：`account_freezes` 与
/// `auto_frozen` 已被 `Core.account_gates` 单一闸门取代（#1045），本簿只保
/// 留与账户无关的全局/场地维连击与自由现金事实。
#[derive(Default)]
pub(crate) struct FreezeBook {
    pub(crate) consecutive_venue_rejects: u32,
    pub(crate) consecutive_venue_rejects_by_account: HashMap<String, u32>,
    pub(crate) venue_free_by_venue: HashMap<String, (i64, Decimal)>,
    pub(crate) consecutive_sweep_failures: u32,
    pub(crate) consecutive_sweep_failures_by_venue: HashMap<String, u32>,
}

impl FreezeBook {
    /// 单账户重置（`trading_reset` 路径）：该账户的按账户拒单 streak 移除；
    /// 全局拒单与 sweep streak 归零（一次账户级重置视为一次新鲜的开始）。
    /// by_venue sweep streak 不在此清--场地维的失败事实不属于账户重置。
    /// 账户入场闸门由 `Core` 同步清除（`reset_for_account` 调用方自行决定）。
    pub(crate) fn reset_for_account(&mut self, account_id: &str) {
        self.consecutive_venue_rejects = 0;
        self.consecutive_venue_rejects_by_account.remove(account_id);
        self.consecutive_sweep_failures = 0;
    }
}

impl Core {
    /// 单账户入场闸门（只读）。
    pub(crate) fn account_gate(&self, account: &str) -> Option<&AccountGate> {
        self.account_gates.get(account)
    }

    /// 全部闸门（stats 读出与 UI 呈现）。
    pub(crate) fn account_gates(&self) -> &HashMap<String, AccountGate> {
        &self.account_gates
    }

    /// 该账户当前是否被闸门拒绝入场。已过 `expires_at_ms` 的闸门按不阻断
    /// 处理（自动到期，无需额外清理动作）。
    pub(crate) fn account_gate_blocked(&self, account: &str, now_ms: i64) -> bool {
        let Some(g) = self.account_gates.get(account) else {
            return false;
        };
        g.blocked && g.expires_at_ms.is_none_or(|expires| now_ms < expires)
    }

    /// 设置或升级一账户的闸门。新原因的优先级 **严格高于** 现存**未过期**
    /// 闸门时才覆盖（先到者胜、更高优先级者覆盖），防止低优先级路由
    ///（如场地自愈）静默解除高优先级闸门（`Manual`/`DailyLoss`）。已过期
    /// 的闸门（DailyLoss 过 UTC 日翻界）不阻挡新闸门：过期的 DailyLoss(1)
    /// 不得吞掉随后的 VenueRejection(0)。返回是否写入。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn set_account_gate(
        &mut self,
        account: &str,
        reason: String,
        reason_kind: GateReason,
        expires_at_ms: Option<i64>,
        actor: &str,
        now_ms: i64,
        venue: Option<&str>,
    ) -> bool {
        let should_set = match self.account_gates.get(account) {
            None => true,
            Some(existing) => {
                // 已过期的闸门（expires_at_ms 已到）不再生效，视为不存在，
                // 避免过期的 DailyLoss(1) 吞掉随后的 VenueRejection(0)。
                let existing_live = existing
                    .expires_at_ms
                    .is_none_or(|expires| now_ms < expires);
                existing_live && reason_kind.priority() > existing.reason_kind.priority()
            }
        };
        if !should_set {
            return false;
        }
        self.account_gates.insert(
            account.to_string(),
            AccountGate {
                blocked: true,
                reason,
                reason_kind,
                since_ms: now_ms,
                expires_at_ms,
                actor: actor.to_string(),
                venue: venue.map(|v| v.to_string()),
            },
        );
        true
    }

    /// 清除一账户的闸门（解冻）。返回被清除的闸门，供调用方审计与回执。
    pub(crate) fn clear_account_gate(&mut self, account: &str) -> Option<AccountGate> {
        self.account_gates.remove(account)
    }

    /// 该账户的冻结原因文案（仅在被闸门阻断时有意义），供 `account.list`/
    /// `account.freeze` 回执与面板呈现。
    pub(crate) fn account_freeze_reason(&self, account: &str, now_ms: i64) -> Option<String> {
        self.account_gate(account).and_then(|g| {
            if g.blocked && g.expires_at_ms.is_none_or(|expires| now_ms < expires) {
                Some(g.reason.clone())
            } else {
                None
            }
        })
    }

    /// 入场闸判定：账户被闸门阻断，且意图不是平仓（平仓腿永远放行，#174
    /// 逃生舱，按账户独立计）。新单拒、平仓放行。
    ///
    /// 两条依据：
    /// 1. `account_gates` 内存表（由 `emit_daily_trip`、手动冻结、
    ///    场地自愈、进程级熔断设置） - 运行时闸门；
    /// 1. 进程级全局熔断闸门 `GLOBAL_KILL_ACCOUNT` - 对全账户生效。
    /// 2. **venue 轴**（#187）：这笔单的执行 venue 被 halt 时拒 —— 只拒这个
    ///    venue 上的新入场，别的 venue 照常。
    ///
    /// `ConfigReadOnly` 是唯一也拦平仓的姿态（`ReadOnly` 配置态映射其上；
    /// `Frozen`/`TradingDisabled`/`Manual`/`KillSwitch` 均放行平仓，绝不
    /// 困住仓位）。**不**回退到持久化的 `daily_loss_tripped_for` 闩锁：
    /// 重启后 `tripped` 不持久化，故重启不自动重触发日内熔断；解冻后
    /// 当日余下时间可交易。这是 #1045 裁决 v3：「删除 tripped* 落盘字段、
    /// 删读时重闩、重启/解冻后当日不再自动重触发」。
    pub(crate) fn account_gate_permits(&self, req: &OrderRequest, now_ms: i64) -> CoreResult<()> {
        let account_id = req.account_id.as_str();
        let is_close = crate::risk::is_close_intent(&req.internal_key);

        // 进程级全局熔断：对每个账户的新单都拒，平仓永远放行。
        if let Some(gate) = self.account_gate(GLOBAL_KILL_ACCOUNT)
            && gate.blocked
            && gate.expires_at_ms.is_none_or(|expires| now_ms < expires)
            && !is_close
        {
            return Err(CoreError::new(
                CoreErrorCode::KillSwitchActive,
                format!(
                    "kill switch active ({}) - new orders refused on all accounts, exits keep flowing",
                    gate.reason
                ),
            ));
        }

        // #277: fail-closed on persistence faults (ledger journal / order log /
        // applied-fill idempotency log write failure). New entries are refused
        // with `RiskRejected` while exits keep flowing so held positions are
        // never trapped.
        if !is_close && let Some(fault) = self.persistence_fault_reason(account_id) {
            return Err(CoreError::new(
                CoreErrorCode::RiskRejected,
                format!(
                    "account `{account_id}` persistence fault ({fault} write failed) — \
                     account marked non-durable; new entries refused (fail-closed, exits keep flowing)"
                ),
            ));
        }

        // #187: venue 轴。执行 venue 被 halt → 只拒这个 venue 的入场，平仓
        // 照旧（halt 永远不是拆仓位）。
        if !is_close {
            let venue = self.venue_in_scope_of(account_id, &req.token_id);
            if self.venue_halts.is_halted(&venue, now_ms)
                && let Some(halt) = self.venue_halts.active(&venue, now_ms)
            {
                return Err(CoreError::new(
                    CoreErrorCode::MarketHalted,
                    format!(
                        "venue `{venue}` is halted (scope venue): {} - new orders on this venue \
                         are refused, other venues and every exit keep flowing",
                        halt.reason
                    ),
                ));
            }
        }

        if let Some(gate) = self.account_gate(account_id)
            && gate.blocked
            && gate.expires_at_ms.is_none_or(|expires| now_ms < expires)
        {
            // ConfigReadOnly 是唯一也拦平仓的姿态；其余闸门（Frozen /
            // TradingDisabled / Manual / VenueRejection / DailyLoss / KillSwitch）
            // 均放行平仓，绝不因闸门而困仓。
            let blocks_closes = matches!(gate.reason_kind, GateReason::ConfigReadOnly);
            if !is_close || blocks_closes {
                let message = match gate.reason_kind {
                    GateReason::DailyLoss => format!(
                        "Daily loss limit for account {} - entries frozen until the next UTC day (exits keep flowing)",
                        account_id
                    ),
                    _ => format!(
                        "account `{}` is frozen (account-scoped gate, {}): {} - {}",
                        account_id,
                        gate.reason_kind.label(),
                        gate.reason,
                        if is_close {
                            "closes refused too (read_only)"
                        } else {
                            "new orders refused, exits keep flowing"
                        }
                    ),
                };
                return Err(CoreError::new(CoreErrorCode::AccountLimit, message));
            }
        }

        Ok(())
    }

    /// #187: the venue an order for `token` under `account` is in scope of.
    ///
    /// Two sources, in order, and the ORDER MATTERS:
    ///
    /// 1. The account's own binding when it has one. `refuse_cross_venue_order`
    ///    has already refused a bound account's order for a token that routes
    ///    elsewhere, so a bound account's order can only ever execute on its
    ///    own venue — that is the venue whose halt must judge it, and it is
    ///    answerable with no engine attached (which is what makes this gate
    ///    total rather than best-effort).
    /// 2. The engine's execution route, for UNBOUND accounts. An unbound
    ///    account passes `refuse_cross_venue_order` untouched and is then
    ///    routed by token (`Engine::execution_venue_for`), so reading only the
    ///    binding would let its order walk into a halted venue. This needs a
    ///    live engine; with none attached there is no route to check, and the
    ///    empty answer admits — an unknown dimension never refuses (#174).
    fn venue_in_scope_of(&self, account_id: &str, token_id: &str) -> String {
        if let Some(account) = self.accounts.account(&AccountId::from(account_id))
            && !account.venue.is_empty()
        {
            return account.venue.clone();
        }
        self.execution_venue_of(account_id, token_id)
    }

    /// #187: the venue axis, read-only, for in-kernel readers. Tests today; a
    /// panel readout needs its own issue — `engine.stats` is a frozen contract
    /// with a byte-exact fixture and must not grow a key here. The `cfg(test)`
    /// gate is what keeps the non-test build free of a dead accessor (R8); the
    /// admission check itself reads `venue_halts` DIRECTLY, so gating the
    /// reader removes nothing from the enforcement path.
    #[cfg(test)]
    pub(crate) fn venue_halt(&self, venue: &str, now_ms: i64) -> Option<&Halt> {
        self.venue_halts.active(venue, now_ms)
    }

    /// #187: every ACTIVE venue halt, venue-name sorted (a `HashMap` source
    /// would shuffle between runs and defeat diffing).
    #[cfg(test)]
    pub(crate) fn active_venue_halts(&self, now_ms: i64) -> Vec<&Halt> {
        self.venue_halts.active_halts(now_ms)
    }

    /// Freeze ONE account (operator route, `account.freeze {frozen: true}`):
    /// `Manual` 闸门，优先级最高，健康探针与日内熔断均不得自解。会话内
    /// 立即生效，重启即清，永不落盘。同一账户重复冻结保留首次原因。
    pub fn freeze_account(&mut self, account: &str, reason: String) {
        let now_ms = blitzkrieg_market_api::net::now_ms();
        let _ = self.set_account_gate(
            account,
            reason.clone(),
            GateReason::Manual,
            None,
            &format!("operator:{account}"),
            now_ms,
            None,
        );
        tracing::error!(account = %account, reason = %reason, "account frozen (manual gate, in-memory only, restart-clear)");
    }

    /// The AUTOMATIC freeze route (venue-rejection streak etc.): `VenueRejection`
    /// 闸门，优先级最低，仅在账户当前无同等或更高优先级闸门时上锁；场地
    /// 健康探针可解（仅此一条），`Manual`/`DailyLoss` 绝不自解。不登记任何
    /// 持久态。
    pub(crate) fn freeze_account_auto(
        &mut self,
        account: &str,
        reason: String,
        venue: Option<&str>,
        now_ms: i64,
    ) {
        let _ = self.set_account_gate(
            account,
            reason.clone(),
            GateReason::VenueRejection,
            None,
            &format!("auto:{account}"),
            now_ms,
            venue,
        );
        tracing::warn!(account = %account, reason = %reason, venue = ?venue, "account auto-frozen (venue rejection gate, in-memory only)");
    }

    /// `account.freeze {frozen: true}` 的显式运营路由：`Manual` 闸门，与
    /// [`Self::freeze_account`] 同一优先级，保留为调用方语义清晰的别名。
    pub fn freeze_account_manual(&mut self, account: &str, reason: String) {
        self.freeze_account(account, reason);
    }

    /// Lift one account's gate (the internal helper behind `account.unfreeze`
    /// and the reset path). `account.freeze {frozen: false}` is RETIRED
    /// (#1059) — the handler refuses it, so this is not reachable from that
    /// verb. Unknown account = no-op.
    pub fn resume_account(&mut self, account: &str) -> bool {
        self.clear_account_gate(account).is_some()
    }

    /// #1045 API: 解冻一账户并返回内核回执 `{unfrozen, previousReason, entriesAllowed}`，
    /// 并写审计日志（actor/reason/时间）。解冻只放行入场，不碰账本任何数字。
    ///
    /// #1057（裁决 A 真单门）：
    /// - 阻断 1：先 persist 配置态（把 `trading_enabled` 置 true、`status`
    ///   置 `Active`，落盘到 SQLite `accounts` 行——操作者裁决 2026-11-14
    ///   后 `.env` 对内核只读，`StoreApi::save_account_active` 一次
    ///   UPDATE 同时翻正两列），**再** clear gate。persist 失败则
    ///   什么都不变，gate 仍在，绝不假成功。
    /// - 阻断 3：全局熔断键 `"global"` 解冻时同时 `risk.resume()`，使
    ///   tradingFrozen 与 accountGates 单点化。
    /// - 阻断 6：`entries_allowed` 仅由 gate 得出（clear 后读
    ///   `account_gate_blocked`），不再并读 `entry_open`。
    pub(crate) fn unfreeze_account(
        &mut self,
        account: &str,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<UnfreezeResult> {
        let id = crate::model::AccountId::from(account);

        // #1057 阻断 3：全局熔断解冻同时清 risk 标志。
        if account == GLOBAL_KILL_ACCOUNT && self.account_gate(account).is_some() {
            self.risk.resume();
        }

        // #1057 阻断 1 & 4：先 persist 配置态（unfreeze 写回 Active /
        // trading_enabled=true 到 `.env`——唯一事实源；遗留 accounts.toml
        // 存在时写入被拒，issue 751），失败则什么都不变、不 clear gate。
        if account != GLOBAL_KILL_ACCOUNT
            && let Some(acct) = self.accounts.account(&id)
            && (!acct.trading_enabled || !acct.status.allows_entries())
        {
            self.set_account_status_persisting(
                &id,
                AccountStatus::Active,
                actor,
                "unfreeze",
                now_ms,
            )?;
        }

        // 然后 clear gate：先 persist 后 clear，失败则 gate 仍在。
        // 配置态闸门（read_only / suspended / trading disabled）不可由
        // account.unfreeze 解除：改 .env 并重启。明确拒绝，不得假成功。
        if let Some(g) = self.account_gate(account)
            && matches!(
                g.reason_kind,
                GateReason::ConfigReadOnly
                    | GateReason::ConfigSuspended
                    | GateReason::TradingDisabled
            )
        {
            return Err(CoreError::new(
                blitzkrieg_market_api::CoreErrorCode::AccountLimit,
                format!(
                    "account {account} gate is {}: configuration posture, not a risk freeze; edit .env and restart instead",
                    g.reason_kind.label()
                ),
            ));
        }
        let previous = self.clear_account_gate(account);
        let previous_reason = previous.as_ref().map(|g| g.reason.clone());

        // 清 session trip latch：解冻后当日余下时间可交易，不再被旧 latch 重拒。
        self.positions.clear_daily_tripped_for(&id);

        // 阻断 6：entries_allowed 仅由 gate 得出（clear 后复读），显示
        // 与执行同源，不再并读 entry_open。
        let entries_allowed = !self.account_gate_blocked(account, now_ms);

        if previous.is_some() {
            tracing::info!(
                actor = %actor,
                account = %account,
                previous_reason = ?previous_reason,
                entries_allowed,
                "account unfrozen (config written back first, then gate cleared; realized ledger untouched)"
            );
        }
        Ok(UnfreezeResult {
            unfrozen: previous.is_some(),
            previous_reason,
            entries_allowed,
        })
    }

    /// 冻结自愈的解冻半边（#1045：只清 `VenueRejection`，不得清 `Manual`/`DailyLoss`。
    /// 手动冻结与日内熔断均不属于「场地不健康」的自愈范围。
    ///
    /// 每个调用方（reconcile sweep、venue ack）先等出 `FREEZE_AUTO_LIFT_MIN_MS`
    /// (#867) 再来，避免波动期内的反复解锁。
    ///
    /// `venue` 是导致冻结的场地。只有当闸门的 venue 与 `venue` 匹配（或闸门
    /// 未记录 venue，即未知来源）时才会解冻，避免场地 A 的冻结被场地 B 的
    /// 自愈误解。
    pub(crate) fn auto_lift_if_venue_healthy(
        &mut self,
        account: &str,
        venue: Option<&str>,
        now_ms: i64,
    ) {
        let Some(gate) = self.account_gate(account) else {
            return;
        };
        if gate.reason_kind != GateReason::VenueRejection {
            return;
        }
        // 只解除该场地导致的冻结。闸门未记录 venue（历史数据 / 未绑定 venue 的账户 / 遗留全局冻结）时仍按旧逻辑解冻；
        // 只有两边都有值且不匹配时才拒绝，避免场地 A 的冻结被场地 B 的自愈误解。
        if gate.venue.is_some() && gate.venue.as_deref() != venue {
            return;
        }
        if now_ms - gate.since_ms < crate::service::FREEZE_AUTO_LIFT_MIN_MS {
            return;
        }
        let since_ms = gate.since_ms;
        self.clear_account_gate(account);
        let minutes = (now_ms - since_ms).max(0) / 60_000;
        tracing::warn!(
            account = %account,
            frozen_duration_ms = now_ms - since_ms,
            venue = ?venue,
            "venue-rejection gate auto-lifted - venue proved healthy"
        );
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            message: format!(
                "自动冻结已解除（{}）：账户 {account} 场地拒单闸门 {minutes} 分钟后场地恢复",
                venue.unwrap_or("unknown"),
            ),
        });
    }

    /// #187: lift a venue halt once the venue has proved itself healthy.
    ///
    /// Same debounce as the account axis ([`FREEZE_AUTO_LIFT_MIN_MS`]): a venue
    /// that fails, recovers, fails again must not open and close its gate every
    /// sweep interval. `venue` is the reporting/swept venue — the caller only
    /// calls this with evidence that venue's transport answered.
    pub(crate) fn auto_lift_venue_halt(&mut self, venue: &str, now_ms: i64) -> bool {
        if venue.is_empty() {
            return false;
        }
        let Some(since_ms) = self.venue_halts.since_ms(venue) else {
            return false;
        };
        if now_ms - since_ms < crate::service::FREEZE_AUTO_LIFT_MIN_MS {
            return false;
        }
        let Some(halt) = self.venue_halts.clear(venue) else {
            return false;
        };
        let minutes = (now_ms - since_ms).max(0) / 60_000;
        tracing::warn!(
            venue = %venue,
            halted_duration_ms = now_ms - since_ms,
            "venue halt auto-lifted - venue proved healthy"
        );
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            message: format!(
                "场馆 {venue} 的 halt 已解除（{minutes} 分钟后场馆恢复）：{}",
                halt.reason
            ),
        });
        true
    }

    /// Freeze trading (kill switch) with a reason the panel can show. A
    /// restart or an explicit `risk.resume` clears it; the startup self-check
    /// re-probes either way. The freeze is logged at error level (#184) - a kill
    /// switch is the loudest thing this kernel does, and every path that can
    /// reach it (venue-refusal streak, failed self-check, failed sweep, audit
    /// halt) must be countable in a run log.
    ///
    /// It deliberately does NOT write the error slot: the slot keeps the ROOT
    /// CAUSE (the raw venue error that pushed a streak over the line), and
    /// `engine.stats.tradingFrozen.reason` already carries this message for the
    /// panel's freeze banner. Overwriting the slot here is what would make it
    /// show a paraphrase instead of the error an operator has to act on.
    pub(crate) fn freeze_trading(&mut self, reason: String, now_ms: i64) {
        if self.risk.is_killed() {
            return; // already frozen - do not re-alarm on every rejection
        }
        self.risk.kill(reason.clone());
        // #1056: the kill switch is pure in-memory - it never outlives the
        // process; a restart comes up clear and the operator re-arms it via
        // `risk.resume` / `unfreeze {target: "global"}`.
        let _ = self.set_account_gate(
            GLOBAL_KILL_ACCOUNT,
            reason.clone(),
            GateReason::KillSwitch,
            None,
            "system",
            now_ms,
            None,
        );
        self.persist_risk_state();
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            message: format!("trading frozen: {reason}"),
        });
        tracing::error!(reason = %reason, "trading frozen (kill switch)");
        self.emit_error_event(crate::model::CoreError::new(
            blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            reason,
        ));
    }

    pub(crate) fn emit_order(&self, id: &str) {
        if let Some(o) = self.ome.get(id) {
            // Durable persistence: every status change flows through here (submit /
            // confirm / reject / bind / cancel), so the order log always reflects
            // the latest state and a restart can rebuild it (orphan guard).
            if let Err(e) = self.persist_order(id) {
                tracing::error!(order = %id, error = %e, "order persist failed — order kept in memory, not durable");
            }
            self.emit(Event::OrderUpdate { order: o.clone() });
        }
    }

    pub(crate) fn emit_fill(&self, d: FillDelta) {
        if let Some(o) = self.ome.get(&d.order_id) {
            if let Err(e) = self.persist_order(&d.order_id) {
                tracing::error!(order = %d.order_id, error = %e, "order persist failed — order kept in memory, not durable");
            }
            self.emit(Event::Fill {
                delta: d.into(),
                order: o.clone(),
            });
        }
    }

    pub(crate) fn new_order_id(&mut self) -> String {
        // "live" is reserved for orders that can actually reach a venue. A
        // read-only order is not live - stamping it `live_` would put a
        // misleading label on the one field an operator reads to decide what is
        // real, so it is named for what it is.
        let prefix = match self.config.mode {
            Mode::Live => "live",
            Mode::ReadOnly => "readonly",
            Mode::Dry => "dry",
        };
        let id = format!("{prefix}_{}", self.next_id);
        self.next_id += 1;
        id
    }

    // ── Risk ───────────────────────────────────────────────────────────────
    pub fn kill(&mut self, reason: String) {
        self.risk.kill(reason.clone());
        // #1056: the kill switch is pure in-memory - it never outlives the
        // process; a restart comes up clear and the operator re-arms it.
        let _ = self.set_account_gate(
            GLOBAL_KILL_ACCOUNT,
            reason.clone(),
            GateReason::KillSwitch,
            None,
            "system",
            blitzkrieg_market_api::net::now_ms(),
            None,
        );
        self.persist_risk_state();
        tracing::error!(reason = %reason, "kill switch engaged (in-memory, restart-clear)");
        self.emit(Event::RiskAlert {
            code: CoreErrorCode::KillSwitchActive,
            message: reason,
        });
    }

    /// Flush pending near-miss records to disk (call on shutdown).
    pub fn flush_near_misses(&mut self) {
        if let Some(path) = self.config.near_miss_path.clone() {
            let recs = self
                .engine
                .as_mut()
                .map(|e| e.flush_near_misses())
                .unwrap_or_default();
            if !recs.is_empty()
                && let Err(e) =
                    crate::shadow::persist_near_misses(std::path::Path::new(&path), &recs)
            {
                tracing::warn!(error = %e, path = %path, "near-miss flush failed");
            }
        }
    }

    pub fn resume(&mut self) {
        self.risk.resume();
        // #1056: the kill switch never outlives the process - resume clears the
        // in-memory risk gate AND the global `GLOBAL_KILL_ACCOUNT` gate. Nothing
        // is persisted, so a restart comes up clear and the operator must
        // explicitly re-arm.
        self.account_gates.remove(GLOBAL_KILL_ACCOUNT);
        // #187: `risk.resume` is the operator's master lever. Before this issue
        // a failed venue probe reached the same switch, so resume had to be
        // enough to bring the kernel back; with venue failures confined to the
        // venue axis, resume must ALSO release those — otherwise the narrowing
        // would have quietly removed the operator's ability to recover a
        // kernel whose venue came back.
        let lifted = self.venue_halts.clear_all();
        if lifted > 0 {
            tracing::warn!(count = lifted, "risk.resume cleared venue-scoped halts");
        }
        self.persist_risk_state();
    }

    pub fn is_killed(&self) -> bool {
        self.risk.is_killed()
    }

    /// TEST SEAM ONLY (test cfg or feature `test-support`; #780/#825 boundary
    /// hygiene - the same gate [`Core::ledger_mut`] carries): blunt access to
    /// the live risk gate configuration. The runtime-config path does NOT come
    /// through here - it applies audited, hot-reload-checked limits via
    /// [`Core::apply_risk_limits`] / [`Core::apply_systemic_limits`]
    /// (`place()` reads the config on every check).
    #[cfg(any(test, feature = "test-support"))]
    pub fn risk_config_mut(&mut self) -> &mut RiskConfig {
        self.risk.config_mut()
    }

    /// Test-only: re-arm the virtual-account book after construction (the
    /// production surface is `CoreConfig::virtual_limits` at boot; tests
    /// build the core first and opt a strategy in afterwards).
    #[cfg(test)]
    pub(crate) fn set_virtual_limits_for_test(
        &mut self,
        limits: HashMap<String, VirtualAccountLimit>,
    ) {
        self.virtual_accounts = crate::risk::virtual_account::VirtualAccounts::new(&limits);
    }

    /// The live risk gate config - the read side of
    /// [`Self::risk_config_mut`]: a `risk.setLimits`/`risk.setSystemic` write
    /// is visible here immediately, which is what keeps the `risk.limits`
    /// readout honest after a hot edit.
    pub fn risk_config(&self) -> &RiskConfig {
        self.risk.config()
    }

    pub(crate) fn emit(&self, ev: Event) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ev);
        }
    }
}
