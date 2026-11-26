/**
 * The blueprint editor's data model — mirrors core/blitzkrieg_core/src/blueprint.rs
 * and core/blitzkrieg_core/src/blueprint/runtime.rs (SSOT schema served via
 * `blueprint.schema`). Local lists act as offline/initial fallbacks before
 * `api.blueprintSchema()` resolves.
 */

import { clonePlain } from './clone-plain'

/** The nine node types the v1/v2 compiler accepts (blueprint.rs NodeKind). */
export type BlueprintNodeType =
  | 'data_source'
  | 'condition'
  | 'action_buy'
  | 'action_buy_pair'
  | 'action_sell'
  | 'action_hold'
  | 'constant'
  | 'logic_and'
  | 'logic_or'

/** Node families for palette grouping + edge semantics. */
export type NodeFamily = 'source' | 'condition' | 'action' | 'gate'

/** Whitelisted fields (fallback matching kernel blueprint.schema SSOT). */
export const FIELD_WHITELIST = [
  'tick.price',
  'tick.mid_price',
  'tick.best_bid',
  'tick.best_ask',
  'tick.spread',
  'tick.obi',
  'tick.time_left_sec',
  'tick.elapsed_pct',
  'tick.asset',
  'tick.side_label',
  'tick.pair_ask_sum',
  'tick.pair_bid_sum',
  'tick.round_min_price',
  'tick.round_max_price',
  'tick.consensus_aligned',
  'tick.trend_confirmed',
  'tick.spot_return_bps',
  'account.available_balance',
] as const
export type WhitelistedField = (typeof FIELD_WHITELIST)[number]

/** The seven whitelisted operators (CONDITION_OPS) — the condition dropdown. */
export const CONDITION_OPS = ['<', '<=', '>', '>=', '==', '!=', 'in'] as const
export type WhitelistedOp = (typeof CONDITION_OPS)[number]

/** Optional v2 strategy metadata carried onto manifest.json & native runner. */
export interface BlueprintMeta {
  description?: string
  holds_to_settlement?: boolean
  entry_maker?: boolean
  once_per_round?: boolean
  thesis_death?: boolean
  gate_exemptions?: {
    timing?: boolean
    momentum?: boolean
    timing_min_time_left_sec?: number
  }
}

/** One node in the editor graph. */
export interface BpNode {
  id: string
  type: BlueprintNodeType
  params: Record<string, unknown>
}

/** One edge; `when` is the compiler's polarity (`false` → `not (…)`) on guard edges. */
export interface BpEdge {
  from: string
  to: string
  when: boolean
}

/** The v1/v2 blueprint document — exact wire shape the kernel parses. */
export interface BlueprintDoc {
  version: 1 | 2
  name: string
  meta?: BlueprintMeta
  nodes: BpNode[]
  edges: BpEdge[]
}

/** Params a node type exposes to the property panel (in panel order). */
export interface NodeSpec {
  type: BlueprintNodeType
  label: string
  family: NodeFamily
  /** One-line panel hint. */
  hint: string
}

export const NODE_SPECS: NodeSpec[] = [
  { type: 'data_source', label: '数据源', family: 'source', hint: '白名单字段（内核动态同步）' },
  { type: 'condition', label: '条件', family: 'condition', hint: 'op + value；左操作数 = field 或入边' },
  { type: 'action_buy', label: '买入', family: 'action', hint: '单腿买入；缺省 price 按实时盘口成交' },
  { type: 'action_buy_pair', label: '双腿买入', family: 'action', hint: '同时买入 UP+DOWN 配对腿' },
  { type: 'action_sell', label: '卖出', family: 'action', hint: 'price + budget_ratio；有 price 即限价单' },
  { type: 'action_hold', label: '持有', family: 'action', hint: '只输出守卫，不下单' },
  { type: 'constant', label: '常量', family: 'source', hint: '数字 / 字符串 / 布尔' },
  { type: 'logic_and', label: '且', family: 'gate', hint: '≥2 条守卫入边' },
  { type: 'logic_or', label: '或', family: 'gate', hint: '≥2 条守卫入边' },
]

export function nodeSpec(type: string): NodeSpec {
  return NODE_SPECS.find((s) => s.type === type) ?? NODE_SPECS[0]
}

/** Value producers feed operand edges; guard producers feed guard-chain edges. */
export function isValueProducer(type: string): boolean {
  return type === 'data_source' || type === 'constant'
}

/** The edge family a NEW connection belongs to, decided by its SOURCE type. */
export function edgeFamily(fromType: string): 'operand' | 'guard' {
  return isValueProducer(fromType) ? 'operand' : 'guard'
}

export function defaultParams(type: BlueprintNodeType): Record<string, unknown> {
  switch (type) {
    case 'data_source':
      return { field: 'tick.price' }
    case 'condition':
      return { op: '>=', value: 0.85 }
    case 'action_buy':
      return { budget_ratio: 0.1 }
    case 'action_buy_pair':
      return { maker: false, budget_ratio: 0.1 }
    case 'action_sell':
      return { price: 0.95, budget_ratio: 0.1 }
    case 'action_hold':
      return {}
    case 'constant':
      return { value: 0.85 }
    case 'logic_and':
    case 'logic_or':
      return {}
  }
}

/**
 * Default starter blueprint template: Blueprint v2 `late_favorite` (BTC only).
 * 45–60s left, price in [0.85, 0.97], once per round, hold to settlement.
 */
export function starterBlueprint(): BlueprintDoc {
  return {
    version: 2,
    name: 'late_favorite',
    meta: {
      description: 'Near-settlement favorite (BTC only): 45-60s left, buy BTC side in [0.85, 0.97] and hold to settlement.',
      holds_to_settlement: true,
      entry_maker: false,
      once_per_round: true,
      thesis_death: false,
      gate_exemptions: {
        timing: true,
        momentum: false,
        timing_min_time_left_sec: 45,
      },
    },
    nodes: [
      { id: 'c_asset', type: 'condition', params: { field: 'tick.asset', op: '==', value: 'BTC' } },
      { id: 'c_tl_min', type: 'condition', params: { field: 'tick.time_left_sec', op: '>=', value: 45 } },
      { id: 'c_tl_max', type: 'condition', params: { field: 'tick.time_left_sec', op: '<=', value: 60 } },
      { id: 'c_px_min', type: 'condition', params: { field: 'tick.price', op: '>=', value: 0.85 } },
      { id: 'c_px_max', type: 'condition', params: { field: 'tick.price', op: '<=', value: 0.97 } },
      { id: 'gate_and', type: 'logic_and', params: {} },
      { id: 'act_buy', type: 'action_buy', params: { budget_ratio: 0.1 } },
    ],
    edges: [
      { from: 'c_asset', to: 'gate_and', when: true },
      { from: 'c_tl_min', to: 'gate_and', when: true },
      { from: 'c_tl_max', to: 'gate_and', when: true },
      { from: 'c_px_min', to: 'gate_and', when: true },
      { from: 'c_px_max', to: 'gate_and', when: true },
      { from: 'gate_and', to: 'act_buy', when: true },
    ],
  }
}

/**
 * Serialize the editor graph into the v2 blueprint document (preserving meta).
 */
export function toBlueprintDoc(
  name: string,
  nodes: BpNode[],
  edges: BpEdge[],
  meta?: BlueprintMeta,
): string {
  const doc: BlueprintDoc = {
    version: 2,
    name: name.trim() || 'untitled_strategy',
    ...(meta ? { meta } : {}),
    nodes: nodes.map((n) => ({ id: n.id, type: n.type, params: n.params })),
    edges: edges.map((e) => ({ from: e.from, to: e.to, when: e.when })),
  }
  return JSON.stringify(doc, null, 2)
}

/**
 * Parse an imported blueprint JSON into the editor graph (supports v1 and v2).
 */
export function parseBlueprintDoc(raw: string): { doc: BlueprintDoc } | { error: string } {
  let parsed: unknown
  try {
    parsed = JSON.parse(raw)
  } catch {
    return { error: '不是合法 JSON。' }
  }
  if (!parsed || typeof parsed !== 'object') return { error: '蓝图文档必须是 JSON 对象。' }
  const doc = parsed as Partial<BlueprintDoc>
  if (doc.version !== 1 && doc.version !== 2) {
    return { error: '仅支持 v1 或 v2 版蓝图文档。' }
  }
  if (!Array.isArray(doc.nodes) || !Array.isArray(doc.edges)) {
    return { error: '缺 nodes 或 edges 数组。' }
  }
  const nodes: BpNode[] = []
  const seen = new Set<string>()
  for (const n of doc.nodes) {
    if (!n || typeof n.id !== 'string' || n.id === '' || seen.has(n.id)) continue
    if (!NODE_SPECS.some((s) => s.type === n.type)) continue
    seen.add(n.id)
    nodes.push({ id: n.id, type: n.type as BlueprintNodeType, params: clonePlain(n.params ?? {}) })
  }
  if (nodes.length === 0) return { error: 'nodes 里没有可识别的节点（类型必须在白名单内、id 非空且不重复）。' }
  const edges: BpEdge[] = []
  for (const e of doc.edges) {
    if (!e || !seen.has(e.from) || !seen.has(e.to)) continue
    edges.push({ from: e.from, to: e.to, when: !!e.when })
  }
  return {
    doc: {
      version: doc.version,
      name: typeof doc.name === 'string' ? doc.name : 'untitled_strategy',
      ...(doc.meta && typeof doc.meta === 'object' ? { meta: clonePlain(doc.meta) } : {}),
      nodes,
      edges,
    },
  }
}

export interface BlueprintSaveBody {
  name: string
  json: string
  overwrite?: boolean
}
