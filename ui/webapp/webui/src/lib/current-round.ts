/**
 * The panel market section's single source of truth.
 *
 * The kernel publishes one per-account `currentRound` (durationSec from the
 * account anchor, roundId = venue slug start ts, startMs/endMs from the
 * real venue window, prices and books for that family only). The gateway
 * forwards it as-is. Countdown, round number, elapsed time, progress bar,
 * price cards and the order book all read this object — they must not
 * derive a duration or a round id of their own, and they must not fall
 * back to the process-default 900.
 */
import type { CurrentRound, Snapshot } from '@/api/snapshot'

export type { CurrentRound }

export function selectCurrentRound(
  snap: Snapshot | null | undefined,
  accountId: string | null | undefined,
): CurrentRound | null {
  if (!snap || !accountId) return null
  const rows = snap.currentRounds
  if (!rows || rows.length === 0) return null
  return rows.find((r) => r.accountId === accountId) ?? null
}

/** Observed window length in seconds (daily DST 23h/25h included). */
export function currentRoundSpanSec(r: CurrentRound | null): number {
  if (!r) return 0
  if (r.startMs > 0 && r.endMs > r.startMs) {
    return Math.round((r.endMs - r.startMs) / 1000)
  }
  const observed = (Number(r.ageSec) || 0) + (Number(r.timeLeftSec) || 0)
  if (observed > 0) return observed
  // Last resort: the account anchor the kernel published on this object,
  // never a process-default 900 the page invented.
  return typeof r.durationSec === 'number' && r.durationSec > 0 ? r.durationSec : 0
}

export function currentRoundBarFraction(
  r: CurrentRound | null,
  leftSec: number,
): number {
  const span = currentRoundSpanSec(r)
  if (!span) return 0
  return Math.max(0, Math.min(1, leftSec / span))
}

/** Remaining seconds from the published end, or 0 when the object is missing. */
export function currentRoundLeftSec(r: CurrentRound | null, nowMs: number): number {
  if (!r) return 0
  if (r.endMs > 0) return Math.max(0, Math.round((r.endMs - nowMs) / 1000))
  return Math.max(0, Math.round(Number(r.timeLeftSec) || 0))
}

/** Elapsed seconds from the published start. */
export function currentRoundAgeSec(r: CurrentRound | null, nowMs: number): number {
  if (!r) return 0
  if (r.startMs > 0) return Math.max(0, Math.round((nowMs - r.startMs) / 1000))
  return Math.max(0, Math.round(Number(r.ageSec) || 0))
}
