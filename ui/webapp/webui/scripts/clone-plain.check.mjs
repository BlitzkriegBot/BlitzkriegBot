/**
 * 生效风控 · 执行策略规则保存路径不得对 Vue/Pinia 代理调用 structuredClone。
 *
 * 回归形态（PR #140 / 设置页 store 化之后）：
 *   useExecutionPolicyStore.load 把内核 section 赋进 policySection ref，
 *   再把这份代理交给 SettingsRisk.loadEditorFromSection。structuredClone
 *   遇 Proxy 抛 DataCloneError，浏览器表现为：
 *     Failed to execute 'structuredClone' on 'Window': #<Object> could not be cloned.
 *   写路径 executionPolicySet 本身用 JSON.stringify，代理可以过线；失败发生在
 *   保存成功后的 loadPolicy() 回读，catch 把克隆错误当成「无法保存」。
 *
 * 本套件钉住：
 *   · structuredClone 对 reactive 规则抛 DataCloneError（缺陷形态，不得「修好」这条）；
 *   · clonePlain 能克隆、结果是普通对象、与用户输入 JSON 字节级一致、互不影响；
 *   · 模拟 store 回读 → 编辑缓冲 → executionPolicySet 的 body，字节级等于用户输入；
 *   · 隔离的 mock HTTP 往返：POST 落盘、GET 读回、再走 loadEditorFromSection 不抛；
 *   · 源码：SettingsRisk / PolicyRulesEditor / Blueprint 调用点走 clonePlain，
 *     src/ 不再出现 structuredClone( 调用。
 *
 *   cd ui/webapp/webui && npm run check:clone-plain
 */
import assert from 'node:assert/strict'
import { readdirSync, readFileSync, statSync } from 'node:fs'
import { dirname, join, relative } from 'node:path'
import { fileURLToPath } from 'node:url'
import { reactive, isReactive, isProxy } from 'vue'
import { clonePlain } from '../src/lib/clone-plain.ts'

const here = dirname(fileURLToPath(import.meta.url))
const webuiRoot = join(here, '..')
const read = (...p) => readFileSync(join(webuiRoot, ...p), 'utf8')

let failures = 0
const check = (label, fn) => {
  try {
    const out = fn()
    if (out && typeof out.then === 'function') {
      return out.then(
        () => console.log(`  ok   ${label}`),
        (e) => {
          failures++
          console.log(`  FAIL ${label}\n       ${e.message}`)
        },
      )
    }
    console.log(`  ok   ${label}`)
  } catch (e) {
    failures++
    console.log(`  FAIL ${label}\n       ${e.message}`)
  }
}

console.log('生效风控规则保存 — clonePlain vs Vue 代理 (structuredClone DataCloneError)')

const entered = {
  name: 'skip_when_two_open',
  priority: 10,
  enabled: true,
  when: { field: 'open_positions', op: '>=', value: 2 },
  then: { action: 'skip' },
  reason: 'fixture only',
}

/** Pinia-style: API payload assigned into reactive state (the store's section). */
function storeSection(rules) {
  return reactive({
    maxPositionsPerAsset: 1,
    rules: clonePlain(rules),
  })
}

check('structuredClone 对 Vue reactive 规则抛 DataCloneError（缺陷形态）', () => {
  const section = storeSection([entered])
  assert.ok(isReactive(section), 'section 必须是 reactive，才是回归形态')
  assert.ok(isProxy(section.rules[0]), 'rules[0] 必须是代理')
  assert.throws(
    () => structuredClone(section.rules[0]),
    (err) =>
      err instanceof DOMException &&
      err.name === 'DataCloneError' &&
      /could not be cloned/i.test(err.message),
    'structuredClone 应对代理抛 DataCloneError，与面板报错一致',
  )
})

check('clonePlain 能克隆代理，结果是普通对象且与用户输入 JSON 字节级一致', () => {
  const section = storeSection([entered])
  const cloned = clonePlain(section.rules)
  assert.equal(Array.isArray(cloned), true)
  assert.equal(isProxy(cloned), false, '克隆结果不得仍是代理')
  assert.equal(isProxy(cloned[0]), false)
  assert.deepEqual(cloned, [entered])
  assert.equal(JSON.stringify(cloned), JSON.stringify([entered]))
  cloned[0].name = 'mutated'
  cloned[0].when.value = 99
  assert.equal(entered.name, 'skip_when_two_open', '克隆与用户输入必须隔离')
  assert.equal(section.rules[0].name, 'skip_when_two_open', '不得写穿 store')
})

check('loadEditorFromSection 回读路径：缓冲可保存，POST body 与用户输入字节级一致', () => {
  const section = storeSection([entered])
  const editRules = clonePlain(section.rules ?? [])
  const params = { accountId: 'defaults', rules: editRules }
  const body = JSON.stringify(params)
  const expected = JSON.stringify({ accountId: 'defaults', rules: [entered] })
  assert.equal(body, expected, 'executionPolicySet 的 body 必须与用户输入字节级一致')
})

await check('隔离 mock HTTP：set 落盘、get 读回、再回读不抛，规则字节级一致', async () => {
  const persist = new Map()
  const posts = []
  const ls = new Map()
  globalThis.localStorage = {
    getItem: (k) => (ls.has(k) ? ls.get(k) : null),
    setItem: (k, v) => ls.set(k, String(v)),
    removeItem: (k) => ls.delete(k),
  }
  ls.set('blitzkrieg-panel-token', 'test-token')

  globalThis.fetch = async (url, init = {}) => {
    const u = String(url)
    if (u.includes('/execution-policy/set') && String(init.method || '').toUpperCase() === 'POST') {
      const raw = String(init.body ?? '')
      posts.push(raw)
      const parsed = JSON.parse(raw)
      persist.set(parsed.accountId, parsed)
      const section = { maxPositionsPerAsset: 1, rules: parsed.rules }
      return new Response(
        JSON.stringify({ accountId: parsed.accountId, written: section, readBack: section }),
        { status: 200, headers: { 'Content-Type': 'application/json' } },
      )
    }
    if (/\/execution-policy\?accountId=/.test(u)) {
      const id = new URL(u, 'http://panel.local').searchParams.get('accountId')
      const parsed = persist.get(id)
      assert.ok(parsed, `mock kernel 没有账户 ${id} 的落盘`)
      return new Response(
        JSON.stringify({ maxPositionsPerAsset: 1, rules: parsed.rules }),
        { status: 200, headers: { 'Content-Type': 'application/json' } },
      )
    }
    throw new Error(`unexpected fetch ${u}`)
  }

  const { executionPolicyApi } = await import('../src/api/execution-policy.ts')
  const section = storeSection([entered])
  const editRules = clonePlain(section.rules ?? [])
  const payload = { accountId: 'acct-isolated', rules: editRules }
  const expectedBody = JSON.stringify({ accountId: 'acct-isolated', rules: [entered] })
  const res = await executionPolicyApi.executionPolicySet(payload)
  assert.equal(res.accountId, 'acct-isolated')
  assert.equal(posts.length, 1, '必须真正 POST 一次')
  assert.equal(posts[0], expectedBody, '内核收到的 body 必须与用户输入字节级一致')
  assert.equal(
    JSON.stringify(res.written.rules),
    JSON.stringify([entered]),
    'written.rules 必须原样落盘',
  )
  assert.equal(JSON.stringify(res.readBack.rules), JSON.stringify([entered]))

  const got = await executionPolicyApi.executionPolicyGet('acct-isolated')
  const reloaded = storeSection(got.rules)
  const again = clonePlain(reloaded.rules ?? [])
  assert.equal(JSON.stringify(again), JSON.stringify([entered]))
  assert.throws(() => structuredClone(reloaded.rules[0]), (err) => err.name === 'DataCloneError')
})

check('SettingsRisk.loadEditorFromSection 走 clonePlain，不再 structuredClone', () => {
  const src = read('src', 'pages', 'settings', 'SettingsRisk.vue')
  const fn = src.match(/function loadEditorFromSection\([^)]*\): void \{[\s\S]*?\n\}/)
  assert.ok(fn, '缺 loadEditorFromSection')
  assert.match(fn[0], /editRules\.value = clonePlain\(doc\?\.rules \?\? \[\]\)/, '回读必须 clonePlain 整份规则')
  assert.doesNotMatch(fn[0], /structuredClone\s*\(/, '回读不得再调用 structuredClone')
  assert.match(src, /import \{ clonePlain \} from '@\/lib\/clone-plain'/)
})

check('PolicyRulesEditor.cloneRule 走同一 helper', () => {
  const src = read('src', 'pages', 'settings', 'PolicyRulesEditor.vue')
  const fn = src.match(/function cloneRule\([^)]*\): PolicyRuleView \{[\s\S]*?\n\}/)
  assert.ok(fn, '缺 cloneRule')
  assert.match(fn[0], /return clonePlain\(r\)/)
  assert.doesNotMatch(fn[0], /structuredClone\s*\(/)
})

check('Blueprint 调用点走 clonePlain', () => {
  const page = read('src', 'pages', 'BlueprintPage.vue')
  const lib = read('src', 'lib', 'blueprint.ts')
  assert.match(page, /params: clonePlain\(n\.params/)
  assert.match(lib, /params: clonePlain\(n\.params \?\? \{\}\)/)
  assert.doesNotMatch(page, /structuredClone\s*\(/)
  assert.doesNotMatch(lib, /structuredClone\s*\(/)
})

check('src/ 不再有 structuredClone( 调用（注释除外）', () => {
  const root = join(webuiRoot, 'src')
  const hits = []
  const walk = (dir) => {
    for (const name of readdirSync(dir)) {
      const p = join(dir, name)
      const st = statSync(p)
      if (st.isDirectory()) {
        walk(p)
        continue
      }
      if (!/\.(ts|vue|js|mjs)$/.test(name)) continue
      const text = readFileSync(p, 'utf8')
      const lines = text.split('\n')
      lines.forEach((line, i) => {
        if (!/structuredClone\s*\(/.test(line)) return
        const trimmed = line.trim()
        if (trimmed.startsWith('//') || trimmed.startsWith('*') || trimmed.startsWith('/*')) return
        hits.push(`${relative(root, p)}:${i + 1}:${trimmed}`)
      })
    }
  }
  walk(root)
  assert.equal(hits.length, 0, `仍有 structuredClone 调用:\n       ${hits.join('\n       ')}`)
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures ? 1 : 0)
