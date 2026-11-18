//! #862: MarketRegime labelling of closed trades — the strategy_logic reference
//! classifier fed each token's own mid path.
//! Pure code motion from `backtest.rs` (no logic edits).

use crate::engine::DataEvent;
use rust_decimal::Decimal;
use strategy_logic::market_regime::{MarketRegime, MarketRegimeConfig};

use super::driver::EventBacktester;

impl EventBacktester {
    /// #862: feed the regime state machine the mid of every book/top the
    /// replay delivers. The mid is read exactly like [`crate::sim::Book::mid`]
    /// (best bid/ask, ask must be positive) so the label and the matcher read
    /// the same book the same way; books without two-sided quotes feed nothing.
    pub(crate) fn observe_regime_mid(&mut self, ev: &DataEvent, at_ms: i64) {
        let mid_of = |bids: &[(Decimal, Decimal)], asks: &[(Decimal, Decimal)]| {
            let best = |levels: &[(Decimal, Decimal)], max: bool| {
                levels
                    .iter()
                    .map(|(p, _)| *p)
                    .reduce(|a, b| if max { a.max(b) } else { a.min(b) })
            };
            match (best(bids, true), best(asks, false)) {
                (Some(b), Some(a)) if a > Decimal::ZERO => Some((b + a) / Decimal::TWO),
                _ => None,
            }
        };
        let mut feed = |token: &str, mid: Option<Decimal>| {
            if let Some(mid) = mid {
                let machine = self
                    .regimes
                    .entry(token.to_string())
                    .or_insert_with(|| MarketRegime::new(MarketRegimeConfig::default()));
                machine.on_price(at_ms, mid);
            }
        };
        match ev {
            DataEvent::Book {
                token_id,
                bids,
                asks,
                ..
            } => feed(token_id, mid_of(bids, asks)),
            DataEvent::Books { books, .. } => {
                for (token_id, bids, asks) in books {
                    feed(token_id, mid_of(bids, asks));
                }
            }
            DataEvent::TopOfBook {
                token_id,
                best_bid,
                best_ask,
                ..
            } => {
                if let (Some(b), Some(a)) = (best_bid, best_ask)
                    && *a > Decimal::ZERO
                {
                    let mid = (*b + *a) / Decimal::TWO;
                    feed(token_id, Some(mid));
                }
            }
            _ => {}
        }
    }

    /// #862: the regime label stamped on a closed trade — the token's own
    /// state machine's verdict at the close. `"none"` when the token never
    /// produced a mid (no book reached the machine): a bucket that says
    /// "none" is a coverage fact, never a made-up regime.
    pub(super) fn regime_of(&self, token: &str) -> String {
        self.regimes
            .get(token)
            .map(|m| m.state().as_str().to_string())
            .unwrap_or_else(|| "none".into())
    }
}
