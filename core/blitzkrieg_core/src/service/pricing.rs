//! Fee-schedule + maker-exit pricing on `Core`, plus the shared
//! `marketable_exit_price` helper. Pure code motion from `service.rs`
//! (no logic edits).

use crate::service::Core;
use rust_decimal::Decimal;

impl Core {
    /// The taker fee a LIVE charge site may use, at `price` (#234, item 4).
    ///
    /// The schedule is process-wide and set-once, and only a replay may install
    /// one other than the shipped default — `--fee-model` is refused without
    /// `--backtest`, and the backtester is the only caller of `set_fee_schedule`.
    /// That isolation used to live ENTIRELY in the CLI: the two live charge sites
    /// read whatever was installed and could not tell a replay from a live
    /// session. This makes the invariant explicit where the money is: a core that
    /// has not declared itself a replay charges the shipped schedule or the
    /// process stops.
    ///
    /// It fails CLOSED on purpose. A counterfactual fee on a real fill is a wrong
    /// number in the ledger, in every PnL derived from it and in the loss
    /// breakers that read them — silent and unrepairable after the fact — while a
    /// refused charge is loud and stops before it can book. If a live run ever
    /// legitimately needs another schedule, that is a design change to the charge
    /// path, not a flag flip.
    pub(super) fn live_taker_fee_pct(&self, price: Decimal) -> Decimal {
        let schedule = crate::exit_policy::fee_schedule();
        assert!(
            self.may_charge(schedule),
            "non-replay core would charge the '{}' taker-fee schedule: --fee-model is \
             replay-only (set CoreConfig::fee_schedule_replay only in a backtest) (#234)",
            schedule.name
        );
        schedule.fee_pct(price)
    }

    /// Whether a charge under `schedule` is legal for THIS core (#234, item 4).
    /// Split out from [`Core::live_taker_fee_pct`] so the predicate can be tested
    /// without installing a schedule: the process-wide schedule is set-once, so a
    /// test cannot put a second one in place.
    pub(super) fn may_charge(&self, schedule: crate::exit_policy::FeeSchedule) -> bool {
        self.config.fee_schedule_replay || schedule == crate::exit_policy::official_schedule()
    }

    /// #496: price a RESTING (maker) exit one tick above the best bid, clamped
    /// to stay strictly below the best ask — the only shape a post-only SELL
    /// can be accepted at. A sell AT the bid crosses the book (`Book::crosses`
    /// on the sell side is `best_bid >= price`), so the venue refuses the POST
    /// every time; a sell at/above the ask crosses the other side. Without a
    /// live book there is nothing to rest against: the judged price stands and
    /// the venue's own refusal (or dry's book aging) reports it.
    pub(super) fn maker_exit_price(&self, token: &str, judged: Decimal) -> Decimal {
        let tick = Decimal::new(1, 2); // 0.01 — the binary-market price grid
        let Some(book) = self.books.get(token) else {
            return judged;
        };
        let Some(bid) = book.best_bid() else {
            return judged;
        };
        let mut price = bid + tick;
        if let Some(ask) = book.best_ask()
            && ask > Decimal::ZERO
            && price >= ask
        {
            // A one-tick spread leaves no inside price: the closest legal
            // rest would be above the bid but there is no grid point strictly
            // between bid and ask. The band is too tight to rest honestly —
            // the judged price stands and the venue's answer reports it.
            if ask > bid + tick {
                price = ask - tick;
            } else {
                return judged;
            }
        }
        price.min(Decimal::new(99, 2))
    }
}

/// Price a taker EXIT to actually fill: the limit nudged a few ticks below
/// the executable bid, floored at the venue minimum. Polymarket prices run in
/// 0.01 ticks, so a tick is 0.01.
pub(crate) fn marketable_exit_price(executable_bid: Decimal, slip_ticks: i64) -> Decimal {
    if executable_bid <= Decimal::ZERO {
        return executable_bid;
    }
    let slipped = executable_bid - Decimal::new(slip_ticks.max(0), 2);
    slipped.max(Decimal::new(1, 2))
}
