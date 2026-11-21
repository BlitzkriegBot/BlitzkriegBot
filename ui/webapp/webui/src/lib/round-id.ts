/**
 * Venue round identifiers — one number for the panel and the ledger.
 *
 * Polymarket names a round by its start Unix timestamp
 * (`btc-updown-5m-1795230000`). The scanner's historical `slot` is the
 * index `start / duration`. Display and expiry use this helper so both
 * identifier forms agree on the same end instant.
 *
 * Values at or above 1e9 are start timestamps; smaller values are legacy
 * slot indices (current 5m indices are ~6e6). A tiny index is left as an
 * index: its product would stay below the floor and become ambiguous.
 */
export const VENUE_TS_FLOOR = 1_000_000_000

export function isLegacySlotIndex(id: number): boolean {
  return id < VENUE_TS_FLOOR
}

export function venueRoundId(slot: number, durationSec: number): number {
  if (slot <= 0 || durationSec <= 0) return slot
  if (!isLegacySlotIndex(slot)) return slot
  const ts = slot * durationSec
  return ts < VENUE_TS_FLOOR ? slot : ts
}

export function roundEndSec(id: number, durationSec: number): number {
  if (id <= 0 || durationSec <= 0) return 0
  return isLegacySlotIndex(id) ? (id + 1) * durationSec : id + durationSec
}

export function durationLabel(durationSec: number): string | null {
  switch (durationSec) {
    case 300: return '5m'
    case 900: return '15m'
    case 3600: return '1h'
    case 14_400: return '4h'
    case 86_400: return '1d'
    default: return null
  }
}

/** Absolute Polymarket event URL, or null when the duration/id/asset is unknown. */
export function exchangeUpdownUrl(
  asset: string,
  durationSec: number,
  roundId: number,
): string | null {
  const label = durationLabel(durationSec)
  const id = venueRoundId(roundId, durationSec)
  const name = asset.trim().toLowerCase()
  if (!label || id < VENUE_TS_FLOOR || !name) return null
  return `https://polymarket.com/zh/event/${name}-updown-${label}-${id}`
}
