# RFC: 智能交易模块 — 全网聪明钱逆向策略引擎（Smart Money Reverse-to-Strategy）

> **状态**：Draft（Phase 1 即将开工）
> **范围**：`tools/onchain_pull` · `core/blitzkrieg_core`（`blueprint` / `ipc` / `backtest`）· `ui/webapp/webui`
> **核心理念**：不跟单（消除 200ms–2s 链上/API 跟单延迟与二手盘口滑点），而是扫描优质聪明钱地址的历史链上成交，提取行为指纹并逆向编译为本地零延迟运行的 Blueprint / Lua 策略，经样本外带延迟回测（`--backtest-mode verify`）验真后方可上架。

---

## 1. 背景与动机

在 5 分钟 / 15 分钟二元预测轮次中，传统“跟单（Copy-Trading）”存在结构性硬伤：
1. **信号滞后**：监听到链上/Data API 成交时已过去 200ms–2s，目标地址吃掉的盘口流动性早已消失；
2. **滑点放大**：尾盘高胜率区（`0.85–0.97`）每差 1–2 个 tick，盈亏比就显著恶化；
3. **黑盒风险**：无法区分目标地址是“可复制的机械规则”还是“短期运气/消息面豪赌”。

BlitzkriegBot 已经具备三大底层能力，可直接拼装成 **「扫描 → 拉取 → 逆向 → 蓝图编译 → 样本外回测 → 影子进化」** 闭环：
- `tools/onchain_pull`（`--onchain-pull` / `--onchain-fills`）：按钱包地址增量拉取 Polymarket 成交与 Gamma 市场元数据；
- `core/blitzkrieg_core/src/blueprint.rs`（`--blueprint-compile`）：三阶段校验（DAG 结构 / 字段与算子白名单 / 零 `os./io./debug` 沙箱代码生成）+ 自动导出可进化 `tunables`；
- `blitzkrieg-core --backtest --backtest-mode verify`：真实延迟 + 官方费率样本外回测。

---

## 2. 非目标与安全边界（Fail-Closed）

1. **绝不做盲目跟单**：本模块不自动跟随任何外部地址下单，只产出**可审计、可回测、可进化的本地策略蓝图**。
2. **不可逆向的地址直接拒收（Fail-Closed）**：样本量 `< 20` 笔、或行为高度离散（疑似消息面/人工主观/高频撤单博弈）的钱包直接归类为 `Unclassifiable`，拒绝生成伪策略，避免过拟合噪音。
3. **样本外验证硬门禁**：逆向生成的 Blueprint/Lua 必须经过时间切分（前段拟合参数，后段 `--backtest-mode verify` 真实延迟验真），且默认处于 **disabled** 状态，必须由操作者人工确认后启用。
4. **零新增沙箱风险**：所有自动生成的策略统一通过既有 `blueprint::compile_with_tunables` 管线生成，天然满足 `FIELD_WHITELIST`、`CONDITION_OPS` 与禁止 `os./io./debug` 构造保证。

---

## 3. 系统架构（4 步闭环）

```text
[1. 聪明钱雷达]              [2. 行为指纹提取]               [3. 骨架分类与蓝图合成]            [4. 样本外验真与上架]
Polymarket Leaderboard  ──▶  tools/onchain_pull          ──▶  reverse::classify_archetype    ──▶  --backtest-mode verify
(5m/15m Crypto 过滤)         (.trades.jsonl + Gamma)          reverse::synthesize_blueprint       (真实延迟 + 官方费率)
                                                                     │                                    │
                                                                     ▼                                    ▼
                                                              blueprint::compile_with_tunables    WebUI 蓝图页 / 影子进化
                                                              (合规 Lua + 可进化 tunables)        (人工审核后一键启用)
```

---

## 4. 核心数据结构与算法设计

### 4.1 钱包行为指纹（`WalletFingerprint`）

从 `tools/onchain_pull` 拉取的规范成交（`RawFill`）+ Gamma 市场元数据（`endDate`、`asset`、`roundDurationSec`）计算：

| 维度 | 指标 | 用途 |
| :--- | :--- | :--- |
| 样本覆盖 | `total_fills`、`buy_fills`、`sell_fills`、`unique_conditions`、`assets`、`durations_sec` | 过滤低样本/非 5m/15m Crypto 钱包 |
| 入场价分布 | `entry_px_p10` / `p25` / `p50` / `p75` / `p90` / `min` / `max` | 拟合 `min_px` / `max_px` 区间 |
| 剩余时间分布 | `time_left_p10` / `p50` / `p90`（秒，`endDate - fill.timestamp`） | 拟合 `min_time_left` / `max_time_left` 窗口 |
| 结构特征 | `pair_ratio`（同 condition 同时买 UP+DOWN 的占比）、`hold_to_settlement_ratio`（无到期前 SELL 占比）、`avg_notional_usd` | 区分单边顺势 vs 双边配对 vs 盘中波段 |
| 收益与质量 | `resolved_rounds`、`win_rate_pct`、`net_pnl_usd`、`profit_factor`（基于已结算 condition） | 排除负期望与幸存者噪音 |

### 4.2 策略骨架分类（`StrategyArchetype`）

确定性、可解释的分类规则（按优先级）：
1. **`Unclassifiable`**：`buy_fills < 20` 或缺少有效 `time_left_sec` 样本 → 拒绝合成；
2. **`PairArb`**：`pair_ratio >= 0.40`（同轮双腿配对）→ 标记为配对套利骨架（提示走 `pair_discount_arb` / `maker_pair` 参数模板，不强塞单边 Blueprint）；
3. **`LateFavorite`**：`entry_px_p50 >= 0.80` 且 `time_left_p50 <= 120` 且 `hold_to_settlement_ratio >= 0.80` → 合成尾盘高胜率顺势 Blueprint（`tick.price >= p25` AND `tick.price <= p90` AND `tick.time_left_sec <= p90` AND `tick.time_left_sec >= p10`）；
4. **`MidRoundMomentum`**：`0.55 <= entry_px_p50 < 0.85` 且 `time_left_p50 > 60` → 合成中段突破 Blueprint；
5. **`CheapOptionHunter`**：`entry_px_p50 <= 0.45` → 合成低价非对称筹码 Blueprint；
6. 其余离散分布 → `Unclassifiable`。

### 4.3 蓝图合成（`synthesize_blueprint`）

直接输出标准 `version: 1` Blueprint JSON：
- 仅使用白名单字段（`tick.price`、`tick.time_left_sec`）与白名单算子（`>=`、`<=`），通过 `logic_and` 汇聚到唯一 `action_buy`；
- 编译后自动携带 `__param(...)` 可进化旋钮（`tunables`），可直接接入 Shadow Evolution 做在线微调。

---

## 5. 分阶段实施与 Issue 拆解

- **Phase 1 — 后端指纹提取 + 骨架分类 + Blueprint 合成器 + CLI**
  - 在 `tools/onchain_pull/src/reverse.rs` 实现 `WalletFingerprint`、`StrategyArchetype`、`analyze_fills`、`classify_archetype`、`synthesize_blueprint` 及全量单测；
  - 在 `blitzkrieg_core` 新增端到端测试验证合成的 Blueprint JSON 通过 `blueprint::compile_with_tunables` 且无沙箱违禁字；
  - 暴露 CLI 子命令 `--reverse-wallet <trades.jsonl>` 输出 JSON 指纹 + 候选 Blueprint。
- **Phase 2 — IPC 接口 + WebUI「聪明钱逆向」面板（接入蓝图页）**
  - 新增 `smart_money.analyze` IPC 方法，供 WebUI 提交钱包地址或已拉取数据集，返回指纹画像与一键可导入的 Blueprint JSON；
  - 在 `BlueprintPage.vue` 增加「聪明钱逆向」抽屉/面板：输入钱包地址 → 展示行为指纹与骨架判定 → 一键导入蓝图画布并编译保存。
- **Phase 3 — 全网聪明钱雷达扫描 + 自动样本外验真门禁**
  - 新增 Polymarket Leaderboard / Data API 批量扫描器，自动筛选 5m/15m Crypto 高分地址；
  - 自动执行时间切分 + `--backtest-mode verify` 样本外验真，按 OOS 净利 / PF / 最大回撤排序推送候选策略。

---

## 6. 验收标准

1. 给定合成或真实 `.trades.jsonl` + Gamma 缓存，`analyze_fills` 精确产出 `WalletFingerprint`；低样本或离散钱包 Fail-Closed 返回 `Unclassifiable`；
2. 合成的 Blueprint JSON 100% 通过 `blitzkrieg_core::blueprint::compile_with_tunables` 且导出的 `tunables` 区间合法；
3. 零破坏既有 `--onchain-pull` / `--blueprint-compile` / `schema_family_golden` 契约。
