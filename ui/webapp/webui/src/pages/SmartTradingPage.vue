<script setup lang="ts">
/**
 * 智能交易 · 聪明钱逆向策略引擎（RFC #249 · 完整最小可用闭环）。
 *
 * 原则：不跟单（避免 5m 轮次 200ms–2s 链上延迟与二手盘口滑点），而是
 * 「聪明钱雷达 → 拉取成交与行为指纹 → 逆向合成 Blueprint/Lua → 样本外回测打分」
 * 四步闭环，经真实延迟回测（--backtest-mode verify）验真后才由人工确认上架。
 */
import { computed, onMounted, ref } from 'vue'
import { ArrowRight, Compass, Cpu, FlaskConical, Play, Radar, Save, Search, ShieldAlert } from 'lucide-vue-next'
import {
  api,
  type BacktestDataset,
  type BacktestResultDoc,
  type RadarCandidateDoc,
  type SmartMoneyAnalyzeDoc,
  type StrategyArchetype,
} from '@/api/client'
import { usePanelStore } from '@/stores/panel'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'

const store = usePanelStore()

// ── 雷达筛选状态（Polymarket Leaderboard / Data API） ──
const radarMarket = ref<'5m' | '15m'>('5m')
const radarPeriod = ref<'DAY' | 'WEEK' | 'MONTH'>('WEEK')
const radarMinFills = ref('20')
const radarBusy = ref(false)
const radarErr = ref<string | null>(null)
const radarScannedCount = ref<number | null>(null)
const radarCandidates = ref<RadarCandidateDoc[]>([])

// ── 单钱包逆向分析台 ──
const walletInput = ref('')
const analyzeBusy = ref(false)
const analyzeErr = ref<string | null>(null)
const analyzeResult = ref<SmartMoneyAnalyzeDoc | null>(null)
const saveBusy = ref(false)
const saveNotice = ref<string | null>(null)

// ── 样本外回测验真状态（Stage 3） ──
const datasets = ref<BacktestDataset[]>([])
const selectedArchive = ref('')
const verifyBusy = ref(false)
const verifyStatusText = ref<string | null>(null)
const verifyResult = ref<BacktestResultDoc | null>(null)

const WALLET_RE = /^0x[a-fA-F0-9]{40}$/
const walletValid = computed(() => WALLET_RE.test(walletInput.value.trim()))

async function loadDatasets(): Promise<void> {
  try {
    const res = await api.backtestOnchainList()
    if (!res.error && Array.isArray(res.datasets)) {
      datasets.value = res.datasets
      if (!selectedArchive.value && res.datasets.length > 0) {
        selectedArchive.value = res.datasets[0].eventsPath
      }
    }
  } catch {
    // 无本地数据集时允许用户手动输入归档路径
  }
}
onMounted(() => void loadDatasets())

function archetypeLabel(a: StrategyArchetype): string {
  switch (a) {
    case 'late_favorite': return '尾盘顺势持有'
    case 'pair_arb': return '双边折价配对'
    case 'mid_round_momentum': return '中段动量突破'
    case 'cheap_option_hunter': return '低价非对称筹码'
    default: return '不可归类（拒收）'
  }
}

function archetypeVariant(a: StrategyArchetype): 'gold' | 'up' | 'default' {
  if (a === 'unclassifiable') return 'default'
  if (a === 'pair_arb') return 'up'
  return 'gold'
}

const blueprintText = computed(() => {
  const bp = analyzeResult.value?.blueprint
  return bp ? JSON.stringify(bp, null, 2) : null
})

const verifyTrades = computed(() => verifyResult.value?.report?.trades ?? null)

const verifyPassed = computed(() => {
  const t = verifyTrades.value
  if (!t) return false
  return t.closed >= 5 && t.netPnlUsd > 0 && (t.profitFactor ?? 0) >= 1.1
})

async function runRadarScan(): Promise<void> {
  if (radarBusy.value) return
  radarErr.value = null
  const minFills = Math.max(1, Number(radarMinFills.value) || 20)
  radarBusy.value = true
  try {
    const res = await api.smartMoneyScan({
      market: radarMarket.value,
      minBuyFills: minFills,
      limit: 15,
      timePeriod: radarPeriod.value,
    })
    if (res.error) {
      radarErr.value = res.error
      return
    }
    radarScannedCount.value = res.scannedWallets
    radarCandidates.value = res.candidates ?? []
  } catch (e) {
    radarErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    radarBusy.value = false
  }
}

async function submitWalletAnalysis(): Promise<void> {
  if (analyzeBusy.value) return
  analyzeErr.value = null
  saveNotice.value = null
  verifyResult.value = null
  const addr = walletInput.value.trim()
  if (!WALLET_RE.test(addr)) {
    analyzeErr.value = '请输入合法的 Polymarket 钱包地址（0x 开头的 40 位十六进制地址）。'
    return
  }
  analyzeBusy.value = true
  try {
    const res = await api.smartMoneyAnalyze(addr, radarMarket.value)
    if (res.error) {
      analyzeErr.value = res.error
      return
    }
    analyzeResult.value = res
  } catch (e) {
    analyzeErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    analyzeBusy.value = false
  }
}

function inspectCandidate(c: RadarCandidateDoc): void {
  walletInput.value = c.wallet
  saveNotice.value = null
  analyzeErr.value = null
  verifyResult.value = null
  analyzeResult.value = {
    fingerprint: {
      wallet: c.wallet,
      totalFills: c.buyFills,
      buyFills: c.buyFills,
      sellFills: 0,
      uniqueConditions: c.uniqueConditions,
      assets: c.assets,
      durationsSec: c.durationsSec,
      entryPxMin: c.entryPxP25,
      entryPxP10: c.entryPxP25,
      entryPxP25: c.entryPxP25,
      entryPxP50: c.entryPxP50,
      entryPxP75: c.entryPxP90,
      entryPxP90: c.entryPxP90,
      entryPxMax: c.entryPxP90,
      timeLeftP10: c.timeLeftP10,
      timeLeftP50: c.timeLeftP50,
      timeLeftP90: c.timeLeftP90,
      pairRatio: c.pairRatio,
      holdToSettlementRatio: c.holdToSettlementRatio,
      avgNotionalUsd: c.avgNotionalUsd,
      resolvedRounds: 0,
      wins: 0,
      winRatePct: 0,
      netPnlUsd: c.leaderboardPnlUsd,
      profitFactor: null,
    },
    archetype: c.archetype,
    reason: c.reason,
    blueprint: c.blueprint ?? null,
  }
}

async function saveSynthesizedBlueprint(): Promise<string | null> {
  const bp = analyzeResult.value?.blueprint
  if (!bp || saveBusy.value) return null
  const name = typeof bp.name === 'string' ? bp.name.trim() : ''
  if (!name) {
    analyzeErr.value = '合成的 Blueprint 缺少策略名。'
    return null
  }
  saveBusy.value = true
  saveNotice.value = null
  analyzeErr.value = null
  try {
    const res = await api.blueprintSave({
      name,
      json: JSON.stringify(bp),
      overwrite: true,
    })
    if (res.error) {
      analyzeErr.value = res.error
      return null
    }
    saveNotice.value = `已保存策略包 ${res.name}（默认 disabled，含 ${res.tunables.length} 个可进化参数）。`
    return res.name
  } catch (e) {
    analyzeErr.value = e instanceof Error ? e.message : String(e)
    return null
  } finally {
    saveBusy.value = false
  }
}

async function saveAndVerifyOutOfSample(): Promise<void> {
  if (verifyBusy.value) return
  const archive = selectedArchive.value.trim()
  if (!archive) {
    analyzeErr.value = '请先选择或填写回测语料路径（events.jsonl / data/onchain/*.jsonl）。'
    return
  }
  verifyBusy.value = true
  verifyResult.value = null
  verifyStatusText.value = '正在保存 Blueprint 并编译沙箱 Lua…'
  try {
    const name = await saveSynthesizedBlueprint()
    if (!name) return
    verifyStatusText.value = '正在执行样本外真实延迟回测（verify · slippage=1 tick）…'
    const runRes = await api.backtestRun({
      archive,
      mode: 'verify',
      slippageTicks: 1,
      strategies: [name],
    })
    if (!runRes.jobId) {
      analyzeErr.value = '回测任务启动失败'
      return
    }
    for (let i = 0; i < 60; i++) {
      await new Promise((r) => setTimeout(r, 1000))
      const st = await api.backtestStatus(runRes.jobId)
      if (st.state === 'done' && st.result) {
        verifyResult.value = st.result
        verifyStatusText.value = null
        return
      }
      if (st.state === 'failed') {
        analyzeErr.value = st.error || st.detail || '样本外回测失败'
        verifyStatusText.value = null
        return
      }
      verifyStatusText.value = `样本外回测运行中（${st.detail || st.state}）…`
    }
    analyzeErr.value = '回测超时，请前往「回测」页查看进度。'
  } catch (e) {
    analyzeErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    verifyBusy.value = false
    verifyStatusText.value = null
  }
}

function openBlueprintPage(): void {
  store.requestNav('blueprint')
}

function openBacktestPage(): void {
  store.requestNav('backtest')
}
</script>

<template>
  <div class="space-y-4">
    <!-- 顶部 4 步闭环概览 -->
    <Card>
      <CardHeader label="智能交易 · 聪明钱逆向策略引擎">
        <template #title>
          <Radar class="size-4 text-primary" />
        </template>
        <template #action>
          <Badge variant="gold" dot>全链路闭环就绪 · 不跟单零延迟</Badge>
        </template>
      </CardHeader>

      <p class="text-[12px] leading-relaxed text-muted-fg">
        不直接跟单（彻底规避 5m 轮次 200ms–2s 链上延迟与二手盘口滑点），而是扫描优质聪明钱地址的历史成交，
        提取行为指纹并逆向编译为本地 Blueprint / Lua 策略，经样本外带延迟回测（<code class="text-[11px]">--backtest-mode verify</code>）验真后由人工审核上架。
      </p>

      <div class="mt-3 grid gap-2 sm:grid-cols-4">
        <div class="rounded-lg border border-line bg-panel-2/40 p-2.5">
          <div class="flex items-center justify-between text-[11px] font-semibold text-fg">
            <span>① 聪明钱雷达</span>
            <Badge variant="up">已接通</Badge>
          </div>
          <p class="mt-1 text-[11px] text-faint-fg">扫描 Polymarket Crypto 榜单，过滤 5m/15m 优质地址</p>
        </div>
        <div class="rounded-lg border border-line bg-panel-2/40 p-2.5">
          <div class="flex items-center justify-between text-[11px] font-semibold text-fg">
            <span>② 行为指纹</span>
            <Badge variant="up">已接通</Badge>
          </div>
          <p class="mt-1 text-[11px] text-faint-fg">入场价分位数、剩余时间窗口、配对率与持有到期率</p>
        </div>
        <div class="rounded-lg border border-line bg-panel-2/40 p-2.5">
          <div class="flex items-center justify-between text-[11px] font-semibold text-fg">
            <span>③ 逆向蓝图</span>
            <Badge variant="up">已接通</Badge>
          </div>
          <p class="mt-1 text-[11px] text-faint-fg">合成合规 v1 Blueprint，一键保存沙箱 Lua + 可进化旋钮</p>
        </div>
        <div class="rounded-lg border border-line bg-panel-2/40 p-2.5">
          <div class="flex items-center justify-between text-[11px] font-semibold text-fg">
            <span>④ 样本外验真</span>
            <Badge variant="up">已接通</Badge>
          </div>
          <p class="mt-1 text-[11px] text-faint-fg">真实延迟 + 官方费率回测打分，正期望才建议上架</p>
        </div>
      </div>
    </Card>

    <!-- 区块 A：全网聪明钱雷达候选池 -->
    <Card>
      <CardHeader label="聪明钱雷达候选池">
        <template #title>
          <Compass class="size-4 text-faint-fg" />
        </template>
        <template #action>
          <span class="num text-[11px] text-faint-fg">
            <template v-if="radarScannedCount !== null">已扫描 {{ radarScannedCount }} 位榜首 · </template>
            命中 {{ radarCandidates.length }} 个
          </span>
        </template>
      </CardHeader>

      <div class="mt-2 flex flex-wrap items-center gap-2">
        <div class="flex items-center gap-1 rounded-md border border-line bg-panel-2 p-0.5 text-[11px]">
          <button
            type="button"
            class="rounded px-2 py-1 font-medium transition-colors"
            :class="radarMarket === '5m' ? 'bg-primary/15 text-fg' : 'text-muted-fg hover:text-fg'"
            @click="radarMarket = '5m'"
          >5m 轮次</button>
          <button
            type="button"
            class="rounded px-2 py-1 font-medium transition-colors"
            :class="radarMarket === '15m' ? 'bg-primary/15 text-fg' : 'text-muted-fg hover:text-fg'"
            @click="radarMarket = '15m'"
          >15m 轮次</button>
        </div>
        <div class="flex items-center gap-1 rounded-md border border-line bg-panel-2 p-0.5 text-[11px]">
          <button
            v-for="p in (['DAY', 'WEEK', 'MONTH'] as const)"
            :key="p"
            type="button"
            class="rounded px-2 py-1 font-medium transition-colors"
            :class="radarPeriod === p ? 'bg-primary/15 text-fg' : 'text-muted-fg hover:text-fg'"
            @click="radarPeriod = p"
          >{{ p === 'DAY' ? '日榜' : p === 'WEEK' ? '周榜' : '月榜' }}</button>
        </div>
        <label class="flex items-center gap-1.5 text-[11.5px] text-muted-fg">
          <span>最小买入笔数</span>
          <Input v-model="radarMinFills" class="num h-7 w-20 text-right text-[12px]" type="number" min="20" step="10" />
        </label>
        <Button size="sm" :disabled="radarBusy" @click="runRadarScan">
          <Radar class="size-3.5" />{{ radarBusy ? '扫榜中…' : '扫描雷达' }}
        </Button>
      </div>

      <AlertBanner v-if="radarErr" class="mt-3" tone="warn" dismissible @dismiss="radarErr = null">
        {{ radarErr }}
      </AlertBanner>

      <div v-if="radarCandidates.length === 0" class="mt-3 rounded-lg border border-dashed border-line bg-panel-2/20 p-4 text-center">
        <p class="text-[12px] text-muted-fg">
          {{ radarBusy ? '正在向 Polymarket Data API 拉取 Crypto 榜首与 5m/15m 成交指纹…' : '点击「扫描雷达」拉取 Polymarket Crypto 榜首，自动筛出可逆向的 5m/15m 候选地址。' }}
        </p>
      </div>

      <div v-else class="mt-3 overflow-x-auto">
        <table class="w-full text-left text-[12px]">
          <thead class="border-b border-line text-[11px] text-faint-fg">
            <tr>
              <th class="py-1.5 pr-3">交易员 / 地址</th>
              <th class="py-1.5 pr-3">骨架分类</th>
              <th class="py-1.5 pr-3 text-right">买入笔数</th>
              <th class="py-1.5 pr-3 text-right">入场价 (p25–p90)</th>
              <th class="py-1.5 pr-3 text-right">剩余窗 (p10–p90)</th>
              <th class="py-1.5 pr-3 text-right">榜单 PnL</th>
              <th class="py-1.5 text-right">操作</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="c in radarCandidates" :key="c.wallet" class="border-t border-line">
              <td class="py-2 pr-3">
                <div class="font-medium text-fg">{{ c.label }}</div>
                <div class="font-mono text-[10.5px] text-faint-fg">{{ c.wallet.slice(0, 8) }}…{{ c.wallet.slice(-6) }}</div>
              </td>
              <td class="py-2 pr-3">
                <Badge :variant="archetypeVariant(c.archetype)">{{ archetypeLabel(c.archetype) }}</Badge>
              </td>
              <td class="num py-2 pr-3 text-right">{{ c.buyFills }}</td>
              <td class="num py-2 pr-3 text-right">{{ c.entryPxP25.toFixed(2) }}–{{ c.entryPxP90.toFixed(2) }}</td>
              <td class="num py-2 pr-3 text-right">{{ c.timeLeftP10 }}s–{{ c.timeLeftP90 }}s</td>
              <td class="num py-2 pr-3 text-right" :class="c.leaderboardPnlUsd >= 0 ? 'text-up' : 'text-down'">
                {{ c.leaderboardPnlUsd >= 0 ? '+' : '' }}${{ c.leaderboardPnlUsd.toFixed(0) }}
              </td>
              <td class="py-2 text-right">
                <Button variant="outline" size="sm" @click="inspectCandidate(c)">载入逆向</Button>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </Card>

    <!-- 区块 B：单钱包逆向分析台 + 样本外回测打分 -->
    <Card>
      <CardHeader label="单钱包逆向与样本外验真">
        <template #title>
          <Cpu class="size-4 text-faint-fg" />
        </template>
        <template #action>
          <div class="flex items-center gap-1.5">
            <Button variant="outline" size="sm" @click="openBlueprintPage">
              蓝图画布 <ArrowRight class="size-3" />
            </Button>
            <Button variant="outline" size="sm" @click="openBacktestPage">
              <FlaskConical class="size-3.5" /> 回测页
            </Button>
          </div>
        </template>
      </CardHeader>

      <div class="mt-2 flex flex-wrap items-end gap-2">
        <label class="min-w-[260px] flex-1">
          <span class="label-micro">目标聪明钱地址（Polymarket 0x 地址）</span>
          <Input
            v-model="walletInput"
            class="mt-1 font-mono text-[12px]"
            type="text"
            placeholder="0x..."
            aria-label="目标聪明钱地址"
          />
        </label>
        <Button size="sm" :disabled="!walletInput.trim() || analyzeBusy" @click="submitWalletAnalysis">
          <Search class="size-3.5" />{{ analyzeBusy ? '分析中…' : '分析与逆向' }}
        </Button>
      </div>

      <AlertBanner v-if="analyzeErr" class="mt-3" tone="warn" dismissible @dismiss="analyzeErr = null">
        {{ analyzeErr }}
      </AlertBanner>
      <AlertBanner v-if="saveNotice" class="mt-3" tone="info" dismissible @dismiss="saveNotice = null">
        {{ saveNotice }}
      </AlertBanner>
      <AlertBanner v-if="verifyStatusText" class="mt-3" tone="info">
        {{ verifyStatusText }}
      </AlertBanner>

      <div v-if="analyzeResult" class="mt-3 space-y-3">
        <div class="grid gap-3 sm:grid-cols-2">
          <div class="rounded-lg border border-line bg-panel-2/30 p-3 text-[12px]">
            <div class="flex items-center justify-between">
              <span class="label-micro">行为指纹</span>
              <Badge :variant="archetypeVariant(analyzeResult.archetype)">{{ archetypeLabel(analyzeResult.archetype) }}</Badge>
            </div>
            <p class="mt-1.5 text-[11.5px] text-muted-fg">{{ analyzeResult.reason }}</p>
            <dl class="mt-2.5 space-y-1 text-[11.5px]">
              <div class="flex justify-between"><dt class="text-faint-fg">买入样本 / 唯一市场</dt><dd class="num">{{ analyzeResult.fingerprint.buyFills }} 笔 / {{ analyzeResult.fingerprint.uniqueConditions }} 场</dd></div>
              <div class="flex justify-between"><dt class="text-faint-fg">交易标的</dt><dd class="num">{{ analyzeResult.fingerprint.assets.join(', ') || '—' }}</dd></div>
              <div class="flex justify-between"><dt class="text-faint-fg">入场价分位 (p25 / p50 / p90)</dt><dd class="num">{{ analyzeResult.fingerprint.entryPxP25.toFixed(2) }} / {{ analyzeResult.fingerprint.entryPxP50.toFixed(2) }} / {{ analyzeResult.fingerprint.entryPxP90.toFixed(2) }}</dd></div>
              <div class="flex justify-between"><dt class="text-faint-fg">剩余时间窗 (p10 / p50 / p90)</dt><dd class="num">{{ analyzeResult.fingerprint.timeLeftP10 }}s / {{ analyzeResult.fingerprint.timeLeftP50 }}s / {{ analyzeResult.fingerprint.timeLeftP90 }}s</dd></div>
              <div class="flex justify-between"><dt class="text-faint-fg">双边配对率 / 持有到期率</dt><dd class="num">{{ (analyzeResult.fingerprint.pairRatio * 100).toFixed(1) }}% / {{ (analyzeResult.fingerprint.holdToSettlementRatio * 100).toFixed(1) }}%</dd></div>
              <div class="flex justify-between"><dt class="text-faint-fg">单笔均额</dt><dd class="num">${{ analyzeResult.fingerprint.avgNotionalUsd.toFixed(2) }}</dd></div>
            </dl>
          </div>

          <div class="rounded-lg border border-line bg-panel-2/30 p-3 text-[12px]">
            <div class="flex flex-wrap items-center justify-between gap-1.5">
              <span class="label-micro">逆向 Blueprint 产物</span>
              <div v-if="blueprintText" class="flex items-center gap-1.5">
                <Button variant="outline" size="sm" :disabled="saveBusy || verifyBusy" @click="saveSynthesizedBlueprint">
                  <Save class="size-3.5" />{{ saveBusy ? '保存中…' : '保存策略包' }}
                </Button>
                <Button size="sm" :disabled="saveBusy || verifyBusy || !selectedArchive.trim()" @click="saveAndVerifyOutOfSample">
                  <Play class="size-3.5" />{{ verifyBusy ? '回测中…' : '保存并回测打分' }}
                </Button>
              </div>
            </div>
            <div v-if="blueprintText" class="mt-2">
              <label class="block text-[10.5px] text-faint-fg">
                样本外回测语料路径（<code>--backtest-mode verify</code>）
                <Input v-model="selectedArchive" class="mt-1 h-7 font-mono text-[11px]" type="text" placeholder="data/archive/events.jsonl" />
              </label>
            </div>
            <pre v-if="blueprintText" class="mt-2 max-h-40 overflow-auto rounded bg-panel p-2 text-[11px]">{{ blueprintText }}</pre>
            <p v-else class="mt-2 text-[11.5px] text-muted-fg">{{ analyzeResult.reason }}</p>
          </div>
        </div>

        <!-- 样本外回测打分结果 -->
        <div v-if="verifyTrades" class="rounded-lg border border-line bg-panel-2/40 p-3">
          <div class="flex items-center justify-between">
            <span class="label-micro">样本外真实延迟回测打分（verify · slippage=1 tick）</span>
            <Badge :variant="verifyPassed ? 'up' : 'default'" dot>
              {{ verifyPassed ? '正期望通过 · 可去策略页启用' : '未达正期望门槛 · 建议继续进化或淘汰' }}
            </Badge>
          </div>
          <div class="mt-2 grid grid-cols-2 gap-2 text-[12px] sm:grid-cols-5">
            <div><span class="text-[10.5px] text-faint-fg">成交笔数</span><div class="num font-semibold">{{ verifyTrades.closed }}</div></div>
            <div><span class="text-[10.5px] text-faint-fg">胜率 (W/L)</span><div class="num font-semibold">{{ verifyTrades.winRatePct.toFixed(1) }}% ({{ verifyTrades.wins }}/{{ verifyTrades.losses }})</div></div>
            <div><span class="text-[10.5px] text-faint-fg">净收益</span><div class="num font-semibold" :class="verifyTrades.netPnlUsd >= 0 ? 'text-up' : 'text-down'">{{ verifyTrades.netPnlUsd >= 0 ? '+' : '' }}${{ verifyTrades.netPnlUsd.toFixed(2) }}</div></div>
            <div><span class="text-[10.5px] text-faint-fg">最大回撤</span><div class="num font-semibold">${{ verifyTrades.maxDrawdownUsd.toFixed(2) }}</div></div>
            <div><span class="text-[10.5px] text-faint-fg">盈亏比 (PF)</span><div class="num font-semibold">{{ verifyTrades.profitFactor?.toFixed(2) ?? '∞' }}</div></div>
          </div>
        </div>
      </div>

      <div v-else class="mt-3 flex items-start gap-2 rounded-lg border border-line bg-panel-2/20 p-3 text-[11.5px] text-faint-fg">
        <ShieldAlert class="mt-0.5 size-3.5 shrink-0 text-primary" />
        <span>
          Fail-Closed 安全边界：样本 &lt; 20 笔或行为离散的钱包将直接拒收（<code>unclassifiable</code>），双边套利地址归为
          <code>pair_arb</code> 不强塞单边蓝图；所有逆向策略默认 <strong>disabled</strong>，须通过样本外真实延迟回测后人工启用。
          <span v-if="walletValid" class="ml-1 text-primary">当前地址格式合法。</span>
        </span>
      </div>
    </Card>
  </div>
</template>
