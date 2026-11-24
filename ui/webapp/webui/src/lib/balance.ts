/**
 * The balance the panel presents — a pass-through view of the account's own book.
 *
 * ## What the panel may compute, and what it may not
 *
 * The ledger is the cash truth (操作员裁决 2026-11-08：账本流水=唯一真相). The
 * kernel already publishes the one net-profit number the panel is allowed to
 * show: `cumulativeNetPnl` = `balance − opening_equity` (issue #1046/#1063,
 * `schema/views.rs`: 「净利的 SSOT，面板不得再按 cash − seed 另算一套」).
 * This module therefore **passes ledger fields through unchanged**:
 *
 *   - `net`   ← `cumulativeNetPnl`, verbatim. Null = unknown = render 「—」.
 *   - `cash` / `reserved` / `available` ← the account's own ledger, verbatim.
 *
 * ## Why `equity` is null instead of a computed number (issue #179)
 *
 * The panel used to compose `equity = seed + net`, where `seed` was
 * `seededTotal` (累计 Seed 入金). That is not the equity identity: the kernel
 * measures net profit against **`opening_equity`**, not against the cumulative
 * seed total, so whenever the two differ the sum is simply a wrong number —
 * arithmetically, not just philosophically. And `AccountView` carries **no
 * `equity` and no `opening_equity` field at all** (only `seeded_total` +
 * `cumulative_net_pnl`), so the panel cannot obtain the real equity either.
 *
 * R7.1 L5 forbids papering over that gap: the UI must not present a fallback
 * figure as if it were data. `equity` is therefore `null` — the one honest
 * value — and every consumer renders 「—」 until the backend ships the field
 * (tracked separately as the `equity` / `opening_equity` addition to
 * `AccountView`). This module will pass those fields through when they exist;
 * it must never re-derive them.
 *
 * ## `gross` — trade-fold basis, deliberately kept
 *
 * `gross = net + fees` adds two figures that both genuinely exist on the wire
 * (`cumulativeNetPnl` and the folded fee total). Fees are an expense already
 * deducted from net, so adding them back recovers the pre-cost figure — that is
 * a definition, not a fabricated balance. It is **not** a ledger number and is
 * never displayed as one: callers label it 「成交记录口径」. The moment the
 * backend publishes `gross_pnl`, this arithmetic is deleted in favour of the
 * field (see issue #179).
 *
 * ## Why the core's cash ledger is never presented as equity
 *
 * The core's `Ledger` is a real control — `reserve()` refuses a BUY whose
 * notional exceeds available collateral — and LIVE seeds it from the venue
 * (`extensions/polymarket/src/live.rs`). In DRY it is seeded from
 * `--seed-balance` at every boot (`service.rs`, `ipc/server.rs`) and then moves
 * only on the fills of *that* run, so its value is always `本金 ＋ 本次会话净利`:
 * a strict subset of what the trade log already records, and one that forgets
 * every earlier run.
 *
 * Measured on this book (2026-09-16): cash 1002.06 while the log's all-time net
 * was 48.21, so the −46.15 difference was nothing more than the profit earned
 * before the boot. Printing that cash figure under an equity label would have
 * reported the session's seed plus this run's fills as the account's worth.
 */
import type { Snapshot } from '@/api/client'
import { money } from './format.ts'

export interface BalanceView {
  /**
   * Cumulative net profit, ledger basis (`cumulativeNetPnl` — the SSOT).
   * Null when the core did not report it (opening equity unknown): render
   * 「—」, never fold a trade sum in its place.
   */
  net: number | null
  /** Total fees paid, an expense. Already deducted from `net`, never added to it. */
  fees: number
  /**
   * Pre-cost profit on the **trade-fold** basis: `net + fees`. Null when `net`
   * is null. Not a ledger figure, and never rendered as a balance.
   */
  gross: number | null
  /**
   * Account equity — **always null today**: `AccountView` carries neither
   * `equity` nor `opening_equity`, and `seed + net` is not the equity identity
   * (net is measured against `opening_equity`, not `seededTotal`). Consumers
   * must render 「—」. The field survives so the shape is stable for the
   * backend addition (#181) — at which point it is passed through verbatim.
   */
  equity: number | null
  /** The core's cash ledger, gross of local reservations. Never a stand-in for `equity`. */
  cash: number
  /** Cash tied up in resting buy orders. */
  reserved: number
  /** Free cash — what the engine can actually commit. */
  available: number
}

export function balanceView(
  balance: Snapshot['balance'] | null | undefined,
  net: number | null,
  fees: number,
): BalanceView {
  const cash = Number(balance?.balance ?? 0)
  const reserved = Number(balance?.reserved ?? 0)
  const available = Number(balance?.available ?? 0)
  const realized = net === null || net === undefined ? null : Number(net) || 0
  const paidFees = Number(fees) || 0

  return {
    net: realized,
    fees: paidFees,
    // Two real inputs on the trade-fold basis (fees are an expense already
    // deducted from net); explicitly labelled 成交口径 wherever it is rendered.
    gross: realized === null ? null : realized + paidFees,
    // No equity on the wire → no equity to show. See the header: composing
    // `seed + net` is the wrong identity (issue #179, R7.1 L5).
    equity: null,
    cash,
    reserved,
    available,
  }
}

/**
 * The equity headline, in one place so both the overview tile and the market
 * page's stat row render the same thing.
 *
 * `equity === null` means **unknown**, and unknown renders 「—」 — never the
 * cash ledger under an equity label (issue #179, R7.1 L5). Centralised because
 * the previous two call sites each carried their own `: money(cash)` fallback.
 */
export function equityHeadline(equity: number | null): string {
  return equity === null ? '—' : money(equity)
}
