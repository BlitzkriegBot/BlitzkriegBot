# RFC-1278 — `StoreApi` v2 契约与迁移说明

> **状态：契约待审批（CONTRACT ONLY）**
> Issue: #1278「StoreApi 未收口：四套写入面 / 两个 SQL 客户端」
> base: `ceer/main` @ `042b9f00` · 分支 `arch/1278-store-contract`
> 本文件 + `core/blitzkrieg_core/src/store/mod.rs` 的类型定义是**唯一**交付物。
> 本提交**没有**任何行为变化，也**没有**任何新调用方。

---

## 0. 一条硬边界（先读）

Issue #1278 原文：**「审批前不得改 `risk/account_store.rs` 实现」**。

因此本提交**不**改动：

| 未改动的文件 | 为什么不动 |
|---|---|
| `core/blitzkrieg_core/src/risk/account_store.rs` | Issue 明令禁止，等审批 |
| `core/blitzkrieg_core/src/db_import.rs` | 删除是另一件事，且要先验存量库（§5） |
| 任何 JSONL 资金写调用方（`order_db.rs` / `trade_db.rs` / `ledger.rs` / `settlement.rs`） | 接线是审批后的工作，且触及 R7 账本口径 |
| `core/blitzkrieg_core/src/store/sqlite.rs` | 未加实现体；新方法走默认实现（§4 待决 D-1） |
| 任何既有测试 | 零行为变化，测试原样通过 |

`store/mod.rs` 里新增的每一项都带 `#[allow(dead_code)]` 与
`// #1278 契约阶段，待审批后接线` 注释，且**默认实现返回 `Err`**（不是 `Ok`）——
理由见 §3.3。

---

## 1. 现状：写入面与 SQL 客户端的真实清单

Issue 标题说「四套写入面 / 两个 SQL 客户端」。按代码核对，实际是 **5 个写入口 / 2 个 SQL 客户端**，
其中「5」= Issue 说的四套 + 导入器（它只在一次性迁移时写）。逐条证据：

| # | 写入面 | 入口 | 证据 |
|:--:|---|---|---|
| 1 | **`StoreApi` trait 面**（`SqliteStore`） | `Core::store` | `store/mod.rs:38`；实现 `store/sqlite.rs:318` |
| 2 | **`AccountRiskDb` 侧门**（裸 SQL，绕过 trait） | `Core::account_risk_db` | `risk/account_store.rs:74-84`（`Database::open` 在 `:82`）；字段 `service.rs:69` |
| 3 | **`db` 自由函数面**（裸 SQL 助手） | `crate::db::{kernel_meta_set, account_setting_set, account_setting_remove}` | `db/mod.rs:308 / 363 / 387`（另有读 `:293 / :341`） |
| 4 | **JSONL 资金面**（订单/成交/账本/结算） | `crate::jsonl::append` | `jsonl.rs:255` |
| 5 | **一次性导入器**（自带 DDL 副本） | `db_import::import_once` | `db_import.rs:401`，由 `blitzkrieg_launcher/src/main.rs:1262` 调用 |

**两个 SQL 客户端**（各自 `open` 一次，各自持连接）：

1. `crate::db::Database` — `db/mod.rs:190` `Database::open`，内部 `Mutex<Connection>`，
   所有 `AccountRiskDb` 与 `SqliteStore` 共用它（`account_store.rs:93` `db()`、
   `assembly.rs:1158` `install_risk_db(s.risk_db().clone())` → 同一实例，不是两个连接）。
2. `db_import` 自开的 `rusqlite::Connection` — `db_import.rs:409` `Connection::open(db_path)`。

> **不要误读成「同一文件上有两个连接」**：生产路径只有 `Database` 一个连接。
> 第二个客户端是导入器，只在 `import_once` 期间存在。收口的目标是让**写语义**
> 只有一套，不是消灭连接数。

### 1.1 已经存在的两条「无 store」姿态（互相矛盾）

同一份代码里，「没有 store」有两种截然不同的处理，这正是 `WriteOutcome` 要收掉的东西：

| 调用点 | 无 store 时 | 证据 |
|---|---|---|
| 账户写（`account.venue.set` 等） | **硬失败**：`Err(InvalidParams, "persistence is disabled … nothing was applied")` | `service/accounts/persist.rs:39-48`（同型 9 处：`:39 :105 :179 :472 :514 :570 :650 :1054 :1180`） |
| 风控写（`risk.setSystemic` / `risk.setExit`） | **静默成功**：内存生效，回包 `persisted: false` | `service/trading_reset.rs:564`、`:624` |

第二种在数据上诚实（回包带 `persisted`），但它要求每个调用方**记得**去看那个字段；
第一种干脆不让步。两套口径同时存在于同一进程里，是 #1278 的核心症状。

**全部 `self.store.as_deref()` 分流站点（共 16 处，可复核）：**

- `core/blitzkrieg_core/src/ipc/server/handlers/risk.rs:23`、`:46`
- `core/blitzkrieg_core/src/ipc/server/handlers/execution_policy.rs:185`、`:256`
- `core/blitzkrieg_core/src/service/trading_reset.rs:564`、`:624`
- `core/blitzkrieg_core/src/service/accounts/persist.rs:39`、`:105`、`:179`、`:472`、`:514`、`:570`、`:650`、`:1054`、`:1180`
- `core/blitzkrieg_core/src/service/accounts/mod.rs:122`

合计 16 处。与 §1.1 的两条「无 store」姿态并列：persist.rs 的 9 处是账户写（硬失败），
`trading_reset.rs:564 / :624` 是风控写（静默成功），ipc 两处 handlers 是 panel 风控 /
执行政策下发。

---

## 2. 新增契约（已落进 `store/mod.rs`）

| 项 | 位置 | 说明 |
|---|---|---|
| `WriteOutcome` | `store/mod.rs:260` | `Persisted` / `MemoryOnly { reason }`。**不是 `bool`** |
| `MemoryOnlyReason` | `store/mod.rs:276` | `NoStoreConfigured` / `ReadOnlyMode` / `BackendRejected(String)` |
| `AccountScope<'a>` | `store/mod.rs:292` | `{ account: &'a AccountId }` + `new()` |
| `MoneyFact<'a>` | `store/mod.rs:315` | 4 变体，**各引用既有类型**，见下 |
| `MoneyFact::kind()` | `store/mod.rs:325` | 仅用于错误文案 |
| `StoreApi::append_money_fact` | `store/mod.rs:208` | 默认实现 `Err` |
| `StoreApi::put_setting` | `store/mod.rs:229` | 默认实现 `Err` |

`MoneyFact` 的四个变体是**既有类型的并集**，不新增任何字段、算术或序列化拼写
（R7：记账口径不可擅动）：

| 变体 | 引用类型 | 定义位置 | 当前写入口 |
|---|---|---|---|
| `Order(&TrackedOrder)` | `model::TrackedOrder` | `model.rs:203` | `order_db.rs:36` |
| `Fill(&TradeRecord)` | `trade_db::TradeRecord` | `trade_db.rs:33` | `trade_db.rs:261` |
| `Ledger(&JournalEntry)` | `ledger::JournalEntry` | `ledger.rs:62` | `ledger.rs:223` |
| `Settlement(&SettlementRecord)` | `settlement::SettlementRecord` | `settlement.rs:206` | `settlement.rs:521` |

> 结算面选 `SettlementRecord`（`pub`）而不是 `JournalLine`：后者是
> `settlement.rs:431` 的**私有** `enum`，要引用它就得先把它改成 `pub`——
> 那是改 `settlement.rs`，超出本阶段边界。`SettlementRecord` 正是
> `JournalLine::Settlement { at_ms, record }` 的载荷，信息不丢。

### 2.1 为什么错误类型是 `CoreError` 而不是新的 `StoreError`

任务书草案写的是 `Result<WriteOutcome, StoreError>`，但**本仓没有 `StoreError`**。
既有 `StoreApi` 的每个方法都返回 `Result<_, CoreError>`（`store/mod.rs:51-180`，
`CoreError` 定义在 `bk_contract/src/types.rs:186`，经 `crate::model` 再导出）。
按「沿用既有命名与错误类型，不要另造一套错误枚举」的要求，新方法返回
`Result<WriteOutcome, CoreError>`，错误码用既有的 `CoreErrorCode`。

---

## 3. 迁移映射：`account_store.rs` 的裸 SQL → v2 trait

`account_store.rs` 共 **16 条**裸 SQL 语句 = **9 条写 + 7 条读**（用
`grep -n "SELECT \|INSERT \|DELETE \|UPDATE " core/blitzkrieg_core/src/risk/account_store.rs`
可复现）。任务书说的「9 处」对应的是**写**语句那 9 条；读语句另外 7 条，
见 §3.2 —— 它们暴露了一个任务书没提的缺口。

### 3.1 九条写语句 → 目标方法

| # | 位置 | SQL | 现所在方法 | v2 目标 |
|:--:|---|---|---|---|
| W1 | `:254` | `INSERT OR IGNORE INTO accounts`（FK 垫片） | `save_risk` | `save_risk_bounds`（**既有**）+ ensure 语义下沉 |
| W2 | `:261` | `INSERT INTO account_risk … ON CONFLICT` | `save_risk` | `save_risk_bounds`（**既有**） |
| W3 | `:286` | `INSERT INTO account_settings … ON CONFLICT`（`risk.max_order_notional_usd`） | `save_risk` | `save_risk_bounds`（**既有**，同一事务捆绑） |
| W4 | `:310` | `INSERT OR IGNORE INTO accounts`（FK 垫片） | `save_exit` | `save_exit`（**既有**） |
| W5 | `:317` | `INSERT INTO account_exit_policy … ON CONFLICT` | `save_exit` | `save_exit`（**既有**） |
| W6 | `:376` | `INSERT OR IGNORE INTO accounts`（FK 垫片） | `save_setting` | **`put_setting`（新增）** + ensure 下沉 |
| W7 | `:412` | `DELETE FROM kernel_meta WHERE key = ?1` | `delete_kernel_meta` | **无对应** → 见 §3.3 缺口 G-2 |
| W8 | `:469` | `INSERT INTO accounts … ON CONFLICT`（全行） | `save_account_row` | `save_account_create`（**既有**）**不完整**，见缺口 G-3 |
| W9 | `:508` | `INSERT INTO account_credentials … ON CONFLICT` | `save_credential_names` | `save_account_create` 的 `credential_keys` 半边（**既有**） |

**结论**：9 条写里，**7 条已经能被既有 trait 方法覆盖**，只有 W6 需要新方法
`put_setting`，W7 需要一个新的 meta 面（缺口 G-2）。也就是说 v2 的写入面缺口
比 Issue 标题暗示的小得多——真正缺的是**读面**（§3.2）和**语义**（§1.1）。

### 3.2 七条读语句 → **全部没有 trait 面**

| # | 位置 | 现方法 | v2 目标 |
|:--:|---|---|---|
| R1 | `:106` | `load_risk` | **无** |
| R2 | `:179` | `load_risk`（`max_order_notional`） | **无** |
| R3 | `:212` | `load_exits` | **无** |
| R4 | `:344` | `load_settings` | **无** |
| R5 | `:428` | `load_kernel_meta_by_prefix` | **无** |
| R6 | `:540` | `load_accounts` | **无** |
| R7 | `:577` | `load_account_credential_keys` | **无** |

`StoreApi` 至今**只有写面**（唯一例外是 `load_arb_config`，`store/mod.rs:140`）。
所有读都走 `account_risk_db` 侧门，这就是「收口」一直没完成的原因：
写能走 trait，读不能，于是没人能真正删掉侧门。

### 3.3 三个缺口（本阶段**不**补，列出来给运营者定）

- **G-1 `put_setting` 未实现** — 默认实现返回 `Err`，等审批后由 `SqliteStore` 覆盖。
- **G-2 `kernel_meta` 无面** — `W7` / `R5` 以及 `save_kernel_meta` / `load_kernel_meta`
  （`account_store.rs:392 / :401`，转调 `db/mod.rs:293 / :308`）都在 trait 之外。
  需要 `put_meta` / `load_meta` 两个方法，或明确「kernel_meta 永不走 StoreApi」。
- **G-3 `save_account_create` 表达不了全行** — `W8` 写 9 列
  （`id, name, market_type, status, venue, trading_enabled, rounds_json, opening_equity, updated_at_ms`），
  而 `save_account_create`（`store/mod.rs:56-62`）只收 `account_id, name, venue, credential_keys`。
  收口 W8 要么扩既有方法签名（**破坏所有实现者**，违反本阶段「不得改既有签名」），
  要么承认 `save_account_row` 是另一个域写。

### 3.4 默认实现为什么返回 `Err` 而不是 `Ok(MemoryOnly)`

本仓最贵的一类缺陷就是「写盘失败静默成功」：`jsonl::append` 返回 `bool`
（`jsonl.rs:255`），四个资金写面（`order_db.rs:36`、`trade_db.rs:261`、
`ledger.rs:223`、`settlement.rs:521`）**全部丢弃**它。

如果默认实现返回 `Ok(MemoryOnly { NoStoreConfigured })`，一个忘记实现的 backend
会继续静默降级——而且会把「后端不支持」误报成「没有配置 store」。
返回 `Err` 则让遗漏变成一次响亮的失败。**审批后应删掉默认实现**（待决 D-1）。

---

## 4. `store: Option<Box<dyn StoreApi>>` 怎么收口

### 4.0 问题是什么

`service.rs:75` 的 `pub(crate) store: Option<Box<dyn crate::store::StoreApi>>`
把「这个内核没有持久化」写成了一个**合法且无声**的运行态。
它服务两个真实场景（回测器、多数进程内测试），但副作用是
**每个调用点都必须自己决定「没有 store 时怎么办」**，而它们做的决定互相矛盾（§1.1）。

`store/mod.rs:12-14` 的模块注释已经承认这一点，并把它包装成「the same honest refusal」。
**它不是诚实的**：诚实要求调用方知道并上报，而现在有 **16 处**
`self.store.as_deref()` 分流（站点清单见 §1.1），其中
有的报错、有的假装成功（§1.1）。

### 4.1 路 A：改成必选 `Box<dyn StoreApi>`

| 项 | 内容 |
|---|---|
| 做法 | `Core::store: Box<dyn StoreApi>`；新增 `MemoryStore`（零持久化 backend）作为「无持久化」的唯一显式拼写 |
| 收益 | 「没有 store」从类型上消失；编译器强制每个调用点只处理 `WriteOutcome` 两种结果 |
| 破坏面 | ① 16 处 `as_deref()` 早退分支全部要改写；② `store/mod.rs:351` 的 `test_support::install_store` 签名变；③ **所有** `Core::new(...)` / `with_account_book(...)` 构造点（含 `tests/store_backend_parity.rs`、`tests/account_decouple_check.rs`、`ipc/server/tests/*` 的几十处）必须显式传一个 store；④ 回测器要改成装 `MemoryStore` |
| 风险 | ③ 是本仓最大的测试面，一次改动跨几十个文件；`MemoryStore` 若实现成「接受一切、什么都不写」，就把 silent degradation 从 `Option` 换了个名字 |

### 4.2 路 B：保留 `Option`，但把它变成显式 `NoStore` 变体

| 项 | 内容 |
|---|---|
| 做法 | `enum StoreCarrier { Sqlite(SqliteStore), NoStore(NoStoreReason) }`（非 `Option`），`NoStoreReason ∈ {Backtester, InProcessTest, ReadOnlyPosture}` |
| 收益 | 破坏面比 A 小：`Core::new` 的默认值可以保持（默认 `NoStore(InProcessTest)`），几十个测试构造点**不用动**；「没有 store」仍在类型里，但**必须带理由**，且理由能进日志/回包 |
| 破坏面 | 16 处 `as_deref()` 仍要改写（但改成 `match carrier`，语义更清楚）；`test_support::install_store` 签名变；需要决定 `NoStoreReason` 的三个值是否够 |
| 风险 | 比 A 少一个数量级的改动，但「没有 store」这个状态**仍然存在**——只是从无声变成有声 |

### 4.3 我的建议：**路 B**，且分两步

**建议路 B。** 理由：

1. **A 的破坏面落在测试上，而测试是本仓最贵的不变量载体。** `DEV.md §6.1` 要求
   「测试要钉行为」；一次跨几十个文件的构造签名改造，最容易的通过方式是
   「加个默认参数让它编译过」——那正好把 A 的收益（编译器强制表态）还回去了。
2. **B 直接命中 #1278 的病灶**：病灶不是「`Option` 存在」，而是「`None` 不带理由、
   调用方各自解释」。给 `None` 一个 `NoStoreReason` 就把这件事钉死了，
   而且**不需要**改任何一个 `Core::new` 调用点。
3. **B 是 A 的严格前置**：先让 `NoStore` 带理由（B），观察一段时间里到底哪些
   `NoStoreReason` 在生产路径上真的出现；如果答案是「只有 Backtester」，
   那时再把 `NoStore` 从生产构造路径上摘掉（A）就是一次机械改动，破坏面已经量过了。

**两步**：

- **B1（本契约审批后第一个 PR）**：`StoreCarrier` + `NoStoreReason`，16 处早退分支改
  `match`，**不加新行为**——`NoStore` 仍按各调用点现有姿态处理，只是每次早退都
  `tracing::warn!` 一次并带理由。
- **B2（观察后）**：把 §1.1 的两套矛盾姿态统一成 `WriteOutcome` 语义
  （`persist.rs` 的硬失败保留给「写了但不该写」；`trading_reset.rs` 的
  `persisted: false` 改由 `MemoryOnly` 承载）。这一步会**改变回包**，
  是真正的行为变化，必须单独审批。

> **不要**在本契约里顺手做 B1/B2（R5）。

---

## 5. 资金写失败后调用方该做什么（R6 待决，只列选项）

这是 R6 意义上的「改真实资金相关逻辑」，**我不替你决定**。四个选项，各自的
代价必须由运营者权衡：

| 选项 | 语义 | 代价 |
|---|---|---|
| **F-1 硬失败，回滚内存** | 写失败 = 整个操作 `Err`，内存状态回退到写之前 | 最强保证，但「写不进磁盘就拒单」在磁盘满/只读挂载时会让**已有的持仓无法平仓**——风控上可能比丢日志更危险 |
| **F-2 内存权威 + 强制上报** | 内存生效，`WriteOutcome::MemoryOnly` 一路冒到 IPC 回包 + 面板红条 | 保住可交易性；但需要面板配合（归 Gemini），且「红条被无视」是新的失效模式 |
| **F-3 按事实类型分级** | 账本/结算（钱真的动了）= F-1；订单快照/成交记录（可重建）= F-2 | 最贴合业务；但「哪类事实可重建」需要一次账本口径审计（R7 范围，必须单独批） |
| **F-4 写入前熔断** | 写失败达到 N 次/窗口 → 触发 kill switch 停止开新仓，允许平仓 | 把持久化故障升级成风控事件；最保守，也最可能误伤 |

**我的建议：F-3，但先用 F-2 落地。** 理由：F-3 需要对账本口径做判断
（`Ledger` 记账口径是 R7 冻结项），在没有一次专门审计之前定不了；
F-2 是唯一「不改变任何资金行为、只增加可见性」的选项，可以作为 F-3 的前置。
F-1 与 F-4 都会在磁盘故障时改变交易能力，风险最高，不建议先做。

**必须由运营者回答的问题**：`ledger.rs:223`（账本行）与 `settlement.rs:521`
（结算行）写失败时，内核应当**继续交易**还是**停止开新仓**？这是资金语义问题，
不是工程问题。

---

## 6. `db_import.rs` 能否删除：判断依据（**不删**，只列怎么查）

`db_import` 是**活代码**，不是遗留：`blitzkrieg_launcher/src/main.rs:1262` 每次启动都会调
`db_import::import_once`。删除它的前提是「没有任何部署还需要把 v1 时代的数据折进 SQLite」。

判断步骤（**按顺序，每步贴输出**）：

```bash
# 1. 生产库里有没有 import_v1 完成标记？有 = 这个库已经导入过，导入器对它已是 no-op。
#    注意：no-op 不等于可删——只有「所有现存部署都已导入」才等于可删。
sqlite3 <data/bank.db> "SELECT key, value, updated_at_ms FROM kernel_meta WHERE key='import_v1';"

# 2. 这个库是不是「从没迁移过」的老库？（schema_migrations 为空 = 导入器还会自己建 v1 DDL）
sqlite3 <data/bank.db> "SELECT * FROM schema_migrations ORDER BY version;"

# 3. 库里到底有没有数据？db::is_empty_database 的口径：account_risk 零行 且 无 import_v1 标记
#    （db/mod.rs:279-287）。
sqlite3 <data/bank.db> "SELECT (SELECT COUNT(*) FROM account_risk), (SELECT COUNT(*) FROM accounts);"

# 4. 旧世界还剩什么可导？导入器只读 .env 与 db 同级的 state 快照（db_import.rs:29-33）。
#    只列文件名与键名，不打印值（R2）。
grep -o '^[A-Z_]*=' <path>/.env | sed 's/=$//'
ls <data/>*.migrated 2>/dev/null

# 5. 面板/启动器是否还有别的调用点？
grep -rn "db_import\|import_once" --include="*.rs" --include="*.mjs" .
```

**判定规则（建议）**：只有当**所有**现存部署的库都满足
「`import_v1` 标记存在」**或**「`schema_migrations` 非空且是迁移器建的」，才可考虑删除；
且删除必须保留 `SCHEMA_V1` 的副本（`db/mod.rs` 的 `schema::SCHEMA_V1`），
否则全新部署就没有 v1 DDL 了。

**不要**在本阶段删任何东西（R3：未经备份不删文件；`data/archive/` 曾丢 617 MB，KI-24）。

---

## 7. 接线点清单（**列出，不改**）

审批通过后，改造会落在这 19 个位置：

### 7.1 契约与装配

| file:line | 现状 | 改造 |
|---|---|---|
| `store/mod.rs:38` | `pub trait StoreApi: Send + Sync` | 本契约已加 2 方法 |
| `store/mod.rs:351` | `test_support::install_store(core, Box<dyn StoreApi>)` | 路 B 下改签名 |
| `store/sqlite.rs:318` | `impl StoreApi for SqliteStore` | 需覆盖 `append_money_fact` / `put_setting` |
| `service.rs:69` | `account_risk_db: Option<AccountRiskDb>` | 侧门，目标删除 |
| `service.rs:75` | `store: Option<Box<dyn StoreApi>>` | §4 |
| `service/assembly.rs:24-36` | `open_account_risk_db`（`if is_some() return` 去重） | 侧门入口 |
| `service/assembly.rs:269-279` | `SqliteStore::open` + 装箱 | 装配点 |
| `service/assembly.rs:1158` | `install_risk_db(s.risk_db().clone())` | 两 facade 合一的关键 |

### 7.2 四个资金写面（`jsonl::append` 调用点，全部丢弃返回值）

| file:line | 事实 | 当前代码 |
|---|---|---|
| `order_db.rs:36` | 订单快照 | `crate::jsonl::append(&self.jsonl_path, order);` |
| `trade_db.rs:261` | 成交/平仓 | `crate::jsonl::append(&self.jsonl_path, rec);` |
| `ledger.rs:223` | 账本行 | `crate::jsonl::append(&path, &entry);` |
| `settlement.rs:521` | 结算行 | `crate::jsonl::append(&self.path, line);` |

上游调用点（改签名会波及）：

| file:line | 说明 |
|---|---|
| `service.rs:1017-1021` | `persist_order`（`db.append(o)`，`OrderDb` 可选） |
| `service/settlement_orders/closing.rs:23-24`、`:355-356` | `TradeRecord::from_closed` + `db.record(...)` |
| `ledger.rs:222` | `if let Some(path) = self.journal_path.clone()` — 路径可选，又是一处无声降级 |
| `service/assembly.rs:671-678` | `trade_db` / `order_db` 的 `Option` 装配 |

> 对照：`hedge/audit.rs:67` 与 `execution_policy/rules.rs:298` **确实**读了
> `jsonl::append` 的 `bool`（前者 warn，后者返回）。所以「调用方全部丢弃」
> 精确地说只适用于上面四个**资金**写面，不是全部 JSONL。

### 7.3 设置/读面

| file:line | 说明 |
|---|---|
| `risk/account_store.rs:80-84` | `AccountRiskDb::open` → `Database::open`（`:82`） |
| `risk/account_store.rs:368-389` | `save_setting`（`put_setting` 的原型） |
| `db/mod.rs:293 / 308` | `kernel_meta_get` / `kernel_meta_set`（缺口 G-2） |
| `db/mod.rs:341 / 363 / 387` | `account_setting_get/set/remove` |
| `db_import.rs:401 / :409` | `import_once` + 自开 `Connection`（第二个 SQL 客户端） |
| `blitzkrieg_launcher/src/main.rs:1262` | 导入器唯一生产调用点 |

---

## 8. 待运营者拍板的问题清单

| # | 问题 | 我的建议 | 理由 |
|:--:|---|---|---|
| **D-1** | 新 trait 方法是否保留**默认实现**？ | **审批后删掉默认实现**（强制每个 backend 表态） | 默认实现是「忘记实现」的容错阀，而 silent degradation 正是要消灭的东西。本阶段保留默认实现只是为了让 2 个既有实现者（`store/sqlite.rs:318`、`store/mod.rs:386` 的 `MemStore`）零改动编译——即「不改既有测试」的硬要求 |
| **D-2** | `store` 收口走**路 A（必选）**还是**路 B（显式 `NoStore` 变体）**？ | **路 B，分两步**（§4.3） | A 的破坏面落在几十个测试构造点上，最容易的通过方式会把 A 的收益还回去；B 直接命中病灶（`None` 不带理由），且不碰任何 `Core::new` 调用点 |
| **D-3** | 读面（§3.2 的 7 条 `SELECT`）是否也进 `StoreApi`？ | **要进**，否则侧门永远删不掉 | 写能走 trait、读不能，是 #1278 一直收不了口的直接原因。但这是**新增 7 个方法**，建议单独一个 PR，不要塞进 v2 首个实现 PR |
| **D-4** | `kernel_meta`（缺口 G-2）归 `StoreApi` 还是明确排除？ | **明确排除**，另立 `MetaStore` 面 | `kernel_meta` 是进程级开关（`shadow_enabled` 等），不是账户域事实；塞进 `StoreApi` 会让 trait 再次变成杂物抽屉 |
| **D-5** | `save_account_row`（`W8`，9 列全行）怎么办？ | **承认它是独立域写**，不硬塞进 `save_account_create` | 扩既有签名会立刻破坏所有实现者（违反本阶段「不得改既有签名」）；而全行物化只在 boot 的 env 簿采纳时用一次（`assembly.rs:324-353`） |
| **D-6** | 资金写失败后调用方做什么（§5 F-1..F-4）？ | **先用 F-2，目标 F-3** | F-2 不改变任何资金行为、只增加可见性，可作为前置；F-3 需要对账本口径做一次专门审计（R7 冻结项），必须单独批 |
| **D-7** | `ledger.rs:222` 的 `journal_path: Option<PathBuf>` 是否也要收口？ | **要**，但它是一处**独立**的无声降级（路径没设 = 不记账，且不报） | 它不在 Issue #1278 的四个 JSONL 面里，是排查时发现的；按 R5 记在这里，**不在本 PR 顺手改** |
| **D-8** | `db_import.rs` 的删除时机（§6） | **本阶段不删**；先跑 §6 的 5 条检查并留存输出 | 它是活代码（`launcher/src/main.rs:1262`），且删除前必须确认无 v1 存量库（R3） |

---

## 9. 复跑本契约

```bash
cd <worktree>
cargo build --release --workspace --locked
cargo test -p blitzkrieg-core --lib
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

契约新增项全部带 `#[allow(dead_code)]` + `#1278 契约阶段，待审批后接线` 注释，
且**无调用方**——所以「所有既有测试原样通过」本身就是零行为变化的证据。

---

_依据：`dev-docs/RULES.md` R1/R5/R6/R7/R8/R11/R12 · `dev-docs/DEV.md` §2.1/§3.1/§6.1/§7
· Issue #1278 · base `042b9f00`_
