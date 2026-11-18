# FeeModel 契约迁移说明（#1279 · 契约先行阶段）

> **状态：契约草案，待人类运营者审批。**
> 本阶段只提交接口定义（`core/bk_contract/src/fee.rs` + `MarketPlugin::fee_model`），
> **不改任何费率计算实现、不改 `extensions/*`、不改内核**。
> 审批通过后才允许进入「接线阶段」。审批前本文档的每个「建议」都是**未生效**的。
>
> 关联证据：`core/blitzkrieg_core/src/exit_fees.rs`、`scripts/lib/fee-model.mjs`、
> `scripts/core-parity.mjs::assertPinnedFeeModel`、`docs/CAPACITY_AND_EQUITY.md:270`。

---

## 1. 现状（以 base `042b9f00` 的代码为准）

### 1.1 内核里到底有几条曲线

**两条，不是一条。** Issue #1279 的描述只提了 `0.07*p*(1-p)`，实际代码里
`exit_fees.rs` 有两条具名 schedule，形态统一为 `fee_per_share = rate * (p*(1-p))^exponent`：

| 名称 | rate | exponent | 身份 | 出处 |
|---|--:|--:|---|---|
| `legacy_quadratic` | 0.125 | 2 | 历史曲线，**无外部发布来源** | `exit_fees.rs:105-113` |
| `official` | 0.07 | 1 | Polymarket 公布的 crypto taker 曲线，**当前默认** | `exit_fees.rs:126-133` |

当前默认是 `official`（`exit_fees.rs:175-189`，`ACTIVE_FEE_SCHEDULE` 未安装时回落
`official_schedule_ref()`）。`legacy_quadratic` 只通过 `--fee-model legacy_quadratic`
在回放中复现旧语料（`blitzkrieg_launcher/src/bin_support/boot/args.rs:697`；
该 flag 无 `--backtest` 时被拒，`blitzkrieg_launcher/src/main.rs:986`）。

**这条必须写进契约说明的原因**：契约不能只覆盖一条曲线。`FeeModel` 至少要能表达
「exponent = 1 和 exponent = 2 两种形态」，否则接线后 `--fee-model legacy_quadratic`
无处落脚，旧语料复现能力当场丢失。

### 1.2 收费路径的实际算术（不是 `per_share * shares`）

真正落到账本上的不是 `fee_per_share(p) * shares`，而是**两除两乘**的链：

```
exit_fees.rs:75   fee_per_share(p) = rate * (p*(1-p))^exponent     // p<=0 或 p>=1 → 0
exit_fees.rs:93   fee_pct(p)       = (fee_per_share(p) / p) * 100
orders.rs:689     notional         = p * delta
orders.rs:719     fee_pct          = if maker { 0 } else { live_taker_fee_pct(p) }
orders.rs:724     fee_usd          = (fee_pct / 100) * notional
```

`reconcile.rs:598-603` 是**第二个收费点**（对账期的外部/手动平仓），算术同上，
maker 同样短路为 0。

### 1.3 其他维度（比 maker/taker 多）

| 维度 | 现状 | 证据 |
|---|---|---|
| maker / taker | maker 费率**恒为 0**（收费点短路，不查曲线） | `orders.rs:719`、`reconcile.rs:598` |
| maker **返佣** | maker 有 **0.20 × fee_per_share** 的建模返佣，走**独立现金通道** `credit_cashflow(.., "rebate")`，受 `config.model_maker_rebate` 开关控制 | `orders.rs:739-754`、`orders.rs:789-796` |
| 入场 / 出场 | **没有区分**：买卖两侧调用同一个 `taker_fee_pct(p)` | `orders.rs:719` 一处决定两侧 |
| 价格定义域 | `p<=0` 或 `p>=1` → 费 0（`p>1` 时 `0.07*p*(1-p)` 为负 = 反向给钱） | `exit_fees.rs:76-78` |
| 舍入 | **无显式舍入**：全链 `rust_decimal` 精确运算，除法的 28 位有效数字截断就是唯一精度边界 | 见 §3 |

---

## 2. 现有 Polymarket 公式如何变成一个 `FeeModel` 实现（只描述，不实现）

接线阶段要做的事，一句话：**把 `exit_fees.rs` 里那个 `FeeSchedule` 值变成一个实现
`FeeModel` 的类型，由 polymarket 扩展声明，内核通过 `MarketPlugin::fee_model()` 问它。**

### 2.1 建议的落地形态（待审批）

`extensions/polymarket/` 新增一个零状态类型（示意，非最终代码）：

```rust
// extensions/polymarket/src/fee.rs —— 只依赖 blitzkrieg-bk-contract
pub struct PolymarketCryptoFee { /* rate, exponent 由曲线族决定 */ }

impl FeeModel for PolymarketCryptoFee {
    fn name(&self) -> &str { "official" }
    fn entry_fee(&self, price: Decimal, role: FeeRole) -> Decimal { self.curve(price, role) }
    fn exit_fee(&self, price: Decimal, role: FeeRole) -> Decimal { self.curve(price, role) }
    fn curve(&self, price: Decimal, role: FeeRole) -> Decimal { /* 逐字照搬 exit_fees.rs:75-85 */ }
}
```

三条硬约束：

1. **`curve` 的体必须是 `exit_fees.rs:75-85` 的逐字搬移**，包括
   `price <= 0 || price >= 1 → 0` 的钳制。搬移时不得「顺手化简」——
   那个钳制是修过的真实资金缺陷（#234 item 3：`p>1` 时旧公式返回负数 = 反向付款）。
2. **不得在扩展侧引入 `blitzkrieg_core` 依赖**（`DEV.md` §2.1 规则 2）。
   因此这个类型不能引用 `exit_fees::FeeSchedule`；它自带 rate/exponent 常量，
   并**在契约 crate 侧被 pin**（见 §2.3）。
3. `entry_fee == exit_fee`（现状是同一函数），但**必须在实现里显式写成两次相同调用**，
   而不是在契约里假设「入场=出场」。若将来出现只收出场费的 venue，
   契约不需要改，实现改一处即可。

### 2.2 内核侧要动的地方（接线阶段，本阶段不动）

- `exit_fees.rs:175-208` 的进程级 set-once 注册表（`ACTIVE_FEE_SCHEDULE` /
  `fee_schedule()` / `set_fee_schedule` / `taker_fee_pct`）是**唯一咨询点**：
  收费点拿到的是「当前 schedule」。接线后它的解析顺序变成
  「插件声明的 `FeeModel` → 未声明则回落到内置 schedule」。
- `orders.rs:719` / `reconcile.rs:598` 两个收费点**不直接改算术**，
  只把「问谁要数」换成「问注册表」。**这是本契约能保持零行为变化的前提。**
- `service.rs:1172 live_taker_fee_pct` 的 `may_charge` 断言（`service.rs:1190`）
  必须同步扩展：它现在只允许「回放」或「shipped official schedule」。
  一旦插件可以声明自己的曲线，这个 fail-closed 断言会**拒绝合法的新 venue**。
  → 列为待拍板问题 Q4。

### 2.3 报告与自检面（必须同步，否则门禁假绿）

| 面 | 位置 | 接线后要做什么 |
|---|---|---|
| 内核自检 | `service/config/fee.rs:86 schedule_reproduces_pin` + `exit_fees.rs:165 pinned_fee_parameters` | 只认 `legacy_quadratic` / `official` 两个名字；插件曲线一律返回 `modelMatches: false`。要么扩展 pin 表，要么让 `fee_quote` 报告插件曲线的来源。**不改 = 所有新 venue 的 `modelMatches` 恒 false。** |
| 跨语言 pin | `scripts/lib/fee-model.mjs:36 TAKER_FEE_MODELS`、`:85 PINNED_DEFAULT_MODEL` | 插件曲线没有对应条目 → `assertPinnedFeeModel`（`core-parity.mjs:103`）的 `feeModelProblems` 会报「pinned default 未描述」。 |
| IPC 读面 | `schema/views.rs:171-175`（`fee_per_share` / `fee_pct_of_price` / `maker_fee_per_share` 恒 0） | `maker_fee_per_share` 恒 0 是**当前事实**；若插件能返回负费率（返佣），这个字段的语义要重新定义。 |

> **关键缺口（本阶段不解决，提请拍板 Q2）**：`FeeModel` 是 trait 对象，**不可序列化**。
> 现有跨语言 pin 与 `core.feeQuote` 都建立在「可 JSON 报告的 `{name, rate, exponent, source}`」
> 之上。要么契约补一个可序列化的描述符（会把 `FeeSchedule` 的形状复制进契约 crate），
> 要么接受「插件曲线不进 pin 表，只在进程内自检」。**本阶段刻意不发明这个类型**——
> 猜一个 wire 格式比留白代价高。

---

## 3. `Decimal` 精度与取整方向（资金语义，必须写明）

### 3.1 结论先行

1. **全链没有 `round_dp` / `trunc` / `ceil` / `floor`。** 费率是精确十进制运算，
   `rust_decimal` 的除法在 28 位有效数字处截断，**这是唯一且真实的舍入点**。
2. 因此「取整方向」在当前实现里**不是一个可配置量，而是一个偶然结果**：
   它等于 rust_decimal 除法的默认行为，落在 `(fee_per_share(p) / p)` 与
   `(fee_pct / 100)` 两次除法上。
3. **`FeeModel` 的返回值单位因此定为「USD/股」，而不是「价格的百分比」。**
   百分比是派生视图（要除一次），per-share 是原始量。让契约去返回派生量，
   等于在契约边界上多加一次除法 = 多一个舍入点 = 多一个账本口径分叉。
4. **落地到账本时不得再舍入。** 任何 `round_dp` 都会让 `core-parity.mjs` 的
   逐位断言变红，且改变已成交语料的 PnL 口径（R7）。

### 3.2 逐位复现的实测证据

契约要求「回落路径必须逐位复现现有常量」。为此实测了
`(fee_pct/100)*notional` 与代数等价式 `fee_per_share*shares` 是否逐位相等
（`rust_decimal 1.43.0`，与 workspace 锁定的版本一致）：

- **venue tick 精度（4 位小数价格 0.0001–0.9999 × 5 种股数 × 2 条曲线 × 2 个指数）
  共 199,980 组：0 组不等。**
- **高精度价格（19 位有效数字）下开始不等**，例如
  `rate=0.125, exp=2, p=0.1414213562373095049, shares=13.37`：
  - 现行链 `(fps/p)*100/100*(p*shares)` = `0.0246394823355358596045327373`
  - 化简链 `fps*shares`                     = `0.0246394823355358596045327372`

**含义**：两种写法在今天的 tick 精度下数值一致，但**在 28 位精度上限附近不一致**。
所以契约的硬要求不是「数学等价」，而是：

> **接线必须保留现有运算顺序（`per_share → /price → *100 → /100 → *notional`），
> 不得预先化简。** 逐位一致性靠「同一串操作」保证，不靠「同一道代数式」。

### 3.3 Polymarket 官方口径 vs 本仓口径（不对称，须知情）

`scripts/lib/fee-model.mjs:55` 记录了官方口径：**费率按 5 位小数取整，
最小 0.00001 USDC**。本仓**没有实现这个取整**——它是精确 Decimal。
这是既存的、刻意的差异（改成官方取整会动所有历史语料的 PnL）。
→ 列为待拍板问题 Q1。

---

## 4. 未声明 `fee_model` 时的回落路径（设计，待审批）

### 4.1 设计要求

`MarketPlugin::fee_model()` 默认返回 `None`（见 `core/bk_contract/src/plugin.rs`）。
`None` 必须**逐位**复现今天的数字，否则：

- `core-parity.mjs::assertPinnedFeeModel` 变红（它拿 `core.feeQuote` 对
  `scripts/lib/fee-model.mjs` 的 pin 表）；
- `exit_policy/tests.rs:122-124,182-184` 的费率断言变红；
- 已成交语料的 PnL 口径变化（R7 边界）。

### 4.2 建议方案（三档，按代价从低到高）

| 档 | 做法 | 优点 | 缺点 |
|---|---|---|---|
| **A（推荐）** | `None` → 注册表原样返回内置 `FeeSchedule`（即**什么都不变**：`fee_schedule()` 今日的路径原封不动）。`FeeModel` 只在 `Some` 时接管。 | 零风险、零改动、回落路径天然逐位一致（它就是现有代码）。 | 内核同时存在两种表达（`FeeSchedule` 值与 `FeeModel` trait 对象），需要一层适配。 |
| B | 内置 `official` 也实现成 `FeeModel`，`None` 回落为 `Box::new(OfficialFee)`。 | 表达统一，只有一条路径。 | 必须证明新路径与旧算术逐位一致（§3.2 的运算顺序约束），门禁是唯一保证。 |
| C | 不改注册表，插件声明只影响**报告**（`fee_quote`），不参与收费。 | 最安全。 | 契约形同虚设——收费仍在内核，Issue #1279 的问题没解决。 |

**建议 A**：契约阶段不承担「统一表达」的风险；把 B 留作后续独立 PR，
由 `core-parity.mjs` 作为唯一验收。C 不满足 Issue 目标。

### 4.3 回落必须守住的三件事

1. **价格定义域**：`p<=0 || p>=1 → 0`。回落路径与插件路径**都要**有这个钳制。
2. **maker 短路**：maker 收费恒 0，**且不查曲线**（`orders.rs:719`）。
   接线后如果改成「问 FeeModel 要 maker 费率」，`official` 曲线会返回非 0
   （它是 taker 曲线）→ 静默多收费。**maker 短路必须留在收费点，不能下移到模型里。**
3. **maker 返佣通道不变**：返佣走 `credit_cashflow(.., "rebate")`，
   是**独立现金通道**，不是「负的费」。`FeeModel` 契约允许负返回值，
   但**接线时不得把返佣合并进 `fee_usd`**——那会同时改动账本口径与
   `stats.modeled_maker_rebate_usd` 的语义（R7）。

---

## 5. 接线点清单（**本阶段一律不改**）

| # | 文件:行 | 是什么 | 接线阶段的动作 |
|:--:|---|---|---|
| 1 | `core/blitzkrieg_core/src/exit_fees.rs:75` | `FeeSchedule::fee_per_share` — 全仓唯一曲线拼写 | 成为 Polymarket `FeeModel` 实现的体（逐字搬移） |
| 2 | `core/blitzkrieg_core/src/exit_fees.rs:93` | `FeeSchedule::fee_pct` — 派生视图 | 保留为派生；**不是**契约的返回单位 |
| 3 | `core/blitzkrieg_core/src/exit_fees.rs:105` | `legacy_quadratic_schedule()` | 需能被 `FeeModel` 表达（exponent=2） |
| 4 | `core/blitzkrieg_core/src/exit_fees.rs:126` | `official_schedule()` — 当前默认 | 同上（exponent=1） |
| 5 | `core/blitzkrieg_core/src/exit_fees.rs:139` | `fee_schedule_by_name` | 名称注册表，需容纳插件曲线 |
| 6 | `core/blitzkrieg_core/src/exit_fees.rs:165` | `pinned_fee_parameters` — 手工钉值表 | 插件曲线无条目 → `modelMatches` 恒 false（Q2） |
| 7 | `core/blitzkrieg_core/src/exit_fees.rs:175-198` | `ACTIVE_FEE_SCHEDULE` / `set_fee_schedule`（set-once） | **唯一咨询点**：改为「插件声明优先，未声明回落」 |
| 8 | `core/blitzkrieg_core/src/exit_fees.rs:204` | `taker_fee_pct` — 收费口径出口 | 变成「问注册表」，算术不动 |
| 9 | `core/blitzkrieg_core/src/service/settlement_orders/orders.rs:719` | 收费点 ①：maker 短路 + `fee_pct` | 只换数据来源；**maker 短路留下** |
| 10 | `core/blitzkrieg_core/src/service/settlement_orders/orders.rs:724` | 收费点 ①：`(fee_pct/100)*notional` | 运算顺序**不得**化简（§3.2） |
| 11 | `core/blitzkrieg_core/src/service/settlement_orders/orders.rs:739` | maker 返佣（0.20 × fee_per_share） | 保持独立现金通道 |
| 12 | `core/blitzkrieg_core/src/service/accounts/reconcile.rs:598` | 收费点 ②（对账期平仓） | 同 9/10 |
| 13 | `core/blitzkrieg_core/src/service.rs:1172` | `live_taker_fee_pct` | 调用方不变 |
| 14 | `core/blitzkrieg_core/src/service.rs:1190` | `may_charge` fail-closed 断言 | **会拒绝合法新 venue**（Q4） |
| 15 | `core/blitzkrieg_core/src/service/config/fee.rs:45` | `charged_per_share` — quote 用 | 需与收费点同源 |
| 16 | `core/blitzkrieg_core/src/service/config/fee.rs:96` | `fee_quote` / `fee_quote_at` | 报告面（Q2） |
| 17 | `core/blitzkrieg_core/src/schema/views.rs:171` | `FeeQuoteResult.maker_fee_per_share` 恒 0 | 负费率语义要重定义（Q3） |
| 18 | `core/blitzkrieg_core/src/shadow.rs:253,396,434` | 影子进化费率 | 跟随注册表 |
| 19 | `core/blitzkrieg_core/src/strategies/shadow_twin.rs:269,341` | shadow twin 费率 | 跟随注册表 |
| 20 | `core/blitzkrieg_core/src/exit_policy.rs:1286` | `taker_fee_pct` 用于 **breakeven lock 门限**（非收费） | 跟随注册表；这是**判定**不是记账，改动需单独评估 |
| 21 | `core/blitzkrieg_core/src/extension/builtins.rs:13` | `BinanceSpotExtension` 壳 | **R5：本阶段不得删除**；接线阶段决定去留 |
| 22 | `core/blitzkrieg_core/src/service/assembly.rs:1029-1059` | 安装内置扩展 | 接线阶段的组合根改动点 |
| 23 | `extensions/{polymarket,binance_spot,kalshi,predictfun,polysimulator}/` | 5 个插件实现 | 各自 `impl MarketPlugin::fee_model`（默认 `None`，零改动可编译） |
| 24 | `scripts/lib/fee-model.mjs:36,85` | 跨语言 pin 表 + 默认模型 | 与内核 pin **必须同一次改动** |
| 25 | `scripts/core-parity.mjs:103` | `assertPinnedFeeModel` | 逐位验收的唯一裁判 |

---

## 6. 本阶段交付的契约面（已落地）

| 项 | 位置 |
|---|---|
| `trait FeeModel`（`name` / `entry_fee` / `exit_fee`） | `core/bk_contract/src/fee.rs` |
| `enum FeeRole { Maker, Taker }` | `core/bk_contract/src/fee.rs` |
| `MarketPlugin::fee_model() -> Option<&dyn FeeModel>`（默认 `None`） | `core/bk_contract/src/plugin.rs` |
| 兼容性 re-export | `core/bk_contract/src/lib.rs`、`core/market_api/src/lib.rs` |

**零行为变化**：`fee_model()` 有默认实现、无调用点；`FeeModel` 无实现者。
`extensions/*` 的 `Cargo.toml` 与源码**一行未动**，仍只依赖 `blitzkrieg-bk-contract`。

### 6.1 为什么没有按 Issue 的参考形态新增一个 `MarketCapabilities` 结构体

Issue #1279 的参考形态是：

```rust
pub struct MarketCapabilities { fee_model: Option<Box<dyn FeeModel>>, round_families, supports_orphan_query, orphan_grace_ms }
```

**这个名字已被占用且是冻结形状**：`core/bk_contract/src/modes.rs:46` 已有
`pub struct MarketCapabilities(pub u64)`（11 个能力位的位图，`CAPABILITY_NAMES`
在 `modes.rs:90`），并且已从 `lib.rs:30` 与 `market_api/src/lib.rs:31` 导出。
新增同名类型 = 破坏性改名。

而参考形态的另外三个字段，**在代码里都已经有 owner**：

| 参考字段 | 已存在的声明 | 位置 |
|---|---|---|
| `round_families: Vec<i64>` | `MarketPlugin::supported_round_secs() -> Option<Vec<i64>>` | `plugin.rs:253` |
| （轮长标签） | `MarketPlugin::round_labels()` | `plugin.rs:266` |
| `supports_orphan_query` / `orphan_grace_ms` | 孤儿宽限是**各 executor 自己的参数**，不是 venue 声明 | `extensions/kalshi/src/executor.rs:411`、`predictfun/src/executor.rs:443`、`polysimulator/src/executor.rs:771` |

所以本契约只补**真正缺失的那一块**（费率），并且用仓库既有的挂载惯例
——「带默认实现的可选 accessor」——挂到 `MarketPlugin` 上，
和 `data_feed()` / `discovery()` / `executor()` / `declare_modes()` /
`supported_round_secs()` / `feed_source()` / `required_credential_env()` 完全同形。
这样 5 个现有扩展**零改动可编译**，第三方插件也不破。

### 6.2 为什么 `FeeRole` 而不是 `is_maker: bool`

maker 与 taker 在本仓**不对称**：maker 费率恒 0，但 maker 有 0.20 × fee_per_share
的**返佣**（`orders.rs:739`），即 maker 成交让现金**反向**流动。
契约里用一个 `bool` 表达这个不对称，正是符号丢失的地方。

### 6.3 为什么 `entry_fee` / `exit_fee` 分成两个方法

参考形态如此，且 venue 确实可能非对称（只收出场费、入场返佣、往返分层）。
**但必须写明**：现状是**一个函数服务两侧**（`orders.rs:719`），
所以 Polymarket 的实现两侧返回同一个数——这是**实现的义务**，不是契约的假设。

---

## 7. 待运营者拍板（详见 PR 正文）

| Q | 问题 | 建议 |
|:--:|---|---|
| Q1 | 费率取整方向：保持精确 Decimal（现状），还是实现官方「5 位小数、最小 0.00001 USDC」？ | **保持精确 Decimal**。改取整 = 改所有历史语料 PnL（R7），且现有门禁逐位断言会红。 |
| Q2 | 插件曲线如何进入 pin 表 / `core.feeQuote` 报告（trait 不可序列化） | 契约阶段留白；接线阶段补一个可序列化描述符，或明确「插件曲线只做进程内自检」。 |
| Q3 | 负费率（返佣）是否属于 `FeeModel` 的语义 | 建议**不属于**：返佣继续走独立 cashflow 通道，`FeeModel` 只表达「收费」。 |
| Q4 | `may_charge` fail-closed 断言如何容纳插件曲线 | 改为「回放 ‖ 插件自声明 ‖ shipped official」，且必须在同一改动里扩展测试。 |
| Q5 | `entry_fee`/`exit_fee` 两个方法 vs 单个 `fee()` | 建议保留两个（契约比实现宽），Polymarket 实现两侧同值。 |
| Q6 | 回落策略选 A / B / C | **A**（未声明 → 走现有 `fee_schedule()` 原路径），B 留后续独立 PR。 |

---

_本文件随契约提交，供审批；接线阶段的任何实现都必须先更新本文件的对应条目。_
