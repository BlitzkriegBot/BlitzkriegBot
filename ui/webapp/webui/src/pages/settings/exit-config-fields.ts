import type { RiskExitView } from '@/api/client'

export type ExitFieldKind = 'pos-decimal' | 'nonneg-decimal' | 'price' | 'nonneg-int' | 'pos-int' | 'signed-int' | 'bool'

export type ExitFieldGroup =
  | 'main'
  | 'time-mode'
  | 'dynamic-stop'
  | 'trailing'
  | 'ratchet-stale'
  | 'quote-maker'

export interface ExitFieldDef {
  key: keyof RiskExitView
  label: string
  hint?: string
  unit?: string
  kind: ExitFieldKind
  group: ExitFieldGroup
}

export interface ExitFieldGroupMeta {
  id: Exclude<ExitFieldGroup, 'main'>
  title: string
}

export const EXIT_ADVANCED_GROUPS: readonly ExitFieldGroupMeta[] = [
  { id: 'time-mode', title: '时间与基础模式' },
  { id: 'dynamic-stop', title: '动态收紧与紧止损' },
  { id: 'trailing', title: '移动止盈与比例回撤' },
  { id: 'ratchet-stale', title: '棘轮保底与滞涨/深度退出' },
  { id: 'quote-maker', title: '行情保护与挂单出场' },
]

/** ExitConfig 全部 35 项的单一真源定义（主视图 3 项 + 高级分组 32 项）。 */
export const EXIT_FIELD_DEFS: readonly ExitFieldDef[] = [
  // 主视图 3 项
  { key: 'stopLossPct', label: '止损', unit: '%', kind: 'pos-decimal', group: 'main' },
  { key: 'takeProfitPct', label: '止盈', unit: '%', kind: 'pos-decimal', group: 'main' },
  { key: 'ceilingTpPrice', label: '天花板止盈价', hint: '0 = 关闭，买一达此价即吃单落袋', unit: 'USD', kind: 'price', group: 'main' },

  // 1. 时间与基础模式
  { key: 'forceExitSec', label: '到期前强平', hint: '0 = 关闭', unit: '秒', kind: 'nonneg-int', group: 'time-mode' },
  { key: 'minTimeLeftSec', label: '最小剩余时间退出', hint: '0 = 仅到期结算', unit: '秒', kind: 'nonneg-int', group: 'time-mode' },
  { key: 'holderStopPct', label: '持有到期策略止损', hint: 'holds_to_settlement 保护宽止损', unit: '%', kind: 'pos-decimal', group: 'time-mode' },
  { key: 'simpleExitEnabled', label: '简单退出模式', hint: '关 = 启用完整退出梯（棘轮/滞涨/深度）', kind: 'bool', group: 'time-mode' },
  { key: 'exitGraceSec', label: '开仓宽限期', hint: '完整模式生效', unit: '秒', kind: 'nonneg-int', group: 'time-mode' },

  // 2. 动态收紧与紧止损
  { key: 'dynamicStopEnabled', label: '动态收紧止损', hint: '临近到期逐步收紧止损', kind: 'bool', group: 'dynamic-stop' },
  { key: 'stopTightenStartSec', label: '开始收紧时间', hint: '剩余秒数低于此值开始收紧', unit: '秒', kind: 'nonneg-int', group: 'dynamic-stop' },
  { key: 'stopMinPct', label: '动态止损下限', hint: '收紧后的最窄止损', unit: '%', kind: 'pos-decimal', group: 'dynamic-stop' },
  { key: 'tightStopEnabled', label: '紧止损开关', hint: '完整模式生效', kind: 'bool', group: 'dynamic-stop' },
  { key: 'tightStopPct', label: '紧止损阈值', hint: '完整模式基础止损', unit: '%', kind: 'pos-decimal', group: 'dynamic-stop' },

  // 3. 移动止盈与比例回撤
  { key: 'trailingEnabled', label: '移动止盈', hint: '浮盈回吐即锁利离场', kind: 'bool', group: 'trailing' },
  { key: 'trailingMinHighPct', label: '移动止盈启动浮盈', hint: '最高浮盈达标后武装', unit: '%', kind: 'nonneg-decimal', group: 'trailing' },
  { key: 'minTrailPct', label: '最小回撤宽度', hint: '移动止盈最小回撤下限', unit: '%', kind: 'nonneg-decimal', group: 'trailing' },
  { key: 'proportionalTrailEnabled', label: '比例回撤开关', hint: '按最高浮盈比例计算回撤', kind: 'bool', group: 'trailing' },
  { key: 'proportionalTrailPct', label: '比例回撤系数', hint: '允许回吐最高浮盈的比例', unit: '%', kind: 'nonneg-decimal', group: 'trailing' },
  { key: 'proportionalTrailMinPct', label: '比例回撤启动浮盈', hint: '低于此值走阶梯表', unit: '%', kind: 'nonneg-decimal', group: 'trailing' },
  { key: 'proportionalTrailMinGivebackPct', label: '比例回撤最小回吐', hint: '最小回撤保护下限', unit: '%', kind: 'nonneg-decimal', group: 'trailing' },

  // 4. 棘轮保底与滞涨/深度退出
  { key: 'ratchetEnabled', label: '棘轮保底止盈', hint: '完整模式生效，确认新高后抬高保底', kind: 'bool', group: 'ratchet-stale' },
  { key: 'ratchetConfirmTicks', label: '棘轮确认 tick 数', hint: '≥ 1', unit: 'tick', kind: 'pos-int', group: 'ratchet-stale' },
  { key: 'ratchetConfirmTolerancePct', label: '棘轮确认容差', hint: '接近高点的允许偏差', unit: '%', kind: 'nonneg-decimal', group: 'ratchet-stale' },
  { key: 'staleProfitPct', label: '滞涨止盈浮盈', hint: '完整模式，买一停滞时离场', unit: '%', kind: 'nonneg-decimal', group: 'ratchet-stale' },
  { key: 'staleProfitBidUnchangedSec', label: '滞涨买一不变时长', unit: '秒', kind: 'nonneg-int', group: 'ratchet-stale' },
  { key: 'stagnantProfitPct', label: '停滞止盈浮盈', hint: '完整模式，无进展超时离场', unit: '%', kind: 'nonneg-decimal', group: 'ratchet-stale' },
  { key: 'stagnantDurationSec', label: '停滞无进展时长', unit: '秒', kind: 'nonneg-int', group: 'ratchet-stale' },
  { key: 'depthCollapseThresholdPct', label: '深度塌陷退出阈值', hint: '盘口总深度骤降比例', unit: '%', kind: 'nonneg-decimal', group: 'ratchet-stale' },

  // 5. 行情保护与挂单出场
  { key: 'maxBidWickPct', label: '防插针偏离上限', hint: '买一低于中价超此比例按中价判定止损', unit: '%', kind: 'nonneg-decimal', group: 'quote-maker' },
  { key: 'maxLastPriceAgeSec', label: '记忆价判定有效期', hint: '无买一时参考价最大时长', unit: '秒', kind: 'pos-int', group: 'quote-maker' },
  { key: 'maxBookAgeSec', label: '盘口定价有效期', hint: '盘口过期后拒绝定价离场', unit: '秒', kind: 'pos-int', group: 'quote-maker' },
  { key: 'makerFirstExitEnabled', label: '固定止盈优先挂单', hint: '固定百分比止盈先挂单再追单', kind: 'bool', group: 'quote-maker' },
  { key: 'makerExitsForTpOnly', label: '时间退出走挂单', hint: '完整模式 time_exit 是否挂单', kind: 'bool', group: 'quote-maker' },
  { key: 'strategyExitMakerFirst', label: '策略出场 maker-first', hint: '止盈优先挂单，止损不转挂单', kind: 'bool', group: 'quote-maker' },
  { key: 'exitMakerTimeoutMs', label: '挂单出场追单超时', hint: '0=沿用入场默认，负数=永不追单', unit: 'ms', kind: 'signed-int', group: 'quote-maker' },
]

export function syncExitBuffers(exit: RiskExitView, target: Record<string, string>): void {
  for (const def of EXIT_FIELD_DEFS) {
    const val = exit[def.key]
    if (val === undefined) continue
    target[def.key] = String(val)
  }
}

export function buildExitPatchFromBuffers(
  exit: RiskExitView,
  bufs: Record<string, string>,
): Record<string, string | number | boolean> | string {
  const patch: Record<string, string | number | boolean> = {}
  for (const def of EXIT_FIELD_DEFS) {
    const curVal = exit[def.key]
    if (curVal === undefined) continue
    if (def.kind === 'bool') {
      const nextBool = (bufs[def.key] ?? 'false') === 'true'
      if (nextBool !== Boolean(curVal)) {
        patch[def.key] = nextBool
      }
      continue
    }
    const raw = (bufs[def.key] ?? '').trim()
    if (raw === '' || raw === String(curVal)) continue
    const n = Number(raw)
    if (!Number.isFinite(n)) return `${def.label}：必须是有效数字`
    switch (def.kind) {
      case 'pos-decimal':
        if (n <= 0) return `${def.label}：必须 > 0`
        patch[def.key] = raw
        break
      case 'nonneg-decimal':
        if (n < 0) return `${def.label}：必须 ≥ 0`
        patch[def.key] = raw
        break
      case 'price':
        if (n < 0 || n > 1) return `${def.label}：必须在 0 ~ 1 之间（0 = 关闭）`
        patch[def.key] = raw
        break
      case 'nonneg-int':
        if (!Number.isInteger(n) || n < 0) return `${def.label}：必须是 ≥ 0 的整数`
        patch[def.key] = n
        break
      case 'pos-int':
        if (!Number.isInteger(n) || n < 1) return `${def.label}：必须是 ≥ 1 的整数`
        patch[def.key] = n
        break
      case 'signed-int':
        if (!Number.isInteger(n)) return `${def.label}：必须是整数`
        patch[def.key] = n
        break
    }
  }
  return patch
}
