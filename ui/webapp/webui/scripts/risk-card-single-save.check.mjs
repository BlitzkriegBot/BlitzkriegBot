/**
 * 生效风控卡：参数与规则必须由【同一个保存按钮】【同一次整段提交】写入。
 *
 * 钉住的真实缺陷（2026-11-24 操作者实报）：
 *   历史上「生效风控」卡与「生效风控 · 执行策略规则」卡各有一个保存按钮。
 *   内核的 `execution_policy.set` 写的是【整个账户段】—— 六个键是该段的全部
 *   足迹，缺省字段按 TOML 段替换语义被清扫（core: execution_policy/env_state.rs
 *   section_env_state 的 None 墓碑）。于是：
 *     · 风控卡保存只发 4 个参数键 → POLICY_RULES* 被清扫 → 规则被清空；
 *     · 规则卡保存只发 rules      → 五个 POLICY_* 参数被清扫 → 参数被清空。
 *   两个按钮互斥，谁后点谁赢。这不是 UI 偏好问题，是账本/风控配置的静默丢失。
 *
 * 本套件钉住的是「不可能再拆回去」这个结构性事实：
 *   1. 设置页源码里 `executionPolicySet` 只出现一次（唯一提交点）；
 *   2. `buildSetParams` 的返回值无条件带 `rules`（即使规则为空数组）；
 *   3. 设置页不再 import 任何第二个执行策略卡组件；
 *   4. 规则编辑器只有 rollback 上抛，不再有 save 入口（保存已上移）；
 *   5. 模拟内核段替换语义：任意一次提交后，六项参数与规则全部保真。
 *
 *   cd ui/webapp/webui && npm run check:risk-save
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

console.log('生效风控卡 — 单保存按钮 / 整段提交（防两个保存按钮互相清扫）')

const risk = read('src', 'pages', 'settings', 'SettingsRisk.vue')
const limits = read('src', 'pages', 'settings', 'SettingsRiskLimits.vue')
const editor = read('src', 'pages', 'settings', 'PolicyRulesEditor.vue')

check('保存路径唯一：saveAll 是唯一 @save-all 处理者，回滚单独存在', () => {
  // executionPolicySet 有且只有两个合法调用点：saveAll（整段保存）与 rollback
  // （整段写回 before）。两者都是【整段】提交，不存在只发半段的路径。
  const calls = [...risk.matchAll(/api\.executionPolicySet\s*\(/g)]
  assert.equal(calls.length, 2, `executionPolicySet 调用点应为 2（saveAll + rollback），实际 ${calls.length}`)
  // 每个调用点的实参都必须是整段 params（由 buildSetParams 或 before 快照构造），
  // 不得出现内联半段字面量。
  assert.match(risk, /const policyParams = buildSetParams\(\)/, 'saveAll 必须用 buildSetParams 整段构造')
  assert.doesNotMatch(
    risk,
    /api\.executionPolicySet\(\s*\{\s*accountId:/,
    '不得内联半段字面量调用 executionPolicySet',
  )
})

check('buildSetParams 无条件带 rules（含空数组）', () => {
  const fn = risk.match(/function buildSetParams\(\).*?\n\}/s)
  assert.ok(fn, '缺 buildSetParams')
  assert.match(fn[0], /params\.rules\s*=\s*editRules\.value/, 'rules 必须无条件随段提交')
  // 不得出现「rules 只在有值时发」的条件包装 —— 那正是清扫另一端的形态。
  assert.doesNotMatch(
    fn[0],
    /if\s*\([^)]*\)\s*\{?\s*params\.rules/,
    'rules 不得被条件包裹：段替换语义下漏发等于清空',
  )
})

check('不存在第二个保存入口（saveRules / buildRuleSetParams 已删）', () => {
  assert.doesNotMatch(risk, /function saveRules\s*\(/, 'saveRules 必须已删除')
  assert.doesNotMatch(risk, /function buildRuleSetParams\s*\(/, 'buildRuleSetParams 必须已删除')
})

check('设置页不再 import 第二个执行策略卡组件', () => {
  const imports = [...risk.matchAll(/import\s+\w+\s+from\s+'\.\/(Settings\w+)\.vue'/g)].map((m) => m[1])
  assert.ok(!imports.includes('SettingsExecutionPolicy'), `仍有第二个卡片 import: ${imports.join(', ')}`)
})

check('规则编辑器不再上抛 save（保存已上移到唯一按钮）', () => {
  const emits = editor.match(/defineEmits<\{[\s\S]*?\}>\(\)/)
  assert.ok(emits, '缺 defineEmits')
  assert.doesNotMatch(emits[0], /\bsave\s*:/, '规则编辑器不得再有 save 事件')
  assert.match(emits[0], /\brollback\s*:/, '回滚必须保留（独立动作）')
  assert.doesNotMatch(editor, /<Save\b/, '规则编辑器不应再渲染保存图标/按钮')
})

check('风控卡只有唯一 @save-all 按钮，且 rules 插槽在卡内', () => {
  const buttons = [...limits.matchAll(/@click="\$emit\('save-all'\)"/g)]
  assert.equal(buttons.length, 1, `save-all 按钮应为 1，实际 ${buttons.length}`)
  assert.match(limits, /<slot name="rules"\s*\/>/, '规则段必须在同一张卡内')
})

check('模拟段替换语义：一次整段提交后六项参数 + 规则全部保真', () => {
  // 内核 section_env_state 的足迹：六个键，缺省 = None（清扫）。
  const FOOTPRINT = [
    'budgetRatio',
    'minBudgetUsd',
    'maxBudgetUsd',
    'minEquityUsd',
    'maxPositionsPerAsset',
    'rules',
  ]
  const sectionReplace = (prev, patch) => {
    const next = {}
    for (const k of FOOTPRINT) {
      const v = patch[k]
      next[k] = v === undefined ? null : v // 缺省 → 清扫
    }
    return next
  }
  const before = {
    budgetRatio: '0.02',
    minBudgetUsd: '10',
    maxBudgetUsd: '100',
    minEquityUsd: '50',
    maxPositionsPerAsset: 2,
    rules: [{ name: 'existing', priority: 10, enabled: true }],
  }
  // 用户只改了预算比例与规则，其余保持原值（buildSetParams 全量发出）。
  const params = {
    budgetRatio: '0.03',
    minBudgetUsd: '10',
    maxBudgetUsd: '100',
    minEquityUsd: '50',
    maxPositionsPerAsset: 2,
    rules: [{ name: 'new_rule', priority: 20, enabled: true }],
  }
  const after = sectionReplace(before, params)
  for (const k of FOOTPRINT) {
    assert.notEqual(after[k], null, `${k} 被清扫 —— 整段提交必须发全六项`)
  }
  assert.equal(after.budgetRatio, '0.03')
  assert.equal(after.rules[0].name, 'new_rule')
})

check('反例自证：半段提交确实会清扫另一半（钉住缺陷形态，不得「修好」这条）', () => {
  const FOOTPRINT = [
    'budgetRatio',
    'minBudgetUsd',
    'maxBudgetUsd',
    'minEquityUsd',
    'maxPositionsPerAsset',
    'rules',
  ]
  const sectionReplace = (patch) => {
    const next = {}
    for (const k of FOOTPRINT) next[k] = patch[k] === undefined ? null : patch[k]
    return next
  }
  // 旧形态 A：风控卡保存（只发参数）
  const onlyParams = sectionReplace({
    budgetRatio: '0.02',
    minBudgetUsd: '10',
    maxBudgetUsd: '100',
    minEquityUsd: '50',
    maxPositionsPerAsset: 2,
  })
  assert.equal(onlyParams.rules, null, '只发参数必须清空规则（这是被修的缺陷）')
  // 旧形态 B：规则卡保存（只发 rules）
  const onlyRules = sectionReplace({ rules: [{ name: 'r', priority: 10, enabled: true }] })
  for (const k of FOOTPRINT.slice(0, 5)) {
    assert.equal(onlyRules[k], null, `${k} 必须被清空（这是被修的缺陷）`)
  }
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures ? 1 : 0)
