<script setup lang="ts">
/**
 * Equity curve — cumulative net PnL across closed trades. Shared by the
 * overview and the HFT template page.
 */
import { computed, ref } from 'vue'
import type { EChartsOption } from 'echarts'
import { useChart, areaFade, palette, tooltipStyle, axisX, axisY } from '@/lib/chart'
import { signedMoney } from '@/lib/format'
import { isKnownPnl } from '@/lib/trades'
import type { TradeRow } from '@/api/client'

const props = withDefaults(
  defineProps<{ rows: TradeRow[]; height?: number; showAxis?: boolean; live?: boolean; total?: number }>(),
  { height: 132, showAxis: true, live: false },
)

const el = ref<HTMLDivElement | null>(null)

/**
 * Cumulative series (oldest → newest) with a 0 origin point.
 *
 * The rows arrive chronological — `trades.history` returns the append-only
 * trade log in file order, and `dedupeTrades` preserves that order (see
 * `lib/trades.ts`). So the series is accumulated as given; reversing here would
 * plot the run backwards.
 *
 * This is not cosmetic: on the live 275-trade log the mirrored curve deviated
 * from the real one by up to $28.17 mid-run, drawing a rising run as a falling
 * one while the endpoint (= the total) still matched the header above it.
 */
	const series = computed<number[]>(() => {
	  let cum = 0
	  let broke = false
	  // 未知盈亏不折叠为 0：首个未知行写 NaN 并置 broke，其后所有累计点均为 NaN，
	  // 因为再无基准可追溯，绝不续出不可核对的数（如 [+10, 未知, +5]
	  // 画成 [10, NaN, 15]）。broke 持续到系列结束——逐行账本基准不可得，
	  // 只能在末点用账本口径 total 覆写（见下文），且末点若已是 NaN 则
	  // 保持断线，绝不把总计盖在未知点上制造虚假涨跌。
	  const pts = props.rows.map((t) => {
	    if (!isKnownPnl(t)) broke = true
	    if (broke) return NaN
	    return (cum += Number(t.netPnlUsd))
	  })
	  // #282: 曲线全程保持成交折叠口径（逐笔净额累计），不再用账本口径 props.total
	  // 强行覆写最后一个点，避免末段出现与前面点不同源的虚假斜率跳变。
	  return [0, ...pts]
	})

	/** 读数口径：props.total（账本口径）优先；否则取曲线最后一个有限读数，缺失则 0。 */
	const total = computed(() => {
	  if (props.total != null) return props.total
	  for (let i = series.value.length - 1; i >= 0; i--) {
	    const v = series.value[i]
	    if (Number.isFinite(v)) return v
	  }
	  return 0
	})
const positive = computed(() => total.value >= 0)

const option = computed<EChartsOption>(() => {
  const p = palette()
  const color = positive.value ? p.up : p.down
  const pts = series.value
  return {
    animationDuration: 480,
    animationEasing: 'cubicOut',
    grid: {
      left: props.showAxis ? 46 : 2,
      right: 2,
      top: 8,
      bottom: props.showAxis ? 20 : 2,
      containLabel: false,
    },
    tooltip: {
      trigger: 'axis',
      ...tooltipStyle(),
      valueFormatter: (v) => signedMoney(Number(v)),
    },
    xAxis: props.showAxis
      ? { ...axisX(pts.map((_, i) => `#${i}`)), boundaryGap: false, axisLine: { show: false } }
      : { type: 'category', data: pts.map((_, i) => i), show: false, boundaryGap: false },
    yAxis: props.showAxis
      ? axisY()
      : { type: 'value', show: false, scale: true },
    series: [
      {
        type: 'line',
        data: pts,
        showSymbol: false,
        smooth: 0.35,
        lineStyle: { width: 2, color, shadowColor: color, shadowBlur: 12, shadowOffsetY: 3 },
        areaStyle: areaFade(color, positive.value ? '4d' : '40'),
        markLine: props.showAxis && pts.length > 1
          ? {
              silent: true,
              symbol: 'none',
              label: { show: false },
              lineStyle: { color: p.axis, type: 'dashed', width: 1 },
              data: [{ yAxis: 0 }],
            }
          : undefined,
      },
    ],
  }
})

useChart(el, () => option.value)
</script>

<template>
  <div class="relative w-full" :style="{ height: `${props.height}px` }">
    <div ref="el" class="absolute inset-0" />
    <div
      v-if="props.rows.length === 0"
      class="absolute inset-0 grid place-items-center text-[11.5px] text-faint-fg"
    >
      暂无平仓交易
    </div>
  </div>
</template>
