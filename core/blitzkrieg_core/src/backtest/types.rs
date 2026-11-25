//! Types shared by the backtest driver: public report surface, config,
//! mode ladder, and the in-memory data source.
//! Pure code motion from `backtest.rs` (no logic edits).

use crate::data_source::{DataSource, SourceStats, TimedEvent};
use crate::engine::DataEvent;
use crate::service::CoreConfig;
use crate::sim::FillModel;
use rust_decimal::Decimal;
use serde_json::Value;

/// A `Backtester` replays a data source and reports what the strategy did.
pub trait Backtester {
    /// Run the full replay. Errors are returned only for setup failures; runtime
    /// problems are collected into the report (`errors`/`riskAlerts`).
    fn run(&mut self) -> Result<BacktestReport, String>;
    /// Provenance line for logs (source + model).
    fn describe(&self) -> String;
}

/// How to run a replay. Manual `Debug` because `progress` is a closure (its
/// `Clone` is the `Arc`'s — the callback is shared, not duplicated).
#[derive(Clone, Default)]
pub struct BacktestConfig {
    /// Base core config (strategy knobs: assets, sizing, gates, exit policy,
    /// per-strategy limits). The replay forces the safe subset — see
    /// [`EventBacktester::new`].
    pub core: CoreConfig,
    /// 回放虚拟时钟步长，0 表示事件驱动下界（1ms），延迟档因此是网格的真实倍数
    /// 而不是和网格重合；live 的间隔由 `--tick-ms` 独立控制（默认 10ms）；
    /// 需要粗网格时显式传参。
    pub tick_ms: i64,
    /// After the last event, keep the clock running this long (ms) so pending
    /// maker escalations and exits fire. 0 = stop at the last event.
    pub tail_ms: i64,
    /// Counterfactual knob overrides for a replay: `(strategy, knob, value)`.
    ///
    /// Delivered through the SAME hot-param registry Shadow Evolution uses, so a
    /// counterfactual run differs from the shipped behaviour only in the value
    /// handed over — same declared knobs, same cell, same `on_hot_params` push on
    /// the hot path. Empty (the default) replays the config in force.
    ///
    /// #916: the value arrives as a STRING and its type is decided against the
    /// strategy's OWN manifest declaration at apply time (`Backtester::new`):
    /// `decimal`/`int` tunables are parsed here and ride the registry exactly as
    /// before, `string` tunables ride the raw `bk.params()` bag, and anything
    /// undeclared or shape-mismatched is a startup error — a silently ignored
    /// counterfactual would make a comparison meaningless.
    pub hot_params: Vec<(String, String, String)>,
    /// The `--backtest-mode` that requested this replay (`mine`/`verify`/
    /// `sweep`), reported in the text render's coordinate-system line (#879).
    /// Empty = the historic single-run path, which prints no such line.
    pub mode: String,
    /// Progress callback: `(delivered_events, virtual_clock_ms)`, invoked at
    /// most once per wall second. The single producer — the replay loop — has
    /// two consumers: the CLI prints it to stderr, the #353 IPC job registry
    /// updates a status cell the WebUI polls. `None` keeps the CLI stderr
    /// heartbeat.
    #[allow(clippy::type_complexity)]
    pub progress: Option<std::sync::Arc<dyn Fn(u64, i64) + Send + Sync>>,
}

impl std::fmt::Debug for BacktestConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BacktestConfig")
            .field("core", &self.core)
            .field("tick_ms", &self.tick_ms)
            .field("tail_ms", &self.tail_ms)
            .field("hot_params", &self.hot_params)
            .field("mode", &self.mode)
            .field("progress", &self.progress.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

// ── #351 mode ladder (in the lib so the #353 IPC job registry composes the
//    SAME contract the CLI does — one definition, two consumers) ────────────

/// The mode's latency rungs. The rungs move ONLY latency — slippage and the
/// fee schedule stay as configured across rungs, so the rungs are comparable
/// and any expectation change is attributable to latency alone. (The one
/// per-MODE friction dial, #879: `mine` pins taker slippage to 0 — see
/// [`mode_rung_model`].)
pub fn mode_rungs(mode: &str, verify_latency_ms: i64) -> Result<Vec<i64>, String> {
    match mode {
        "sweep" => Ok(vec![0, 50, 100, 200, 300]),
        "mine" => Ok(vec![0]),
        "verify" => Ok(vec![verify_latency_ms]),
        other => Err(format!(
            "unknown --backtest-mode {other} (mine|verify|sweep)"
        )),
    }
}

/// The fill model for one mode rung. `mine` zeroes the latency dials and —
/// #879 用户裁决（2026-11-02）— pins taker slippage to 0: a 1-tick overlay on
/// a price that already walked the spread (`walk_marketable`'s VWAP) bills
/// the spread twice, so the mining tier stops paying it. `verify`/`sweep`
/// keep the configured friction as the 1-tick control arm, and there the
/// fail-closed floor still refuses a zero taker slippage the OPERATOR asked
/// for — the honest-friction floor is `--slippage-ticks 1`.
pub fn mode_rung_model(mode: &str, base: FillModel, rung: i64) -> Result<FillModel, String> {
    if mode != "mine" && base.taker_slippage_ticks == 0 {
        return Err(format!(
            "backtest-mode {mode} refuses zero taker slippage: the honest-friction floor is \
             --slippage-ticks 1 (verify/sweep 模式禁止把滑点设为 0)"
        ));
    }
    // #862: the mode ladder re-prices the LATENCY dials, which is exactly what
    // a named model owns. An l2-delay base rung through the ladder would be a
    // chimera — the walk's 150 ms coordinate replaced by the rung's while the
    // trade-through rule stayed — so the combination refuses instead.
    if base.maker_fill_rule == crate::sim::MakerFillRule::TradeThrough {
        return Err(
            "--fill-model l2-delay defines its own latency/maker coordinate; --backtest-mode \
             re-prices those dials and the two cannot be combined (l2-delay 与 mode ladder 互斥)"
                .to_string(),
        );
    }
    Ok(match mode {
        "mine" => FillModel {
            taker_latency_ms: 0,
            taker_rtt_ms: 0,
            maker_latency_ms: 0,
            // #879: pinned by the tier itself — the operator's --slippage-ticks
            // no longer reaches the mine rung (it stays the verify/sweep dial).
            taker_slippage_ticks: 0,
            ..base
        },
        "verify" | "sweep" => FillModel {
            taker_latency_ms: rung,
            taker_rtt_ms: rung,
            maker_latency_ms: rung,
            ..base
        },
        other => {
            return Err(format!(
                "unknown --backtest-mode {other} (mine|verify|sweep)"
            ));
        }
    })
}

/// The two rungs the sweep verdict reads: 0 ms and the top (real-latency)
/// rung. Every middle rung — the 50 ms step included — stays in the report
/// for reference and never decides pass/fail: on the shipped 1 ms replay grid
/// a 50 ms rung is fifty grid steps, and judging "latency insensitivity" on it
/// re-introduces the one-step cliff the old 50 ms grid produced.
pub fn verdict_basis_nets(nets: &[Decimal]) -> Vec<Decimal> {
    match (nets.first(), nets.last()) {
        (Some(first), Some(last)) if nets.len() > 1 => vec![*first, *last],
        _ => nets.to_vec(),
    }
}

/// The sweep verdict, from the per-rung net PnLs: first rung and top rung both
/// positive → insensitive to latency; first rung positive but top rung negative
/// → latency-bound edge; first rung negative → the strategy has no edge at
/// all, eliminate it. Callers pass [`verdict_basis_nets`], so the middle rungs
/// never decide the verdict.
pub fn latency_verdict(nets: &[Decimal]) -> &'static str {
    let Some(first) = nets.first() else {
        return "no rungs ran";
    };
    let Some(last) = nets.last() else {
        return "no rungs ran";
    };
    if nets.iter().all(|n| *n > Decimal::ZERO) {
        "首尾档均为正 → 延迟不敏感（稳）"
    } else if *first > Decimal::ZERO && *last < Decimal::ZERO {
        "first rung positive, top rung negative → 延迟敏感型"
    } else if *first <= Decimal::ZERO {
        "first rung negative → 策略本身无正期望，淘汰"
    } else {
        "mixed → 见表"
    }
}

/// An in-memory [`DataSource`]: used by tests and by callers that already hold a
/// parsed stream (e.g. a trimmed slice of an archive).
pub struct VecSource {
    events: std::vec::IntoIter<TimedEvent>,
    label: String,
    total: u64,
    /// #354 (5.2): Spot ticks in the stream, counted up front — the same
    /// completeness anchor a replay source reports, so an equivalence
    /// comparison of `SourceStats` between the live feed and its archive
    /// replay compares like with like.
    spots: u64,
}

impl VecSource {
    pub fn new(events: Vec<TimedEvent>) -> Self {
        let total = events.len() as u64;
        let spots = events
            .iter()
            .filter(|te| matches!(te.event, DataEvent::Spot { .. }))
            .count() as u64;
        Self {
            events: events.into_iter(),
            label: format!("memory ({total} events)"),
            total,
            spots,
        }
    }
}

impl DataSource for VecSource {
    fn next_event(&mut self) -> Option<TimedEvent> {
        self.events.next()
    }
    fn describe(&self) -> String {
        self.label.clone()
    }
    fn stats(&self) -> SourceStats {
        SourceStats {
            events: self.total,
            spot_events: self.spots,
            ..Default::default()
        }
    }
}

/// One realised trade as reported (mirrors the `PositionClosed` event).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeLine {
    pub id: String,
    pub asset: String,
    pub direction: String,
    pub reason: String,
    /// The position's identity, mirroring `PositionClosed`: `strategy`
    /// attributes the trade, and `conditionId` groups the legs of ONE trade
    /// that closed as several positions. A complete-set pair is exactly that —
    /// it settles as two (winner $1 / loser $0), and neither leg's own P&L says
    /// anything on its own: the trade's statistics exist only as the group.
    pub strategy: String,
    pub token_id: String,
    pub condition_id: String,
    /// Entry price of the position — the #353 price-band distribution groups
    /// trades by it.
    #[serde(with = "crate::decimal")]
    pub entry_price: Decimal,
    #[serde(with = "crate::decimal")]
    pub net_pnl_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub net_pnl_pct: Decimal,
    /// #862: the token's MarketRegime state (`range`/`trendUp`/`trendDown`/
    /// `volatile`, the strategy_logic reference definition fed the token's own
    /// mid path) at the close — the one regime label a reconciliation bucket
    /// may use, so a per-regime fill-price comparison cannot grow a second
    /// definition. `"none"` when the token's book never produced a mid (no
    /// regime samples reached the state machine).
    pub regime: String,
}

/// Aggregate trade statistics over the realized equity curve.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeStats {
    pub closed: u64,
    pub wins: u64,
    pub losses: u64,
    /// Trades that closed exactly flat (neither win nor loss).
    pub flat: u64,
    #[serde(with = "crate::decimal")]
    pub gross_profit_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub gross_loss_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub net_pnl_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub avg_pnl_usd: Decimal,
    /// Win rate as a percent (0–100): `wins / (wins + losses) × 100`, flat
    /// trades excluded from the denominator. The formula lives ONLY in
    /// [`crate::metrics::Tally`] (issue #181) — before that, this divided by
    /// `closed` (flat included), so the same trade set reported a lower win
    /// rate here than `trades.summary` did. `0` when nothing was decided, the
    /// field's historical "no data" spelling.
    #[serde(with = "crate::decimal")]
    pub win_rate_pct: Decimal,
    /// gross profit / |gross loss|; `null` when there were no losing trades.
    #[serde(with = "crate::decimal::opt", skip_serializing_if = "Option::is_none")]
    pub profit_factor: Option<Decimal>,
    /// Realized peak-to-trough decline (open positions are not marked here).
    #[serde(with = "crate::decimal")]
    pub max_drawdown_usd: Decimal,
    /// Max drawdown as a percentage of the equity peak that preceded it.
    #[serde(with = "crate::decimal")]
    pub max_drawdown_pct: Decimal,
    /// Fees summed from the per-strategy accounting.
    #[serde(with = "crate::decimal")]
    pub fees_usd: Decimal,
}

impl TradeStats {
    /// Fold a replay's trade lines into the report's aggregate stats.
    ///
    /// Split out of `EventBacktester::trade_stats` (issue #181) so the win-rate
    /// SSOT test drives the SAME math the replay does. Every win/loss/flat
    /// classification goes through [`crate::metrics::Outcome`] and the
    /// `winRatePct` through [`crate::metrics::Tally::win_rate_pct_of`] —
    /// this function owns no formula of its own.
    pub fn of_trades(trades: &[TradeLine], fees_usd: Decimal) -> Self {
        let mut wins = 0u64;
        let mut losses = 0u64;
        let mut flat = 0u64;
        let mut gross_profit = Decimal::ZERO;
        let mut gross_loss = Decimal::ZERO;
        let mut net = Decimal::ZERO;
        for t in trades {
            net += t.net_pnl_usd;
            match crate::metrics::Outcome::of_net(t.net_pnl_usd) {
                crate::metrics::Outcome::Win => {
                    wins += 1;
                    gross_profit += t.net_pnl_usd;
                }
                crate::metrics::Outcome::Loss => {
                    losses += 1;
                    gross_loss += t.net_pnl_usd.abs();
                }
                crate::metrics::Outcome::Flat => flat += 1,
            }
        }
        let closed = trades.len() as u64;
        // Realized equity curve: start from zero PnL, walk the trades in close
        // order, track peak-to-trough. Percentage is of the peak that preceded
        // the trough (a drawdown cannot exceed the equity that made it).
        let mut equity = Decimal::ZERO;
        let mut peak = Decimal::ZERO;
        let mut max_dd = Decimal::ZERO;
        let mut max_dd_pct = Decimal::ZERO;
        for t in trades {
            equity += t.net_pnl_usd;
            if equity > peak {
                peak = equity;
            }
            let dd = peak - equity;
            if dd > max_dd {
                max_dd = dd;
                // Peak is > 0 whenever a drawdown exists (a fall from a positive
                // peak), so the division is safe; guard anyway.
                if peak > Decimal::ZERO {
                    max_dd_pct = (dd / peak) * Decimal::ONE_HUNDRED;
                }
            }
        }
        Self {
            closed,
            wins,
            losses,
            flat,
            gross_profit_usd: gross_profit,
            gross_loss_usd: gross_loss,
            net_pnl_usd: net,
            avg_pnl_usd: if closed == 0 {
                Decimal::ZERO
            } else {
                net / Decimal::from(closed)
            },
            win_rate_pct: crate::metrics::Tally::win_rate_pct_of(wins, losses)
                .unwrap_or(Decimal::ZERO),
            profit_factor: if gross_loss > Decimal::ZERO {
                Some(gross_profit / gross_loss)
            } else {
                None
            },
            max_drawdown_usd: max_dd,
            max_drawdown_pct: max_dd_pct,
            fees_usd,
        }
    }
}

/// The taker-fee schedule a replay charged, as reported (#203).
///
/// Part of the report rather than a CLI transcript detail: a fee-sensitive
/// conclusion ("this strategy is still profitable") is only interpretable
/// against the schedule it was measured under, and the fee is exactly the input
/// that moved the strategy's sign. `source` travels with it so a reader of the
/// JSON — a reviewer, a later gate — can see whether the parameters are
/// published or inherited from this repository's history.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeScheduleReport {
    pub name: String,
    #[serde(with = "crate::decimal")]
    pub rate: Decimal,
    pub exponent: u32,
    pub source: String,
}

impl From<crate::exit_policy::FeeSchedule> for FeeScheduleReport {
    fn from(s: crate::exit_policy::FeeSchedule) -> Self {
        Self {
            name: s.name.to_string(),
            rate: s.rate,
            exponent: s.exponent,
            source: s.source.to_string(),
        }
    }
}

/// #388: the wallet-level venue cash a replay credited from the stream
/// (activity-ledger MAKER_REBATE / REWARD rows), split by kind. Part of the
/// report rather than the ledger alone: a profitability conclusion is only
/// interpretable against the cash the wallet earned OUTSIDE its fills, and
/// the split matters — a rebate scales with maker volume, a reward with the
/// venue's program. Zero on a corpus without cash rows (the pre-#388 shape).
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CashflowReport {
    /// MAKER_REBATE rows credited (count and exact USD total).
    pub rebates: u64,
    #[serde(with = "crate::decimal")]
    pub rebates_usd: Decimal,
    /// REWARD rows credited (count and exact USD total).
    pub rewards: u64,
    #[serde(with = "crate::decimal")]
    pub rewards_usd: Decimal,
}

/// Order counts by the last status each order reached.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderCounts {
    pub orders: u64,
    pub filled: u64,
    pub cancelled: u64,
    pub rejected: u64,
    pub failed: u64,
    pub live_at_end: u64,
}

/// How much of what we asked for actually traded (issue #183).
///
/// A dry run used to fill every crossing maker whole, so these numbers could
/// only ever read 100% and a backtest could not tell a book deep enough for the
/// strategy from one that would have left the entry half-open. They are now a
/// property of the run rather than an assumption.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FillStats {
    /// Orders observed at least once, in any status.
    pub orders: u64,
    /// Orders whose last observed state was fully filled.
    pub filled_orders: u64,
    /// Orders that traded but never reached their size — the partial fills.
    pub partial_orders: u64,
    /// Orders that never traded a single share (cancelled, rejected, expired…).
    pub unfilled_orders: u64,
    /// Share of orders that traded at all. `0` when no order was placed.
    #[serde(with = "crate::decimal")]
    pub fill_rate_pct: Decimal,
    /// Share of orders left PARTIALLY filled — the dry run's exposure to the
    /// live-bug-② shape (#183).
    #[serde(with = "crate::decimal")]
    pub partial_rate_pct: Decimal,
    /// Filled size over requested size across every order: the volume view.
    #[serde(with = "crate::decimal")]
    pub size_fill_ratio_pct: Decimal,
}

/// The replay result. Serializable so a run can be filed next to the archive.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BacktestReport {
    pub source: String,
    pub source_stats: SourceStats,
    pub tick_ms: i64,
    pub tail_ms: i64,
    pub start_at_ms: i64,
    pub end_at_ms: i64,
    pub virtual_ms: i64,
    pub fill_model: FillModel,
    /// The requesting `--backtest-mode` (#879). Text-render only: the JSON
    /// wire stays byte-identical (the coordinate is already derivable from
    /// `fillModel.takerSlippageTicks` + the fee schedule's name).
    #[serde(skip)]
    pub mode: String,
    /// The taker-fee schedule this replay charged (#203). See [`FeeScheduleReport`].
    pub fee_schedule: FeeScheduleReport,
    /// #388: wallet-level venue cash credited from the stream (MAKER_REBATE /
    /// REWARD ledger rows). See [`CashflowReport`].
    pub cashflows: CashflowReport,
    pub entry_maker_timeout_ms: i64,
    pub orders: OrderCounts,
    /// Fill rate / partial-fill rate (#183): what actually traded, order by
    /// order, so a dry acceptance run reports its own optimism instead of
    /// hiding it behind a 100% assumption.
    pub fill_stats: FillStats,
    pub fills: u64,
    pub trades: TradeStats,
    /// Per-strategy accounting (`engine.stats.strategies[]`), verbatim.
    pub strategies: Vec<Value>,
    /// Feed counters (`engine.stats`): books/tops/spots/rounds/evaluations/…
    pub feed: Value,
    pub blocked: Value,
    pub open_positions: usize,
    #[serde(with = "crate::decimal")]
    pub open_notional_usd: Decimal,
    pub risk_alerts: Vec<String>,
    pub errors: Vec<String>,
    pub trade_lines: Vec<TradeLine>,
    /// #355: ALWAYS 0 — the serialized `trade_lines` list is complete (a
    /// per-condition aggregator like the @almach overlay must never read a
    /// clipped prefix). Kept for report-schema compatibility; the Display
    /// summary still caps its own printout at [`MAX_TRADE_LINES`].
    pub trade_lines_truncated: u64,
    /// #351 reproducibility digest: SHA256 over the closed-trade list in close
    /// order (each line's canonical JSON, concatenated). Two replays of the
    /// same archive under the same config MUST produce the same digest — a
    /// differing one is a determinism bug, not a statistic.
    pub trades_sha256: String,
    /// Modeled maker rebate the replay credited on MAKER fills
    /// (`--maker-rebate`; almach REPORT §5 失真点 5). Distinct from
    /// `cashflows.rebates_usd`, which counts the corpus's REAL MAKER_REBATE
    /// ledger rows (#388) — a replay that models the pool on a corpus also
    /// carrying real rows would show both, and a reader comparing maker-vs-
    /// taker runs needs to see which cash was claimed which way.
    #[serde(with = "crate::decimal")]
    pub maker_rebate_usd: Decimal,
    /// True when the replay had to force `Mode::Dry` (it always does).
    pub forced_dry: bool,
}

impl BacktestReport {
    /// Human-readable rendering for the CLI.
    pub fn render(&self) -> String {
        let mut s = String::new();
        // #879: a report names the coordinate system it was mined/verified in
        // (slippage overlay + fee tier), so two numbers are never compared
        // across different frictions. The fee prints the schedule's own name —
        // `legacy_quadratic` on the default, the shipped coordinate the ruling
        // calls "legacy 费"; `official` when the operator ran --fee-model
        // official. Empty mode = the historic single-run path, unchanged.
        if !self.mode.is_empty() {
            s.push_str(&format!(
                "坐标系：{}({} tick, {} 费)\n",
                self.mode, self.fill_model.taker_slippage_ticks, self.fee_schedule.name
            ));
        }
        s.push_str(&format!("backtest: {}\n", self.source));
        s.push_str(&format!(
            "  window           : {} → {} ({} ms virtual)\n",
            self.start_at_ms, self.end_at_ms, self.virtual_ms
        ));
        s.push_str(&format!(
            "  events           : {} ({} malformed skipped, {} out-of-order, {} spot)\n",
            self.source_stats.events,
            self.source_stats.malformed_lines,
            self.source_stats.out_of_order_events,
            self.source_stats.spot_events
        ));
        s.push_str(&format!(
            "  fill model       : slippage {} tick(s), maker latency {} ms, maker fill {} bps, maker rule {}, taker latency {}+{} ms\n",
            self.fill_model.taker_slippage_ticks,
            self.fill_model.maker_latency_ms,
            self.fill_model.maker_fill_prob_bps,
            self.fill_model.maker_fill_rule.as_str(),
            self.fill_model.taker_latency_ms,
            self.fill_model.taker_rtt_ms
        ));
        s.push_str(&format!(
            "  entry escalate   : {} ms\n",
            self.entry_maker_timeout_ms
        ));
        s.push_str(&format!(
            "  taker fee        : {} rate={} exponent={} (#203)\n",
            self.fee_schedule.name, self.fee_schedule.rate, self.fee_schedule.exponent
        ));
        // #388: show the wallet-level cash the venue paid. Always named, so a
        // reader can tell "no cash rows in the corpus" from "cash not shown".
        s.push_str(&format!(
            "  cashflows        : rebates ${:.2} ({}), rewards ${:.2} ({}) (#388)\n",
            self.cashflows.rebates_usd,
            self.cashflows.rebates,
            self.cashflows.rewards_usd,
            self.cashflows.rewards
        ));
        s.push_str(&format!(
            "  maker rebate     : ${:.2} modeled (--maker-rebate; §5-5)\n",
            self.maker_rebate_usd
        ));
        s.push_str(&format!(
            "  orders           : {} ({} filled, {} cancelled, {} rejected, {} failed, {} live at end)\n",
            self.orders.orders,
            self.orders.filled,
            self.orders.cancelled,
            self.orders.rejected,
            self.orders.failed,
            self.orders.live_at_end
        ));
        s.push_str(&format!("  fills            : {}\n", self.fills));
        let f = &self.fill_stats;
        s.push_str(&format!(
            "  fill rate        : {:.2}% of orders ({} filled, {} partial, {} never) — {:.2}% of size\n",
            f.fill_rate_pct, f.filled_orders, f.partial_orders, f.unfilled_orders, f.size_fill_ratio_pct
        ));
        s.push_str(&format!(
            "  partial fills    : {:.2}% of orders (#183)\n",
            f.partial_rate_pct
        ));
        let t = &self.trades;
        s.push_str(&format!(
            "  trades           : {} closed ({} win / {} loss / {} flat, win {:.0}%)\n",
            t.closed, t.wins, t.losses, t.flat, t.win_rate_pct
        ));
        s.push_str(&format!(
            "  gross            : +{:.2} / {:.2} (fees {:.2})\n",
            t.gross_profit_usd, t.gross_loss_usd, t.fees_usd
        ));
        match t.profit_factor {
            Some(pf) => s.push_str(&format!("  profit factor    : {:.2}\n", pf)),
            None => s.push_str("  profit factor    : n/a (no losing trades)\n"),
        }
        s.push_str(&format!("  net PnL          : {:.2}\n", t.net_pnl_usd));
        s.push_str(&format!(
            "  max drawdown     : {:.2} ({:.2}% of peak)\n",
            t.max_drawdown_usd, t.max_drawdown_pct
        ));
        s.push_str(&format!(
            "  open at end      : {} position(s), ${:.2} notional\n",
            self.open_positions, self.open_notional_usd
        ));
        for strat in &self.strategies {
            let name = strat.get("name").and_then(Value::as_str).unwrap_or("?");
            let pnl = strat
                .get("netPnlUsd")
                .map(render_num)
                .unwrap_or_else(|| "?".into());
            let n = strat
                .get("closedTrades")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let open = strat
                .get("openPositions")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            s.push_str(&format!(
                "  strategy {name:<12}: net {pnl}, {n} closed, {open} open\n"
            ));
        }
        if !self.risk_alerts.is_empty() {
            s.push_str(&format!(
                "  risk alerts      : {}\n",
                self.risk_alerts.len()
            ));
            for a in self.risk_alerts.iter().take(5) {
                s.push_str(&format!("    - {a}\n"));
            }
        }
        if !self.errors.is_empty() {
            s.push_str(&format!("  errors           : {}\n", self.errors.len()));
            for e in self.errors.iter().take(5) {
                s.push_str(&format!("    - {e}\n"));
            }
        }
        if !self.trade_lines.is_empty() {
            s.push_str("  trades:\n");
            // The serialized `trade_lines` list is complete (#355); the human
            // summary still caps its printout at MAX_TRADE_LINES.
            let shown = self.trade_lines.len().min(MAX_TRADE_LINES);
            for line in &self.trade_lines[..shown] {
                s.push_str(&format!(
                    "    {:<14} {:<4} {:<18} {:>8.2} ({:.2}%)\n",
                    line.asset, line.direction, line.reason, line.net_pnl_usd, line.net_pnl_pct
                ));
            }
            let more = self.trade_lines.len() - shown;
            if more > 0 {
                s.push_str(&format!("    … {more} more\n"));
            }
        }
        s
    }
}

fn render_num(v: &Value) -> String {
    match v {
        Value::Number(n) => format!("{n}"),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A Decimal carried either as a JSON number (the `dec_json` convention used by
/// the strategy accounting) or as a string (rust_decimal's default serde).
pub fn json_decimal(v: Option<&Value>) -> Option<Decimal> {
    match v? {
        Value::Number(n) => Decimal::from_str_exact(&n.to_string()).ok(),
        Value::String(s) => Decimal::from_str_exact(s.trim()).ok(),
        _ => None,
    }
}

/// Trades shown by the report's Display summary. The serialized `trade_lines`
/// list is complete since #355 (counts always cover all trades) — a per-
/// condition aggregator must never read a clipped prefix.
pub const MAX_TRADE_LINES: usize = 2_000;
/// Risk alerts / errors kept in the report.
pub const MAX_MESSAGES: usize = 200;
