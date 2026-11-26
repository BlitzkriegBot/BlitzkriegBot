<script setup lang="ts">
/**
 * 智能交易 · 聪明钱逆向策略工作台（RFC 249 · 完整闭环 + 统一设计系统）。
 *
 * 不直接跟单（规避 5m 轮次链上延迟与二手盘口滑点），通过四步量化研究流：
 *   1. 全网聪明钱雷达扫榜（smart_money.scan）
 *   2. 目标地址行为指纹提取（smart_money.analyze）
 *   3. 自动合成可进化 Blueprint JSON 并一键保存（blueprint.save）
 *   4. 样本外带延迟回测验真（backtest.run mode=verify, slippageTicks=1）
 *
 * 所有表单控件（Input / Select / SegmentedControl / Button）严格复用统一
 * 高度阶梯（sm=32px, default=36px）与 FormField 基线包装。
 */
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import {
  ArrowUpRight,
  Code2,
  Cpu,
  FlaskConical,
  Radar,
  RefreshCw,
  Save,
  Search,
  SlidersHorizontal,
} from 'lucide-vue-next'
import {
  api,
  ApiError,
  type BacktestDataset,
  type BacktestResultDoc,
  type RadarCandidateDoc,
  type SmartMoneyAnalyzeDoc,
  type StrategyArchetype,
} from '@/api/client'
import { money, pct, shortAddr, signedMoney } from '@/lib/format'
import { usePanelStore } from '@/stores/panel'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import FormField from '@/components/ui/field/FormField.vue'
import Input from '@/components/ui/input/Input.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'
import SegmentedControl from '@/components/ui/segmented/SegmentedControl.vue'
import Select from '@/components/ui/select/Select.vue'

const store = usePanelStore()

// ── 顶部控制条 & 雷达状态 ────────────────────────────────────────────────────

const MARKET_SEGMENTS = [
  { id: '5m', label: '5m 轮次' },
  { id: '15m', label: '15m 轮次' },
]

const PERIOD_OPTIONS = [
  { value: 'DAY', label: '近 24 小时' },
  { value: 'WEEK', label: '近 7 天' },
  { value: 'MONTH', label: '近 30 天' },
]

const ARCHETYPE_FILTER_OPTIONS = [
  { value: 'all', label: '全部原型' },
  { value: 'late_favorite', label: '尾盘顺势持有' },
  { value: 'pair_arb', label: '双边折价配对' },
  { value: 'mid_round_momentum', label: '中段动量突破' },
  { value: 'cheap_option_hunter', label: '低价非对称筹码' },
]

const radarMarket = ref<'5m' | '15m'>('5m')
const radarPeriod = ref<'DAY' | 'WEEK' | 'MONTH'>('WEEK')
const radarMinFills = ref('20')
const radarSearch = ref('')
const radarArchetypeFilter = ref('all')

const scanning = ref(false)
const scanErr = ref<string | null>(null)
const scannedWallets = ref(0)
const radarCandidates = ref<RadarCandidateDoc[]>([])

const filteredCandidates = computed<RadarCandidateDoc[]>(() => {
  const q = radarSearch.value.trim().toLowerCase()
  return radarCandidates.value.filter((c) => {
    if (radarArchetypeFilter.value !== 'all' && c.archetype !== radarArchetypeFilter.value) {
      return false
    }
    if (!q) return true
    return c.wallet.toLowerCase().includes(q) || c.label.toLowerCase().includes(q)
  })
})

function onMarketChange(v: string): void {
  radarMarket.value = v === '15m' ? '15m' : '5m'
}

function onPeriodChange(v: string): void {
  if (v === 'DAY' || v === 'WEEK' || v === 'MONTH') radarPeriod.value = v
}

function archetypeLabel(a: StrategyArchetype): string {
  switch (a) {
    case 'late_favorite':
      return '尾盘顺势持有'
    case 'pair_arb':
      return '双边折价配对'
    case 'mid_round_momentum':
      return '中段动量突破'
    case 'cheap_option_hunter':
      return '低价非对称筹码'
    default:
      return '不可归类'
  }
}

function archetypeVariant(a: StrategyArchetype): 'gold' | 'up' | 'info' | 'default' {
  switch (a) {
    case 'late_favorite':
      return 'gold'
    case 'pair_arb':
      return 'up'
    case 'mid_round_momentum':
      return 'info'
    default:
      return 'default'
  }
}

async function runRadarScan(): Promise<void> {
  scanning.value = true
  scanErr.value = null
  try {
    const minBuyFills = Math.max(5, Math.floor(Number(radarMinFills.value) || 20))
    const res = await api.smartMoneyScan({
      market: radarMarket.value,
      minBuyFills,
      limit: 30,
      timePeriod: radarPeriod.value,
    })
    if (res.error) throw new Error(res.error)
    scannedWallets.value = res.scannedWallets ?? 0
    radarCandidates.value = res.candidates ?? []
  } catch (e) {
    scanErr.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    scanning.value = false
  }
}

// ── 单钱包逆向分析与蓝图合成 ─────────────────────────────────────────────────

const WALLET_RE = /^0x[a-fA-F0-9]{40}$/
const walletInput = ref('')
const analyzing = ref(false)
const analyzeErr = ref<string | null>(null)
const analysis = ref<SmartMoneyAnalyzeDoc | null>(null)
const showJsonPreview = ref(false)

const saveName = ref('')
const saving = ref(false)
const saveErr = ref<string | null>(null)
const savedStrategyName = ref<string | null>(null)

const blueprintJsonText = computed(() => {
  const bp = analysis.value?.blueprint
  return bp ? JSON.stringify(bp, null, 2) : null
})

function selectCandidate(c: RadarCandidateDoc): void {
  walletInput.value = c.wallet
  void runWalletAnalyze()
}

async function runWalletAnalyze(): Promise<void> {
  analyzeErr.value = null
  saveErr.value = null
  savedStrategyName.value = null
  verifyErr.value = null
  verifyResult.value = null

  const addr = walletInput.value.trim()
  if (!WALLET_RE.test(addr)) {
    analyzeErr.value = '请输入合法的 Polymarket 0x 钱包地址（42 位十六进制）。'
    return
  }

  analyzing.value = true
  try {
    const res = await api.smartMoneyAnalyze(addr, radarMarket.value)
    if (res.error) throw new Error(res.error)
    analysis.value = res
    const bpName =
      res.blueprint && typeof res.blueprint.name === 'string' && res.blueprint.name.trim()
        ? res.blueprint.name.trim()
        : `sm_${addr.slice(2, 10).toLowerCase()}`
    saveName.value = bpName
  } catch (e) {
    analyzeErr.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    analyzing.value = false
  }
}

async function saveReverseBlueprint(): Promise<void> {
  saveErr.value = null
  const json = blueprintJsonText.value
  const name = saveName.value.trim()
  if (!json || !name) {
    saveErr.value = '缺少可保存的蓝图 JSON 或策略名称。'
    return
  }
  saving.value = true
  try {
    const res = await api.blueprintSave({ name, json, overwrite: true })
    if (res.error) throw new Error(res.error)
    savedStrategyName.value = res.name
  } catch (e) {
    saveErr.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    saving.value = false
  }
}

function openInBlueprintCanvas(): void {
  if (savedStrategyName.value) {
    store.requestBlueprintPreload(savedStrategyName.value)
  } else {
    store.requestNav('blueprint')
  }
}

// ── 样本外回测验真闭环（Stage 3） ────────────────────────────────────────────

const datasets = ref<BacktestDataset[]>([])
const selectedDataset = ref('')
const loadingDatasets = ref(false)
const verifying = ref(false)
const verifyErr = ref<string | null>(null)
const verifyProgress = ref<string | null>(null)
const verifyResult = ref<BacktestResultDoc | null>(null)
let pollTimer: ReturnType<typeof setTimeout> | null = null

const datasetOptions = computed(() =>
  datasets.value.map((d) => ({
    value: d.eventsPath,
    label: `${d.dataset}${d.manifest?.counts?.events ? ` (${d.manifest.counts.events} events)` : ''}`,
  })),
)

const verifyPositive = computed(() => {
  const r = verifyResult.value
  if (!r) return false
  return Number(r.report.trades.netPnlUsd) > 0 && Number(r.report.trades.closed) > 0
})

async function refreshDatasets(): Promise<void> {
  loadingDatasets.value = true
  try {
    const res = await api.backtestOnchainList()
    datasets.value = res.datasets ?? []
    if (!selectedDataset.value && datasets.value.length > 0) {
      selectedDataset.value = datasets.value[0].eventsPath
    }
  } catch {
    // 离线或无数据集时保持空列表
  } finally {
    loadingDatasets.value = false
  }
}

function stopPoll(): void {
  if (pollTimer !== null) {
    clearTimeout(pollTimer)
    pollTimer = null
  }
}

async function runOosVerify(): Promise<void> {
  verifyErr.value = null
  verifyResult.value = null
  verifyProgress.value = null

  if (!selectedDataset.value) {
    verifyErr.value = '请先选择一个本地样本外数据集（可在回测页拉取链上数据）。'
    return
  }

  // 若尚未保存策略包，先自动落盘再跑验真
  if (!savedStrategyName.value) {
    await saveReverseBlueprint()
    if (!savedStrategyName.value) return
  }

  verifying.value = true
  verifyProgress.value = '提交样本外 verify 回测任务…'
  try {
    const start = await api.backtestRun({
      archive: selectedDataset.value,
      strategies: [savedStrategyName.value],
      mode: 'verify',
      slippageTicks: 1,
    })
    if (!start.jobId) {
      throw new Error('回测任务未返回 jobId')
    }
    const jobId = start.jobId
    const poll = async (): Promise<void> => {
      try {
        const st = await api.backtestStatus(jobId)
        if (st.state === 'done' && st.result) {
          verifyResult.value = st.result
          verifyProgress.value = null
          verifying.value = false
          return
        }
        if (st.state === 'failed') {
          verifyErr.value = st.error || '样本外回测执行失败。'
          verifyProgress.value = null
          verifying.value = false
          return
        }
        verifyProgress.value = st.detail || (st.progress > 0 ? `样本外回测进度 ${Math.round(st.progress * 100)}%…` : '样本外回测运行中…')
        pollTimer = setTimeout(() => {
          void poll()
        }, 1000)
      } catch (e) {
        verifyErr.value = e instanceof ApiError ? e.message : String(e)
        verifyProgress.value = null
        verifying.value = false
      }
    }
    void poll()
  } catch (e) {
    verifyErr.value = e instanceof ApiError ? e.message : String(e)
    verifyProgress.value = null
    verifying.value = false
  }
}

onMounted(() => {
  void refreshDatasets()
})

onBeforeUnmount(() => {
  stopPoll()
})
</script>

<template>
  <div class="space-y-4">
    <!-- 顶部命令条：统一 h-9 (36px) 控件系统，零高度错位 -->
    <Card class="rise-in">
      <div class="flex flex-col gap-4 lg:flex-row lg:items-center lg:justify-between">
        <div class="min-w-0">
          <div class="flex items-center gap-2">
            <Radar class="size-4 shrink-0 text-primary" />
            <h1 class="text-[15px] font-semibold tracking-tight text-fg">智能交易 · 聪明钱逆向工作台</h1>
            <Badge variant="gold" dot>零延迟逆向</Badge>
          </div>
          <p class="mt-1 text-[12.5px] text-muted-fg">
            扫描 5m/15m 优质地址成交指纹 → 自动合成可进化蓝图策略 → 样本外带滑点回测验真，不直接跟单。
          </p>
        </div>

        <div class="flex flex-wrap items-center gap-2">
          <SegmentedControl
            :model-value="radarMarket"
            :segments="MARKET_SEGMENTS"
            @update:model-value="onMarketChange"
          />
          <Select
            :model-value="radarPeriod"
            :options="PERIOD_OPTIONS"
            aria-label="扫描时间窗口"
            class="w-32"
            @update:model-value="onPeriodChange"
          />
          <Button variant="gold" :disabled="scanning" @click="runRadarScan">
            <RefreshCw class="size-3.5" :class="scanning && 'animate-spin'" />
            {{ scanning ? '扫描中…' : '扫描雷达' }}
          </Button>
          <Button variant="outline" @click="store.requestNav('blueprint')">
            蓝图画布
            <ArrowUpRight class="size-3.5" />
          </Button>
        </div>
      </div>

      <AlertBanner v-if="scanErr" class="mt-3" tone="warn" dismissible @dismiss="scanErr = null">
        {{ scanErr }}
      </AlertBanner>
    </Card>

    <!-- 主工作区：左 7 列雷达候选表 + 右 5 列逆向分析与验真台 -->
    <div class="grid gap-4 lg:grid-cols-12">
      <!-- 左侧：聪明钱雷达候选池 -->
      <Card class="lg:col-span-7">
        <CardHeader label="聪明钱雷达候选池">
          <template #title>
            <SlidersHorizontal class="size-4 text-faint-fg" />
          </template>
          <template #action>
            <div class="flex items-center gap-2 text-[11.5px] text-faint-fg num">
              <span v-if="scannedWallets > 0">已扫 <RollingNumber :value="scannedWallets" /> 钱包</span>
              <Badge variant="default">
                <RollingNumber :value="filteredCandidates.length" /> 候选
              </Badge>
            </div>
          </template>
        </CardHeader>

        <!-- 统一 sm (h-8 / 32px) 过滤工具条，所有控件严格等高 -->
        <div class="mt-3 grid gap-2 sm:grid-cols-12 items-end">
          <FormField label="地址 / 标签检索" class="sm:col-span-5">
            <Input
              v-model="radarSearch"
              size="sm"
              placeholder="筛选 0x 地址或用户名…"
            />
          </FormField>
          <FormField label="策略原型" class="sm:col-span-4">
            <Select
              v-model="radarArchetypeFilter"
              size="sm"
              :options="ARCHETYPE_FILTER_OPTIONS"
              aria-label="按策略原型筛选"
            />
          </FormField>
          <FormField label="最小买入笔数" class="sm:col-span-3">
            <Input
              v-model="radarMinFills"
              size="sm"
              type="number"
              class="num text-right"
              placeholder="20"
            />
          </FormField>
        </div>

        <div v-if="filteredCandidates.length > 0" class="mt-3 overflow-x-auto">
          <table class="w-full text-left text-[12.5px]">
            <thead>
              <tr class="border-b border-line text-[11px] text-faint-fg">
                <th class="label-micro py-2 pr-2">地址</th>
                <th class="label-micro px-2 py-2">策略原型</th>
                <th class="label-micro px-2 py-2 text-right">榜单盈亏</th>
                <th class="label-micro px-2 py-2 text-right">买单</th>
                <th class="label-micro px-2 py-2 text-right">入场价带</th>
                <th class="label-micro py-2 pl-2 text-right">操作</th>
              </tr>
            </thead>
            <tbody class="divide-y divide-line/60">
              <tr
                v-for="c in filteredCandidates"
                :key="c.wallet"
                class="cursor-pointer transition-colors hover:bg-panel-2/60"
                :class="walletInput.trim().toLowerCase() === c.wallet.toLowerCase() && 'bg-primary/8'"
                @click="selectCandidate(c)"
              >
                <td class="py-2.5 pr-2">
                  <div class="font-mono text-[12px] text-fg">{{ shortAddr(c.wallet) }}</div>
                  <div v-if="c.label" class="truncate text-[11px] text-faint-fg max-w-[130px]">{{ c.label }}</div>
                </td>
                <td class="px-2 py-2.5">
                  <Badge :variant="archetypeVariant(c.archetype)">
                    {{ archetypeLabel(c.archetype) }}
                  </Badge>
                </td>
                <td
                  class="num px-2 py-2.5 text-right font-medium"
                  :class="c.leaderboardPnlUsd >= 0 ? 'text-up' : 'text-down'"
                >
                  {{ signedMoney(c.leaderboardPnlUsd) }}
                </td>
                <td class="num px-2 py-2.5 text-right text-muted-fg">{{ c.buyFills }}</td>
                <td class="num px-2 py-2.5 text-right text-muted-fg">
                  {{ c.entryPxP25.toFixed(2) }}–{{ c.entryPxP90.toFixed(2) }}
                </td>
                <td class="py-2.5 pl-2 text-right">
                  <Button variant="outline" size="sm" @click.stop="selectCandidate(c)">
                    逆向
                  </Button>
                </td>
              </tr>
            </tbody>
          </table>
        </div>

        <EmptyState
          v-else
          class="mt-4"
          :title="radarCandidates.length === 0 ? '尚未扫描聪明钱雷达' : '无匹配筛选条件的候选地址'"
          :description="radarCandidates.length === 0
            ? '点击上方「扫描雷达」拉取 Polymarket 5m/15m 加密榜单高胜率地址，或在右侧直接输入 0x 地址逆向。'
            : '放宽策略原型或关键词筛选条件以查看候选地址。'"
        >
          <Button v-if="radarCandidates.length === 0" variant="gold" size="sm" :disabled="scanning" @click="runRadarScan">
            <Radar class="size-3.5" />
            立即扫描雷达
          </Button>
        </EmptyState>
      </Card>

      <!-- 右侧：单钱包逆向分析与样本外验真台 -->
      <div class="space-y-4 lg:col-span-5">
        <Card>
          <CardHeader label="单钱包逆向与蓝图合成">
            <template #title>
              <Cpu class="size-4 text-faint-fg" />
            </template>
            <template #action>
              <Badge v-if="analysis" :variant="archetypeVariant(analysis.archetype)">
                {{ archetypeLabel(analysis.archetype) }}
              </Badge>
            </template>
          </CardHeader>

          <!-- 统一 default (h-9 / 36px) 输入 + 按钮，基线严格对齐 -->
          <div class="mt-3 flex items-end gap-2">
            <FormField label="Polymarket 钱包地址 (0x)" class="flex-1">
              <Input
                v-model="walletInput"
                class="font-mono"
                placeholder="0x..."
              />
            </FormField>
            <Button
              variant="gold"
              :disabled="analyzing || !walletInput.trim()"
              @click="runWalletAnalyze"
            >
              <Search class="size-3.5" />
              {{ analyzing ? '分析中…' : '逆向分析' }}
            </Button>
          </div>

          <AlertBanner v-if="analyzeErr" class="mt-3" tone="warn" dismissible @dismiss="analyzeErr = null">
            {{ analyzeErr }}
          </AlertBanner>

          <template v-if="analysis">
            <!-- 行为指纹 2x2 指标网格 -->
            <div class="mt-4 grid grid-cols-2 gap-2">
              <div class="rounded-md border border-line bg-panel-2/50 p-2.5">
                <div class="label-micro">入场价带 (p25–p90)</div>
                <div class="stat-num mt-1 text-[15px] font-semibold text-fg">
                  {{ analysis.fingerprint.entryPxP25.toFixed(2) }}–{{ analysis.fingerprint.entryPxP90.toFixed(2) }}
                </div>
              </div>
              <div class="rounded-md border border-line bg-panel-2/50 p-2.5">
                <div class="label-micro">剩余时间窗 (p10–p90)</div>
                <div class="stat-num mt-1 text-[15px] font-semibold text-fg">
                  {{ Math.round(analysis.fingerprint.timeLeftP10) }}s–{{ Math.round(analysis.fingerprint.timeLeftP90) }}s
                </div>
              </div>
              <div class="rounded-md border border-line bg-panel-2/50 p-2.5">
                <div class="label-micro">双边配对 / 持有到期</div>
                <div class="stat-num mt-1 text-[15px] font-semibold text-fg">
                  {{ pct(analysis.fingerprint.pairRatio * 100) }} / {{ pct(analysis.fingerprint.holdToSettlementRatio * 100) }}
                </div>
              </div>
              <div class="rounded-md border border-line bg-panel-2/50 p-2.5">
                <div class="label-micro">已结算胜率 / 净利</div>
                <div class="stat-num mt-1 text-[15px] font-semibold text-fg">
                  {{ pct(analysis.fingerprint.winRatePct) }} ·
                  <span :class="analysis.fingerprint.netPnlUsd >= 0 ? 'text-up' : 'text-down'">
                    {{ signedMoney(analysis.fingerprint.netPnlUsd) }}
                  </span>
                </div>
              </div>
            </div>

            <p class="mt-2.5 text-[12px] leading-relaxed text-muted-fg">{{ analysis.reason }}</p>

            <!-- 逆向 Blueprint 保存条：统一 h-9 控件 -->
            <div v-if="blueprintJsonText" class="mt-4 space-y-2.5 border-t border-line pt-3">
              <div class="flex items-end gap-2">
                <FormField label="逆向蓝图策略名" class="flex-1">
                  <Input v-model="saveName" class="font-mono" placeholder="sm_late_fav" />
                </FormField>
                <Button :disabled="saving || !saveName.trim()" @click="saveReverseBlueprint">
                  <Save class="size-3.5" />
                  {{ saving ? '保存中…' : '保存蓝图' }}
                </Button>
                <Button variant="outline" size="icon" title="查看蓝图 JSON" @click="showJsonPreview = !showJsonPreview">
                  <Code2 class="size-4" />
                </Button>
              </div>

              <AlertBanner v-if="savedStrategyName" tone="info" title="蓝图策略包已保存">
                <div class="flex flex-wrap items-center justify-between gap-2">
                  <span>策略包 <code class="font-mono font-semibold">{{ savedStrategyName }}</code> 已落盘，可直接进入画布编辑或立即跑样本外验真。</span>
                  <Button variant="outline" size="sm" @click="openInBlueprintCanvas">
                    在画布打开
                    <ArrowUpRight class="size-3.5" />
                  </Button>
                </div>
              </AlertBanner>

              <AlertBanner v-if="saveErr" tone="warn" dismissible @dismiss="saveErr = null">
                {{ saveErr }}
              </AlertBanner>

              <pre
                v-if="showJsonPreview"
                class="max-h-52 overflow-auto rounded-md border border-line bg-panel p-2.5 font-mono text-[11px] leading-relaxed text-muted-fg"
              >{{ blueprintJsonText }}</pre>
            </div>

            <AlertBanner v-else class="mt-3" tone="info">
              该地址未通过单腿可逆向校验（例如属于双边套利或样本不足），已按策略纪律拒收，不生成蓝图。
            </AlertBanner>
          </template>

          <EmptyState
            v-else
            class="mt-4"
            title="等待选择目标地址"
            description="在左侧雷达表点击任意聪明钱地址，或在上方输入 0x 地址，即可提取行为指纹并合成 Blueprint。"
          />
        </Card>

        <!-- 区块 C：样本外带延迟回测验真 -->
        <Card>
          <CardHeader label="样本外回测验真 (verify · 1 tick 滑点)">
            <template #title>
              <FlaskConical class="size-4 text-faint-fg" />
            </template>
            <template #action>
              <Button variant="ghost" size="sm" :disabled="loadingDatasets" @click="refreshDatasets">
                <RefreshCw class="size-3" :class="loadingDatasets && 'animate-spin'" />
                刷新语料
              </Button>
            </template>
          </CardHeader>

          <div class="mt-3 flex items-end gap-2">
            <FormField label="本地样本外数据集" class="flex-1">
              <Select
                v-model="selectedDataset"
                :options="datasetOptions.length > 0 ? datasetOptions : [{ value: '', label: '暂无可用数据集（请先在回测页拉取）' }]"
                :disabled="datasetOptions.length === 0"
                aria-label="选择样本外数据集"
              />
            </FormField>
            <Button
              variant="gold"
              :disabled="verifying || !blueprintJsonText || !selectedDataset"
              @click="runOosVerify"
            >
              <FlaskConical class="size-3.5" />
              {{ verifying ? '验真中…' : '样本外验真' }}
            </Button>
          </div>

          <p v-if="verifyProgress" class="mt-2 text-[12px] text-muted-fg">{{ verifyProgress }}</p>

          <AlertBanner v-if="verifyErr" class="mt-3" tone="warn" dismissible @dismiss="verifyErr = null">
            {{ verifyErr }}
          </AlertBanner>

          <div v-if="verifyResult" class="mt-3 space-y-2.5 border-t border-line pt-3">
            <div class="flex items-center justify-between">
              <span class="label-micro">验真结论</span>
              <Badge :variant="verifyPositive ? 'up' : 'down'" dot>
                {{ verifyPositive ? '正期望 · 可人工审核上架' : '未通过正期望门禁 · 拦截' }}
              </Badge>
            </div>
            <div class="grid grid-cols-3 gap-2">
              <div class="rounded-md border border-line bg-panel-2/50 p-2">
                <div class="label-micro">样本外净利</div>
                <div
                  class="stat-num mt-0.5 text-[14px] font-semibold"
                  :class="Number(verifyResult.report.trades.netPnlUsd) >= 0 ? 'text-up' : 'text-down'"
                >
                  {{ signedMoney(Number(verifyResult.report.trades.netPnlUsd)) }}
                </div>
              </div>
              <div class="rounded-md border border-line bg-panel-2/50 p-2">
                <div class="label-micro">成交笔数</div>
                <div class="stat-num mt-0.5 text-[14px] font-semibold text-fg">
                  <RollingNumber :value="Number(verifyResult.report.trades.closed)" />
                </div>
              </div>
              <div class="rounded-md border border-line bg-panel-2/50 p-2">
                <div class="label-micro">胜率 / 费后</div>
                <div class="stat-num mt-0.5 text-[14px] font-semibold text-fg">
                  {{ pct(Number(verifyResult.report.trades.winRatePct ?? 0)) }}
                </div>
              </div>
            </div>
            <div class="flex items-center justify-between text-[11.5px] text-faint-fg num">
              <span>手续费 {{ money(Number(verifyResult.report.trades.feesUsd ?? 0)) }}</span>
              <Button variant="outline" size="sm" @click="store.requestNav('backtest')">
                前往回测页看明细
                <ArrowUpRight class="size-3.5" />
              </Button>
            </div>
          </div>
        </Card>
      </div>
    </div>
  </div>
</template>
