#![cfg(test)]
//! ── #175: venue-trade attribution ───────────────────────────────────────────
//!
//! The defect: `VenueTrade` carried no account, so the external-fill fold
//! matched positions by TOKEN alone and hard-coded `default_account_id()` for
//! unknown BUYs. The engine deliberately keys `pending_tokens` on
//! `(account, token)`, so two accounts holding one token is a SUPPORTED state —
//! and in that state account B's manual venue sell closed account A's position,
//! booked A the realized gain, and left B's cash gone with B's shares still on
//! the book. Not a reconciliation difference: money moved between books that
//! had never met.
//!
//! These are the falsification tests. Each drives the pre-fix counterexample
//! through the REAL production path and asserts the books do NOT move the way
//! the bug moved them. The contract pinned here:
//!
//!   * `VenueTrade::account` is the only attribution source;
//!   * a trade with no account, or an ambiguous/unmatched position, is PARKED
//!     — a first-class state on `positions.unverified_exposure`, announced by a
//!     `RiskAlert` and recorded in the audit log — never guessed into `default`;
//!   * `apply_external_fills` returns one `FoldVerdict` per trade.

use super::*;
use rust_decimal_macros::dec;

const SEED: Decimal = dec!(10_000);
const TOKEN: &str = "tok";

fn account(id: &str) -> crate::account::Account {
    crate::account::Account {
        id: crate::account::AccountId::from(id),
        name: id.to_string(),
        market_type: blitzkrieg_market_api::MarketType::Prediction,
        status: crate::account::AccountStatus::Active,
        credential_keys: crate::account::CredentialKeys::default(),
        venue: String::new(),
        trading_enabled: true,
        strategies: Vec::new(),
        rounds: Vec::new(),
        opening_equity: None,
        updated_at_ms: 0,
    }
}

/// Two ACTIVE books (`default` + `paper`) on one core, both seeded. No durable
/// logs: every fixture that trades carries none (same contract as the audit
/// fixtures — parallel tests share the process cwd).
fn two_book_core(audit_log_path: Option<String>) -> Core {
    let book = crate::account::AccountLedgers::new(
        vec![account("default"), account("paper")],
        crate::account::AccountId::from("default"),
    );
    let mut cfg = CoreConfig {
        risk: RiskConfig {
            // 2000 shares @ 0.40 = 800 notional per account.
            max_order_notional: dec!(10_000),
            ..Default::default()
        },
        dry_seed_balance: SEED,
        auto_exits_enabled: false,
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    cfg.audit_log_path = audit_log_path;
    Core::with_account_book(cfg, book)
}

fn order_for(account: &str, key: &str, price: Decimal, size: Decimal) -> OrderRequest {
    OrderRequest {
        token_id: TOKEN.into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Maker,
        price,
        size,
        internal_key: key.into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::account::AccountId::from(account),
    }
}

fn fill_for(order_id: &str, trade: &str, account: &str, price: Decimal, size: Decimal) -> Fill {
    Fill {
        order_id: order_id.into(),
        trade_id: Some(trade.into()),
        token_id: TOKEN.into(),
        side: Side::Buy,
        price,
        size,
        status: FillStatus::Confirmed,
        ts_ms: 0,
        tx_hash: None,
        maker: None,
        account_id: crate::account::AccountId::from(account),
    }
}

/// Open a maker position of `size` shares at `price` in `account` through the
/// REAL live path (register → venue ack → user-WS fill). The trade id is
/// per-account: the OME's applied-trade table is keyed by trade id alone, so a
/// shared id would dedup the second book's identical fill.
fn open_in(c: &mut Core, account: &str, price: Decimal, size: Decimal, now: i64) {
    let (id, _) = c
        .place_pending(order_for(account, "entry", price, size), now)
        .unwrap();
    c.confirm_live(&id, now).unwrap();
    let trade = format!("entry-{account}");
    c.ingest_fill(fill_for(&id, &trade, account, price, size), now + 1)
        .unwrap();
}

/// A venue trade row, spelled out by NAME. The eight fields used to be
/// positional arguments, and a positional slip (a `maker: false` landing in the
/// wrong slot) is exactly how a test can silently assert a fee that was never
/// charged. Named fields make each row self-documenting.
struct VenueTradeSpec<'a> {
    venue_order_id: &'a str,
    trade_id: &'a str,
    side: Side,
    size: Decimal,
    price: Decimal,
    ts_ms: i64,
    maker: bool,
    account: Option<&'a str>,
}

impl VenueTradeSpec<'_> {
    fn build(&self) -> crate::reconcile::VenueTrade {
        crate::reconcile::VenueTrade {
            venue_order_id: self.venue_order_id.into(),
            trade_id: self.trade_id.into(),
            token_id: TOKEN.into(),
            side: self.side,
            size: self.size,
            price: self.price,
            ts_ms: self.ts_ms,
            tx_hash: None,
            maker: Some(self.maker),
            account: self.account.map(crate::account::AccountId::from),
        }
    }
}

fn balance_of(c: &Core, account: &str) -> Decimal {
    c.accounts()
        .ledger_of(&crate::account::AccountId::from(account))
        .expect("the book exists")
        .balance()
}

fn reserved_of(c: &Core, account: &str) -> Decimal {
    c.accounts()
        .ledger_of(&crate::account::AccountId::from(account))
        .expect("the book exists")
        .reserved()
}

/// The identity a position must keep bit-for-bit when a sibling account's
/// trade is folded: id, shares, basis, entry price, entry clock.
type PositionSnapshot = (String, Decimal, Decimal, Decimal, i64);

fn open_snapshot(c: &Core, account: &str) -> Option<PositionSnapshot> {
    c.positions()
        .open_positions()
        .iter()
        .find(|p| p.account_id.as_str() == account)
        .map(|p| {
            (
                p.id.clone(),
                p.shares,
                p.cost_usd,
                p.entry_price,
                p.entered_at_ms,
            )
        })
}

// ── 1. the A/B 2000-share disaster ──────────────────────────────────────────

/// A and B each hold 2000 shares of ONE token (the engine allows it: positions
/// are keyed `(account, token)`). ONE venue SELL belongs to B. Before the fix
/// the token-only match closed A's position and booked A +1,800 realized while
/// B's cash was gone and B's shares stayed. Now the sell folds into B alone:
/// A's position AND cash are bit-for-bit unchanged, B closes correctly.
#[test]
fn external_sell_closes_only_its_own_accounts_position() {
    let mut c = two_book_core(None);
    let now: i64 = 1_000_000;

    open_in(&mut c, "default", dec!(0.40), dec!(2000), now);
    open_in(&mut c, "paper", dec!(0.40), dec!(2000), now + 10);

    let a_before = open_snapshot(&c, "default").expect("A's position is on the book");
    let b_before = open_snapshot(&c, "paper").expect("B's position is on the book");
    let a_cash_before = balance_of(&c, "default");
    let a_reserved_before = reserved_of(&c, "default");
    let b_cash_before = balance_of(&c, "paper");
    assert_eq!(a_cash_before, SEED - dec!(800));
    assert_eq!(b_cash_before, SEED - dec!(800));
    assert_eq!(a_before.1, dec!(2000));
    assert_eq!(b_before.1, dec!(2000));

    // ONE venue SELL, belonging to B (a manual close made on the venue).
    c.reconcile_for_venue(
        crate::reconcile::VenueSnapshot {
            open_order_ids: vec![],
            trades: vec![
                VenueTradeSpec {
                    venue_order_id: "0xmanual",
                    trade_id: "t-manual",
                    side: Side::Sell,
                    size: dec!(2000),
                    price: dec!(0.60),
                    ts_ms: now + 100,
                    maker: true,
                    account: Some("paper"),
                }
                .build(),
            ],
            now_ms: now + 200,
        },
        "polymarket",
    )
    .unwrap();

    // A: the position did not move a digit, and neither did its cash.
    assert_eq!(
        open_snapshot(&c, "default"),
        Some(a_before),
        "A's position must be bit-for-bit unchanged: the sell belongs to B"
    );
    assert_eq!(
        balance_of(&c, "default"),
        a_cash_before,
        "A's cash must not move: booking B's proceeds into A IS the cross-account defect"
    );
    assert_eq!(reserved_of(&c, "default"), a_reserved_before);
    assert_eq!(
        c.positions().daily_pnl_for("default"),
        Decimal::ZERO,
        "no realized gain may appear in A's day"
    );
    assert!(
        c.positions()
            .closed_positions()
            .iter()
            .all(|p| p.account_id.as_str() != "default"),
        "A must have no closed row at all"
    );

    // B: correctly closed — shares gone, proceeds in B's own book.
    assert!(
        c.positions()
            .open_positions()
            .iter()
            .all(|p| p.account_id.as_str() != "paper"),
        "B's position is closed"
    );
    let closed: Vec<_> = c
        .positions()
        .closed_positions()
        .iter()
        .filter(|p| p.account_id.as_str() == "paper")
        .collect();
    assert_eq!(closed.len(), 1, "exactly one close, in B's book");
    assert_eq!(closed[0].id, b_before.0);
    assert_eq!(closed[0].shares, dec!(2000));
    assert_eq!(closed[0].exit_reason, ExitReason::Manual);
    // Maker exit: no fee. net = proceeds 1200 − cost 800.
    assert_eq!(closed[0].net_pnl_usd, dec!(400));
    assert_eq!(
        balance_of(&c, "paper"),
        b_cash_before + dec!(1200),
        "B received exactly the proceeds of its own sale"
    );
    assert_eq!(
        c.positions().daily_pnl_for("paper"),
        dec!(400),
        "the realized gain lands in B's day, not a sibling's"
    );
    assert_eq!(c.positions().closed_positions().len(), 1);
}

// ── 2. an unknown BUY never lands on `default` ──────────────────────────────

/// A place-timeout BUY on `paper` that the venue really filled arrives as a
/// trade row with NO account claim. The pre-fix fold hard-coded
/// `default_account_id()` and rebuilt the position in `default` — the live-WS
/// half of the same defect. Now the row parks (`missing_account`), nothing is
/// booked in either book, and the SAME trade with its account named folds into
/// `paper` alone.
#[test]
fn unknown_buy_never_lands_on_default_account() {
    let mut c = two_book_core(None);
    let now: i64 = 1_000_000;
    // The fold needs the token's market metadata (strategy/condition/slot) —
    // the place-timeout BUY carries none itself.
    c.enable_engine(crate::engine::Engine::new(
        crate::engine::EngineConfig::default(),
    ));
    let slot = now / 1000 / 900;
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![crate::model::CryptoMarket {
                asset: "BTC".into(),
                condition_id: "cond".into(),
                question_id: "q".into(),
                up_token_id: TOKEN.into(),
                down_token_id: "tok-down".into(),
                up_price: dec!(0.6),
                down_price: dec!(0.4),
                expires_at_ms: (slot + 1) * 900 * 1000,
                round_slot: slot,
                round_duration_sec: 900,
                archive_verdict: false,
                venue: String::new(),
                neg_risk: false,
                question: "?".into(),
            }],
            now_ms: now,
        },
        now,
    );

    // The place-timeout order: registered, never acked, never given an id.
    let (id, _) = c
        .place_pending(order_for("paper", "timed-out", dec!(0.40), dec!(10)), now)
        .unwrap();
    c.note_place_timeout(&id);
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Pending);
    let paper_before = balance_of(&c, "paper");
    let default_before = balance_of(&c, "default");

    // The venue trade the sweep found for that timed-out place: no account.
    c.reconcile(crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![
            VenueTradeSpec {
                venue_order_id: "venue-unknown-1",
                trade_id: "t-ext-1",
                side: Side::Buy,
                size: dec!(5),
                price: dec!(0.45),
                ts_ms: now + 1,
                // The venue reported a maker fill: no fee.
                maker: true,
                account: None,
            }
            .build(),
        ],
        now_ms: now + 2,
    })
    .unwrap();

    // Parked, NOT booked anywhere — least of all `default`.
    assert!(
        c.positions()
            .open_positions()
            .iter()
            .all(|p| p.account_id.as_str() != "default"),
        "an unattributed BUY must not rebuild a position in `default`"
    );
    assert!(
        c.positions()
            .open_positions()
            .iter()
            .all(|p| p.account_id.as_str() != "paper"),
        "and it must not be guessed into any other book either — it has no owner yet"
    );
    assert_eq!(balance_of(&c, "default"), default_before);
    assert_eq!(balance_of(&c, "paper"), paper_before);
    assert_eq!(c.positions().closed_positions().len(), 0);
    let report = c.unverified_exposure_report();
    assert_eq!(
        report.parked.len(),
        1,
        "the refused fill is a first-class row on the unverified-exposure report"
    );
    assert_eq!(report.parked[0].reason, "missing_account");
    assert_eq!(report.parked[0].account_id, None);
    assert_eq!(report.parked[0].trade_id, "t-ext-1");

    // The same trade WITH its account named folds into `paper` — and only
    // there. (Newer ts: the watermark already absorbed the parked row.)
    c.reconcile(crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![
            VenueTradeSpec {
                venue_order_id: "venue-unknown-1",
                trade_id: "t-ext-1-fixed",
                side: Side::Buy,
                size: dec!(5),
                price: dec!(0.45),
                ts_ms: now + 3,
                // Taker: the fold must charge the schedule's fee in B's own book.
                maker: false,
                account: Some("paper"),
            }
            .build(),
        ],
        // Past the place-timeout budget: the sweep also retires the never-acked
        // order and releases its reservation.
        now_ms: now + PLACE_TIMEOUT_VERDICT_MS + 10,
    })
    .unwrap();

    let rebuilt: Vec<_> = c
        .positions()
        .open_positions()
        .iter()
        .filter(|p| p.token_id == TOKEN)
        .collect();
    assert_eq!(rebuilt.len(), 1, "the attributed fill rebuilt ONE position");
    assert_eq!(
        rebuilt[0].account_id.as_str(),
        "paper",
        "in the account the trade named — never `default`"
    );
    assert_eq!(rebuilt[0].shares, dec!(5));
    assert_eq!(
        balance_of(&c, "default"),
        default_before,
        "`default` pays nothing toward a sibling account's fill"
    );
    let fee = (crate::exit_policy::taker_fee_pct(dec!(0.45)) / dec!(100)) * dec!(2.25);
    assert_eq!(
        balance_of(&c, "paper"),
        paper_before - dec!(2.25) - fee,
        "the cash the venue spent leaves B's book (notional + its taker fee)"
    );
    assert_eq!(
        reserved_of(&c, "paper"),
        Decimal::ZERO,
        "the retired place released its reservation"
    );
    assert_eq!(
        c.ome().get(&id).unwrap().status,
        OrderStatus::Cancelled,
        "the never-acked place is retired by the sweep"
    );
}

// ── 3. no account on the trade = rejected verdict, alert, report, audit ──────

/// A venue SELL with NO account claim, against a book that holds the token.
/// The fold must refuse it as a VERDICT — `Parked(MissingAccount)` — and the
/// refusal must be visible: `RiskAlert`, an `unattributed_fill` audit row and a
/// row on `positions.unverified_exposure`. Nothing is booked, nothing is
/// guessed, and a re-report does not page twice.
#[test]
fn venue_trade_without_account_is_rejected_not_folded() {
    let dir = std::env::temp_dir().join(format!(
        "bk-175-attribution-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let audit_path = dir.join("audit.jsonl").to_string_lossy().into_owned();

    let mut c = two_book_core(Some(audit_path.clone()));
    let now: i64 = 1_000_000;
    open_in(&mut c, "default", dec!(0.40), dec!(10), now);
    let cash_before = balance_of(&c, "default");
    let pos_before = c.positions().open_positions()[0].clone();
    assert_eq!(pos_before.shares, dec!(10));

    let trade = VenueTradeSpec {
        venue_order_id: "0xmanual",
        trade_id: "t-no-account",
        side: Side::Sell,
        size: dec!(10),
        price: dec!(0.40),
        ts_ms: now + 100,
        maker: true,
        account: None,
    }
    .build();
    // The fold's public contract: one VERDICT per trade, and this one says no.
    let verdicts = c.apply_external_fills(std::slice::from_ref(&trade), "polymarket", now + 101);
    assert_eq!(verdicts.len(), 1);
    match &verdicts[0] {
        crate::reconcile::FoldVerdict::Parked(p) => {
            assert_eq!(
                p.reason,
                crate::reconcile::UnattributedReason::MissingAccount
            );
            assert_eq!(p.account, None, "no owner may be invented");
            assert_eq!(p.venue, "polymarket");
            assert_eq!(p.trade_id, "t-no-account");
            assert_eq!(p.size, dec!(10));
            assert_eq!(p.price, dec!(0.40));
        }
        crate::reconcile::FoldVerdict::Folded { account, .. } => {
            panic!(
                "a no-account trade was FOLDED into `{}` — the #175 defect",
                account.as_str()
            )
        }
    }
    // Nothing moved: not the shares, not the cash.
    assert_eq!(c.positions().open_positions().len(), 1);
    assert_eq!(c.positions().open_positions()[0].shares, pos_before.shares);
    assert_eq!(
        c.positions().open_positions()[0].cost_usd,
        pos_before.cost_usd
    );
    assert_eq!(balance_of(&c, "default"), cash_before);
    assert_eq!(c.positions().closed_positions().len(), 0);

    // The verdict becomes the page and the record.
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    c.report_parked_fills(&verdicts, now + 101);

    let mut alerts = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            alerts.push(message);
        }
    }
    assert_eq!(alerts.len(), 1, "exactly one page for one refusal");
    assert!(
        alerts[0].contains("unattributed_fill reason=missing_account"),
        "the alert must be greppable and name the rule: {}",
        alerts[0]
    );
    assert!(
        alerts[0].contains("nothing booked"),
        "the alert must state that no money moved: {}",
        alerts[0]
    );

    let report = c.unverified_exposure_report();
    assert_eq!(report.parked.len(), 1);
    let row = &report.parked[0];
    assert_eq!(row.venue, "polymarket");
    assert_eq!(row.venue_order_id, "0xmanual");
    assert_eq!(row.trade_id, "t-no-account");
    assert_eq!(row.token_id, TOKEN);
    assert_eq!(row.account_id, None);
    assert_eq!(row.side, Side::Sell);
    assert_eq!(row.shares, dec!(10));
    assert_eq!(row.price, dec!(0.40));
    assert_eq!(row.notional_usd, dec!(4));
    assert_eq!(row.reason, "missing_account");
    // A parked fill is NOT a position: it takes no share of the position
    // aggregates and books nothing into realized PnL.
    assert!(report.entries.is_empty());
    assert_eq!(report.total_positions, 0);
    assert_eq!(report.total_cost_usd, Decimal::ZERO);
    assert_eq!(report.parked_total_notional_usd, dec!(4));

    // One audit row on disk — the durable half of the same fact.
    let audit_rows = |path: &str| -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some("unattributed_fill"))
            .collect()
    };
    let rows = audit_rows(&audit_path);
    assert_eq!(rows.len(), 1, "one unattributed_fill audit row");
    assert_eq!(rows[0]["reason"], "missing_account");
    assert_eq!(rows[0]["count"], 1);
    assert_eq!(rows[0]["fills"][0]["tradeId"], "t-no-account");
    assert_eq!(rows[0]["fills"][0]["account"], serde_json::Value::Null);

    // A re-delivery of the same trade is absorbed by the watermark: no second
    // verdict, no second alert, no duplicate report row (one unknown must not
    // become a permanent alert storm).
    let again = c.apply_external_fills(std::slice::from_ref(&trade), "polymarket", now + 300);
    assert!(again.is_empty(), "a re-delivered trade is not re-folded");
    c.report_parked_fills(&again, now + 300);
    let mut late = 0;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, Event::RiskAlert { .. }) {
            late += 1;
        }
    }
    assert_eq!(late, 0, "no alert storm: the unknown is reported once");
    assert_eq!(c.unverified_exposure_report().parked.len(), 1);
    assert_eq!(audit_rows(&audit_path).len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}
