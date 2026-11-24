#![cfg(test)]
//! issue #177 — the read-only surface is DERIVED from a classification table,
//! and this file is the falsification of both defects the issue names:
//!
//! 1. **The gate**: `READONLY_REFUSED` was 27 hand-registered configuration
//!    verbs, so `orders.place`, `positions.exit`, `risk.kill`,
//!    `account.freeze`, `strategy.enable`, … walked straight through. The
//!    replacement refuses on `MethodKind != Read`. The tests below iterate the
//!    shipped table — never a baked-in count — so a verb added tomorrow is
//!    covered the moment it is classified, and a verb the dispatch serves
//!    without a spec FAILS the parity test instead of penetrating silently.
//! 2. **The settle point**: `risk_admission/admission.rs` shared
//!    `Mode::Dry | Mode::ReadOnly`, so a read-only core that received
//!    `orders.place` produced simulated fills and positions. Both layers are
//!    asserted here: at the wire (balance and position book unchanged after
//!    `orders.place` + `books.snapshot`) and at the kernel (`Core::place`
//!    refuses with `ReadOnlyNoExecution` before registering anything, which is
//!    the path an engine-driven intent takes — it never passes the IPC gate).
//!
//! Dry is the control: `dry_still_simulates_and_opens_a_position` proves the
//! tightening landed on read-only only, because "read-only refuses" is
//! worthless if it was bought by weakening the simulation.

use super::*;

use crate::model::{
    CoreErrorCode, FillPolicy, OrderRequest, OrderStatus, Side, default_account_id,
};
use crate::risk::RiskConfig;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{BTreeMap, BTreeSet};

use crate::ipc::server::method_spec::{self, MethodKind};

/// A bare read-only core: no durable files (nothing this test writes can leak
/// into `data/`), auto-exits off (no tick-driven orders), a per-order cap wide
/// enough that a refusal could only come from the read-only guard.
fn read_only_core() -> Core {
    Core::new(CoreConfig {
        mode: Mode::ReadOnly,
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
    })
}

fn dry_core() -> Core {
    Core::new(CoreConfig {
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
    })
}

fn buy_taker(price: Decimal, size: Decimal, key: &str) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price,
        size,
        internal_key: key.into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: default_account_id(),
    }
}

fn balance_of(c: &Core) -> Decimal {
    c.accounts()
        .ledger_of(&default_account_id())
        .expect("the default account's book exists")
        .balance()
}

fn reserved_of(c: &Core) -> Decimal {
    c.accounts()
        .ledger_of(&default_account_id())
        .expect("the default account's book exists")
        .reserved()
}

/// One `orders.place` at the wire, as a client would send it.
fn place_line(id: u32) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"orders.place","params":{{
            "tokenId":"tok","conditionId":"cond","side":"buy","mode":"taker",
            "price":0.40,"size":10,"internalKey":"k-ro","strategy":"spread_arb",
            "asset":"BTC","direction":"up","roundSlot":1
        }}}}"#
    )
}

/// A book that WOULD fill the taker above: an ask resting at its limit.
fn snapshot_line(id: u32) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"books.snapshot","params":{{
            "tokenId":"tok","bids":[],"asks":[{{"price":0.40,"size":500}}]
        }}}}"#
    )
}

/// **The core falsification of defect 1.** Every verb the table classifies as
/// anything other than `Read` is sent to a read-only core and must be refused
/// by the gate — the refusal names the verb, so a nested handler error cannot
/// masquerade as coverage.
///
/// The loop is over [`method_spec::METHODS`]; the count is never written down.
/// A verb classified tomorrow is asserted tomorrow.
#[tokio::test]
async fn every_non_read_verb_is_refused_in_read_only_mode() {
    let core = Arc::new(AsyncMutex::new(read_only_core()));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    let mut checked = 0usize;
    for spec in method_spec::METHODS
        .iter()
        .filter(|s| s.kind != MethodKind::Read)
    {
        let reply = rpc(
            &core,
            &registry,
            &peer,
            format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"{}","params":{{}}}}"#,
                spec.name
            ),
        )
        .await;
        assert_eq!(
            reply["error"]["code"],
            serde_json::json!(Failure::APPLICATION),
            "read-only must refuse the {:?} verb {}: {reply}",
            spec.kind,
            spec.name
        );
        let message = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("read-only mode"),
            "the refusal must name the mode for {}: {message}",
            spec.name
        );
        assert!(
            message.contains(spec.name),
            "the refusal must name the verb it refused ({}): {message}",
            spec.name
        );
        checked += 1;
    }

    // Not a hardcoded expectation of the table's size — a floor that the loop
    // actually ran and that the shipped surface is bigger than one verb.
    assert!(
        checked > 1,
        "the table must classify more than one non-Read verb, saw {checked}"
    );
}

/// The other half of the contract: `Read` is always served. A read-only core
/// exists to be observed, so the gate must never answer for an observation
/// verb. A handler is allowed to fail on its own terms (an unknown job id, a
/// file that is not there) — what is forbidden is the GATE's refusal.
#[tokio::test]
async fn read_verbs_are_never_refused_by_the_read_only_gate() {
    let core = Arc::new(AsyncMutex::new(read_only_core()));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    for spec in method_spec::METHODS
        .iter()
        .filter(|s| s.kind == MethodKind::Read)
    {
        let reply = rpc(
            &core,
            &registry,
            &peer,
            format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"{}","params":{{}}}}"#,
                spec.name
            ),
        )
        .await;
        let message = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(
            !message.contains("read-only mode"),
            "a Read verb must not be refused by the read-only gate: {} → {reply}",
            spec.name
        );
    }
}

/// The exact verbs the issue lists as penetrating the old whitelist. The loop
/// above is the coverage mechanism; this pins the historical hole as a named
/// regression, so a future reclassification of one of them to `Read` fails
/// with its own name instead of hiding inside a table.
#[tokio::test]
async fn the_verbs_named_by_the_issue_are_classified_and_refused() {
    const NAMED: [&str; 13] = [
        "orders.place",
        "orders.cancel",
        "orders.cancel_all",
        "orders.force_close",
        "positions.exit",
        "positions.force_close",
        "account.freeze",
        "account.unfreeze",
        "account.strategies.set",
        "strategy.enable",
        "risk.kill",
        "risk.resume",
        "market.credentials.set",
    ];

    let core = Arc::new(AsyncMutex::new(read_only_core()));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    for verb in NAMED {
        let spec = method_spec::get(verb)
            .unwrap_or_else(|| panic!("{verb} must be classified — #177 named it as a hole"));
        assert_ne!(
            spec.kind,
            MethodKind::Read,
            "{verb} moves state and must not be classified Read"
        );
        let reply = rpc(
            &core,
            &registry,
            &peer,
            format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{verb}","params":{{}}}}"#),
        )
        .await;
        assert_eq!(
            reply["error"]["code"],
            serde_json::json!(Failure::APPLICATION),
            "read-only must refuse {verb}: {reply}"
        );
    }
}

/// **Structural property 3** from the module docs: the table cannot silently
/// fall behind the dispatch. A verb the dispatch match serves but the table
/// omits would fall through the gate (the match would serve it), so omission
/// must break a test rather than penetrate.
///
/// Parses the two sources of truth — the `schema::method` constants and the
/// dispatch arms in `server/mod.rs` — and asserts a bijection with the table.
#[test]
fn every_dispatched_method_has_exactly_one_spec() {
    // 1. `schema::method`: CONST_NAME -> wire verb.
    let declared_src = include_str!("../../../schema/methods.rs");
    let mut declared: BTreeMap<String, String> = BTreeMap::new();
    for line in declared_src.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("pub const ") else {
            continue;
        };
        let Some((name, value)) = rest.split_once(": &str = ") else {
            continue;
        };
        let value = value.trim().trim_end_matches(';').trim();
        let verb = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or_else(|| panic!("constant {name} is not a string literal: {line}"));
        declared.insert(name.trim().to_string(), verb.to_string());
    }
    assert!(
        declared.len() > 1,
        "the method-constant parse found {} entries — the source shape moved",
        declared.len()
    );

    // 2. The dispatch match in `server/mod.rs`: one arm per constant.
    let dispatch_src = include_str!("../mod.rs");
    let mut dispatched: BTreeSet<String> = BTreeSet::new();
    for line in dispatch_src.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("method::") else {
            continue;
        };
        let Some((name, tail)) = rest.split_once(" =>") else {
            continue;
        };
        // Only arm positions: the name must be a bare constant.
        if !name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
        {
            continue;
        }
        assert!(
            tail.contains("handler")
                || tail.starts_with(" {")
                || tail.trim_start().starts_with('{'),
            "unexpected dispatch arm shape for {name}: {line}"
        );
        dispatched.insert(name.to_string());
    }
    assert!(
        dispatched.len() > 1,
        "the dispatch parse found {} arms — the match shape moved",
        dispatched.len()
    );

    // 3. Every dispatched arm resolves to a declared verb …
    let served: BTreeSet<&str> = dispatched
        .iter()
        .map(|name| {
            declared
                .get(name)
                .map(String::as_str)
                .unwrap_or_else(|| panic!("dispatch serves method::{name} with no declaration"))
        })
        .collect();

    // 4. … and every served verb is classified.
    let mut missing: Vec<&str> = served
        .iter()
        .copied()
        .filter(|verb| method_spec::get(verb).is_none())
        .collect();
    missing.sort_unstable();
    assert!(
        missing.is_empty(),
        "the dispatch serves verbs with no MethodSpec (they would penetrate the \
         read-only gate): {missing:?}"
    );

    // 5. … and the table carries nothing the dispatch does not serve, so a
    // stale entry cannot hide (it would refuse in read-only while answering
    // METHOD_NOT_FOUND elsewhere).
    let mut stale: Vec<&str> = method_spec::METHODS
        .iter()
        .map(|s| s.name)
        .filter(|name| !served.contains(name))
        .collect();
    stale.sort_unstable();
    assert!(
        stale.is_empty(),
        "the table classifies verbs the dispatch does not serve: {stale:?}"
    );
    assert_eq!(
        method_spec::METHODS.len(),
        served.len(),
        "one spec per served verb"
    );
}

/// **Defect 2 at the kernel.** `Core::place` is what an engine-driven intent
/// calls — it never passes the IPC gate — so this is the assertion that makes
/// "a read-only core cannot execute" a kernel fact rather than a wire policy.
///
/// The refusal happens BEFORE submission, which is why nothing is registered:
/// an order row would be a persistent write (the shared order log) in the one
/// mode that must not write.
#[test]
fn read_only_refuses_execution_before_registering_anything() {
    let mut c = read_only_core();
    let seeded = balance_of(&c);
    assert_eq!(seeded, dec!(1000), "the boot seed is the read-only book");

    let err = c
        .place(buy_taker(dec!(0.40), dec!(10), "k-ro"), 0, 1_000)
        .expect_err("a read-only core must not accept an order");
    assert_eq!(err.code, CoreErrorCode::ReadOnlyNoExecution);
    assert!(
        err.message.contains("read-only"),
        "the refusal names the mode: {}",
        err.message
    );

    assert!(
        c.ome().all().is_empty(),
        "nothing may be registered: a row in the order log is a write"
    );
    assert!(c.positions().open_positions().is_empty());
    assert_eq!(balance_of(&c), seeded, "no cash moved");
    assert_eq!(reserved_of(&c), Decimal::ZERO, "no reservation stranded");
}

/// **The issue's first mandated scenario**: `orders.place` + `books.snapshot`
/// on a read-only core leaves the position book empty.
///
/// Today the ingest verb is refused too (module docs, `method_spec.rs`), so
/// the book never lands; the assertion is written against the OUTCOME, so it
/// keeps its meaning if a future ingest verb is relaxed.
#[tokio::test]
async fn readonly_core_never_creates_positions() {
    let core = Arc::new(AsyncMutex::new(read_only_core()));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    // The exact sequence #177 describes: a client asks for a fill.
    let place = rpc(&core, &registry, &peer, place_line(1)).await;
    assert!(
        place.get("error").is_some(),
        "read-only must not answer orders.place with a result: {place}"
    );
    let snap = rpc(&core, &registry, &peer, snapshot_line(2)).await;
    assert!(
        snap.get("error").is_some(),
        "read-only must not accept a book ingest: {snap}"
    );

    let listed = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"positions.list","params":{}}"#.to_string(),
    )
    .await;
    assert!(
        listed.get("error").is_none(),
        "positions.list is a Read and must answer: {listed}"
    );
    assert_eq!(
        listed["result"]["positions"].as_array().map(Vec::len),
        Some(0),
        "a read-only core holds no position: {listed}"
    );

    // The order book is empty as well: a REJECTED row would still be a write.
    let orders = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"orders.list","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        orders["result"]["orders"].as_array().map(Vec::len),
        Some(0),
        "a read-only core registers no order row: {orders}"
    );
}

/// **The issue's second mandated scenario**: `orders.place` + a simulated fill
/// leaves `ledger.balance` untouched.
#[tokio::test]
async fn readonly_core_never_credits_balance() {
    let core = Arc::new(AsyncMutex::new(read_only_core()));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    let before = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"ledger.balance","params":{}}"#.to_string(),
    )
    .await;
    let seeded = before["result"]["balance"]
        .as_f64()
        .expect("ledger.balance answers a number");

    // The fill attempt: the order, then the crossing book that would have
    // produced it under the bug.
    let _ = rpc(&core, &registry, &peer, place_line(2)).await;
    let _ = rpc(&core, &registry, &peer, snapshot_line(3)).await;

    let after = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"ledger.balance","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        after["result"]["balance"].as_f64(),
        Some(seeded),
        "read-only books no fill, so the balance cannot move: {after}"
    );
    assert_eq!(
        after["result"]["reserved"].as_f64(),
        Some(0.0),
        "no reservation may be left behind: {after}"
    );
}

/// The control: Dry still simulates. If the read-only tightening had been
/// achieved by removing the settle path, this fails — which is the point.
#[test]
fn dry_still_simulates_and_opens_a_position() {
    let mut c = dry_core();
    let seeded = balance_of(&c);

    // A book the taker can cross; then the order that crosses it.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(500))], 900);
    let (id, status) = c
        .place(buy_taker(dec!(0.40), dec!(10), "k-dry"), 0, 1_000)
        .expect("Dry must still accept an order");
    assert_eq!(status, OrderStatus::Filled, "Dry fills the taker");
    assert_eq!(
        c.ome().get(&id).map(|o| o.status),
        Some(OrderStatus::Filled)
    );
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "Dry books the position"
    );
    assert!(
        balance_of(&c) < seeded,
        "Dry spends cash on the fill ({} → {})",
        seeded,
        balance_of(&c)
    );
    assert_eq!(
        reserved_of(&c),
        Decimal::ZERO,
        "a filled BUY holds no reserve"
    );
}
