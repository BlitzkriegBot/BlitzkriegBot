/**
 * 等宽数字（tabular figures）覆盖门禁 — issue #29。
 *
 * 等宽已不再是「记得加类」的约定，而是容器属性：
 * 容器挂 `.figures`（theme.css）→ 子孙继承 `font-variant-numeric:
 * tabular-nums`，数字按列对齐、数值变化不抖宽度。`format.ts` 家族
 *（num / money / pct / …）只返回字符串，不自带标记，所以「每个
 * 格式化插值都落在等宽容器内」由本门禁兜底，不靠评审。
 *
 * 断言：
 *   1. 模板里 `{{ num(...)|money(...)|...|toFixed(...) }}` 所在元素，或其
 *      模板内可静态解析的祖先，须带等宽容器类（`.figures` / `.num` /
 *      `.font-mono` / `.stat-num`）；`<RollingNumber>` 自身等宽，豁免。
 *   2. 全仓不得出现 `proportional-nums` / `oldstyle-nums` 覆盖。
 *
 * 修前（main）必须是红的，红的清单就是本单要修的点：
 *   cd ui/webapp/webui && npm run check:tabular
 */
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { FIGURES_CLASS } from '../src/lib/format.ts'

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
const read = (...p) => readFileSync(join(here, '..', ...p), 'utf8')

// 等宽容器类：.figures 是本单新增的容器级类，其余是既有类级契约。
const TABULAR_CLASSES = [FIGURES_CLASS, 'num', 'font-mono', 'stat-num']
const TABULAR_CLASS_RE = new RegExp(
  `(?:^|\\s)(?:${TABULAR_CLASSES.join('|')})(?:\\s|$)`,
)

/** 格式化家族的调用：经它输出的字符串必须等宽渲染。 */
const FORMAT_CALL_RE = /\b(?:num|money|signedMoney|pct|signedPct|cents|duration|mmss|clockTime|dateTime|toFixed)\(/

/** 禁用的比例数字声明。 */
const PROPORTIONAL_RE = /proportional-nums|oldstyle-nums/g

// 极简 HTML 标签走法（复用 round-header.check.mjs 的思路）：只识别标签边界
// 与 class 字面量，不解析动态绑定——动态类不可静态判定，按不可覆盖处理。
const TAG = /<(\/?)([A-Za-z][-\w.]*)((?:"[^"]*"|'[^']*'|[^>"'])*?)(\/?)>/g

function classOf(attrs) {
  return attrs.match(/\bclass="([^"]*)"/)?.[1] ?? ''
}

const { readdirSync, statSync } = await import('node:fs')

function collectVue(dir, out = []) {
  for (const name of readdirSync(dir)) {
    const full = join(dir, name)
    const stat = statSync(full)
    if (stat.isDirectory()) collectVue(full, out)
    else if (name.endsWith('.vue')) out.push(full)
  }
  return out
}

const lineOf = (src, index) => src.slice(0, index).split('\n').length

const srcRoot = join(here, '..', 'src')
const files = collectVue(srcRoot)
console.log(`扫描 Vue 组件：${files.length} 个`)

for (const file of files) {
  const src = readFileSync(file, 'utf8')
  // 剥注释：注释里写规范说明不应触发规则。
  // 剥注释，但保留换行，这样报出的行号仍可对准源文件。
  let clean = src
  let prev
  do {
    prev = clean
    clean = clean
      .replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, ' '))
      .replace(/<!--[\s\S]*?-->/g, (m) => m.replace(/[^\n]/g, ' '))
  } while (clean !== prev)

  const rel = file.slice(srcRoot.length + 1)
  const violations = []

  // 模板走栈：frame.covered = 该元素自身或其模板内祖先已提供等宽容器。
  const stack = []
  let covered = false
  let cursor = 0
  TAG.lastIndex = 0
  let m
  while ((m = TAG.exec(clean))) {
    // 文本块：上一个标签结束 → 此标签开始之间的文本/插值。
    const chunk = clean.slice(cursor, m.index)
    cursor = TAG.lastIndex
    if (!covered) {
      for (const mm of chunk.matchAll(/\{\{([\s\S]*?)\}\}/g)) {
        const expr = mm[1].trim()
        if (FORMAT_CALL_RE.test(expr)) {
          violations.push({
            file: rel,
            line: lineOf(clean, cursor + mm.index),
            expr,
            hint: '数字插值未落在等宽容器内（加 .figures 容器类，或包入 <RollingNumber>）',
          })
        }
      }
    }
    const [, closing, tag, attrs, selfClose] = m
    if (closing) {
      stack.pop()
      covered = stack.length > 0 ? stack[stack.length - 1].covered : false
    } else if (!selfClose) {
      const own =
        TABULAR_CLASS_RE.test(classOf(attrs)) || tag === 'RollingNumber'
      stack.push({ covered: own || covered })
      covered = own || covered
    }
  }
  // 尾部文本块。
  {
    const chunk = clean.slice(cursor)
    if (!covered) {
      for (const mm of chunk.matchAll(/\{\{([\s\S]*?)\}\}/g)) {
        const expr = mm[1].trim()
        if (FORMAT_CALL_RE.test(expr)) {
          violations.push({
            file: rel,
            line: lineOf(clean, cursor + mm.index),
            expr,
            hint: '数字插值未落在等宽容器内（加 .figures 容器类，或包入 <RollingNumber>）',
          })
        }
      }
    }
  }

  // 比例数字覆盖（全文件，含样式块）。
  for (const mm of clean.matchAll(PROPORTIONAL_RE)) {
    violations.push({
      file: rel,
      line: lineOf(clean, mm.index),
      expr: mm[0],
      hint: '禁止使用 proportional-nums / oldstyle-nums，会破坏等宽对齐',
    })
  }

  if (violations.length) {
    failures++
    console.log(`  FAIL ${rel}: ${violations.length} 处数字未按等宽容器渲染`)
    for (const v of violations) {
      console.log(`       ${v.file}:${v.line} {{ ${v.expr }}} — ${v.hint}`)
    }
  }
}

console.log(
  `\n等宽数字覆盖门禁：${failures === 0 ? 'PASS' : `FAIL (${failures} 个文件含违规)`}`,
)
process.exit(failures === 0 ? 0 : 1)
