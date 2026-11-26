//! IPC method-name constants. Moved verbatim from `schema/mod.rs` (pure code
//! motion).

pub mod method {
    pub const PING: &str = "core.ping";
    pub const READY: &str = "core.ready";
    /// Read-only quote of the taker fee schedule the kernel is charging (#182).
    pub const CORE_FEE_QUOTE: &str = "core.feeQuote";
    pub const RISK_KILL: &str = "risk.kill";
    pub const RISK_RESUME: &str = "risk.resume";
    /// Limited hot reload of the ENTRY limits (#191): the per-order /
    /// portfolio notional caps and the entry share band, applied in memory
    /// with an audit record. Everything else is refused by name — see
    /// [`crate::risk::hot_reload_refusal`].
    pub const RISK_SET_LIMITS: &str = "risk.setLimits";
    /// 生效风控可编辑（用户裁决：单笔最大亏损、当日最大回撤等设置选项必须
    /// 都可以被编辑）：六项账户级限额的运行时写路径（issue #983：风控只
    /// 跟随账户，无全局限额）。三项热生效（每笔入场现场判读、只约束新
    /// 敞口、不携带计数器）；回撤预算与连亏熔断对按「下次会话」语义落盘
    /// 态生效 —— 不是热旋钮（HOT_RELOAD_REFUSED 记名拒绝的理由仍然成立），
    /// 回执里逐项注明生效时机。持久化：写路径把改动的金额字段合并进
    /// SQLite `account_risk` 行（`StoreApi::save_risk_bounds`），重启后
    /// 从库里重新武装 —— 操作者裁决 2026-11-14：`.env` 对内核只读，不再写回。
    pub const RISK_SET_SYSTEMIC: &str = "risk.setSystemic";
    /// 退出纪律可编辑（用户裁决：Gate 4 绑定的止损/止盈/强平必须可以改）：
    /// 退出纪律的运行时写路径。**live** —— 对下一笔入场立即生效（Gate 4
    /// 每次入场现场从活配置绑定）；已开仓位的退出扫描按 `PositionManager`
    /// 的活配置判读，同样立即跟随新值 —— 回执逐项写明，不静默。
    /// 持久化：写路径把退出纪律落成 SQLite `account_exit_policy` 行
    /// （`StoreApi::save_exit`），重启后从库里重新解析出同样的值 ——
    /// 操作者裁决 2026-11-14：`.env` 对内核只读，不再写回。
    pub const RISK_SET_EXIT: &str = "risk.setExit";
    /// 轮次时间周期的运行时读数（UI 设置卡）：当前生效秒数、来源
    /// （`cli_or_env` / `default`，由启动解析裁定）、可切换的白名单档位与
    /// 「下次会话生效」的回执
    /// 语义。只读。
    pub const ROUND_DURATION_GET: &str = "round_duration.get";
    /// 轮次时间周期的运行时写路径（用户需求：UI 可以把 15m 切到 5m 等）。
    /// **live** —— 秒切秒换，本会话立即生效：先撤该账户在途轮市场的全部挂单、
    /// 按市价真实强平其仓位（PnL 按真实成交价，台账 `operator_grid_switch`），
    /// 再切网格；任一步 fail-closed。持久化落 SQLite `account_settings`
    /// （key=`round.duration_sec`，操作者裁决 2026-11-14），重启后 overlay 回
    /// 账户网格；无 SQLite 库的内核只在本会话生效并如实告知。
    /// 显式 `--round-sec`（普查/回放命令行）锁定网格，破坏性切换被如实拒绝 ——
    /// 逐位复现的回放基线永远不被 UI 设置劫持。
    pub const ROUND_DURATION_SET: &str = "round_duration.set";
    /// 行情插件的运行时读数（插件页）：本会话 active 插件、编译进内核的注册表、
    /// 「下次会话将选什么」（持久格 outrank 首个注册者，显式 `--market-plugin`
    /// outrank 持久格）。只读。
    pub const MARKET_PLUGIN_GET: &str = "market.plugin.get";
    /// 行情插件的 next_session 写路径：feed/discovery/executor 生命周期都绑定
    /// 启动选择，热切在架构上不可行 —— 只写持久格 `marketPlugin`
    /// （data/runtime/config.json），重启后启动层读回。plan-first：未注册名
    /// 拒绝（注册表编译在内核里，一个不存在的名字不能等到重启才发现）。
    /// 显式 `--market-plugin` outrank 持久格 —— 回放基线不被 UI 劫持。
    pub const MARKET_PLUGIN_SET: &str = "market.plugin.set";
    /// 行情插件凭据：只读报表（每插件所需 env 变量名 + 下一会话是否齐全，
    /// 值永不过线）与 next_session 写路径（原子写持久格 credentials，
    /// 重启后 boot 注入环境；真实环境变量 outrank 格）。名字按插件需求表
    /// plan-first 校验。
    pub const MARKET_CREDENTIALS_STATUS: &str = "market.credentials.status";
    pub const MARKET_CREDENTIALS_SET: &str = "market.credentials.set";
    pub const ORDER_PLACE: &str = "orders.place";
    pub const ORDER_CANCEL: &str = "orders.cancel";
    pub const ORDER_CANCEL_ALL: &str = "orders.cancel_all";
    /// E31-c: cancel the resting remainder of a partially filled order.
    pub const ORDER_CANCEL_REMAINING: &str = "orders.cancel_remaining";
    /// E31-c: sell only the shares already filled behind an order.
    pub const ORDER_CLOSE_FILLED: &str = "orders.close_filled";
    /// #457: kernel-side force-close of a GHOST order — one whose round has
    /// expired, so it can never fill, yet no honest terminal receipt ever
    /// arrived. Refuses a still-live round (that is `orders.cancel`'s job);
    /// marks `ForceClosed` in the audit trail so "we withdrew" and "we gave
    /// up waiting" stay readable apart.
    pub const ORDER_FORCE_CLOSE: &str = "orders.force_close";
    pub const ORDER_LIST: &str = "orders.list";
    pub const LEDGER_BALANCE: &str = "ledger.balance";
    /// List open positions (enriched with current price / unrealised PnL).
    pub const POSITIONS_LIST: &str = "positions.list";
    /// Force-close a position at market (manual flatten).
    pub const POSITION_EXIT: &str = "positions.exit";
    /// #684: the operator's clear-out verb for a position whose market has
    /// EXPIRED and whose settlement verdict never arrived (the updown round
    /// list only carries future rounds, so an expired condition can never be
    /// answered). Kernel gates: position exists, expiry is genuinely past (a
    /// live round is refused — that is `positions.exit`'s job), and a
    /// manual-ops audit path is configured. Closes at a bookkeeping price
    /// (last known mid, else entry) with NO redemption claim; the audit row
    /// names the operator.
    pub const POSITION_FORCE_CLOSE: &str = "positions.force_close";
    /// Closed-trade history (Node-compatible records) for the UI.
    pub const TRADES_HISTORY: &str = "trades.history";
    pub const TRADES_SUMMARY: &str = "trades.summary";
    /// Issue 991: the UNVERIFIED EXPOSURE report — every closed position whose
    /// exit price is still a bookkeeping estimate (`source == "fallback"` or
    /// `"reconciled"`; a `venue` close never appears). Read-only, empty params.
    /// Separate from realized PnL by construction; the UI display is the
    /// panel's lane, this is only the query surface.
    pub const POSITIONS_UNVERIFIED_EXPOSURE: &str = "positions.unverified_exposure";
    /// Manually trigger a reconciliation sweep with a caller-supplied snapshot
    /// (live venue snapshot is gathered internally in live mode; this method is
    /// mainly for tests/ops).
    pub const ORDER_RECONCILE: &str = "orders.reconcile";
    /// P0 bridge: Node pushes the latest L2 snapshot so DRY mode can simulate
    /// fills. Replaced in P3 when market-data ingestion moves into the core.
    pub const BOOK_SNAPSHOT: &str = "books.snapshot";
    /// Top-of-book only update (price_change / best_bid_ask).
    pub const TOP_OF_BOOK: &str = "books.top";
    /// Data-feed bridge: deliver a full L2 book straight to the self-driving
    /// engine (`Core::engine_on_data`) — the same choke point the Rust-native
    /// market feed (`--feed-ws`) drives and the one the backtester replays.
    /// Unlike `books.snapshot` it does NOT run the DRY maker-fill simulation, so
    /// a session captured through this method replays decision-for-decision.
    pub const ENGINE_BOOK: &str = "engine.book";
    /// Binance spot price tick (feeds the momentum filter).
    pub const SPOT_PRICE: &str = "spot.price";
    /// Current round state (slot / timing / market count).
    pub const ENGINE_ROUND: &str = "engine.round";
    /// Read-only depth view: the round's assets with each side's live L2 levels,
    /// best prices, OBI and spread — what the WebUI's 盘口深度 chart renders.
    /// Same books the engine trades on; purely observational, no side effects.
    pub const ENGINE_BOOKS: &str = "engine.books";
    /// Diagnostic snapshot: feed counters + confirmed trend tokens.
    pub const ENGINE_STATS: &str = "engine.stats";
    /// List strategies registered in the kernel's strategy engine.
    pub const STRATEGY_LIST: &str = "strategy.list";
    /// Enable/disable a strategy by name.
    pub const STRATEGY_ENABLE: &str = "strategy.enable";
    /// Load a user-layer strategy shared library (feature `strategy-loading`).
    pub const STRATEGY_LOAD: &str = "strategy.load";
    /// Unload a dynamic strategy (drop instance + free the library). Refuses
    /// in-tree or still-enabled strategies and open positions (E9-b).
    pub const STRATEGY_UNLOAD: &str = "strategy.unload";
    /// Atomic swap (E9-b): load the NEW dylib under the SAME registered name,
    /// dropping the old instance only after the new one parses; enable state
    /// is preserved. `params: {name, path}`.
    pub const STRATEGY_RELOAD: &str = "strategy.reload";
    /// Issue #481 (G3): one strategy's declared tunables + the values in force.
    /// Read-only; the schema source is the strategy's OWN knob declaration
    /// (manifest tunables for Lua packages), the kernel invents nothing.
    pub const STRATEGY_PARAMS_GET: &str = "strategy.params.get";
    /// Issue #481 (G3): the tunables' operator write path — validate against
    /// the declared domains (plan first, write later), hot-swap the value the
    /// strategy reads on its next evaluate, audit the change. The same
    /// `risk.setExit` posture: refused values move nothing, receipts name
    /// every old→new.
    pub const STRATEGY_PARAMS_SET: &str = "strategy.params.set";
    /// Re-scan the Lua strategy dir (`user_layer/strategies_lua`) and register
    /// any package not already in the engine's provenance ledger — the
    /// non-restart recovery path for a package refused at boot (issue #975).
    /// `params: {}` (no arguments). Returns the names freshly loaded so
    /// `strategy.list` shows them again.
    pub const STRATEGY_PACKAGES_RELOAD: &str = "strategy.packages.reload";
    /// Issue #481 (G4): the confirmed cross-venue mappings as a read-only
    /// snapshot — per event: status, the venues holding a leg, and the
    /// discrepancy marks. The panel's 配对视图 data source.
    pub const UNIFIED_LIST: &str = "unified.list";
    /// ④: the latest round's cross-venue mapping CANDIDATES (auto-matching
    /// proposes, the operator disposes). Read-only: `a`/`b` listings + score.
    pub const UNIFIED_CANDIDATES: &str = "unified.candidates";
    /// ④: the operator confirms a candidate pair as a cross-venue mapping.
    /// Params: `title` (operator-facing event name), `listings` (≥2, distinct
    /// venues — the candidate JSON round-trips verbatim), optional `actor`
    /// (default "panel"). The ledger revalidates everything; a refused
    /// confirm moves nothing.
    pub const UNIFIED_CONFIRM: &str = "unified.confirm";
    /// ④: withdraw a confirmed mapping. Params: `eventId`, optional `actor`.
    pub const UNIFIED_REVOKE: &str = "unified.revoke";
    /// Extension lifecycle: list / enable / disable / uninstall.
    pub const EXTENSION_LIST: &str = "extension.list";
    pub const EXTENSION_ENABLE: &str = "extension.enable";
    pub const EXTENSION_DISABLE: &str = "extension.disable";
    /// Market plugins: list registered market extensions (name/type/capabilities).
    pub const MARKET_LIST: &str = "market.list";
    /// Network self-check: probe the active market plugin's network paths
    /// (resolver → TCP → TLS → one cheap request per endpoint) and answer with a
    /// `NetCheckReport`. Read-only and credential-free; the answer to "is this a
    /// network problem or a venue problem?" when a bot has gone quiet.
    pub const NET_CHECK: &str = "net.check";
    /// Shadow Evolution control surface (opt-in feature). Every mutation is
    /// per-strategy (E2-c): `apply`/`rollback` name ONE strategy, and `status`
    /// reports each evolved strategy's own block.
    pub const SE_ENABLE: &str = "shadow_evolution.enable";
    pub const SE_DISABLE: &str = "shadow_evolution.disable";
    pub const SE_STATUS: &str = "shadow_evolution.status";
    pub const SE_HISTORY: &str = "shadow_evolution.history";
    pub const SE_ROLLBACK: &str = "shadow_evolution.rollback";
    pub const SE_APPLY: &str = "shadow_evolution.apply";
    /// E13 proposal workflow: every known proposal's latest state (the UIs'
    /// 对比表), the operator's verdict on one proposal, and the auto-evolve
    /// switch (`params.enabled: bool`, persisted across restarts).
    pub const SE_PROPOSALS: &str = "shadow_evolution.proposals";
    pub const SE_DECIDE: &str = "shadow_evolution.decide";
    pub const SE_SET_AUTO: &str = "shadow_evolution.set_auto";
    /// Supply the current round's UP/DOWN markets (discovered by Node's scanner
    /// in P3-transition; the Rust scanner takes over in P4).
    pub const ENGINE_MARKETS: &str = "engine.markets";
    /// Version + build provenance + update state (VERSIONING.md §5).
    /// Request: empty params `{}`; reply: a `SystemVersion` payload. Read-only,
    /// side-effect-free, takes no Core lock.
    pub const SYSTEM_VERSION: &str = "system.version";
    /// Explicitly re-read reload-safe values from `.env`.
    pub const SYSTEM_ENV_RELOAD: &str = "system.env.reload";
    /// Read/write of the update switches (§7.4). The write lands an audit record.
    pub const SYSTEM_UPDATE_CONFIGURE: &str = "system.update.configure";
    /// One manual update check (the UI's "check for updates" button).
    pub const SYSTEM_UPDATE_CHECK: &str = "system.update.check";
    /// #379: download + verify the newer release into the staging directory
    /// (`data/update/staging/`). REPLACEMENT is the launcher's job (§7.5) —
    /// this arm only advances the state machine to "restart-ready", it never
    /// touches a running binary.
    pub const SYSTEM_UPDATE_STAGE: &str = "system.update.stage";
    /// #458: wipe this core's trading state and re-seed — stop enabled
    /// strategies, cancel resting orders, clear the order/trade/position
    /// records and the in-memory books, re-seed the cash to
    /// `dry_seed_balance`. Refused in Live mode (see
    /// `RESET_TRADING_LIVE_REFUSAL`); requires `confirm: "RESET"`. Every
    /// invocation appends a record to the manual-ops audit file.
    pub const SYSTEM_RESET_TRADING: &str = "system.reset_trading";

    // ── v0.3 Wave 0 (#329) — the two read-only envelope freezes ─────────────
    // The two arms Wave 0 lands in `server.rs`, with their wire shapes frozen
    // here (§12.2/§12.3). Both answer truthfully with an empty envelope until
    // their producers land (E25: `data/audit/intents.jsonl`; E29: the K-line
    // aggregator); neither takes the Core lock.

    /// Historical K-lines for one `(symbol, interval)` (§12.2). Read-only.
    pub const KLINE_HISTORY: &str = "kline.history";
    /// Tail of the intent-arbitration audit log (§12.3). Read-only.
    pub const INTENT_AUDIT_TAIL: &str = "intent.audit.tail";

    // ── E28 (§12.1) — account management ────────────────────────────────────
    /// Every configured account with its own book (§12.1 AccountView).
    /// Read-only.
    pub const ACCOUNT_LIST: &str = "account.list";
    /// Move THIS connection's default account (§9.5: session-level — the
    /// process-level active account and every other connection are
    /// untouched).
    pub const ACCOUNT_SWITCH: &str = "account.switch";
    /// Tighten one account's lifecycle status (§12.1: a live call may never
    /// GRANT a capability — unfreezing is config + restart).
    pub const ACCOUNT_STATUS: &str = "account.status";
    /// The account↔venue binding readout: one row per configured account
    /// (empty string = unbound). Read-only.
    pub const ACCOUNT_VENUE_GET: &str = "account.venue.get";
    /// The account↔venue binding write (`accountId` + `venue`): non-empty
    /// venue pins the account to that market-plugin name (MUST be registered
    /// — plan-first), the empty string unbinds. The SAME venue on SEVERAL
    /// accounts is the deployment model (one exchange, several API sets, one
    /// account each), so no uniqueness is enforced. Next the binding gates
    /// `orders.place` cross-venue (the existing refuse_cross_venue_order
    /// guard reads the in-memory field), and — forward-looking — it becomes
    /// the next batch's plugin-ENABLEMENT derivation input (boot reads the
    /// account book and enables every venue at least one account is bound
    /// to), which is why the write persists to the SQLite store (the
    /// `accounts.venue` column, issue 751's seam) and must never
    /// lose a sibling account's row on the way.
    pub const ACCOUNT_VENUE_SET: &str = "account.venue.set";
    /// The account's round-length FAMILY write (`account.rounds.set`,
    /// 轮次多族 B2): the set spells exactly which (venue, duration) families
    /// the account trades. An ACCOUNT-CONFIGURATION MANAGEMENT verb, the same
    /// family as `account.venue.set`/`account.list` — it writes the CONFIG
    /// PLANE's fact (the SQLite `accounts.rounds_json` column + the in-memory
    /// book), its consumers are
    /// the panel's account card and the ops CLI, and it is NOT the strategy
    /// runtime's business-verb family #568 vetoed (策略运行时对周期集的感知
    /// 走内存共享态，不另开业务动词). The write is ONE transaction (persist
    /// FIRST, then memory): a removed family with open positions refuses the
    /// WHOLE write, a removal with no exposure soft-unloads the family's
    /// poll output, an added family mounts its discovery instance at runtime
    /// (增族即挂). Empty set = the row's rounds column is emptied (the
    /// unrestricted spelling, identical to an absent set at the next boot).
    pub const ACCOUNT_ROUNDS_SET: &str = "account.rounds.set";
    /// The per-account trading switch (`account.trading.set`): `false` =
    /// this account's NEW ENTRIES are refused while its EXITS keep flowing
    /// (positions stay strategy-managed until flat). Persisted to the
    /// SQLite `accounts.trading_enabled` column, re-read at next boot.
    pub const ACCOUNT_TRADING_SET: &str = "account.trading.set";
    /// The per-account FREEZE (account-scoped kill switch,
    /// `account.freeze`): `{accountId, frozen, reason}` — a frozen account
    /// refuses NEW orders (manual AND strategy entries; the engine drops its
    /// candidates) while its EXITS keep flowing. SESSION-scoped: an
    /// automatic freeze (venue-rejection streak) lives in memory only, so
    /// the lift is too — a restart starts clean and re-probes.
    pub const ACCOUNT_FREEZE: &str = "account.freeze";
    /// #1045: unfreeze one account's entry gate, returning
    /// `{unfrozen, previousReason, entriesAllowed}` and writing an audit log
    /// (actor/reason/time). This is the ONLY unfreeze write path —
    /// `account.freeze {frozen: false}` is retired (#1059) and the handler
    /// refuses it.
    pub const ACCOUNT_UNFREEZE: &str = "account.unfreeze";
    /// The per-account strategy ENABLEMENT write (`account.strategies.set`
    /// / the persistence half of the panel's per-account 启用按钮): the
    /// set spells exactly which (strategy, account) scopes run; a removed
    /// strategy with open positions keeps managing them exits-only until
    /// flat.
    pub const ACCOUNT_STRATEGIES_SET: &str = "account.strategies.set";
    /// The DISPLAY pointer write (显示账户配置化): point the snapshot's
    /// `displayAccount`/`displayVenue` at the named account. A pure
    /// presentation pointer — it does NOT touch the active/accounting/
    /// strategy-enablement axes and no trading semantics attach to it.
    /// The kernel validates the account exists AND is `active` (plan-first),
    /// updates the in-memory pointer AND persists the SQLite `kernel_meta`
    /// `display_account` cell in one transaction; the next boot
    /// re-reads it (fail-closed on a dangling id).
    pub const DISPLAY_ACCOUNT_SET: &str = "display.account.set";
    /// One account's cash-journal tail (#509's read surface): the append-only
    /// `JournalEntry` rows the ledger records for every balance change.
    /// Read-only — answers from the in-memory window
    /// (`journal_snapshot()`), never touches the file. `accountId` absent =
    /// the SESSION's default account (the same resolution `account.list`
    /// uses); `limit` defaults to 100, capped at 500.
    pub const ACCOUNT_JOURNAL: &str = "account.journal";
    /// Create ONE account at runtime (`accountId` + optional `name`/`venue`):
    /// the identity lands in the in-memory book AND the SQLite store (the
    /// `accounts` row) — the same
    /// plan-and-persist transaction `account.venue.set` runs. Status is
    /// ALWAYS `active` (a runtime call that minted a restricted account would
    /// be a loosening in disguise — tightening is `account.status`'s job);
    /// market_type is ALWAYS `prediction` (the only type a 0.3 plugin
    /// exists for, identical to the file's own default). NO credential field
    /// travels here (§9.4: the value never enters the kernel) — credentials
    /// are the existing `market.credentials.set` env-name path, wired by the
    /// panel after the identity exists. `default` is refused (reserved).
    pub const ACCOUNT_CREATE: &str = "account.create";
    /// Remove ONE account at runtime: the row leaves the in-memory book AND
    /// the SQLite store (the `accounts` row, FK cascades + removal tombstone).
    /// `default` is refused (reserved); an account with any
    /// OPEN position is refused (its money must not become unaddressable);
    /// removing the process-active account re-activates `default` (the
    /// session switchers resolve against the book, this only fixes the
    /// process-level cell).
    pub const ACCOUNT_REMOVE: &str = "account.remove";

    // ── #363 — execution policy surface ─────────────────────────────────────
    /// List every account section the loaded policy names, plus the effective
    /// global defaults. Read-only.
    pub const EXECUTION_POLICY_LIST: &str = "execution_policy.list";
    /// One account's effective section (its own overrides folded over the
    /// globals) and its rules. Read-only.
    pub const EXECUTION_POLICY_GET: &str = "execution_policy.get";
    /// Write ONE account's section to the policy TOML (section-replace,
    /// never a whole-file rewrite of other accounts), then full-reload the
    /// policy in memory and land one audit line. Refuses accounts other
    /// than the named one — cross-account writes are a fail-closed no.
    pub const EXECUTION_POLICY_SET: &str = "execution_policy.set";
    /// Remove one account's section from the file (it falls back to the
    /// globals), full-reload, audit.
    pub const EXECUTION_POLICY_RESET: &str = "execution_policy.reset";
    /// The audit journal tail for one account (or all accounts).
    pub const EXECUTION_POLICY_HISTORY: &str = "execution_policy.history";
    /// #364: replay the CURRENT section over the recent closed trades of one
    /// account (`evaluate` on historical facts) — the UI preview's "these
    /// rules would skip N of the last M entries". Read-only, no audit line.
    pub const EXECUTION_POLICY_PREVIEW: &str = "execution_policy.preview";

    // ── E26 (§4.4) — systemic risk readout ────────────────────────────────
    /// The effective systemic limits and WHERE each came from. Read-only,
    /// zero side effects, and answered from a boot-time snapshot WITHOUT the
    /// Core lock: the nine new limits are restart-to-change (deliberately
    /// absent from `risk.setLimits`) and the exit resolution is not a #191
    /// hot key either, so nothing on this readout can go stale mid-run.
    pub const RISK_LIMITS: &str = "risk.limits";

    // ── E29 (§12.2) — K-line subscriptions ─────────────────────────────────
    // (`kline.history` itself was frozen by Wave 0 above; E29 extends its
    // BEHAVIOUR — real bars — without re-spelling the shape. One semantic note
    // now that bars exist: `klines` is oldest first with the still-growing bar
    // riding LAST (`isClosed = false`), so a chart draws it translucent without
    /// a second call.)
    pub const KLINE_SUBSCRIBE: &str = "kline.subscribe";
    /// Explicit counterpart; the auto-unsubscribe on disconnect makes this
    /// optional for clients, not useless (a live panel can narrow its feed).
    pub const KLINE_UNSUBSCRIBE: &str = "kline.unsubscribe";

    // ── #353 — the WebUI backtest surface (拉数据→配置→回测→看结果) ──────────
    // Every arm fronts the in-kernel job registry (`crate::backtest_jobs`):
    // the WebUI names datasets/assets/strategy names over IPC and NEVER
    // touches the local filesystem or executable code — the spec's hard
    // boundary. Pull = resumable, cache-addressable #352 pulls (one per asset
    // filter); run = the #351 mode ladder on one dataset; status/result/export
    // poll a running or finished job.
    pub const BACKTEST_ONCHAIN_PULL: &str = "backtest.onchain.pull";
    pub const BACKTEST_ONCHAIN_LIST: &str = "backtest.onchain.list";
    pub const BACKTEST_RUN: &str = "backtest.run";
    pub const BACKTEST_STATUS: &str = "backtest.status";
    pub const BACKTEST_RESULT: &str = "backtest.result";
    pub const BACKTEST_EXPORT: &str = "backtest.export";

    // ── #362 — the blueprint editor surface (compile + save) ─────────────────
    // The WebUI canvas authors a blueprint JSON and asks the KERNEL to do
    // everything that touches bytes on disk: `blueprint.compile` is the pure
    // three-stage compiler behind the preview pane (a refusal is the same
    // node-id-bearing message the CLI prints), `blueprint.save` validates THEN
    // writes the strategy package (blueprint.json + strategy.lua + manifest
    // under `user_layer/strategies_lua/<name>/`, the directory the loader
    // scans). The UI never touches the filesystem directly — the repo iron
    // rule — and the save receipt is the caller's only proof the write landed.
    pub const BLUEPRINT_COMPILE: &str = "blueprint.compile";
    pub const BLUEPRINT_SAVE: &str = "blueprint.save";
    /// #393: read one saved package's blueprint document back (the editor's
    // preload path — the read-only mirror of `blueprint.save`).
    pub const BLUEPRINT_LOAD: &str = "blueprint.load";
    /// 手写包的「在蓝图中打开」（用户裁决：编辑入口必须能打开所有 Lua 包）：
    /// 把手写包的 Lua 源码读回来（带 manifest 元数据），编辑器的源码模式
    /// 展示并允许原样改写落盘 —— 画布图是蓝图编译产物的专属表示，不编造。
    pub const BLUEPRINT_LOAD_SOURCE: &str = "blueprint.loadSource";
    /// `blueprint.loadSource` 的写回镜像：把手写包的 `strategy.lua` 原样写
    /// 回（sha256 重算进 manifest，装载器照常验证）。`overwrite` 显式，与
    /// `blueprint.save` 同一裁决 —— 静默覆盖可能换掉操作员正在启用的策略。
    pub const BLUEPRINT_SAVE_SOURCE: &str = "blueprint.saveSource";

    // ── 跨所套利弹窗 v2 — the arb leg-configuration surface ──────────────────
    /// Write the cross-venue arbitrage leg configuration (`arb.config.set`):
    /// two DISTINCT venues (`legAVenue`/`legBVenue`), the account each venue
    /// trades under (`legAAccount`/`legBAccount`), and the asset `pairs` the
    /// strategy pairs across them. Fail-closed validation names the offending
    /// field (version, venue/account presence, venue distinctness, account
    /// existence, account venue-binding coverage, pairs non-empty); a refused
    /// write moves NOTHING (SQLite rows and memory stay untouched). The kernel
    /// persists the section to the SQLite `arb_pairs` table
    /// (`StoreApi::save_arb_config`) in
    /// the same transaction the memory mirror + engine push update — the next
    /// boot re-reads them (fail-closed on a bad hand edit).
    pub const ARB_CONFIG_SET: &str = "arb.config.set";
    /// The leg configuration readout (`arb.config.get`): `configured: true`
    /// with the four leg fields + `pairs`, or `configured: false` with null
    /// legs and an empty array — never a partial shape.
    pub const ARB_CONFIG_GET: &str = "arb.config.get";

    // ── Smart Money Reverse-to-Strategy (RFC #249) ──────────────────────────
    pub const SMART_MONEY_SCAN: &str = "smart_money.scan";
    pub const SMART_MONEY_ANALYZE: &str = "smart_money.analyze";
}
