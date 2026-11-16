/**
 * 提示音 — 快照差分（issue 781：从 HftPage 的脚本搬出的 domain 逻辑）。
 *
 * 声音合成仍在 ./alertSounds（Web Audio，单上下文 + gain + compressor）；
 * 这里只决定「哪一次快照变化放哪一声」：
 *
 *   盈利单笔 (profit) / 亏损单笔 (dang) —— 成交行计数增长；
 *   新开仓 (order)  —— 持仓行数增长；
 *   熔断 (wuwu) / 风控恢复 (ding) —— 内核的交易冻结状态翻转；
 *   日亏熔断 (wuwu) —— dailyLoss.accounts[] 行 tripped false→true（issue 1199 D10
 *                     接上：该字段早已过 seam —— render.rs 转发 st.daily_loss、
 *                     api/snapshot.ts 有类型、设置 · 风控在消费 —— 缺的只是声源）；
 *   账户冻结 (wuwu) —— accountGates[] 新增 blocked 账户（同批接上：多账户
 *                     部署 B 账户被冻结时全局闸不动，此前全程无声）。
 *
 * 触发面是差分而不是日志重放：首帧只建立基线（重挂载/刷新不会补放历史成交
 * 的声音风暴），此后每类每帧最多三声（批量成交刷屏的上限）。
 *
 * issue 1199 D10：唯一挂载点是 App.vue 壳层（原 HftPage —— 无 KeepAlive，切页即
 * 卸载，总览/设置页全程无声）。
 */
import { watch, type ComputedRef } from 'vue'
import type { Snapshot, TradeRow } from '@/api/client'
import { isKnownPnl } from '@/lib/trades'
import { playDang, playDing, playOrder, playProfit, playWuwu } from './alertSounds'

/**
 * @param snap      the live snapshot (the shell's store mirror)
 * @param tradeRows the deduped closed-trade rows, read at callback time
 */
export function useAlertSounds(
  snap: ComputedRef<Snapshot | null>,
  tradeRows: () => TradeRow[],
): void {
  let primedWins: number | null = null
  let primedLosses: number | null = null
  let primedPositions: number | null = null
  let wasHalted = false
  // Per-account risk baselines (issue 1199 D10): the account ids already tripped
  // (daily-loss breaker) / blocked (entry gate) in the last seen frame. Primed
  // on the first frame together with the trade counters — an account frozen
  // before this composable mounted must not replay its freeze on the second
  // frame (same no-replay-storm rule as the counters below).
  let trippedAccounts = new Set<string>()
  let blockedAccounts = new Set<string>()

  watch(snap, (s) => {
    if (!s) return
    // Deduped: the raw rows can carry the same `hft-N` id from an earlier run, and
    // counting those twice would fire an alert sound for a trade that never closed.
    const rows = tradeRows()
    // 未知 netPnlUsd（结算未核实）既不胜也不负：折叠成 0 会把「未知结果」误判成亏损，
    // 进而触发 playDang()。只对 isKnownPnl 的行做胜负计数。
    const wins = rows.filter((t) => isKnownPnl(t) && Number(t.netPnlUsd) > 0).length
    const losses = rows.filter((t) => isKnownPnl(t) && Number(t.netPnlUsd) <= 0).length
    const pos = s.positions?.length ?? 0

    // Fold the two per-account risk books (issue 1199 D10). Absent fields (older
    // cores) fold to empty sets — absence never invents a freeze, same rule as
    // the banner layer (lib/safety.ts).
    const dailyTrippedNow = new Set<string>()
    for (const row of s.stats?.dailyLoss?.accounts ?? []) {
      if (row?.account && row.tripped === true) dailyTrippedNow.add(row.account)
    }
    const gatesBlockedNow = new Set<string>()
    for (const gate of s.stats?.accountGates ?? []) {
      if (gate?.account && gate.blocked === true) gatesBlockedNow.add(gate.account)
    }

    // first frame only establishes the baseline (no replay storm on reload)
    if (primedWins === null || primedLosses === null || primedPositions === null) {
      primedWins = wins; primedLosses = losses; primedPositions = pos
      trippedAccounts = dailyTrippedNow
      blockedAccounts = gatesBlockedNow
      return
    }
    if (wins > primedWins) for (let i = 0; i < Math.min(wins - primedWins, 3); i++) playProfit()
    if (losses > primedLosses) for (let i = 0; i < Math.min(losses - primedLosses, 3); i++) playDang()
    if (pos > primedPositions) playOrder()
    primedWins = wins; primedLosses = losses; primedPositions = pos

    // Trading halted: the sound's source is the kernel's freeze state
    // (`stats.tradingFrozen`), not `snapshot.lastError` — that one is the
    // GATEWAY's "core not reachable" string, which never contains "breaker", so
    // this pair of sounds could never fire (issue 236 shook this loose). Since
    // issue 1199 D10 the per-account counterparts below are wired from the same
    // stats blob: the daily-loss breaker (`dailyLoss.accounts[].tripped` —
    // carried through the seam long since; Settings · Risk consumes the same
    // rows) and the entry gates (`accountGates[].blocked`).
    const halted = s.stats?.tradingFrozen?.active === true
    if (halted && !wasHalted) playWuwu()
    if (!halted && wasHalted) playDing()
    wasHalted = halted

    // Per-account warning (issue 1199 D10): one breaker sound per NEWLY tripped /
    // blocked account, capped like the fill sounds. The kernel mirrors a
    // daily-loss trip into the gate book (daily_loss_gate.rs set_account_gate →
    // GateReason::DailyLoss), so one trip lands in BOTH sets on the same frame —
    // sound once per account (union), not once per book. Un-trip / unblock
    // stays silent: the recovery is a reading (banner / 设置 · 风控), not a
    // second sound here.
    let sounded = 0
    for (const id of dailyTrippedNow) {
      if (sounded >= 3) break
      if (trippedAccounts.has(id) || blockedAccounts.has(id)) continue
      playWuwu(); sounded++
    }
    for (const id of gatesBlockedNow) {
      if (sounded >= 3) break
      if (blockedAccounts.has(id) || dailyTrippedNow.has(id)) continue
      playWuwu(); sounded++
    }
    trippedAccounts = dailyTrippedNow
    blockedAccounts = gatesBlockedNow
  })
}
