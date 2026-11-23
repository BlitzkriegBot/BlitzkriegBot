/**
 * Cents / spread formatters must never print NaN¢, and a one-sided book
 * must never invent a 0.0¢ spread (issue 144, L1).
 *
 *   cd ui/webapp/webui && npm run check:cents
 */
import assert from 'node:assert/strict'
import { readFileSync, readdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  binarySpreadCents,
  bookSpreadText,
  cents,
  finiteNumber,
  formatSpreadCents,
  oneSidedQuoteText,
} from '../src/lib/format.ts'

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
const srcRoot = join(here, '..', 'src')

console.log('format helpers — no NaN, one-sided spread is —')

check('finiteNumber rejects empty / NaN / Infinity', () => {
  assert.equal(finiteNumber(undefined), null)
  assert.equal(finiteNumber(null), null)
  assert.equal(finiteNumber(''), null)
  assert.equal(finiteNumber(Number.NaN), null)
  assert.equal(finiteNumber(Infinity), null)
  assert.equal(finiteNumber('abc'), null)
  assert.equal(finiteNumber(0.5), 0.5)
  assert.equal(finiteNumber('0.01'), 0.01)
})

check('cents falls back on non-finite input', () => {
  assert.equal(cents(undefined), '—')
  assert.equal(cents(null), '—')
  assert.equal(cents(Number.NaN), '—')
  assert.equal(cents(Infinity), '—')
  assert.equal(cents(''), '—')
  assert.equal(cents(0.5), '50.0¢')
  assert.equal(cents(0.01), '1.0¢')
  assert.equal(cents(0), '0.0¢')
  for (const v of [undefined, null, Number.NaN, 'foo', 0.505]) {
    assert.doesNotMatch(cents(v), /NaN/, `NaN in cents(${JSON.stringify(v)})`)
  }
})

check('binarySpreadCents is null when either mid is missing', () => {
  assert.equal(binarySpreadCents(0.5, 0.5), 0)
  assert.equal(binarySpreadCents(0.51, 0.51), 2)
  assert.equal(binarySpreadCents(undefined, 0.5), null)
  assert.equal(binarySpreadCents(0.5, null), null)
  assert.equal(binarySpreadCents(Number.NaN, 0.5), null)
  assert.equal(formatSpreadCents(null), '—')
  assert.equal(formatSpreadCents(Number.NaN), '—')
  assert.equal(formatSpreadCents(2), '2.0¢')
  assert.doesNotMatch(formatSpreadCents(binarySpreadCents(undefined, 0.5)), /NaN/)
})

check('bookSpreadText is — when one side is empty, even if spread is 0', () => {
  assert.equal(bookSpreadText(null, 0.01, 0), '—')
  assert.equal(bookSpreadText(0.99, null, 0), '—')
  assert.equal(bookSpreadText(undefined, 0.01, 0), '—')
  assert.equal(bookSpreadText(0.49, 0.51, 0.02), '2.0¢')
  assert.equal(bookSpreadText(0.49, 0.51), '2.0¢')
  assert.doesNotMatch(bookSpreadText(null, 0.01, 0), /NaN/)
})

check('oneSidedQuoteText surfaces the live touch, never a mid', () => {
  assert.equal(oneSidedQuoteText(null, 0.01), '卖一 1.0¢')
  assert.equal(oneSidedQuoteText(0.99, null), '买一 99.0¢')
  assert.equal(oneSidedQuoteText(0.01, 0.02), '买一 1.0¢ · 卖一 2.0¢')
  assert.equal(oneSidedQuoteText(null, null), '—')
  assert.doesNotMatch(oneSidedQuoteText(Number.NaN, 0.01), /NaN/)
})

console.log('static scan — no raw toFixed¢ outside format.ts')

function walk(dir, acc = []) {
  for (const ent of readdirSync(dir, { withFileTypes: true })) {
    if (ent.name === 'node_modules' || ent.name === 'dist') continue
    const full = join(dir, ent.name)
    if (ent.isDirectory()) walk(full, acc)
    else if (/\.(ts|vue|mjs)$/.test(ent.name)) acc.push(full)
  }
  return acc
}

check('no raw toFixed…¢ in Vue/TS except format.ts', () => {
  const offenders = []
  for (const file of walk(srcRoot)) {
    if (file.endsWith('/lib/format.ts')) continue
    const src = readFileSync(file, 'utf8')
    if (/\.toFixed\([^)]*\)\s*}}¢/.test(src) || /\.toFixed\([^)]*\)\s*¢/.test(src) || /\.toFixed\([^)]*\)\s*\+\s*['"]¢['"]/.test(src)) {
      offenders.push(file.replace(srcRoot, 'src'))
    }
  }
  assert.deepEqual(offenders, [], `raw toFixed cents:\n${offenders.join('\n')}`)
})

check('no NaN¢ literal in src', () => {
  const offenders = []
  for (const file of walk(srcRoot)) {
    if (readFileSync(file, 'utf8').includes('NaN¢')) offenders.push(file.replace(srcRoot, 'src'))
  }
  assert.deepEqual(offenders, [], `NaN¢ literal:\n${offenders.join('\n')}`)
})

check('PriceGrid uses formatSpreadCents / binarySpreadCents, not a local NaN-prone spread', () => {
  const src = readFileSync(join(srcRoot, 'components/hft/PriceGrid.vue'), 'utf8')
  assert.ok(src.includes('binarySpreadCents'), 'PriceGrid missing binarySpreadCents')
  assert.ok(src.includes('formatSpreadCents'), 'PriceGrid missing formatSpreadCents')
  assert.ok(src.includes('oneSidedQuoteText'), 'PriceGrid missing oneSidedQuoteText')
  assert.doesNotMatch(src, /m\.up\s*\+\s*m\.down/)
})

check('DepthCard uses bookSpreadText for the 点差', () => {
  const src = readFileSync(join(srcRoot, 'components/hft/DepthCard.vue'), 'utf8')
  assert.ok(src.includes('bookSpreadText'), 'DepthCard missing bookSpreadText')
  assert.ok(src.includes('spreadText'), 'DepthCard missing spreadText')
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)
