#!/usr/bin/env node
// Market-section single source of truth: every clock / price / book / bar
// reading comes from the kernel's per-account currentRound. No component
// in the market section may compute a duration or a round id of its own,
// and none may fall back to the process-default 900.
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  selectCurrentRound,
  currentRoundSpanSec,
  currentRoundBarFraction,
  currentRoundLeftSec,
  currentRoundAgeSec,
} from '../src/lib/current-round.ts'

const here = dirname(fileURLToPath(import.meta.url))
const read = (...parts) => readFileSync(join(here, '..', ...parts), 'utf8')

const checks = []
function check(name, fn) {
  checks.push({ name, fn })
}

const fiveM = {
  accountId: 'default',
  venue: 'polymarket',
  durationSec: 300,
  slot: 5_984_146,
  roundId: 1_795_243_800,
  startMs: 1_795_243_800_000,
  endMs: 1_795_244_100_000,
  ageSec: 60,
  timeLeftSec: 240,
  canTrade: true,
  prices: [
    { asset: 'BTC', venue: 'polymarket', durationSec: 300, up: 0.61, down: 0.39 },
    { asset: 'ETH', venue: 'polymarket', durationSec: 300, up: 0.44, down: 0.56 },
  ],
  books: [
    { asset: 'BTC', venue: 'polymarket', durationSec: 300, up: {}, down: {} },
    { asset: 'ETH', venue: 'polymarket', durationSec: 300, up: {}, down: {} },
  ],
}

check('selectCurrentRound returns only the named account', () => {
  const snap = { currentRounds: [fiveM, { ...fiveM, accountId: 'paper', durationSec: 900 }] }
  const cur = selectCurrentRound(snap, 'default')
  assert.equal(cur?.durationSec, 300)
  assert.equal(selectCurrentRound(snap, 'missing'), null)
  assert.equal(selectCurrentRound({ currentRounds: [] }, 'default'), null)
})

check('5m-anchored bar fraction is left/300, never left/900', () => {
  const frac = currentRoundBarFraction(fiveM, 240)
  assert.equal(frac, 240 / 300)
  assert.notEqual(frac, 240 / 900)
  assert.equal(currentRoundSpanSec(fiveM), 300)
})

check('missing start/end falls back to the object durationSec (account anchor), not a page-level 900', () => {
  const cur = { ...fiveM, startMs: 0, endMs: 0, ageSec: 0, timeLeftSec: 0, durationSec: 300 }
  assert.equal(currentRoundSpanSec(cur), 300)
  assert.equal(currentRoundBarFraction(cur, 240), 240 / 300)
})

check('daily observed 23h span wins over the 86400 family label', () => {
  const daily = {
    ...fiveM,
    durationSec: 86400,
    startMs: 1_000_000,
    endMs: 1_000_000 + 82_800 * 1000,
    ageSec: 3_600,
    timeLeftSec: 79_200,
  }
  assert.equal(currentRoundSpanSec(daily), 82_800)
  assert.equal(currentRoundBarFraction(daily, 79_200), 79_200 / 82_800)
})

check('left and age follow startMs/endMs', () => {
  const now = fiveM.startMs + 60_000
  assert.equal(currentRoundLeftSec(fiveM, now), 240)
  assert.equal(currentRoundAgeSec(fiveM, now), 60)
})

check('5m-anchored currentRound carries exactly one price card per asset', () => {
  const assets = fiveM.prices.map((p) => p.asset)
  assert.equal(new Set(assets).size, assets.length)
  assert.ok(fiveM.prices.every((p) => p.durationSec === 300))
})

const hft = read('src', 'pages', 'HftPage.vue')
const grid = read('src', 'components', 'hft', 'PriceGrid.vue')
const depth = read('src', 'components', 'hft', 'DepthCard.vue')

check('HftPage market section reads selectCurrentRound / currentRound only', () => {
  assert.match(hft, /selectCurrentRound/, '必须用 selectCurrentRound 取账户当前轮')
  assert.match(hft, /const currentRound = computed/, '必须有 currentRound computed')
  assert.match(hft, /currentRoundBarFraction/, '进度条必须来自 currentRound 窗口')
  assert.match(hft, /currentRound\.value\?\.prices/, '价卡必须来自 currentRound.prices')
  assert.match(hft, /currentRound\.value\?\.books/, '盘口必须来自 currentRound.books')
  assert.match(hft, /currentRound\.value\?\.roundId/, '轮次号必须来自 currentRound.roundId，不得本地 slot×duration')
  assert.match(hft, /currentRoundLeftSec/, '倒计时必须按 currentRound 的 endMs 走')
  assert.doesNotMatch(
    hft,
    /const leftFraction = computed\(\(\) => \{[\s\S]*?round\.value\?\.ageSec/,
    '进度条不得再读全局 round.ageSec',
  )
  assert.doesNotMatch(hft, /venueRoundId\(/, '行情区不得再本地计算 venueRoundId')
})

check('market-section components do not invent a 900 duration or a round id', () => {
  for (const [name, src] of [['PriceGrid', grid], ['DepthCard', depth]]) {
    assert.doesNotMatch(src, /effectiveSec/, `${name} 不得读进程默认 effectiveSec 当轮长`)
    assert.doesNotMatch(src, /venueRoundId\(/, `${name} 不得本地计算 roundId`)
    assert.doesNotMatch(src, /\?\? 900\b/, `${name} 不得 ?? 900 回落轮长`)
  }
})

const store = read('src', 'stores', 'panel.ts')
const app = read('src', 'App.vue')
const sse = read('src', 'lib', 'sse.ts')

check('00:00 self-heal refetches the snapshot and reconnects the live stream', () => {
  assert.match(hft, /stepCountdownSelfHeal/, 'HftPage 必须跑 00:00 自愈步进')
  assert.match(hft, /store\.refresh\(\)/, '自愈必须重新拉取快照（含 currentRounds）')
  assert.match(hft, /reconnectLive/, '自愈必须重连实时流')
  assert.match(hft, /healMsg/, '自愈尝试必须在 UI 上可见，不得静默')
  assert.doesNotMatch(hft, /location\.reload/, '不得 location.reload 以免刷新死循环')
  assert.match(store, /function reconnectLive/, 'store 必须提供 reconnectLive')
  assert.match(store, /liveGeneration/, 'store 必须暴露 liveGeneration')
  assert.match(app, /liveGeneration/, 'App.vue 必须监听 liveGeneration 并中止旧 SSE')
  assert.match(sse, /signal\?: AbortSignal/, 'readEventStream 必须接受 AbortSignal')
})

let failed = 0
for (const c of checks) {
  try {
    c.fn()
    console.log(`  ok  ${c.name}`)
  } catch (err) {
    failed += 1
    console.error(`  FAIL  ${c.name}`)
    console.error(err)
  }
}
if (failed) {
  console.error(`${failed} check(s) failed`)
  process.exit(1)
}
console.log(`current-round: ${checks.length} checks passed`)
