//! Reserved-key seal tests: the §2.4 seal (warn once, entry field-identical)
//! must hold without a loaded library, because `adopt_eval_json` is a free
//! function. Reverse acceptance B drives it directly here.
//! Moved verbatim from `foreign.rs` (pure code motion).

use super::reserved_keys::{adopt_eval_json, reserved_intent_keys};
use crate::strategies::StrategyCtx;
use std::sync::{Arc, Mutex, MutexGuard};

/// In-memory warn capture: a `MakeWriter` over a shared buffer, so the
/// test can count the `reserved key dropped` events a parse produces.
#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = CaptureWriter<'a>;
    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter(self.0.lock().unwrap())
    }
}

struct CaptureWriter<'a>(MutexGuard<'a, Vec<u8>>);

impl std::io::Write for CaptureWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// Presence is the signal, whatever the value: an intent that carries a
/// reserved key is warned about and the key dropped — never refused.
#[test]
fn reserved_intent_keys_are_detected_by_presence() {
    let one = serde_json::json!({
        "token": "UP", "price": "0.40", "suggested_stop_loss": "0.30"
    });
    assert_eq!(reserved_intent_keys(&one), vec!["suggested_stop_loss"]);

    let two = serde_json::json!({
        "token": "UP", "suggested_take_profit": "0.9", "suggested_max_hold_sec": 60
    });
    assert_eq!(
        reserved_intent_keys(&two),
        vec!["suggested_take_profit", "suggested_max_hold_sec"]
    );

    // `null` still counts as present: the author wrote the key, which is
    // the thing that must be visible.
    let null_valued = serde_json::json!({ "suggested_stop_loss": null });
    assert_eq!(
        reserved_intent_keys(&null_valued),
        vec!["suggested_stop_loss"]
    );

    // A clean intent, and a non-object, both yield nothing.
    let clean = serde_json::json!({ "token": "UP", "price": "0.40" });
    assert!(reserved_intent_keys(&clean).is_empty());
    assert!(reserved_intent_keys(&serde_json::json!("not an object")).is_empty());
}

/// Reverse acceptance B (§16.6): an intent carrying `suggested_stop_loss`
/// warns EXACTLY ONCE and the entry it produces is field-identical to the
/// same intent without the key — the seal has no stop effect at all.
#[test]
fn reserved_key_intent_warns_once_and_entry_is_field_identical() {
    let market = crate::model::CryptoMarket {
        asset: "asset-1".into(),
        condition_id: "cond-1".into(),
        question_id: "q-1".into(),
        up_token_id: "UP".into(),
        down_token_id: "DOWN".into(),
        up_price: rust_decimal_macros::dec!(0.5),
        down_price: rust_decimal_macros::dec!(0.5),
        expires_at_ms: 0,
        round_slot: 1,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: String::new(),
        neg_risk: false,
        question: "q".into(),
    };
    let markets = vec![market];
    let no_book = |_: &str| None;
    let ctx = StrategyCtx::new(&markets, 1, 600, 1_000, &no_book);

    let with_key = serde_json::json!({
        "entries": [{
            "token": "UP", "price": "0.40", "reason": "band", "shares": "10",
            "suggested_stop_loss": "0.30"
        }]
    });
    let without_key = serde_json::json!({
        "entries": [{ "token": "UP", "price": "0.40", "reason": "band", "shares": "10" }]
    });

    let captured = Capture(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();
    let (with, without) = tracing::subscriber::with_default(subscriber, || {
        let mut exits = Vec::new();
        let mut breaks = Vec::new();
        let with = adopt_eval_json(
            "probe",
            "blitzkrieg_market_api::DEFAULT_ACCOUNT_ID",
            with_key,
            &ctx,
            &mut exits,
            &mut breaks,
            None,
        );
        let without = adopt_eval_json(
            "probe",
            "blitzkrieg_market_api::DEFAULT_ACCOUNT_ID",
            without_key,
            &ctx,
            &mut exits,
            &mut breaks,
            None,
        );
        (with, without)
    });

    let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert_eq!(
        logs.matches("reserved key dropped").count(),
        1,
        "the reserved key must warn exactly once; captured:\n{logs}"
    );

    assert_eq!(with.len(), 1, "the entry must not be refused");
    assert_eq!(without.len(), 1);
    // Field-by-field: `TradeSignal` derives PartialEq, so this compares
    // every field — the dropped key reaches none of them.
    assert_eq!(
        with[0], without[0],
        "entry with the key must equal the entry without it"
    );
    assert_eq!(with[0].shares, Some(rust_decimal_macros::dec!(10)));
    assert_eq!(with[0].reason, "band");
}
