/**
 * Panel store — snapshot polling + strategy/plugin data, shared by all pages.
 */
import { defineStore } from 'pinia'
import { ref, computed, watch } from 'vue'
import {
  api, clearToken, hasToken,
  type Snapshot, type PluginsDoc, type StrategyStatsRow, type TradeRow,
  type CumulativeTradeSummary,
} from '../api/client'
import { dedupeTrades } from '../lib/trades'
import { registryStrategyRows } from '../lib/strategy-source'
import { feedProgress } from '../lib/feed'
import { classifyOutcome } from '../lib/session'

/**
 * One account row as `account.list` reports it (E28 §12.1 AccountView) — only
 * the fields pages render. Decimals cross as strings or numbers (the core's
 * Decimal wire allows both); pages format through lib/format.
 */
export interface PanelAccountRow {
  id: string
  name: string
  marketType: string
  status: string
  /** 该账户的拒绝/暂停原因（账户卡行内展示）；老内核缺省 = 不渲染。 */
  statusReason?: string
  balance: string | number
  available: string | number
  /**
   * 凭证是否已从进程环境加载（§9.4：值永不出内核）——只读存在事实。
   * 账户卡只渲染这个布尔，不渲染任何凭证值（线里也没有该字段）。
   */
  credentialsLoaded: boolean
  /** 该账户当前持仓数（停止交易时卡片披露「N 笔持仓管理中」）。 */
  openPositions: number
  /** 交易开关（多账户扇出）；false = 拒开仓、平仓照放。老内核缺省 = 开。 */
  tradingEnabled?: boolean
  /** 只读：老内核回读缺该字段时以 '' 渲染（未绑定，可交易任意交易所）。 */
  venue?: string
  /**
   * 该账户的策略启用集（启停绑定在「账户+策略」级别 —— 账户↔venue 解耦
   * 裁决）。非空 = 逐字集合；空数组 = 显式不跑（INHERIT 已废除，空集不跟
   * 随全局开关 —— issue 552 裁决）；undefined = 老内核缺字段（启用按钮降
   * 级为全局语义）。可见性与它无关：策略列表恒显全部。
   */
  strategies?: string[]
  /**
   * 该账户的轮长族声明（轮次多族 B1d；秒）。空数组 = 未声明限制（许可轴
   * 空集放行）；undefined = 老内核缺字段。面板周期集卡（PR-D）读它。
   */
  rounds?: number[]
  /** 账户自有账本的预留/可用（账户级余额瓦片走这里，进程级 ledger 不再是
   *  余额来源 —— E28 §9.5：一个账户一套账本）。 */
  reserved?: string | number
  /** 现金真值锚点（操作员裁决 2026-11-08：账本流水=唯一真相）：本账户历史
   *  累计 Seed 入金。净利的账本口径 = balance − seededTotal（构造上恒等）；
   *  成交行的模型费净利降级为「成交口径」展示。老内核缺字段 = undefined。 */
  seededTotal?: string | number
}

export const usePanelStore = defineStore('panel', () => {
  const snapshot = ref<Snapshot | null>(null)
  const plugins = ref<PluginsDoc | null>(null)
  const error = ref<string | null>(null)
  const loading = ref(false)
  const lastUpdated = ref<number | null>(null)

  /**
   * Set when the gateway answered 401: the session is gone — expired, revoked,
   * or issued by a run that has since restarted (the token lives in server
   * memory, so a restart invalidates every session).
   *
   * Deliberately separate from `error`. A dropped session is not a fault to
   * report, it is a state to act on: the shell drops back to the login form.
   * Conflating the two is what used to strand the panel on a 网关连接异常 alert —
   * an alert describes a problem the operator cannot fix from that screen.
   */
  const sessionExpired = ref(false)

  /**
   * When the orderbook counters last moved — the panel's only evidence that
   * market data is still arriving (see `lib/feed.ts` for why nothing else in
   * the snapshot can tell us). Tracked here rather than per-page so every page
   * judges liveness from the same clock, and so it survives navigation.
   */
  const feedAt = ref<number | null>(null)
  let lastProgress = -1

  /**
   * #393: one pending cross-page navigation. Pages only ever SET it; the shell
   * (App.vue) watches, switches its own tab (the tab ids are lib/nav.ts's —
   * the single source the parity/separation gates parse too) and clears it.
   * Page switching stays the shell's business; this is just the wire.
   */
  const navRequest = ref<{ tab: string } | null>(null)

  /**
   * #393: a strategy package the blueprint page should preload (set by the
   * strategy page's 编辑策略 entry). Consumed exactly once, on the blueprint
   * page's mount — a stale request must not overwrite a later manual edit.
   */
  const blueprintPreload = ref<string | null>(null)

  /**
   * issue 561（2026-10-20 操作者裁决）：display account 升格为面板唯一
   * 账户事实源 —— 切换动作 = 写 display；一切消费点（Header、行情过滤、
   * 策略归属、流水卡）读 display。内核侧缺省解析链同步改为
   * session → display → 进程 default（ipc/server.rs 的
   * effective_default_account），因此账户级聚合读数（trades.summary /
   * engine.stats 带 accountId）也按 display 走。
   *
   * 事实来源按优先级：快照顶层的 displayAccount/displayVenue（内核的
   * display_account 独立事实，2s/15s 轮询每拍带回）> refreshAccounts 镜像
   * （account.list 回执的 displayAccount，快照未到前先有一拍）> null
   * （两者都没到，渲染「未知」而不是猜 `default`）。
   */
  const displayAccount = ref<string>('')
  const displayVenue = ref<string>('')
  const accounts = ref<PanelAccountRow[]>([])

  /**
   * issue 781：写回世代号。`refreshSeq` 每开一拍轮询自增，只有最新一拍
   * 允许写回；`accountsSeq` 是账户簿读数的同一机制；`displayEpoch` 在显示
   * 账户/venue 身份变化时自增 —— 在途的旧身份请求全部作废。三者都是普通
   * 递增整数（不含时间戳等易变字段），且活在 Pinia setup store 的闭包里，
   * 重绘不会重置（身份记忆必须跨重绘存活）。
   */
  let refreshSeq = 0
  let accountsSeq = 0
  let displayEpoch = 0

  /**
   * setAccounts：refreshAccounts 的镜像入口（issue 781 后切换器不再自建
   * 投影）。行数据（账户簿）始终镜像更新；display id 只在快照尚未说话时
   * 兜底采纳（快照在场 = 内核说了话，镜像不得降格覆盖它）。写动作本身只经
   * `setDisplayAccount`。
   */
  function setAccounts(active: string, rows: PanelAccountRow[], mirrorDisplay?: string): void {
    const snapshotInForce = snapshot.value?.displayAccount !== undefined
    if (!snapshotInForce) {
      if (mirrorDisplay && mirrorDisplay !== displayAccount.value) {
        // 已知身份 A → 镜像 B 是真切换，世代自增把在途旧响应作废；
        // 首次采纳（'' → default）不是切换 —— 那一拍的快照/全局读数
        // 仍是当前那一拍的数据，不能把它作废（否则首屏要等下一拍）。
        if (displayAccount.value) displayEpoch++
        displayAccount.value = mirrorDisplay
      } else if (active && !displayAccount.value) {
        // 同上：首次从空身份采纳不推进世代。
        displayAccount.value = active
      }
    }
    accounts.value = rows
  }

  /**
   * issue 561：切户的乐观写（唯一写方 AccountSwitcher.switchTo，且只在
   * display.account.set 成功后调用）。venue 由 account.list 行派生（行未
   * 到时空串占位，下一拍快照带回内核读数）。
   */
  function setDisplayAccount(id: string): void {
    // issue 781：切换即换身份 —— 自增世代，任何在途的旧账户批处理不得写回。
    displayEpoch++
    displayAccount.value = id
    displayVenue.value = accounts.value.find((a) => a.id === id)?.venue ?? ''
  }

  /**
   * issue 561：display 账户的绑定 venue（''/undefined = 未绑定，可交易任
   * 意交易所）。行情页的 venue 过滤是这里的纯派生 —— 账户切换器就是交易
   * 所切换器，页面不另设 venue 控件（账户↔venue 配对卡的下游跟随者）。
   * 源：快照 displayVenue（内核读数，优先）→ account.list 行派生。
   */
  const activeAccountVenue = computed(() => {
    if (snapshot.value?.displayVenue !== undefined) {
      return snapshot.value?.displayVenue ?? ''
    }
    const id = displayAccount.value
    if (!id) return ''
    return accounts.value.find((a) => a.id === id)?.venue ?? ''
  })

  /** display 账户的行（账户级余额瓦片读它的 balance/available/reserved）。 */
  const activeAccountRow = computed(() => {
    const id = displayAccount.value
    if (!id) return null
    return accounts.value.find((a) => a.id === id) ?? null
  })

  // ── 兼容读数（issue 561 迁移期）──────────────────────────────────────────
  // activeAccount/displayAccount 曾是两套事实（会话格 vs 显示指针）；
  // display 升格唯一可信源后二者同指一格，统一从 displayAccount 出。保留
  // activeAccount 名字让既有消费点逐字不动（页面渐进迁移，不留双源双节拍）。
  const activeAccount = computed<string | null>(() => displayAccount.value || null)

  function requestNav(tab: string): void {
    navRequest.value = { tab }
  }
  function clearNavRequest(): void {
    navRequest.value = null
  }
  function requestBlueprintPreload(name: string): void {
    blueprintPreload.value = name
  }
  function takeBlueprintPreload(): string | null {
    const v = blueprintPreload.value
    blueprintPreload.value = null
    return v
  }

  const connected = computed(() => snapshot.value?.connected ?? plugins.value?.connected ?? false)

  /**
   * 累计净 PnL（账本口径 = balance − openingEquity，issue #1046/#1063，
   * reserved 抵押金不计入）。唯一读取入口：老内核/未上报期初权益时为
   * null，页面须显示「未知」，禁止回退到成交折叠。所有页面
   * （PnLCurveCard、StatsRow、HftPage 累计头卡、TUI）都从此处读，
   * 天然四处相等。
   */
  const cumulativeNetPnl = computed(() => snapshot.value?.balance?.cumulativeNetPnl ?? null)

  /**
   * Closed-trade rows with cross-run id collisions collapsed. `hft-N` restarts
   * at 1 on every core boot while the trade log is append-only, so a long-lived
   * log holds several rows per id; summing the raw list over-counts trades and
   * skews net PnL and win rate against the kernel's own counters. Every page
   * reads rows through here so they all report the same totals.
   */
  const tradeRows = computed<TradeRow[]>(() => dedupeTrades(snapshot.value?.tradeRows ?? []))

  // ── 全量成交行（issue 1282 快照分页的两源分工）──────────────────────────────────
  // 快路径 `tradeRows`（上一行）：15s 轮询 + 1s SSE 随快照带回的近 200 行
  // （网关 trade_limit 首屏背压）。它服务「随快照实时刷新」的读数——告警
  // 音（useAlertSounds）、订单终态归因（useOrders）。这些视图只关心最近
  // 的成交/订单，200 行窗绰绰有余。
  // 深路径 `fullHistoryRows`：append-only 成交日志的**全史**投影。任何
  // 「要全部历史才口径正确」的视图——历史 tab 表格、权益曲线（Overview 的
  // equityCurveRows 与两页共用的累计净 PnL 卡）、cumStats 的行折叠回落——
  // 只能从这里读：直接读快路径会把 200 行之外的成交静默截出曲线和统计。
  // 实现按（display 账户, venue）身份惰性分页拉取：GET /api/trades
  // （trades.history {offset, limit} 薄代理）每次 FULL_TRADES_PAGE 行、
  // offset 从最新端往回数，翻到短页即底；页间先按「最旧页在前」拼接
  // （每页内部是文件序 = 时间升序），最后 dedupeTrades 保持升序，权益曲线
  // 直接可用。身份变化（切户/切所）即作废在途请求并重拉——全史归属必须
  // 跟随显示身份，与 refresh() 的 isCurrent 仲裁同一纪律。
  const FULL_TRADES_PAGE = 500
  const fullTradeRows = ref<TradeRow[] | null>(null)
  let fullTradesSeq = 0

  async function loadFullTrades(): Promise<void> {
    const requestId = ++fullTradesSeq
    const epoch = displayEpoch
    const pages: TradeRow[][] = []
    for (let offset = 0; ; offset += FULL_TRADES_PAGE) {
      try {
        const doc = await api.tradesPage({ offset, limit: FULL_TRADES_PAGE })
        // 身份在途变更（切户/切所/登出）：本批作废，身份 watch 已重拉。
        if (requestId !== fullTradesSeq || epoch !== displayEpoch || !hasToken()) return
        const page = Array.isArray(doc.trades) ? doc.trades : []
        pages.push(page)
        // 短页 = 翻过最旧一行，全史取完（内核 offset 越界返回空页）。
        if (page.length < FULL_TRADES_PAGE) break
      } catch {
        // 失败不抛：fullTradeRows 保持 null/旧值，消费入口回落快路径
        // （首屏不空、口径在注释里），下一次身份变化再试。网关不可达时
        // error/alert 由轮询的统一判定负责，这里不重复报。
        return
      }
    }
    // 页序反转成「最旧页在前」，拼接后整体时间升序（dedupe 保持插入序）。
    pages.reverse()
    fullTradeRows.value = dedupeTrades(pages.flat())
  }

  /**
   * 深路径消费入口：全史未到（或上次拉取失败）时回落快路径——首屏不空，
   * 但代价是 200 行窗外的旧成交暂缺；快照快路径仍每拍刷新，全史到位后
   * 以它为底、快照新行（窗口内）随时合流。合流后 dedupe：重复行取最后一次
   * 写入（快照那份更新），插入位置不变 → 整体仍时间升序，权益曲线不抖。
   */
  const fullHistoryRows = computed<TradeRow[]>(() =>
    fullTradeRows.value ? dedupeTrades([...fullTradeRows.value, ...tradeRows.value]) : tradeRows.value,
  )

  // 身份变化 → 全史重拉。首次采纳（'' → default）也算：那是第一次有归属
  // 可言。登出（token 清空）由 hasToken 门与在途仲裁兜住。
  watch([displayAccount, activeAccountVenue], () => {
    if (!hasToken()) return
    fullTradeRows.value = null
    void loadFullTrades()
  })

  /**
   * PR H (账户筛选修复): the account-scoped aggregates, refreshed beside the
   * snapshot on every poll. `activeAccount` is this panel's session default
   * (AccountSwitcher mirrors it), so the reads and the snapshot describe the
   * SAME account; `null`/'' active = global headline and both stay empty so
   * pages fall back to the snapshot values (老面板行为不变).
   */
  const accountTradeSummary = ref<CumulativeTradeSummary | null>(null)
  const accountStrategyStats = ref<StrategyStatsRow[]>([])

  /**
   * 切户残留窗口（issue 552 审计）：成功切换默认账户后、下一次 refresh 落
   * 地前，账户级读数（trade summary / strategy stats 切片）还是旧账户的。
   * 置空让页面回落全局面读数（单账户部署与老面板的既有行为），而不是把
   * 旧账户的数字渲染成新账户的。snapshot 全局块不清 —— 它本来就无归属。
   */
  function clearAccountScoped(): void {
    accountTradeSummary.value = null
    accountStrategyStats.value = []
  }

  // Prefer engine snapshot strategyStats (richer); fall back to /api/plugins.
  // PR H (账户筛选修复): when an account is active, the counters come from
  // the account-scoped engine.stats read (`accountStrategyStats`) — the same
  // per-NAME rows with THIS account's counter slice. The row SET never
  // shrinks (the kernel folds the full registry either way), and pages must
  // not filter it again (账户↔venue 解耦裁决: 策略列表恒显全部已安装).
  // The merge keeps the mode/compat verdicts from the registry either way.
  const strategyRows = computed<StrategyStatsRow[]>(() => {
    const acc = accountStrategyStats.value
    const st = acc.length ? acc : snapshot.value?.strategyStats
    const fromStats = (snapshot.value as unknown as { stats?: { strategies?: StrategyStatsRow[] } } | undefined)?.stats?.strategies
    const plug = plugins.value?.strategies
    if ((st && st.length) || (fromStats && fromStats.length)) {
      const rows = st && st.length ? st : fromStats ?? []
      // E27 (§8.3): engine.stats rows predate the mode handshake and carry no
      // compat verdict; the registry rows (`strategy.list`) now do. Merge by
      // name so the table shows one consistent view regardless of which path
      // provided the counters.
      if (plug?.length) {
        const reg = new Map(plug.map((r) => [r.name, r]))
        return rows.map((row) => {
          const match = reg.get(row.name)
          if (!match) return row
          return {
            ...row,
            modes: match.modes ?? null,
            compatible: match.compatible ?? true,
            incompatibleReason: match.incompatibleReason ?? null,
            // 启停归属（账户↔venue 解耦合同）：registry 行带 accounts
            // 时把它带过 —— 策略页的「N 个账户启用中」徽标与账户级启停
            // 降级判定都读它。engine.stats 的 fold 行自己也带 accounts，
            // registry 缺席时保留 stats 行的。
            accounts: (match as { accounts?: string[] }).accounts ?? row.accounts,
          }
        })
      }
      return rows
    }
    if (plug) {
      // Registry-only rows: no counters and no provenance. `strategySource`
      // reports the source as unknown rather than defaulting to `builtin` —
      // `kind` is a market class, not provenance, and since PR-B the kernel
      // ships zero builtins, so that default mislabelled every external
      // strategy as in-tree. See `lib/strategy-source.ts`.
      return registryStrategyRows(plug)
    }
    return []
  })

  /**
   * Act on one poll's outcome. A 401 discards the token here rather than in the
   * shell, so no page can act on a session that has already been refused.
   * `sessionExpired` then drives the login fallback; see `lib/session.ts` for
   * why a dead session is kept distinct from an unreachable gateway.
   */
  function noteOutcome(results: PromiseSettledResult<unknown>[]): void {
    const verdict = classifyOutcome(results)
    if (verdict.kind === 'expired') {
      clearToken()
      sessionExpired.value = true
      error.value = null
      return
    }
    error.value = verdict.kind === 'unreachable' ? verdict.message : null
  }

  /**
   * 重拉账户列表（`accounts` 命令，与 AccountSwitcher.refresh 同一面）并把
   * 行镜像进 store。**启用按钮的首选读数就在这里**（account.list 行的
   * `strategies` 集合）：启停写回（strategy.enable / account.strategies.set）
   * 只动内核与这行镜像 —— 不重拉它，按钮翻转后读的仍是内存旧集，要等
   * 手动刷新才见变化（stale-readout 缺陷，2026-10 操作者报告）。
   *
   * 失败不抛：账户列表读不到时保留现有行（比清空好 —— 旧集只是陈旧，
   * 清空会把按钮读数打到降级面）；返回错误文案（null = 成功），供
   * AccountSwitcher 在自己的浮层里 inline 展示 —— 行投影只此一份，切换
   * 器不再复制投影（issue 781）。
   */
  async function refreshAccounts(): Promise<string | null> {
    // issue 781：账户簿读数的请求序号 + 身份世代。只有最新一次读数、且
    // 显示身份未变时才允许镜像写回；旧响应静默丢弃（不清行、不报错）。
    const requestId = ++accountsSeq
    const epoch = displayEpoch
    try {
      const out = await api.command('accounts')
      if (requestId !== accountsSeq || epoch !== displayEpoch) return null
      if (!out.ok) return out.message ?? '账户列表不可用'
      const doc = out.data as {
        active?: string
        displayAccount?: string
        accounts?: Array<{
          id: string
          name: string
          marketType: string
          status: string
          statusReason?: string
          balance: string | number
          available: string | number
          credentialsLoaded?: boolean
          openPositions?: number
          tradingEnabled?: boolean
          venue?: string
          strategies?: unknown
          rounds?: unknown
          reserved?: string
          seededTotal?: string | number
        }>
      } | undefined
      if (!doc) return '网关未返回账户数据'
      if (!Array.isArray(doc.accounts) || !doc.accounts.length) return null
      const rows: PanelAccountRow[] = doc.accounts.map((a) => ({
        id: a.id,
        name: a.name,
        marketType: a.marketType,
        status: a.status,
        statusReason: a.statusReason,
        balance: a.balance,
        available: a.available,
        credentialsLoaded: a.credentialsLoaded === true,
        openPositions: typeof a.openPositions === 'number' ? a.openPositions : 0,
        tradingEnabled: a.tradingEnabled,
        venue: a.venue ?? '',
        strategies: Array.isArray(a.strategies) ? [...(a.strategies as string[])] : undefined,
        rounds: Array.isArray(a.rounds) ? [...(a.rounds as number[])] : undefined,
        reserved: a.reserved ?? '',
        seededTotal: a.seededTotal,
      }))
      // issue 561：镜像只供行数据与显示兜底 —— display 账户优先读快照
      // （内核事实），报文的 displayAccount 在场也采纳（快照未到的一拍）；
      // 旧报文的 `active`（会话格语义）不再作为显示源。
      const mirrorDisplay =
        typeof doc.displayAccount === 'string' && doc.displayAccount
          ? doc.displayAccount
          : displayAccount.value
      setAccounts(mirrorDisplay ?? '', rows, mirrorDisplay || undefined)
      return null
    } catch (e) {
      // 失败不抛：保留现有行（旧集只是陈旧，清空会把按钮读数打到降级面）。
      // 401 仍走统一会话判定（classifyOutcome）：账号簿读数的失效与轮询
      // 读数一样，把面板送回登录面而不是停在错误提示上。
      const verdict = classifyOutcome([{ status: 'rejected', reason: e }])
      if (verdict.kind === 'expired') {
        clearToken()
        sessionExpired.value = true
        return '未登录或会话已失效，请重新登录。'
      }
      return e instanceof Error ? e.message : String(e)
    }
  }

  /**
   * 轮次周期集写（`account.rounds.set`，轮次多族 B2）：把该账户的轮长族
   * 声明交给内核（空数组 = 不限制）。成功后 refreshAccounts 重拉账户簿
   * （AccountView.rounds 随行带回，卡面读数即新值）；失败抛出 —— 内核的
   * 拒绝（如移除有敞口的族，消息点名族列表）与老内核的 unknown-method 都
   * 由调用方原样透出，这里不吞、不预判敞口。
   */
  async function setAccountRounds(id: string, rounds: number[]): Promise<void> {
    const out = await api.accountRoundsSet({ accountId: id, rounds, reason: 'panel rounds card' })
    if (out.error) throw new Error(out.error)
    await refreshAccounts()
  }

  async function refresh(): Promise<void> {
    if (!hasToken()) return
    // issue 781：每拍轮询有单调序号 + 请求时刻的 (display 账户, venue) 身份
    // 捕获。15s / 2s 等多个计时器并发时，只有「最新一拍且身份仍匹配」的
    // 响应可以写回 —— 切户/切所后迟到的旧账户快照静默丢弃，不再覆盖新
    // 账户的读数。身份只用账户 id + venue，不用时间戳（易变字段会让去重
    // 永远失配）；序号活在 store 闭包里，重绘不重置。
    const requestId = ++refreshSeq
    const epoch = displayEpoch
    const identityAccount = displayAccount.value
    const identityVenue = activeAccountVenue.value
    // 空 display 账户 = 尚未知道身份：这一拍发起的是全局读数（acct
    // undefined），首拍被账户簿镜像补充上默认账户后仍可落地 —— 否则首屏
    // 快照会被自家镜像作废，空等下一拍（15s）。已知身份则必须逐字匹配。
    const identityKnown = identityAccount !== ''
    const isCurrent = (): boolean =>
      requestId === refreshSeq &&
      epoch === displayEpoch &&
      (!identityKnown || identityAccount === displayAccount.value) &&
      (!identityKnown || identityVenue === activeAccountVenue.value)
    loading.value = true
    try {
      // PR H: the account-scoped aggregates ride their own proxy routes
      // beside the snapshot. `acct` absent = 全局面板 —— 两条读数置空，页面
      // 回落 snapshot 值（单账户部署与老面板逐字节一致）。
      // issue 561: `acct` 跟随 display 账户（唯一可信源）——聚合读数与页
      // 面过滤同源，不再有「Header 一样、数字两套」的双节拍。
      const acct = identityAccount || undefined
      const [snap, plug, accSummary, accStats, acctList] = await Promise.allSettled([
        api.snapshot(),
        api.plugins(),
        api.tradesSummary({ accountId: acct }),
        api.engineStatsFor({ accountId: acct }),
        refreshAccounts(),
      ])
      // 本拍的写回裁决只做一次：这是单线程同步段，裁决后不会被别的响应
      // 插入。内部写身份字段（快照带回新 display）也不会把本拍自己的其余
      // 写回误判成陈旧。
      const current = isCurrent()
      if (snap.status === 'fulfilled' && current) {
        snapshot.value = snap.value
        // issue 561: 快照顶层的 displayAccount/displayVenue 是内核的账户
        // 事实 —— 轮询每拍镜像（乐观写在两拍之间由 setDisplayAccount 维
        // 持，快照到达后以内核读数为准）。缺字段（老内核）保留现值。
        const snapDisplay = snap.value.displayAccount
        if (typeof snapDisplay === 'string' && snapDisplay) {
          const nextVenue =
            snap.value.displayVenue ??
            accounts.value.find((a) => a.id === snapDisplay)?.venue ??
            ''
          // 已知身份被内核改掉（或 venue 改绑）同样要作废在途的其它批
          // 处理；首次从空身份采纳不算切换，不推进世代（同 setAccounts）。
          if (displayAccount.value &&
            (snapDisplay !== displayAccount.value || nextVenue !== displayVenue.value)) {
            displayEpoch++
          }
          displayAccount.value = snapDisplay
          displayVenue.value = nextVenue
        }
        const progress = feedProgress(snap.value.stats?.books, snap.value.stats?.tops)
        if (progress !== lastProgress) {
          lastProgress = progress
          feedAt.value = Date.now()
        }
      }
      if (plug.status === 'fulfilled' && current) plugins.value = plug.value
      if (current) {
        accountTradeSummary.value =
          acct && accSummary.status === 'fulfilled' ? accSummary.value.summary ?? null : null
        accountStrategyStats.value =
          acct && accStats.status === 'fulfilled' ? accStats.value.strategies ?? [] : []
        noteOutcome([snap, plug, accSummary, accStats])
        lastUpdated.value = Date.now()
      }
      void acctList
    } catch (e) {
      if (isCurrent()) error.value = e instanceof Error ? e.message : String(e)
    } finally {
      loading.value = false
    }
  }

  /** Called by the shell once the operator has seen the expiry and logged in. */
  function acknowledgeSessionReset(): void {
    sessionExpired.value = false
  }

  /**
   * #838: write one SSE-delivered snapshot document into the store. The
   * gateway's push stream carries the exact `/api/snapshot` document, so the
   * writeback mirrors what `refresh()` does with its polled snapshot — same
   * display-account mirror, same feed-progress liveness stamp. Scope is
   * deliberately snapshot-only: the account-scoped aggregates keep their own
   * poll cadence (they are request-time folds, not kernel state), and pages
   * that already run their own fast tick keep doing so. `tradeRows` is a
   * computed over the snapshot, so banners (lastError) and order terminal
   * states refresh with no extra code — the two event-shaped readings the
   * stream exists for.
   *
   * issue 1199 D10：写回也要过 `displayEpoch` 仲裁 —— 与 refresh() 的
   * isCurrent（issue 781）同一机制，此前这条路径完全绕开它。网关在处理
   * `display.account.set` **之前**构建的帧，可能在操作者乐观写
   * （setDisplayAccount，世代自增）**之后**才送达：直接写回会把旧账户的读数
   * 闪回新身份之上，下方的 display 镜像甚至会把乐观身份回退成旧账户（账户
   * 切换回闪）。流没有「请求时刻」可捕获世代，捕获点取上一帧**被接受**时的
   * 世代（sseEpoch）：仅在「自上一被接受帧后发生过切换 且 该帧仍说着旧身份」
   * 时丢弃。已携带新身份的帧、以及无切换在途时的一切帧（含内核侧改显示账户
   * 的采纳路径）照常通过；被丢的帧由下一个 1s tick 或轮询自愈。老内核缺
   * display 字段（frameDisplay 空）无从判定 —— 放行，与全站 feature-detect
   * 降级同一规则。
   */
  let sseEpoch = 0
  function applySseSnapshot(doc: Record<string, unknown>): void {
    if (!hasToken()) return
    const snap = doc as unknown as Snapshot
    // Epoch arbitration (issue 1199 D10, mirrors refresh()'s isCurrent): an
    // overtaken frame that still speaks the pre-switch account must not
    // rewrite `snapshot` or the display mirror below.
    const frameDisplay = typeof snap.displayAccount === 'string' ? snap.displayAccount : ''
    if (sseEpoch !== displayEpoch && frameDisplay && frameDisplay !== displayAccount.value) return
    sseEpoch = displayEpoch
    snapshot.value = snap
    // issue 561 mirror, verbatim from refresh(): the snapshot's top-level
    // displayAccount/displayVenue are the kernel's account facts; a mismatch
    // invalidates in-flight writes by advancing the epoch.
    const snapDisplay = snap.displayAccount
    if (typeof snapDisplay === 'string' && snapDisplay) {
      const nextVenue =
        snap.displayVenue ??
        accounts.value.find((a) => a.id === snapDisplay)?.venue ??
        ''
      if (displayAccount.value &&
        (snapDisplay !== displayAccount.value || nextVenue !== displayVenue.value)) {
        displayEpoch++
      }
      displayAccount.value = snapDisplay
      displayVenue.value = nextVenue
      // The adoption above advanced the epoch — re-sync the SSE capture point
      // so the invariant「每次被接受的帧结束时 sseEpoch === displayEpoch」holds
      // and the next frame is not misjudged as overtaken by this very bump.
      sseEpoch = displayEpoch
    }
    const progress = feedProgress(snap.stats?.books, snap.stats?.tops)
    if (progress !== lastProgress) {
      lastProgress = progress
      feedAt.value = Date.now()
    }
    lastUpdated.value = Date.now()
  }

  return {
    snapshot, plugins, error, loading, lastUpdated,
    connected, cumulativeNetPnl, strategyRows, tradeRows, fullHistoryRows, feedAt, sessionExpired,
    navRequest, blueprintPreload,
    activeAccount, accounts, setAccounts, setDisplayAccount, clearAccountScoped, refreshAccounts, activeAccountVenue, activeAccountRow, setAccountRounds,
    displayAccount, displayVenue,
    accountTradeSummary, accountStrategyStats,
    refresh, acknowledgeSessionReset, applySseSnapshot,
    requestNav, clearNavRequest, requestBlueprintPreload, takeBlueprintPreload,
  }
})
