//! The replay driver: [`EventBacktester`] — a real [`Core`] on a virtual clock,
//! plus the per-event maintenance grid (issue #390 deadline jump).
//! Pure code motion from `backtest.rs` (no logic edits).

use crate::data_source::DataSource;
use crate::engine::DataEvent;
use crate::schema::Event;
use crate::service::Core;
use rust_decimal::Decimal;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use strategy_logic::market_regime::MarketRegime;
use tokio::sync::mpsc;

use super::types::{
    BacktestConfig, BacktestReport, Backtester, CashflowReport, FillStats, MAX_MESSAGES,
    OrderCounts, TradeLine, TradeStats, json_decimal,
};

/// Event-driven replay driver: a real [`Core`] on a virtual clock.
pub struct EventBacktester {
    source: Box<dyn DataSource>,
    pub(crate) core: Core,
    rx: mpsc::UnboundedReceiver<Event>,
    tick_ms: i64,
    tail_ms: i64,
    /// The requesting `--backtest-mode` (empty on the historic single-run
    /// path) — flows into the report's coordinate-system line (#879).
    mode: String,
    /// Orders by their last observed status.
    order_status: HashMap<String, String>,
    /// Orders by the last observed `(size, filled_size)` (#183).
    order_sizes: HashMap<String, (Decimal, Decimal)>,
    fills: u64,
    trades: Vec<TradeLine>,
    /// #862: one online MarketRegime state machine per token, fed the token's
    /// own mid path as the replay delivers it — the strategy_logic reference
    /// definition (300 s window, hysteresis), so a trade's regime label is the
    /// canonical classifier's verdict, never a re-implementation.
    pub(crate) regimes: HashMap<String, MarketRegime>,
    risk_alerts: Vec<String>,
    errors: Vec<String>,
    /// Re-check the invariants the equivalence gate relies on.
    checked_events: u64,
    /// A refused engine install (#265): why this replay must not produce a
    /// report. Kept rather than panicked on in `new` (which cannot fail) so the
    /// caller gets `run`'s ordinary `Err` — and a sweep can tell "this candidate
    /// could not load its strategy" from "the strategy had no signal".
    install_error: Option<String>,
    /// Progress consumer handed in via [`BacktestConfig::progress`] (None =
    /// CLI stderr heartbeat). Stashed on the struct because `run` takes `&mut
    /// self` and the callback needs to outlive the config.
    cfg_progress: Option<std::sync::Arc<dyn Fn(u64, i64) + Send + Sync>>,
    /// #916: counterfactual knobs held back from the constructor — the hosted
    /// strategies register AFTER `EventBacktester::new` (the host calls
    /// `host_strategy` between construction and `run`), and the dispatch
    /// needs their loaded declarations (string tunables vs decimal) to exist.
    /// Applied at the top of `run`, before the clock starts: a refusal there
    /// is the ordinary startup `Err`, not a half-applied replay.
    pending_hot_params: Vec<(String, String, String)>,
}

impl EventBacktester {
    /// Build a replay around `source`.
    ///
    /// The core config is forced to the replay-safe subset: `Mode::Dry`,
    /// no persistence (trade/order/position logs, near-miss), no live feeds or
    /// discovery, no shadow evolution. Strategy knobs (assets, sizing, gates,
    /// exit policy, per-strategy limits, fill model) are taken as configured.
    /// `event_archive_path` is left alone: recording during a replay is an
    /// explicit opt-in and is useful for trimming a long archive.
    pub fn new(mut cfg: BacktestConfig, source: Box<dyn DataSource>) -> Self {
        let mut core_cfg = std::mem::take(&mut cfg.core);
        core_cfg.mode = crate::model::Mode::Dry;
        core_cfg.trade_log_path = None;
        core_cfg.order_log_path = None;
        core_cfg.position_log_path = None;
        // The durable cash journals are a live-deployment persistence; a
        // replay neither appends to nor replays them (the corpus is the only
        // input), and the boot seed must run exactly as it always did.
        core_cfg.ledger_journal_dir = None;
        // The account book's config source is likewise live-deployment
        // persistence: a replay must never write to the operator's account
        // store (venue binds, account.create/account.remove all refuse when
        // this is None — the same posture the backtester's other None-ing of
        // durable writers takes).
        core_cfg.accounts_config_path = None;
        core_cfg.near_miss_path = None;
        core_cfg.discovery_enabled = false;
        core_cfg.feed_ws_enabled = false;
        core_cfg.market_plugin = None;
        core_cfg.shadow_evolution_enabled = false;
        core_cfg.shadow_evolution_tuning = None;
        // #234: this core may run under a counterfactual taker-fee schedule
        // (`--fee-model`), which the live charge sites refuse for any core that
        // has not declared itself a replay. The declaration belongs here — the
        // only constructor that may charge one.
        core_cfg.fee_schedule_replay = true;
        // Issue #390: a replay runs the deadline-driven maintenance scheduling
        // on BOTH paths — `backtest_fast` implies it, and the nofast replay
        // opts in here (the live kernel never does: a replay is the one
        // deployment whose maintenance cadence is a per-invocation choice).
        core_cfg.replay_scheduling = true;

        let mut core = Core::new(core_cfg.clone());
        // #265: a replay that asked for strategies and resolved none must fail
        // rather than report zero trades — the whole point of a sweep is that a
        // number means what it says.
        let install_error = if core_cfg.engine_enabled {
            core_cfg.install_engine(&mut core).err()
        } else {
            None
        };
        // Issue #390: the exit ladder's deadline schedule backfills its counter
        // ratchets over the grid the replay actually drives — tell the core the
        // step it will be stepped with (the mutator owns the ≥1ms floor).
        core.set_tick_schedule_ms(cfg.tick_ms);
        // Counterfactual knobs are applied at the top of `run()` (#916): the
        // dispatch needs each hosted strategy's loaded declaration (string
        // tunables vs decimal), and the host registers its strategies BETWEEN
        // construction and run — applying here would answer "no loaded
        // strategy" for every override and refuse healthy counterfactuals.

        let (tx, rx) = mpsc::unbounded_channel::<Event>();
        core.set_event_sink(tx);
        let tick_ms = cfg.tick_ms.max(1);
        Self {
            source,
            core,
            rx,
            tick_ms,
            tail_ms: cfg.tail_ms.max(0),
            mode: std::mem::take(&mut cfg.mode),
            order_status: HashMap::new(),
            order_sizes: HashMap::new(),
            fills: 0,
            trades: Vec::new(),
            regimes: HashMap::new(),
            risk_alerts: Vec::new(),
            errors: Vec::new(),
            checked_events: 0,
            install_error,
            cfg_progress: cfg.progress.take(),
            pending_hot_params: std::mem::take(&mut cfg.hot_params),
        }
    }

    /// Read-only access to the replayed core (tests, post-run inspection).
    pub fn core(&self) -> &Core {
        &self.core
    }

    /// Mutable access to the replayed core (host a strategy, toggle one, seed
    /// state) before `run`.
    pub fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    /// Host an additional strategy into the replay's engine.
    ///
    /// The kernel ships no strategies (PR-B): `install_engine` loads whatever
    /// the configured `strategy_dir` holds and registers nothing else, so a
    /// programmatic replay that wants a specific strategy hands one over here —
    /// the same `EngineStrategy` contract a loaded library implements. Call it
    /// after construction and before `run`; like every registration the strategy
    /// starts DISABLED, so enable it by name on the core's engine.
    pub fn host_strategy(&mut self, strategy: Box<dyn crate::strategies::EngineStrategy>) {
        let _ = self.core.host_replay_strategy(strategy, "replay-hosted");
    }

    /// Advance the virtual clock one maintenance step: the live loop's
    /// `tick` + `engine_evaluate`, in that order.
    fn step(&mut self, now_ms: i64) {
        if let Err(e) = self.core.tick(now_ms) {
            self.push_error(format!("tick@{now_ms}: {e:?}"));
        }
        self.core.engine_evaluate(now_ms);
        self.drain();
    }

    /// Issue #390 (replay deadline jump): run the maintenance grid from
    /// `next_eval_ms` up to `until_ms` (EXCLUSIVE — an event instant lands
    /// here as `at + 1`, matching the base walk's `<= at`), stepping only
    /// the grid points that can produce output instead of every one.
    ///
    /// A quiet grid tick is, after #390's deadline scheduling, a proven
    /// no-op: the ladder runs on its dirty/deadline gates, the engine
    /// evaluation on its dirty/timing/fallback gates, settlement on the
    /// position/expiry clocks, escalations on the order-set dirty flag and
    /// the armed deadlines. Every one of those re-arms through
    /// `next_replay_wake_ms`, which the loop re-reads after every step, so
    /// it never needs to guess when a deadline moves. The only per-cycle
    /// state a skipped tick would advance is the `evaluations` feed counter
    /// (the sparse-cadence contract counts scheduled cycles, not real Lua
    /// calls), credited per skipped span via `replay_credit_cycles`.
    ///
    /// Landing points, so every output stays byte-identical:
    ///   * every grid tick at or after a WAKE deadline — the deadline's
    ///     owner re-arms inside that tick;
    ///   * the FIRST grid tick after each event batch — the caller stops
    ///     the sweep before the event instant, and the event's own tick
    ///     runs normally (a delivery can wake any dirty gate);
    ///   * the grid tick IMMEDIATELY after a step that left a dirty flag
    ///     or an unconsumed strategy exit intent (`has_forced_next_tick`) —
    ///     its consuming pass must run on the same tick the every-tick
    ///     walk used, stamps included.
    ///
    /// The grid is ANCHORED at the first event's timestamp + k·tick, not at
    /// absolute multiples of `tick`, so wake instants are ceiled onto the
    /// anchored grid relative to the current cursor.
    fn jump_grid(&mut self, next_eval_ms: &mut i64, until_ms: i64) {
        if !self.core.replay_jump_allowed() {
            // Shadow Evolution (or a non-replay kernel): the every-tick walk
            // IS the contract — its per-cycle cadence is observable.
            while *next_eval_ms < until_ms {
                self.step(*next_eval_ms);
                *next_eval_ms += self.tick_ms;
            }
            return;
        }
        let tick = self.tick_ms;
        // The FIRST landing of every call is the cursor itself: the first
        // grid point after the previous event batch — the tick the every-
        // tick walk used to observe that batch's dirty wakes. After each
        // step, the flag carries the core's own forced-next state (a step
        // that left an unconsumed intent or a dirty flag pins the next
        // landing to the immediate next grid point).
        let mut force_next = true;
        loop {
            let cursor = *next_eval_ms;
            if !(cursor < until_ms) {
                break;
            }
            let landing = if force_next {
                cursor
            } else {
                match self.core.next_replay_wake_ms(cursor - 1) {
                    // A wake due at/after `until_ms` skips every remaining
                    // grid point of this span (they are quiet no-ops — the
                    // event batch has not been delivered yet). Credit their
                    // scheduled cycles and leave the cursor on the grid.
                    Some(t) => {
                        let d = t - cursor;
                        let off = if d <= 0 { 0 } else { (d - 1) / tick + 1 };
                        let p = cursor + off * tick;
                        if p >= until_ms {
                            let s = until_ms - cursor;
                            let skipped = if s <= 0 { 0 } else { (s - 1) / tick + 1 };
                            self.core.replay_credit_cycles(skipped as u64);
                            *next_eval_ms = cursor + skipped * tick;
                            return;
                        }
                        p
                    }
                    None => {
                        let s = until_ms - cursor;
                        let skipped = if s <= 0 { 0 } else { (s - 1) / tick + 1 };
                        self.core.replay_credit_cycles(skipped as u64);
                        *next_eval_ms = cursor + skipped * tick;
                        return;
                    }
                }
            };
            if landing > cursor {
                // Quiet grid points between the last run point and the
                // landing: one scheduled evaluation each.
                let skipped = ((landing - cursor) / tick) as u64;
                self.core.replay_credit_cycles(skipped);
                *next_eval_ms = landing;
            }
            self.step(*next_eval_ms);
            // Issue #390 (replay nofast): a standing candidate — a suggestion
            // the base walk would RE-SUBMIT into the kernel-side gates on
            // every tick of this gap (cooldowns/breakers time out mid-gap;
            // the retry that lands the entry is the observable one) — pins
            // the next landing to the immediate next grid point, exactly like
            // a forced tick. The quiet-cycle replay keeps the gate stream
            // cycle-for-cycle; the driver keeps its cadence.
            force_next = self.core.has_forced_next_tick() || self.core.has_standing_candidates();
            *next_eval_ms += tick;
        }
    }

    fn drain(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::PositionClosed {
                    id,
                    asset,
                    direction,
                    reason,
                    strategy,
                    token_id,
                    condition_id,
                    entry_price,
                    net_pnl_usd,
                    net_pnl_pct,
                    ..
                } => {
                    let regime = self.regime_of(&token_id);
                    self.trades.push(TradeLine {
                        id,
                        asset,
                        direction,
                        reason,
                        strategy,
                        token_id,
                        condition_id,
                        entry_price,
                        net_pnl_usd,
                        net_pnl_pct,
                        regime,
                    });
                }
                Event::OrderUpdate { order } => {
                    self.order_status
                        .insert(order.order_id.clone(), format!("{:?}", order.status));
                    // Latest (size, filled) per order: the fill-rate view (#183)
                    // needs what was ASKED for against what TRADED, which the
                    // status alone cannot say.
                    self.order_sizes
                        .insert(order.order_id.clone(), (order.size, order.filled_size));
                }
                Event::Fill { .. } => self.fills += 1,
                Event::RiskAlert { code, message } => {
                    self.push_risk(format!("{code:?}: {message}"));
                }
                Event::Error { error } => self.push_error(format!("{error:?}")),
                _ => {}
            }
        }
    }

    fn push_risk(&mut self, msg: String) {
        if self.risk_alerts.len() < MAX_MESSAGES {
            self.risk_alerts.push(msg);
        }
    }

    fn push_error(&mut self, msg: String) {
        if self.errors.len() < MAX_MESSAGES {
            self.errors.push(msg);
        }
    }

    fn order_counts(&self) -> OrderCounts {
        let mut c = OrderCounts {
            orders: self.order_status.len() as u64,
            ..Default::default()
        };
        for status in self.order_status.values() {
            match status.as_str() {
                "Filled" => c.filled += 1,
                "Cancelled" => c.cancelled += 1,
                "Rejected" => c.rejected += 1,
                "Failed" => c.failed += 1,
                "Pending" | "Live" | "PartiallyFilled" => c.live_at_end += 1,
                _ => {}
            }
        }
        c
    }

    /// Fill rate / partial-fill rate over every order the run saw (#183).
    fn fill_stats(&self) -> FillStats {
        let mut s = FillStats {
            orders: self.order_sizes.len() as u64,
            ..Default::default()
        };
        let mut requested = Decimal::ZERO;
        let mut filled = Decimal::ZERO;
        for (size, got) in self.order_sizes.values() {
            requested += *size;
            filled += *got;
            if *got <= Decimal::ZERO {
                s.unfilled_orders += 1;
            } else if *got >= *size {
                s.filled_orders += 1;
            } else {
                s.partial_orders += 1;
            }
        }
        let pct = |n: Decimal, d: Decimal| {
            if d <= Decimal::ZERO {
                Decimal::ZERO
            } else {
                n * Decimal::ONE_HUNDRED / d
            }
        };
        let total = Decimal::from(s.orders);
        s.fill_rate_pct = pct(Decimal::from(s.filled_orders + s.partial_orders), total);
        s.partial_rate_pct = pct(Decimal::from(s.partial_orders), total);
        s.size_fill_ratio_pct = pct(filled, requested);
        s
    }

    /// The report's trade stats. The math lives on [`TradeStats::of_trades`] so
    /// the win-rate SSOT test (`tests/metrics_ssot.rs`) drives the exact same
    /// code the replay does, instead of a re-implementation of it.
    fn trade_stats(&self, fees_usd: Decimal) -> TradeStats {
        TradeStats::of_trades(&self.trades, fees_usd)
    }
}

impl Backtester for EventBacktester {
    fn describe(&self) -> String {
        format!("{} → {:?}", self.source.describe(), self.core.mode())
    }

    fn run(&mut self) -> Result<BacktestReport, String> {
        // #265: the engine refused to install (an explicit strategy request that
        // resolved to nothing). Report it instead of replaying into a report that
        // would read as "the strategy produced no signals".
        if let Some(why) = &self.install_error {
            return Err(format!("strategy startup self-check: {why}"));
        }
        // #916: counterfactual knobs apply HERE — after the host's
        // between-construction-and-run strategy registrations, before the
        // clock starts. The value arrives verbatim and its type is decided
        // against the strategy's manifest: a tunable declaring
        // `"type": "string"` rides the strategy's own `bk.params()` bag (the
        // word it is, no decimal pipe), everything else parses as a Decimal —
        // and a non-numeric value on such a knob is a loudly refused
        // counterfactual (P0 #228/#441: a silently ignored override would
        // make the comparison meaningless).
        let pending = std::mem::take(&mut self.pending_hot_params);
        if !pending.is_empty() {
            self.apply_hot_params(pending)?;
        }
        // The event clock starts at the first event's timestamp — the instant
        // the live core saw its first datum — and only ever moves forward.
        let mut pending = self.source.next_event();
        let mut clock = pending.as_ref().map(|p| p.at_ms.max(0)).unwrap_or(0);
        let start_at_ms = clock;
        let mut end_at_ms = clock;
        let mut delivered: u64 = 0;
        // Maintenance runs on its own `tick_ms` schedule, exactly like the live
        // server's interval timer: a real feed delivers bursts of sub-millisecond
        // events, so an event-gap-driven cycle would evaluate far less often than
        // live and exits would fire late (see `dense_stream_keeps_live_cadence`).
        let mut next_eval_ms = clock + self.tick_ms;

        // #353: one stderr heartbeat per wall-second — the gateway's backtest
        // job parses these into the panel's progress bar. Tests never see one
        // (they finish inside the first second) and the pipe can never fill
        // at one line per second.
        let mut last_beat = std::time::Instant::now();

        while let Some(te) = pending.take() {
            let at = if te.at_ms > 0 { te.at_ms } else { clock };
            if at > clock {
                // Everything the live timer would have fired up to this
                // instant — on the subset of grid points that can produce
                // output (issue #390 deadline jump). `at + 1` keeps the
                // base walk's `<= at` boundary: the event instant itself
                // never steps here, and `next_replay_wake_ms`'s probe at
                // (cursor - 1) admits a wake landing exactly at `at`.
                self.jump_grid(&mut next_eval_ms, at + 1);
                clock = at;
            }
            // #354 (5.4): the replay grid is fail-closed against the corpus.
            // A market that DECLARES its round duration (converter-written,
            // non-zero) and disagrees with the configured grid would run every
            // scanner gate — age, time-left, slot expiry — on the wrong clock:
            // the exact silent mis-timing this issue exists to kill. Refuse
            // the replay instead. An undeclared market (0: old archives, and
            // the live host's own path) stays silent — it rides the configured
            // grid by definition.
            //
            // B1d (轮次多族) refinement: the refusal is per (venue, duration)
            // FAMILY. The live kernel times each family on its own grid (the
            // scanner keys its clocks (venue, duration)); a replay corpus is
            // a single grid, so a corpus carrying MORE THAN ONE declared
            // family (or one family that is not the configured grid) cannot
            // be replayed honestly — the error names the families and the
            // split instruction instead of the old single-value wording. A
            // corpus whose only declared family IS the configured grid
            // (the historical shape, and every existing baseline) passes
            // byte-identically, as does a corpus with no declared duration
            // at all.
            if let DataEvent::RoundMarkets { markets, .. } = &te.event {
                let grid = self.core.config().default_round_duration_sec;
                let mut foreign: Vec<(String, i64)> = Vec::new();
                for m in markets {
                    if m.round_duration_sec > 0
                        && m.round_duration_sec != grid
                        && !foreign
                            .iter()
                            .any(|(v, d)| v == &m.venue && d == &m.round_duration_sec)
                    {
                        foreign.push((m.venue.clone(), m.round_duration_sec));
                    }
                }
                if !foreign.is_empty() {
                    let listed = foreign
                        .iter()
                        .map(|(v, d)| format!("({v}, {d}s)"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(format!(
                        "replay cadence mismatch: the corpus declares round families {listed} \
                         but the replay grid is {grid}s (--round-sec). The replay engine runs \
                         ONE grid; the live kernel runs one clock per (venue, duration) family. \
                         Every timing gate would run on the wrong clock for the foreign \
                         families — replay each (venue, duration) family as its own run with \
                         the matching --round-sec (请按 (venue, duration) 分段跑)."
                    ));
                }
            }
            // Late/out-of-order events still get the current clock (never a
            // backwards jump) — the source counts them separately.
            self.observe_regime_mid(&te.event, at);
            self.core.engine_on_data(te.event, clock);
            self.checked_events += 1;
            delivered += 1;
            end_at_ms = clock;
            if last_beat.elapsed() >= std::time::Duration::from_secs(1) {
                match &self.cfg_progress {
                    Some(cb) => cb(delivered, clock),
                    None => eprintln!("backtest progress: {delivered} events, virtual {clock}ms"),
                }
                last_beat = std::time::Instant::now();
            }
            self.drain();
            pending = self.source.next_event();
        }

        // Tail: keep ticking so maker escalations and pending exits resolve
        // — on the jump-selected subset of the grid, like the main loop.
        // The final clock still reads `end`, exactly as the base walk left
        // it (its last step overshoots `end` or lands at `end`, then the
        // clock clamps to `end` for the report's diagnostics).
        if self.tail_ms > 0 {
            let end = clock + self.tail_ms;
            self.jump_grid(&mut next_eval_ms, end + 1);
            if end > clock {
                clock = end;
            }
            end_at_ms = clock;
        }

        // Pull the last events out (a final close may have been emitted).
        self.drain();

        // Diagnostics are taken as of the end of the *virtual* run: the host
        // clock would make every book look years stale in an offline replay.
        let feed = self.core.engine_stats_at(clock);
        let strategies = self.core.strategy_stats();
        // #388: the cash rows the core credited — a snapshot straight off the
        // credit path's counters, so the report and the ledger can never
        // disagree about what was paid.
        let (cf_rebates, cf_rebates_usd, cf_rewards, cf_rewards_usd) = self.core.cashflow_stats();
        let blocked = feed.get("blocked").cloned().unwrap_or(Value::Null);
        let fees_usd: Decimal = strategies
            .iter()
            .filter_map(|s| json_decimal(s.get("feesUsd")))
            .sum();
        let views = self.core.position_views(clock);
        let open_notional: Decimal = views.iter().map(|p| p.entry_price * p.shares).sum();
        let stats = self.source.stats();
        // #355: the serialized report carries EVERY closed trade — a per-
        // condition aggregator (the @almach overlay) must never compute from
        // a silently clipped prefix (a full-window 5m replay closes ~24k
        // legs; the old 2k cap kept only the earliest). The Display summary
        // still caps its own printout at MAX_TRADE_LINES.
        let trade_lines = self.trades.clone();
        let trades = self.trade_stats(fees_usd);
        // #351 reproducibility digest: the canonical JSON of every closed trade
        // in close order, concatenated and SHA256'd. Two replays of the same
        // archive under the same config MUST agree byte-for-byte — a differing
        // digest is a determinism bug, not a statistic. Hashed from the FULL
        // trade list, which `tradeLines` now also carries complete.
        let mut hasher = Sha256::new();
        for t in &self.trades {
            let line = serde_json::to_string(t).unwrap_or_default();
            hasher.update(line.as_bytes());
            hasher.update(b"\n");
        }
        let mut trades_sha256 = String::new();
        for b in hasher.finalize() {
            let _ = write!(trades_sha256, "{b:02x}");
        }
        // 端到端保真度只在本会话内成立：同一次回放里，来源投递的事件数
        // 与引擎统计的 events 必须逐位相等。这是一条 release 生效的完整性断言。
        // 「采集→重放逐位相等」的跨会话端到端断言（已归档 live 快照 vs 回放）
        // 目前没有替代品（DEV §5.2）：回放器是纯会话内内存重放（Mode::Dry，
        // 不持久化、不发现、不行情、不影子进化），不对真实 live 快照做
        // 跨会话对账，因此本断言不得被读作跨 live 快照校验。
        assert_eq!(
            delivered, stats.events,
            "source delivered a different event count"
        );

        Ok(BacktestReport {
            source: self.source.describe(),
            source_stats: stats,
            tick_ms: self.tick_ms,
            tail_ms: self.tail_ms,
            start_at_ms,
            end_at_ms,
            virtual_ms: clock - start_at_ms,
            fill_model: self.core.config().fill_model,
            mode: self.mode.clone(),
            fee_schedule: crate::exit_policy::fee_schedule().into(),
            // #388: the cash rows the core credited — read straight off the
            // stats the credit path wrote, so the report and the ledger can
            // never disagree about what was paid.
            cashflows: CashflowReport {
                rebates: cf_rebates,
                rebates_usd: cf_rebates_usd,
                rewards: cf_rewards,
                rewards_usd: cf_rewards_usd,
            },
            entry_maker_timeout_ms: self.core.config().entry_maker_timeout_ms,
            orders: self.order_counts(),
            fill_stats: self.fill_stats(),
            fills: self.fills,
            trades,
            strategies,
            feed,
            blocked,
            open_positions: views.len(),
            open_notional_usd: open_notional,
            risk_alerts: std::mem::take(&mut self.risk_alerts),
            errors: std::mem::take(&mut self.errors),
            trade_lines,
            trade_lines_truncated: 0,
            trades_sha256,
            // The fill-level pool-share approximation the replay credited
            // (`--maker-rebate`); zero when off (the pre-rebate shape).
            maker_rebate_usd: self.core.modeled_maker_rebate_usd(),
            forced_dry: true,
        })
    }
}
