#!/usr/bin/env node
/**
 * #179 — the panel must not compute the account's money itself.
 *
 * The ledger is the cash truth (操作员裁决 2026-11-08). The kernel publishes the
 * one net-profit figure the panel may show (`cumulativeNetPnl`, the SSOT —
 * `schema/views.rs`: 「面板不得再按 cash − seed 另算一套」), and it does NOT
 * publish equity (`AccountView` has neither `equity` nor `opening_equity`). The
 * panel used to close that gap by arithmetic:
 *
 *   equity = seed + net          ← seed is 累计 Seed 入金, not 期初权益
 *   identityBroken = |cash − (seed + net)| > 0.005
 *   Overview.vue:  equity === null ? '—' : money(cash)      ← cash as equity
 *   StatsRow.vue:  equity !== null ? money(equity) : money(cash)
 *
 * Every one of those is R7.1 L5: a fallback figure presented as data. This gate
 * pins three things:
 *
 *   [A] STRUCTURE — `balanceView` reports `equity: null` and no `seed` /
 *       `identityBroken` field, whatever the inputs. Composed from real-looking
 *       numbers or not, there is no equity on the wire to report.
 *   [B] HEADLINE — `equityHeadline(null)` is 「—」, and a null equity never
 *       produces a currency amount. This is the rendered-string contract both
 *       call sites (Overview tile, StatsRow recon row) now share, so the two
 *       cannot drift back into their own fallbacks.
 *   [C] SOURCE SCAN — no `src/**` file reintroduces the arithmetic or the
 *       cash-under-an-equity-label pattern. Comments are stripped first: the
 *       module's own rationale quotes `seed + net`, and a scanner that fails on
 *       its own documentation is a scanner people delete.
 *
 *   cd ui/webapp/webui && npm run check:money
 */
import assert from 'node:assert/strict'
import { readFileSync, readdirSync, statSync, mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs'
import { dirname, join, relative } from 'node:path'
import { fileURLToPath } from 'node:url'
import { tmpdir } from 'node:os'

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

const HERE = dirname(fileURLToPath(import.meta.url))
const WEBUI = join(HERE, '..')
const SRC = join(WEBUI, 'src')

const { balanceView, equityHeadline } = await import('../src/lib/balance.ts')

const snapshotBalance = (over) => ({ balance: 0, reserved: 0, available: 0, seed: null, ...over })

console.log('A. balanceView — equity is never composed')

check('equity is null when backend omits it, even when a seed and a net are both present', () => {
  // The exact case the old code composed: the live book's 2026-11-08 state.
  const v = balanceView(snapshotBalance({ balance: 1000, available: 1000, seed: 1000 }), -3.08, 1.86)
  assert.equal(v.equity, null, 'no equity on the wire → none to report')
  assert.equal(v.net, -3.08, 'the ledger net is passed through untouched')
  assert.equal(v.cash, 1000, 'the ledger cash is passed through untouched')
})

check('backend-shipped AccountView.equity is passed through verbatim (including 0)', () => {
  const funded = balanceView(snapshotBalance({ balance: 980, available: 980 }), 12.5, 0, '1012.50')
  assert.equal(funded.equity, 1012.5, 'kernel equity crosses the wire verbatim')
  const zero = balanceView(snapshotBalance({ balance: 0, available: 0 }), 0, 0, '0')
  assert.equal(zero.equity, 0, 'zero equity is a real fact, not null')
})

check('equity stays null for every combination of seed and net', () => {
  for (const seed of [null, 0, 1000, -5]) {
    for (const net of [null, 0, 39.44, -50]) {
      const v = balanceView(snapshotBalance({ balance: 1073.29, seed }), net, 34.96)
      assert.equal(v.equity, null, `seed=${seed} net=${net} still yields no equity`)
    }
  }
})

check('no seed field survives on the view (no principal to compose with)', () => {
  const v = balanceView(snapshotBalance({ balance: 1073.29, seed: 1000 }), 39.44, 0)
  assert.equal('seed' in v, false, '`seed` left the shape with the equity arithmetic')
})

check('no identityBroken flag survives (the identity it asserted was wrong)', () => {
  // |cash − (seed + net)| was only meaningful if cash == seed + net held, which
  // it does not when opening_equity ≠ seeded_total — i.e. on the real book.
  const v = balanceView(snapshotBalance({ balance: 1000, seed: 1000 }), -3.08, 1.86)
  assert.equal('identityBroken' in v, false, 'the flag left with the equation it policed')
})

check('gross is net + fees on the trade-fold basis (kept: both inputs are real)', () => {
  // Deliberately retained — see balance.ts. Fees are an expense already deducted
  // from net, so adding them back recovers the pre-cost figure. It is not a
  // ledger number and is labelled 成交口径 wherever rendered.
  const v = balanceView(snapshotBalance({ balance: 1039.44 }), 39.44, 34.96)
  assert.equal(Number(v.gross.toFixed(2)), 74.40)
  assert.equal(v.net, 39.44)
  assert.ok(v.gross > v.net, 'the expense is deducted from gross, not added')
})

check('gross is null when net is unknown, never a bare fee number', () => {
  const v = balanceView(snapshotBalance({ balance: 1073.29 }), null, 34.96)
  assert.equal(v.gross, null)
  assert.equal(v.net, null)
})

check('a missing balance object degrades the ledger reads to zeros, not NaN', () => {
  const v = balanceView(undefined, 0, 0)
  assert.deepEqual([v.cash, v.reserved, v.available], [0, 0, 0])
  assert.equal(v.equity, null)
  assert.ok(Number.isFinite(v.gross), 'gross must stay a number')
})

console.log('\nB. equityHeadline — unknown renders 「—」, never an amount')

check('a null equity renders the em dash', () => {
  assert.equal(equityHeadline(null), '—')
})

check('a null equity never renders a currency amount', () => {
  const out = equityHeadline(null)
  assert.equal(/\$|\d/.test(out), false, `unknown equity printed figures: ${out}`)
})

check('a known equity renders as money (ready for the backend field)', () => {
  // The helper is exercised with a real number so the #181 wire-up is a
  // pass-through, not a rewrite. Until then balanceView only ever emits null.
  assert.equal(equityHeadline(1039.44), '$1,039.44')
})

check('a zero equity is a number, not "unknown"', () => {
  // 0 is a fact (the account is wiped out); only null is unknown. Conflating
  // them is how a wiped-out account would render as "no data".
  assert.equal(equityHeadline(0), '$0.00')
})

check('both equity call sites route through the one helper', () => {
  // The two fallbacks were independent copies; one helper means one place to
  // get the unknown case right.
  const overview = readFileSync(join(SRC, 'pages', 'Overview.vue'), 'utf8')
  const statsRow = readFileSync(join(SRC, 'components', 'hft', 'StatsRow.vue'), 'utf8')
  assert.match(overview, /equityHeadline\(accountBalance\.equity\)/, 'overview tile lost the helper')
  assert.match(statsRow, /equityHeadline\(recon\.equity\)/, 'stats row lost the helper')
})

console.log('\nC. source scan — no client-side money math')

const SOURCE_EXT = new Set(['.ts', '.vue'])

function listSources(root) {
  const out = []
  const walk = (dir) => {
    for (const name of readdirSync(dir)) {
      if (name === 'node_modules' || name === 'dist') continue
      const p = join(dir, name)
      if (statSync(p).isDirectory()) walk(p)
      else if (SOURCE_EXT.has(name.slice(name.lastIndexOf('.')))) out.push(p)
    }
  }
  walk(root)
  return out.sort()
}

/** Drop // and /* *\/ comments and HTML <!-- --> comments; keep strings. */
function stripComments(text) {
  let out = ''
  let i = 0
  let quote = null
  while (i < text.length) {
    const c = text[i]
    if (quote) {
      out += c
      if (c === '\\' && i + 1 < text.length) {
        out += text[++i]
        i++
        continue
      }
      if (c === quote) quote = null
      i++
      continue
    }
    if (c === '"' || c === "'" || c === '`') {
      quote = c
      out += c
      i++
      continue
    }
    if (c === '/' && text[i + 1] === '/') {
      while (i < text.length && text[i] !== '\n') i++
      continue
    }
    if (c === '/' && text[i + 1] === '*') {
      const end = text.indexOf('*/', i + 2)
      i = end === -1 ? text.length : end + 2
      continue
    }
    if (c === '<' && text.startsWith('<!--', i)) {
      const end = text.indexOf('-->', i + 4)
      i = end === -1 ? text.length : end + 3
      continue
    }
    out += c
    i++
  }
  return out
}

/**
 * [rule, pattern, why] — each one is a shape that was really in the tree before
 * #179. Matched against comment-stripped source, so documentation may quote them.
 */
const BANNED = [
  [
    'seed-plus-net',
    /\bseed\s*\+\s*(?:realized|net)\b/,
    'composes equity from 累计 Seed 入金 — not the 期初权益 net is measured against',
  ],
  [
    'realized-plus-seed',
    /\b(?:realized|net)\s*\+\s*seed\b/,
    'same wrong identity, operands swapped',
  ],
  [
    'cash-minus-seed',
    /\bcash\s*-\s*(?:seed|seededTotal)\b/,
    'derives net profit from cash − seed instead of reading the SSOT',
  ],
  [
    'cash-as-equity',
    /\bmoney\(\s*(?:[A-Za-z_$][\w$]*\.)?cash\s*\)/,
    'renders the cash ledger under a money slot — that is what impersonated equity',
  ],
  [
    'equity-null-coalesce',
    /equity\s*\?\?\s*(?:0\b|money\()/,
    'silently turns unknown equity into a number',
  ],
  [
    'identity-broken',
    /\bidentityBroken\b/,
    'policed an identity that does not hold when opening_equity ≠ seeded_total',
  ],
]

function scan(root) {
  const hits = []
  for (const abs of listSources(join(root, 'src'))) {
    const rel = relative(root, abs).split('\\').join('/')
    const raw = stripComments(readFileSync(abs, 'utf8'))
    raw.split(/\r?\n/).forEach((line, idx) => {
      for (const [rule, re, why] of BANNED) {
        if (re.test(line)) hits.push({ file: rel, line: idx + 1, rule, why, text: line.trim() })
      }
    })
  }
  return hits
}

const hits = scan(WEBUI)
check('no client-side equity/net arithmetic anywhere in src/**', () => {
  assert.deepEqual(
    hits.map((h) => `${h.file}:${h.line}:${h.rule}  ${h.why}\n       ${h.text}`),
    [],
    'banned money pattern(s) found',
  )
})

console.log('\nD. scanner self-test — the rules catch the shapes they name')

check('the rules fire on the shapes they name, and skip comments', () => {
  const dir = mkdtempSync(join(tmpdir(), 'money-ssot-'))
  try {
    mkdirSync(join(dir, 'src', 'pages'), { recursive: true })
    writeFileSync(
      join(dir, 'src', 'pages', 'Bad.vue'),
      [
        '<script setup lang="ts">',
        'const equity = seed + realized',
        'const net = cash - seed',
        'const a = money(cash)',
        'const b = equity ?? 0',
        'const broken = cash !== seed + net',
        '</script>',
        '<template><span>{{ equity }}</span></template>',
      ].join('\n'),
    )
    // Documentation and comments legitimately quote the arithmetic.
    writeFileSync(
      join(dir, 'src', 'pages', 'GoodDoc.vue'),
      [
        '<script setup lang="ts">',
        '// the old code was equity = seed + net and it was wrong',
        '/* money(cash) used to stand in for equity */',
        '</script>',
        '<template>',
        '<!-- never compose seed + net -->',
        '<span>{{ equityHeadline(equity) }}</span>',
        '</template>',
      ].join('\n'),
    )
    const got = scan(dir)
    const rules = new Set(got.map((h) => h.rule))
    for (const expect of ['seed-plus-net', 'cash-minus-seed', 'cash-as-equity', 'equity-null-coalesce']) {
      assert.ok(rules.has(expect), `scanner missed ${expect}; got ${[...rules].join(', ') || '(none)'}`)
    }
    assert.equal(
      got.filter((h) => h.file.includes('GoodDoc')).length,
      0,
      'the scanner must not flag its own documentation',
    )
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

check('panel store refreshAccounts forwards AccountView.equity verbatim', () => {
  const panelSrc = readFileSync(join(WEBUI, 'src', 'stores', 'panel.ts'), 'utf8')
  assert.ok(
    /equity:\s*a\.equity\b/.test(panelSrc),
    'stores/panel.ts refreshAccounts() must forward `equity: a.equity` into PanelAccountRow, otherwise Overview/HFT render 「—」',
  )
})

console.log(`\nRESULT: ${failures === 0 ? 'PASS' : `FAIL (${failures})`}`)
process.exit(failures === 0 ? 0 : 1)
