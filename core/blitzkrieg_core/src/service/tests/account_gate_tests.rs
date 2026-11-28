#![cfg(test)]

//! #1045 acceptance: the single in-memory account gate.
//!
//! 1. trip → unfreeze → entries allowed for the rest of the day (unfreeze clears
//!    the session trip latch; `emit_daily_trip` never re-lifts a stale latch —
//!    cash truth wins over a remembered breaker);
//! 2. trip → UTC day roll → gate auto-clears, entries re-open;
//! 3. a legacy risk-state file carrying `accountFreezes`/`auto_frozen` parses
//!    without error and replays nothing — a clean boot starts trading.

use super::*;
use crate::position::{EquityBasis, PositionConfig};
use blitzkrieg_market_api::net::now_ms;
use rust_decimal_macros::dec;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::{Level, Subscriber};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Layer, layer::Context, registry};

const SEED: rust_decimal::Decimal = dec!(1000);

fn gate_core() -> Core {
    Core::new(CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(10_000),
            ..Default::default()
        },
        dry_seed_balance: SEED,
        auto_exits_enabled: false,
        positions: PositionConfig {
            max_daily_loss_equity_pct: dec!(5), // cap = 5% of opening equity = 50
            ..Default::default()
        },
        ..Default::default()
    })
}

fn acct_id() -> crate::model::AccountId {
    crate::model::default_account_id()
}

fn buy_req(key: &str) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price: dec!(1),
        size: dec!(1000),
        internal_key: key.into(),
        strategy: "s1".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: acct_id(),
    }
}

fn entry_req(key: &str) -> OrderRequest {
    let mut r = buy_req(key);
    r.size = dec!(10);
    r.price = dec!(0.4);
    r
}

/// Book a 600 loss on `default` against a 50 cap (5% of the 1000 seed), latch
/// the trip this session (as `note_daily_drawdown` would), then announce it via
/// `emit_daily_trip` (this is what every tick does). The loss is booked
/// straight into the day's realized bucket so the test does not depend on the
/// engine cycle matching a buy against a sell.
fn trip_daily_loss(c: &mut Core, at_ms: i64) {
    let acct = crate::model::default_account_id();
    c.positions
        .roll_daily("default", at_ms, SEED, EquityBasis::Dry);
    c.positions.set_daily_realized_for_test(&acct, dec!(-600));
    c.positions
        .set_daily_tripped_for_test(&acct, at_ms, dec!(50));
    // Announce the trip (every tick does this): lifts a DailyLoss gate
    // expiring at the next UTC day boundary.
    c.emit_daily_trip();
    assert!(
        c.account_gate_blocked("default", at_ms + 500),
        "daily loss trip must gate entries"
    );
}

#[test]
fn daily_loss_trip_unfreeze_allows_entries_rest_of_day() {
    let mut c = gate_core();
    let t0 = 1_700_000_000_000;
    trip_daily_loss(&mut c, t0);
    assert!(
        c.account_gate_blocked("default", t0 + 500),
        "trip gates entries"
    );

    // Unfreeze clears the gate AND the session trip latch — entries allowed for
    // the rest of the day. Unfreeze never erases the realized loss, but the
    // daily-loss gate is not re-armed until the UTC day rolls.
    let r = c
        .unfreeze_account("default", "op", t0 + 600)
        .expect("unfreeze");
    assert!(r.unfrozen, "gate was cleared");
    assert!(
        r.previous_reason.is_some(),
        "the receipt names the previous gate"
    );
    assert!(r.entries_allowed, "entries allowed at the unfreeze instant");
    assert!(
        !c.account_gate_blocked("default", t0 + 700),
        "entries allowed after unfreeze"
    );

    // A subsequent emit_daily_trip does NOT re-lift the gate: the latch was
    // cleared by unfreeze, and emit_daily_trip only gates newly-latched trips.
    c.emit_daily_trip();
    assert!(
        !c.account_gate_blocked("default", t0 + 800),
        "emit_daily_trip does not re-latch after unfreeze"
    );
    assert!(
        c.place_pending(entry_req("entry"), t0 + 900).is_ok(),
        "an unfrozen account trades again"
    );
}

#[test]
fn daily_loss_gate_auto_clears_at_utc_day_roll() {
    let mut c = gate_core();
    let t0 = 1_700_000_000_000; // day 19675
    trip_daily_loss(&mut c, t0);
    assert!(c.account_gate_blocked("default", t0 + 500));

    // Roll into the next UTC day (day 19676): the DailyLoss gate expires at the
    // boundary and is cleared by `roll_daily_budget` — no API call needed.
    let t1 = 1_700_006_401_000; // t0's next UTC day boundary + 1s
    c.roll_daily_budget(t1);
    assert!(
        !c.account_gate_blocked("default", t1),
        "DailyLoss gate auto-cleared at the UTC day boundary"
    );
    // Entries re-open immediately (the ledger is untouched; only the gate cleared).
    assert!(
        c.place_pending(entry_req("entry"), t1).is_ok(),
        "entries re-open after the daily loss gate expires"
    );
}

#[test]
fn legacy_risk_state_freezes_are_ignored_at_boot() {
    use std::path::PathBuf;

    let root = std::env::temp_dir().join(format!(
        "test-1045-legacy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("temp root creates");
    let path: PathBuf = root.join("positions.risk-state.json");

    // An OLD risk-state file: carries the retired `accountFreezes` /
    // `auto_frozen` cells plus a limit. serde ignores unknown keys; the boot
    // restore must not replay any freeze.
    std::fs::write(
        &path,
        r#"{
  "savedAtMs": 1000,
  "limits": { "maxOrderNotional": "250" },
  "accountFreezes": { "default": "stale manual hold" },
  "auto_frozen": { "default": 1000 }
}"#,
    )
    .unwrap();

    assert!(matches!(
        crate::service::risk_admission::load_risk_state_file(&path),
        crate::service::risk_admission::RiskStateLoad::Ok(_)
    ));

    let mut c = crate::service::Core::with_account_book(
        crate::service::CoreConfig {
            mode: Mode::Dry,
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: SEED,
            auto_exits_enabled: false,
            risk_state_path: Some(path.to_string_lossy().to_string()),
            ..Default::default()
        },
        crate::account::AccountLedgers::new(
            vec![crate::account::Account {
                id: acct_id(),
                name: "default".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                opening_equity: None,
                updated_at_ms: 0,
            }],
            acct_id(),
        ),
    );

    // The limit restored (file parsed), but NO freeze was replayed.
    assert_eq!(c.risk.config().max_order_notional, dec!(250));
    assert!(!c.is_killed());
    assert!(!c.account_gate_blocked("default", now_ms()));
    assert!(c.account_freeze_reason("default", now_ms()).is_none());
    // And the account trades: entry passes every gate.
    assert!(
        c.place_pending(entry_req("entry"), now_ms()).is_ok(),
        "a clean boot trades even with a stale freeze book on disk"
    );

    drop(c);
    let _ = std::fs::remove_dir_all(&root);
}

/// #1062 阻断 1：unfreeze persist 失败 → gate 仍在，绝不假成功。
/// 当没有 wired store（无法落 SQLite——操作者裁决 2026-11-14 后 `.env`
/// 对内核只读，不再是持久化载体），解冻必须返回错误，运行时闸门不动，
/// 账户仍拒新单。
#[test]
fn unfreeze_persist_failure_keeps_gate() {
    use crate::account::{Account, AccountId, AccountLedgers, AccountStatus, CredentialKeys};
    use crate::model::Mode;

    let now = now_ms();
    let book = AccountLedgers::new(
        vec![Account {
            id: AccountId::from("default"),
            name: "default".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: AccountStatus::ReadOnly,
            credential_keys: CredentialKeys::default(),
            venue: String::new(),
            trading_enabled: false,
            strategies: Vec::new(),
            rounds: Vec::new(),
            opening_equity: None,
            updated_at_ms: now,
        }],
        AccountId::from("default"),
    );

    // No wired store → the persist half refuses (persistence is disabled).
    // The account starts non-trading so the unfreeze persist branch is
    // entered (not skipped).
    let mut c = crate::service::Core::with_account_book(
        crate::service::CoreConfig {
            mode: Mode::Dry,
            risk: crate::risk::RiskConfig::default(),
            dry_seed_balance: dec!(1000),
            auto_exits_enabled: false,
            ..Default::default()
        },
        book,
    );

    // A Manual gate on top of the config posture.
    c.freeze_account("default", "operator: investigating".into());
    assert!(
        c.account_gate_blocked("default", now),
        "precondition: account is gated"
    );

    // Unfreeze must fail because persist cannot write (no config path).
    let r = c.unfreeze_account("default", "operator", now);
    assert!(r.is_err(), "unfreeze must refuse when persist cannot write");

    // The gate is still there — nothing cleared, nothing assumed.
    assert!(
        c.account_gate_blocked("default", now),
        "persist failure leaves the gate in place"
    );
    assert!(
        c.account_freeze_reason("default", now).is_some(),
        "freeze reason still readable"
    );
}

/// #1062 阻断 1 的成功半边：unfreeze 把配置态写进 SQLite `accounts` 行
/// （唯一事实源，操作者裁决 2026-11-14），`status` 与 `trading_enabled`
/// 在一次 UPDATE 里同时翻正；用同一份 store 重建内核后闸门不复活。生产形态
/// 是 `accounts.toml` 不存在（它只是 write-refusal basis，不是可写载体，
/// issue 751/923）。
#[test]
fn unfreeze_writes_env_back_and_entries_stay_open_after_rebuild() {
    use crate::account::{Account, AccountId, AccountLedgers, AccountStatus, CredentialKeys};
    use crate::model::Mode;

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "test-1062-unfreeze-{}-{}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&root).expect("temp root");
    let env_path = root.join(".env");
    let accounts_path = root.join("accounts.toml"); // deliberately ABSENT
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default\nACCOUNT_DEFAULT_STATUS=frozen\nACCOUNT_DEFAULT_TRADING_ENABLED=false\n",
    )
    .expect("env fixture");

    let now = now_ms();
    let book = AccountLedgers::new(
        vec![Account {
            id: AccountId::from("default"),
            name: "default".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: AccountStatus::Frozen,
            credential_keys: CredentialKeys::default(),
            venue: String::new(),
            trading_enabled: false,
            strategies: Vec::new(),
            rounds: Vec::new(),
            opening_equity: None,
            updated_at_ms: now,
        }],
        AccountId::from("default"),
    );
    let cfg = || crate::service::CoreConfig {
        mode: Mode::Dry,
        risk: crate::risk::RiskConfig::default(),
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
        env_file_path: Some(env_path.to_string_lossy().into_owned()),
        // 持久化后端是 SQLite（`.env` 只读）：unfreeze 的 status/trading 落 accounts 表。
        db_path: Some(root.join("store.sqlite")),
        ..Default::default()
    };

    let mut c = crate::service::Core::with_account_book(cfg(), book);
    crate::service::reconcile_config_gates(&mut c, now);
    assert!(
        c.account_gate_blocked("default", now),
        "precondition: the config posture gates entries"
    );

    // env 在写前快照：unfreeze 不得动文件（内核零写入）。
    let env_before = std::fs::read_to_string(&env_path).expect("env readable");

    let r = c
        .unfreeze_account("default", "operator", now)
        .expect("unfreeze must persist and succeed");
    assert!(r.entries_allowed, "entries open at the unfreeze instant");
    assert!(!c.account_gate_blocked("default", now));

    // `.env` 逐字节不动——唯一写入者是安装器，内核不碰。
    let env_after = std::fs::read_to_string(&env_path).expect("env readable");
    assert_eq!(
        env_after, env_before,
        "unfreeze must not touch the .env: {env_after}"
    );

    // 持久化半边：accounts 表里的 default 行翻成 active / trading-on。
    let rows = crate::risk::account_store::AccountRiskDb::open(root.join("store.sqlite"))
        .expect("store re-opens")
        .load_accounts()
        .expect("accounts readable");
    let row = rows
        .iter()
        .find(|r| r.id == "default")
        .expect("default row present");
    assert_eq!(row.status, "active", "status row is active");
    assert!(row.trading_enabled, "trading_enabled row is on");

    // Restart 从同一 store 读：SQL 行是权威，env 里的 stale frozen 不再参与门禁。
    let fresh =
        crate::account::AccountLedgers::new(Vec::new(), crate::account::AccountId::from("default"));
    let mut c2 = crate::service::Core::with_account_book(cfg(), fresh);
    crate::service::reconcile_config_gates(&mut c2, now);
    assert!(
        !c2.account_gate_blocked("default", now),
        "a rebuild from the store must not resurrect the gate"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// #1070: `emit_daily_trip` の報文検証。
///
/// 金額は 2 桁小数、報告するのは有効 cap 規則のみ（absolute または
/// pct of opening equity、混在しない）、trigger 口径は完整 drawdown
///（realized + unrealized、unrealized なし時は realized only）。
fn daily_core(positions: PositionConfig) -> Core {
    Core::new(CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(10_000),
            ..Default::default()
        },
        dry_seed_balance: SEED,
        auto_exits_enabled: false,
        positions,
        ..Default::default()
    })
}

/// 捕捉 `emit_daily_trip` の kernel warn（`drawdown … (realized … + unrealized …) vs cap …`）。
fn capture_kernel_warn<F>(f: F) -> String
where
    F: FnOnce(),
{
    let captured = Arc::new(Mutex::new(None::<String>));
    let layer = WarnCapture(captured.clone());
    let subscriber = registry().with(layer);
    tracing::subscriber::with_default(subscriber, f);
    captured
        .lock()
        .unwrap()
        .take()
        .expect("emit_daily_trip must log a warn")
}

struct WarnCapture(Arc<Mutex<Option<String>>>);
impl<S: Subscriber> Layer<S> for WarnCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if *event.metadata().level() != Level::WARN {
            return;
        }
        let mut visitor = Message(&self.0);
        event.record(&mut visitor);
    }
}

struct Message<'a>(&'a Arc<Mutex<Option<String>>>);
impl Visit for Message<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            *self.0.lock().unwrap() = Some(format!("{:?}", value));
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            *self.0.lock().unwrap() = Some(value.to_string());
        }
    }
}

/// 絶対 cap が有効：`cap $5.00 (absolute)`。
#[test]
fn emit_daily_trip_reports_absolute_cap() {
    let mut c = daily_core(PositionConfig {
        max_daily_loss_usd: dec!(5),
        max_daily_loss_equity_pct: dec!(50),
        ..Default::default()
    });
    let acct = crate::model::default_account_id();
    let t0 = 1_700_000_000_000;
    c.positions
        .roll_daily(acct.as_str(), t0, dec!(1000), EquityBasis::Dry);
    c.positions.set_daily_realized_for_test(&acct, dec!(-3.344));
    c.positions.set_daily_tripped_for_test(&acct, t0, dec!(5));
    c.emit_daily_trip();

    let gate = c.account_gate(acct.as_str()).expect("daily loss gate set");
    assert!(
        gate.reason
            .contains("drawdown $-3.34 vs cap $5.00 (absolute)"),
        "unexpected gate message"
    );
}

/// 百分比 cap が有効：`cap $50.00 (5% of opening equity $1000.00)`、absolute は混在しない。
#[test]
fn emit_daily_trip_reports_percentage_cap() {
    let mut c = daily_core(PositionConfig {
        max_daily_loss_usd: dec!(100), // 絶対 cap は非結合
        max_daily_loss_equity_pct: dec!(5),
        ..Default::default()
    });
    let acct = crate::model::default_account_id();
    let t0 = 1_700_000_000_000;
    c.positions
        .roll_daily(acct.as_str(), t0, dec!(1000), EquityBasis::Dry);
    c.positions.set_daily_realized_for_test(&acct, dec!(-3.344));
    c.positions.set_daily_tripped_for_test(&acct, t0, dec!(50));
    c.emit_daily_trip();

    let gate = c.account_gate(acct.as_str()).expect("daily loss gate set");
    assert!(
        gate.reason
            .contains("drawdown $-3.34 vs cap $50.00 (5% of opening equity $1000.00)"),
        "unexpected gate message"
    );
}

/// opening equity が不明（0）の時、パーセント cap は算出できず absolute に降級。
#[test]
fn emit_daily_trip_degrades_to_absolute_when_opening_equity_unknown() {
    let mut c = daily_core(PositionConfig {
        max_daily_loss_usd: dec!(5),
        max_daily_loss_equity_pct: dec!(5),
        ..Default::default()
    });
    let acct = crate::model::default_account_id();
    let t0 = 1_700_000_000_000;
    c.positions
        .roll_daily(acct.as_str(), t0, dec!(0), EquityBasis::Dry); // opening 未知
    c.positions.set_daily_realized_for_test(&acct, dec!(-3.344));
    c.positions.set_daily_tripped_for_test(&acct, t0, dec!(5));
    c.emit_daily_trip();

    let gate = c.account_gate(acct.as_str()).expect("daily loss gate set");
    assert!(
        gate.reason
            .contains("drawdown $-3.34 vs cap $5.00 (absolute)"),
        "unexpected gate message"
    );
}

/// unrealized が null（marks off）の時、kernel は realized only を報告。
#[test]
fn emit_daily_trip_reports_realized_only_when_unrealized_null() {
    let mut c = daily_core(PositionConfig {
        max_daily_loss_usd: dec!(5),
        max_daily_loss_equity_pct: dec!(50),
        daily_loss_marks_unrealized: false,
        ..Default::default()
    });
    let acct = crate::model::default_account_id();
    let t0 = 1_700_000_000_000;
    c.positions
        .roll_daily(acct.as_str(), t0, dec!(1000), EquityBasis::Dry);
    c.positions.set_daily_realized_for_test(&acct, dec!(-3.344));
    c.positions.set_daily_tripped_for_test(&acct, t0, dec!(5));
    let kernel = capture_kernel_warn(|| c.emit_daily_trip());
    assert!(
        kernel.contains("(realized $-3.34, realized only)"),
        "unexpected kernel message: {:?}",
        kernel
    );
    let gate = c.account_gate(acct.as_str()).expect("daily loss gate set");
    assert!(
        gate.reason
            .contains("drawdown $-3.34 vs cap $5.00 (absolute)"),
        "unexpected gate message"
    );
}

#[test]
fn opening_positions_does_not_shrink_opening_equity_or_trip_daily_loss() {
    let mut c = daily_core(PositionConfig {
        max_daily_loss_usd: dec!(100),
        max_daily_loss_equity_pct: dec!(20), // 20% of $1000 = $200 cap
        ..Default::default()
    });
    let acct = crate::model::default_account_id();
    let t0 = 1_700_000_000_000;
    c.roll_daily_budget(t0);
    assert_eq!(
        c.positions
            .daily_state_for(acct.as_str())
            .opening_equity_usd,
        SEED
    );

    // Simulate spending $800 of cash (80% of $1000) to open a position:
    // cash drops to $200, open position cost is $800 -> total equity stays $1000.
    // Pre-fix, `roll_daily_budget` passed bare cash ($200), shrinking
    // `opening_equity_usd` from $1000 to $200 and falsely tripping a -$60 loss.
    c.accounts.get_mut(&acct).unwrap().seed(dec!(200));
    let pos = c.positions.open(
        crate::position::OpenParams {
            strategy: "s1".into(),
            asset: "BTC".into(),
            direction: SignalDirection::Up,
            token_id: "tok".into(),
            condition_id: "cond".into(),
            account_id: acct.clone(),
            venue: "polymarket".into(),
            partner_token_id: String::new(),
            entry_price: dec!(0.8),
            expires_at_ms: t0 + 300_000,
            was_maker: false,
            target_exit_price: None,
            round_duration_sec: 300,
            round_slot: 1,
        },
        t0 + 1_000,
    );
    c.positions.apply_entry_fill(
        &pos.id,
        dec!(1000),
        dec!(0.8),
        Decimal::ZERO,
        OrderRole::Taker,
    );
    c.positions.set_daily_realized_for_test(&acct, dec!(-60));
    c.roll_daily_budget(t0 + 2_000);

    assert_eq!(
        c.positions
            .daily_state_for(acct.as_str())
            .opening_equity_usd,
        SEED,
        "opening_equity_usd must not shrink when cash moves into open positions"
    );
    assert!(
        !c.positions.daily_loss_tripped_for(acct.as_str()),
        "-$60 realized on $1000 equity (20% cap = $200) must NOT trip"
    );
}

/// Audit finding #1: `run_accounting_audit` must audit EVERY active account,
/// not just `active_id`. When `active_id` is an empty `default` book and a
/// sibling active account (`polysimulator`) develops a stray reservation, the
/// audit must catch it, halt `polysimulator` entries, and leave `default` free.
#[test]
fn run_accounting_audit_catches_drift_on_non_active_account() {
    use crate::account::{Account, AccountId, AccountLedgers, AccountStatus, CredentialKeys};

    let default_id = AccountId::from("default");
    let poly_id = AccountId::from("polysimulator");
    let book = AccountLedgers::new(
        vec![
            Account {
                id: default_id.clone(),
                name: "default".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                opening_equity: None,
                updated_at_ms: 0,
            },
            Account {
                id: poly_id.clone(),
                name: "polysimulator".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                opening_equity: None,
                updated_at_ms: 0,
            },
        ],
        default_id.clone(),
    );
    let mut c = crate::service::Core::with_account_book(
        crate::service::CoreConfig {
            mode: Mode::Dry,
            risk: RiskConfig {
                max_order_notional: dec!(10_000),
                ..Default::default()
            },
            dry_seed_balance: SEED,
            auto_exits_enabled: false,
            audit_halt_entries: true,
            ..Default::default()
        },
        book,
    );
    let t0 = 1_700_000_000_000;
    // Establish clean anchors on both accounts.
    c.run_accounting_audit(t0);
    assert!(!c.audit_blocks_entries());

    // Inject a stray reservation on the NON-ACTIVE account `polysimulator`.
    c.accounts
        .get_mut(&poly_id)
        .unwrap()
        .reserve("ghost-order-1", dec!(10))
        .unwrap();
    c.run_accounting_audit(t0 + 1_000);

    assert!(
        c.audit_blocks_entries(),
        "audit must notice structural drift on non-active account"
    );
    assert!(
        c.audit_entry_gate_for(&poly_id).is_err(),
        "polysimulator entries must be halted"
    );
    assert!(
        c.audit_entry_gate_for(&default_id).is_ok(),
        "clean default account stays unblocked"
    );
}

/// Audit finding #3: a high-price Taker BUY (>= 0.70) must be refused when
/// the live mirrored book's best ask has already collapsed >25% below the
/// intended limit (preventing falling-knife fills like 0.93 -> 0.33 or 0.88 -> 0.06).
#[test]
fn high_price_taker_buy_refused_when_book_collapsed() {
    let mut c = gate_core();
    let t0 = 1_700_000_000_000;
    // Mirror a collapsed book (best ask = 0.33) for token `tok`.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.30), dec!(100))],
        vec![(dec!(0.33), dec!(100))],
        t0,
    );
    let mut req = entry_req("late_fav_knife");
    req.mode = FillPolicy::Taker;
    req.price = dec!(0.93);
    req.size = dec!(10);
    let err = c
        .place(req, 0, t0 + 100)
        .expect_err("collapsed ask must refuse high-price taker buy");
    assert_eq!(err.code, CoreErrorCode::WouldCross);
    assert!(
        err.message.contains("adverse_book_collapse"),
        "unexpected error: {}",
        err.message
    );

    // A normal book near the limit (best ask = 0.92) still passes cleanly.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.90), dec!(100))],
        vec![(dec!(0.92), dec!(100))],
        t0 + 200,
    );
    let mut ok_req = entry_req("late_fav_ok");
    ok_req.mode = FillPolicy::Taker;
    ok_req.price = dec!(0.93);
    ok_req.size = dec!(10);
    assert!(
        c.place(ok_req, 0, t0 + 300).is_ok(),
        "normal high-price taker buy near best ask must succeed"
    );
}
