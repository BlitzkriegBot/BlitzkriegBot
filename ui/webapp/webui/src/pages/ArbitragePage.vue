<script setup lang="ts">
/**
 * 跨所套利工作台 — 独立于单账户策略页的双腿套利实时监控与控制面。
 *
 * 设计目标：
 *  1. 让操作员一眼看清套利此刻「是否在扫盘 / 是否就绪 / 卡在哪个前置条件」；
 *  2. 显式展示并锁定 A/B 交易所共同支持的时间轮次周期（交集）；
 *  3. 实时呈现各监控币对的双腿盘口卖一价、组合成本与套利空间（1.00 - 组合成本）；
 *  4. 跨 A/B 两账户聚合活跃配对（PairBook）、套利订单与历史成交，彻底消除
 *     「单账户视角下套利利润恒为 $0、看不到历史」的割裂感。
 */
import { computed, onMounted, ref, watch } from 'vue'
import {
  ArrowLeftRight,
  CheckCircle2,
  Clock,
  Link2,
  RefreshCw,
  Trash2,
  XCircle,
} from 'lucide-vue-next'
import { api, type ArbStatusDoc } from '@/api/client'
import { usePanelStore } from '@/stores/panel'
import { clockTime, num, signedMoney } from '@/lib/format'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import Switch from '@/components/ui/switch/Switch.vue'
import StatTile from '@/components/ui/stat/StatTile.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'
import SegmentedControl from '@/components/ui/segmented/SegmentedControl.vue'
import ConfirmDialog from '@/components/ui/confirm/ConfirmDialog.vue'
import CreateArbDialog from '@/components/CreateArbDialog.vue'

const ARB_STRATEGY = 'cross_venue_arb'
const store = usePanelStore()

const status = ref<ArbStatusDoc | null>(null)
const loading = ref(true)
const error = ref<string | null>(null)
const flash = ref<{ tone: 'info' | 'error'; msg: string } | null>(null)
const toggling = ref(false)
const clearing = ref(false)
const dialogOpen = ref(false)
const clearConfirmOpen = ref(false)

type ActivityTab = 'pairs' | 'orders' | 'trades'
const activityTab = ref<ActivityTab>('pairs')

async function loadStatus(silent = false): Promise<void> {
  if (!silent) loading.value = true
  try {
    const doc = await api.arbStatus()
    if (doc.error) {
      error.value = doc.error
    } else {
      status.value = doc
      error.value = null
    }
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

// Follow the shell's live SSE / poll heartbeat instead of running a page-local timer.
watch(
  () => store.lastUpdated,
  () => {
    void loadStatus(true)
  },
)
onMounted(() => {
  void loadStatus()
})

function formatDuration(sec: number | null | undefined): string {
  if (!sec || sec <= 0) return '未锁定'
  if (sec === 300) return '5m'
  if (sec === 900) return '15m'
  if (sec === 3600) return '1h'
  if (sec === 14400) return '4h'
  if (sec === 86400) return 'daily'
  return sec % 60 === 0 ? `${sec / 60}m` : `${sec}s`
}

function formatCents(v: string | null | undefined): string {
  if (v == null || v === '') return '—'
  const n = Number(v)
  if (!Number.isFinite(n)) return '—'
  return `${(n * 100).toFixed(1)}¢`
}

function formatEdgeCents(v: string | null | undefined): string {
  if (v == null || v === '') return '—'
  const n = Number(v)
  if (!Number.isFinite(n)) return '—'
  const c = n * 100
  return `${c >= 0 ? '+' : ''}${c.toFixed(2)}¢`
}

function formatLeft(sec: number | null | undefined): string {
  if (sec == null || sec < 0) return '—'
  const m = Math.floor(sec / 60)
  const s = sec % 60
  return `${m}:${String(s).padStart(2, '0')}`
}

const configured = computed(() => Boolean(status.value?.configured && status.value?.config))
const cfg = computed(() => status.value?.config ?? null)
const readiness = computed(() => status.value?.readiness ?? [])
const failedChecks = computed(() => readiness.value.filter((c) => !c.ok))
const allReady = computed(() => configured.value && readiness.value.length > 0 && failedChecks.value.length === 0)

const stateBadge = computed<{ label: string; variant: 'up' | 'gold' | 'outline' | 'down' }>(() => {
  if (!configured.value) return { label: '未配置', variant: 'outline' }
  if (!status.value?.running) return { label: '已暂停', variant: 'outline' }
  if (!allReady.value) return { label: '前置未就绪', variant: 'gold' }
  return { label: '扫盘运行中', variant: 'up' }
})

const roundBadgeLabel = computed(() => {
  const c = cfg.value
  if (!c) return '未锁定轮次'
  const label = formatDuration(c.roundDurationSec)
  if (c.lockedByVenue) return `${label} 共同轮次 · 跟随 ${c.lockedByVenue}`
  return `${label} 共同轮次`
})

const bestEdgeRow = computed(() => {
  const rows = status.value?.scanner ?? []
  let best: { asset: string; edge: number; raw: string } | null = null
  for (const r of rows) {
    if (r.edgeUsd == null) continue
    const n = Number(r.edgeUsd)
    if (!Number.isFinite(n)) continue
    if (!best || n > best.edge) best = { asset: r.asset, edge: n, raw: r.edgeUsd }
  }
  return best
})

const netPnlNum = computed(() => Number(status.value?.summary?.netPnlUsd ?? 0))

async function toggleArb(next: boolean): Promise<void> {
  if (!cfg.value || toggling.value) return
  toggling.value = true
  flash.value = null
  try {
    const res = await api.setStrategy(ARB_STRATEGY, next, cfg.value.legAAccount)
    if (res.error) throw new Error(String(res.error))
    await Promise.all([loadStatus(true), store.refresh()])
  } catch (e) {
    flash.value = { tone: 'error', msg: `切换套利开关失败：${e instanceof Error ? e.message : String(e)}` }
  } finally {
    toggling.value = false
  }
}

async function clearArb(): Promise<void> {
  if (clearing.value) return
  clearing.value = true
  flash.value = null
  try {
    const legA = cfg.value?.legAAccount
    if (legA && status.value?.running) {
      await api.setStrategy(ARB_STRATEGY, false, legA).catch(() => undefined)
    }
    const res = await api.arbConfigClear('panel')
    if (res.error) throw new Error(res.error)
    clearConfirmOpen.value = false
    flash.value = { tone: 'info', msg: '已解除跨所套利双腿配置并停止扫盘。' }
    await Promise.all([loadStatus(true), store.refresh()])
  } catch (e) {
    flash.value = { tone: 'error', msg: `解除套利失败：${e instanceof Error ? e.message : String(e)}` }
  } finally {
    clearing.value = false
  }
}

async function onArbCommitted(): Promise<void> {
  flash.value = { tone: 'info', msg: '套利双腿配置与共同轮次周期已更新并生效。' }
  await Promise.all([loadStatus(true), store.refresh()])
}
</script>

<template>
  <div class="rise-in space-y-3.5">
    <AlertBanner
      v-if="error"
      tone="error"
      title="套利状态读取失败"
    >
      {{ error }}
    </AlertBanner>

    <AlertBanner
      v-if="flash"
      :tone="flash.tone"
      title="套利操作回执"
      dismissible
      @dismiss="flash = null"
    >
      {{ flash.msg }}
    </AlertBanner>

    <!-- 顶部状态与控制条 -->
    <Card dense>
      <CardHeader label="跨所套利工作台">
        <template #action>
          <div class="flex flex-wrap items-center gap-2">
            <Badge :variant="stateBadge.variant" dot>
              {{ stateBadge.label }}
            </Badge>
            <Badge v-if="configured" variant="gold">
              <Clock class="mr-1 size-3" />{{ roundBadgeLabel }}
            </Badge>
            <div v-if="configured" class="flex items-center gap-2 rounded-md border border-line bg-panel-2 px-2.5 py-1">
              <span class="text-[12px] text-muted-fg">套利总开关</span>
              <Switch
                :model-value="Boolean(status?.running)"
                :disabled="toggling"
                @update:model-value="toggleArb"
              />
            </div>
            <Button variant="gold" size="sm" @click="dialogOpen = true">
              <Link2 class="size-3.5" />{{ configured ? '修改双腿配置' : '配置跨所套利' }}
            </Button>
            <Button
              v-if="configured"
              variant="outline"
              size="sm"
              :disabled="clearing"
              @click="clearConfirmOpen = true"
            >
              <Trash2 class="size-3.5" />解除套利
            </Button>
            <Button variant="ghost" size="icon-sm" title="刷新实时状态" @click="loadStatus()">
              <RefreshCw class="size-3.5" :class="loading && 'animate-spin'" />
            </Button>
          </div>
        </template>
      </CardHeader>

      <div v-if="cfg" class="flex flex-wrap items-center gap-3 rounded-lg border border-line bg-panel-2 px-3 py-2.5 text-[13px]">
        <div class="flex items-center gap-2">
          <span class="label-micro">A 腿</span>
          <span class="font-semibold">{{ cfg.legAAccount }}</span>
          <Badge variant="outline">{{ cfg.legAVenue }}</Badge>
        </div>
        <ArrowLeftRight class="size-4 text-faint-fg" />
        <div class="flex items-center gap-2">
          <span class="label-micro">B 腿</span>
          <span class="font-semibold">{{ cfg.legBAccount }}</span>
          <Badge variant="outline">{{ cfg.legBVenue }}</Badge>
        </div>
        <div class="ml-auto flex flex-wrap items-center gap-1.5">
          <span class="label-micro">监控币对</span>
          <Badge v-for="pair in cfg.pairs" :key="pair" variant="outline">{{ pair }}</Badge>
        </div>
      </div>

      <EmptyState
        v-else
        :loading="loading"
        text="尚未配置跨所套利"
        hint="跨所套利同时管理 A/B 两个交易所账户并在双方共同支持的时间轮次上扫盘；点击右上角「配置跨所套利」完成双腿绑定。"
      />
    </Card>

    <template v-if="configured && status">
      <!-- 4 张核心合并 KPI -->
      <div class="grid gap-3.5 sm:grid-cols-2 xl:grid-cols-4">
        <StatTile
          label="双腿合并净盈亏"
          :value="signedMoney(status.summary.netPnlUsd)"
          :tone="netPnlNum >= 0 ? 'up' : 'down'"
        >
          <template #sub>
            胜 <span class="text-up font-semibold"><RollingNumber :value="status.summary.wins" /></span>
            · 负 <span class="text-down font-semibold"><RollingNumber :value="status.summary.losses" /></span>
            · 共 <RollingNumber :value="status.summary.closedTrades" /> 笔
          </template>
        </StatTile>
        <StatTile label="活跃配对（PairBook）" :value="num(status.summary.activePairs)" tone="gold">
          <template #sub>当前未平仓/对冲中的双腿配对</template>
        </StatTile>
        <StatTile label="套利订单（双腿合计）" :value="num(status.summary.ordersCount)">
          <template #sub>跨 A/B 两账户累计套利委托</template>
        </StatTile>
        <StatTile
          label="当前最优套利空间"
          :value="bestEdgeRow ? formatEdgeCents(bestEdgeRow.raw) : '—'"
          :tone="bestEdgeRow && bestEdgeRow.edge > 0 ? 'up' : 'default'"
        >
          <template #sub>
            {{ bestEdgeRow ? `${bestEdgeRow.asset} · 1.00 - 最优双腿组合成本` : '等待双腿盘口报价' }}
          </template>
        </StatTile>
      </div>

      <!-- 前置就绪体检 -->
      <Card dense>
        <CardHeader label="运行就绪体检">
          <template #action>
            <Badge :variant="allReady ? 'up' : 'gold'" dot>
              {{ allReady ? '全部就绪 · 正在扫盘' : `${failedChecks.length} 项待处理` }}
            </Badge>
          </template>
        </CardHeader>

        <AlertBanner
          v-if="failedChecks.length > 0"
          tone="warn"
          class="mb-3"
          title="套利暂未触发下单的原因"
        >
          {{ failedChecks.map((c) => `${c.label}：${c.detail}`).join('；') }}
        </AlertBanner>

        <div class="grid gap-2.5 sm:grid-cols-2 lg:grid-cols-3">
          <div
            v-for="chk in readiness"
            :key="chk.id"
            class="flex items-start gap-2.5 rounded-lg border border-line bg-panel-2 px-3 py-2.5"
          >
            <CheckCircle2 v-if="chk.ok" class="mt-0.5 size-4 shrink-0 text-up" />
            <XCircle v-else class="mt-0.5 size-4 shrink-0 text-down" />
            <div class="min-w-0">
              <div class="text-[12.5px] font-semibold">{{ chk.label }}</div>
              <div class="mt-0.5 text-[11.5px] text-muted-fg">{{ chk.detail }}</div>
            </div>
          </div>
        </div>
      </Card>

      <!-- 实时跨所价差扫描板 -->
      <Card dense>
        <CardHeader label="实时跨所价差扫描（每 3 秒刷新）">
          <template #action>
            <span class="text-[11.5px] text-faint-fg">
              组合成本 = A 腿卖一 + B 腿反向卖一；低于 100¢ 即出现正向套利空间
            </span>
          </template>
        </CardHeader>

        <div v-if="status.scanner.length" class="-mx-2 overflow-x-auto">
          <table class="w-full min-w-[780px] text-[13px]">
            <thead>
              <tr class="text-left">
                <th class="label-micro px-2 pb-2">标的</th>
                <th class="label-micro px-2 pb-2 text-right">剩余</th>
                <th class="label-micro px-2 pb-2 text-right">{{ cfg?.legAVenue }} 卖一 (UP / DN)</th>
                <th class="label-micro px-2 pb-2 text-right">{{ cfg?.legBVenue }} 卖一 (UP / DN)</th>
                <th class="label-micro px-2 pb-2 text-right">A_UP + B_DN</th>
                <th class="label-micro px-2 pb-2 text-right">A_DN + B_UP</th>
                <th class="label-micro px-2 pb-2 text-right">实时空间</th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="row in status.scanner"
                :key="row.asset"
                class="border-t border-line hover:bg-panel-2"
              >
                <td class="px-2 py-2.5 font-semibold">
                  {{ row.asset }}
                  <span class="ml-1 text-[11px] text-faint-fg">{{ formatDuration(row.roundDurationSec) }}</span>
                </td>
                <td class="px-2 py-2.5 text-right num text-muted-fg">{{ formatLeft(row.timeLeftSec) }}</td>
                <td class="px-2 py-2.5 text-right num">
                  <span v-if="!row.legA.marketFound" class="text-faint-fg">无此周期市场</span>
                  <span v-else-if="row.legA.stale" class="text-faint-fg">盘口等待中</span>
                  <span v-else>{{ formatCents(row.legA.upAsk) }} / {{ formatCents(row.legA.downAsk) }}</span>
                </td>
                <td class="px-2 py-2.5 text-right num">
                  <span v-if="!row.legB.marketFound" class="text-faint-fg">无此周期市场</span>
                  <span v-else-if="row.legB.stale" class="text-faint-fg">盘口等待中</span>
                  <span v-else>{{ formatCents(row.legB.upAsk) }} / {{ formatCents(row.legB.downAsk) }}</span>
                </td>
                <td class="px-2 py-2.5 text-right num">{{ formatCents(row.comboUpDownCost) }}</td>
                <td class="px-2 py-2.5 text-right num">{{ formatCents(row.comboDownUpCost) }}</td>
                <td class="px-2 py-2.5 text-right num font-semibold">
                  <Badge v-if="row.opportunity" variant="up">{{ formatEdgeCents(row.edgeUsd) }}</Badge>
                  <span v-else class="text-muted-fg">{{ formatEdgeCents(row.edgeUsd) }}</span>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <EmptyState v-else text="暂无监控币对" hint="请在配置中至少选择一个监控币对。" />
      </Card>

      <!-- 活跃配对 / 套利订单 / 历史成交 -->
      <Card dense>
        <CardHeader label="套利持仓与历史记录">
          <template #action>
            <SegmentedControl
              v-model="activityTab"
              size="sm"
              :segments="[
                { id: 'pairs', label: '活跃配对', badge: status.activePairs.length },
                { id: 'orders', label: '套利订单', badge: status.orders.length },
                { id: 'trades', label: '历史成交', badge: status.trades.length },
              ]"
            />
          </template>
        </CardHeader>

        <!-- 活跃配对 -->
        <template v-if="activityTab === 'pairs'">
          <div v-if="status.activePairs.length" class="-mx-2 overflow-x-auto">
            <table class="w-full min-w-[720px] text-[13px]">
              <thead>
                <tr class="text-left">
                  <th class="label-micro px-2 pb-2">配对状态</th>
                  <th class="label-micro px-2 pb-2">轮次</th>
                  <th class="label-micro px-2 pb-2">双腿明细（账户 · 成交 / 声明份额）</th>
                  <th class="label-micro px-2 pb-2 text-right">创建时间</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="p in status.activePairs" :key="p.id" class="border-t border-line">
                  <td class="px-2 py-2.5">
                    <Badge :variant="p.status === 'complete' ? 'up' : p.status === 'orphan' ? 'down' : 'gold'">
                      {{ p.status }}
                    </Badge>
                  </td>
                  <td class="px-2 py-2.5 num text-muted-fg">#{{ p.slot }}</td>
                  <td class="px-2 py-2.5">
                    <div class="flex flex-wrap gap-2">
                      <Badge v-for="leg in p.legs" :key="leg.internalKey" variant="outline" class="num">
                        {{ leg.accountId }}: {{ leg.filledShares }}/{{ leg.declaredShares }}
                      </Badge>
                    </div>
                  </td>
                  <td class="px-2 py-2.5 text-right num text-faint-fg">{{ clockTime(p.createdMs) }}</td>
                </tr>
              </tbody>
            </table>
          </div>
          <EmptyState v-else text="当前无活跃配对" hint="当双腿同时出现正向套利空间并触发下单后，配对会实时显示在这里。" />
        </template>

        <!-- 套利订单 -->
        <template v-else-if="activityTab === 'orders'">
          <div v-if="status.orders.length" class="-mx-2 overflow-x-auto">
            <table class="w-full min-w-[760px] text-[13px]">
              <thead>
                <tr class="text-left">
                  <th class="label-micro px-2 pb-2">账户</th>
                  <th class="label-micro px-2 pb-2">标的</th>
                  <th class="label-micro px-2 pb-2">方向</th>
                  <th class="label-micro px-2 pb-2 text-right">价格</th>
                  <th class="label-micro px-2 pb-2 text-right">成交 / 委托</th>
                  <th class="label-micro px-2 pb-2">状态</th>
                  <th class="label-micro px-2 pb-2 text-right">时间</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="o in status.orders" :key="o.orderId" class="border-t border-line">
                  <td class="px-2 py-2 font-semibold">{{ o.accountId }}</td>
                  <td class="px-2 py-2">{{ o.asset }}</td>
                  <td class="px-2 py-2 uppercase text-muted-fg">{{ o.side }} {{ o.direction }}</td>
                  <td class="px-2 py-2 text-right num">{{ formatCents(o.price) }}</td>
                  <td class="px-2 py-2 text-right num">{{ o.filledSize }} / {{ o.size }}</td>
                  <td class="px-2 py-2"><Badge variant="outline">{{ o.status }}</Badge></td>
                  <td class="px-2 py-2 text-right num text-faint-fg">{{ clockTime(o.createdAtMs) }}</td>
                </tr>
              </tbody>
            </table>
          </div>
          <EmptyState v-else text="暂无套利订单" hint="套利策略在 A/B 两个账户发出的所有订单都会聚合展示在此。" />
        </template>

        <!-- 历史成交 -->
        <template v-else>
          <div v-if="status.trades.length" class="-mx-2 overflow-x-auto">
            <table class="w-full min-w-[820px] text-[13px]">
              <thead>
                <tr class="text-left">
                  <th class="label-micro px-2 pb-2">账户 / 场地</th>
                  <th class="label-micro px-2 pb-2">标的</th>
                  <th class="label-micro px-2 pb-2">方向</th>
                  <th class="label-micro px-2 pb-2 text-right">入场 / 离场</th>
                  <th class="label-micro px-2 pb-2 text-right">份额</th>
                  <th class="label-micro px-2 pb-2 text-right">净盈亏</th>
                  <th class="label-micro px-2 pb-2 text-right">平仓时间</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="t in status.trades" :key="`${t.id}-${t.exitedAtMs}`" class="border-t border-line">
                  <td class="px-2 py-2 font-semibold">
                    {{ t.accountId }}
                    <span class="ml-1 text-[11px] font-normal text-muted-fg">{{ t.venue }}</span>
                  </td>
                  <td class="px-2 py-2">{{ t.asset }}</td>
                  <td class="px-2 py-2 uppercase text-muted-fg">{{ t.direction }}</td>
                  <td class="px-2 py-2 text-right num">{{ formatCents(t.entryPrice) }} → {{ formatCents(t.exitPrice) }}</td>
                  <td class="px-2 py-2 text-right num">{{ t.shares }}</td>
                  <td
                    class="px-2 py-2 text-right num font-semibold"
                    :class="Number(t.netPnlUsd) >= 0 ? 'text-up' : 'text-down'"
                  >
                    {{ signedMoney(t.netPnlUsd) }}
                  </td>
                  <td class="px-2 py-2 text-right num text-faint-fg">{{ clockTime(t.exitedAtMs) }}</td>
                </tr>
              </tbody>
            </table>
          </div>
          <EmptyState v-else text="暂无套利历史成交" hint="套利配对结算或平仓后，A/B 两腿成交明细与合并净收益将记录在此。" />
        </template>
      </Card>
    </template>

    <CreateArbDialog
      v-model:open="dialogOpen"
      :active-account-id="store.activeAccount"
      :active-venue="store.activeAccountVenue"
      @committed="onArbCommitted"
    />

    <ConfirmDialog
      v-model:open="clearConfirmOpen"
      title="解除跨所套利配置"
      message="解除后将立即停止 cross_venue_arb 扫盘并清空双腿绑定配置（已有持仓与历史记录不受影响）。确认继续？"
      confirm-label="确认解除"
      tone="danger"
      :busy="clearing"
      @confirm="clearArb"
    />
  </div>
</template>
