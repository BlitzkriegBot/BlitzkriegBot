<script setup lang="ts">
/**
 * 智能交易 · 聪明钱逆向策略引擎（RFC #249 · 骨架 + WebUI）。
 *
 * 原则：不跟单（避免 5m 轮次 200ms–2s 链上延迟与二手盘口滑点），而是
 * 「聪明钱雷达 → 拉取成交与行为指纹 → 逆向合成 Blueprint/Lua → 样本外回测打分」
 * 四步闭环，经真实延迟回测验真后才由人工确认上架。
 */
import { computed, ref } from 'vue'
import { ArrowRight, Compass, Cpu, FlaskConical, Radar, Search, ShieldAlert } from 'lucide-vue-next'
import { usePanelStore } from '@/stores/panel'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'

interface RadarCandidate {
  wallet: string
  label: string
  market: '5m' | '15m'
  buyFills: number
  winRatePct: number
  netPnlUsd: number
  profitFactor: number | null
  archetype: 'late_favorite' | 'pair_arb' | 'mid_round_momentum' | 'cheap_option_hunter' | 'unclassifiable'
}

interface FingerprintView {
  wallet: string
  buyFills: number
  assets: string[]
  entryPxBand: string
  timeLeftBand: string
  pairRatioPct: number
  holdRatioPct: number
  archetype: RadarCandidate['archetype']
  reason: string
  blueprintJson: string | null
}

const store = usePanelStore()

// ── 雷达筛选状态（Stage 2 接入 Polymarket Leaderboard / Data API） ──
const radarMarket = ref<'5m' | '15m'>('5m')
const radarMinFills = ref('200')
const radarCandidates = ref<RadarCandidate[]>([])

// ── 单钱包逆向输入与状态（Stage 3 接通内核 smart_money.analyze + OOS 回测） ──
const walletInput = ref('')
const lookbackDays = ref('7')
const analyzeErr = ref<string | null>(null)
const analyzeNotice = ref<string | null>(null)
const activeFingerprint = ref<FingerprintView | null>(null)

const WALLET_RE = /^0x[a-fA-F0-9]{40}$/
const walletValid = computed(() => WALLET_RE.test(walletInput.value.trim()))

function archetypeLabel(a: RadarCandidate['archetype']): string {
  switch (a) {
    case 'late_favorite': return '尾盘顺势持有'
    case 'pair_arb': return '双边折价配对'
    case 'mid_round_momentum': return '中段动量突破'
    case 'cheap_option_hunter': return '低价非对称筹码'
    default: return '不可归类（拒收）'
  }
}

function submitWalletAnalysis(): void {
  analyzeErr.value = null
  analyzeNotice.value = null
  const addr = walletInput.value.trim()
  if (!WALLET_RE.test(addr)) {
    analyzeErr.value = '请输入合法的 Polymarket 钱包地址（0x 开头的 40 位十六进制地址）。'
    return
  }
  const days = Number(lookbackDays.value)
  if (!Number.isFinite(days) || days < 1 || days > 90) {
    analyzeErr.value = '回看天数需在 1–90 天之间。'
    return
  }
  analyzeNotice.value = `地址 ${addr.slice(0, 8)}…${addr.slice(-6)}（${radarMarket.value} · 近 ${days} 天）已通过格式校验；Stage 2/3 接通链上拉取与 smart_money.analyze IPC 后将在此直接输出行为指纹、逆向 Blueprint 与样本外回测评分。`
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
          <Badge variant="gold" dot>骨架就绪 · 不跟单零延迟逆向</Badge>
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
            <Badge variant="default">Stage 2</Badge>
          </div>
          <p class="mt-1 text-[11px] text-faint-fg">扫描 5m/15m Crypto 榜单，过滤低样本与赌狗地址</p>
        </div>
        <div class="rounded-lg border border-line bg-panel-2/40 p-2.5">
          <div class="flex items-center justify-between text-[11px] font-semibold text-fg">
            <span>② 行为指纹</span>
            <Badge variant="up">已建模</Badge>
          </div>
          <p class="mt-1 text-[11px] text-faint-fg">入场价分位数、剩余时间窗口、配对率与持有到期率</p>
        </div>
        <div class="rounded-lg border border-line bg-panel-2/40 p-2.5">
          <div class="flex items-center justify-between text-[11px] font-semibold text-fg">
            <span>③ 逆向蓝图</span>
            <Badge variant="up">已建模</Badge>
          </div>
          <p class="mt-1 text-[11px] text-faint-fg">合成合规 v1 Blueprint JSON，编译沙箱 Lua + 可进化旋钮</p>
        </div>
        <div class="rounded-lg border border-line bg-panel-2/40 p-2.5">
          <div class="flex items-center justify-between text-[11px] font-semibold text-fg">
            <span>④ 样本外验真</span>
            <Badge variant="default">Stage 3</Badge>
          </div>
          <p class="mt-1 text-[11px] text-faint-fg">真实延迟 + 官方费率回测打分，正期望才允许上架</p>
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
          <span class="num text-[11px] text-faint-fg">已收录 {{ radarCandidates.length }} 个地址</span>
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
        <label class="flex items-center gap-1.5 text-[11.5px] text-muted-fg">
          <span>最小买入笔数</span>
          <Input v-model="radarMinFills" class="num h-7 w-20 text-right text-[12px]" type="number" min="20" step="10" />
        </label>
        <Badge variant="default">自动扫榜将在 Stage 2 接入</Badge>
      </div>

      <div v-if="radarCandidates.length === 0" class="mt-3 rounded-lg border border-dashed border-line bg-panel-2/20 p-4 text-center">
        <p class="text-[12px] text-muted-fg">雷达候选池为空——Stage 2 接入 Polymarket Leaderboard / Data API 后将自动筛选 5m/15m Crypto 优质地址。</p>
        <p class="mt-1 text-[11px] text-faint-fg">现在可直接在下方输入任意已知聪明钱地址，提前验证地址与逆向工作流。</p>
      </div>
    </Card>

    <!-- 区块 B：单钱包逆向分析台 -->
    <Card>
      <CardHeader label="单钱包逆向分析台">
        <template #title>
          <Cpu class="size-4 text-faint-fg" />
        </template>
        <template #action>
          <div class="flex items-center gap-1.5">
            <Button variant="outline" size="sm" @click="openBlueprintPage">
              蓝图画布 <ArrowRight class="size-3" />
            </Button>
            <Button variant="outline" size="sm" @click="openBacktestPage">
              <FlaskConical class="size-3.5" /> 回测验真
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
        <label class="w-28">
          <span class="label-micro">回看天数</span>
          <Input v-model="lookbackDays" class="num mt-1 text-right text-[12px]" type="number" min="1" max="90" />
        </label>
        <Button size="sm" :disabled="!walletInput.trim()" @click="submitWalletAnalysis">
          <Search class="size-3.5" /> 分析与逆向
        </Button>
      </div>

      <AlertBanner v-if="analyzeErr" class="mt-3" tone="warn" dismissible @dismiss="analyzeErr = null">
        {{ analyzeErr }}
      </AlertBanner>
      <AlertBanner v-if="analyzeNotice" class="mt-3" tone="info" dismissible @dismiss="analyzeNotice = null">
        {{ analyzeNotice }}
      </AlertBanner>

      <div v-if="activeFingerprint" class="mt-3 grid gap-3 sm:grid-cols-2">
        <div class="rounded-lg border border-line bg-panel-2/30 p-3 text-[12px]">
          <div class="flex items-center justify-between">
            <span class="label-micro">行为指纹</span>
            <Badge variant="gold">{{ archetypeLabel(activeFingerprint.archetype) }}</Badge>
          </div>
          <dl class="mt-2 space-y-1 text-[11.5px]">
            <div class="flex justify-between"><dt class="text-faint-fg">买入样本</dt><dd class="num">{{ activeFingerprint.buyFills }} 笔</dd></div>
            <div class="flex justify-between"><dt class="text-faint-fg">入场价分位 (p25–p90)</dt><dd class="num">{{ activeFingerprint.entryPxBand }}</dd></div>
            <div class="flex justify-between"><dt class="text-faint-fg">剩余时间窗口 (p10–p90)</dt><dd class="num">{{ activeFingerprint.timeLeftBand }}</dd></div>
            <div class="flex justify-between"><dt class="text-faint-fg">双边配对率 / 持有到期率</dt><dd class="num">{{ activeFingerprint.pairRatioPct }}% / {{ activeFingerprint.holdRatioPct }}%</dd></div>
          </dl>
        </div>
        <div class="rounded-lg border border-line bg-panel-2/30 p-3 text-[12px]">
          <div class="label-micro">逆向 Blueprint 产物</div>
          <pre v-if="activeFingerprint.blueprintJson" class="mt-2 max-h-40 overflow-auto rounded bg-panel p-2 text-[11px]">{{ activeFingerprint.blueprintJson }}</pre>
          <p v-else class="mt-2 text-[11.5px] text-muted-fg">{{ activeFingerprint.reason }}</p>
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
