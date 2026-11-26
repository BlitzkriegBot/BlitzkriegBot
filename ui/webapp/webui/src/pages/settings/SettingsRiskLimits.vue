<script setup lang="ts">
/**
 * 生效风控读数 + 编辑（E26 §4.4 / 用户裁决：六项系统限额必须都能改）。每个限额
 * 都是「值 + 来源」，来源不明的数字不是操作者能据以行动的答案。
 *
 * 可编辑：每行一个输入框（预填当前生效值），保存走 risk.setSystemic —— 只发
 * 改动过的字段，内核 plan-first（一项非法整体拒）+ 逐字段审计。效果分级由
 * 内核在回执里逐字段给出：六项 live（立即约束下一笔策略开仓），回撤预算 +
 * 连亏熔断 + 冷静期三项 next_session（本会话不追溯改判，重启后按新值武装）。
 * 未持久化（persisted=false）：重启回到启动 flag/env/TOML 的解析结果 —— 来源
 * 徽标会变成「命令行/热改」，这是内核 LimitSource::Flag 的如实呈现。
 */
import { computed, onMounted, ref } from 'vue'
import { storeToRefs } from 'pinia'
import { RefreshCw, Save, ShieldCheck } from 'lucide-vue-next'
import {
  type RiskBound, type RiskLimitsDoc,
} from '@/api/client'
import { finiteNumber } from '@/lib/format'
import { useRiskLimitsStore } from '@/stores/risk-limits'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'
import Switch from '@/components/ui/switch/Switch.vue'

const limits = useRiskLimitsStore()
const { riskDoc, riskErr, riskBusy } = storeToRefs(limits)
const riskSaving = ref(false)

/** 本卡片是「生效风控」一张表单：账户级限额 + 退出纪律 + 账户生效参数（#params 插槽）
 *  由底部唯一的保存按钮一次性提交，子组件不自己调接口。 */
defineEmits<{ (e: 'save-all'): void }>()

defineExpose({
  buildPatch,
  buildExitPatch,
  riskSaving,
})

async function loadRisk(): Promise<void> {
  const doc = await limits.load()
  if (doc && !doc.error) syncBuffers(doc)
}

/** 来源词表：出厂/环境变量/命令行或热改，与内核 LimitSource 一致（去 TOML 后只允许 flag/env/default）。 */
function sourceLabel(s: RiskBound['source']): string {
  switch (s) {
    case 'default': return '出厂'
    case 'env': return '环境变量'
    case 'flag': return '命令行/热改'
    default: return String(s)
  }
}
/** 0 = 关闭（§4.1 出厂即静默）：显示「关闭」而不是一个伪装成保护措施的 0。 */
function boundArmed(b: RiskBound): boolean {
  return Number(b.value) > 0
}
function boundText(b: RiskBound): string {
  return boundArmed(b) ? b.value : '关闭'
}

/** 一行的定义：wire 字段名（camelCase）+ 展示元数据 + 整数/小数。 */
interface RiskRowDef {
  field: string; label: string; unit: string; group: string; int: boolean
  max?: number
  /** 可选的通俗解释：某一行的含义用户读不出时给一句。 */
  hint?: string
  bound: () => RiskBound | undefined
}
const ROW_DEFS: RiskRowDef[] = [
  { field: 'maxSingleLossUsd', label: '单笔最大亏损', unit: 'USD', group: '账户', int: false, bound: () => riskDoc.value?.account.limits.maxSingleLossUsd },
  { field: 'maxDailyDrawdownUsd', label: '当日最大回撤', unit: 'USD', group: '账户', int: false, bound: () => riskDoc.value?.account.limits.maxDailyDrawdownUsd },
  { field: 'maxPositionSize', label: '单仓上限', unit: '股', group: '账户', int: true, bound: () => riskDoc.value?.account.limits.maxPositionSize },
  { field: 'maxConsecutiveLosses', label: '连亏熔断', unit: '次', group: '账户', int: true, bound: () => riskDoc.value?.account.limits.maxConsecutiveLosses },
  { field: 'cooldownMinutes', label: '熔断冷静期', unit: '分钟', group: '账户', int: true, bound: () => riskDoc.value?.account.limits.cooldownMinutes },
  { field: 'maxOrderNotionalUsd', label: '单笔最大限额', unit: 'USD', group: '账户', int: false, hint: '无论策略算出要下多少钱，系统单笔最多按这个金额发单（超出部分静默截断）', bound: () => riskDoc.value?.account.limits.maxOrderNotionalUsd },
  {
    field: 'sizePct',
    label: '单笔权益比例 (BK_SIZE_PCT)',
    unit: '%',
    group: '账户',
    int: false,
    max: 100,
    hint: '按账户现金权益百分比计算单笔预算（0 = 关闭，走固定股数/金额；例：填 2 表示 2%）',
    bound: () => riskDoc.value?.sizePct !== undefined
      ? { value: String(riskDoc.value.sizePct), source: Number(riskDoc.value.sizePct) > 0 ? 'flag' : 'default' }
      : undefined,
  },
  {
    field: 'maxPositions',
    label: '最大并发持仓',
    unit: '笔',
    group: '账户',
    int: true,
    hint: '账户同时持有的未到期持仓上限（0 = 关闭/不限；修改后立即生效并持久化）',
    bound: () => riskDoc.value?.maxPositions !== undefined
      ? { value: String(riskDoc.value.maxPositions), source: riskDoc.value.maxPositions > 0 ? 'flag' : 'default' }
      : undefined,
  },
  {
    field: 'minShares',
    label: '单笔最小份额',
    unit: '股',
    group: '账户',
    int: false,
    hint: '单笔下单份额下限（需 ≤ 单笔最大份额；修改后立即生效并持久化）',
    bound: () => riskDoc.value?.minShares !== undefined
      ? { value: String(riskDoc.value.minShares), source: Number(riskDoc.value.minShares) > 0 ? 'flag' : 'default' }
      : undefined,
  },
  {
    field: 'maxShares',
    label: '单笔最大份额',
    unit: '股',
    group: '账户',
    int: false,
    hint: '单笔下单份额上限（必须 > 0 且 ≥ 单笔最小份额；例：10、100、500）',
    bound: () => riskDoc.value?.maxShares !== undefined
      ? { value: String(riskDoc.value.maxShares), source: Number(riskDoc.value.maxShares) > 0 ? 'flag' : 'default' }
      : undefined,
  },
]

// ── 编辑缓冲 + 保存（risk.setSystemic，只发改动项）──
const editBufs = ref<Record<string, string>>({})

function syncBuffers(d: RiskLimitsDoc): void {
  const next: Record<string, string> = {}
  for (const r of ROW_DEFS) next[r.field] = r.bound()?.value ?? '0'
  // 退出纪律三行同用一块编辑缓冲（field 不与限额行重叠）。
  next['stopLossPct'] = String(d.exit.stopLossPct)
  next['takeProfitPct'] = String(d.exit.takeProfitPct)
  next['forceExitSec'] = String(d.exit.forceExitSec)
  // 老内核缺 trailingEnabled → undefined：开关缺省 = 关（显示层如实降级，
  // 发送的 patch 里也不带它 —— 不会意外把未声明的开关拨到 true）。
  next['trailingEnabled'] = String(d.exit.trailingEnabled ?? false)
  next['makerFirstExits'] = String(d.exit.strategyExitMakerFirst ?? false)
  editBufs.value = next
}

/** 只发改动过的字段；空串=保持不变。非法输入就地拒绝保存。 */
function buildPatch(): Record<string, string | number> | string {
  const patch: Record<string, string | number> = {}
  for (const r of ROW_DEFS) {
    const raw = (editBufs.value[r.field] ?? '').trim()
    if (raw === '') continue // 空串 = 这一行不动
    const cur = r.bound()?.value ?? '0'
    if (raw === cur) continue
    const n = Number(raw)
    if (!Number.isFinite(n) || n < 0) return `${r.label}：必须是 ≥ 0 的数字（0 = 关闭）`
    if (r.field === 'maxShares' && n <= 0) return `${r.label}：必须 > 0`
    if (r.max !== undefined && n > r.max) return `${r.label}：不能超过 ${r.max}`
    if (r.int && !Number.isInteger(n)) return `${r.label}：必须是整数`
    patch[r.field] = r.field === 'maxPositions' ? n : raw
  }
  return patch
}

// ── 退出纪律（Gate 4 绑定）：止损/止盈/强平三行，走 risk.setExit ──────────
// 与系统限额同卡但不同写路径：退出纪律不是 Bound（没有来源徽标的枚举语义，
// 内核里它是行为旋钮），percentages 拒绝 ≤ 0，forceExitSec 0 = 关闭强平
// （出厂校准值）。生效 = live：新入场下一笔按新值绑定，已开仓位的退出扫描
// 同步跟随 —— 内核回执逐项 old→new。
interface ExitRowDef { field: string; label: string; toggle?: boolean; hint?: string; excluded?: boolean }
const EXIT_ROW_DEFS: ExitRowDef[] = [
  { field: 'stopLossPct', label: '止损' },
  { field: 'takeProfitPct', label: '止盈' },
  { field: 'forceExitSec', label: '强平' },
  {
    field: 'trailingEnabled',
    label: '移动止盈',
    toggle: true,
    hint: '浮盈回吐即锁利离场。出厂关；spread_arb 一类价差策略建议开。',
  },
  {
    field: 'makerFirstExits',
    label: '策略出场 maker-first',
    toggle: true,
    hint: 'on 时利润侧策略出场（止盈/移动止盈）先挂单后升级，降出场烧格；保护侧（止损/强平）永不转挂单。',
  },
  {
    field: 'holderStopPct',
    label: '止损（持有到期口径）',
    excluded: true,
    hint: '策略族口径（holds_to_settlement 专属），不是全局设置，不进本卡。',
  },
]
const EXIT_FIELDS = new Set(EXIT_ROW_DEFS.filter((r) => !r.excluded).map((r) => r.field))

function exitPctText(v: unknown): string {
  const n = finiteNumber(v)
  return n === null ? '—' : `${n}%`
}

/** 退出纪律行的当前值文本（单位随行）：输入框的参照。 */
function exitCurrentText(field: string): string {
  const e = riskDoc.value?.exit
  if (!e) return '—'
  if (field === 'stopLossPct') return `%（当前 ${exitPctText(e.stopLossPct)}）`
  if (field === 'takeProfitPct') return `%（当前 ${exitPctText(e.takeProfitPct)}）`
  if (field === 'holderStopPct') return `%（持有到期策略生效 ${exitPctText(e.holderStopPct)}；通用 12% 不绑它们）`
  return `秒（当前 ${e.forceExitSec}s${Number(e.forceExitSec) === 0 ? '，关闭' : ''}）`
}
function exitPlaceholder(field: string): string {
  const e = riskDoc.value?.exit
  if (!e || isExcluded(field)) return ''
  if (field === 'stopLossPct') return String(e.stopLossPct)
  if (field === 'takeProfitPct') return String(e.takeProfitPct)
  return String(e.forceExitSec)
}
/** 被排除的行（策略族口径）不属于本卡的可写面。 */
function isExcluded(field: string): boolean {
  return EXIT_ROW_DEFS.find((r) => r.field === field)?.excluded ?? false
}
/** trailing 开关（toggle 行）：草稿缓冲存 'true'/'false' 字符串。 */
function trailingOn(): boolean {
  return (editBufs.value['trailingEnabled'] ?? 'false') === 'true'
}
function setTrailing(on: boolean): void {
  editBufs.value['trailingEnabled'] = String(on)
}
/** 老内核不认识 trailingEnabled（读数缺字段）→ 开关禁用并说明。 */
const trailingSupported = computed(() => riskDoc.value?.exit.trailingEnabled !== undefined)

/** maker-first 开关（toggle 行，同 trailing 的缓冲/适用性约定）。 */
function makerFirstOn(): boolean {
  return (editBufs.value['makerFirstExits'] ?? 'false') === 'true'
}
function setMakerFirst(on: boolean): void {
  editBufs.value['makerFirstExits'] = String(on)
}
const makerFirstSupported = computed(() => riskDoc.value?.exit.strategyExitMakerFirst !== undefined)

/** 退出纪律没有 LimitSource 枚举：热改后 badge 如实写「行为旋钮」。 */
function exitSourceLabel(): string {
  return '行为旋钮'
}

/** 退出纪律 patch：只发改动项；百分比必须 > 0，强平 ≥ 0（0 = 关闭）。 */
function buildExitPatch(): Record<string, string | number | boolean> | string {
  const e = riskDoc.value?.exit
  if (!e) return '读不到退出纪律当前值'
  const patch: Record<string, string | number | boolean> = {}
  const sl = (editBufs.value['stopLossPct'] ?? '').trim()
  if (sl !== '' && String(e.stopLossPct) !== sl) {
    const n = Number(sl)
    if (!Number.isFinite(n) || n <= 0) return '止损：必须是 > 0 的百分比'
    patch.stopLossPct = sl
  }
  const tp = (editBufs.value['takeProfitPct'] ?? '').trim()
  if (tp !== '' && String(e.takeProfitPct) !== tp) {
    const n = Number(tp)
    if (!Number.isFinite(n) || n <= 0) return '止盈：必须是 > 0 的百分比'
    patch.takeProfitPct = tp
  }
  const fe = (editBufs.value['forceExitSec'] ?? '').trim()
  if (fe !== '' && String(e.forceExitSec) !== fe) {
    const n = Number(fe)
    if (!Number.isFinite(n) || n < 0 || !Number.isInteger(n)) return '强平：必须是 ≥ 0 的整数秒（0 = 关闭强平）'
    patch.forceExitSec = n
  }
  // trailing 开关只在老内核缺字段（undefined）时降级为不可拨 —— 缺省发送
  // 会把未声明的开关意外拨到 true，宁可不发。
  if (e.trailingEnabled !== undefined) {
    const tr = (editBufs.value['trailingEnabled'] ?? 'false') === 'true'
    if (tr !== e.trailingEnabled) patch.trailingEnabled = tr
  }
  // maker-first 开关同约定：老内核缺字段不拨。
  if (e.strategyExitMakerFirst !== undefined) {
    const mf = (editBufs.value['makerFirstExits'] ?? 'false') === 'true'
    if (mf !== e.strategyExitMakerFirst) patch.strategyExitMakerFirst = mf
  }
  return patch
}

/** 行渲染：读数（bound）驱动徽标，编辑缓冲驱动输入框。 */
const riskRows = computed(() =>
  ROW_DEFS.map((r) => ({ ...r, bound: r.bound() })))
const armedRows = computed(() => riskRows.value.filter((r) => r.bound && boundArmed(r.bound)))

const riskBadge = computed(() => {
  if (!riskDoc.value) return { text: '不可用', variant: 'default' as const }
  return armedRows.value.length > 0
    ? { text: `已武装 ${armedRows.value.length} 项`, variant: 'gold' as const }
    : { text: '出厂（全部关闭）', variant: 'up' as const }
})
const riskHint = computed(() => {
  if (!riskDoc.value) return ''
  return armedRows.value.length > 0
    ? '六项改动立即约束下一笔策略开仓；回撤 / 连亏 / 冷静期三项重启后生效（本会话不追溯改判）。连亏熔断触发时暂停该账户的新开仓，平仓永不受限。'
    : '六项系统限额全部为 0 = 关闭（出厂承诺）：内核行为与未加风控时逐位一致。保存后六项立即生效。'
})
onMounted(() => void loadRisk())
</script>

<template>
  <Card>
    <CardHeader label="生效风控">
      <template #title>
        <ShieldCheck class="size-4 text-faint-fg" />
      </template>
      <template #action>
        <Badge :variant="riskBadge.variant" dot>{{ riskBadge.text }}</Badge>
      </template>
    </CardHeader>

    <p class="text-[11.5px] leading-snug text-muted-fg">
      内核正在执行的系统性限额（risk.limits）——每个数字都带着它从哪来（出厂 / 环境变量 /
      热改）。没来源的数字不是能据以行动的答案；0 = 关闭，不是伪装成保护措施的零。
      改动保存进内核内存并逐字段落审计，重启后回到启动配置的解析结果。
    </p>

    <AlertBanner v-if="!riskDoc && !riskErr" class="mt-3" tone="info">正在读取风控读数…</AlertBanner>
    <AlertBanner v-else-if="!riskDoc" class="mt-3" tone="warn">
      读不到风控读数（{{ riskErr ?? '内核离线或版本过旧' }}）。这个内核可能不认识 risk.limits。
    </AlertBanner>

    <template v-else>
      <div class="mt-3">
        <template v-for="group in ['账户']" :key="group">
          <div class="label-micro mt-2 first:mt-0">{{ group }}级限额</div>
          <div class="mt-1">
            <div
              v-for="(row, i) in riskRows.filter((r) => r.group === group)"
              :key="row.field"
              class="flex items-center justify-between gap-3 py-1.5 text-[12px]"
              :class="i === 0 ? '' : 'border-t border-line'"
            >
              <span class="text-faint-fg">{{ row.label }}<span v-if="row.hint" class="ml-1 text-[10.5px] text-faint-fg">{{ row.hint }}</span></span>
              <span class="flex items-center gap-2">
                <Input
                  v-model="editBufs[row.field]"
                  class="num h-7 w-24 text-right text-[12px]"
                  :type="row.int ? 'number' : 'text'"
                  :min="0"
                  :step="row.int ? 1 : 'any'"
                  :placeholder="row.bound ? boundText(row.bound) : ''"
                  :aria-label="row.label"
                />
                <span v-if="row.bound && boundArmed(row.bound)" class="text-[10.5px] font-normal text-faint-fg">{{ row.unit }}</span>
                <span v-else class="text-[10.5px] font-normal text-faint-fg">关闭</span>
                <Badge :variant="row.bound && boundArmed(row.bound) ? 'gold' : 'default'">{{ row.bound ? sourceLabel(row.bound.source) : '—' }}</Badge>
              </span>
            </div>
          </div>
        </template>

        <div class="label-micro mt-3">退出纪律</div>
        <div class="mt-1">
          <div
            v-for="(row, i) in EXIT_ROW_DEFS.filter((r) => !r.excluded)"
            :key="row.field"
            class="flex items-center justify-between gap-3 py-1.5 text-[12px]"
            :class="i === 0 ? '' : 'border-t border-line'"
          >
            <span class="text-faint-fg">{{ row.label }}<span v-if="row.hint" class="ml-1 text-[10.5px] text-faint-fg">{{ row.hint }}</span></span>
            <span class="flex items-center gap-2">
              <!-- trailingEnabled 热配开关（bool 行用 Switch，
                   数值行保持 Input；老内核缺字段 → 禁用态，不造默认值）。 -->
              <Switch
                v-if="row.toggle"
                :model-value="row.field === 'trailingEnabled' ? trailingOn() : makerFirstOn()"
                :disabled="row.field === 'trailingEnabled' ? !trailingSupported : !makerFirstSupported"
                :aria-label="row.label"
                @update:model-value="(v: boolean) => (row.field === 'trailingEnabled' ? setTrailing(v) : setMakerFirst(v))"
              />
              <Input
                v-else
                v-model="editBufs[row.field]"
                class="num h-7 w-24 text-right text-[12px]"
                type="text"
                :min="0"
                step="any"
                :placeholder="exitPlaceholder(row.field)"
                :aria-label="row.label"
              />
              <span class="text-[10.5px] font-normal text-faint-fg">{{ row.field === 'trailingEnabled' ? (trailingSupported ? (trailingOn() ? '开' : '关') : '内核不支持') : row.toggle ? (makerFirstSupported ? (makerFirstOn() ? '开' : '关') : '内核不支持') : exitCurrentText(row.field) }}</span>
              <Badge variant="gold">{{ exitSourceLabel() }}</Badge>
            </span>
          </div>
        </div>
      </div>

      <!-- issue 393 (①): 账户生效执行参数（可编辑）插进来 —— 有运行时写路径的
           风控参数与系统限额同卡呈现，各归各位（插槽内容来自编排壳）。
           参数与规则同属 execution_policy 的【一个账户段】，由底部唯一的保存按钮
           整段提交 —— 拆成两个按钮时，各自只发半段，内核按「缺省即清扫」把另一半
           抹掉（2026-11-24 修复：两个保存按钮互相排斥）。 -->
      <slot name="params" />

      <!-- 执行策略规则：与上方参数同一个账户段，共用同一个保存按钮。 -->
      <slot name="rules" />

      <div class="mt-3 flex flex-wrap items-center gap-2">
        <Button
          size="sm" :disabled="riskSaving || riskBusy"
          title="把本卡全部内容（账户级限额 / 退出纪律 / 账户生效参数 / 执行策略规则）一次性提交给内核"
          @click="$emit('save-all')"
        >
          <Save class="size-3.5" />{{ riskSaving ? '保存中…' : '保存' }}
        </Button>
        <Button
          variant="outline" size="sm" :disabled="riskBusy" title="重新读取内核的风控读数（放弃未保存的编辑）"
          @click="loadRisk"
        >
          <RefreshCw class="size-3.5" />刷新
        </Button>
        <span class="text-[11px] leading-snug text-faint-fg">{{ riskHint }}</span>
      </div>
      <p v-if="riskErr" class="mt-2 text-[11px] leading-snug text-down">{{ riskErr }}</p>
    </template>
  </Card>
</template>
