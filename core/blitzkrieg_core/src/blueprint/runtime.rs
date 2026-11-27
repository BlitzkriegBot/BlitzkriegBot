//! Native Rust Blueprint runtime & Single-Source-of-Truth (SSOT) strategy schema.
//!
//! Eliminates three-way drift between Rust strategies, Lua strategies, and
//! Blueprint strategies:
//! - [`schema()`] is the single canonical schema of all whitelisted fields,
//!   operators, and node types (served directly to WebUI via `blueprint.schema`).
//! - [`UnifiedTickView`] is the single read-only per-token snapshot (serializable
//!   for Lua/JSON AND directly queryable by Rust via `get_field`).
//! - [`BlueprintStrategy`] implements [`crate::strategies::EngineStrategy`]
//!   natively in Rust — no Lua string round-trip at runtime.

use super::{Blueprint, BlueprintMeta, CONDITION_OPS, Graph, NodeKind};
use crate::model::{OrderbookSnapshot, SignalDirection};
use crate::strategies::{EngineStrategy, GateExemptions, StrategyCtx};
use arc_swap::ArcSwap;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use strategy_logic::params::{KnobSpec, StrategyParams};
use strategy_logic::signal::{TradePairDecl, TradeSignal};

/// Wire-ready description of one whitelisted strategy field.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlueprintFieldDef {
    pub name: &'static str,
    pub value_type: &'static str,
    pub label: &'static str,
    pub group: &'static str,
}

/// Wire-ready description of one blueprint node type.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlueprintNodeDef {
    pub node_type: &'static str,
    pub label: &'static str,
    pub family: &'static str,
    pub hint: &'static str,
}

/// Complete SSOT schema returned by `blueprint.schema`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlueprintSchema {
    pub supported_versions: Vec<u32>,
    pub default_version: u32,
    pub fields: Vec<BlueprintFieldDef>,
    pub operators: Vec<&'static str>,
    pub nodes: Vec<BlueprintNodeDef>,
}

/// Single Source of Truth for all blueprint fields, operators, and node types.
pub fn schema() -> BlueprintSchema {
    BlueprintSchema {
        supported_versions: vec![1, 2],
        default_version: 2,
        fields: vec![
            BlueprintFieldDef {
                name: "tick.price",
                value_type: "number",
                label: "当前买一/卖一价",
                group: "book",
            },
            BlueprintFieldDef {
                name: "tick.mid_price",
                value_type: "number",
                label: "盘口中间价",
                group: "book",
            },
            BlueprintFieldDef {
                name: "tick.best_bid",
                value_type: "number",
                label: "最优买价 (Bid)",
                group: "book",
            },
            BlueprintFieldDef {
                name: "tick.best_ask",
                value_type: "number",
                label: "最优卖价 (Ask)",
                group: "book",
            },
            BlueprintFieldDef {
                name: "tick.spread",
                value_type: "number",
                label: "买卖价差",
                group: "book",
            },
            BlueprintFieldDef {
                name: "tick.obi",
                value_type: "number",
                label: "盘口失衡 (OBI)",
                group: "book",
            },
            BlueprintFieldDef {
                name: "tick.time_left_sec",
                value_type: "number",
                label: "轮次剩余秒数",
                group: "round",
            },
            BlueprintFieldDef {
                name: "tick.elapsed_pct",
                value_type: "number",
                label: "轮次已过比例 (0-1)",
                group: "round",
            },
            BlueprintFieldDef {
                name: "tick.asset",
                value_type: "string",
                label: "标的资产 (BTC/ETH/SOL/XRP)",
                group: "market",
            },
            BlueprintFieldDef {
                name: "tick.side_label",
                value_type: "string",
                label: "方向腿 (up/down)",
                group: "market",
            },
            BlueprintFieldDef {
                name: "tick.pair_ask_sum",
                value_type: "number",
                label: "双腿 Ask 之和",
                group: "pair",
            },
            BlueprintFieldDef {
                name: "tick.pair_bid_sum",
                value_type: "number",
                label: "双腿 Bid 之和",
                group: "pair",
            },
            BlueprintFieldDef {
                name: "tick.round_min_price",
                value_type: "number",
                label: "本轮最低观测价",
                group: "state",
            },
            BlueprintFieldDef {
                name: "tick.round_max_price",
                value_type: "number",
                label: "本轮最高观测价",
                group: "state",
            },
            BlueprintFieldDef {
                name: "tick.consensus_aligned",
                value_type: "boolean",
                label: "多资产同向共识",
                group: "signal",
            },
            BlueprintFieldDef {
                name: "tick.trend_confirmed",
                value_type: "boolean",
                label: "趋势确认标志",
                group: "signal",
            },
            BlueprintFieldDef {
                name: "tick.spot_return_bps",
                value_type: "number",
                label: "现货轮内涨跌幅 (bps)",
                group: "signal",
            },
            BlueprintFieldDef {
                name: "account.available_balance",
                value_type: "number",
                label: "账户可用余额",
                group: "account",
            },
        ],
        operators: CONDITION_OPS.to_vec(),
        nodes: vec![
            BlueprintNodeDef {
                node_type: "data_source",
                label: "数据源",
                family: "source",
                hint: "白名单字段（内核动态同步）",
            },
            BlueprintNodeDef {
                node_type: "condition",
                label: "条件",
                family: "condition",
                hint: "op + value；左操作数 = field 或入边",
            },
            BlueprintNodeDef {
                node_type: "action_buy",
                label: "买入",
                family: "action",
                hint: "单腿买入；缺省 price 按实时盘口成交",
            },
            BlueprintNodeDef {
                node_type: "action_buy_pair",
                label: "双腿买入",
                family: "action",
                hint: "同时买入 UP+DOWN 配对腿",
            },
            BlueprintNodeDef {
                node_type: "action_sell",
                label: "卖出",
                family: "action",
                hint: "price + budget_ratio",
            },
            BlueprintNodeDef {
                node_type: "action_hold",
                label: "持有",
                family: "action",
                hint: "只输出守卫，不下单",
            },
            BlueprintNodeDef {
                node_type: "constant",
                label: "常量",
                family: "source",
                hint: "数字 / 字符串 / 布尔",
            },
            BlueprintNodeDef {
                node_type: "logic_and",
                label: "且",
                family: "gate",
                hint: "≥2 条守卫入边",
            },
            BlueprintNodeDef {
                node_type: "logic_or",
                label: "或",
                family: "gate",
                hint: "≥2 条守卫入边",
            },
        ],
    }
}

/// Scalar runtime value produced by a `UnifiedTickView` field or `constant` node.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldVal {
    Num(f64),
    Str(String),
    Bool(bool),
}

/// Unified read-only tick snapshot shared across Rust, Lua (via serde), and
/// native Blueprint evaluation.
#[derive(Debug, Clone, Serialize)]
pub struct UnifiedTickView {
    pub asset: String,
    pub side_label: String,
    pub price: f64,
    pub mid_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub spread: Option<f64>,
    pub obi: Option<f64>,
    pub time_left_sec: f64,
    pub elapsed_pct: Option<f64>,
    pub pair_ask_sum: Option<f64>,
    pub pair_bid_sum: Option<f64>,
    pub round_min_price: f64,
    pub round_max_price: f64,
    pub consensus_aligned: bool,
    pub trend_confirmed: Option<bool>,
    pub spot_return_bps: Option<f64>,
    pub available_balance: Option<f64>,
}

impl UnifiedTickView {
    /// Single canonical field lookup used by the native Blueprint evaluator.
    pub fn get_field(&self, name: &str) -> Option<FieldVal> {
        match name {
            "tick.price" => Some(FieldVal::Num(self.price)),
            "tick.mid_price" => self.mid_price.map(FieldVal::Num),
            "tick.best_bid" => self.best_bid.map(FieldVal::Num),
            "tick.best_ask" => self.best_ask.map(FieldVal::Num),
            "tick.spread" => self.spread.map(FieldVal::Num),
            "tick.obi" => self.obi.map(FieldVal::Num),
            "tick.time_left_sec" => Some(FieldVal::Num(self.time_left_sec)),
            "tick.elapsed_pct" => self.elapsed_pct.map(FieldVal::Num),
            "tick.asset" => Some(FieldVal::Str(self.asset.clone())),
            "tick.side_label" => Some(FieldVal::Str(self.side_label.clone())),
            "tick.pair_ask_sum" => self.pair_ask_sum.map(FieldVal::Num),
            "tick.pair_bid_sum" => self.pair_bid_sum.map(FieldVal::Num),
            "tick.round_min_price" => Some(FieldVal::Num(self.round_min_price)),
            "tick.round_max_price" => Some(FieldVal::Num(self.round_max_price)),
            "tick.consensus_aligned" => Some(FieldVal::Bool(self.consensus_aligned)),
            "tick.trend_confirmed" => self.trend_confirmed.map(FieldVal::Bool),
            "tick.spot_return_bps" => self.spot_return_bps.map(FieldVal::Num),
            "account.available_balance" => self.available_balance.map(FieldVal::Num),
            _ => None,
        }
    }
}

/// Native Rust execution engine for any validated Blueprint (v1 or v2).
///
/// Implements [`EngineStrategy`] directly so Blueprint strategies share the
/// exact same execution surface, latency, and safety boundary as Rust strategies.
pub struct BlueprintStrategy {
    bp: Blueprint,
    graph: Graph,
    topo: Vec<String>,
    action_id: String,
    account_id: String,
    knobs: Vec<KnobSpec>,
    params_cell: Option<Arc<ArcSwap<StrategyParams>>>,
    slot_seen: Option<i64>,
    fired: HashSet<String>,
    min_px: HashMap<String, f64>,
    max_px: HashMap<String, f64>,
    round_duration_sec: f64,
}

impl BlueprintStrategy {
    /// Parse and validate a Blueprint JSON document into a runnable native
    /// Rust [`EngineStrategy`].
    pub fn from_json(json: &str) -> Result<Self, String> {
        let bp: Blueprint =
            serde_json::from_str(json).map_err(|e| format!("blueprint parse failed: {e}"))?;
        let graph = super::validate_structure(&bp)?;
        super::validate_semantics(&bp, &graph)?;
        let topo = super::topo_order(&bp, &graph.outgoing)?;
        let action_id = bp
            .nodes
            .iter()
            .find(|n| {
                graph.kind_by_id[&n.id] == NodeKind::Action && graph.incoming.contains_key(&n.id)
            })
            .map(|n| n.id.clone())
            .ok_or_else(|| "blueprint has no wired action node".to_string())?;
        Ok(Self {
            bp,
            graph,
            topo,
            action_id,
            account_id: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            knobs: Vec::new(),
            params_cell: None,
            slot_seen: None,
            fired: HashSet::new(),
            min_px: HashMap::new(),
            max_px: HashMap::new(),
            round_duration_sec: 300.0,
        })
    }

    /// Override the strategy registration name (e.g., to match `manifest.json`
    /// when a saved package was renamed from its blueprint document's internal name).
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.bp.name = name.into();
        self
    }

    /// Scope this native blueprint instance to a specific account.
    pub fn with_account_id(mut self, account_id: impl Into<String>) -> Self {
        self.account_id = account_id.into();
        self
    }

    /// Attach evolvable/hot-tunable knob specs (typically from `manifest.json`).
    pub fn with_knobs(mut self, knobs: Vec<KnobSpec>) -> Self {
        self.knobs = knobs;
        self
    }

    /// Resolve a numeric knob override from the live ParamRegistry cell if
    /// present, falling back to the blueprint document's literal value.
    fn param_f64(&self, knob_name: &str, fallback: f64) -> f64 {
        self.params_cell
            .as_ref()
            .and_then(|cell| cell.load().get(knob_name))
            .and_then(|d| d.to_f64())
            .unwrap_or(fallback)
    }

    pub fn meta(&self) -> &BlueprintMeta {
        &self.bp.meta
    }

    fn reset_round(&mut self, slot: i64, time_left_sec: i64) {
        if self.slot_seen != Some(slot) {
            self.slot_seen = Some(slot);
            self.fired.clear();
            self.min_px.clear();
            self.max_px.clear();
            if time_left_sec > 300 {
                self.round_duration_sec = 900.0;
            } else {
                self.round_duration_sec = 300.0;
            }
        }
    }

    fn eval_graph(&self, tick: &UnifiedTickView) -> bool {
        let mut vals: HashMap<&str, Option<FieldVal>> = HashMap::new();
        let mut guards: HashMap<&str, bool> = HashMap::new();

        for id in &self.topo {
            let Some(n) = self.bp.nodes.iter().find(|x| &x.id == id) else {
                continue;
            };
            match self.graph.kind_by_id[id] {
                NodeKind::Source if n.node_type == "data_source" => {
                    let field = n
                        .params
                        .get("field")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    vals.insert(id.as_str(), tick.get_field(field));
                }
                NodeKind::Source => {
                    let v = n.params.get("value").and_then(json_to_field_val);
                    vals.insert(id.as_str(), v);
                }
                NodeKind::Condition => {
                    let left = if let Some(field) = n.params.get("field").and_then(|v| v.as_str()) {
                        tick.get_field(field)
                    } else {
                        self.graph
                            .incoming
                            .get(id)
                            .and_then(|ins| {
                                ins.iter()
                                    .find(|(src, _)| self.graph.kind_by_id[src] == NodeKind::Source)
                            })
                            .and_then(|(src, _)| vals.get(src.as_str()).cloned().flatten())
                    };
                    let op = n.params.get("op").and_then(|v| v.as_str()).unwrap_or("==");
                    let rhs_override = n.params.get("value").and_then(|v| v.as_f64()).map(|raw| {
                        let live = self.param_f64(&format!("{}_value", n.id), raw);
                        serde_json::json!(live)
                    });
                    let rhs = rhs_override.as_ref().or_else(|| n.params.get("value"));
                    let ok = match (left, rhs) {
                        (Some(lv), Some(rv)) => eval_cmp(&lv, op, rv),
                        _ => false,
                    };
                    guards.insert(id.as_str(), ok);
                }
                NodeKind::Gate => {
                    let ins = self.graph.incoming.get(id);
                    let is_and = n.node_type == "logic_and";
                    let res = if let Some(edges) = ins {
                        if is_and {
                            edges.iter().all(|(src, when)| {
                                let g = guards.get(src.as_str()).copied().unwrap_or(false);
                                if *when { g } else { !g }
                            })
                        } else {
                            edges.iter().any(|(src, when)| {
                                let g = guards.get(src.as_str()).copied().unwrap_or(false);
                                if *when { g } else { !g }
                            })
                        }
                    } else {
                        false
                    };
                    guards.insert(id.as_str(), res);
                }
                NodeKind::Action => {}
            }
        }

        // Walk the guard chain feeding the wired action node.
        let mut cur = self.action_id.as_str();
        loop {
            let Some(ins) = self.graph.incoming.get(cur) else {
                break;
            };
            let Some((src, when)) = ins
                .iter()
                .find(|(s, _)| self.graph.kind_by_id[s] != NodeKind::Source)
            else {
                break;
            };
            let g = guards.get(src.as_str()).copied().unwrap_or(false);
            let passed = if *when { g } else { !g };
            if !passed {
                return false;
            }
            cur = src.as_str();
        }
        true
    }
}

fn json_to_field_val(v: &serde_json::Value) -> Option<FieldVal> {
    if let Some(n) = v.as_f64() {
        Some(FieldVal::Num(n))
    } else if let Some(b) = v.as_bool() {
        Some(FieldVal::Bool(b))
    } else {
        v.as_str().map(|s| FieldVal::Str(s.to_string()))
    }
}

fn eval_cmp(left: &FieldVal, op: &str, rhs: &serde_json::Value) -> bool {
    if op == "in" {
        let Some(arr) = rhs.as_array() else {
            return false;
        };
        return arr.iter().any(|m| eval_cmp(left, "==", m));
    }
    match left {
        FieldVal::Num(l) => {
            let Some(r) = rhs.as_f64() else {
                return false;
            };
            match op {
                "<" => *l < r,
                "<=" => *l <= r,
                ">" => *l > r,
                ">=" => *l >= r,
                "==" => (*l - r).abs() < 1e-12,
                "!=" => (*l - r).abs() >= 1e-12,
                _ => false,
            }
        }
        FieldVal::Str(l) => {
            let Some(r) = rhs.as_str() else {
                return false;
            };
            match op {
                "==" => l == r,
                "!=" => l != r,
                _ => false,
            }
        }
        FieldVal::Bool(l) => {
            let Some(r) = rhs.as_bool() else {
                return false;
            };
            match op {
                "==" => l == &r,
                "!=" => l != &r,
                _ => false,
            }
        }
    }
}

fn book_ask(b: &OrderbookSnapshot) -> Option<f64> {
    (!b.best_ask.is_zero())
        .then(|| b.best_ask.to_f64())
        .flatten()
}

fn book_bid(b: &OrderbookSnapshot) -> Option<f64> {
    (!b.best_bid.is_zero())
        .then(|| b.best_bid.to_f64())
        .flatten()
}

fn book_mid(b: &OrderbookSnapshot) -> Option<f64> {
    match (book_bid(b), book_ask(b)) {
        (Some(bid), Some(ask)) => Some((bid + ask) / 2.0),
        (Some(bid), None) => Some(bid),
        (None, Some(ask)) => Some(ask),
        (None, None) => None,
    }
}

fn compute_consensus(ctx: &StrategyCtx<'_>) -> Option<&'static str> {
    let mut up_n = 0usize;
    let mut dn_n = 0usize;
    for m in ctx.markets() {
        let pu = ctx
            .fresh_book(&m.up_token_id)
            .as_ref()
            .and_then(|b| book_mid(b).or_else(|| book_ask(b)));
        let pd = ctx
            .fresh_book(&m.down_token_id)
            .as_ref()
            .and_then(|b| book_mid(b).or_else(|| book_ask(b)));
        if pu.is_some_and(|p| p >= 0.65) {
            up_n += 1;
        } else if pd.is_some_and(|p| p >= 0.65) {
            dn_n += 1;
        }
    }
    if up_n >= 3 {
        Some("up")
    } else if dn_n >= 3 {
        Some("down")
    } else {
        None
    }
}

impl EngineStrategy for BlueprintStrategy {
    fn name(&self) -> &str {
        &self.bp.name
    }

    fn on_book(&mut self, token_id: &str, snap: &OrderbookSnapshot, _now_ms: i64) {
        if let Some(px) = book_ask(snap).or_else(|| book_mid(snap)) {
            let mn = self.min_px.entry(token_id.to_string()).or_insert(px);
            if px < *mn {
                *mn = px;
            }
            let mx = self.max_px.entry(token_id.to_string()).or_insert(px);
            if px > *mx {
                *mx = px;
            }
        }
    }

    fn on_round(&mut self, slot: i64, time_left_sec: i64, _now_ms: i64) {
        self.reset_round(slot, time_left_sec);
    }

    fn gate_exemptions(&self) -> GateExemptions {
        let Some(ge) = &self.bp.meta.gate_exemptions else {
            return GateExemptions::none();
        };
        GateExemptions {
            timing: ge.timing,
            momentum: ge.momentum,
            timing_min_time_left_sec: ge.timing_min_time_left_sec,
        }
    }

    fn holds_to_settlement(&self) -> bool {
        self.bp.meta.holds_to_settlement
    }

    fn entry_maker(&self) -> bool {
        self.bp.meta.entry_maker
    }

    fn reads_wall_clock(&self) -> bool {
        false
    }

    fn instance_account(&self) -> String {
        self.account_id.clone()
    }

    fn set_hot_params(&mut self, registry: Option<Arc<crate::registry::ParamRegistry>>) {
        self.params_cell = registry.as_ref().and_then(|r| r.handle_for(self.name()));
    }

    fn evolvable_knobs(&self) -> Vec<KnobSpec> {
        self.knobs.clone()
    }

    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        self.reset_round(ctx.round_slot(), ctx.time_left_sec());
        let Some(action) = self.bp.nodes.iter().find(|n| n.id == self.action_id) else {
            return Vec::new();
        };
        if action.node_type == "action_hold" {
            return Vec::new();
        }
        let maker = action
            .params
            .get("maker")
            .and_then(|v| v.as_bool())
            .unwrap_or(self.bp.meta.entry_maker);
        let fixed_price = action
            .params
            .get("price")
            .and_then(|v| v.as_f64())
            .map(|fp| self.param_f64(&format!("{}_price", action.id), fp));

        let held_tokens: HashSet<&str> = ctx
            .held_positions()
            .iter()
            .map(|h| h.token_id.as_str())
            .collect();
        let consensus = compute_consensus(ctx);
        let mut out = Vec::new();

        for m in ctx.markets() {
            let fk = format!("{}:{}", ctx.round_slot(), m.condition_id);
            if self.bp.meta.once_per_round
                && (self.fired.contains(&fk)
                    || held_tokens.contains(m.up_token_id.as_str())
                    || held_tokens.contains(m.down_token_id.as_str()))
            {
                continue;
            }

            let legs = [
                (&m.up_token_id, SignalDirection::Up, "up", &m.down_token_id),
                (
                    &m.down_token_id,
                    SignalDirection::Down,
                    "down",
                    &m.up_token_id,
                ),
            ];

            for (token, dir, side_label, other_token) in legs {
                if self.bp.meta.once_per_round && self.fired.contains(&fk) {
                    break;
                }
                let Some(b) = ctx.fresh_book(token) else {
                    continue;
                };
                let ask = book_ask(&b);
                let bid = book_bid(&b);
                let mid = book_mid(&b);
                let Some(price) = ask.or(mid) else {
                    continue;
                };
                let mn = *self
                    .min_px
                    .entry(token.clone())
                    .and_modify(|v| {
                        if price < *v {
                            *v = price;
                        }
                    })
                    .or_insert(price);
                let mx = *self
                    .max_px
                    .entry(token.clone())
                    .and_modify(|v| {
                        if price > *v {
                            *v = price;
                        }
                    })
                    .or_insert(price);

                let ob = ctx.fresh_book(other_token);
                let oask = ob.as_ref().and_then(book_ask);
                let obid = ob.as_ref().and_then(book_bid);
                let elapsed = if self.round_duration_sec > 0.0 {
                    ((self.round_duration_sec - ctx.time_left_sec() as f64)
                        / self.round_duration_sec)
                        .clamp(0.0, 1.0)
                } else {
                    f64::NAN
                };

                let tick = UnifiedTickView {
                    asset: m.asset.clone(),
                    side_label: side_label.to_string(),
                    price,
                    mid_price: mid,
                    best_bid: bid,
                    best_ask: ask,
                    spread: b.spread.to_f64().or_else(|| match (ask, bid) {
                        (Some(a), Some(bi)) => Some((a - bi).max(0.0)),
                        _ => None,
                    }),
                    obi: b.obi.to_f64(),
                    time_left_sec: ctx.time_left_sec() as f64,
                    elapsed_pct: if elapsed.is_finite() {
                        Some(elapsed)
                    } else {
                        None
                    },
                    pair_ask_sum: match (ask, oask) {
                        (Some(a), Some(oa)) => Some(a + oa),
                        _ => None,
                    },
                    pair_bid_sum: match (bid, obid) {
                        (Some(bi), Some(obi)) => Some(bi + obi),
                        _ => None,
                    },
                    round_min_price: mn,
                    round_max_price: mx,
                    consensus_aligned: consensus == Some(side_label),
                    trend_confirmed: None,
                    spot_return_bps: None,
                    available_balance: None,
                };

                if !self.eval_graph(&tick) {
                    continue;
                }

                let px_dec = if let Some(fp) = fixed_price {
                    let Some(d) = Decimal::from_f64_retain(fp) else {
                        continue;
                    };
                    d.round_dp(6)
                } else if maker {
                    if !b.best_bid.is_zero() {
                        b.best_bid
                    } else {
                        b.best_ask
                    }
                } else if !b.best_ask.is_zero() {
                    b.best_ask
                } else {
                    b.mid_price
                };
                if px_dec.is_zero() {
                    continue;
                }

                if action.node_type == "action_buy_pair" {
                    let other_dec = ob.as_ref().map(|o| {
                        if maker && !o.best_bid.is_zero() {
                            o.best_bid
                        } else if !o.best_ask.is_zero() {
                            o.best_ask
                        } else {
                            o.mid_price
                        }
                    });
                    let Some(other_dec) = other_dec.filter(|d| !d.is_zero()) else {
                        continue;
                    };
                    let other_dir = match dir {
                        SignalDirection::Up => SignalDirection::Down,
                        SignalDirection::Down => SignalDirection::Up,
                    };
                    let other_leg = if side_label == "up" { "down" } else { "up" };
                    let pair_key = format!("{}:{}", ctx.round_slot(), m.condition_id);
                    out.push(TradeSignal {
                        strategy: self.bp.name.clone(),
                        account_id: self.account_id.clone(),
                        asset: m.asset.clone(),
                        direction: dir,
                        token_id: token.clone(),
                        condition_id: m.condition_id.clone(),
                        price: px_dec,
                        reason: "blueprint-pair".to_string(),
                        shares: None,
                        pair: Some(TradePairDecl {
                            key: pair_key.clone(),
                            leg: side_label.to_string(),
                        }),
                    });
                    out.push(TradeSignal {
                        strategy: self.bp.name.clone(),
                        account_id: self.account_id.clone(),
                        asset: m.asset.clone(),
                        direction: other_dir,
                        token_id: other_token.clone(),
                        condition_id: m.condition_id.clone(),
                        price: other_dec,
                        reason: "blueprint-pair".to_string(),
                        shares: None,
                        pair: Some(TradePairDecl {
                            key: pair_key,
                            leg: other_leg.to_string(),
                        }),
                    });
                    if self.bp.meta.once_per_round {
                        self.fired.insert(fk.clone());
                    }
                    break;
                } else if action.node_type == "action_buy" {
                    out.push(TradeSignal {
                        strategy: self.bp.name.clone(),
                        account_id: self.account_id.clone(),
                        asset: m.asset.clone(),
                        direction: dir,
                        token_id: token.clone(),
                        condition_id: m.condition_id.clone(),
                        price: px_dec,
                        reason: if maker {
                            "blueprint-maker".to_string()
                        } else {
                            "blueprint".to_string()
                        },
                        shares: None,
                        pair: None,
                    });
                    if self.bp.meta.once_per_round {
                        self.fired.insert(fk.clone());
                    }
                }
            }
        }
        out
    }
}
