/**
 * Regression check for the Overview「未平仓浮动合计」unit (issue 744): the
 * aggregate must be DOLLARS — (currentPrice - entryPrice) × shares — never the
 * kernel's `unrealizedPct` (a percent, the per-row display unit) times shares
 * dressed up as money by `signedMoney`. The bug shape: 10 shares @0.50→0.05
 * rendered as -900「U」when the real floating P&L is -4.50.
 *
 * The computed lives inside the SFC, so this check extracts the exact reduce
 * expression from the source and executes it against fixtures — the gate runs
 * the arithmetic the page will run, not a restatement of it.

 *   cd ui/webapp/webui && npm run check:overview-unrealized
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
const page = readFileSync(join(here, '..', 'src', 'pages', 'Overview.vue'), 'utf8')

console.log('overview unrealized total: dollars, not percent')

// The aggregate reducer, extracted verbatim from the computed — evaluated
// below so the fixtures exercise the page's real arithmetic. TWO shapes are
// legal: a bare expression `(a, p) => a + ...`, and a block body
// `(a, p) => { ... }`. The block form is what a per-row guard needs (a row
// whose mark is unobserved must contribute nothing rather than fold a 0
// sentinel in as "principal wiped out"), so the extractor accepts both and
// the fixtures below run whichever one the page actually ships.
const m = page.match(
  /const unrealized = computed\(\(\) =>\s*\n\s*positions\.value\.reduce\(\s*\(a, p\) => (.*?),\s*\n\s*0,/s,
)
const step = m?.[1]?.trim()
const run = step
  ? new Function('a', 'p', step.startsWith('{') ? step : `return ${step}`)
  : null

check('the reduce is present and pure-price × shares shaped', () => {
  assert.ok(step, 'the unrealized computed reduce was not found in Overview.vue')
  assert.match(step, /Number\(p\.currentPrice\)\s*-\s*Number\(p\.entryPrice\)/)
  assert.match(step, /Number\(p\.shares\)\s*\|\|\s*0/, 'legacy cores may omit shares — it must floor to 0')
})

check('the percent field never enters the aggregate (the bug shape)', () => {
  assert.doesNotMatch(step ?? '', /unrealizedPct/, 'unrealizedPct is a percent — multiplying it into shares is the issue 744 bug')
})

check('10 shares @0.50→0.05 totals -4.50 dollars, not -900', () => {
  assert.equal(run?.(0, { currentPrice: 0.05, entryPrice: 0.50, shares: 10 }), -4.5)
})

check('a missing shares field contributes zero (old cores)', () => {
  assert.equal(run?.(0, { currentPrice: 1.5, entryPrice: 1.0, shares: undefined }), 0)
})

check('a missing price cannot poison the sum with NaN', () => {
  assert.equal(run?.(0, { currentPrice: undefined, entryPrice: 0.5, shares: 10 }), 0)
})

// `markObserved === false` = the venue never quoted this token, so
// `currentPrice` is only the kernel's 0 sentinel. Folding that zero in reads as
// "principal fully lost" — -50 on a 10-share 0.50 entry that has no mark at
// all. The row must contribute NOTHING, and a pre-field snapshot (undefined)
// must still count (the panel treats a missing key as observed).
check('an unobserved mark contributes nothing instead of a fake total loss', () => {
  const unobserved = { currentPrice: 0, entryPrice: 0.5, shares: 10, markObserved: false }
  assert.equal(run?.(0, unobserved), 0, 'a 0 sentinel must never reach the aggregate')
  assert.equal(
    run?.(0, { ...unobserved, markObserved: undefined }),
    -5,
    'a core without the field is treated as observed — the old arithmetic stands',
  )
})

check('the stat still renders the total as money, not a percent', () => {
  assert.match(page, /signedMoney\(unrealized\)/)
})

process.exit(failures ? 1 : 0)
