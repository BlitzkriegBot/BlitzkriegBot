#![cfg(test)]

//! #183: `set_tokens` is REPLACE semantics, so the ONLY safe input for one
//! venue's control is the plane's COMPLETE union. Before this fix
//! `engine.markets` handed the control just the arriving batch's up/down
//! tokens: a 5m family poll displaced the 15m family's token set and the next
//! 15m poll displaced it back — every sibling family was evicted by the
//! other's polls.
//!
//! These four tests are the falsification suite. They drive the REAL
//! `handle_engine` arm (not a copy of its routing) and read the state a feed
//! would actually read: a `SubscriptionControl` whose `set_tokens` /
//! `set_placeholder_tokens` UPDATE a current-set cell, exactly the replace
//! semantics the live feeds implement. A test double is legitimate here —
//! this is a read-only subscription assertion, no money path is mocked.

use super::*;
use crate::account::{Account, AccountId, AccountLedgers, AccountStatus};
use crate::ipc::server::engine_handler_for_tests;
use crate::market::registry::MarketPluginRegistry;
use crate::model::default_account_id;
use crate::schema::method;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;

const VENUE: &str = "polymarket";

/// A `SubscriptionControl` that holds the CURRENT set (replace semantics,
/// like every live feed) and counts the writes. Assertions read `live()` —
/// the state a feed's poll loop would act on — never a log of intentions.
#[derive(Default)]
struct RecordingControl {
    live: Mutex<Vec<String>>,
    placeholder: Mutex<Vec<String>>,
    live_writes: AtomicUsize,
    placeholder_writes: AtomicUsize,
}

impl RecordingControl {
    fn live(&self) -> Vec<String> {
        self.live.lock().unwrap().clone()
    }
    fn placeholder(&self) -> Vec<String> {
        self.placeholder.lock().unwrap().clone()
    }
}

impl blitzkrieg_market_api::SubscriptionControl for RecordingControl {
    fn set_tokens(&self, tokens: Vec<blitzkrieg_market_api::TokenId>) {
        *self.live.lock().unwrap() = tokens;
        self.live_writes.fetch_add(1, Ordering::SeqCst);
    }
    fn set_placeholder_tokens(&self, tokens: Vec<blitzkrieg_market_api::TokenId>) {
        *self.placeholder.lock().unwrap() = tokens;
        self.placeholder_writes.fetch_add(1, Ordering::SeqCst);
    }
}

fn account(id: &str) -> Account {
    Account {
        id: AccountId::from(id),
        name: id.into(),
        market_type: blitzkrieg_market_api::MarketType::Prediction,
        status: AccountStatus::Active,
        credential_keys: Default::default(),
        venue: VENUE.into(),
        trading_enabled: true,
        strategies: Vec::new(),
        rounds: Vec::new(),
        opening_equity: None,
        updated_at_ms: 0,
    }
}

/// A dry, engine-LESS core whose account book carries one account per
/// `(id, family)` pair — the plane seeds its live refs from exactly these
/// anchors (`sync_family_feed_from_accounts` in the constructor). The engine
/// stays out on purpose: this is the hostless shape (replay / Node bridge),
/// where the handler's own plane write is the only thing that records the
/// batch.
fn hostless_core(families: &[(&str, i64)]) -> Arc<AsyncMutex<Core>> {
    let book = AccountLedgers::new(
        families.iter().map(|(id, _)| account(id)).collect(),
        default_account_id(),
    );
    let cfg = CoreConfig {
        default_round_duration_sec: 900,
        round_duration_sec: 900,
        account_round_durations: families
            .iter()
            .map(|(id, dur)| (id.to_string(), *dur))
            .collect(),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    Arc::new(AsyncMutex::new(Core::with_account_book(cfg, book)))
}

/// The engine-ON shape: the production wiring (`engine.markets` → ingest arm
/// → `note_family_feed_markets`). One account anchored at the global 900s
/// family; the 3600s family is a live sibling ref (second account) whose
/// tokens must survive every 900s batch.
fn engine_core(families: &[(&str, i64)]) -> Arc<AsyncMutex<Core>> {
    let book = AccountLedgers::new(
        families.iter().map(|(id, _)| account(id)).collect(),
        default_account_id(),
    );
    let cfg = CoreConfig {
        default_round_duration_sec: 900,
        round_duration_sec: 900,
        account_round_durations: families
            .iter()
            .map(|(id, dur)| (id.to_string(), *dur))
            .collect(),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let mut c = Core::with_account_book(cfg, book);
    c.enable_engine(crate::engine::Engine::new(crate::engine::EngineConfig {
        scanner: crate::scanner::ScannerConfig {
            assets: vec!["BTC".into()],
            round_duration_sec: 900,
            min_round_age_sec: 0,
            min_time_left_sec: 0,
        },
        ..Default::default()
    }));
    Arc::new(AsyncMutex::new(c))
}

fn control(core: &Arc<AsyncMutex<Core>>) -> Arc<RecordingControl> {
    let ctl = Arc::new(RecordingControl::default());
    // Install synchronously: `set_subscription` is a plain map write, and the
    // tests need the handle before any batch moves the subscription.
    let mut c = core.try_lock().expect("the fixture core is uncontended");
    c.set_subscription(VENUE, ctl.clone());
    drop(c);
    ctl
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

fn tokens(stem: &str) -> [String; 2] {
    [format!("{stem}-up"), format!("{stem}-down")]
}

/// One round market row, camelCase exactly as the wire carries it.
fn market_row(venue: &str, dur: i64, slot: i64, stem: &str) -> serde_json::Value {
    serde_json::json!({
        "asset": "BTC",
        "conditionId": format!("cond-{venue}-{dur}-{slot}-{stem}"),
        "questionId": format!("q-{stem}"),
        "upTokenId": format!("{stem}-up"),
        "downTokenId": format!("{stem}-down"),
        "upPrice": 0.6,
        "downPrice": 0.4,
        "expiresAtMs": (slot + 1) * dur * 1000,
        "roundSlot": slot,
        "roundDurationSec": dur,
        "archiveVerdict": false,
        "venue": venue,
        "negRisk": false,
        "question": "?",
    })
}

/// Drive the REAL `engine.markets` arm. `registry` is empty: every row is
/// venue-tagged, so the active-plugin fallback never fires.
async fn feed(core: &Arc<AsyncMutex<Core>>, rows: Vec<serde_json::Value>) {
    let registry = MarketPluginRegistry::new();
    let reply = engine_handler_for_tests(
        method::ENGINE_MARKETS,
        core,
        &registry,
        serde_json::json!({ "markets": rows }),
    )
    .await
    .expect("engine.markets must accept a venue-tagged batch");
    assert_eq!(reply["ok"], serde_json::json!(true), "{reply}");
}

/// Two live families on ONE venue, batch A then batch B: the control's set
/// must be the UNION after every write. Falsifies "last writer wins" at the
/// entry point itself (no engine involved — the hostless shape).
#[tokio::test]
async fn two_families_union_survives_sequential_publishes() {
    let core = hostless_core(&[("default", 900), ("sibling", 3600)]);
    let ctl = control(&core);

    feed(&core, vec![market_row(VENUE, 900, 5, "five-min")]).await;
    assert_eq!(
        sorted(ctl.live()),
        sorted(tokens("five-min").to_vec()),
        "the first batch's own family is the whole known union"
    );

    feed(&core, vec![market_row(VENUE, 3600, 2, "hour")]).await;
    assert_eq!(
        sorted(ctl.live()),
        sorted([tokens("five-min"), tokens("hour")].concat()),
        "the second publish must carry BOTH live families, not replace the first"
    );
}

/// Production wiring (engine ON): re-feeding the 900s family after the 3600s
/// family landed must leave the sibling's tokens in the control set. Under
/// the pre-#183 arm the third batch (`tokens` = this batch only) evicted the
/// sibling every time.
#[tokio::test]
async fn engine_markets_does_not_evict_sibling_family() {
    let core = engine_core(&[("default", 900), ("sibling", 3600)]);
    let ctl = control(&core);
    let now = blitzkrieg_market_api::net::now_ms();
    let slot_900 = now / 1000 / 900;
    let slot_3600 = now / 1000 / 3600;

    feed(&core, vec![market_row(VENUE, 900, slot_900, "five-min")]).await;
    feed(&core, vec![market_row(VENUE, 3600, slot_3600, "hour")]).await;
    let after_both = sorted(ctl.live());
    assert_eq!(
        after_both,
        sorted([tokens("five-min"), tokens("hour")].concat()),
        "two live families must coexist after their own batches"
    );
    assert!(
        ctl.placeholder().is_empty(),
        "both families have an anchoring account — neither belongs on the \
         placeholder channel, got {:?}",
        ctl.placeholder()
    );

    // The sibling-evicting shape: another poll of the FIRST family only.
    feed(&core, vec![market_row(VENUE, 900, slot_900, "five-min")]).await;
    assert_eq!(
        sorted(ctl.live()),
        after_both,
        "a 900s re-feed must not evict the 3600s family"
    );
    assert_eq!(
        ctl.live_writes.load(Ordering::SeqCst),
        3,
        "every batch republishes (the union is recomputed, not cached)"
    );
}

/// Refcount release: when one of two accounts sharing the 900s family leaves
/// (account removed from the book, then the plane re-synced — the exact
/// sequence `round_duration.set` / account removal run), the family stays
/// live at refcount 1 and BOTH live families keep their subscriptions.
#[tokio::test]
async fn refcount_release_keeps_sibling_alive() {
    let core = hostless_core(&[("default", 900), ("leaving", 900), ("sibling", 3600)]);
    let ctl = control(&core);

    // Give the 3600s family tokens first so "alive" has something to read.
    feed(&core, vec![market_row(VENUE, 3600, 2, "hour")]).await;

    {
        let mut c = core.lock().await;
        c.accounts_mut()
            .remove(&AccountId::from("leaving"))
            .expect("the fixture account is present");
        c.sync_family_feed_from_accounts();
        assert_eq!(
            c.family_feed().live_ref_count(VENUE, 900),
            1,
            "one account still anchors the 900s family"
        );
    }

    feed(&core, vec![market_row(VENUE, 900, 5, "five-min")]).await;
    assert_eq!(
        sorted(ctl.live()),
        sorted([tokens("five-min"), tokens("hour")].concat()),
        "the released family's sibling AND the surviving family both stay subscribed"
    );
}

/// A rollover on ONE family (new slot, new tokens) must REPLACE that
/// family's tokens and keep the sibling family untouched: the rolled family's
/// old tokens are gone from the control set, the sibling's are intact.
#[tokio::test]
async fn rollover_does_not_drop_sibling() {
    let core = hostless_core(&[("default", 900), ("sibling", 3600)]);
    let ctl = control(&core);

    feed(&core, vec![market_row(VENUE, 900, 5, "old-round")]).await;
    feed(&core, vec![market_row(VENUE, 3600, 2, "hour")]).await;
    feed(&core, vec![market_row(VENUE, 900, 6, "new-round")]).await;

    assert_eq!(
        sorted(ctl.live()),
        sorted([tokens("new-round"), tokens("hour")].concat()),
        "rollover replaces its own family and leaves the sibling's set intact"
    );
    assert!(
        !ctl.live().contains(&"old-round-up".to_string()),
        "the previous round's tokens must not linger in the live set"
    );
}
