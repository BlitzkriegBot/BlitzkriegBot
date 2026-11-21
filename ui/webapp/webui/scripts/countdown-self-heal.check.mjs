/**
 * Pins the countdown-card progress bar to the account's anchored family
 * (not the process-default 15m) and the 00:00 self-heal (refetch +
 * reconnect, backoff, no reload loop, surfaced in the UI, no invented clock).
 *
 *   cd ui/webapp/webui && npm run check:countdown
 */
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..')
const read = (...parts) => readFileSync(join(ROOT, ...parts), 'utf8')

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

const {
  countdownBarFraction,
  stepCountdownSelfHeal,
  emptyHealState,
  ZERO_STALL_MS,
  HEAL_MAX_ATTEMPTS,
} = await import('../src/lib/countdown-self-heal.ts')

check('5m-anchored bar fraction is left/300, never left/900', () => {
  // age=60 into a 5m window, 240s remain. A 15m denominator would be 240/900.
  const frac = countdownBarFraction({
    leftSec: 240,
    familyAgeSec: 60,
    familyLeftSec: 240,
    identityDurSec: 300,
  })
  assert.equal(frac, 240 / 300)
  assert.notEqual(frac, 240 / 900)
})

check('when the family span is missing, the account anchor (300) is the denominator', () => {
  const frac = countdownBarFraction({
    leftSec: 150,
    familyAgeSec: null,
    familyLeftSec: null,
    identityDurSec: 300,
  })
  assert.equal(frac, 150 / 300)
})

check('daily prefers the observed start/end span over the nominal 86400', () => {
  // 23h DST day: 82800s observed.
  const frac = countdownBarFraction({
    leftSec: 41400,
    familyAgeSec: 41400,
    familyLeftSec: 41400,
    identityDurSec: 86400,
  })
  assert.equal(frac, 41400 / 82800)
})

check('00:00 arms, heals after the stall window, then backs off, then gives up', () => {
  let state = emptyHealState()
  let out = stepCountdownSelfHeal({ leftSec: 0, frozen: false, nowMs: 1_000, state })
  assert.equal(out.action, 'arm')
  state = out.state
  out = stepCountdownSelfHeal({ leftSec: 0, frozen: false, nowMs: 1_000 + ZERO_STALL_MS - 1, state })
  assert.equal(out.action, 'wait')
  out = stepCountdownSelfHeal({ leftSec: 0, frozen: false, nowMs: 1_000 + ZERO_STALL_MS, state })
  assert.equal(out.action, 'heal')
  assert.match(out.message ?? '', /重新拉取快照并重连实时流/)
  state = out.state
  const soon = state.lastHealAtMs + 100
  out = stepCountdownSelfHeal({ leftSec: 0, frozen: false, nowMs: soon, state })
  assert.equal(out.action, 'wait')
  // leave frozen / advancing clocks idle the counter
  out = stepCountdownSelfHeal({ leftSec: 12, frozen: false, nowMs: soon + 1, state })
  assert.equal(out.action, 'idle')
  assert.equal(out.state.attempts, 0)
  out = stepCountdownSelfHeal({ leftSec: 0, frozen: true, nowMs: soon + 1, state: emptyHealState() })
  assert.equal(out.action, 'idle')
  // exhaust attempts
  state = { stalledSinceMs: 0, attempts: HEAL_MAX_ATTEMPTS, lastHealAtMs: 0 }
  out = stepCountdownSelfHeal({ leftSec: 0, frozen: false, nowMs: ZERO_STALL_MS + 60_000, state })
  assert.equal(out.action, 'give-up')
  assert.match(out.message ?? '', /请手动刷新页面/)
})

const hft = read('src', 'pages', 'HftPage.vue')
const app = read('src', 'App.vue')
const store = read('src', 'stores', 'panel.ts')
const sse = read('src', 'lib', 'sse.ts')

check('progress bar uses the family span / identityDurSec, not the global 15m round scalar', () => {
  const block = hft.match(/const leftFraction = computed\(\(\) =>[^\n]+\)/)
    ?? hft.match(/const leftFraction = computed\(\(\) => \{[\s\S]*?\n\}\)/)
  assert.ok(block, '缺 leftFraction computed')
  assert.match(block[0], /currentRoundBarFraction/, 'leftFraction 必须走 currentRoundBarFraction（账户当前轮窗口）')
  assert.match(block[0], /currentRound/, '进度条必须读 currentRound，不得自行拼 duration')
  assert.doesNotMatch(
    block[0],
    /round\.value\?\.ageSec/,
    'leftFraction 仍用全局 round.ageSec+timeLeftSec —— 5m 账户会按 15m 长度走进度条',
  )
})

check('a new venueRound id rebinds the local interpolation (same-family roll must not stick at 00:00)', () => {
  const snapWatch = hft.match(/watch\(snap, \(s\) => \{[\s\S]*?\}, \{ immediate: true \}\)/)
  assert.ok(snapWatch, '缺 watch(snap)')
  assert.match(snapWatch[0], /countForRound/, '同一档换轮（roundId 前进）必须重绑插值锚点，不能把新轮 300s 当成回弹丢掉')
})

check('00:00 self-heal refetches the snapshot, reconnects the live stream, and is visible', () => {
  assert.match(hft, /stepCountdownSelfHeal/, '倒计时卡必须接入 00:00 自愈策略')
  assert.match(hft, /store\.refresh\(/, '自愈必须重新拉取快照')
  assert.match(hft, /reconnectLive/, '自愈必须重连实时流')
  assert.match(hft, /healMsg/, '自愈必须在 UI 上标明，不能静默')
  assert.doesNotMatch(hft, /location\.reload/, '禁止 location.reload 死循环')
  assert.match(store, /function reconnectLive/, 'store 必须提供 reconnectLive')
  assert.match(app, /liveGeneration/, 'App 壳层必须监视 liveGeneration 以中止并重连 SSE')
  assert.match(sse, /signal\?: AbortSignal/, 'readEventStream 必须接受 AbortSignal，才能掐掉僵死的长连接')
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)
