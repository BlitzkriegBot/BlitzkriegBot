<script setup lang="ts">
/**
 * 交易时段风控卡 — 账户级 + 策略级交易时间窗口配置。
 *
 * 设计契约：
 *  - 两层作用域（账户级全局安全网 + 策略级独立覆写），同时开启时取交集（Fail-Closed）；
 *  - 仅拦截新开仓，绝不拦截平仓、止损、thesis_death_exit 与到期结算；
 *  - 保存后经 `risk.tradingWindows.set` 写入 SQLite 并热推送到内核，无需重启。
 */
import { computed, onMounted, ref, watch } from 'vue'
import { Clock, Plus, Save, Trash2 } from 'lucide-vue-next'
import {
  api,
  type AccountTradingWindows,
  type TradingTimeInterval,
  type TradingWindowMode,
  type TradingWindowPolicy,
  type TradingWindowStatusDoc,
} from '@/api/client'
import { usePanelStore } from '@/stores/panel'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import Switch from '@/components/ui/switch/Switch.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'

const store = usePanelStore()

const loading = ref(true)
const saving = ref(false)
const error = ref<string | null>(null)
const flash = ref<string | null>(null)
const statusDoc = ref<TradingWindowStatusDoc | null>(null)

const acctPolicy = ref<TradingWindowPolicy>(defaultPolicy())
const strategyPolicies = ref<Array<{ name: string; policy: TradingWindowPolicy }>>([])
const newStrategyName = ref('')

const TZ_OPTIONS = [
  { offset: 480, label: '北京时间 (UTC+8)', tz: 'Asia/Shanghai' },
  { offset: -300, label: '美东冬令时 (UTC-5)', tz: 'America/New_York' },
  { offset: -240, label: '美东夏令时 (UTC-4)', tz: 'America/New_York' },
  { offset: 0, label: 'UTC 标准时间 (UTC+0)', tz: 'UTC' },
]

const WEEKDAY_LABELS = [
  { id: 1, label: '一' },
  { id: 2, label: '二' },
  { id: 3, label: '三' },
  { id: 4, label: '四' },
  { id: 5, label: '五' },
  { id: 6, label: '六' },
  { id: 7, label: '日' },
]

interface WindowPreset {
  id: string
  label: string
  mode: TradingWindowMode
  tzOffsetMin: number
  timezone: string
  intervals: TradingTimeInterval[]
}

const PRESETS: WindowPreset[] = [
  {
    id: 'deny-us-open',
    label: '避开美盘插针 (20:00-23:30)',
    mode: 'deny',
    tzOffsetMin: 480,
    timezone: 'Asia/Shanghai',
    intervals: [{ startMin: 20 * 60, endMin: 23 * 60 + 30, weekdays: [1, 2, 3, 4, 5] }],
  },
  {
    id: 'allow-asia',
    label: '仅亚盘温和期 (08:00-16:00)',
    mode: 'allow',
    tzOffsetMin: 480,
    timezone: 'Asia/Shanghai',
    intervals: [{ startMin: 8 * 60, endMin: 16 * 60, weekdays: [] }],
  },
  {
    id: 'deny-night',
    label: '夜间休眠 (23:00-07:00)',
    mode: 'deny',
    tzOffsetMin: 480,
    timezone: 'Asia/Shanghai',
    intervals: [{ startMin: 23 * 60, endMin: 7 * 60, weekdays: [] }],
  },
]

function defaultPolicy(): TradingWindowPolicy {
  return {
    enabled: false,
    mode: 'deny',
    tzOffsetMin: 480,
    timezone: 'Asia/Shanghai',
    intervals: [{ startMin: 20 * 60, endMin: 23 * 60 + 30, weekdays: [1, 2, 3, 4, 5] }],
    label: null,
  }
}

function minToTime(min: number): string {
  const clamped = Math.max(0, Math.min(1439, Math.floor(min)))
  const h = Math.floor(clamped / 60)
  const m = clamped % 60
  return `${String(h).padStart(2, '0')}:${String(m).padStart(2, '0')}`
}

function timeToMin(hhmm: string, fallback: number): number {
  const parts = hhmm.split(':')
  if (parts.length !== 2) return fallback
  const h = Number(parts[0])
  const m = Number(parts[1])
  if (!Number.isFinite(h) || !Number.isFinite(m) || h < 0 || h > 23 || m < 0 || m > 59) {
    return fallback
  }
  return h * 60 + m
}

function applyPreset(target: TradingWindowPolicy, preset: WindowPreset): void {
  target.enabled = true
  target.mode = preset.mode
  target.tzOffsetMin = preset.tzOffsetMin
  target.timezone = preset.timezone
  target.label = preset.label
  target.intervals = preset.intervals.map((iv) => ({
    startMin: iv.startMin,
    endMin: iv.endMin,
    weekdays: [...iv.weekdays],
  }))
}

function toggleWeekday(iv: TradingTimeInterval, day: number): void {
  const idx = iv.weekdays.indexOf(day)
  if (idx >= 0) {
    iv.weekdays.splice(idx, 1)
  } else {
    iv.weekdays.push(day)
    iv.weekdays.sort((a, b) => a - b)
  }
}

function addInterval(pol: TradingWindowPolicy): void {
  pol.intervals.push({ startMin: 20 * 60, endMin: 23 * 60 + 30, weekdays: [] })
}

function removeInterval(pol: TradingWindowPolicy, idx: number): void {
  pol.intervals.splice(idx, 1)
}

const currentAccount = computed(() => store.activeAccount || 'default')

const availableStrategies = computed(() => {
  const existing = new Set(strategyPolicies.value.map((s) => s.name))
  const raw = (store.snapshot?.strategies ?? []) as Array<string | { name?: string }>
  const list = raw.map((s) => (typeof s === 'string' ? s : s.name ?? '')).filter(Boolean)
  return list.filter((n) => !existing.has(n))
})

function addStrategyPolicy(): void {
  const name = newStrategyName.value.trim()
  if (!name) return
  if (strategyPolicies.value.some((s) => s.name === name)) return
  const pol = defaultPolicy()
  pol.enabled = true
  strategyPolicies.value.push({ name, policy: pol })
  newStrategyName.value = ''
}

function removeStrategyPolicy(idx: number): void {
  strategyPolicies.value.splice(idx, 1)
}

function hydrate(doc: TradingWindowStatusDoc): void {
  statusDoc.value = doc
  acctPolicy.value = doc.windows.account
    ? {
        ...defaultPolicy(),
        ...doc.windows.account,
        intervals: (doc.windows.account.intervals ?? []).map((iv) => ({
          startMin: iv.startMin,
          endMin: iv.endMin,
          weekdays: [...(iv.weekdays ?? [])],
        })),
      }
    : defaultPolicy()
  strategyPolicies.value = Object.entries(doc.windows.strategies ?? {}).map(([name, pol]) => ({
    name,
    policy: {
      ...defaultPolicy(),
      ...pol,
      intervals: (pol.intervals ?? []).map((iv) => ({
        startMin: iv.startMin,
        endMin: iv.endMin,
        weekdays: [...(iv.weekdays ?? [])],
      })),
    },
  }))
}

async function loadWindows(): Promise<void> {
  loading.value = true
  error.value = null
  try {
    const doc = await api.riskTradingWindows(currentAccount.value)
    if (doc.error) {
      error.value = doc.error
    } else {
      hydrate(doc)
    }
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

async function saveWindows(): Promise<void> {
  saving.value = true
  error.value = null
  flash.value = null
  try {
    const strategies: Record<string, TradingWindowPolicy> = {}
    for (const row of strategyPolicies.value) {
      strategies[row.name] = row.policy
    }
    const windows: AccountTradingWindows = {
      account: acctPolicy.value.enabled || acctPolicy.value.intervals.length > 0 ? acctPolicy.value : null,
      strategies,
    }
    const doc = await api.setRiskTradingWindows({ account: currentAccount.value, windows })
    if (doc.error) {
      error.value = doc.error
    } else {
      hydrate(doc)
      flash.value = `已保存并热生效（账户 ${doc.accountId}；仅拦截新开仓，平仓与止损始终放行）`
    }
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    saving.value = false
  }
}

watch(currentAccount, () => {
  void loadWindows()
})

onMounted(() => {
  void loadWindows()
})
</script>

<template>
  <Card dense class="mt-3.5">
    <CardHeader label="交易时段风控（账户级 + 策略级）">
      <template #action>
        <div class="flex items-center gap-2">
          <Badge
            v-if="statusDoc"
            :variant="statusDoc.activeNow.accountAllowed ? 'up' : 'gold'"
            dot
          >
            {{ statusDoc.activeNow.accountAllowed ? '当前允许开仓' : '当前禁开仓时段 · 平仓照放' }}
          </Badge>
          <Button variant="gold" size="sm" :disabled="saving || loading" @click="saveWindows">
            <Save class="size-3.5" />{{ saving ? '保存中…' : '保存时段规则' }}
          </Button>
        </div>
      </template>
    </CardHeader>

    <p class="mb-3 text-[12px] text-muted-fg">
      按传统金融盘面时段（亚盘/欧盘/美盘）限制新开仓，规避高波双向插针期。账户级与策略级同时开启时取交集；<strong class="font-semibold text-fg">绝不拦截平仓、止损与到期结算</strong>。
    </p>

    <AlertBanner v-if="error" tone="error" title="时段风控操作失败" class="mb-3">
      {{ error }}
    </AlertBanner>
    <AlertBanner v-if="flash" tone="info" title="时段风控已生效" dismissible class="mb-3" @dismiss="flash = null">
      {{ flash }}
    </AlertBanner>

    <!-- 账户级时段风控 -->
    <div class="rounded-lg border border-line bg-panel-2 p-3">
      <div class="flex flex-wrap items-center justify-between gap-2">
        <div class="flex items-center gap-2">
          <Clock class="size-4 text-primary" />
          <span class="text-[13px] font-semibold">账户级全局时段（{{ currentAccount }}）</span>
        </div>
        <div class="flex items-center gap-2">
          <span class="text-[11.5px] text-muted-fg">{{ acctPolicy.enabled ? '已启用' : '未启用（全天候）' }}</span>
          <Switch v-model="acctPolicy.enabled" />
        </div>
      </div>

      <div v-if="acctPolicy.enabled" class="mt-3 space-y-2.5 border-t border-line pt-2.5">
        <div class="flex flex-wrap items-center gap-2 text-[12px]">
          <select
            v-model="acctPolicy.mode"
            class="h-7 rounded-md border border-line bg-panel px-2 text-[12px]"
          >
            <option value="deny">黑名单（禁止以下时段开仓）</option>
            <option value="allow">白名单（仅允许以下时段开仓）</option>
          </select>
          <select
            v-model.number="acctPolicy.tzOffsetMin"
            class="h-7 rounded-md border border-line bg-panel px-2 text-[12px]"
            @change="acctPolicy.timezone = TZ_OPTIONS.find((t) => t.offset === acctPolicy.tzOffsetMin)?.tz ?? 'UTC'"
          >
            <option v-for="t in TZ_OPTIONS" :key="t.offset" :value="t.offset">{{ t.label }}</option>
          </select>
          <div class="flex flex-wrap items-center gap-1.5">
            <Button
              v-for="p in PRESETS"
              :key="p.id"
              variant="outline"
              size="sm"
              @click="applyPreset(acctPolicy, p)"
            >
              {{ p.label }}
            </Button>
          </div>
        </div>

        <div
          v-for="(iv, idx) in acctPolicy.intervals"
          :key="idx"
          class="flex flex-wrap items-center gap-2 rounded-md border border-line bg-panel px-2.5 py-1.5 text-[12px]"
        >
          <input
            type="time"
            class="figures h-7 rounded border border-line bg-panel-2 px-1.5 text-[12px]"
            :value="minToTime(iv.startMin)"
            @change="iv.startMin = timeToMin(($event.target as HTMLInputElement).value, iv.startMin)"
          />
          <span class="text-faint-fg">至</span>
          <input
            type="time"
            class="figures h-7 rounded border border-line bg-panel-2 px-1.5 text-[12px]"
            :value="minToTime(iv.endMin)"
            @change="iv.endMin = timeToMin(($event.target as HTMLInputElement).value, iv.endMin)"
          />
          <span v-if="iv.startMin > iv.endMin" class="text-[11px] text-primary">跨午夜</span>
          <div class="flex items-center gap-1">
            <button
              v-for="wd in WEEKDAY_LABELS"
              :key="wd.id"
              type="button"
              class="h-6 w-6 rounded text-[11px] font-medium transition-colors"
              :class="
                iv.weekdays.length === 0 || iv.weekdays.includes(wd.id)
                  ? 'bg-primary/15 text-primary border border-primary/40'
                  : 'bg-panel-2 text-faint-fg border border-line'
              "
              @click="toggleWeekday(iv, wd.id)"
            >
              {{ wd.label }}
            </button>
          </div>
          <span class="text-[11px] text-faint-fg">
            {{ iv.weekdays.length === 0 ? '每天' : '仅选中星期' }}
          </span>
          <Button variant="ghost" size="icon-sm" class="ml-auto" @click="removeInterval(acctPolicy, idx)">
            <Trash2 class="size-3.5 text-down" />
          </Button>
        </div>

        <Button variant="outline" size="sm" @click="addInterval(acctPolicy)">
          <Plus class="size-3.5" />添加时间段
        </Button>
      </div>
    </div>

    <!-- 策略级独立时段风控 -->
    <div class="mt-3 rounded-lg border border-line bg-panel-2 p-3">
      <div class="flex flex-wrap items-center justify-between gap-2">
        <span class="text-[13px] font-semibold">策略级独立时段（覆盖单个策略）</span>
        <div class="flex items-center gap-2">
          <select
            v-model="newStrategyName"
            class="h-7 rounded-md border border-line bg-panel px-2 text-[12px]"
          >
            <option value="">选择要限时的策略…</option>
            <option v-for="name in availableStrategies" :key="name" :value="name">{{ name }}</option>
          </select>
          <Button variant="outline" size="sm" :disabled="!newStrategyName" @click="addStrategyPolicy">
            <Plus class="size-3.5" />添加策略时段
          </Button>
        </div>
      </div>

      <div v-if="strategyPolicies.length === 0" class="mt-2 text-[11.5px] text-faint-fg">
        暂无策略级独立规则；未单独配置的策略将直接沿用上方账户级时段设置。
      </div>

      <div v-else class="mt-2.5 space-y-2.5">
        <div
          v-for="(item, sIdx) in strategyPolicies"
          :key="item.name"
          class="rounded-md border border-line bg-panel p-2.5"
        >
          <div class="flex flex-wrap items-center justify-between gap-2">
            <div class="flex items-center gap-2">
              <span class="font-mono text-[12.5px] font-semibold">{{ item.name }}</span>
              <Badge
                v-if="statusDoc?.activeNow.strategies[item.name]"
                :variant="statusDoc.activeNow.strategies[item.name].allowed ? 'up' : 'gold'"
              >
                {{ statusDoc.activeNow.strategies[item.name].allowed ? '当前可开仓' : '当前禁开仓' }}
              </Badge>
            </div>
            <div class="flex items-center gap-2">
              <Switch v-model="item.policy.enabled" />
              <Button variant="ghost" size="icon-sm" @click="removeStrategyPolicy(sIdx)">
                <Trash2 class="size-3.5 text-down" />
              </Button>
            </div>
          </div>

          <div v-if="item.policy.enabled" class="mt-2 space-y-2 border-t border-line pt-2">
            <div class="flex flex-wrap items-center gap-2 text-[12px]">
              <select
                v-model="item.policy.mode"
                class="h-7 rounded-md border border-line bg-panel-2 px-2 text-[12px]"
              >
                <option value="deny">黑名单（禁止以下时段）</option>
                <option value="allow">白名单（仅允许以下时段）</option>
              </select>
              <select
                v-model.number="item.policy.tzOffsetMin"
                class="h-7 rounded-md border border-line bg-panel-2 px-2 text-[12px]"
                @change="item.policy.timezone = TZ_OPTIONS.find((t) => t.offset === item.policy.tzOffsetMin)?.tz ?? 'UTC'"
              >
                <option v-for="t in TZ_OPTIONS" :key="t.offset" :value="t.offset">{{ t.label }}</option>
              </select>
              <Button
                v-for="p in PRESETS"
                :key="p.id"
                variant="outline"
                size="sm"
                @click="applyPreset(item.policy, p)"
              >
                {{ p.label }}
              </Button>
            </div>

            <div
              v-for="(iv, idx) in item.policy.intervals"
              :key="idx"
              class="flex flex-wrap items-center gap-2 rounded border border-line bg-panel-2 px-2 py-1 text-[12px]"
            >
              <input
                type="time"
                class="figures h-7 rounded border border-line bg-panel px-1.5 text-[12px]"
                :value="minToTime(iv.startMin)"
                @change="iv.startMin = timeToMin(($event.target as HTMLInputElement).value, iv.startMin)"
              />
              <span class="text-faint-fg">至</span>
              <input
                type="time"
                class="figures h-7 rounded border border-line bg-panel px-1.5 text-[12px]"
                :value="minToTime(iv.endMin)"
                @change="iv.endMin = timeToMin(($event.target as HTMLInputElement).value, iv.endMin)"
              />
              <div class="flex items-center gap-1">
                <button
                  v-for="wd in WEEKDAY_LABELS"
                  :key="wd.id"
                  type="button"
                  class="h-6 w-6 rounded text-[11px] font-medium"
                  :class="
                    iv.weekdays.length === 0 || iv.weekdays.includes(wd.id)
                      ? 'bg-primary/15 text-primary border border-primary/40'
                      : 'bg-panel text-faint-fg border border-line'
                  "
                  @click="toggleWeekday(iv, wd.id)"
                >
                  {{ wd.label }}
                </button>
              </div>
              <Button variant="ghost" size="icon-sm" class="ml-auto" @click="removeInterval(item.policy, idx)">
                <Trash2 class="size-3.5 text-down" />
              </Button>
            </div>

            <Button variant="outline" size="sm" @click="addInterval(item.policy)">
              <Plus class="size-3.5" />添加时间段
            </Button>
          </div>
        </div>
      </div>
    </div>
  </Card>
</template>
