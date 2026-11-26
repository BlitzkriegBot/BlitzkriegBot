# 智能交易（Smart Money 逆向工作台）

> **核心理念：不直接跟单，而是把聪明钱的交易行为逆向成可验证、可进化的蓝图策略。**

在 5 分钟 / 15 分钟加密二元预测市场中，直接跟单（Copy Trading）往往面临结构性劣势：等你监听到目标地址在链上成交、再发起市价追单时，盘口流动性已被吃干，滑点与延迟会迅速吞噬原本的期望收益。

BlitzkriegBot v0.4.3 推出的 **智能交易（Smart Trading）** 板块，提供了一套完整的「**雷达发现 → 行为指纹提取 → 蓝图策略合成 → 样本外回测验真**」四步量化研究闭环。

---

## 一、四步工作流

1. **全网聪明钱雷达扫榜（`smart_money.scan`）**
   - 直接对接 Polymarket Data API v2（`/v2/leaderboard` 与 `/v2/trades`），支持按 `5m` / `15m` 轮次、时间窗口（近 24 小时 / 近 7 天 / 近 30 天）与最小买单笔数筛选头部盈利钱包。
   - 后端采用 6 并发限速拉取（35ms 节流，严格控制在官方速率限额内），过滤掉非加密涨跌轮次（如政治、体育等噪声市场），只保留纯 5m/15m 加密轮次成交样本。
2. **单钱包行为指纹提取（`smart_money.analyze`）**
   - 点击雷达候选行，或在右侧输入任意 Polymarket `0x` 地址，即可提取该钱包在目标轮次上的量化行为指纹：
     - **入场价格带（p25–p90）**
     - **剩余时间窗口（p10–p90）**
     - **双边配对率（Pair Ratio）与持有到期率（Hold-to-Settlement Ratio）**
     - **已结算胜率、净盈亏与平均单笔名义额**
3. **自动合成可进化 Blueprint JSON**
   - 系统按行为指纹自动归类策略原型，并只对**单腿可复现原型**合成标准 Blueprint JSON；一键保存到本地策略包（`blueprint.save`）后，可直接跳入「蓝图画布」做节点级可视化微调，或交给影子进化引擎优化参数。
4. **样本外带延迟回测验真（`backtest.run` · `mode=verify`）**
   - 在工作台右下角直接选择本地历史盘口数据集，以 **`verify` 模式 + `slippageTicks = 1`**（强制 1 tick 滑点摩擦）跑样本外回测。
   - 只有通过正期望门禁（费后净利 > 0 且存在真实成交）的策略，才建议由人工审核后开启 dry-run 观察，严禁未经验证直接推实盘。

---

## 二、支持的策略原型与拒收纪律

| 原型标识 | 中文名 | 典型行为特征 | 是否合成蓝图 |
| --- | --- | --- | --- |
| `late_favorite` | 尾盘顺势持有 | 轮次尾段买入高概率优势方并持有至结算 | ✅ 自动合成 |
| `mid_round_momentum` | 中段动量突破 | 轮次中段顺势追击方向突破 | ✅ 自动合成 |
| `cheap_option_hunter` | 低价非对称筹码 | 低价区埋伏高赔率反转筹码 | ✅ 自动合成 |
| `pair_arb` | 双边折价配对 | 同市场同时买入双边锁定折价套利 | ❌ 拒收（需走跨所/双边套利模块） |
| `unclassifiable` | 不可归类 / 样本不足 | 有效买单笔数不足或行为无稳定结构 | ❌ fail-closed 拒收 |

---

## 三、架构与接口契约

| 层级 | 组件 / 路由 | 说明 |
| --- | --- | --- |
| 链上拉取与分析 | `tools/onchain_pull/src/radar.rs` | Polymarket Data API v2 `/v2/leaderboard` + `/v2/trades`（6 并发拉取、`data` 信封解析、`snake_case` 字段适配） |
| 内核只读 IPC | `smart_money.scan` / `smart_money.analyze` | 注册于 `method_spec.rs`（ReadOnly），契约见 [`docs/rust-core/INTERFACES.md`](/rust-core/INTERFACES) §2.17 |
| 网关 HTTP 代理 | `POST /api/smart-money/scan` / `POST /api/smart-money/analyze` | 分别配置 45s / 30s IPC 读取超时预算，防止外网扫描误触发短超时 |
| WebUI 工作台 | `策略 → 智能交易`（`SmartTradingPage.vue`） | 顶部命令条 + 7/5 主从工作台，内置 Blueprint 预览、一键保存、画布预载与 Stage 3 样本外验真 |

---

## 四、风险提示与使用边界

- 智能交易工作台**只生成并保存策略包**，不会自动启用策略，也不会关闭系统的 `DRY_RUN` 保护。
- 任何逆向得到的策略都必须先通过本地 `verify` 回测与 `dry-run` 实况观察，再由操作者自行决定是否进入实盘。
- 完整的设计背景与阶段演进记录见 [`SMART_MONEY_REVERSE_RFC.md`](/SMART_MONEY_REVERSE_RFC)。
