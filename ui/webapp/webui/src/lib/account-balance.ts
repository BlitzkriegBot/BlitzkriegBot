/**
 * 账户级现金流水账本的影子。
 *
 * 余额 = 账本现金（`row.balance`），净利 = 内核上报的 cumulativeNetPnl
 * （账本口径 SSOT，操作员裁决 2026-11-08）。面板不再按
 * balance − seededTotal 另算一套；cumulativeNetPnl 缺失（LIVE 尚未
 * 收到首笔期初权益）= 净利「未知」，绝不回退到 cash − seed。
 * 这是全面板余额/净利显示的唯一来源，绝不参杂
 * 成交折叠口径（trade fold `netPnlUsd` 只用于权益曲线/胜率/手续费等
 * 「成交口径」展示，不参与余额拼接）。
 *
 * 操作员裁决 2026-11-08：账本流水=唯一真相，面板不再拿成交行的
 * 模型费净利去拼一个可能两头都不是的余额。
 *
 * 返回 `null` 时表示没有账户行可读，调用方自行决定兜底（快照顶层
 * balance 或保持空态），绝不自行编造一个口径。
 */
import type { BalanceView } from './balance'
import { balanceView } from './balance'
import type { PanelAccountRow } from '@/stores/panel'

export function useAccountBalance(
  row: PanelAccountRow | null,
): BalanceView | null {
  if (!row) return null

	  const cash = Number(row.balance)
	  const seed = row.seededTotal !== undefined ? Number(row.seededTotal) || 0 : null
	  // 净利按内核账本口径 SSOT（row.cumulativeNetPnl）读取，不再按
	  // cash − seed 另算（那是导致 Live 下「净利 = 全部余额」的根因）。
	  // 期初权益未知时 cumulativeNetPnl 为 null → 净利未知，向下游透传
	  // null，显示「未知」，绝不回退到 cash − seed 或任何成交折叠。
	  const net = row.cumulativeNetPnl ?? null

	  return balanceView(
    {
      balance: cash,
      reserved: Number(row.reserved) || 0,
      available: Number(row.available) || 0,
      seed,
    },
    net,
    0,
  )
}
