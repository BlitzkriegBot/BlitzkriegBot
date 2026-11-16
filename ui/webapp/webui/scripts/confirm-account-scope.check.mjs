/**
 * 多账户确认框探测口径 — the confirm-dialog account-scope gate
 * (issue 1199 E/T5, pins P1-5).
 *
 * HftPage.vue 的 confirmThenWrite 在写轮次（保存/清除）前探测「当前账户的
 * 在途持仓」，有仓才弹确认框。审计 §12.3：多账户场景下确认框探测口径零
 * 覆盖。P1-5 的缺陷形态是过滤写成 `(p.accountId ?? accountId) === accountId`
 * —— position.accountId 缺失时静默兜底成当前账户，会把无归属的仓算进当前
 * 账户的确认数量（确认框口径失真；无归属行归属 venue 层过滤，见 HftPage
 * 内 E28 注释）。P1-5 修复后口径：过滤必须是 `p.accountId === accountId`。
 *
 * 本套件钉住：
 *   · 探测过滤 = p.accountId === accountId && (p.shares ?? 0) > 0，且
 *     accountId 是探测前冻结的本地值（await snapshot 期间切账户不漂移）；
 *   · `?? accountId` 兜底不得出现在过滤行与整个探测函数体；
 *   · 冻结账户贯通确认框状态与 writeRound（探测→确认→写是同一个账户）。
 *
 *   cd ui/webapp/webui && npm run check:confirm-scope
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

console.log('多账户确认框探测口径 — confirmThenWrite 账户过滤 (issue 1199 E/T5, P1-5)')

const page = read('src', 'pages', 'HftPage.vue')

// confirmThenWrite 整函数（函数体内右括号全部缩进，非贪婪到第一个行首 `}` 即函数尾）。
const fn = page.match(/async function confirmThenWrite\(kind: 'set' \| 'reset'\): Promise<void> \{[\s\S]*?\n\}/)
assert.ok(fn, 'HftPage.vue 缺 confirmThenWrite —— 有仓必弹确认门被移除?')
const body = fn[0]
const filterM = body.match(/\(p: Position\) => [^\n]+/)

check('探测过滤 = p.accountId === accountId，只数当前冻结账户的在途仓', () => {
  // accountId 必须是探测前冻结的本地值：await snapshot 期间切账户，探测
  // 口径仍是发起时的账户（HftPage 内 P2-7 注释的冻结语义）。
  assert.match(body, /const accountId = store\.activeAccount \?\? 'default'/, '缺探测前冻结的 accountId 本地值')
  assert.ok(filterM, '缺持仓探测过滤行')
  assert.match(filterM[0], /p\.accountId === accountId/, '过滤未按 p.accountId 精确匹配冻结账户 —— 多账户口径失真')
  assert.match(filterM[0], /\(p\.shares \?\? 0\) > 0/, '过滤未限定在途仓（shares > 0）—— 平掉已空仓的确认框语义丢失')
})

check('过滤行无 ?? accountId 兜底（P1-5 缺陷形态不得回退）', () => {
  assert.ok(filterM, '缺持仓探测过滤行')
  assert.doesNotMatch(
    filterM[0],
    /\?\? accountId/,
    'p.accountId 缺失时静默兜底成当前账户 —— 无归属仓被算进当前账户的确认数量',
  )
  // 整个探测函数体同样不得出现该兜底形态（防止过滤逻辑挪行后逃过上一条）。
  assert.doesNotMatch(body, /p\.accountId \?\? accountId/, '探测函数体出现 p.accountId ?? accountId 兜底')
})

check('冻结账户贯通：确认框状态与 writeRound 都用探测时的 accountId', () => {
  assert.match(body, /hftRoundConfirmAccount\.value = accountId/, '确认框未冻结探测账户 —— 停留期间切账户后文案漂移')
  assert.match(body, /await writeRound\(kind, accountId\)/, '写入未携带探测冻结的账户 —— 确认与写不是同一个账户')
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures ? 1 : 0)
