/**
 * Regression check for the issue 1282 snapshot pagination two-source split.
 *
 * The gateway used to pass `trade_limit=0` (drain the ENTIRE append-only trade
 * log into every snapshot payload — 15s poll AND 1s SSE tick), because the
 * history tab and the equity curve read `store.tradeRows`. issue 1282 caps the
 * snapshot's first screen at the newest 200 rows; that silently truncated
 * every view that still read the snapshot's rows:
 *
 *   - the equity curve (Overview + the shared 累计净 PnL card) would end 200
 *     trades in, while the summary above it kept quoting the all-time total —
 *     the same "shape mirrors the total" contradiction check:history already
 *     pinned once;
 *   - the cumStats ROW-FOLD fallback (old cores without a summary) would fold
 *     only the newest 200 rows;
 *   - the history tab would scroll to nothing past row 200.
 *
 * The fix is a two-source split in the store, and these checks pin each half
 * so the truncation cannot silently return:
 *
 *   fast path  `store.tradeRows`       — newest ~200 rows with the snapshot
 *                                        (15s poll + 1s SSE); feeds the
 *                                        alert sounds and order attribution,
 *                                        which only care about recent rows;
 *   deep path  `store.fullHistoryRows` — the paged full log (GET /api/trades
 *                                        → trades.history {offset, limit}),
 *                                        falling back to the fast path until
 *                                        the first page walk lands.
 *
 *   cd ui/webapp/webui && npm run check:trades-pages
 */
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

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

const { dedupeTrades, tradeIdentity } = await import('../src/lib/trades.ts')

const here = dirname(fileURLToPath(import.meta.url))
const read = (p) => readFileSync(join(here, '..', p), 'utf8')
const store = read('src/stores/panel.ts')
const overview = read('src/pages/Overview.vue')
const hft = read('src/pages/HftPage.vue')
const pnlCard = read('src/components/ui/pnl/PnLCurveCard.vue')
// The gateway and route live one level up from this package.
const repo = (p) => readFileSync(join(here, '..', '..', '..', '..', p), 'utf8')
const gatewayRun = repo('ui/ui_kit/src/web/gateway_run.rs')
const routes = repo('ui/ui_kit/src/web/routes.rs')

console.log('snapshot pagination: two-source split, depth path, wire')

// ── the deep path is the ONLY source for full-depth views ───────────────────
check('the equity curve reads the full-depth rows, not the snapshot window', () => {
  // The reported defect: cap the snapshot at 200 rows and the curve silently
  // ends 200 trades in while the headline above it stays all-time.
  assert.match(overview, /const tradeRows = computed\(\(\) => store\.fullHistoryRows\)/,
    'Overview tradeRows must read store.fullHistoryRows')
  assert.match(pnlCard, /filterByAccount\(store\.fullHistoryRows/,
    'the 累计净 PnL card must read store.fullHistoryRows')
})

check('the history tab reads the full-depth rows', () => {
  // Same defect for the tab: the waterfall would stop at the snapshot window.
  assert.match(hft, /filterByAccount\(store\.fullHistoryRows, store\.activeAccount\)/,
    'HftPage accountFilter must read store.fullHistoryRows')
  assert.doesNotMatch(hft, /filterByAccount\(store\.tradeRows/,
    'HftPage must not read the snapshot window for history-derived views')
})

// ── the store actually walks the log to the bottom ──────────────────────────
check('the store pages GET /api/trades with offset windows until a short page', () => {
  // The loader must walk offset forward until the kernel answers a page
  // shorter than the window (or empty — offset past the oldest row) — a
  // single-page loader would just be a 500-row truncation with extra steps.
  assert.match(store, /api\.tradesPage\(\{ offset, limit: FULL_TRADES_PAGE \}\)/,
    'the loader must page through /api/trades')
  assert.match(store, /if \(page\.length < FULL_TRADES_PAGE\) break/,
    'a short page is the end-of-log sentinel')
})

check('the store falls back to the fast path until the walk lands', () => {
  // First paint has no full log yet; the deep path must degrade to the
  // snapshot window instead of rendering an empty curve/history.
  assert.match(store, /fullTradeRows\.value \? dedupeTrades\(\[\.\.\.fullTradeRows\.value, \.\.\.tradeRows\.value\]\) : tradeRows\.value/)
})

check('an identity change invalidates the in-flight walk', () => {
  // 切户 must not let a page walk for the OLD identity write into the new
  // one's rows — the same arbitration refresh() already runs (issue 781).
  assert.match(store, /requestId !== fullTradesSeq \|\| epoch !== displayEpoch/,
    'the walk checks its request id and the display epoch before each write')
})

// ── the wire: gateway cap + the paging route ────────────────────────────────
check('the gateway caps the first screen instead of draining the log', () => {
  // issue 1282's whole point: limit=0 made every snapshot payload grow with the
  // log forever. Reverting the cap re-opens the backpressure hole.
  assert.match(gatewayRun, /WebServer::with_shared_gateway\(client, 200, dispatcher\.clone\(\)\)/,
    'the gateway snapshot trade_limit must be 200')
  assert.doesNotMatch(gatewayRun, /with_shared_gateway\(client, 0,/,
    'trade_limit=0 (full drain per snapshot) must stay gone')
})

check('GET /api/trades forwards the offset window to trades.history', () => {
  // The route is the depth path's only wire; without it the store's loader
  // would 404 and every full-depth view would silently live on the fallback.
  assert.match(routes, /"GET", "\/api\/trades"/)
  assert.match(routes, /self\.proxy_read\("trades\.history"/)
  assert.match(routes, /query_param\("offset"\)/)
  assert.match(routes, /query_param\("limit"\)/)
})

// ── the merge keeps the curve chronological ─────────────────────────────────
check('pages joined oldest-first stay ascending, and the snapshot merge keeps it', () => {
  // The loader receives the newest page FIRST (offset=0). Concatenating in
  // arrival order would hand the equity curve a sawtooth — the shape defect
  // check:history pinned for the curve lives again in the merge. Replicates
  // the store's merge: pages reversed, then snapshot rows appended, dedupe
  // keeps the last write at its original position.
  const page = (from, to) => rows(from, to)
  const rows = (from, to) => {
    const out = []
    for (let t = from; t < to; t += 60_000) {
      out.push({ id: `hft-${t}`, asset: 'BTC', entryTime: t, exitTime: t + 3_000, netPnlUsd: 1 })
    }
    return out
  }
  const pages = [rows(2_400_000, 2_900_000), rows(1_800_000, 2_400_000)] // newest first, as fetched
  pages.reverse()
  const merged = dedupeTrades([...pages.flat(), ...page(2_900_000, 2_950_000)])
  for (let i = 1; i < merged.length; i++) {
    assert.ok(
      Number(merged[i - 1].entryTime) < Number(merged[i].entryTime),
      `row ${i} breaks the ascending order`,
    )
  }
})

check('a row the snapshot refreshes keeps its place and takes the newer write', () => {
  // The fast path re-delivers rows the full log already holds; the merge must
  // not duplicate them (curve double-counts) and must keep the freshest copy.
  const full = [
    { id: 'hft-1', asset: 'BTC', entryTime: 1000, exitTime: 4000, netPnlUsd: 1 },
    { id: 'hft-2', asset: 'ETH', entryTime: 2000, exitTime: 5000, netPnlUsd: -1 },
  ]
  const fresh = [
    { id: 'hft-2', asset: 'ETH', entryTime: 2000, exitTime: 5000, netPnlUsd: -0.5 },
    { id: 'hft-3', asset: 'SOL', entryTime: 6000, exitTime: 9000, netPnlUsd: 2 },
  ]
  const merged = dedupeTrades([...full, ...fresh])
  assert.equal(merged.length, 3, 'the duplicate collapses')
  const dup = merged.find((r) => tradeIdentity(r) === 't:ETH:2000:5000')
  assert.equal(dup.netPnlUsd, -0.5, 'the snapshot copy (newer write) wins')
  assert.equal(merged[merged.length - 1].asset, 'SOL', 'brand-new rows append at the end')
})

console.log(`\nRESULT: ${failures === 0 ? 'PASS' : `FAIL (${failures})`}`)
process.exit(failures === 0 ? 0 : 1)
