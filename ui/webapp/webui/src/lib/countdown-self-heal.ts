/**
 * Countdown-card helpers: the progress bar's denominator, and the 00:00
 * self-heal policy.
 *
 * The bar must follow the ACCOUNT's anchored family, never the process
 * default 15m (effectiveSec / the global `round` scalar, which is the
 * earliest-expiring clock and is usually 900). Daily windows are 23h or
 * 25h on DST days, so the actual venueRound age+left span wins when it
 * is present; otherwise the account's identityDurSec / engineActiveSec.
 *
 * When the digits sit at 00:00 for a few seconds and the round id has
 * not advanced, the page refetches /api/snapshot and reconnects the
 * live SSE stream. It never invents a next-round clock (L1–L6) and
 * never calls location.reload (that would loop). The attempt is shown
 * in the UI. Backoff is bounded; after HEAL_MAX_ATTEMPTS it stops and
 * asks the operator to refresh once.
 */

/** 00:00 may sit this long without a new round id before a self-heal. */
export const ZERO_STALL_MS = 4_000

/** Delay after the 1st, 2nd, 3rd, … successful trigger, before the next. */
export const HEAL_BACKOFF_MS = [4_000, 8_000, 16_000, 32_000] as const

export const HEAL_MAX_ATTEMPTS = 5

export type HealAction = 'idle' | 'arm' | 'wait' | 'heal' | 'give-up'

export interface HealState {
  stalledSinceMs: number | null
  attempts: number
  lastHealAtMs: number | null
}

export function emptyHealState(): HealState {
  return { stalledSinceMs: null, attempts: 0, lastHealAtMs: null }
}

/**
 * Remaining-time fraction for the countdown hairline.
 * `familyAgeSec + familyLeftSec` is the observed window (daily DST 23h/25h
 * included). When that span is missing, `identityDurSec` (the account
 * anchor: 300/900/3600/86400) is the denominator. The global 15m scalar
 * is never consulted.
 */
export function countdownBarFraction(opts: {
  leftSec: number
  familyAgeSec?: number | null
  familyLeftSec?: number | null
  identityDurSec?: number | null
}): number {
  const age = opts.familyAgeSec
  const famLeft = opts.familyLeftSec
  const observed =
    typeof age === 'number' && typeof famLeft === 'number' && age + famLeft > 0
      ? age + famLeft
      : 0
  const anchored =
    typeof opts.identityDurSec === 'number' && opts.identityDurSec > 0
      ? opts.identityDurSec
      : 0
  const span = observed > 0 ? observed : anchored
  if (!span) return 0
  return Math.max(0, Math.min(1, opts.leftSec / span))
}

export function stepCountdownSelfHeal(input: {
  leftSec: number
  frozen: boolean
  nowMs: number
  state: HealState
}): { action: HealAction; state: HealState; message: string | null } {
  if (input.frozen || input.leftSec > 0) {
    return { action: 'idle', state: emptyHealState(), message: null }
  }
  let { stalledSinceMs, attempts, lastHealAtMs } = input.state
  if (stalledSinceMs == null) {
    return {
      action: 'arm',
      state: { stalledSinceMs: input.nowMs, attempts, lastHealAtMs },
      message: null,
    }
  }
  if (input.nowMs - stalledSinceMs < ZERO_STALL_MS) {
    return {
      action: 'wait',
      state: { stalledSinceMs, attempts, lastHealAtMs },
      message: null,
    }
  }
  if (attempts >= HEAL_MAX_ATTEMPTS) {
    return {
      action: 'give-up',
      state: { stalledSinceMs, attempts, lastHealAtMs },
      message: '倒计时停在 00:00 且轮次号未前进，自动重连已停止。请手动刷新页面。不会编造下一轮的时间。',
    }
  }
  if (lastHealAtMs != null) {
    const backoff = HEAL_BACKOFF_MS[Math.min(attempts - 1, HEAL_BACKOFF_MS.length - 1)]
    if (input.nowMs - lastHealAtMs < backoff) {
      return {
        action: 'wait',
        state: { stalledSinceMs, attempts, lastHealAtMs },
        message: `倒计时停在 00:00 且轮次号未前进，等待重试（已尝试 ${attempts} 次）。`,
      }
    }
  }
  const next = attempts + 1
  return {
    action: 'heal',
    state: { stalledSinceMs, attempts: next, lastHealAtMs: input.nowMs },
    message: `倒计时停在 00:00 且轮次号未前进，正在重新拉取快照并重连实时流（第 ${next} 次）。不会编造倒计时。`,
  }
}
