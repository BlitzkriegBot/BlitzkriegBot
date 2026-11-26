/**
 * Smart Money Radar & Reverse-to-Strategy API proxies.
 */
import { request } from './core.ts'

export type StrategyArchetype =
  | 'late_favorite'
  | 'pair_arb'
  | 'mid_round_momentum'
  | 'cheap_option_hunter'
  | 'unclassifiable'

export interface RadarScanBody {
  market?: '5m' | '15m'
  minBuyFills?: number
  limit?: number
  timePeriod?: 'DAY' | 'WEEK' | 'MONTH'
}

export interface RadarCandidateDoc {
  wallet: string
  label: string
  leaderboardPnlUsd: number
  leaderboardVolUsd: number
  archetype: StrategyArchetype
  reason: string
  buyFills: number
  uniqueConditions: number
  assets: string[]
  durationsSec: number[]
  entryPxP25: number
  entryPxP50: number
  entryPxP90: number
  timeLeftP10: number
  timeLeftP50: number
  timeLeftP90: number
  pairRatio: number
  holdToSettlementRatio: number
  avgNotionalUsd: number
  blueprint?: Record<string, unknown> | null
}

export interface RadarScanDoc {
  scannedWallets: number
  qualifiedCount: number
  candidates: RadarCandidateDoc[]
  error?: string
}

export interface WalletFingerprintDoc {
  wallet: string
  totalFills: number
  buyFills: number
  sellFills: number
  uniqueConditions: number
  assets: string[]
  durationsSec: number[]
  entryPxMin: number
  entryPxP10: number
  entryPxP25: number
  entryPxP50: number
  entryPxP75: number
  entryPxP90: number
  entryPxMax: number
  timeLeftP10: number
  timeLeftP50: number
  timeLeftP90: number
  pairRatio: number
  holdToSettlementRatio: number
  avgNotionalUsd: number
  resolvedRounds: number
  wins: number
  winRatePct: number
  netPnlUsd: number
  profitFactor?: number | null
}

export interface SmartMoneyAnalyzeDoc {
  fingerprint: WalletFingerprintDoc
  archetype: StrategyArchetype
  reason: string
  blueprint?: Record<string, unknown> | null
  error?: string
}

export const smartMoneyApi = {
  smartMoneyScan: (body: RadarScanBody) =>
    request<RadarScanDoc>('/smart-money/scan', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  smartMoneyAnalyze: (wallet: string, market?: '5m' | '15m') =>
    request<SmartMoneyAnalyzeDoc>('/smart-money/analyze', {
      method: 'POST',
      body: JSON.stringify(market ? { wallet, market } : { wallet }),
    }),
}
