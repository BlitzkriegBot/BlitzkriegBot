/**
 * Percent formatters must never emit `%` after a non-numeric value (`—%`,
 * `null%`, `undefined%`, `NaN%`) and must never coerce null/empty/boolean into
 * `0.0%` (operator ruling 2026-11-25). Win rate stays numeric (`0.0%` on 0
 * trades via `winRateOf`).
 *
 *   cd ui/webapp/webui && npm run check:pct
 */
import assert from 'node:assert/strict'
import { readFileSync, readdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { finiteNumber, pct, signedPct, winRateOf } from '../src/lib/format.ts'

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

console.log('percent formatters — % only follows finite numbers')

check('finiteNumber rejects null / undefined / empty / whitespace / boolean / NaN / Infinity', () => {
  for (const bad of [undefined, null, '', '   ', false, true, Number.NaN, Infinity, -Infinity, 'abc']) {
    assert.equal(finiteNumber(bad), null, `expected null for ${JSON.stringify(bad)}`)
  }
  assert.equal(finiteNumber(0), 0)
  assert.equal(finiteNumber('0'), 0)
  assert.equal(finiteNumber(12.5), 12.5)
  assert.equal(finiteNumber('12.5'), 12.5)
})

check('pct falls back to — on non-numeric inputs and preserves numeric outputs', () => {
  for (const bad of [undefined, null, '', '   ', false, true, Number.NaN, Infinity, -Infinity, 'foo']) {
    const out = pct(bad)
    assert.equal(out, '—', `pct(${JSON.stringify(bad)}) -> ${out}`)
    assert.ok(!out.includes('%'), `non-numeric pct must not include %: ${out}`)
  }
  assert.equal(pct(0), '0.0%')
  assert.equal(pct(66.66), '66.7%')
  assert.equal(pct(100), '100.0%')
  assert.equal(pct(winRateOf(0, 0)), '0.0%')
  assert.equal(pct(winRateOf(3, 1)), '75.0%')
})

check('signedPct falls back to — on non-numeric inputs and preserves signed numeric outputs', () => {
  for (const bad of [undefined, null, '', '   ', false, true, Number.NaN, Infinity, -Infinity, 'foo']) {
    const out = signedPct(bad)
    assert.equal(out, '—', `signedPct(${JSON.stringify(bad)}) -> ${out}`)
    assert.ok(!out.includes('%'), `non-numeric signedPct must not include %: ${out}`)
  }
  assert.equal(signedPct(0), '0.0%')
  assert.equal(signedPct(4.91, 2), '+4.91%')
  assert.equal(signedPct(-12.3, 1), '−12.3%')
})

console.log('static scan — no —% / null% / undefined% / NaN% in src')

function walk(dir, acc = []) {
  for (const ent of readdirSync(dir, { withFileTypes: true })) {
    if (ent.name === 'node_modules' || ent.name === 'dist') continue
    const full = join(dir, ent.name)
    if (ent.isDirectory()) walk(full, acc)
    else if (/\.(ts|vue|mjs)$/.test(ent.name)) acc.push(full)
  }
  return acc
}

check('no non-numeric % literal or ?? "—"}% interpolation in src', () => {
  const offenders = []
  for (const file of walk(srcRoot)) {
    const src = readFileSync(file, 'utf8')
    if (/(?:—|null|undefined|NaN)%/.test(src) || /\?\?\s*['"]—['"]\s*\}%/.test(src)) {
      offenders.push(file.replace(srcRoot, 'src'))
    }
  }
  assert.deepEqual(offenders, [], `non-numeric % pattern found:\n${offenders.join('\n')}`)
})

check('SettingsRiskLimits exitCurrentText formats % fields via finiteNumber', () => {
  const src = readFileSync(join(srcRoot, 'pages/settings/SettingsRiskLimits.vue'), 'utf8')
  assert.ok(src.includes('finiteNumber'), 'SettingsRiskLimits must import finiteNumber')
  assert.doesNotMatch(src, /\$\{e\.(stopLossPct|takeProfitPct|holderStopPct)\b[^}]*\}%/)
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)
