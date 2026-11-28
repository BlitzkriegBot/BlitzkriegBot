<script setup lang="ts">
/**
 * 生效风控卡（傻白甜单真源版 — RFC 241/242/243）：
 *   主视图只保留 3 组 / 8 项高频直觉控件：
 *     1. 每单下多少：下注比例（% + 实时 USD 估算预览）+ 单笔金额范围（最小 ~ 最大 USD）
 *     2. 最多开几单：总并发持仓上限（0=不限）+ 单币种持仓上限
 *     3. 亏了怎么停：当日最大亏损 + 连亏保护（连亏 N 次暂停 M 分钟）+ 止损 % + 止盈 %
 *   低频与专家参数（单笔最大亏损、单仓上限、份额上下限、最低权益、强平秒数、移动止盈、
 *   maker-first、执行策略规则）统一折叠在「高级设置」抽屉内。
 *   整张卡片共用底部唯一「保存」按钮，一次整段提交。
 */
import { computed, onMounted, ref } from 'vue'
import { storeToRefs } from 'pinia'
import { ChevronRight, RefreshCw, Save, ShieldCheck } from 'lucide-vue-next'
import { type RiskBound, type RiskLimitsDoc } from '@/api/client'
import { finiteNumber } from '@/lib/format'
import { usePanelStore } from '@/stores/panel'
import { useRiskLimitsStore } from '@/stores/risk-limits'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import SettingsExitConfigFields from './SettingsExitConfigFields.vue'
import { buildExitPatchFromBuffers, syncExitBuffers } from './exit-config-fields'

const props = defineProps<{
  /** 当前编辑中的最小下注金额（USD），用于实时估算单笔下单金额预览。 */
  minBudgetUsd?: string
}>()

const panel = usePanelStore()
const limits = useRiskLimitsStore()
const { riskDoc, riskErr, riskBusy } = storeToRefs(limits)
const riskSaving = ref(false)
const advancedOpen = ref(false)

/** 本卡片是「生效风控」一张表单：由底部唯一保存按钮一次性提交。 */
defineEmits<{ (e: 'save-all'): void }>()

defineExpose({
  buildPatch,
  buildExitPatch,
  currentSizePct,
  riskSaving,
})

async function loadRisk(): Promise<void> {
  const doc = await limits.load()
  if (doc && !doc.error) syncBuffers(doc)
}

function boundArmed(b: RiskBound): boolean {
  return Number(b.value) > 0
}
function boundText(b: RiskBound): string {
  return boundArmed(b) ? b.value : '0'
}

interface RiskRowDef {
  field: string
  label: string
  unit: string
  int: boolean
  max?: number
  hint?: string
  bound: () => RiskBound | undefined
}

const ROW_DEFS: RiskRowDef[] = [
  { field: 'maxSingleLossUsd', label: '单笔最大亏损', unit: 'USD', int: false, hint: '0 = 关闭', bound: () => riskDoc.value?.account.limits.maxSingleLossUsd },
  { field: 'maxDailyDrawdownUsd', label: '当日最大亏损', unit: 'USD', int: false, hint: '0 = 关闭', bound: () => riskDoc.value?.account.limits.maxDailyDrawdownUsd },
  { field: 'maxPositionSize', label: '单仓上限', unit: '股', int: true, hint: '0 = 关闭', bound: () => riskDoc.value?.account.limits.maxPositionSize },
  { field: 'maxConsecutiveLosses', label: '连亏次数', unit: '次', int: true, bound: () => riskDoc.value?.account.limits.maxConsecutiveLosses },
  { field: 'cooldownMinutes', label: '冷静期', unit: '分钟', int: true, bound: () => riskDoc.value?.account.limits.cooldownMinutes },
  { field: 'maxOrderNotionalUsd', label: '单笔最大金额', unit: 'USD', int: false, bound: () => riskDoc.value?.account.limits.maxOrderNotionalUsd },
  {
    field: 'sizePct',
    label: '下注比例',
    unit: '%',
    int: false,
    max: 100,
    bound: () => riskDoc.value?.sizePct !== undefined
      ? { value: String(riskDoc.value.sizePct), source: Number(riskDoc.value.sizePct) > 0 ? 'flag' : 'default' }
      : undefined,
  },
  {
    field: 'maxPositions',
    label: '总并发持仓上限',
    unit: '笔',
    int: true,
    bound: () => riskDoc.value?.maxPositions !== undefined
      ? { value: String(riskDoc.value.maxPositions), source: riskDoc.value.maxPositions > 0 ? 'flag' : 'default' }
      : undefined,
  },
  {
    field: 'minShares',
    label: '单笔最小份额',
    unit: '股',
    int: false,
    hint: '0 = 仅按场地门槛',
    bound: () => riskDoc.value?.minShares !== undefined
      ? { value: String(riskDoc.value.minShares), source: Number(riskDoc.value.minShares) > 0 ? 'flag' : 'default' }
      : undefined,
  },
  {
    field: 'maxShares',
    label: '单笔最大份额',
    unit: '股',
    int: false,
    hint: '0 = 不限（仅受金额上限与余额约束）',
    bound: () => riskDoc.value?.maxShares !== undefined
      ? { value: String(riskDoc.value.maxShares), source: Number(riskDoc.value.maxShares) > 0 ? 'flag' : 'default' }
      : undefined,
  },
]

const ADVANCED_LIMIT_FIELDS = ['maxSingleLossUsd', 'maxPositionSize', 'minShares', 'maxShares'] as const

const editBufs = ref<Record<string, string>>({})

function syncBuffers(d: RiskLimitsDoc): void {
  const next: Record<string, string> = {}
  // issue 278: 读不到限额时留空而非填 '0'（0 在风控语义中代表关闭该限额）。
  for (const r of ROW_DEFS) next[r.field] = r.bound()?.value ?? ''
  syncExitBuffers(d.exit, next)
  editBufs.value = next
}

function setEditBuf(key: string, value: string): void {
  editBufs.value[key] = value
}

function currentSizePct(): number | null {
  return finiteNumber((editBufs.value['sizePct'] ?? '').trim())
}

/** 实时单笔预算预览：按当前展示账户可用余额 × 下注比例，叠加最小/最大金额范围。 */
const liveBudgetPreview = computed<string>(() => {
  const pct = currentSizePct()
  if (pct === null || pct <= 0) return '0 = 关闭比例下单（走固定预算）'
  const bal = finiteNumber(panel.activeAccountRow?.available ?? panel.activeAccountRow?.balance)
  if (bal === null || bal <= 0) return `占可用余额 ${pct}%`
  let est = (bal * pct) / 100
  const minUsd = finiteNumber(props.minBudgetUsd)
  const maxUsd = finiteNumber(editBufs.value['maxOrderNotionalUsd'])
  if (minUsd !== null && minUsd > 0 && est < minUsd) est = minUsd
  if (maxUsd !== null && maxUsd > 0 && est > maxUsd) est = maxUsd
  return `≈ $${est.toFixed(2)} / 单（余额 $${bal.toFixed(2)}）`
})

/** 只发改动过的字段；空串 = 保持不变。maxShares 允许 0（不限）。 */
function buildPatch(): Record<string, string | number> | string {
  const patch: Record<string, string | number> = {}
  for (const r of ROW_DEFS) {
    const raw = (editBufs.value[r.field] ?? '').trim()
    if (raw === '') continue
    const cur = r.bound()?.value ?? '0'
    if (raw === cur) continue
    const n = Number(raw)
    if (!Number.isFinite(n) || n < 0) return `${r.label}：必须是 ≥ 0 的数字（0 = 关闭/不限）`
    if (r.max !== undefined && n > r.max) return `${r.label}：不能超过 ${r.max}`
    if (r.int && !Number.isInteger(n)) return `${r.label}：必须是整数`
    patch[r.field] = r.field === 'maxPositions' ? n : raw
  }
  return patch
}

function exitPctText(v: unknown): string {
  const n = finiteNumber(v)
  return n === null ? '—' : `${n}%`
}

function exitPriceText(v: unknown): string {
  const n = finiteNumber(v)
  if (n === null) return '不支持'
  return n <= 0 ? '关闭' : `$${n}`
}

const ceilingTpSupported = computed(() => riskDoc.value?.exit.ceilingTpPrice !== undefined)

function buildExitPatch(): Record<string, string | number | boolean> | string {
  const e = riskDoc.value?.exit
  if (!e) return '读不到退出纪律当前值'
  return buildExitPatchFromBuffers(e, editBufs.value)
}

const riskRows = computed(() => ROW_DEFS.map((r) => ({ ...r, bound: r.bound() })))
const advancedLimitRows = computed(() =>
  riskRows.value.filter((r) => (ADVANCED_LIMIT_FIELDS as readonly string[]).includes(r.field)),
)
const armedCount = computed(() => riskRows.value.filter((r) => r.bound && boundArmed(r.bound)).length)

const riskBadge = computed(() => {
  if (!riskDoc.value) return { text: '不可用', variant: 'default' as const }
  return armedCount.value > 0
    ? { text: `已生效 ${armedCount.value} 项`, variant: 'gold' as const }
    : { text: '出厂默认', variant: 'up' as const }
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
      只保留 3 组核心风控（每单下多少 / 最多开几单 / 亏了怎么停），修改后点底部「保存」立即生效并持久化。
    </p>

    <AlertBanner v-if="!riskDoc && !riskErr" class="mt-3" tone="info">正在读取风控读数…</AlertBanner>
    <AlertBanner v-else-if="!riskDoc" class="mt-3" tone="warn">
      读不到风控读数（{{ riskErr ?? '内核离线或版本过旧' }}）。
    </AlertBanner>

    <template v-else>
      <!-- 1. 每单下多少（2 项） -->
      <div class="mt-3" data-risk-group="sizing">
        <div class="label-micro">每单下多少</div>
        <div class="mt-1.5 space-y-2">
          <div class="flex flex-wrap items-center justify-between gap-2 py-1 text-[12px]">
            <div>
              <span class="text-fg">下注比例</span>
              <span class="ml-2 num text-[11px] text-primary">{{ liveBudgetPreview }}</span>
            </div>
            <span class="flex items-center gap-1.5">
              <Input
                v-model="editBufs['sizePct']"
                class="num h-7 w-24 text-right text-[12px]"
                type="text"
                min="0"
                max="100"
                step="any"
                placeholder="如 2"
                aria-label="下注比例"
              />
              <span class="w-8 text-[11px] text-faint-fg">%</span>
            </span>
          </div>

          <div class="flex flex-wrap items-center justify-between gap-2 border-t border-line py-1.5 text-[12px]">
            <div>
              <span class="text-fg">单笔金额范围</span>
              <span class="ml-1.5 text-[10.5px] text-faint-fg">最小 ~ 最大（最大 0 = 不限）</span>
            </div>
            <span class="flex items-center gap-1.5">
              <slot name="sizing-min" />
              <span class="text-[11px] text-faint-fg">~</span>
              <Input
                v-model="editBufs['maxOrderNotionalUsd']"
                class="num h-7 w-24 text-right text-[12px]"
                type="text"
                min="0"
                step="any"
                placeholder="最大 USD"
                aria-label="单笔最大金额"
              />
              <span class="w-8 text-[11px] text-faint-fg">USD</span>
            </span>
          </div>
        </div>
      </div>

      <!-- 2. 最多开几单（2 项） -->
      <div class="mt-3 border-t border-line pt-3" data-risk-group="positions">
        <div class="label-micro">最多开几单</div>
        <div class="mt-1.5 space-y-1.5">
          <div class="flex items-center justify-between gap-3 py-1 text-[12px]">
            <div>
              <span class="text-fg">总并发持仓上限</span>
              <span class="ml-1.5 text-[10.5px] text-faint-fg">0 = 不限</span>
            </div>
            <span class="flex items-center gap-1.5">
              <Input
                v-model="editBufs['maxPositions']"
                class="num h-7 w-24 text-right text-[12px]"
                type="number"
                min="0"
                step="1"
                placeholder="0"
                aria-label="总并发持仓上限"
              />
              <span class="w-8 text-[11px] text-faint-fg">笔</span>
            </span>
          </div>

          <div class="flex items-center justify-between gap-3 border-t border-line py-1.5 text-[12px]">
            <div>
              <span class="text-fg">单币种持仓上限</span>
              <span class="ml-1.5 text-[10.5px] text-faint-fg">同一标的最多同时持有</span>
            </div>
            <span class="flex items-center gap-1.5">
              <slot name="positions-per-asset" />
              <span class="w-8 text-[11px] text-faint-fg">笔</span>
            </span>
          </div>
        </div>
      </div>

      <!-- 3. 亏了怎么停（4 项） -->
      <div class="mt-3 border-t border-line pt-3" data-risk-group="protection">
        <div class="label-micro">亏了怎么停</div>
        <div class="mt-1.5 space-y-1.5">
          <div class="flex items-center justify-between gap-3 py-1 text-[12px]">
            <div>
              <span class="text-fg">当日最大亏损</span>
              <span class="ml-1.5 text-[10.5px] text-faint-fg">达到后当日停止新开仓，0 = 关闭</span>
            </div>
            <span class="flex items-center gap-1.5">
              <Input
                v-model="editBufs['maxDailyDrawdownUsd']"
                class="num h-7 w-24 text-right text-[12px]"
                type="text"
                min="0"
                step="any"
                placeholder="0"
                aria-label="当日最大亏损"
              />
              <span class="w-8 text-[11px] text-faint-fg">USD</span>
            </span>
          </div>

          <div class="flex flex-wrap items-center justify-between gap-2 border-t border-line py-1.5 text-[12px]">
            <div>
              <span class="text-fg">连亏保护</span>
              <span class="ml-1.5 text-[10.5px] text-faint-fg">0 = 关闭（重启后生效）</span>
            </div>
            <span class="flex items-center gap-1.5">
              <span class="text-[11px] text-faint-fg">连亏</span>
              <Input
                v-model="editBufs['maxConsecutiveLosses']"
                class="num h-7 w-16 text-right text-[12px]"
                type="number"
                min="0"
                step="1"
                placeholder="0"
                aria-label="连亏次数"
              />
              <span class="text-[11px] text-faint-fg">次暂停</span>
              <Input
                v-model="editBufs['cooldownMinutes']"
                class="num h-7 w-16 text-right text-[12px]"
                type="number"
                min="0"
                step="1"
                placeholder="0"
                aria-label="冷静期"
              />
              <span class="text-[11px] text-faint-fg">分钟</span>
            </span>
          </div>

          <div class="flex items-center justify-between gap-3 border-t border-line py-1.5 text-[12px]">
            <div>
              <span class="text-fg">止损</span>
              <span class="ml-1.5 text-[10.5px] text-faint-fg">当前 {{ exitPctText(riskDoc.exit.stopLossPct) }}</span>
            </div>
            <span class="flex items-center gap-1.5">
              <Input
                v-model="editBufs['stopLossPct']"
                class="num h-7 w-24 text-right text-[12px]"
                type="text"
                min="0"
                step="any"
                aria-label="止损"
              />
              <span class="w-8 text-[11px] text-faint-fg">%</span>
            </span>
          </div>

          <div class="flex items-center justify-between gap-3 border-t border-line py-1.5 text-[12px]">
            <div>
              <span class="text-fg">止盈</span>
              <span class="ml-1.5 text-[10.5px] text-faint-fg">当前 {{ exitPctText(riskDoc.exit.takeProfitPct) }}</span>
            </div>
            <span class="flex items-center gap-1.5">
              <Input
                v-model="editBufs['takeProfitPct']"
                class="num h-7 w-24 text-right text-[12px]"
                type="text"
                min="0"
                step="any"
                aria-label="止盈"
              />
              <span class="w-8 text-[11px] text-faint-fg">%</span>
            </span>
          </div>

          <div class="flex items-center justify-between gap-3 border-t border-line py-1.5 text-[12px]">
            <div>
              <span class="text-fg">天花板止盈价</span>
              <span class="ml-1.5 text-[10.5px] text-faint-fg">当前 {{ exitPriceText(riskDoc.exit.ceilingTpPrice) }}（0 = 关闭）</span>
            </div>
            <span class="flex items-center gap-1.5">
              <Input
                v-model="editBufs['ceilingTpPrice']"
                class="num h-7 w-24 text-right text-[12px]"
                type="text"
                min="0"
                max="1"
                step="any"
                :disabled="!ceilingTpSupported"
                placeholder="0.98"
                aria-label="天花板止盈价"
              />
              <span class="w-8 text-[11px] text-faint-fg">USD</span>
            </span>
          </div>
        </div>
      </div>

      <!-- 默认折叠：高级设置抽屉 -->
      <div class="mt-3 border-t border-line pt-3" data-risk-drawer="advanced">
        <button
          type="button"
          class="flex w-full items-center justify-between rounded-md px-1 py-1 text-left text-[12px] font-medium text-muted-fg hover:text-fg"
          :aria-expanded="advancedOpen"
          @click="advancedOpen = !advancedOpen"
        >
          <span class="flex items-center gap-1.5">
            <ChevronRight
              class="size-3.5 transition-transform"
              :class="advancedOpen ? 'rotate-90' : ''"
            />
            高级设置（份额上下限 / 单仓限额 / 强平与策略出场 / 执行规则）
          </span>
          <span class="text-[10.5px] text-faint-fg">{{ advancedOpen ? '收起' : '展开' }}</span>
        </button>

        <div v-show="advancedOpen" class="mt-2 space-y-2 rounded-lg border border-line bg-panel-2/30 p-3">
          <div class="label-micro">份额与单仓微调</div>
          <div>
            <div
              v-for="(row, i) in advancedLimitRows"
              :key="row.field"
              class="flex items-center justify-between gap-3 py-1.5 text-[12px]"
              :class="i === 0 ? '' : 'border-t border-line'"
            >
              <span class="text-faint-fg">
                {{ row.label }}
                <span v-if="row.hint" class="ml-1 text-[10.5px] text-faint-fg">{{ row.hint }}</span>
              </span>
              <span class="flex items-center gap-1.5">
                <Input
                  v-model="editBufs[row.field]"
                  class="num h-7 w-24 text-right text-[12px]"
                  :type="row.int ? 'number' : 'text'"
                  :min="0"
                  :step="row.int ? 1 : 'any'"
                  :placeholder="row.bound ? boundText(row.bound) : ''"
                  :aria-label="row.label"
                />
                <span class="w-8 text-[10.5px] text-faint-fg">{{ row.unit }}</span>
              </span>
            </div>
          </div>

          <slot name="advanced-policy" />

          <SettingsExitConfigFields
            :exit="riskDoc.exit"
            :bufs="editBufs"
            @update:field="setEditBuf"
          />

          <!-- 执行策略规则：保留在卡内，共用底部唯一保存按钮。 -->
          <slot name="rules" />
        </div>
      </div>

      <div class="mt-3 flex flex-wrap items-center gap-2">
        <Button
          size="sm"
          :disabled="riskSaving || riskBusy"
          title="把本卡全部风控参数与执行规则一次性提交给内核"
          @click="$emit('save-all')"
        >
          <Save class="size-3.5" />{{ riskSaving ? '保存中…' : '保存' }}
        </Button>
        <Button
          variant="outline"
          size="sm"
          :disabled="riskBusy"
          title="重新读取内核的风控读数（放弃未保存的编辑）"
          @click="loadRisk"
        >
          <RefreshCw class="size-3.5" />刷新
        </Button>
      </div>
      <p v-if="riskErr" class="mt-2 text-[11px] leading-snug text-down">{{ riskErr }}</p>
    </template>
  </Card>
</template>
