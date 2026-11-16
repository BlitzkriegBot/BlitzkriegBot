/**
 * 回执文案 — the exit-config change-receipt text gate (issue 1199 E/T4,
 * pins P1-4).
 *
 * SettingsRiskLimits.vue 的 changeText 是风控限额改动回执的唯一文案来源
 * （模板 `{{ changeText(c) }}`）。审计 §12.3：check:all 25+ 套件无一渲染或
 * 断言该回执文案，P1-4（布尔/数字两分支互换）修复后无回归保护。
 *
 * P1-4 的缺陷形态：布尔行（trailingEnabled / strategyExitMakerFirst）的
 * from/to 是 'true'/'false' 原文，数字行的 '0' 语义是「关闭」——两分支一旦
 * 互换，布尔回执渲染成乱码、数字回执丢失「关闭」语义与单位。本套件钉住：
 *   · 布尔分支：from/to 逐侧 'true' → '开'，否则 '关'；
 *   · 数字分支：fieldUnit(c.field) 取单位，'0' → '关闭'，其余真实值 + 单位；
 *   · 两分支不互换（if 条件肯定形式 + 各分支内容互斥）；
 *   · EXIT_BOOL_FIELDS 仍包含 trailingEnabled 与 strategyExitMakerFirst；
 *   · 回执模板确实渲染 changeText(c)（文案路径是活线，不是死代码）。
 *
 *   cd ui/webapp/webui && npm run check:receipt-text
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

console.log('风控限额回执文案 — changeText 布尔/数字分支 (issue 1199 E/T4, P1-4)')

const page = read('src', 'pages', 'settings', 'SettingsRiskLimits.vue')

// changeText 整函数（函数体内右括号全部缩进，非贪婪到第一个行首 `}` 即函数尾）。
const fn = page.match(/function changeText\([\s\S]*?\n\}/)
assert.ok(fn, 'SettingsRiskLimits.vue 缺 changeText 函数 —— 回执文案来源被移除?')

// 分支提取：if 块 = 布尔分支；if 块之后到函数尾 = 数字分支。
// 「不互换」类断言在剥离 // 注释后的代码文本上做——注释里复述口径是允许的，
// 断言只看可执行代码（否则注释会同时命中两个口径的关键词）。
const boolM = fn[0].match(/if \(EXIT_BOOL_FIELDS\.has\(c\.field\)\) \{([\s\S]*?)\n  \}/)
const stripComments = (s) => s.replace(/\/\/[^\n]*/g, '')
const boolBranch = boolM ? stripComments(boolM[1]) : ''
const numericBranch = boolM ? stripComments(fn[0].slice(boolM.index + boolM[0].length)) : ''

check('布尔分支：from/to 逐侧 === \'true\' → 开/关（P1-4 布尔口径）', () => {
  assert.ok(boolM, '缺 EXIT_BOOL_FIELDS 布尔分支 —— P1-4 修复被移除')
  assert.match(
    boolBranch,
    /c\.from === 'true' \? '开' : '关'/,
    'from 侧未按 \'true\' 映射 开/关',
  )
  assert.match(
    boolBranch,
    /c\.to === 'true' \? '开' : '关'/,
    'to 侧未按 \'true\' 映射 开/关',
  )
})

check('数字分支：fieldUnit 单位 + \'0\' → 关闭（P1-4 数字口径）', () => {
  assert.ok(boolM, '缺布尔分支，数字分支无从谈起')
  assert.match(numericBranch, /const u = fieldUnit\(c\.field\)/, '数字分支未取 fieldUnit(c.field) 单位')
  assert.match(numericBranch, /c\.from === '0' \? '关闭' : c\.from/, 'from 侧 \'0\' 未映射「关闭」')
  assert.match(numericBranch, /c\.to === '0' \? '关闭' : c\.to/, 'to 侧 \'0\' 未映射「关闭」')
  assert.match(numericBranch, /return `\$\{f\}\$\{u\} → \$\{t\}\$\{u\}`/, '单位未拼进回执两侧')
})

check('两分支不互换：布尔分支不碰 fieldUnit/关闭，数字分支不碰布尔口径', () => {
  assert.ok(boolM, '缺布尔分支')
  assert.doesNotMatch(boolBranch, /fieldUnit|关闭/, '布尔分支混入数字口径 —— P1-4 互换形态回归')
  assert.doesNotMatch(numericBranch, /=== 'true'| \? '开' : '关'/, '数字分支混入布尔口径 —— P1-4 互换形态回归')
  // if 条件必须是肯定形式；!EXIT_BOOL_FIELDS.has(...) 是分支互换的另一种形态。
  assert.match(fn[0], /if \(EXIT_BOOL_FIELDS\.has\(c\.field\)\)/, 'if 条件不是 EXIT_BOOL_FIELDS.has(c.field)')
  assert.doesNotMatch(fn[0], /if \(!EXIT_BOOL_FIELDS/, 'if 条件被取反 —— 分支互换的变体')
})

check('EXIT_BOOL_FIELDS 含 trailingEnabled 与 strategyExitMakerFirst（布尔行清单完整）', () => {
  const decl = page.match(/const EXIT_BOOL_FIELDS = new Set\(\[([^\]]*)\]\)/)
  assert.ok(decl, '缺 EXIT_BOOL_FIELDS 集合声明')
  for (const field of ['trailingEnabled', 'strategyExitMakerFirst']) {
    assert.ok(decl[1].includes(`'${field}'`), `EXIT_BOOL_FIELDS 缺 '${field}' —— 该行回执将按数字口径渲染`)
  }
})

check('回执模板确实渲染 changeText(c)（文案路径活线，非死代码）', () => {
  assert.match(page, /\{\{ changeText\(c\) \}\}/, '模板不再渲染 changeText(c) —— 回执文案路径断线')
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures ? 1 : 0)
