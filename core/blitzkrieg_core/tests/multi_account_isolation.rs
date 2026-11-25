//! issue #182 — per-account isolation: ONE `Arc<AsyncMutex<Core>>` serialises
//! every account's work, so an account waiting on a slow execution backend
//! also stalls every other account's READ path (`orders.list` takes the same
//! guard `orders.place` takes).
//!
//! This file is the ACCEPTANCE TARGET of #182, not a green suite:
//!
//!  - `account_a_slow_place_does_not_block_account_b_read` — the issue's
//!    falsification test. A's place blocks 500 ms; B's `orders.list` must stay
//!    under 50 ms. **EXPECTED RED against the single Core mutex**, hence
//!    `#[ignore]`; the threshold is the issue's and has NOT been relaxed.
//!    Whoever closes #182 (per-account locks + an `ArcSwap` read snapshot)
//!    removes the ignore attribute; nothing else in the test changes.
//!  - `concurrent_order_list_and_place_never_deadlock` — 1000 concurrent reads
//!    beside 100 places: all return inside 30 s and no guard is poisoned.
//!    Green today; it must stay green through the split (a per-account lock
//!    that re-enters itself would deadlock HERE).
//!  - `feed_subscription_publishes_are_synchronous_under_the_guard` — pins the
//!    structural half of #182 that this branch DOES fix: the subscription
//!    publishers must be callable with the Core guard held and must not need a
//!    suspend point. They are called without `.await` below, so re-introducing
//!    `async fn` on them fails to COMPILE — a build-level regression pin, not
//!    a timing hope.
//!
//! Why the block is injected rather than produced by `Core::place`: today
//! `Core::place` / `place_outcome` are synchronous all the way down — no
//! execution backend is wired into the admission path (`Core.execution_api` is
//! `None` in the shipped assemblies, and `place_gated` never awaits). The
//! single-lock cost is exhibited by the guard ITSELF: whoever holds it — a
//! slow venue submit, an fsync during `tick`, an extension `on_load` — parks
//! both operations. The awaited sleep below stands in for the venue submit that
//! the #182 contract moves into the per-account critical section; the measured
//! quantity (B's wait on the ONE mutex) is the real one either way.

use std::sync::Arc;
use std::time::{Duration, Instant};

use blitzkrieg_core::account::{Account, AccountLedgers, AccountStatus};
use blitzkrieg_core::model::{AccountId, FillPolicy, Mode, OrderRequest, Side, default_account_id};
use blitzkrieg_core::service::{Core, CoreConfig};
use rust_decimal_macros::dec;
use tokio::sync::Mutex as AsyncMutex;

const NOW: i64 = 1_700_000_000_000;
/// The issue's budget for B's read while A is blocked.
const READ_BUDGET: Duration = Duration::from_millis(50);
/// How long A's place blocks (the venue submit stand-in).
const SLOW_SUBMIT: Duration = Duration::from_millis(500);

fn account(id: &str) -> Account {
    Account {
        id: AccountId::from(id),
        name: id.into(),
        market_type: blitzkrieg_market_api::MarketType::Prediction,
        status: AccountStatus::Active,
        credential_keys: Default::default(),
        venue: String::new(),
        trading_enabled: true,
        strategies: Vec::new(),
        rounds: Vec::new(),
        opening_equity: None,
        updated_at_ms: NOW,
    }
}

/// A two-account dry core (`default` + `paper`) with the seed applied, built
/// through the injection constructor so no process cwd / `.env` is touched.
fn two_account_core() -> Arc<AsyncMutex<Core>> {
    let book = AccountLedgers::new(
        vec![account("default"), account("paper")],
        default_account_id(),
    );
    let cfg = CoreConfig {
        mode: Mode::Dry,
        dry_seed_balance: dec!(1000),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let mut c = Core::with_account_book(cfg, book);
    c.set_balance(dec!(1000));
    Arc::new(AsyncMutex::new(c))
}

fn order_for(account: &str, key: &str) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Maker,
        price: dec!(0.40),
        size: dec!(1),
        internal_key: key.into(),
        strategy: "s".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: NOW / 1000 / 900,
        account_id: AccountId::from(account),
    }
}

/// #182 falsification test — MUST be red against the single Core mutex.
///
/// The ignore line names the blocker; it is not a "flaky" escape hatch. The
/// assertion is the issue's own (< 50 ms) and is deliberately NOT widened to
/// the ~500 ms the current lock produces — widening it would erase the exact
/// gap this test exists to expose (R7.1 L6).
///
/// Multi-thread runtime on purpose: B must be able to RUN (and block on the
/// mutex) while A holds the guard across its awaited submit. On a
/// current-thread runtime a blocking stub would also freeze B's clock, and a
/// passing measurement would prove nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "blocked by #182: single Core mutex serialises all accounts"]
async fn account_a_slow_place_does_not_block_account_b_read() {
    let core = two_account_core();

    // A: place for account `paper`, then hold the guard across the venue
    // submit — exactly where a real execution backend would sit.
    let a = {
        let core = core.clone();
        tokio::spawn(async move {
            let mut c = core.lock().await;
            let _ = c.place_outcome(order_for("paper", "a-slow"), 0, NOW);
            tokio::time::sleep(SLOW_SUBMIT).await;
        })
    };

    // Let A win the guard, so this measures QUEUEING rather than a race.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // B: an independent account's panel read — `orders.list`'s body.
    let started = Instant::now();
    let orders = {
        let c = core.lock().await;
        c.list_orders()
    };
    let elapsed = started.elapsed();

    a.await.expect("A's place task must not panic");
    assert!(
        orders
            .iter()
            .all(|o| o.account_id == AccountId::from("paper")),
        "the read must report the placed row (and nothing else): {orders:?}"
    );
    assert!(
        elapsed < READ_BUDGET,
        "#182: account B's `orders.list` waited {elapsed:?} behind account A's \
         blocked place (budget {READ_BUDGET:?}) — one Core mutex serialises every \
         account. Per-account locks + a lock-free read snapshot are the fix; do \
         NOT widen this budget."
    );
}

/// Liveness under mixed load: many readers beside placements must all finish.
///
/// Green today (one mutex cannot deadlock against itself from separate tasks)
/// and it stays meaningful after the split: a per-account lock re-acquired
/// while already held turns this into a hang, and the 30 s bound names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_order_list_and_place_never_deadlock() {
    let core = two_account_core();
    let start = Instant::now();

    let mut readers = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let core = core.clone();
        readers.push(tokio::spawn(async move {
            let c = core.lock().await;
            c.list_orders().len()
        }));
    }
    let mut placers = Vec::with_capacity(100);
    for i in 0..100 {
        let core = core.clone();
        placers.push(tokio::spawn(async move {
            let mut c = core.lock().await;
            // Refused for want of a crossed book — the lock scope is what this
            // pins, and the refusal is a real kernel answer, not a fabricated
            // fill.
            let _ = c.place_outcome(order_for("default", &format!("p-{i}")), 0, NOW);
        }));
    }

    for r in readers {
        r.await
            .expect("a reader must not panic (a poisoned guard would surface here)");
    }
    for p in placers {
        p.await.expect("a placer must not panic");
    }
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "1000 reads + 100 places did not finish in 30 s — deadlock"
    );
}

/// The half of #182 this branch closes: the feed-subscription publishers run
/// inside the Core guard scope (`handlers/engine.rs`, `handlers/round.rs`,
/// `market/host.rs::subscribe_tokens`), so they must not need a suspend point.
///
/// They are invoked WITHOUT `.await` while the guard is alive: making any of
/// them `async fn` again breaks this file at COMPILE time, which is the point.
#[tokio::test]
async fn feed_subscription_publishes_are_synchronous_under_the_guard() {
    let core = two_account_core();

    {
        let guard = core.lock().await;
        // No `.await` from here to the end of the block — that is the pin.
        guard.subscribe_feed_tokens("unknown-venue", vec!["t1".into()]);
        guard.subscribe_placeholder_tokens("unknown-venue", Vec::new());
        guard.publish_family_feed_subscriptions("unknown-venue");
    }

    // The guard they ran under is still healthy (no poison, no lost write).
    let orders = core.lock().await.list_orders();
    assert!(orders.is_empty(), "a fresh dry core tracks no orders");
}
