# RFC-1276: 事件信封（seq / scope / filter）——契约先行

> Refs #1276「事件无账户过滤、无单调 seq、Lagged 静默丢事件」。
> 状态：**提案 — 待运营者审批**。本文档 + 三个类型定义构成「契约先行阶段」的全部交付；
> **业务实现一行未动**。审批通过后才允许按 §6 的接线点清单进入实现。
>
> 本 RFC 只解决**流**的三件事（顺序、归属、丢失可观测），不改任何事件负载的语义，
> 不改 `Event` 枚举，不改 SSE，不改前端，不改 C ABI。

---

## 一、背景与三个缺陷

`core.event` 是内核 → 客户端唯一的推送通道，形状是
`{jsonrpc, method:"core.event", params: Event}`（`core/blitzkrieg_core/src/schema/mod.rs:60` 定义 `EVENT_METHOD`，
`schema/views.rs:1343` 定义 `Notification`）。当前它有四个可核实的问题：

| # | 缺陷 | 证据（base `042b9f00`） |
|:--:|---|---|
| 1 | **事件无账户过滤**：只有 `KlineUpdate` 按订阅过滤，`OrderUpdate` / `Fill` / `PositionClosed` 全量广播；账户过滤发生在前端 | `ipc/server/session.rs:1035-1045`（唯一的过滤臂）；`ui/webapp/webui/src/lib/account-filter.ts:42-47`（前端补的过滤） |
| 2 | **无单调 seq**：`Event` 无序号字段，全库唯一序号是影子进化的 `cycle_seq`（`views.rs:1261`） | `views.rs:1175` 起的 `pub enum Event` 全变体 |
| 3 | **`Lagged` 静默丢事件**：广播环满时直接 `continue`，客户端收不到任何「你漏了 N 条」的信号 | `ipc/server/session.rs:1052` `Err(RecvError::Lagged(_)) => continue` |
| 4 | **出站队列无界**：慢消费者 OOM 面，注释自认留给后续 issue | `ipc/server/session.rs:996-1000` |

缺陷 1–3 是同一个病：**流的属性没有建模**。它们都是「流」的属性，不是任何单条事件的属性。
这就是本 RFC 不往 `Event` 上加 `seq` 字段的理由——见 §2.2。

另有一条**已有但长错层**的 seq：UI 网关自己的 `EventBus` 维护了一个单调计数器
（`ui/ui_kit/src/core/event_bus.rs:23` 字段、`:53-68` `publish` 里 `g.seq += 1`）。
它是在**解码之后**才分配的，所以内核 `broadcast` 环丢掉的条目在网关侧完全不可见——
序号存在，但存在于错误的层。这是缺陷 3 至今无人察觉的直接原因。

---

## 二、新增类型（`core/blitzkrieg_core/src/schema/views/event_contract.rs`）

新类型放在 `views.rs` 的**子模块**里，由 `schema/views.rs:9-10` 的
`mod event_contract; pub use event_contract::*;` 重新导出，所以
`crate::schema::EventEnvelope` 与放在 `views.rs` 里**对调用方完全等价**。

为什么另开文件而不是就地扩 `views.rs`：`views.rs` 在 baseline 已是 1349 行，而仓库的
文件尺寸棘轮是 1500 行硬门（`scripts/file-size-check.mjs`）。就地追加会把它推成
1618 行 → 该门禁直接变红；而把一个已冻结的 schema 文件塞进 `ALLOW` 是**放宽门禁**
（R8 明确禁止）。拆子模块是在不改门禁、不改既有行为的前提下唯一合规的落法。

| 类型 | 位置（`schema/views/event_contract.rs`） | 用途 |
|---|---|---|
| `EventKind` | `:44` | 对 `Event` 的轻量判别枚举（14 个 unit 变体），`SCREAMING_SNAKE_CASE`，与 `Event` 的 serde tag 逐字一致 |
| `impl Event::kind()` | `:69` | `Event` → `EventKind` 的**穷尽**映射 |
| `EventScope` | `:99` | `{ account: AccountId, venue: Option<String> }` |
| `EventEnvelope` | `:113` | `{ seq: u64, scope: Option<EventScope>, event: Event }` |
| `EventFilter` | `:136` | `{ accounts: Vec<AccountId>, venues: Vec<String>, kinds: Vec<EventKind> }` |

全部带 `#[allow(dead_code)]` 与「#1276 契约阶段，待审批后接线」注释；`serde(rename_all = "camelCase")`
沿用 `views.rs` 既有视图类型的惯例（`PluginCredentialRow:17`、`CredentialsStatus:32`、`AccountJournalResult:1161`）。

### 2.1 为什么用 `EventKind` 而不拆 `Event`

`Event` 是 golden-locked 的冻结线（`tests/schema_family_golden.rs` family 21 逐字节锁死）。
拆成 20 个类型会新增 20 个被锁的 wire family，而消费者需要的能力只是「按种类过滤」——
一个 14 变体的 unit enum 就够。`EventKind` 的 wire 值直接复用 `Event` 的 tag 词汇
（`ORDER_UPDATE` / `POSITION_CLOSED` / …），客户端用**同一批字符串**过滤它已经在看的 `kind`。

`Event::kind()` 写成穷尽 `match` 是刻意的：将来新增一个 `Event` 变体，编译器会强制这里同步，
否则该变体会落在所有 `EventFilter` 之外——那正是缺陷 1 一个变体一个变体地复发。

### 2.2 为什么 seq 不放在 `Event` 上

`Event` 有 119 处构造/匹配点（`grep -rn "Event::(变体)"`，全仓）。往变体里加 `seq` 会让每一处构造点
立刻编译失败，且要求 **51 个 `self.emit(...)` 调用点**（16 个文件）各自填一个正确的序号——
任何一个漏填就是错序。放进信封则只需在**唯一的 sink** 上填一次。

**这不是替换，是并列**：`Event` 的 serde 形状一个字节都不变，`Notification::new` 也不变。

### 2.3 `EventEnvelope` 只有 `Serialize`

与 `Event` 一致：内核是事件流的唯一生产者，Rust 侧没有任何地方反序列化它（消费方都是 JSON 客户端）。
所以 `EventEnvelope` 只 derive `Serialize`。`EventFilter` / `EventScope` / `EventKind` 则
`Serialize + Deserialize`——过滤条件将来要从会话命令里**解析**进来。

---

## 三、字段语义与「谁负责填」

| 字段 | 语义 | 谁填 | 何时填 |
|---|---|---|---|
| `EventEnvelope.seq` | 本进程内的事件序号，从 1 开始，每 emit 一次 `+1` | **内核**（`Core::emit`，`service/risk_admission/freeze.rs:633`） | emit 的瞬间，单点 |
| `EventEnvelope.scope` | 事件归属；`None` = 不归属任何单一账户 | **内核**（emit 点） | emit 的瞬间 |
| `EventScope.account` | 该事件的钱落在哪个账户 | 内核 | 见下表 |
| `EventScope.venue` | 该事件属于哪个交易场所；`None` = 非 venue 维度 | 内核 | 见下表 |
| `EventEnvelope.event` | 原样的 `Event`，语义不变 | 现有 51 个 emit 点 | 不变 |
| `EventFilter.accounts` | 只收这些账户的事件；**空 = 全部** | **会话**（客户端下发） | `event.subscribe`（待建） |
| `EventFilter.venues` | 只收这些 venue 的事件；**空 = 全部** | 会话 | 同上 |
| `EventFilter.kinds` | 只收这些种类；**空 = 全部** | 会话 | 同上 |

`scope` 的逐变体来源（这是接线阶段唯一需要人工判断的地方）：

| `Event` 变体 | `account` | `venue` | 备注 |
|---|---|---|---|
| `OrderUpdate` | `order.account_id` | `None` → 待定（§7 Q3） | `TrackedOrder` **没有** venue 字段（`model.rs:203`），要 venue 必须从市场上下文取 |
| `Fill` | `delta.account_id`（= `order.account_id`） | 同上 | |
| `PositionClosed` | `account_id`（已有字段，`views.rs:1209`） | 同上 | |
| `IntentDecision` | `account_id`（已有字段，`views.rs:1273`） | `None` | |
| `KlineUpdate` | `None` | 应从 kline 的 symbol 映射到 venue | 行情是 venue 维度、非账户维度 |
| `ReconcileReport` | `None`（跨账户清扫） | `None` | |
| `Ready` / `RiskAlert` / `Error` / `Evolution*` | `None` | `None` | 内核级事件 |

**`EventScope` 必须由内核填，不能由消费者推导**：`venue` 在 `TrackedOrder` 里不存在，
消费者拿不到；让客户端自己去查市场表，等于把「账户过滤」的旧错误（缺陷 1）从事件层搬到查询层。

### 3.1 seq 的单调性边界（建议 + 理由）

**建议：进程内单调、重启后重置为 1、不落盘、不加 epoch 字段。**

理由：

1. **持久顺序的权威是账本，不是通知流。** 落盘 seq 意味着每笔成交都要在热路径上写一次持久计数器，
   而账本/`account.journal` 已经提供了带游标的持久顺序（`core/blitzkrieg_core/src/service/tests/journal_cursor_tests.rs` 就是它的门禁）。
   在一个通知流上再复制一份持久序号，是两条会漂移的真相。
2. **事件流的正确消费契约是「断了就重读快照」，不是「回放缺失的事件」。** 因此 seq 只需要回答一个问题：
   「我漏了吗？」——这只需要**同一进程内**单调。跨重启的连续性没有消费者需求。
3. **重启可被无歧义识别，不需要 epoch。** 客户端规则（写进契约）：*任何序号不连续——包括倒退——
   都判定为「可能漏事件」→ 重读 `/api/snapshot`。* 重启后客户端看到 seq 从 5000 掉回 1，
   这条规则同样触发重读，结论与「真的漏了」完全一致。加 epoch 只能改善**诊断可读性**，
   不能改善正确性，而它会多一个必须被所有消费方理解的字段。
4. **`seq` 用 `u64` 且从 1 起**：`0` 保留为「未知/未填充」，让「忘了填」与「第 0 条」不混淆。

**代价（必须知情）**：seq 不是审计凭据。需要「这条成交一定没被漏掉」的场合，走账本/`journal`，
不要走 `core.event`。这一条要写进 RFC 的对外语义里，否则将来会有人拿 seq 当对账依据。

### 3.2 `Lagged` 的处置（缺陷 3）

`seq` 的存在使丢事件**可被检测**，但检测发生在客户端。内核侧建议同时做两件事（接线阶段）：
`Lagged(n)` 时计数并上报（`ReconcileReport` 式的一条 `Error` 事件，或一个新的 `EVENT_GAP` 变体——
见 §7 Q4），而不是 `continue`。当前 `session.rs:1052` 的 `continue` 必须变成「记录 + 继续」。

---

## 四、`EventFilter` 的空值策略（安全默认值 —— 本 RFC 最关键的一条）

**建议：空 vector = 该维度不受约束，即 `EventFilter::default()` 是全收（pass-through）。**

### 4.1 为什么不是「空 = 全拒」

反方（fail-closed / 最小权限）的论证是：默认拒绝 = 安全默认。这条在这里**不成立**，四个理由：

1. **`EventFilter` 是相关性过滤器，不是授权边界。** 授权边界在 `command` 面：同一个 UDS 连接
   可以调 `engine.stats` / `account.journal` / `risk.*`，读到的数据严格多于事件流。
   一个能连上内核的客户端本来就有全量读权限，所以「少发事件」不会限制任何人的能力，
   只会让面板变瞎。**过滤不能假装成权限**——若将来真需要按账户鉴权，那需要
   「每连接身份」这个当前不存在的东西（`SessionState` 只有 `session_active`，
   `ipc/server/mod.rs:288-293`），属于另一个 RFC。
2. **失效方向不对称。** 多收 = 面板多几行，而且前端已经有一层账户过滤（`account-filter.ts:42-47`）。
   少收 = `RISK_ALERT` / `PositionClosed` 静默消失。前者是显示噪音，后者是**漏掉止损/强平通知**，
   属于资金风险。默认值必须往噪音那一侧倒。
3. **静默性。** 全拒的失效模式是「面板看起来安静」而不是「面板看起来坏了」——这正是
   `STATUS.md` §3 第 3 条「UI 会说谎」的同类问题，也是最难被发现的一类。
4. **向后兼容。** 全收 = 今天的全量广播，所以「引入 filter 但没人配置」不会造成任何行为回归。
   全拒则会在接线当天让所有面板静默失明。

### 4.2 配套的两条硬规则（建议一并批准）

- **安全事件不可过滤。** `Ready` / `RiskAlert` / `Error` 必须**无视 filter 恒发**。
  一个过滤器绝不允许关掉 kill-switch 告警。若运营者不批准这条，则 §4.1 的结论要重算。
- **不提供「全拒」拼写。** `EventFilter` 没有 `deny_all` 构造：想要静默的调用方应该关连接。
  加一个 `deny_all` 只会制造「我配了 filter 所以我在看全部」的错觉。

### 4.3 三维度统一规则

`accounts` / `venues` / `kinds` 三个字段用**同一条**规则（空 = 不受约束），不做逐字段特例。
`scope == None` 的事件（内核级）对 `accounts` / `venues` 约束**一律放行**——否则
`RISK_ALERT` 会被一个账户过滤顺手滤掉，直接违反 §4.2 第一条。

---

## 五、兼容期方案（旧客户端收到带 seq 的结构怎么办）

现状：`params` 直接就是 `Event`，`kind` 在 `params.kind`。任何把 `params` 换成信封的方案
都是**破坏性**的：`ui/ui_kit/src/core/types.rs:1028` 的 `CoreEvent`
（`#[serde(tag = "kind", ...)]`）会在 `params.kind` 找不到 tag 而整条反序列化失败，
`event_bus.rs:159` 的 `from_value::<CoreEvent>` 静默 `.ok()?` 掉——即**事件全丢且无报错**。
这是最坏的一种兼容破坏。

### 三个候选

| 方案 | 做法 | 旧客户端 | 新客户端 | 代价 |
|---|---|---|---|---|
| **A（建议）** | seq/scope 作为**通知层的兄弟字段**：`{jsonrpc, method:"core.event", seq, scope, params: Event}` | **零改动**（只读 `method`/`params`） | 读兄弟字段 | `EventEnvelope` 的 `event` 在线上被摊平成 `params` |
| B | 新方法 `core.event.v2`，params = 信封，与 `core.event` **并列双写** | 零改动 | 订阅 v2 | 推送流量翻倍；两个 method 的语义会漂移 |
| C | 直接把 `params` 换成信封 | **全丢**（见上） | 需同步改 `ui_kit` | 破坏性，且失效是静默的 |

**建议 A。** 理由：

- 它是唯一**零改动旧消费方**的方案。`params` 仍是 `Event`，所有既有 serde/zod schema 原样有效，
  `schema_family_golden.rs` family 21 一个字节不变。
- 没有 B 的双份流量和双份语义。
- 新增字段是**opt-in 可观测性**：今天所有客户端都只解析 `params`，所以它们看到的字节流完全没变。
- 不需要双写一版。**不建议并列双写**：两套序号/两套形状必然漂移，
  且 `R5` 精神是只做一件事。

### A 方案下的类型关系（给接线阶段）

`EventEnvelope` 仍然有用，它有两个角色：
1. **进程内**传递：`Core::emit` 之后、序列化之前，内核内部流动的就是 `EventEnvelope`
   （`broadcast` 环里存的是它，seq 在入环前就固定了——这正是缺陷 3 能被检测的原因：
   序号在**入环之前**分配，出环时的 `Lagged` 就能被换算成「漏了 seq ∈ [x, y]」）。
2. **线上**：`EventScope` 与 `seq` 直接复用，`event` 摊平进 `params`。

因此线上形状是 A，类型是 `EventEnvelope`——两者不冲突，只是 `EventEnvelope` 的
`event` 字段在序列化时由 `Notification` 承担。**这也是为什么本阶段不动 `Notification`**：
给 `Notification` 加字段会连带改 `Notification::new`，而它唯一的调用点在
`session.rs:1046`——那是被明令禁止改动的文件。

---

## 六、接线点清单（**本阶段一律不改**，审批后按序动）

| # | 位置 | 现状 | 接线时要做什么 |
|:--:|---|---|---|
| 1 | `core/blitzkrieg_core/src/service/risk_admission/freeze.rs:633` | `Core::emit(&self, ev: Event)` → `tx.send(ev)` | **唯一的 seq 分配点**。`&self` 下要单调需 `AtomicU64`（51 个调用点 / 16 个文件，全部经过这里） |
| 2 | `core/blitzkrieg_core/src/service.rs:356` | `tx: Option<mpsc::UnboundedSender<Event>>` | 通道元素类型改成 `EventEnvelope` |
| 3 | `core/blitzkrieg_core/src/service/assembly.rs:1729` | `set_event_sink(tx)` | 签名随 #2 改 |
| 4 | `core/blitzkrieg_core/src/ipc/server/session.rs:25` | `mpsc::unbounded_channel::<Event>()` | 改成 `EventEnvelope` |
| 5 | `core/blitzkrieg_core/src/ipc/server/session.rs:26` | `broadcast::channel::<Event>(512)` | 改成 `EventEnvelope`；容量 512 是否够，见 §7 Q5 |
| 6 | `core/blitzkrieg_core/src/ipc/server/session.rs:176` | `core.set_event_sink(evt_tx)` | 随 #2/#4 |
| 7 | `core/blitzkrieg_core/src/ipc/server/session.rs:929` | `bus_tx.subscribe()` | 随 #5 |
| 8 | `core/blitzkrieg_core/src/ipc/server/session.rs:1029-1055` | 每会话转发循环 | 在这里加 `EventFilter` 判定；`Lagged` 处理 |
| 9 | `core/blitzkrieg_core/src/ipc/server/session.rs:1035-1045` | 唯一的过滤臂（`KlineUpdate` 订阅） | 由通用 `EventFilter` 取代；`kline_subs` 语义要保留 |
| 10 | `core/blitzkrieg_core/src/ipc/server/session.rs:1052` | `Lagged(_) => continue` | 改为「计数 + 上报缺口」 |
| 11 | `core/blitzkrieg_core/src/ipc/server/session.rs:1000` | 出站 `unbounded_channel::<String>()` | 缺陷 4 的 OOM 面；**建议不在本 Issue 内改**（R5），只记 Issue |
| 12 | `core/blitzkrieg_core/src/ipc/server/mod.rs:288-293` | `SessionState { session_active, kline_subs }` | `EventFilter` 的归属地（每连接一份） |
| 13 | `core/blitzkrieg_core/src/schema/views.rs:1343-1357` | `Notification { jsonrpc, method, params }` | 按 §5 方案 A 加 `seq` / `scope` 兄弟字段（可选、`skip_serializing_if`） |
| 14 | `core/blitzkrieg_core/src/schema/mod.rs:60` | `EVENT_METHOD = "core.event"` | 不变（方案 A 不需要新 method） |
| 15 | `core/blitzkrieg_core/src/backtest.rs:748` | 第二个 `set_event_sink`（回测装配） | 随 #2；seq 在回测里也要单调，否则回放门禁会看到倒退 |
| 16 | `ui/ui_kit/src/core/types.rs:1028` | `CoreEvent`（`params` 的镜像） | 方案 A 下**不需要改**；要读 seq 才加 |
| 17 | `ui/ui_kit/src/core/event_bus.rs:152-164` | `ingest_notification` 只读 `method` / `params` | 方案 A 下**不需要改** |
| 18 | `ui/ui_kit/src/core/event_bus.rs:23,53-68` | 网关自己的 `seq`（解码后分配） | 应改为**透传内核 seq**，否则两层序号会打架（见 §1 的「长错层」） |
| 19 | `ui/webapp/webui/src/lib/account-filter.ts:42-47` | 前端账户过滤 | 内核过滤生效后可退化为兜底；**归前端（RULES §四），本 Issue 不碰** |
| 20 | `ui/webapp/webui/src/lib/sse.ts:23-25` | SSE 帧只有 `snapshot` / `ping` | **确认 SSE 不承载事件** → 本 Issue 无需改 SSE |

### 6.1 关于 C ABI（R7）

`BK_ABI_VERSION = 2` 已冻结，本契约**不跨越 ABI**：
`EventEnvelope` 走的是 JSON-RPC 通知，而策略侧的事件面是
`crate::extension::ExtensionContext::emit(&self, event: Event)`（`service/helpers.rs:22`）。
若将来要让扩展也感知 seq/scope，必须走「可选符号」路径（`DEV.md` §9.1 已有两例），
**不得**改 vtable 槽位。本 RFC 不需要它，故不做。

---

## 七、需要运营者拍板的具体问题

| # | 问题 | 我的建议 | 理由 |
|:--:|---|---|---|
| Q1 | `EventFilter` 空值 = 全收还是全拒？ | **全收** | §4.1 四条；核心是失效方向不对称（漏风险告警 > 多显示几行）与「过滤≠授权」 |
| Q2 | `Ready`/`RiskAlert`/`Error` 是否无视 filter 恒发？ | **是，恒发** | 过滤器不允许关掉 kill-switch 告警。这条与 Q1 是一组，不批则 Q1 要重算 |
| Q3 | `OrderUpdate`/`Fill` 的 `EventScope.venue` 从哪来？ | 接线时从**执行/市场上下文**取，取不到填 `None` | `TrackedOrder` 无 venue 字段（`model.rs:203`）；不许让客户端猜 |
| Q4 | `Lagged` 的缺口怎么上报？ | 复用 `Event::Error`（带 `seq` 区间）**先上**；新增 `EVENT_GAP` 变体**等审批** | 新增变体 = 改 `Event` = 改 golden wire，属于 `[CONTRACT-BREAKING]`，必须单独批准 |
| Q5 | 广播环 512 是否要调？ | **不动**，先靠 seq 把「漏了」变可见 | 调容量是猜；先把可观测性装上，用真实 `Lagged` 计数再定容量 |
| Q6 | 线上形状取 A（兄弟字段）/ B（v2 双写）/ C（换 params）？ | **A** | §5；只有 A 是旧消费方零改动，且失效不会是静默的 |
| Q7 | seq 要不要持久化 / 加 epoch？ | **都不要** | §3.1；持久顺序的权威是账本，加 epoch 只改善诊断不改善正确性 |
| Q8 | `EventKind` 是否允许将来与 `Event` 变体不同步？ | **不允许**，靠 `Event::kind()` 的穷尽 match 强制 | §2.1；否则缺陷 1 会一个变体一个变体地复发 |

---

## 八、本阶段的验证

`core/blitzkrieg_core/src/schema/views/event_contract.rs:150` 起 `#[cfg(test)] mod event_contract_tests`，6 条断言：

| 测试 | 钉住什么 |
|---|---|
| `envelope_is_flat_camel_case_with_seq_scope_event` | 信封线上形状：`{seq, scope:{account, venue}, event}`，camelCase |
| `unscoped_envelope_omits_the_scope_key` | `scope: None` 必须**缺键**，不能是 `null`（否则「无 scope」与「解析失败」不可分） |
| `event_kind_wire_values_match_the_event_tag_vocabulary` | `EventKind` 的 wire 值 = `Event` 的 tag 词汇（双向） |
| `every_event_variant_maps_to_its_own_kind` | `Event::kind()` 的对应关系 |
| `default_filter_is_empty_on_every_axis` | **Q1 的安全默认值**：`EventFilter::default()` 三维全空 = pass-through |
| `filter_round_trips_through_json` | `EventFilter` 可反序列化（将来从会话命令解析） |

这 6 条钉的是**待审批的形状**，不是行为；改动它们等于改 wire，必须回到本 RFC。

门禁：`cargo build --release --workspace --locked` / `cargo test -p blitzkrieg-core --lib` /
`cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets -- -D warnings` 全绿，
**零既有测试改动、零行为变化**。

---

## 九、非目标

- 不改 `Event` 枚举的任何变体或字段（golden family 21 逐字节不变）。
- 不改 `session.rs` 的广播/转发逻辑、不改 `Notification`、不改 SSE、不改前端。
- 不碰 C ABI vtable（`BK_ABI_VERSION = 2` 冻结）。
- 不改账本、风控阈值、`ImmutableConfig`（R7）。
- 不解决缺陷 4（出站队列无界 OOM）——那是独立 Issue，见 §6 第 11 行。
- 不启用 Live（R1）。

---

_作者：BlitzkriegBot · 契约阶段交付 · 待运营者审批后方可接线_
