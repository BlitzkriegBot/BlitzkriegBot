//! Shadow record replay: run the shared exit policy over a recorded position
//! path. Moved verbatim from `shadow.rs`.

use super::*;
use crate::exit_policy::{
    BookScalarView, ExitConfig, ExitState, ExitTickInput, decide_exit, executable_bid,
    update_exit_state,
};
use crate::model::OrderbookSnapshot;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Replay a recorded path through the shared exit policy. Conservative: charges
/// the taker fee on every exit (a live maker-first TP can only do better).
pub fn replay(rec: &ShadowRecord, cfg: &ExitConfig) -> ReplayResult {
    if rec.own.is_empty() {
        return ReplayResult {
            pnl: Decimal::ZERO,
            exit_reason: crate::model::ExitReason::Manual,
            exit_price: rec.entry_price,
        };
    }
    let entry = rec.entry_price;
    let shares = rec.shares;
    let mut state = ExitState::new(entry, 0);
    let entry_fee = if rec.was_maker_entry {
        Decimal::ZERO
    } else {
        crate::exit_policy::taker_fee_pct(entry) / Decimal::ONE_HUNDRED * entry * shares
    };

    let mut exit_price = rec
        .own
        .last()
        .map(|s| s.bid.unwrap_or(s.price))
        .unwrap_or(entry);
    let mut exit_reason = crate::model::ExitReason::ForceExit;

    for s in &rec.own {
        let now = rec.entered_at_ms + s.t_ms;
        let book = OrderbookSnapshot::from_levels(
            rec.token_id.clone(),
            vec![(s.bid.unwrap_or(s.price), dec!(100))],
            vec![(s.ask.unwrap_or(s.price), dec!(100))],
            now,
        );
        update_exit_state(&mut state, entry, Some(&book), now, cfg);
        let time_left = (rec.expires_at_ms - now) / 1000;
        let d = decide_exit(ExitTickInput {
            entry_price: entry,
            book: Some(&book as &dyn BookScalarView),
            fallback_price: Some(s.price),
            time_left_sec: time_left,
            hold_sec: s.t_ms / 1000,
            state: &state,
            now_ms: now,
            cfg,
        });
        if let Some(d) = d {
            exit_price = executable_bid(Some(&book));
            exit_reason = d.reason;
            break;
        }
    }

    let exit_fee =
        crate::exit_policy::taker_fee_pct(exit_price) / Decimal::ONE_HUNDRED * exit_price * shares;
    let pnl = shares * (exit_price - entry) - entry_fee - exit_fee;
    ReplayResult {
        pnl,
        exit_reason,
        exit_price,
    }
}
