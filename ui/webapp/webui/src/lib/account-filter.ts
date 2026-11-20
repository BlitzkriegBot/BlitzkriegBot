/**
 * Per-account row filtering (E28 §9.5).
 *
 * The kernel attributes every position/order/trade row with the account it
 * belongs to (`accountId`, forwarded verbatim by the gateway). The panel's
 * connection has ONE session default account (the top-bar switcher), so the
 * pages filter rows locally: only rows of the active account stay.
 *
 * The empty string is a STATE, not a missing value: a pre-E28 core (or a
 * pre-E28 record in a long-lived trade log) writes no `accountId` at all.
 * Those rows are unattributed — nobody can say which account they belong to,
 * so hiding them would silently falsify the book. They always show, whatever
 * the switcher says, and `attributed` reports whether any filtering is
 * actually happening so pages can label the view honestly.
 */

/** One attributed row: the only field the filter reads. */
interface AccountedRow {
  accountId?: string
}

export interface AccountFilterResult<T> {
  /** Rows to render, in input order. */
  rows: T[]
  /**
   * True only when filtering is IN EFFECT: an active account is known AND at
   * least one input row carried a non-empty accountId (i.e. the core speaks
   * E28). A page may label its table "已按账户过滤" only when this is true.
   */
  attributed: boolean
}

/**
 * Filter rows to the active account. `active === null` (switcher not mounted,
 * book not fetched yet) or an empty accountId on the row → keep the row.
 */
export function filterByAccount<T extends AccountedRow>(
  rows: readonly T[],
  active: string | null,
): AccountFilterResult<T> {
  if (!active) return { rows: [...rows], attributed: false }
  const anyAttributed = rows.some((r) => !!r.accountId)
  if (!anyAttributed) return { rows: [...rows], attributed: false }
  return {
    // Unattributed rows (empty accountId) stay visible: pre-E28 cores and
    // pre-E28 log records cannot be filtered and must not be silently dropped.
    rows: rows.filter((r) => !r.accountId || r.accountId === active),
    attributed: true,
  }
}

// ── venue 层（账户绑定交易所的下游过滤）────────────────────────────────────
/** One venue-tagged row: the only field the venue filter reads (#427 rows). */
interface VenuedRow {
  venue?: string
  /**
   * 轮次档位（秒）：该行属于哪一档轮次。老内核/未打 tag 行缺省 →
   *  undefined，前端不据此丢行，降回 venue-only（feature-detect）。
   */
  durationSec?: number
}

export interface VenueFilterResult<T> {
  rows: T[]
  /** True only when an active venue is bound AND rows actually carry venue tags. */
  attributed: boolean
}

/**
 * Filter rows to the venue the ACTIVE ACCOUNT is bound to (账户切换器就是
 * 交易所切换器). Empty binding (or no active account) → show everything —
 * an unbound account trades any venue, so its page must too. Rows without a
 * venue tag (pre-#427 cores/records) stay visible for the same reason
 * `filterByAccount` keeps unattributed rows: nobody can say where they came
 * from, so hiding them would falsify the book.
 */
export function filterByVenue<T extends VenuedRow>(
  rows: readonly T[],
  venue: string,
): VenueFilterResult<T> {
  if (!venue) return { rows: [...rows], attributed: false }
  const anyTagged = rows.some((r) => !!r.venue)
  if (!anyTagged) return { rows: [...rows], attributed: false }
  return {
    rows: rows.filter((r) => !r.venue || r.venue === venue),
    attributed: true,
  }
}

/**
 * venue 层双过滤（行情多档位：同一 venue 的 5m/15m 轮次可并行，行带
 * `durationSec` 标记所属档位）。仅当 `durationSec` 有值且输入行中
 * 有人携带 `durationSec` 时，才按 `venue + durationSec` 双过滤；
 * `durationSec` 缺省（null/undefined）或全行未打 tag 时降回 venue-only，
 * 与 `filterByVenue` 语义一致——老内核/未打 tag 行不被静默丢弃。
 * 单行自缺 `durationSec` 时同样退回 venue-only（保留不可归档位的行）。
 */
export function filterByVenueDur<T extends VenuedRow>(
  rows: readonly T[],
  venue: string,
  durationSec?: number | null,
): VenueFilterResult<T> {
  if (!venue) return { rows: [...rows], attributed: false }
  const anyTagged = rows.some((r) => !!r.venue)
  if (!anyTagged) return { rows: [...rows], attributed: false }
  const someCarryDur = rows.some((r) => typeof r.durationSec === 'number')
  if (typeof durationSec === 'number' && durationSec > 0 && someCarryDur) {
    return {
      rows: rows.filter(
        (r) =>
          (!r.venue || r.venue === venue) &&
          (typeof r.durationSec !== 'number' || r.durationSec === durationSec),
      ),
      attributed: true,
    }
  }
  return {
    rows: rows.filter((r) => !r.venue || r.venue === venue),
    attributed: true,
  }
}
