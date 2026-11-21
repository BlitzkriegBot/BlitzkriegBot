/**
 * Regression check for the per-account row filter (`src/lib/account-filter.ts`,
 * E28 §9.5). The properties pinned here:
 *
 *   - rows attributed to the active account stay; rows of other accounts go;
 *   - an UNATTRIBUTED row (accountId empty/absent — a pre-E28 core or a
 *     pre-E28 record in the append-only trade log) always stays: hiding it
 *     would silently falsify the book;
 *   - no active account (switcher not mounted / book not fetched) = no
 *     filtering at all;
 *   - a core that attributes NOTHING (every accountId empty) is treated as
 *     pre-E28: no filtering, and `attributed: false` so pages label the view
 *     范围 instead of 已过滤;
 *   - `attributed` is true exactly when filtering is in effect — the honest
 *     label the pages render;
 *   - order and references are preserved (pages key rows on identity).
 *
 *   cd ui/webapp/webui && npm run check:account
 */
import assert from 'node:assert/strict'

let failures = 0
const check = (label, fn) => {
  try {
    fn()
    console.log(`  ok   ${label}`)
  } catch (e) {
    failures++
    console.log(`  FAIL ${label}\n       ${e.message}`)
  }
}

const { filterByAccount, filterByVenueDur } = await import('../src/lib/account-filter.ts')
const { priceCardId } = await import('../src/lib/round-id.ts')

const row = (id, accountId) => ({ id, accountId })

check('rows of the active account stay, others go', () => {
  const rows = [row('a', 'default'), row('b', 'paper'), row('c', 'default')]
  const out = filterByAccount(rows, 'default')
  assert.deepEqual(out.rows.map((r) => r.id), ['a', 'c'])
  assert.equal(out.attributed, true)
})

check('unattributed rows (pre-E28) always stay', () => {
  const rows = [row('a', 'default'), row('old', ''), row('older', undefined)]
  const out = filterByAccount(rows, 'paper')
  // Only the attributed `paper` row and the two unattributed ones survive.
  assert.deepEqual(out.rows.map((r) => r.id), ['old', 'older'])
  assert.equal(out.attributed, true)
})

check('no active account = no filtering, nothing attributed', () => {
  const rows = [row('a', 'default'), row('b', 'paper')]
  const out = filterByAccount(rows, null)
  assert.deepEqual(out.rows.map((r) => r.id), ['a', 'b'])
  assert.equal(out.attributed, false)
})

check('a fully unattributed list reads as pre-E28: nothing filtered', () => {
  const rows = [row('a', ''), row('b', undefined)]
  const out = filterByAccount(rows, 'default')
  assert.deepEqual(out.rows.map((r) => r.id), ['a', 'b'])
  assert.equal(out.attributed, false, 'pages must label this 范围, not 已过滤')
})

check('order is preserved and the input is not mutated', () => {
  const rows = [row('a', 'paper'), row('b', 'default'), row('c', 'paper')]
  const snapshot = JSON.stringify(rows)
  const out = filterByAccount(rows, 'paper')
  assert.deepEqual(out.rows.map((r) => r.id), ['a', 'c'])
  assert.equal(JSON.stringify(rows), snapshot, 'input array must be untouched')
})

check('real wire shapes: trade rows with accountId from the gateway', () => {
  const rows = [
    { id: 'hft-1', accountId: 'default', asset: 'BTC_UP', netPnlUsd: 1.2 },
    { id: 'hft-0', accountId: '', asset: 'BTC_UP', netPnlUsd: -0.3 },
  ]
  const out = filterByAccount(rows, 'default')
  assert.deepEqual(out.rows.map((r) => r.id), ['hft-1', 'hft-0'])
  assert.equal(out.attributed, true)
})


const price = (asset, durationSec, up, down) => ({
  venue: 'polymarket', asset, durationSec, up, down,
})

check('priceCardId stamps the family suffix the operator asked for', () => {
  assert.equal(priceCardId('BTC', 300), 'BTC-5m')
  assert.equal(priceCardId('ETH', 900), 'ETH-15m')
  assert.equal(priceCardId('SOL', 3600), 'SOL-1h')
  assert.equal(priceCardId('XRP', 86400), 'XRP-1d')
})

check('a 5m-anchored account shows exactly one price card per asset', () => {
  const rows = [
    price('BTC', 900, 0.97, 0.03),
    price('ETH', 900, 0.99, 0.01),
    price('SOL', 900, 0.96, 0.04),
    price('XRP', 900, 0.98, 0.02),
    price('BTC', 300, 0.84, 0.16),
    price('ETH', 300, 0.77, 0.23),
    price('SOL', 300, 0.74, 0.26),
    price('XRP', 300, 0.85, 0.15),
  ]
  const out = filterByVenueDur(rows, 'polymarket', 300)
  assert.equal(out.attributed, true)
  assert.deepEqual(out.rows.map((r) => r.asset), ['BTC', 'ETH', 'SOL', 'XRP'])
  assert.ok(out.rows.every((r) => r.durationSec === 300), 'must not keep the 15m twin of the same asset')
  const ids = out.rows.map((r) => priceCardId(r.asset, r.durationSec))
  assert.deepEqual(ids, ['BTC-5m', 'ETH-5m', 'SOL-5m', 'XRP-5m'])
  assert.equal(new Set(ids).size, ids.length, 'card ids must be unique')
})

check('untagged twins do not sneak into a duration-filtered 5m grid', () => {
  const rows = [
    price('BTC', 300, 0.84, 0.16),
    { venue: 'polymarket', asset: 'BTC', up: 0.97, down: 0.03 },
    price('ETH', 300, 0.77, 0.23),
  ]
  const out = filterByVenueDur(rows, 'polymarket', 300)
  assert.deepEqual(out.rows.map((r) => r.asset), ['BTC', 'ETH'])
  assert.ok(out.rows.every((r) => r.durationSec === 300))
})

check('old kernels that stamp no durationSec still fall back to venue-only', () => {
  const rows = [
    { venue: 'polymarket', asset: 'BTC', up: 0.5, down: 0.5 },
    { venue: 'polymarket', asset: 'ETH', up: 0.4, down: 0.6 },
  ]
  const out = filterByVenueDur(rows, 'polymarket', 300)
  assert.deepEqual(out.rows.map((r) => r.asset), ['BTC', 'ETH'])
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)

process.exit(failures === 0 ? 0 : 1)
