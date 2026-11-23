//! Event-driven live evaluation — decision equivalence with the every-tick
//! path. Two dry kernels host the REAL late_favorite package with its
//! manifest declarations and are driven by the identical feed and the
//! production 10 ms maintenance cadence (`tick` then `engine_evaluate`):
//! one with `event_driven_eval = false` (the Lua call on every tick, the
//! pre-change behaviour), one with the default `true`. Every observable
//! decision must match: the per-tick placement counts, every order the OME
//! saw (id, token, side, price, size, mode, status, timestamps), the open
//! and closed positions, the ledger balance, and the engine's decision
//! counters — while the event-driven kernel calls Lua a small fraction as
//! often.
//!
//! The feed exercises each reason a real evaluation must run: book changes
//! with millisecond jitter, a window edge (`time_left_sec` 120 / 45) that is
//! crossed while NO book changes, a book that goes stale inside the window
//! (and a fresh one that ends the gap), a fill (ledger + positions push),
//! and a round rollover.

use super::*;
use crate::engine::{DataEvent, Engine};
use crate::model::CryptoMarket;
use crate::risk::RiskConfig;
use crate::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const SLOT: i64 = 6_000_000;
const ROUND_MS: i64 = 300_000;

fn start_ms(round: i64) -> i64 {
    (SLOT + round) * ROUND_MS
}

/// Instant with `tl_ms` milliseconds left in `round`.
fn at(round: i64, tl_ms: i64) -> i64 {
    start_ms(round) + ROUND_MS - tl_ms
}

fn package_dir(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua")
        .join(name)
}

fn markets(round: i64) -> Vec<CryptoMarket> {
    ["BTC", "ETH"]
        .iter()
        .map(|asset| CryptoMarket {
            asset: (*asset).into(),
            condition_id: format!("{asset}-cond-{round}"),
            question_id: "q".into(),
            up_token_id: format!("{asset}-up-{round}"),
            down_token_id: format!("{asset}-down-{round}"),
            up_price: dec!(0.5),
            down_price: dec!(0.5),
            expires_at_ms: start_ms(round) + ROUND_MS,
            round_slot: SLOT + round,
            round_duration_sec: 300,
            archive_verdict: false,
            venue: String::new(),
            neg_risk: false,
            question: format!("{asset} up or down"),
        })
        .collect()
}

fn core_with(package: &str, event_driven: bool, overrides: &[(&str, &str)]) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(10_000),
            ..Default::default()
        },
        dry_seed_balance: dec!(100_000),
        engine_enabled: true,
        round_duration_sec: 300,
        min_round_age_sec: 0,
        min_shares: dec!(1),
        max_shares: dec!(10_000),
        order_log_path: None,
        trade_log_path: None,
        position_log_path: None,
        execution_policy_path: None,
        event_driven_eval: event_driven,
        ..Default::default()
    });
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    crate::strategies::test_support::host_disabled(&mut eng, cfg.trend, cfg.spread_arb);
    let loaded = load_lua_package(&package_dir(package)).expect("package must load");
    let mut tunables = loaded.tunables;
    for (name, value) in overrides {
        tunables.insert((*name).to_string(), (*value).to_string());
    }
    let adapter = LuaEngineAdapter::new(loaded.strategy, tunables)
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
        .declare_atomic_batch(loaded.atomic_batch)
        .declare_trailing_exit(loaded.trailing_exit)
        .declare_entry_maker(loaded.entry_maker);
    eng.register_user_strategy(
        Box::new(adapter),
        format!("lua:{}", package_dir(package).display()),
    )
    .expect("package registers");
    assert!(eng.set_strategy_enabled(package, true));
    c.enable_engine(eng);
    c
}

/// One scheduled feed event.
enum Feed {
    Round(i64),
    Book(String, Decimal),
}

/// Deterministic jitter (LCG) so book arrivals land on arbitrary
/// milliseconds, not on the tick grid.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self, span: i64) -> i64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as i64).rem_euclid(span)
    }
}

/// Up-leg ask for `asset` in `round` with `tl_ms` left; `None` = no update
/// (a feed gap). The down leg quotes the complement.
fn up_ask(asset: &str, round: i64, tl_ms: i64) -> Option<Decimal> {
    let tl = tl_ms / 1000;
    match (asset, round) {
        // Fresh 0.82 OUTSIDE the window, then the feed falls silent at
        // tl 124: the window opens at 120 with the book still fresh and
        // unchanged (a pure clock edge), and it goes stale at ~116 inside
        // the gap; it resumes at tl 100.
        ("BTC", 0) => (tl > 124 || tl <= 100).then_some(dec!(0.82)),
        // Climbing 1¢/s from 0.70 at tl 130: enters the band at tl 120 on
        // a book change.
        ("ETH", 0) => {
            Some((dec!(0.70) + Decimal::from((130 - tl).max(0)) * dec!(0.01)).min(dec!(0.97)))
        }
        // Out of band, one in-band print at tl 130, then silent until tl 100:
        // stale by the time the window opens → the entry waits for tl 100.
        ("BTC", 1) => {
            if tl > 130 {
                Some(dec!(0.95))
            } else if tl == 130 {
                Some(dec!(0.85))
            } else if tl <= 100 {
                Some(dec!(0.86))
            } else {
                None
            }
        }
        // Below band until tl 50, then in band → entry on a book change
        // just before the 45 s floor.
        ("ETH", 1) => Some(if tl > 50 { dec!(0.75) } else { dec!(0.85) }),
        _ => None,
    }
}

fn schedule(windows: &[(i64, i64)]) -> Vec<(i64, Feed)> {
    let mut out = Vec::new();
    let mut rng = Lcg(0x5eed);
    // The first round's markets arrive with the feed; the second's at its
    // start (the rollover).
    out.push((windows[0].0, Feed::Round(0)));
    for &(from, to) in windows {
        let s = start_ms(1);
        if s >= from && s < to {
            out.push((s, Feed::Round(1)));
        }
        for asset in ["BTC", "ETH"] {
            let mut t = from + rng.next(1000);
            while t < to {
                let round = t.div_euclid(ROUND_MS) - SLOT;
                let tl_ms = start_ms(round) + ROUND_MS - t;
                if let Some(ask) = up_ask(asset, round, tl_ms) {
                    out.push((t, Feed::Book(format!("{asset}-up-{round}"), ask)));
                    let down = (dec!(1.02) - ask).max(dec!(0.02));
                    out.push((
                        t + rng.next(40),
                        Feed::Book(format!("{asset}-down-{round}"), down),
                    ));
                }
                t += 750 + rng.next(500);
            }
        }
    }
    out.sort_by_key(|(t, _)| *t);
    out
}

fn apply(c: &mut Core, ts: i64, feed: &Feed) {
    match feed {
        Feed::Round(round) => {
            c.engine_on_data(
                DataEvent::RoundMarkets {
                    markets: markets(*round),
                    now_ms: ts,
                },
                ts,
            );
        }
        Feed::Book(token, ask) => {
            c.engine_on_data(
                DataEvent::Book {
                    token_id: token.clone(),
                    bids: vec![(*ask - dec!(0.01), dec!(600))],
                    asks: vec![(*ask, dec!(600))],
                    now_ms: ts,
                },
                ts,
            );
        }
    }
}

/// Everything a decision changes, rendered for comparison.
fn decisions(c: &Core, end: i64) -> (Vec<String>, String, String, String, serde_json::Value) {
    let mut orders: Vec<String> = c.list_orders().iter().map(|o| format!("{o:?}")).collect();
    orders.sort();
    let open = format!("{:?}", c.positions.open_positions());
    let closed = format!("{:?}", c.positions.closed_positions());
    let balance = format!("{}", c.accounts.active_ledger().balance());
    (orders, open, closed, balance, c.engine_stats_at(end))
}

/// Drop the host diagnostics that count Lua calls themselves (the one
/// thing the change is meant to alter) from the stats document.
fn strip_call_counters(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for key in [
                "evaluations",
                "calls",
                "lua_calls",
                "eval_us",
                "last_eval_us",
            ] {
                map.remove(key);
            }
            for (_, child) in map.iter_mut() {
                strip_call_counters(child);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_call_counters),
        _ => {}
    }
}

/// Placement instants and counts (`(now_ms, placed)` for every tick that
/// placed anything).
type Placements = Vec<(i64, usize)>;

/// Both kernels after the drive, their placement streams, and the last tick.
type Driven = (Core, Core, Placements, Placements, i64);

/// Drive both kernels through the windows on the 10 ms cadence; return
/// (placements per tick for each, final decisions for each).
fn drive(package: &str, overrides: &[(&str, &str)]) -> Driven {
    // r0 tl 150 → 90; r0 tl 50 → r1 tl 280 (expiry + rollover);
    // r1 tl 140 → past the r1 expiry.
    let windows = [
        (at(0, 150_000), at(0, 90_000)),
        (at(0, 50_000), at(1, 280_000)),
        (at(1, 140_000), at(1, -5_000)),
    ];
    let feed = schedule(&windows);
    let mut every = core_with(package, false, overrides);
    let mut event = core_with(package, true, overrides);
    let (mut placed_every, mut placed_event) = (Vec::new(), Vec::new());
    let mut idx = 0usize;
    let mut end = 0;
    for &(from, to) in &windows {
        let mut now = from;
        while now < to {
            while idx < feed.len() && feed[idx].0 <= now {
                apply(&mut every, feed[idx].0, &feed[idx].1);
                apply(&mut event, feed[idx].0, &feed[idx].1);
                idx += 1;
            }
            every.tick(now).expect("tick");
            event.tick(now).expect("tick");
            let a = every.engine_evaluate(now);
            let b = event.engine_evaluate(now);
            if a > 0 {
                placed_every.push((now, a));
            }
            if b > 0 {
                placed_event.push((now, b));
            }
            end = now;
            now += 10;
        }
    }
    (every, event, placed_every, placed_event, end)
}

fn assert_equivalent(package: &str, overrides: &[(&str, &str)]) -> (Core, Core) {
    let (every, event, placed_every, placed_event, end) = drive(package, overrides);
    assert_eq!(placed_every, placed_event, "per-tick placements diverged");
    let (o1, open1, closed1, bal1, mut s1) = decisions(&every, end);
    let (o2, open2, closed2, bal2, mut s2) = decisions(&event, end);
    assert_eq!(o1, o2, "OME orders diverged");
    assert_eq!(open1, open2, "open positions diverged");
    assert_eq!(closed1, closed2, "closed positions diverged");
    assert_eq!(bal1, bal2, "ledger balance diverged");
    strip_call_counters(&mut s1);
    strip_call_counters(&mut s2);
    assert_eq!(s1, s2, "engine decision counters diverged");
    eprintln!(
        "{package} {overrides:?}: placements {placed_every:?}, orders {}",
        o1.len()
    );
    (every, event)
}

/// Real (Lua-calling) evaluations the kernel ran.
fn lua_evaluations(c: &Core) -> u64 {
    c.stats.lua_evaluations
}

/// Taker late_favorite (the production configuration): identical decisions,
/// and the event-driven kernel engages (late_favorite is clock-safe).
#[test]
fn late_favorite_taker_decisions_match_every_tick_path() {
    let (every, event) = assert_equivalent("late_favorite", &[]);
    assert!(
        event.live_event_eval_engaged(),
        "late_favorite reads no ms clock"
    );
    assert!(!every.live_event_eval_engaged(), "switch off = every tick");
    assert!(
        !every.list_orders().is_empty(),
        "the fixture premise: entries are placed"
    );
    let (n_every, n_event) = (lua_evaluations(&every), lua_evaluations(&event));
    eprintln!("lua evaluations: every-tick {n_every} vs event-driven {n_event}");
    assert!(
        n_event * 10 < n_every,
        "event-driven must call Lua far less often ({n_event} vs {n_every})"
    );
}

/// Maker late_favorite: resting bids and the `breaks` cancel at the 45 s
/// floor (a pure clock edge) must also match.
#[test]
fn late_favorite_maker_decisions_match_every_tick_path() {
    assert_equivalent("late_favorite", &[("maker_entry", "1")]);
}

/// A package that reads the millisecond clock keeps the every-tick Lua call.
#[test]
fn clock_reading_package_keeps_every_tick_evaluation() {
    let c = core_with("lua_momentum", true, &[]);
    assert!(
        !c.live_event_eval_engaged(),
        "lua_momentum reads bk.now_ms → no event-driven skipping"
    );
}
