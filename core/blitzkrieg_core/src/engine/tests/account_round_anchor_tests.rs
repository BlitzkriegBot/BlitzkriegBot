#![cfg(test)]

use super::*;
use std::sync::{Arc, Mutex};

use super::round_rollover_tests::family_market;

/// Records the (slot, time_left, market count) of every evaluation ctx and
/// every `on_round` fire — the surfaces an account duration anchor must scope.
struct AnchorProbe {
    rounds: Arc<Mutex<Vec<(i64, i64)>>>,
    ctxs: Arc<Mutex<Vec<(i64, i64, usize)>>>,
}
impl EngineStrategy for AnchorProbe {
    fn name(&self) -> &str {
        "anchor_probe"
    }
    fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _n: i64) {}
    fn on_round(&mut self, slot: i64, left: i64, _now: i64) {
        self.rounds.lock().unwrap().push((slot, left));
    }
    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<crate::signal::TradeSignal> {
        self.ctxs.lock().unwrap().push((
            ctx.round_slot(),
            ctx.time_left_sec(),
            ctx.markets().len(),
        ));
        Vec::new()
    }
}

fn host_on_default(e: &mut Engine, probe: AnchorProbe) {
    e.register_user_strategy(Box::new(probe), "test".into())
        .unwrap();
    assert!(e.set_strategy_enabled("anchor_probe", true));
}

fn end_ms(duration_sec: i64) -> i64 {
    // Slot 2 of this family; far enough from sibling families that the
    // legacy clocks disagree at the shared evaluation instant.
    duration_sec * 2 * 1000
}

/// Feed one row for each family, then evaluate. Both rows stay in the
/// table (different families merge, they do not replace each other).
fn feed_families_and_evaluate(e: &mut Engine, families: &[i64], now_ms: i64) {
    for &d in families {
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![family_market(end_ms(d), "polymarket", d)],
            now_ms: 1_000,
        });
    }
    e.evaluate(now_ms);
}

/// The production bug is asymmetric: the compiled default family (900s)
/// stays in the scanner table after a live switch, so an unscoped
/// strategy keeps seeing those rows and the admission gate correctly
/// refuses them. The cases below pin BOTH directions, and they use
/// 300/900/3600 — not just 300↔900 — so a later plugin-declaration
/// change that drops 1h/4h/daily cannot mask the dispatch contract.
fn assert_account_sees_only(from: i64, to: i64, now_ms: i64) {
    let ctxs = Arc::new(Mutex::new(Vec::new()));
    let rounds = Arc::new(Mutex::new(Vec::new()));
    let mut e = Engine::new(cfg());
    host_on_default(
        &mut e,
        AnchorProbe {
            rounds: rounds.clone(),
            ctxs: ctxs.clone(),
        },
    );
    e.set_account_round_durations(
        [(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(), to)].into(),
    );
    feed_families_and_evaluate(&mut e, &[from, to], now_ms);
    assert_eq!(
        e.scanner.markets().len(),
        2,
        "{from}s→{to}s: the table still holds both families"
    );
    let seen = ctxs.lock().unwrap();
    assert!(
        !seen.is_empty(),
        "{from}s→{to}s: the anchored instance must evaluate"
    );
    let (slot, tleft, n) = seen[0];
    let own = e.scanner.round_timing_legacy_at(to, now_ms);
    let other = e.scanner.round_timing_legacy_at(from, now_ms);
    assert_ne!(
        own.slot, other.slot,
        "{from}s→{to}s setup: family clocks must disagree at {now_ms}"
    );
    assert_eq!(
        slot, own.slot,
        "{from}s→{to}s: ctx slot must follow the account's {to}s clock"
    );
    assert_eq!(
        tleft, own.time_left_sec,
        "{from}s→{to}s: ctx time_left must follow the account's {to}s clock"
    );
    assert_eq!(
        n, 1,
        "{from}s→{to}s: ctx must hold ONLY the account's {to}s rows, not the leftover {from}s family"
    );
    // The bound instance hears only its family's rollover, even when the
    // leftover family (typically the boot default) also transitions.
    assert_eq!(
        rounds.lock().unwrap().len(),
        1,
        "{from}s→{to}s: on_round must fire only for the account's family: {:?}",
        rounds.lock().unwrap()
    );
}

#[test]
fn an_account_anchor_scopes_markets_in_both_directions() {
    // 1_500_000 ms sits inside every family's early slots used above
    // and is the same instant the #1170 family-binding tests pin.
    let now = 1_500_000i64;
    for (from, to) in [
        (900, 300),  // the reported 15m→5m failure
        (300, 900),  // the reported 5m→15m success
        (900, 3600), // 15m→1h: same dispatch, independent of plugin markets
        (3600, 900),
        (300, 3600),
        (3600, 300),
        (900, 14_400),
        (14_400, 900),
    ] {
        assert_account_sees_only(from, to, now);
    }
}

#[test]
fn a_live_switch_away_from_the_boot_default_hides_leftover_default_rows() {
    // Reproduce the production sequence: the account starts on the
    // compiled default (900s), both families are in the table, then
    // `round_duration.set` pushes a 300s (or 3600s) anchor mid-session.
    // After the switch the ctx must not still list the 900s row — that
    // leftover is what the admission gate rejected as a duration conflict.
    let now = 1_500_000i64;
    for to in [300i64, 3600, 14_400] {
        let ctxs = Arc::new(Mutex::new(Vec::new()));
        let mut e = Engine::new(cfg());
        host_on_default(
            &mut e,
            AnchorProbe {
                rounds: Arc::new(Mutex::new(Vec::new())),
                ctxs: ctxs.clone(),
            },
        );
        feed_families_and_evaluate(&mut e, &[900, to], now);
        {
            let seen = ctxs.lock().unwrap();
            assert_eq!(
                seen.last().map(|c| c.2),
                Some(2),
                "before the switch an unbound instance sees the whole table (to={to})"
            );
        }
        e.set_account_round_durations(
            [(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(), to)].into(),
        );
        ctxs.lock().unwrap().clear();
        e.evaluate(now);
        let seen = ctxs.lock().unwrap();
        assert_eq!(
            seen[0].2, 1,
            "after a live switch to {to}s the ctx must drop the leftover 900s row"
        );
        let own = e.scanner.round_timing_legacy_at(to, now);
        assert_eq!(
            seen[0].0, own.slot,
            "after switch the ctx clock is the {to}s family"
        );
    }
}

#[test]
fn an_account_anchor_outranks_the_strategy_family_binding() {
    // A package bound to 900s on an account that just switched to 300s
    // must follow the account, or the panel switch is a no-op for that
    // package and admission refuses its leftover 15m entries.
    let ctxs = Arc::new(Mutex::new(Vec::new()));
    let mut e = Engine::new(cfg());
    host_on_default(
        &mut e,
        AnchorProbe {
            rounds: Arc::new(Mutex::new(Vec::new())),
            ctxs: ctxs.clone(),
        },
    );
    e.set_strategy_round_families([(crate::engine::strategy_env_fold("anchor_probe"), 900)].into());
    e.set_account_round_durations(
        [(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(), 300)].into(),
    );
    feed_families_and_evaluate(&mut e, &[300, 900], 1_500_000);
    let (slot, _, n) = ctxs.lock().unwrap()[0];
    let own300 = e.scanner.round_timing_legacy_at(300, 1_500_000);
    assert_eq!(n, 1, "account anchor outranks the 900s package binding");
    assert_eq!(slot, own300.slot, "ctx follows the account's 300s clock");
}

#[test]
fn an_account_anchor_does_not_fall_back_to_the_whole_table_when_its_family_is_empty() {
    // Switching onto a family that has no rows yet (5m just re-admitted,
    // or 1h/4h/daily with no venue markets) must yield an empty ctx —
    // never the leftover default family's rows.
    let ctxs = Arc::new(Mutex::new(Vec::new()));
    let mut e = Engine::new(cfg());
    host_on_default(
        &mut e,
        AnchorProbe {
            rounds: Arc::new(Mutex::new(Vec::new())),
            ctxs: ctxs.clone(),
        },
    );
    e.set_account_round_durations(
        [(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(), 300)].into(),
    );
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end_ms(900), "polymarket", 900)],
        now_ms: 1_000,
    });
    e.evaluate(1_500_000);
    assert_eq!(
        e.scanner.markets().len(),
        1,
        "the table holds only the leftover 15m row"
    );
    assert_eq!(
        ctxs.lock().unwrap()[0].2,
        0,
        "an empty 5m view must not fall back to the 15m table"
    );
}
