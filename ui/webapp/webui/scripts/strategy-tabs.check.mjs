/**
 * 策略与套利分离门禁 — the strategy-tabs / arbitrage-separation contract.
 *
 * 2026-11-27 重构后：跨所套利迁出策略页，成为「策略」导航组下与「策略」和
 * 「智能交易」平级的独立页面（ArbitragePage.vue）；Strategies.vue 不再渲染
 * 「单交易所策略 / 套利策略」二级子菜单，专注管理传统单交易所策略。
 *
 *   cd ui/webapp/webui && npm run check:strategy-tabs
 */
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const read = (...p) => readFileSync(join(here, '..', ...p), 'utf8')

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

console.log('策略与套利独立页面分离 — the strategy-tabs contract')

const page = read('src', 'pages', 'Strategies.vue')
const arbPage = read('src', 'pages', 'ArbitragePage.vue')
const nav = read('src', 'lib', 'nav.ts')
const client = read('src', 'api', 'snapshot.ts')

check('策略页已移除旧的「单交易所策略 / 套利策略」子菜单', () => {
  assert.doesNotMatch(page, /单交易所策略/, 'Strategies.vue 仍残留「单交易所策略」子菜单')
  assert.doesNotMatch(page, /label: '套利策略'/, 'Strategies.vue 仍残留「套利策略」子菜单')
  assert.doesNotMatch(page, /\bperfTab\b/, 'Strategies.vue 仍残留 perfTab 状态')
})

check('策略页过滤掉 cross_venue_arb（专注单交易所策略）', () => {
  assert.match(page, /const ARB_STRATEGY = 'cross_venue_arb'/, '缺 ARB_STRATEGY 常量')
  const m = page.match(/const singleRows = computed[\s\S]*?\n/)
  assert.ok(m, '缺 singleRows computed')
  assert.match(m[0], /ARB_STRATEGY/, 'singleRows 未过滤 ARB_STRATEGY')
})

check('独立套利页面 ArbitragePage.vue 与策略和智能交易平级注册在导航中', () => {
  assert.match(nav, /id: 'arbitrage', label: '套利'/, 'nav.ts 缺独立 arbitrage 页签')
  assert.match(arbPage, /api\.arbStatus\(\)/, 'ArbitragePage.vue 缺 arbStatus 实时状态读取')
  assert.match(arbPage, /api\.arbConfigClear/, 'ArbitragePage.vue 缺解除套利能力')
  assert.match(arbPage, /CreateArbDialog/, 'ArbitragePage.vue 缺双腿配置弹窗入口')
})

check('原因列：徽标与展开按钮同一条 flex 队列，不靠内容长度碰运气', () => {
  const cell = page.match(/<td class="px-2 py-2\.5 text-right">\s*<!-- 原因列[\s\S]*?<\/td>/)
  assert.ok(cell, '原因列 td 缺失')
  assert.match(cell[0], /flex flex-wrap items-center justify-end gap-x-1\.5/, '徽标与按钮没有排进同一条 flex 队列')
  assert.match(cell[0], /class="shrink-0"\s+variant="ghost"/, '展开按钮必须 shrink-0')
})

check('client.ts StrategyStatsRow 保留五个可选 arb 字段以兼容老内核读数', () => {
  for (const f of ['arbNetPnlUsd', 'arbWins', 'arbLosses', 'arbClosedTrades', 'arbActivePairs']) {
    assert.match(client, new RegExp(`${f}\\?:`), `StrategyStatsRow 缺可选字段 ${f}`)
  }
})

process.exit(failures ? 1 : 0)
