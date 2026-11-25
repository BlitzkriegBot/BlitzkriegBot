#![cfg(test)]

//! #187 (audit P1-7) falsification tests: a venue health failure halts ITS
//! venue and nothing else.
//!
//! The defect under test is an ESCALATION, not a missing check:
//! `on_venue_self_check` and `on_venue_reconcile_failed` call
//! `freeze_trading` whenever `account_ids_bound_to_venue(venue)` is empty, and
//! `freeze_trading` is `risk.kill` + `GLOBAL_KILL_ACCOUNT`. So a venue with no
//! account bound to it — a dry test, a fresh install, a venue the operator has
//! not wired yet — turns one venue's network blip into a process-level kill
//! switch: every account on every venue stops entering until a human resumes.
//! That is the reverse of the isolation the day-loss breaker already has
//! (`daily_by_account`), and it is the widest possible knife for the narrowest
//! possible problem.
//!
//! What the tests therefore assert is the SCOPE of the halt, read off the
//! kernel's own venue-halt accessor (`Core::active_venue_halts`, which carries
//! the contract's `HaltScope`), plus the operator-visible `tradingFrozen` key in
//! `engine.stats` — the global switch, which must stay DOWN through a venue
//! failure. A panel readout of the venue axis is deliberately NOT added here:
//! `engine.stats` is a frozen contract with a byte-exact fixture, so a new key is
//! its own issue rather than a rider on this fix.
//!
//! The admission teeth are asserted separately (`place_pending` on a request
//! routed to the halted venue refuses, a request routed elsewhere passes) —
//! a stats-only assertion would pass on a halt that blocks nothing.
//!
//! Note on routing: the gate resolves the account's binding first (answerable
//! with no engine), and falls back to the engine's EXECUTION route for UNBOUND
//! accounts, which pass `refuse_cross_venue_order` untouched and are then routed
//! by token. `venue`-tagged markets below exist so that second route is
//! exercised rather than assumed.

use super::*;
use crate::account::AccountId;
use crate::model::CryptoMarket;
use crate::risk::RiskConfig;
use blitzkrieg_market_api::SelfCheckReport;
use rust_decimal_macros::dec;

/// A dry core with `auto_exits_enabled: false` and no durable paths — the
/// standard in-process fixture shape (nothing here touches `data/`).
fn dry_core_cfg() -> CoreConfig {
    CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    }
}

/// One entry intent for `token`, spending `account`.
fn entry_req(token: &str, account: &str, key: &str) -> OrderRequest {
    OrderRequest {
        token_id: token.into(),
        condition_id: format!("cond-{token}"),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price: dec!(0.40),
        size: dec!(1),
        internal_key: key.into(),
        strategy: "s1".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: AccountId::from(account),
    }
}

/// A market row on `venue` carrying `token` as its UP leg — this is what makes
/// `execution_venue_of(token)` answer `venue` for an UNBOUND account (the
/// unique-candidate route in `Engine::execution_venue_for`).
fn market_with_token(venue: &str, token: &str) -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: format!("cond-{token}"),
        question_id: format!("q-{token}"),
        up_token_id: token.into(),
        down_token_id: format!("{token}-down"),
        up_price: dec!(0.40),
        down_price: dec!(0.60),
        expires_at_ms: 1_795_230_000_000,
        round_slot: 1_795_230_000,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: venue.into(),
        neg_risk: false,
        question: "BTC up or down?".into(),
    }
}

fn failing_self_check(ts: i64) -> SelfCheckReport {
    SelfCheckReport {
        ok: false,
        ts_ms: ts,
        items: vec![blitzkrieg_market_api::SelfCheckItem {
            name: "venue_balance".into(),
            ok: false,
            detail: "HTTP 401 unauthorized".into(),
        }],
    }
}

fn sweep_err() -> CoreError {
    CoreError::new(
        blitzkrieg_market_api::CoreErrorCode::VenueError,
        "sweep timed out",
    )
}

/// Attach a real engine and feed `markets` through the SAME entry point a plugin
/// uses (`engine_on_data` → `DataEvent::RoundMarkets`). This is what makes
/// `execution_venue_of(token)` answer: the gate's unbound-account route reads
/// the engine's scanner table, so a test that skipped this would be asserting
/// against an empty table and could not tell "no route" from "route to a
/// healthy venue".
fn install_engine_with(c: &mut Core, markets: Vec<CryptoMarket>, now_ms: i64) {
    c.enable_engine(crate::engine::Engine::new(
        crate::engine::EngineConfig::default(),
    ));
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets { markets, now_ms },
        now_ms,
    );
}

/// An account with NO venue binding. `Core::new`'s zero-config `default` is
/// born bound to `DEFAULT_SEED_VENUE` (issue #1127), so the unbound shape has
/// to be built by injection — the same reason #183's shape-3 test is.
fn unbound_book() -> crate::account::AccountLedgers {
    crate::account::AccountLedgers::new(
        vec![crate::account::Account {
            id: AccountId::from("corpus"),
            name: "corpus".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys: Default::default(),
            venue: String::new(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            opening_equity: None,
            updated_at_ms: 1_000,
        }],
        AccountId::from("corpus"),
    )
}

/// SPEC TEST 1 (issue #187, verbatim name) — a venue health failure with NO
/// account bound to that venue halts ONLY that venue.
///
/// Pre-fix this test cannot pass: `on_venue_reconcile_failed` reached its
/// `accounts.is_empty()` branch and called `freeze_trading`, so `tradingFrozen`
/// went active and the second placement (on a healthy venue) was refused with
/// `KillSwitchActive`.
#[test]
fn sweep_failure_on_unbound_venue_halts_only_that_venue() {
    let mut c = Core::with_account_book(dry_core_cfg(), unbound_book());
    // Two venues are visible to the engine: one that will fail, one that is
    // healthy. Both carry a token the unbound account can be routed to.
    install_engine_with(
        &mut c,
        vec![
            market_with_token("broken-venue", "broken-tok"),
            market_with_token("healthy-venue", "healthy-tok"),
        ],
        1_000,
    );

    // Drive the streak to its threshold on the venue nobody is bound to.
    for i in 0..crate::service::SWEEP_FAILURE_FREEZE {
        c.on_venue_reconcile_failed("broken-venue", sweep_err(), 2_000 + i as i64);
    }

    // The halt exists, and it says which venue and which scope.
    let halts = c.active_venue_halts(3_000);
    assert_eq!(
        halts.len(),
        1,
        "exactly one venue is halted (the broken one): {halts:?}"
    );
    match &halts[0].scope {
        crate::contracts::HaltScope::Venue(v) => assert_eq!(
            v.0, "broken-venue",
            "the halt names the venue that failed: {:?}",
            halts[0]
        ),
        other => panic!("the venue axis holds venue scopes only, got {other:?}"),
    }
    assert!(
        halts[0].reason.contains("sweep failed"),
        "the halt carries the producer's own reason: {:?}",
        halts[0]
    );
    let stats = c.engine_stats();

    // THE HEADLINE: no global gate. This is the assertion the old code fails.
    assert!(
        !c.is_killed(),
        "an unbound venue's failure must not engage the process-level kill switch"
    );
    assert_eq!(
        stats["tradingFrozen"]["active"], false,
        "the operator's global switch stays down: {stats}"
    );
    assert!(
        !stats["accountGates"]
            .as_array()
            .map(|arr| arr
                .iter()
                .any(|g| g.get("account").and_then(|a| a.as_str()) == Some("global")))
            .unwrap_or(false),
        "no global row in the account gate book either: {stats}"
    );

    // Admission teeth: the halted venue refuses, the healthy one admits.
    // Same account, same size — only the routing differs, so a refusal here
    // can only come from the venue axis.
    let e = c
        .place_pending(entry_req("broken-tok", "corpus", "on-broken"), 3_000)
        .unwrap_err();
    assert_eq!(
        e.code,
        blitzkrieg_market_api::CoreErrorCode::MarketHalted,
        "a new entry routed to the halted venue is refused as halted, not as a kill switch: {e}"
    );
    assert!(
        e.message.contains("broken-venue"),
        "the refusal names the halted venue: {e}"
    );
    c.place_pending(entry_req("healthy-tok", "corpus", "on-healthy"), 3_000)
        .unwrap_or_else(|e| panic!("the healthy venue's account must keep trading: {e}"));
}

/// SPEC TEST 2 (issue #187, verbatim name) — the operator's kill switch is
/// still GLOBAL.
///
/// The narrowing must not take the human master lever with it: this test is the
/// counterweight to test 1. `risk.kill` is an operator act
/// (`HaltScope::GlobalOperator` in the contract vocabulary) and it must stop
/// every venue, including ones with no health problem at all.
#[test]
fn operator_kill_still_produces_global_scope() {
    let mut c = Core::with_account_book(dry_core_cfg(), unbound_book());
    install_engine_with(
        &mut c,
        vec![
            market_with_token("venue-a", "tok-a"),
            market_with_token("venue-b", "tok-b"),
        ],
        1_000,
    );

    c.kill("operator: maintenance window".into());

    assert!(c.is_killed(), "the operator's kill switch engages");
    let stats = c.engine_stats();
    assert_eq!(
        stats["tradingFrozen"]["active"], true,
        "the global banner is the operator's act, and it shows: {stats}"
    );
    assert!(
        stats["tradingFrozen"]["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("maintenance window"),
        "the operator's own reason is surfaced verbatim: {stats}"
    );

    // Global reach: EVERY venue refuses, not just a halted one. The gate here
    // is the global row, so the code is KillSwitchActive (not MarketHalted) —
    // the two are deliberately distinguishable to an operator.
    for (token, key) in [("tok-a", "a-entry"), ("tok-b", "b-entry")] {
        let e = c
            .place_pending(entry_req(token, "corpus", key), 2_000)
            .unwrap_err();
        assert_eq!(
            e.code,
            blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            "the global switch refuses {token} regardless of its venue: {e}"
        );
    }

    // And the operator's resume is what lifts it — plus the venue halts, since
    // resume is the master lever (a kernel whose venue came back must be
    // recoverable without a restart).
    c.resume();
    assert!(!c.is_killed());
    let stats = c.engine_stats();
    assert_eq!(stats["tradingFrozen"]["active"], false);
    assert!(
        c.active_venue_halts(2_000).is_empty(),
        "risk.resume clears the venue axis too"
    );
}

/// REGRESSION (this PR's own finding) — an UNBOUND account's order must not
/// slip past a halted venue.
///
/// The obvious implementation reads the halt off `account.venue`. For an
/// unbound account that string is empty, so the gate sees nobody and admits —
/// but `refuse_cross_venue_order` also returns early on empty, and
/// `execution_venue_of` then routes the order to the token's only venue, which
/// is the halted one. The order reaches the broken venue with the gate none the
/// wiser. The gate therefore reads the EXECUTION venue.
#[test]
fn unbound_account_cannot_route_around_a_venue_halt() {
    let mut c = Core::with_account_book(dry_core_cfg(), unbound_book());
    install_engine_with(
        &mut c,
        vec![market_with_token("broken-venue", "broken-tok")],
        1_000,
    );

    assert!(
        c.venue_halts
            .halt("broken-venue", "self-check failed: 401".into(), 2_000),
        "precondition: the venue is halted (0 -> 1 edge)"
    );

    let e = c
        .place_pending(entry_req("broken-tok", "corpus", "unbound-entry"), 3_000)
        .unwrap_err();
    assert_eq!(
        e.code,
        blitzkrieg_market_api::CoreErrorCode::MarketHalted,
        "the order's EXECUTION venue is halted, binding or no binding: {e}"
    );
}

/// A halted venue never traps a position: closes keep flowing, on the same
/// venue, through the same gate.
#[test]
fn venue_halt_never_blocks_an_exit() {
    let mut c = Core::with_account_book(dry_core_cfg(), unbound_book());
    install_engine_with(
        &mut c,
        vec![market_with_token("broken-venue", "broken-tok")],
        1_000,
    );
    c.venue_halts
        .halt("broken-venue", "self-check failed: 401".into(), 2_000);

    let mut close = entry_req("broken-tok", "corpus", "flatten:manual:broken-tok");
    close.side = Side::Sell;
    c.place_pending(close, 3_000)
        .unwrap_or_else(|e| panic!("a halt must never be a teardown: {e}"));
}

/// A failed self-check on an unbound venue takes the SAME venue path (the
/// second escalation site), and a recovered venue lifts it after the debounce.
#[test]
fn self_check_on_unbound_venue_halts_then_lifts_on_recovery() {
    let mut c = Core::with_account_book(dry_core_cfg(), unbound_book());
    install_engine_with(
        &mut c,
        vec![market_with_token("lonely-venue", "tok")],
        1_000,
    );

    c.on_venue_self_check("lonely-venue", failing_self_check(2_000));

    assert!(
        !c.is_killed(),
        "the self-check path must not escalate either"
    );
    assert!(c.venue_halts.is_halted("lonely-venue", 2_000));
    assert!(
        c.venue_halt("lonely-venue", 2_000)
            .is_some_and(|h| h.reason.contains("self-check failed")),
        "the halt names the venue's own failure reason"
    );

    // A sweep that RUNS is evidence the transport works. Inside the debounce
    // the halt holds (a flapping venue must not reopen every cycle).
    c.reconcile(crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![],
        now_ms: 2_000 + crate::service::FREEZE_AUTO_LIFT_MIN_MS / 2,
    })
    .unwrap();
    assert!(
        c.venue_halts.is_halted("lonely-venue", 2_000),
        "an ack inside the debounce must not lift the venue halt"
    );

    // Past the debounce, the same evidence lifts it.
    c.reconcile_for_venue(
        crate::reconcile::VenueSnapshot {
            open_order_ids: vec![],
            trades: vec![],
            now_ms: 2_000 + crate::service::FREEZE_AUTO_LIFT_MIN_MS + 1,
        },
        "lonely-venue",
    )
    .unwrap();
    assert!(
        !c.venue_halts.is_halted(
            "lonely-venue",
            2_000 + crate::service::FREEZE_AUTO_LIFT_MIN_MS + 1
        ),
        "the venue halt lifts once the venue has proved healthy past the debounce"
    );
}

/// Repeat failures are idempotent: one halt row, the FIRST reason kept, and the
/// `since_ms` clock not re-stamped (otherwise the debounce above could never be
/// reached by a steadily failing venue).
#[test]
fn repeat_failures_keep_one_halt_with_the_first_reason() {
    let mut c = Core::with_account_book(dry_core_cfg(), unbound_book());

    assert!(c.venue_halts.halt("v", "first".into(), 1_000));
    assert!(
        !c.venue_halts.halt("v", "second".into(), 5_000),
        "the second failure is not a new lock"
    );
    assert_eq!(
        c.venue_halts.len(),
        1,
        "one row per venue, never one per event"
    );
    assert_eq!(
        c.venue_halts.since_ms("v"),
        Some(1_000),
        "the clock is the FIRST failure: re-stamping would restart the debounce forever"
    );
    assert_eq!(
        c.venue_halts.active("v", 2_000).map(|h| h.reason.as_str()),
        Some("first"),
        "the root cause the operator saw first stands"
    );

    // Empty venue = no identity = no halt row (and no panic in release).
    assert!(
        !c.venue_halts.halt("", "orphan".into(), 1_000),
        "an empty venue must not enter the table"
    );
    assert_eq!(c.venue_halts.len(), 1);
}
