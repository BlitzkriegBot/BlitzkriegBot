/** Display formatters — one place so every page renders numbers identically. */

export function num(v: unknown, fallback = '—'): string {
  if (v === undefined || v === null || v === '') return fallback
  const n = Number(v)
  return Number.isFinite(n) ? n.toLocaleString() : String(v)
}

/** Compact large counts: 148,913,828 → 148.9M */
export function compact(v: unknown, fallback = '—'): string {
  const n = Number(v)
  if (!Number.isFinite(n)) return fallback
  const abs = Math.abs(n)
  if (abs >= 1e9) return `${(n / 1e9).toFixed(2)}B`
  if (abs >= 1e6) return `${(n / 1e6).toFixed(2)}M`
  if (abs >= 1e4) return `${(n / 1e3).toFixed(1)}K`
  return n.toLocaleString()
}

export function money(v: unknown, digits = 2, fallback = '—'): string {
  if (v === undefined || v === null || v === '') return fallback
  const n = Number(v)
  if (!Number.isFinite(n)) return fallback
  return `$${n.toLocaleString(undefined, { minimumFractionDigits: digits, maximumFractionDigits: digits })}`
}

/** Signed money for PnL: +$12.30 / −$4.10 */
export function signedMoney(v: unknown, digits = 2, fallback = '—'): string {
  if (v === undefined || v === null || v === '') return fallback
  const n = Number(v)
  if (!Number.isFinite(n)) return fallback
  const sign = n > 0 ? '+' : n < 0 ? '−' : ''
  return `${sign}$${Math.abs(n).toLocaleString(undefined, { minimumFractionDigits: digits, maximumFractionDigits: digits })}`
}

/**
 * Core shadow-evolution proposals carry winRate as a 0..1 fraction; scale to
 * percent. **只供 shadow 链**（EvolutionPage）——trade_summary 的 raw 字段
 * 是百分数 0–100，走这里会被二次放大成 7330% 这类恶性数字。
 */
export function winRatePct(v: unknown): number {
  const n = Number(v)
  if (!Number.isFinite(n)) return 0
  return n * 100
}

/**
 * 胜率的唯一口径（操作者裁决 2026-11-14）：已知盈亏的已平仓笔数之比，
 * 0 笔 = 0.0%。全站只有这一处产出胜率 —— 页面不得各自拼百分数，也
 * 不得把「缺字段」渲染成「未知」：0 笔与缺字段落到同一个数值。
 */
export function winRateOf(wins: number, losses: number): number {
  const denom = wins + losses
  return denom > 0 ? (wins / denom) * 100 : 0
}

/**
 * 服务端 summary 的胜率 **raw 字段不再被信任**（2026-11-15 操作者实机实锤：
 * wire 上 raw 历史两套口径并存 —— trade_summary 产百分数 0–100，shadow 产
 * 小数 0–1；旧实现把百分数再 ×100，二次放大成 7330% 这种恶性数字）。本
 * 函数永远由同一 summary 自带的盈/亏计数复算（`winRateOf`，百分数 0–100），
 * 与「总览/行情页已平仓交易」卡同源同口径；raw 参数仅为签名兼容保留。
 * 0 笔 = 0.0%，绝不返回 null。
 */
export function winRateValue(raw: unknown, wins: number, losses: number): number {
  void raw
  return winRateOf(wins, losses)
}

export function pct(v: unknown, digits = 1, fallback = '—'): string {
  const n = finiteNumber(v)
  if (n === null) return fallback
  return `${n.toFixed(digits)}%`
}

export function signedPct(v: unknown, digits = 1, fallback = '—'): string {
  const n = finiteNumber(v)
  if (n === null) return fallback
  const sign = n > 0 ? '+' : n < 0 ? '−' : ''
  return `${sign}${Math.abs(n).toFixed(digits)}%`
}

/** Finite number or null. Empty string, booleans, NaN, and Infinity are not numbers. */
export function finiteNumber(v: unknown): number | null {
  if (v === undefined || v === null || typeof v === 'boolean') return null
  if (typeof v === 'string' && v.trim() === '') return null
  const n = Number(v)
  return Number.isFinite(n) ? n : null
}

/** Price on a 0..1 prediction market → cents with 1 decimal. */
export function cents(v: unknown, fallback = '—'): string {
  const n = finiteNumber(v)
  if (n === null) return fallback
  return `${(n * 100).toFixed(1)}¢`
}

/**
 * Implied UP/DOWN mid spread in cents: max(0, up + down − 1) × 100.
 * Either mid missing → null (caller renders —). Never NaN.
 */
export function binarySpreadCents(up: unknown, down: unknown): number | null {
  const u = finiteNumber(up)
  const d = finiteNumber(down)
  if (u === null || d === null) return null
  // Tenth of a cent matches formatSpreadCents / toFixed(1) and kills 0.51+0.51 fp dust.
  return Math.round(Math.max(0, u + d - 1) * 1000) / 10
}

/** Format a value already in cents, or — when not a finite number. */
export function formatSpreadCents(centsValue: unknown, fallback = '—'): string {
  const n = finiteNumber(centsValue)
  if (n === null) return fallback
  return `${n.toFixed(1)}¢`
}

/**
 * Book touch spread. If either best bid or best ask is missing, return
 * fallback — never 0.0¢ for a one-sided book (that would invent a tight market).
 * `spread` is in the same 0..1 units as bestBid / bestAsk when present.
 */
export function bookSpreadText(
  bestBid: unknown,
  bestAsk: unknown,
  spread?: unknown,
  fallback = '—',
): string {
  const bid = finiteNumber(bestBid)
  const ask = finiteNumber(bestAsk)
  if (bid === null || ask === null) return fallback
  const s = spread === undefined || spread === null || spread === ''
    ? ask - bid
    : finiteNumber(spread)
  if (s === null) return fallback
  return cents(s)
}

/**
 * One-sided (or two-touch) quote when a mid is unreliable.
 * Never fabricates a mid (L1). Missing book → fallback.
 */
export function oneSidedQuoteText(
  bestBid: unknown,
  bestAsk: unknown,
  fallback = '—',
): string {
  const bid = finiteNumber(bestBid)
  const ask = finiteNumber(bestAsk)
  if (bid !== null && ask !== null) return `买一 ${cents(bid)} · 卖一 ${cents(ask)}`
  if (ask !== null) return `卖一 ${cents(ask)}`
  if (bid !== null) return `买一 ${cents(bid)}`
  return fallback
}

/** Label under an unreliable mid: this is a book touch, not a trade price. */
export const NOT_A_TRADE_PRICE_LABEL = '不能作为成交价'

export function duration(sec: unknown, fallback = '—'): string {
  const s = Math.floor(Number(sec))
  if (!Number.isFinite(s) || s < 0) return fallback
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  const rem = s % 60
  if (m < 60) return `${m}m${String(rem).padStart(2, '0')}s`
  return `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}m`
}

export function mmss(sec: unknown, fallback = '--:--'): string {
  const s = Number(sec)
  if (!Number.isFinite(s) || s < 0) return fallback
  return `${String(Math.floor(s / 60)).padStart(2, '0')}:${String(Math.floor(s % 60)).padStart(2, '0')}`
}

export function clockTime(ms?: number | null, fallback = '—'): string {
  if (!ms) return fallback
  return new Date(ms).toLocaleTimeString(undefined, { hour12: false })
}

export function dateTime(ms?: number | null, fallback = '—'): string {
  if (!ms) return fallback
  const d = new Date(ms)
  return `${d.getMonth() + 1}/${d.getDate()} ${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}:${String(d.getSeconds()).padStart(2, '0')}`
}

export function shortAddr(addr?: string | null): string {
  if (!addr) return '—'
  return addr.length > 12 ? `${addr.slice(0, 6)}…${addr.slice(-4)}` : addr
}

/**
 * 等宽数字容器类。所有 `num / money / signedMoney / pct / signedPct /
 * cents / duration / mmss / clockTime / dateTime` 的输出只在等宽容器
 * 内渲染：容器挂 `.figures`（styles/theme.css）→ 子孙继承
 * `font-variant-numeric: tabular-nums`，数字按列对齐、数值变化不抖
 * 宽度。
 *
 * 调用点不再逐元素补 `num` / `stat-num`；容器类缺失由
 * `scripts/tabular-figures.check.mjs`（注册在 `check:all` 里）在构建时
 * 拦住——漏挂让门禁变红，不靠评审。
 *
 * 此常量是模板与门禁的单一拼写来源，二者必须同步改动。
 */
export const FIGURES_CLASS = 'figures'
