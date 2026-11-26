/**
 * 生效风控卡 — 傻白甜单真源结构门禁（RFC #241/#242/#243）：
 *   1. 主视图仅包含 3 组核心分组（sizing / positions / protection）；
 *   2. 不再同时渲染两套下单比例输入（BK_SIZE_PCT vs 下注比例 ratio）；
 *   3. 下注比例带有实时单笔金额估算预览（liveBudgetPreview）；
 *   4. 低频/专家项与执行策略规则折叠在默认收起的「高级设置」抽屉（advancedOpen = ref(false)）。
 *
 *   cd ui/webapp/webui && npm run check:risk-ssot
 */
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const webuiRoot = join(here, '..')
const read = (...p) => readFileSync(join(webuiRoot, ...p), 'utf8')

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

console.log('生效风控卡 — 单真源与 3 组主视图门禁 (#242/#243)')

const risk = read('src', 'pages', 'settings', 'SettingsRisk.vue')
const limits = read('src', 'pages', 'settings', 'SettingsRiskLimits.vue')

check('主视图严格包含 3 组核心分组（sizing / positions / protection）', () => {
  const groups = [...limits.matchAll(/data-risk-group="([^"]+)"/g)].map((m) => m[1])
  assert.deepEqual(groups, ['sizing', 'positions', 'protection'])
})

check('不存在第二套重复下单比例输入框（editBudgetRatio 不再直接渲染）', () => {
  assert.doesNotMatch(risk, /v-model="editBudgetRatio"/, '主视图/插槽不得再渲染重复的 editBudgetRatio 输入框')
  assert.doesNotMatch(limits, /BK_SIZE_PCT/, '主视图不再暴露晦涩内部环境变量名 BK_SIZE_PCT')
})

check('下注比例具备实时 USD 估算预览', () => {
  assert.match(limits, /const liveBudgetPreview = computed/, '缺少 liveBudgetPreview 实时估算')
  assert.match(limits, /\{\{\s*liveBudgetPreview\s*\}\}/, '模板未渲染 liveBudgetPreview')
})

check('高级设置抽屉默认收起且承载低频参数与规则插槽', () => {
  assert.match(limits, /const advancedOpen = ref\(false\)/, '高级设置抽屉必须默认收起')
  assert.match(limits, /data-risk-drawer="advanced"/, '缺少高级设置抽屉容器')
  const drawerIdx = limits.indexOf('data-risk-drawer="advanced"')
  const rulesIdx = limits.indexOf('<slot name="rules"')
  assert.ok(drawerIdx > 0 && rulesIdx > drawerIdx, '执行策略规则插槽必须位于高级设置抽屉内')
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures ? 1 : 0)
