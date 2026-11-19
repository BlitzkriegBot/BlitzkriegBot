/**
 * Regression check for the #1294 trades-degrade signal.
 *
 * The defect this pins: `IpcClient::snapshot()` used `unwrap_or_default()` on
 * the trades fetch, so ONE shape-drifted row failed the whole
 * `Vec<TradeView>` parse and the panel drew an EMPTY history on a snapshot
 * that otherwise read healthy (`connected: true`, no error). "No closed
 * trades" and "trade data unusable" were indistinguishable, and the history
 * tab / equity curve / cumulative stats all silently reset to zero.
 *
 * The fix has three parts, and each check below pins one:
 *   1. the snapshot JSON carries `tradesDegrade` (null = healthy) — the wire
 *      type must declare it, feature-detect friendly for older cores;
 *   2. the 行情 page reads it and renders a kit `AlertBanner` — a hand-rolled
 *      tinted div would bypass the design gate's single alert primitive;
 *   3. both degradation halves speak: `unavailable` (call failed) vs
 *      `skippedRows` (rows dropped), so the operator sees which one happened.
 *
 *   cd ui/webapp/webui && npm run check:trades-degrade
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

const here = dirname(fileURLToPath(import.meta.url))
const snapshotType = readFileSync(join(here, '..', 'src', 'api', 'snapshot.ts'), 'utf8')
const page = readFileSync(join(here, '..', 'src', 'pages', 'HftPage.vue'), 'utf8')

console.log('trades degrade: wire type, banner binding, both halves')

check('Snapshot type declares tradesDegrade as optional and nullable', () => {
  const m = /tradesDegrade\?:\s*\{[^}]*unavailable:\s*boolean[^}]*skippedRows:\s*number[^}]*\}\s*\|\s*null/.exec(
    snapshotType,
  )
  assert.ok(
    m,
    'tradesDegrade must be optional (`?`) and `| null` — an older core omits the key, and the panel must read that as healthy, not as a type error at runtime',
  )
})

check('行情页 derives the signal from the raw snapshot', () => {
  assert.ok(
    /const tradesDegrade = computed\(\(\) => snap\.value\?\.tradesDegrade \?\? null\)/.test(page),
    'HftPage must expose `tradesDegrade` from `snap.value?.tradesDegrade ?? null` — the `?? null` is the old-core feature detect',
  )
})

check('the banner is the kit AlertBanner, not a hand-rolled div', () => {
  assert.ok(
    /<AlertBanner v-if="tradesDegrade"[\s\S]{0,120}:tone="tradesDegrade\.unavailable \? 'error' : 'warn'"/.test(
      page,
    ),
    'the degrade signal must render through AlertBanner with error for unavailable / warn for skipped rows',
  )
})

check('both degradation halves have operator copy', () => {
  assert.ok(
    page.includes('成交数据不可用'),
    'the unavailable half must say the data is NOT a full record — an empty table alone reads as "no trades"',
  )
  assert.ok(
    /skippedRows \}\} 行成交数据无法解析/.test(page),
    'the skipped half must print the row count so the operator knows how much is missing',
  )
})

check('a healthy snapshot renders no banner', () => {
  assert.ok(
    /<AlertBanner v-if="tradesDegrade"/.test(page),
    'the banner must be gated on the signal — null (healthy) must render nothing',
  )
})

console.log(failures ? `\nRESULT: FAIL (${failures})` : '\nRESULT: PASS')
process.exit(failures ? 1 : 0)
