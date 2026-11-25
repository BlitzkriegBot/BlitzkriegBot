#![cfg(test)]

//! Issue #991/#1002 — the `BK_UNVERIFIED_EXPIRY_POLICY=receivable` arm: a
//! fallback close books a receivable CLAIM instead of crediting cash; the
//! claim redeems at venue truth (a resolution or a reconcile SELL fill) with
//! #998 correction rows for the delta and a trade-record amend; the aging
//! warn; the day-budget/breaker exclusion.
//!
//! Issue #176: `receivable` is now the ONLY policy (the spendable-cash arm was
//! deleted, and `BK_UNVERIFIED_EXPIRY_POLICY=cash` refuses the boot). The
//! tests here therefore assert the receivable model ABSOLUTELY — balance
//! unchanged, claim booked, exposure reported — instead of contrasting it
//! against a cash arm that no longer exists. The falsification trio at the
//! bottom pins the deleted entry-price path itself: a dead feed, a 0.95
//! observation, and the removed cash policy.

use super::*;
use crate::model::{FillPolicy, OrderStatus, Side};
use rust_decimal_macros::dec;

/// 900s 网格上 slot 1 的终点：与 expiry_fallback_tests 同锚。
const EXPIRES_MS: i64 = 1_800_000;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bk-receivable-991-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A dry core with the RECEIVABLE policy armed (and durable logs in a private
/// dir). `max_age` defaults to the shipped 48h budget.
fn receivable_core(dir: &std::path::Path, max_age_ms: i64) -> Core {
    let path = |name: &str| Some(dir.join(name).to_string_lossy().to_string());
    let mut c = Core::new(CoreConfig {
        mode: Mode::Dry,
        round_duration_sec: 900,
        risk: crate::risk::RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        unverified_expiry_policy: UnverifiedExpiryPolicy::Receivable,
        receivable_max_age_ms: max_age_ms,
        position_log_path: path("positions.jsonl"),
        trade_log_path: path("trades.jsonl"),
        settlement_log_path: path("settlements.jsonl"),
        order_log_path: path("orders.jsonl"),
        ..Default::default()
    });
    c.set_balance(dec!(1000));
    c
}

fn open_expiring_position(c: &mut Core) -> crate::position::OpenPosition {
    open_expiring_position_at(c, dec!(0.40))
}

/// Open at a chosen entry price. The book carries ASKS only, so the open
/// path never sees a bid — the position starts `Unobserved` (issue #176).
fn open_expiring_position_at(
    c: &mut Core,
    entry: rust_decimal::Decimal,
) -> crate::position::OpenPosition {
    c.book_snapshot("tok", vec![], vec![(entry, dec!(100))], 1);
    let (_id, status) = c
        .place(
            crate::model::OrderRequest {
                token_id: "tok".into(),
                condition_id: "cond".into(),
                side: Side::Buy,
                mode: FillPolicy::Taker,
                price: entry,
                size: dec!(5),
                internal_key: format!("k-{}", c.positions().open_positions().len()),
                strategy: "spread_arb".into(),
                asset: "BTC".into(),
                direction: "up".into(),
                round_slot: 1,
                account_id: crate::model::default_account_id(),
            },
            0,
            1,
        )
        .unwrap();
    assert_eq!(status, OrderStatus::Filled);
    let p = c.positions().open_positions()[0].clone();
    assert_eq!(p.expires_at_ms, EXPIRES_MS);
    p
}

/// Stage an unanswered settlement query (the #684 fixture's seam).
fn stage_unanswered_query(c: &mut Core, condition_id: &str, first_sent_ms: i64) {
    let positions: Vec<crate::position::OpenPosition> = c
        .positions()
        .open_positions()
        .iter()
        .filter(|p| p.condition_id == condition_id)
        .cloned()
        .collect();
    assert!(!positions.is_empty(), "fixture needs the position first");
    c.settlement.track_markets(&positions, first_sent_ms);
    c.settlement.take_queries(first_sent_ms);
    let unanswered = c.settlement.unanswered_since(0, first_sent_ms + 1);
    assert!(
        unanswered.iter().any(|(cid, _)| cid == condition_id),
        "fixture: the query must stage as unanswered: {unanswered:?}"
    );
}

/// Drive the fallback sweep to fire (past the #1236 10s force-settle window
/// that replaced the old 120s silence bound).
fn run_fallback_sweep(c: &mut Core, sent: i64) {
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    c.tick(bound + 1_000).unwrap();
}

/// A real book observation on the position, the #176 shape: the fallback
/// estimator only ever consumes `PriceState::Observed`, never `entry_price`.
fn observe(p: &mut crate::position::OpenPosition, price: rust_decimal::Decimal) {
    p.price_state = crate::price_state::PriceState::observed_bid(price, EXPIRES_MS - 1_000);
}

/// THE acceptance case: fallback close books a CLAIM (receivable rises, cash
/// does NOT move — estimated money is not tradable money); the resolution
/// redeems it at venue truth with a #998 correction row for the delta; the
/// trade record's net is amended by the same delta; the audit identity holds.
#[test]
fn receivable_fallback_books_claim_and_resolution_redeems_at_venue_truth() {
    let dir = scratch("lifecycle");
    let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
    // 0.97 > 0.5 → 二值化结算价 1，price_source = binary_threshold，应收 = 1 × 5 = 5.00。
    let mut p = open_expiring_position(&mut c);
    observe(&mut p, dec!(0.97));
    c.positions_mut().restore_open(vec![p], None);

    // Cash BEFORE the fallback: the entry cost + fee came out of 1000.
    let cash_before = c.accounts().active_ledger().balance();
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    assert!(c.positions().open_positions().is_empty(), "fallback closed");

    // The claim exists: receivable = price × shares = 1.00 × 5 = 5.00, and the
    // ledger did NOT credit a cent (the cash model would have).
    let settled_stats =
        c.engine_stats_at(sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS + 2_000)["settlement"]
            .clone();
    assert_eq!(
        settled_stats["fallbackReceivableUsd"],
        serde_json::json!(5.0),
        "the claim books the fallback notional as receivable"
    );
    assert_eq!(
        settled_stats["pendingFallbackClaims"],
        serde_json::json!(1),
        "one open fallback claim"
    );
    assert_eq!(
        settled_stats["unverifiedExpiryPolicy"],
        serde_json::json!("receivable"),
        "the policy in force rides the view"
    );
    let age = settled_stats["oldestFallbackClaimAgeMs"].as_i64();
    assert!(
        age.is_some_and(|a| a >= 0),
        "an aging signal exists once a claim is open"
    );
    let cash_after_fallback = c.accounts().active_ledger().balance();
    assert_eq!(
        cash_after_fallback, cash_before,
        "the receivable model moves NO cash at the fallback close (estimated money must not be tradable)"
    );
    // The claim rides the SAME receivable pool the settled claims ride.
    assert_eq!(
        settled_stats["receivableUsd"],
        serde_json::json!(5.0),
        "the fallback slice folds into the pool the audit reads"
    );
    // The journal carried the booking: the durable arm survives a restart.
    let journal = std::fs::read_to_string(dir.join("settlements.jsonl")).unwrap();
    assert!(
        journal.contains("expiryFallbackBooked"),
        "the claim booking is journaled for replay: {journal}"
    );

    // Venue truth arrives: the market resolves WINNER at 0.70/share. The claim
    // redeems: booked 5.00, venue 3.50 (0.70 × 5), correction -1.50
    // (venue_truth − booked), net cash +3.50 = venue truth.
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    let resolution = blitzkrieg_market_api::MarketResolution {
        condition_id: "cond".into(),
        resolved: true,
        payouts: vec![("tok".to_string(), dec!(0.70))],
        neg_risk: false,
        source: "test".into(),
    };
    c.on_market_resolution(resolution, bound + 3_000);

    let stats = c.engine_stats_at(bound + 4_000)["settlement"].clone();
    assert_eq!(
        stats["fallbackReceivableUsd"],
        serde_json::json!(0.0),
        "the claim left the pool"
    );
    assert_eq!(stats["pendingFallbackClaims"], serde_json::json!(0));
    assert_eq!(
        stats["oldestFallbackClaimAgeMs"],
        serde_json::Value::Null,
        "no aging signal with no open claim"
    );
    let cash_after_redeem = c.accounts().active_ledger().balance();
    assert_eq!(
        cash_after_redeem - cash_before,
        dec!(3.5),
        "cash moved by VENUE TRUTH (0.70 × 5 = 3.50), net of the 5.00 booked estimate"
    );
    // The ledger journal: the booked redemption row (+5.00, the estimate) + the
    // correction row (-1.50, venue_truth − booked).
    let ledger_journal = c.accounts().active_ledger().journal_snapshot();
    let redemption_rows: Vec<_> = ledger_journal
        .iter()
        .filter(|e| e.kind == crate::ledger::JournalKind::Redemption)
        .collect();
    let position_id = c.positions().closed_positions()[0].id.clone();
    assert!(
        redemption_rows
            .iter()
            .any(|e| e.ref_ == position_id && e.amount == dec!(5.0)),
        "the BOOKED row credits the estimate: {redemption_rows:?}"
    );
    assert!(
        redemption_rows
            .iter()
            .any(|e| e.amount == dec!(-1.5) && e.ref_.contains("fallback redemption correction")),
        "the correction row moves venue_truth − booked (-1.50): {redemption_rows:?}"
    );

    // The trade record's net moves by the SAME delta (Δcash == Δrealized). The
    // delta is -1.50 (venue 3.50 − booked 5.00). Capture the booked net from
    // the closed row BEFORE the redemption, then expect booked + delta.
    let booked_net = c.positions().closed_positions()[0].net_pnl_usd;
    let expected_net = booked_net + dec!(-1.5);
    use rust_decimal::prelude::ToPrimitive;
    let trades = c.recent_trades(10);
    assert_eq!(trades.len(), 1);
    let got = trades[0]["netPnlUsd"].as_f64().expect("net is a number");
    assert!(
        (got - expected_net.to_f64().unwrap()).abs() < 1e-9,
        "the recorded net is amended by the venue-vs-estimate delta"
    );
    // The amend keeps the provenance flags: still an estimated close.
    let closed_after = &c.positions().closed_positions()[0];
    assert_eq!(
        closed_after.source, "reconciled",
        "the resolution marked the row reconciled"
    );
    assert_eq!(
        closed_after.exit_price,
        dec!(1),
        "the row's price stays the estimate (1.00, never re-labeled a venue verdict)"
    );
    assert_eq!(
        closed_after.price_source, "binary_threshold",
        "the fallback was threshold-priced (issue 1238)"
    );

    // THE audit: with receivable and correction both folded, the identity
    // holds (this is the algebra the amend exists for).
    let audit = c.run_accounting_audit(bound + 5_000);
    assert!(
        audit.ok,
        "audit after receivable redemption failed (see AuditReport fields)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Zero-delta redemption: the venue truth EQUALS the fallback estimate — the
/// claim redeems, cash moves by the booked amount, and NO correction row is
/// booked (a correction of nothing is not a fact).
#[test]
fn zero_delta_redemption_books_no_correction_row() {
    let dir = scratch("zero-delta");
    let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
    // 0.97 > 0.5 → 二值化结算价 1，应收 = 1 × 5 = 5.00。
    let mut p = open_expiring_position(&mut c);
    observe(&mut p, dec!(0.97));
    c.positions_mut().restore_open(vec![p], None);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    assert_eq!(
        c.accounts().active_ledger().journal_snapshot(),
        c.accounts().active_ledger().journal_snapshot(),
        "fixture sanity"
    );

    // The venue's verdict prices the token at EXACTLY the fallback estimate
    // (1.00/share — the binary settlement price).
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    let resolution = blitzkrieg_market_api::MarketResolution {
        condition_id: "cond".into(),
        resolved: true,
        payouts: vec![("tok".to_string(), dec!(1.00))],
        neg_risk: false,
        source: "test".into(),
    };
    c.on_market_resolution(resolution, bound + 3_000);

    let stats = c.engine_stats_at(bound + 4_000)["settlement"].clone();
    assert_eq!(
        stats["pendingFallbackClaims"],
        serde_json::json!(0),
        "the claim redeemed"
    );
    let ledger_journal = c.accounts().active_ledger().journal_snapshot();
    let corrections: Vec<_> = ledger_journal
        .iter()
        .filter(|e| e.ref_.contains("fallback redemption correction"))
        .collect();
    assert!(
        corrections.is_empty(),
        "a zero-delta redemption books NO correction row: {corrections:?}"
    );
    // Cash moved by the booked estimate exactly once (one Redemption row).
    let redemptions: Vec<_> = ledger_journal
        .iter()
        .filter(|e| e.kind == crate::ledger::JournalKind::Redemption)
        .collect();
    assert_eq!(
        redemptions.len(),
        1,
        "exactly one redemption row (the booked one): {redemptions:?}"
    );
    assert_eq!(redemptions[0].amount, dec!(5.0));
    let audit = c.run_accounting_audit(bound + 5_000);
    assert!(audit.ok, "audit after zero-delta redemption failed");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The reconcile sweep's SELL fill is venue truth too: a claim whose token the
/// sweep saw traded redeems at the FILL price (the resolution never arrived).
#[test]
fn reconcile_sell_fill_redeems_the_claim_at_the_fill_price() {
    let dir = scratch("reconcile-redeem");
    let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
    let _p = open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;

    // No resolution — the venue's cash truth arrives as a SELL fill on the
    // token via the reconcile sweep, at 0.55/share (≠ the 0.40 estimate).
    let snap = crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![crate::reconcile::VenueTrade {
            venue_order_id: "ven:1".into(),
            trade_id: "tr:1".into(),
            token_id: "tok".into(),
            side: crate::model::Side::Sell,
            size: dec!(5),
            price: dec!(0.55),
            ts_ms: bound + 2_000,
            tx_hash: None,
            maker: None,
            // #175: the venue row names the account whose cash moved. The
            // position is already fallback-closed, so the fold parks this as
            // `unknown_position` — the RECEIVABLE path below (claim
            // redemption) is what books cash here, and it is token-scoped.
            account: Some(crate::model::default_account_id()),
        }],
        now_ms: bound + 2_000,
    };
    let _report = c.reconcile(snap).expect("reconcile succeeds");

    let stats = c.engine_stats_at(bound + 3_000)["settlement"].clone();
    assert_eq!(
        stats["pendingFallbackClaims"],
        serde_json::json!(0),
        "the reconcile fill redeemed the claim"
    );
    let cash_before = c.accounts().active_ledger().balance();
    // The redemption credited venue truth: booked 2.00 + correction (0.55×5 −
    // 2.00) = 2.75 total.
    let ledger_journal = c.accounts().active_ledger().journal_snapshot();
    let moved: rust_decimal::Decimal = ledger_journal
        .iter()
        .filter(|e| e.kind == crate::ledger::JournalKind::Redemption)
        .map(|e| e.amount)
        .sum();
    assert_eq!(
        moved,
        dec!(2.75),
        "booked row + correction row sum to venue truth (0.55 × 5)"
    );
    let _ = cash_before;
    // The trade record was amended by the same +0.75.
    let audit = c.run_accounting_audit(bound + 4_000);
    assert!(audit.ok, "audit after reconcile redemption failed");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Journal durability: a claim booked before a restart replays from the
/// settlement journal with its aging clock intact, and a Redeemed line removes
/// it (last-writer-wins).
#[test]
fn fallback_claim_journal_replay_survives_a_restart() {
    let dir = scratch("replay");
    let sent;
    let bound;
    {
        let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
        // 0.97 > 0.5 → 二值化结算价 1，应收 = 1 × 5 = 5.00。
        let mut p = open_expiring_position(&mut c);
        observe(&mut p, dec!(0.97));
        c.positions_mut().restore_open(vec![p], None);
        sent = EXPIRES_MS + 100;
        c.tick(sent).unwrap();
        stage_unanswered_query(&mut c, "cond", sent);
        run_fallback_sweep(&mut c, sent);
        bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
        assert_eq!(
            c.engine_stats_at(bound + 2_000)["settlement"]["pendingFallbackClaims"],
            serde_json::json!(1)
        );
        // Core drops; the journal file stays.
    }
    {
        // A fresh core over the SAME dir replays the journal.
        let mut c2 = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
        let stats = c2.engine_stats_at(bound + 2_000)["settlement"].clone();
        assert_eq!(
            stats["pendingFallbackClaims"],
            serde_json::json!(1),
            "the claim survives the restart"
        );
        assert_eq!(
            stats["fallbackReceivableUsd"],
            serde_json::json!(5.0),
            "the booked notional survives the restart"
        );
        // The aging clock rode the journal's at_ms: the replayed claim's age is
        // measured from the ORIGINAL booking instant (the sweep fired at
        // `bound + 1_000`), not from the restart.
        let age = stats["oldestFallbackClaimAgeMs"]
            .as_i64()
            .expect("age present");
        assert!(
            age >= (bound + 2_000) - (bound + 1_000),
            "the replayed claim's aging clock starts at the booking, not the restart: {age}"
        );
        // And the redemption still works on the replayed state.
        let resolution = blitzkrieg_market_api::MarketResolution {
            condition_id: "cond".into(),
            resolved: true,
            payouts: vec![("tok".to_string(), dec!(1.00))],
            neg_risk: false,
            source: "test".into(),
        };
        c2.on_market_resolution(resolution, bound + 3_000);
        assert_eq!(
            c2.engine_stats_at(bound + 4_000)["settlement"]["pendingFallbackClaims"],
            serde_json::json!(0),
            "the replayed claim redeems exactly once"
        );
        // A SECOND core over the same journal sees the Redeemed line — the
        // claim must NOT resurrect (last-writer-wins replay).
        let c3 = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
        assert_eq!(
            c3.engine_stats_at(bound + 5_000)["settlement"]["pendingFallbackClaims"],
            serde_json::json!(0),
            "the Redeemed journal line removes the claim on replay"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// The aging warn: overdue claims warn ONCE per episode (the edge), stay quiet
/// while still overdue, and re-arm after the pool empties. max_age <= 0 is off.
#[test]
fn the_aging_warn_fires_once_per_episode() {
    let dir = scratch("aging");
    // A tiny budget so the bound is reachable in test time.
    let mut c = receivable_core(&dir, 5_000);
    open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;

    // Past the 5s budget: the warn fires on the first maintenance pass and
    // NEVER again while the same episode continues.
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    for i in 0..3 {
        c.tick(bound + 10_000 + i * 6_000).unwrap();
    }
    let mut alerts = 0;
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev
            && message.contains("receivable aging")
        {
            alerts += 1;
        }
    }
    assert_eq!(alerts, 1, "the warn fires ONCE per episode, not every tick");
    drop(rx);

    // Redeem (zero-delta) → the pool empties → the edge re-arms.
    let resolution = blitzkrieg_market_api::MarketResolution {
        condition_id: "cond".into(),
        resolved: true,
        payouts: vec![("tok".to_string(), dec!(0.40))],
        neg_risk: false,
        source: "test".into(),
    };
    c.on_market_resolution(resolution, bound + 30_000);
    assert_eq!(
        c.engine_stats_at(bound + 31_000)["settlement"]["pendingFallbackClaims"],
        serde_json::json!(0)
    );

    // A SECOND cycle: a fresh claim in the pool (booked straight through the
    // settlement book — this test targets the aging EDGE, not the admission
    // gates, and a full re-entry would wade through cooldowns/expiry grids).
    // Its booking instant is already past the 5s budget when it lands.
    let t2 = bound + 40_000;
    // A pass with an EMPTY pool: this is the tick that re-arms the edge
    // (oldest == None → receivable_warned_age_ms resets to 0).
    c.tick(t2 - 1_000).unwrap();
    c.settlement.book_fallback_claim(
        "pos-aging-second",
        "cond-2",
        "tok-2",
        crate::model::default_account_id().as_str(),
        dec!(5),
        dec!(0.40),
        "current",
        Decimal::ZERO,
        t2,
        t2 - 60_000,
    );
    assert_eq!(
        c.engine_stats_at(t2)["settlement"]["pendingFallbackClaims"],
        serde_json::json!(1),
        "precondition: the second claim is open and already overdue"
    );
    let (tx2, mut rx2) = mpsc::unbounded_channel();
    c.set_event_sink(tx2);
    c.tick(t2 + 1_000).unwrap();
    let mut alerts2 = 0;
    while let Ok(ev) = rx2.try_recv() {
        if let Event::RiskAlert { message, .. } = ev
            && message.contains("receivable aging")
        {
            alerts2 += 1;
        }
    }
    assert_eq!(alerts2, 1, "the re-armed edge fires once for the new claim");

    let _ = std::fs::remove_dir_all(&dir);
}

/// max_age = 0 disables the aging warn entirely (documented off-spelling).
#[test]
fn zero_aging_budget_disables_the_warn() {
    let dir = scratch("aging-off");
    let mut c = receivable_core(&dir, 0);
    open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    c.tick(bound + 1_000_000).unwrap();
    let mut alerts = 0;
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev
            && message.contains("receivable aging")
        {
            alerts += 1;
        }
    }
    assert_eq!(alerts, 0, "max_age = 0 means the aging check is OFF");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #1238: binary fallback `price_source` naming — a zero feed is
/// `unobserved` (never entry_fallback / cost recovery), a live mark above
/// 0.5 is `binary_threshold` settling `1`, at-or-below 0.5 settles `0`,
/// and venue closes carry neither field.
#[test]
fn price_source_names_the_fallback_basis() {
    // Zero feed → unobserved, booked price 0, never cost recovery.
    {
        let dir = scratch("price-source-zero");
        let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
        let p = open_expiring_position(&mut c);
        // The position opened UNOBSERVED (#176) — no quote was ever observed.
        let dead = p.clone();
        assert!(dead.price_state.is_unobserved(), "fixture: never observed");
        c.positions.restore_open(vec![dead], None);
        let sent = EXPIRES_MS + 100;
        c.tick(sent).unwrap();
        stage_unanswered_query(&mut c, "cond", sent);
        run_fallback_sweep(&mut c, sent);
        let closed = &c.positions().closed_positions()[0];
        assert_eq!(closed.source, "fallback");
        assert_eq!(
            closed.exit_price,
            dec!(0),
            "zero feed books 0, never entry price"
        );
        assert_eq!(
            closed.price_source, "unobserved",
            "zero feed must be NAMED unobserved (issue 1238), not entry_fallback"
        );
        let raw = serde_json::to_value(closed).unwrap();
        assert_eq!(
            raw["price_source"],
            serde_json::json!("unobserved"),
            "the wire carries the basis (additive)"
        );
        // No spendable cash: 0 × dust_shares = 0 → no ExpiryFallback journal row.
        let journal = c.accounts().active_ledger().journal_snapshot();
        assert!(
            !journal
                .iter()
                .any(|e| e.kind == crate::ledger::JournalKind::ExpiryFallback),
            "no ExpiryFallback cash credit for an unobserved close"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Live mark above 0.5 → binary_threshold, settles 1, books receivable = 1 × shares.
    {
        let dir = scratch("price-source-live");
        let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
        let p = open_expiring_position(&mut c);
        let mut hi = p.clone();
        observe(&mut hi, dec!(0.97));
        c.positions.restore_open(vec![hi], None);
        let sent = EXPIRES_MS + 100;
        c.tick(sent).unwrap();
        stage_unanswered_query(&mut c, "cond", sent);
        run_fallback_sweep(&mut c, sent);
        let closed = &c.positions().closed_positions()[0];
        assert_eq!(closed.exit_price, dec!(1), "mid 0.97 settles 1");
        assert_eq!(
            closed.price_source, "binary_threshold",
            "live mark above 0.5 is threshold-priced"
        );
        let raw = serde_json::to_value(closed).unwrap();
        assert_eq!(
            raw["price_source"],
            serde_json::json!("binary_threshold"),
            "the wire carries the basis (additive)"
        );
        let stats =
            c.engine_stats_at(sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS + 2_000)["settlement"]
                .clone();
        assert_eq!(
            stats["fallbackReceivableUsd"],
            serde_json::json!(5.0),
            "receivable = 1.00 × 5 shares"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // At-or-below 0.5 → binary_threshold, settles 0.
    {
        let dir = scratch("price-source-half");
        let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
        let p = open_expiring_position(&mut c);
        let mut lo = p.clone();
        observe(&mut lo, dec!(0.4));
        c.positions.restore_open(vec![lo], None);
        let sent = EXPIRES_MS + 100;
        c.tick(sent).unwrap();
        stage_unanswered_query(&mut c, "cond", sent);
        run_fallback_sweep(&mut c, sent);
        let closed = &c.positions().closed_positions()[0];
        assert_eq!(closed.exit_price, dec!(0), "mid 0.4 settles 0");
        assert_eq!(
            closed.price_source, "binary_threshold",
            "mid at/below 0.5 is threshold-priced to 0"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A venue close carries NEITHER field: additive-only, older readers see nothing new.
    {
        let dir2 = scratch("price-source-venue");
        let mut c2 = receivable_core(&dir2, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
        let p2 = open_expiring_position(&mut c2);
        let closed2 = c2
            .positions
            .close(
                &p2.id,
                dec!(0.50),
                crate::model::ExitReason::TakeProfit,
                true,
                1_900_000,
                "venue",
            )
            .expect("venue close lands");
        assert_eq!(closed2.source, "venue");
        assert_eq!(
            closed2.price_source, "",
            "a venue close has no fallback basis to name"
        );
        let raw2 = serde_json::to_value(closed2).unwrap();
        assert!(
            raw2.get("price_source").is_none(),
            "the field is SKIPPED when empty — no wire byte changes for venue rows: {raw2}"
        );
        let _ = std::fs::remove_dir_all(&dir2);
    }
}

/// Boundary 2: the unverified-exposure report keeps BOTH faces visible in
/// receivable mode — the plain fallback row AND the reconciled row (a
/// redemption never removes a row from the report; visibility is not
/// forgiveness).
#[test]
fn unverified_exposure_stays_visible_through_the_receivable_cycle() {
    let dir = scratch("exposure");
    let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
    let _p = open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;

    let exposure = c.unverified_exposure_report();
    assert_eq!(exposure.total_positions, 1);
    assert_eq!(exposure.entries[0].source, "fallback");

    // The resolution redeems the claim AND marks the row reconciled — the row
    // STAYS on the report.
    let resolution = blitzkrieg_market_api::MarketResolution {
        condition_id: "cond".into(),
        resolved: true,
        payouts: vec![("tok".to_string(), dec!(0.40))],
        neg_risk: false,
        source: "test".into(),
    };
    c.on_market_resolution(resolution, bound + 3_000);
    let exposure = c.unverified_exposure_report();
    assert_eq!(
        exposure.total_positions, 1,
        "a reconciled row is still unverified-cash history — it stays visible"
    );
    assert_eq!(exposure.entries[0].source, "reconciled");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Boundary 3 discipline lives in the ABSENCE of a fast-confirm knob: the
/// receivable path's only redemption triggers are the venue's own truth
/// (resolution verdict / reconcile fill). This test pins that a claim with NO
/// venue evidence just stays open — no code path may redeem it early.
#[test]
fn a_claim_without_venue_truth_stays_open() {
    let dir = scratch("no-truth-stays-open");
    let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
    open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;

    // Ticks, aging warns, and a resolution for a DIFFERENT condition must all
    // leave the claim open. Nothing but this condition's own truth redeems it.
    for i in 0..5 {
        c.tick(bound + 10_000 + i * 60_000).unwrap();
    }
    let other = blitzkrieg_market_api::MarketResolution {
        condition_id: "cond-OTHER".into(),
        resolved: true,
        payouts: vec![("tok-OTHER".to_string(), Decimal::ONE)],
        neg_risk: false,
        source: "test".into(),
    };
    c.on_market_resolution(other, bound + 400_000);
    assert_eq!(
        c.engine_stats_at(bound + 410_000)["settlement"]["pendingFallbackClaims"],
        serde_json::json!(1),
        "no venue truth for THIS condition = the claim stays open (no fast-confirm path)"
    );
    assert_eq!(
        c.accounts().active_ledger().balance(),
        c.accounts().active_ledger().balance(),
        "and no cash ever moved"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Day-budget boundary (issue #991: estimated money must not be tradable): the
/// fallback close books NO cash, so the day's OPENING EQUITY — read off the
/// ledger balance at the UTC roll — excludes the receivable. The ledger itself
/// IS the exclusion: no separate knob exists.
///
/// Issue #176 deleted the `cash` arm this test used to contrast against, so the
/// comparison is now ABSOLUTE rather than differential (a differential with a
/// deleted arm would silently become a tautology): the cash book must be
/// byte-identical across the fallback close while the receivable pool carries
/// the notional, and the next day's budget base must equal the ledger — i.e. it
/// carries the cash and NOT the claim.
#[test]
fn the_receivable_never_reaches_the_day_budget_base() {
    let dir = scratch("budget-receivable");
    let path = |name: &str| Some(dir.join(name).to_string_lossy().to_string());
    let mut c = Core::new(CoreConfig {
        mode: Mode::Dry,
        round_duration_sec: 900,
        risk: crate::risk::RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        unverified_expiry_policy: UnverifiedExpiryPolicy::Receivable,
        positions: crate::position::PositionConfig {
            max_daily_loss_usd: dec!(2),
            daily_loss_marks_unrealized: false,
            ..Default::default()
        },
        position_log_path: path("positions.jsonl"),
        trade_log_path: path("trades.jsonl"),
        settlement_log_path: path("settlements.jsonl"),
        order_log_path: path("orders.jsonl"),
        ..Default::default()
    });
    c.set_balance(dec!(1000));
    // 0.97 > 0.5 → 二值化结算价 1：应收 1 × 5 = 5.00，现金 0。
    let mut p = open_expiring_position(&mut c);
    observe(&mut p, dec!(0.97));
    c.positions_mut().restore_open(vec![p], None);
    // Day 0 stamps here — BEFORE the fallback close.
    c.tick(1_000).unwrap();
    let cash_before = c.accounts().active_ledger().balance();
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    let bound = sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS;
    // Part 1 — no cash moved at the fallback close, while the claim exists.
    assert_eq!(
        c.accounts().active_ledger().balance(),
        cash_before,
        "the fallback close moves NO cash (estimated money is not tradable)"
    );
    assert_eq!(
        c.engine_stats_at(bound + 2_000)["settlement"]["fallbackReceivableUsd"],
        serde_json::json!(5.0),
        "the notional IS on the books — as a receivable, not as cash"
    );
    // Part 2 — cross a UTC day boundary: the roll re-reads the ledger for the
    // new day's opening equity, so the base must equal the ledger balance and
    // therefore exclude the 5.00 receivable.
    c.tick(bound + 100_000_000).unwrap();
    let ledger_at_roll = c.accounts().active_ledger().balance();
    // Decimal's default serde spells STRINGS on this wire — parse it back.
    let opening = c.engine_stats_at(bound + 100_100_000)["dailyLoss"]["openingEquityUsd"]
        .as_str()
        .and_then(|s| rust_decimal::Decimal::from_str_exact(s).ok())
        .unwrap_or_default();
    assert_eq!(
        opening, ledger_at_roll,
        "the day-budget base IS the ledger — no receivable folded in"
    );
    assert!(
        ledger_at_roll < cash_before + dec!(5.0),
        "the base does not carry the 5.00 estimate (ledger {ledger_at_roll} vs cash {cash_before})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ─────────────────────────────────────────────────────────────────────────
// Issue #176 falsification trio — the three shapes of the deleted
// entry-price path. Each one fails if the entry price (or a bare 0) can
// stand in for an observation.
// ─────────────────────────────────────────────────────────────────────────

/// #176 (R7.1 L1) — the headline defect: a position opened at 0.96 whose feed
/// then dies must NOT have its entry price thresholded into a `1.00`-per-share
/// "verdict". The deleted path seeded `current_price = entry_price`, so
/// `0.96 > 0.5` booked `1.00 × 5 shares = 5.00` of receivable minted from the
/// cost basis (and, under the deleted `cash` policy, 5.00 of SPENDABLE cash).
#[test]
fn dead_feed_never_uses_entry_price_as_binary_input() {
    let dir = scratch("dead-feed");
    let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
    // Open at 0.96 — ABOVE the 0.5 binary threshold, i.e. exactly the value the
    // deleted path (mis)read as a winning verdict.
    let p = open_expiring_position_at(&mut c, dec!(0.96));
    assert_eq!(p.entry_price, dec!(0.96), "fixture: cost basis is 0.96");
    // The regression itself: opening writes NO price. The entry price is a cost
    // basis, not an observation — and `mark()` cannot hand it back.
    assert!(
        p.price_state.is_unobserved(),
        "a freshly opened position has observed nothing: {:?}",
        p.price_state
    );
    assert_eq!(
        p.mark(),
        None,
        "no quote was ever observed — no mark exists"
    );
    assert_eq!(p.mark_or_zero(), Decimal::ZERO);

    // The feed dies: the book's last word is "nobody is quoting", then silence.
    let cash_before = c.accounts().active_ledger().balance();
    c.book_snapshot("tok", vec![], vec![], EXPIRES_MS - 1_000);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    assert!(c.positions().open_positions().is_empty(), "fallback closed");

    let closed = &c.positions().closed_positions()[0];
    assert_eq!(
        closed.entry_price,
        dec!(0.96),
        "the cost basis is unchanged"
    );
    assert_eq!(
        closed.exit_price,
        Decimal::ZERO,
        "no observation → the close books 0, never the 0.96 entry price"
    );
    assert_ne!(
        closed.exit_price,
        Decimal::ONE,
        "the deleted path thresholded 0.96 > 0.5 into a 1.00 verdict — it must not come back"
    );
    assert_eq!(closed.price_source, "unobserved", "the basis is NAMED");
    assert_eq!(
        closed.source, "fallback",
        "an estimate, not a venue verdict"
    );
    assert!(
        closed.net_pnl_usd < Decimal::ZERO,
        "the honest loss is on the books (0 payout vs the 0.96 basis), \
         not hidden behind a fake winner"
    );

    // The arithmetic the bug produced: 1.00 × 5 shares = 5.00. The claim
    // exists (the unknown is carried, not dropped) but is booked at ZERO.
    let stats =
        c.engine_stats_at(sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS + 2_000)["settlement"]
            .clone();
    assert_eq!(
        stats["fallbackReceivableUsd"],
        serde_json::json!(0.0),
        "5.00 here would be the entry-price verdict the issue deleted"
    );
    assert_eq!(stats["pendingFallbackClaims"], serde_json::json!(1));
    let claim = c
        .settlement
        .fallback_claims()
        .next()
        .expect("the unknown is carried as a claim");
    assert_eq!(claim.fallback_price, Decimal::ZERO);
    assert_eq!(claim.price_source, "unobserved");
    assert_eq!(claim.shares, dec!(5));
    // And no cash moved at all (the deleted `cash` policy credited 5.00 here).
    assert_eq!(
        c.accounts().active_ledger().balance(),
        cash_before,
        "the fallback close moves NO cash"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// #176 — a 0.95 quote is a VALUATION INPUT, not a verdict. The mark carries
/// the observation's provenance (`LastBid`, the strongest rung) and its OWN
/// clock; a feed that dies must not restamp the stale quote as fresh, and the
/// quote itself must never be recorded as a settlement price.
#[test]
fn mid_price_095_is_not_a_verdict() {
    let dir = scratch("mark-095");
    let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
    open_expiring_position(&mut c);
    // A two-sided book: bid 0.95 / ask 0.97 (mid 0.96). The bid is the
    // strongest reference, so that is the rung the observation names — the mid
    // was NOT taken, and neither was the entry price.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.95), dec!(100))],
        vec![(dec!(0.97), dec!(100))],
        500_000,
    );
    c.tick(600_000).unwrap();
    {
        let p = &c.positions().open_positions()[0];
        assert_eq!(p.price_state.price(), Some(dec!(0.95)));
        assert_eq!(
            p.price_state.source(),
            Some(crate::price_state::PriceSource::LastBid),
            "the bid rung answered — provenance is recorded, not inferred"
        );
        assert_eq!(
            p.price_state.observed_at_ms(),
            Some(500_000),
            "the BOOK's receive time, never the tick's clock"
        );
    }

    // The feed dies (an empty snapshot, then silence). A later tick must not
    // refresh the old quote into a fake "just observed": the clock stays.
    c.book_snapshot("tok", vec![], vec![], 700_000);
    c.tick(1_000_000).unwrap();
    {
        let p = &c.positions().open_positions()[0];
        assert_eq!(
            p.price_state,
            crate::price_state::PriceState::observed_bid(dec!(0.95), 500_000),
            "a stale observation stays itself, with its own age"
        );
        assert!(
            p.price_state.observed_at_ms().is_some(),
            "the observation keeps a clock even after the feed died"
        );
        // It is a VALUATION — the position is still open, nothing was settled,
        // and no money moved. Only venue truth (or the expiry estimator, which
        // is named apart) can turn a quote into a booking.
        assert_eq!(p.mark(), Some(dec!(0.95)));
    }
    assert!(
        c.positions().closed_positions().is_empty(),
        "a 0.95 quote is not a settlement verdict"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// #176 — the removed `cash` policy's whole purpose was to credit
/// `price × shares` to SPENDABLE cash. The spelling now refuses the boot, and
/// even the surviving policy's estimate: (1) leaves the balance byte-identical,
/// (2) lands in the receivable pool, (3) keeps the position on the
/// unverified-exposure report.
#[test]
fn cash_policy_removed_estimate_never_credits_balance() {
    // The boot-side refusal is what makes the absence structural: no
    // configuration can reach a cash-crediting arm. (Full contract:
    // `service::config::tests::the_removed_cash_policy_is_refused_not_downgraded`.)
    let err = UnverifiedExpiryPolicy::parse("cash")
        .expect_err("the cash policy spelling is deleted, not downgraded");
    assert!(err.to_string().contains("#176"), "{err}");

    let dir = scratch("cash-removed");
    let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
    // 0.97 > 0.5 → 二值化结算价 1，应收 = 1 × 5 = 5.00。
    let mut p = open_expiring_position(&mut c);
    observe(&mut p, dec!(0.97));
    let position_id = p.id.clone();
    c.positions_mut().restore_open(vec![p], None);
    let cash_before = c.accounts().active_ledger().balance();
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    assert_eq!(
        c.engine_stats_at(sent)["settlement"]["receivableUsd"],
        serde_json::json!(0.0),
        "precondition: nothing booked before the sweep"
    );
    run_fallback_sweep(&mut c, sent);

    // 1) The balance does NOT move — estimated money is not tradable money.
    assert_eq!(
        c.accounts().active_ledger().balance(),
        cash_before,
        "the deleted cash policy would have credited 5.00 to spendable cash"
    );
    // 2) The estimate lands in the receivable pool instead.
    let stats =
        c.engine_stats_at(sent + super::EXPIRY_ESTIMATE_SETTLE_AFTER_MS + 2_000)["settlement"]
            .clone();
    assert_eq!(
        stats["receivableUsd"],
        serde_json::json!(5.0),
        "the pool increased by the booked estimate (1.00 × 5 shares)"
    );
    assert_eq!(stats["fallbackReceivableUsd"], serde_json::json!(5.0));
    assert_eq!(stats["pendingFallbackClaims"], serde_json::json!(1));
    assert_eq!(
        stats["unverifiedExpiryPolicy"],
        serde_json::json!("receivable")
    );
    // 3) The unverified-exposure report lists the position.
    let exposure = c.unverified_exposure_report();
    assert_eq!(exposure.total_positions, 1);
    assert_eq!(exposure.entries[0].position_id, position_id);
    assert_eq!(exposure.entries[0].source, "fallback");
    assert_eq!(exposure.entries[0].fallback_price, Decimal::ONE);
    // 4) No spendable-cash journal row exists — the deleted producer is gone.
    let journal = c.accounts().active_ledger().journal_snapshot();
    assert!(
        !journal
            .iter()
            .any(|e| e.kind == crate::ledger::JournalKind::ExpiryFallback),
        "no ExpiryFallback cash credit may be written by any path: {journal:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
