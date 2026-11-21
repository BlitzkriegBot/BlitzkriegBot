#!/usr/bin/env node
/**
 * check:single-source — ratchet gate for the frontend single-source pattern (#129).
 *
 * Kernel state already has one write-up path on the panel: the Pinia store
 * fed by App.vue's 15s snapshot poll and the SSE change stream. Pages must
 * read that store. This gate fails on NEW copies of two patterns that caused
 * divergence:
 *
 *   [api-snapshot]  api.snapshot() outside src/stores/ — the store owns the
 *                   snapshot fetch. A page that pulls its own snapshot can
 *                   show a different generation than the rest of the shell.
 *   [poll-kernel]   useIntervalFn(...) / setInterval(...) whose callback
 *                   (or the named function it calls in the same file) hits
 *                   api.* or store.refresh(). Only the App.vue shell poll
 *                   is allowed to refresh kernel state on a timer.
 *
 * Local clock ticks (Date.now / nowMs) and one-shot user commands are fine.
 * Existing hits are snapshotted in single-source-baseline.txt; the baseline
 * is debt, not a licence, and may only shrink (--write-baseline).
 *
 * Usage:
 *   node scripts/single-source.check.mjs
 *   node scripts/single-source.check.mjs --self-test
 *   node scripts/single-source.check.mjs --write-baseline
 *
 * Exit: 0 pass / 1 violations / 2 usage.
 */

import { readFileSync, writeFileSync, readdirSync, statSync, mkdtempSync, rmSync, mkdirSync } from 'fs'
import { dirname, join, relative, resolve } from 'path'
import { fileURLToPath } from 'url'
import { tmpdir } from 'os'

const HERE = dirname(fileURLToPath(import.meta.url))
const WEBUI = resolve(HERE, '..')
const SRC = resolve(WEBUI, 'src')
const BASELINE = resolve(HERE, 'single-source-baseline.txt')

const ARGS = new Set(process.argv.slice(2))
const SELF_TEST = ARGS.has('--self-test')
const WRITE_BASELINE = ARGS.has('--write-baseline')

const SOURCE_EXT = new Set(['.vue', '.ts', '.js', '.mjs'])

/** Paths relative to webui/, or exact file names, allowed to poll-kernel. */
const POLL_KERNEL_ALLOW = [
  { path: 'src/App.vue', why: 'shell 15s store.refresh + SSE companion — the one official snapshot cadence' },
  { path: 'src/stores/', why: 'store-owned fetch / cadence is the single write-up path' },
]

function isAllowedPoll(rel) {
  return POLL_KERNEL_ALLOW.some((a) => rel === a.path || rel.startsWith(a.path))
}

function isStorePath(rel) {
  return rel === 'src/stores' || rel.startsWith('src/stores/')
}

/** api.snapshot() in the HTTP client or the store is the transport, not a second source. */
function isApiSnapshotAllow(rel) {
  return isStorePath(rel) || rel === 'src/api' || rel.startsWith('src/api/')
}

function listSourceFiles(root) {
  const out = []
  const walk = (dir) => {
    for (const name of readdirSync(dir)) {
      if (name === 'node_modules' || name === 'dist' || name === '.mimosa') continue
      const p = join(dir, name)
      const st = statSync(p)
      if (st.isDirectory()) walk(p)
      else if (SOURCE_EXT.has(name.slice(name.lastIndexOf('.')))) out.push(p)
    }
  }
  walk(root)
  return out.sort()
}

function stripLineComment(line) {
  // Keep strings intact enough for our regexes; only drop // comments outside
  // of quotes, and ignore HTML/block comment lines we already skip.
  let out = ''
  let quote = null
  for (let i = 0; i < line.length; i++) {
    const c = line[i]
    if (quote) {
      out += c
      if (c === '\\' && i + 1 < line.length) {
        out += line[++i]
        continue
      }
      if (c === quote) quote = null
      continue
    }
    if (c === '"' || c === "'" || c === '`') {
      quote = c
      out += c
      continue
    }
    if (c === '/' && line[i + 1] === '/') break
    out += c
  }
  return out
}

function relOf(abs, root) {
  return relative(root, abs).split('\\').join('/')
}

/**
 * Collect [api-snapshot] / [poll-kernel] hits in one file.
 * Lines are 1-based. rel is webui-relative (src/...).
 */
function scanFile(abs, root) {
  const rel = relOf(abs, root)
  const text = readFileSync(abs, 'utf8')
  const lines = text.split(/\r?\n/)
  const hits = []

  if (!isApiSnapshotAllow(rel)) {
    lines.forEach((raw, idx) => {
      const line = stripLineComment(raw)
      if (/\bapi\.snapshot\s*\(/.test(line)) {
        hits.push({ file: rel, line: idx + 1, rule: 'api-snapshot', why: 'api.snapshot() outside src/stores/ — snapshot fetch belongs to the store' })
      }
    })
  }

  if (!isAllowedPoll(rel)) {
    const callRe = /(?:window\.)?setInterval\s*\(|useIntervalFn\s*\(/g
    let m
    while ((m = callRe.exec(text)) !== null) {
      const start = m.index
      const lineNo = text.slice(0, start).split(/\r?\n/).length
      const firstArg = firstArgText(text, start + m[0].length)
      if (firstArg == null) continue
      const targets = callTargets(firstArg)
      const reachesKernel = targets.some((t) => fnReachesKernel(text, t))
      if (reachesKernel) {
        hits.push({
          file: rel,
          line: lineNo,
          rule: 'poll-kernel',
          why: 'timer callback reaches api.* or store.refresh() — kernel refresh on a timer belongs to the App.vue shell (or the store)',
        })
      }
    }
  }

  return hits
}

/** Text of the first argument starting at `at` (immediately after the '(' of the call). */
function firstArgText(text, at) {
  let i = at
  while (i < text.length && /\s/.test(text[i])) i++
  if (i >= text.length) return null
  const start = i
  let depth = 0
  let quote = null
  for (; i < text.length; i++) {
    const c = text[i]
    if (quote) {
      if (c === '\\') {
        i++
        continue
      }
      if (c === quote) quote = null
      continue
    }
    if (c === '"' || c === "'" || c === '`') {
      quote = c
      continue
    }
    if (c === '(' || c === '{' || c === '[') depth++
    else if (c === ')' || c === '}' || c === ']') {
      if (depth === 0) return text.slice(start, i)
      depth--
    } else if (c === ',' && depth === 0) {
      return text.slice(start, i)
    }
  }
  return text.slice(start, i)
}

function callTargets(arg) {
  const trimmed = arg.trim()
  // Inline function / arrow: the body is the target (scan as a synthetic fn).
  if (
    /^(?:async\s+)?(?:function\b|\()/.test(trimmed) ||
    /^(?:async\s+)?[A-Za-z_$][\w$]*\s*=>/.test(trimmed) ||
    /^(?:async\s+)?\([^)]*\)\s*=>/.test(trimmed)
  ) {
    return [{ kind: 'inline', source: trimmed }]
  }
  const name = trimmed.match(/^[A-Za-z_$][\w$]*$/)
  if (name) return [{ kind: 'name', name: name[0] }]
  // () => foo() already caught; fallback: scan the arg itself.
  return [{ kind: 'inline', source: trimmed }]
}

const BUILTIN_CALLS = new Set([
  'if', 'for', 'while', 'switch', 'catch', 'function', 'return', 'await', 'void',
  'Math', 'Date', 'Number', 'String', 'Boolean', 'Array', 'Object', 'JSON', 'Promise',
  'setTimeout', 'setInterval', 'clearTimeout', 'clearInterval', 'useIntervalFn',
  'useNow', 'useIntersectionObserver', 'console', 'parseInt', 'parseFloat',
  'isNaN', 'isFinite', 'encodeURIComponent', 'decodeURIComponent',
])

function fnReachesKernel(fileText, target, depth = 0) {
  const source = target.kind === 'inline' ? target.source : namedFnSource(fileText, target.name)
  if (!source) return false
  // #124 countdown self-heal: a local clock that may store.refresh() only when
  // the countdown is stuck. That is not a page-owned snapshot poll.
  if (/\bstepCountdownSelfHeal\s*\(/.test(source)) return false
  if (/\bapi\.\w+\s*\(/.test(source) || /\bstore\.refresh\s*\(/.test(source)) return true
  if (depth >= 2) return false
  const names = new Set()
  const re = /\b([A-Za-z_$][\w$]*)\s*\(/g
  let m
  while ((m = re.exec(source)) !== null) {
    if (!BUILTIN_CALLS.has(m[1])) names.add(m[1])
  }
  for (const name of names) {
    if (fnReachesKernel(fileText, { kind: 'name', name }, depth + 1)) return true
  }
  return false
}

function namedFnSource(fileText, name) {
  const patterns = [
    new RegExp(String.raw`(?:async\s+)?function\s+${name}\s*\(`),
    new RegExp(String.raw`(?:const|let|var)\s+${name}\s*=\s*(?:async\s+)?(?:function\s*\(|\([^)]*\)\s*=>|[A-Za-z_$][\w$]*\s*=>)`),
  ]
  for (const re of patterns) {
    const m = re.exec(fileText)
    if (!m) continue
    // From the match to a reasonable stop: next top-level function or end.
    const start = m.index
    const fromParen = fileText.indexOf('{', start)
    const fromArrow = fileText.indexOf('=>', start)
    let bodyStart = start
    if (fromParen >= 0 && (fromArrow < 0 || fromParen < fromArrow + 2)) {
      const extracted = braceBlock(fileText, fromParen)
      if (extracted) return extracted
    }
    return fileText.slice(start, start + 4000)
  }
  return null
}

function braceBlock(text, openIdx) {
  if (text[openIdx] !== '{') return null
  let depth = 0
  let quote = null
  for (let i = openIdx; i < text.length; i++) {
    const c = text[i]
    if (quote) {
      if (c === '\\') {
        i++
        continue
      }
      if (c === quote) quote = null
      continue
    }
    if (c === '"' || c === "'" || c === '`') {
      quote = c
      continue
    }
    if (c === '{') depth++
    else if (c === '}') {
      depth--
      if (depth === 0) return text.slice(openIdx, i + 1)
    }
  }
  return text.slice(openIdx)
}

function formatHit(h) {
  return `${h.file}:${h.line}:${h.rule}`
}

function loadBaseline(path) {
  if (SELF_TEST) return new Set()
  try {
    const text = readFileSync(path, 'utf8')
    return new Set(
      text
        .split(/\r?\n/)
        .map((l) => l.trim())
        .filter((l) => l && !l.startsWith('#')),
    )
  } catch (err) {
    if (err && err.code === 'ENOENT') return new Set()
    throw err
  }
}

function writeBaseline(path, hits) {
  const lines = [
    '# single-source baseline — existing frontend SSOT debt (#129).',
    '# One hit per line: <src-relative path>:<line>:<rule>',
    '# This file is debt, not a licence. New hits fail the gate; retire a line when the page is converted.',
    ...hits.map(formatHit),
    '',
  ]
  writeFileSync(path, lines.join('\n'))
}

function compare(hits, baseline) {
  const now = new Set(hits.map(formatHit))
  const added = [...now].filter((x) => !baseline.has(x)).sort()
  const retired = [...baseline].filter((x) => !now.has(x)).sort()
  return { added, retired, now }
}

function runScan(root) {
  const files = listSourceFiles(join(root, 'src'))
  const hits = []
  for (const f of files) hits.push(...scanFile(f, root))
  hits.sort((a, b) => a.file.localeCompare(b.file) || a.line - b.line || a.rule.localeCompare(b.rule))
  return hits
}

function selfTest() {
  const dir = mkdtempSync(join(tmpdir(), 'single-source-'))
  try {
    const src = join(dir, 'src')
    const pages = join(src, 'pages')
    const stores = join(src, 'stores')
    mkdirSync(pages, { recursive: true })
    mkdirSync(stores, { recursive: true })
    writeFileSync(
      join(src, 'App.vue'),
      `import { useIntervalFn } from '@vueuse/core'\nconst { pause } = useIntervalFn(() => { void store.refresh() }, 15_000)\n`,
    )
    writeFileSync(join(stores, 'panel.ts'), `export async function refresh() { await api.snapshot() }\n`)
    writeFileSync(join(pages, 'GoodClock.vue'), `useIntervalFn(() => { nowMs.value = Date.now() }, 2000)\n`)
    writeFileSync(
      join(pages, 'BadSnapshot.vue'),
      `async function confirm() {\n  const snap = await api.snapshot()\n}\n`,
    )
    writeFileSync(
      join(pages, 'BadNamedPoll.vue'),
      `async function refresh() {\n  const doc = await api.intentAuditTail({ limit: 200 })\n}\nuseIntervalFn(refresh, 4000)\n`,
    )
    writeFileSync(
      join(pages, 'BadInlinePoll.vue'),
      `useIntervalFn(() => {\n  if (settings.fastPoll) void store.refresh()\n}, 2_000)\n`,
    )
    writeFileSync(
      join(pages, 'BadIndirectPoll.vue'),
      `async function refresh() {\n  const doc = await api.klineHistory({ symbol: 'BTC' })\n}\nuseIntervalFn(() => {\n  void refresh()\n}, 2000)\n`,
    )
    writeFileSync(
      join(pages, 'GoodSelfHeal.vue'),
      `useIntervalFn(() => {\n  const step = stepCountdownSelfHeal({ leftSec: 0 })\n  if (step.action === 'heal') void store.refresh()\n}, 500)\n`,
    )
    mkdirSync(join(src, 'api'), { recursive: true })
    writeFileSync(
      join(src, 'api', 'client.ts'),
      `export async function probeSession() { await api.snapshot() }\n`,
    )
    const hits = runScan(dir)
    const keys = hits.map(formatHit)
    const expect = [
      'src/pages/BadIndirectPoll.vue:4:poll-kernel',
      'src/pages/BadInlinePoll.vue:1:poll-kernel',
      'src/pages/BadNamedPoll.vue:4:poll-kernel',
      'src/pages/BadSnapshot.vue:2:api-snapshot',
    ]
    const missing = expect.filter((e) => !keys.includes(e))
    const unexpected = keys.filter((k) => !expect.includes(k))
    if (missing.length || unexpected.length) {
      console.error('self-test failed')
      if (missing.length) console.error('  missing:', missing.join(', '))
      if (unexpected.length) console.error('  unexpected:', unexpected.join(', '))
      console.error('  got:', keys.join(', ') || '(none)')
      process.exit(1)
    }
    console.log('single-source: self-test passed')
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
}

if (SELF_TEST) {
  selfTest()
  process.exit(0)
}

const hits = runScan(WEBUI)
if (WRITE_BASELINE) {
  writeBaseline(BASELINE, hits)
  console.log(`wrote ${hits.length} hit(s) to ${relative(WEBUI, BASELINE) || 'scripts/single-source-baseline.txt'}`)
  for (const h of hits) console.log(`  ${formatHit(h)}  ${h.why}`)
  process.exit(0)
}

const baseline = loadBaseline(BASELINE)
const { added, retired } = compare(hits, baseline)

if (added.length) {
  console.error('single-source: NEW violations (not in the baseline):')
  for (const a of added) {
    const h = hits.find((x) => formatHit(x) === a)
    console.error(`  ${a}${h ? '  ' + h.why : ''}`)
  }
}
if (retired.length) {
  console.error('single-source: baseline entries no longer present (shrink the baseline):')
  for (const r of retired) console.error(`  ${r}`)
  console.error('  re-run with --write-baseline after confirming the page was converted')
}
if (added.length || retired.length) {
  console.error(`${added.length} new, ${retired.length} retired; ${hits.length} current / ${baseline.size} baseline`)
  process.exit(1)
}

console.log(`single-source: ${hits.length} known-debt hit(s), no new violations`)
for (const h of hits) console.log(`  ${formatHit(h)}`)
