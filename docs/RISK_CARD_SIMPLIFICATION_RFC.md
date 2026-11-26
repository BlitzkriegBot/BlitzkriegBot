# RFC: 风控卡片极简化与前后端单真源整合方案

- **状态**：Draft（待操作者逐项确认 R6 资金/风控语义后进入实施）
- **日期**：2026-11-26
- **前置地基**：PR #173（移除启动器隐藏 `$6.00` 单笔限额并暴露 `BK_SIZE_PCT`）、PR #239（移除启动器隐藏 `--max-positions 2` 与 `--min/max-shares 10` 硬编码并暴露到 WebUI）

---

## 1. 背景与痛点

当前「设置 → 风控与策略」页面横跨 **启动器（`ui_kit`）→ 策略引擎（`Engine`）→ 执行策略（`ExecutionPolicy`）→ 账户风控（`RiskGate` + `PositionManager`）** 四个层次，共暴露了近 20 个互相重叠的旋钮：

1. **下单大小有 6 处互相打架**：
   - 引擎层 `BK_SIZE_PCT`（总现金权益的百分数 `0..=100`）
   - 执行策略层 `budgetRatio`（可用余额的小数 `(0, 1]`）
   - 执行策略层 `minBudgetUsd`（最小下注 USD）
   - 引擎层 `minShares` / `maxShares`（单笔份额上下限）
   - 账户风控层 `maxPositionSize`（单仓股数上限）
   - 账户风控层 `maxOrderNotionalUsd`（单笔最大金额 USD）
2. **持仓数量有 4 处重叠**：
   - 启动器 `--max-positions`、内核 `PositionManager.max_positions`、执行策略 `maxPositionsPerAsset`、规则引擎 `open_positions >= N`。
3. **语义不统一**：
   - 有的字段填 `0` 表示关闭（如 `maxOrderNotionalUsd`、`maxPositions`、`BK_SIZE_PCT`），有的填 `0` 表示拒绝（如 `maxShares`、`budgetRatio`）；
   - 有的填百分数（`2` = 2%），有的填小数（`0.02` = 2%）。

结果是用户经常遇到「明明在面板里改了某个值，下单却被另一层看不见的参数截断或拒单」。

---

## 2. 设计原则

1. **一个概念只留一个入口**：同一业务意图（每单买多少、最多开几单、亏多少停手、怎么止盈止损）在前后端都只认一个真源，不再并存两套计算路径。
2. **傻白甜主卡片 + 高级抽屉分层**：
   - 主卡片只保留普通用户每天真正关心的 **3 组 8 项**核心设置；
   - 面向精细化调参的规则构建器（`POLICY_RULES`）、强平秒、移动止盈、`maker-first`、单笔最大亏损、单仓上限等统一收进默认折叠的 **「高级设置」** 抽屉。
3. **所见即所得（实时换算预览）**：输入下注比例（如 `2%`）时，界面旁边实时显示按当前账户余额换算的单笔预估金额（如 `≈ $20.00 / 单`）。
4. **统一 `0 = 关闭` 语义 + 百分数单位**：所有上限/比例均采用人类直觉的百分数与「`0` = 不限制」。
5. **平滑迁移、绝不静默放宽（Fail-Closed）**：旧 SQLite 配置自动映射到新口径时，冲突取更严值（never widen），并通过门禁钉死禁止回退。

---

## 3. 字段合并与退场对照表

### 3.1 主卡片（3 组 8 项）

| 分组 | 新字段（UI 标签） | 单位 / 语义 | 合并替代的旧字段 | 后端唯一真源（SQLite） |
|---|---|---|---|---|
| **一、每单下多少** | **1. 单笔下注比例** | `%`（`0..=100`，`0` = 按最小金额固定下单） | 合并 `BK_SIZE_PCT` + `budgetRatio` | `account_settings`: `sizing.budget_pct`（取代双路径） |
| | **2. 单笔金额范围（最小 ~ 最大）** | `USD`（最小 `>= 0`，最大 `0` = 不限） | 合并 `minBudgetUsd` + `maxOrderNotionalUsd` + 退化 `minShares`/`maxShares` | `account_settings`: `sizing.min_usd` / `risk.max_order_notional_usd` |
| **二、最多开几单** | **3. 总并发持仓上限** | `笔`（`0` = 不限） | 合并启动器 `--max-positions` + `PositionManager.max_positions` | `account_settings`: `positions.max_positions` |
| | **4. 单币种持仓上限** | `笔`（整数 `>= 1`） | 保留 `maxPositionsPerAsset` | `account_settings`: `POLICY_MAX_POSITIONS_PER_ASSET` |
| **三、亏了怎么停** | **5. 当日最大亏损** | `USD`（`0` = 关闭） | 保留 `maxDailyDrawdownUsd` | `account_risk.max_daily_drawdown_usd` |
| | **6. 连亏熔断（次数 + 冷静期）** | `连亏 [X] 次 → 暂停 [Y] 分钟`（合并为一行，`0` = 关闭） | 合并 `maxConsecutiveLosses` + `cooldownMinutes` | `account_risk.max_consecutive_losses` / `cooldown_minutes` |
| | **7. 止损（%）** | `%`（`> 0`） | 保留 `stopLossPct` | `account_exit_policy.stop_loss_pct` |
| | **8. 止盈（%）** | `%`（`> 0`） | 保留 `takeProfitPct` | `account_exit_policy.take_profit_pct` |

### 3.2 收进「高级设置」折叠抽屉（默认折叠，保留功能不丢失）

- **高级风控**：`单笔最大亏损 (maxSingleLossUsd)`、`单仓上限 (maxPositionSize)`、`最低权益门槛 (minEquityUsd)`、`单笔最大份额兜底 (maxShares)`
- **高级出场**：`到期前强平 (forceExitSec)`、`移动止盈 (trailingEnabled)`、`策略出场 maker-first (strategyExitMakerFirst)`
- **高级规则**：`执行策略条件规则编辑器 (POLICY_RULES)`（如 `symbol == "XRP" → skip`）

---

## 4. 后端单真源计算公式（R6 重点确认）

整合后，每笔开仓的预算与股数只经过 **一条** 确定性公式：

1. **计算单笔目标金额 `budget_usd`**：
   - 若 `budget_pct > 0`：`raw = available_balance × (budget_pct / 100)`
   - 若 `budget_pct == 0`：`raw = min_usd`（固定金额模式）
   - 下限钳位：`budget = max(raw, min_usd)`（若余额不足 `min_usd` 则拒单，不凭空借钱）
   - 上限钳位：若 `max_order_notional_usd > 0`，`budget = min(budget, max_order_notional_usd)`
2. **换算股数 `shares`**：
   - `shares = floor_to_grid(budget / entry_price, 0.01)`
   - 若高级兜底 `max_shares > 0`，`shares = min(shares, max_shares)`（默认设为足够大或 `0 = 不限`，不再偷偷卡死在 `10` 股）
   - 若 `shares < venue_min_shares`，拒单并明确回报 `below_venue_min_shares`
3. **并发持仓门控**：
   - 仅检查 `positions.max_positions`（`0 = 不限`）与 `max_positions_per_asset`，启动器不传硬编码覆盖值。

---

## 5. 分期实施与 Issue 拆分

本项目拆为 **3 个独立可验收的阶段（3 张 Gitea Issue）**：

1. **Phase 1（后端单真源收口 · R6）**：
   - 收口 `BK_SIZE_PCT` 与 `budgetRatio` 为单一预算公式，明确旧参数兼容优先级（取更严值，绝不静默放宽）；
   - `Engine` 支持 `max_shares == 0` 表示不限（仅受金额上限与余额约束）；
   - 补充单元测试钉死：`budget_pct` + `min_usd` + `max_order_notional_usd` 端到端计算与重启恢复。
2. **Phase 2（WebUI 傻白甜重构）**：
   - 重塑 `SettingsRisk.vue` / `SettingsRiskLimits.vue` 主卡片为 3 组 8 项；
   - 增加实时换算预览（输入 `2%` 实时显示 `≈ $XX.XX / 单`，并标注实际生效上下限）；
   - 连亏次数与冷静期合并为单行复合输入；
   - 将高级风控、高级出场、条件规则编辑器移入默认折叠的「高级设置」抽屉，保持单一「保存」按钮一次提交。
3. **Phase 3（迁移兼容与防回归门禁）**：
   - 提供 SQLite 读侧平滑兼容（老库仅有 `BK_SIZE_PCT` 或 `POLICY_BUDGET_RATIO` 时自动折算，双配冲突取更严值并打 WARN 日志）；
   - 新增前后端门禁脚本：禁止启动器恢复硬编码风控参数、禁止主卡片重新膨胀出重复口径字段。

---

## 6. 风险与边界

- **R6 合规要求**：Phase 1 涉及下单预算计算与 `max_shares == 0` 语义变更，开工前须经操作者对 §4 公式逐项确认。
- **既有测试兼容**：`schema_family_golden` 冻结契约与 `risk-card-single-save.check.mjs` 等前端门禁必须保持全绿。
- 本次文档仅作方案设计，未对项目安全性作任何声称，完整审计待重跑。
