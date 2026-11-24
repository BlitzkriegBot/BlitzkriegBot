//! The typed price-observation state of an open position (issue #176).
//!
//! WHY THIS TYPE EXISTS — the #176 defect in one line: `OpenPosition` used to
//! carry a bare `current_price: Decimal` SEEDED FROM `entry_price` at open. A
//! dead feed therefore left the ENTRY price standing as the position's mark, and
//! the expiry fallback thresholded that entry price into a binary `1`/`0`
//! estimate — a settlement verdict nobody ever observed, worth up to
//! `1.00 × shares` on the books. That is R7.1 **L1** (编造判词): the number the
//! ledger books was not traceable to any venue quote or settlement.
//!
//! Two different facts shared one scalar: "nobody has quoted this token yet"
//! and "the last quote was 0.00". `0` doubled as the "no value" sentinel while
//! `entry_price` filled the "initial value" slot, so the code could not tell an
//! observation from a restatement of the cost basis. Neither the estimator nor
//! a reviewer could have known which one they were holding.
//!
//! `PriceState` makes "unobserved" a FIRST-CLASS state, which is the only thing
//! that structurally prevents the entry price from coming back:
//!
//!   * opening a position leaves it [`PriceState::Unobserved`] — `entry_price`
//!     is never written here, so there is no code path that can substitute it;
//!   * only a real book observation transitions it to
//!     [`PriceState::Observed`], and that observation carries its own clock
//!     (`at_ms`) and its provenance ([`PriceSource`]);
//!   * the settlement estimate derives its basis from this type
//!     ([`EstimateBasis`]), which has no `EntryPrice` variant — a variant that
//!     does not exist cannot be selected by a future edit.
//!
//! Deliberately NOT a `Decimal` plus a bool: "observed at 0.00" and
//! "never observed" are different facts with different accounting
//! consequences (the first is a real `0`-payout estimate, the second is an
//! unknown that must ride the receivable pool and be reported).

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// WHERE an observed price came from. Both variants are real feed
/// observations; there is no "entry", "last mid of yesterday", or "mean"
/// member, because none of those is evidence of what a buyer is offering now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceSource {
    /// The book's best bid — the strongest reference: it is what a seller
    /// could actually have crossed.
    LastBid,
    /// The mid of a TWO-SIDED book. Weaker than a bid (nobody is bidding, but
    /// both sides quoted), so it is named apart rather than folded into
    /// [`Self::LastBid`]. A one-sided book has no mid at all (the F6 phantom
    /// `(0 + ask)/2` is refused upstream), so this can never stand for "only
    /// an ask exists".
    BookMid,
}

impl PriceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LastBid => "last_bid",
            Self::BookMid => "book_mid",
        }
    }
}

/// The position's price state — the ONE place a mark can live.
///
/// `Unobserved` is what a freshly opened position carries and what it keeps
/// until a book actually quotes the token. It is NOT a zero and NOT the entry
/// price; every consumer must decide explicitly what "no observation" means for
/// it (display: show no mark; settlement: book zero at `unobserved`; a
/// protective stop: no judgement to make).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum PriceState {
    /// No book has quoted this token since the position opened. The only
    /// honest thing to say about its current price.
    #[default]
    Unobserved,
    /// A real feed observation: the price, which side of the book it came
    /// from, and the instant the book recorded it. Field names follow the
    /// rest of the position record (camelCase); the money value uses
    /// `crate::decimal` so it stays a JSON number for finite values.
    #[serde(rename_all = "camelCase")]
    Observed {
        #[serde(with = "crate::decimal")]
        price: Decimal,
        source: PriceSource,
        /// The observation's OWN clock — the book's receive time, never the
        /// time a later caller happened to ask. `0` means the book carried no
        /// clock (synthetic/replay fixtures); that is "unknown age", which is a
        /// different statement from "age zero" and is kept apart here. The
        /// `at_ms` alias keeps hand-written snake_case fixtures loadable.
        #[serde(alias = "at_ms")]
        at_ms: i64,
    },
}

impl PriceState {
    /// A best-bid observation at the book's own receive time.
    pub fn observed_bid(price: Decimal, at_ms: i64) -> Self {
        Self::Observed {
            price,
            source: PriceSource::LastBid,
            at_ms,
        }
    }

    /// A two-sided-mid observation at the book's own receive time.
    pub fn observed_mid(price: Decimal, at_ms: i64) -> Self {
        Self::Observed {
            price,
            source: PriceSource::BookMid,
            at_ms,
        }
    }

    /// The observed price, or `None` when nothing was ever observed. Callers
    /// that need a number must say what "unknown" means for them — this never
    /// hands back a sentinel that could be mistaken for a quote.
    pub fn price(&self) -> Option<Decimal> {
        match self {
            Self::Unobserved => None,
            Self::Observed { price, .. } => Some(*price),
        }
    }

    /// The observation's own clock, or `None` when unobserved.
    pub fn observed_at_ms(&self) -> Option<i64> {
        match self {
            Self::Unobserved => None,
            Self::Observed { at_ms, .. } => Some(*at_ms),
        }
    }

    pub fn source(&self) -> Option<PriceSource> {
        match self {
            Self::Unobserved => None,
            Self::Observed { source, .. } => Some(*source),
        }
    }

    pub fn is_unobserved(&self) -> bool {
        matches!(self, Self::Unobserved)
    }
}

/// WHAT an unverifiable-expiry estimate was based on (issue #176).
///
/// Structurally excludes the entry price: the old code path fed
/// `current_price == entry_price` into the binary threshold, which turned an
/// unobserved position into a `1.00`-per-share "estimate" out of the cost
/// basis. There is no `EntryPrice` variant here, so that path cannot be
/// re-created by adding an arm — it would have to add a variant, which is a
/// reviewed contract change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateBasis {
    /// The estimate thresholded a real best bid.
    LastBid,
    /// The estimate thresholded a real two-sided mid.
    BookMid,
    /// No observation existed. The booked price is `0` and the notional rides
    /// the receivable pool — explicitly NOT a settled outcome.
    NoObservation,
}

impl EstimateBasis {
    /// The `price_source` spelling this basis writes onto the closed record.
    ///
    /// Both observed bases keep the historical `binary_threshold` string: that
    /// label names the RULE (the price was mapped into the binary settlement
    /// domain `{0, 1}`), and every existing audit/parity reader keys on it.
    /// WHICH side of the book the input came from lives on
    /// [`PriceState::source`] while the position is open; changing the closed
    /// record's spelling would be a wire change this fix does not need.
    pub fn as_price_source(self) -> &'static str {
        match self {
            Self::LastBid | Self::BookMid => crate::settlement::PRICE_SOURCE_BINARY_THRESHOLD,
            Self::NoObservation => crate::settlement::PRICE_SOURCE_UNOBSERVED,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// The whole point of the type: "never observed" cannot produce a price,
    /// so no caller can threshold it into a verdict.
    #[test]
    fn unobserved_has_no_price_and_no_clock() {
        let s = PriceState::Unobserved;
        assert_eq!(s.price(), None);
        assert_eq!(s.observed_at_ms(), None);
        assert_eq!(s.source(), None);
        assert!(s.is_unobserved());
        assert_eq!(EstimateBasis::NoObservation.as_price_source(), "unobserved");
    }

    /// An observation carries its provenance and its OWN clock — a later
    /// caller's `now` can never be substituted for the book's time.
    #[test]
    fn an_observation_keeps_its_source_and_clock() {
        let bid = PriceState::observed_bid(dec!(0.95), 1_234);
        assert_eq!(bid.price(), Some(dec!(0.95)));
        assert_eq!(bid.observed_at_ms(), Some(1_234));
        assert_eq!(bid.source(), Some(PriceSource::LastBid));

        let mid = PriceState::observed_mid(dec!(0.95), 5_678);
        assert_eq!(mid.source(), Some(PriceSource::BookMid));
        assert_eq!(mid.observed_at_ms(), Some(5_678));
    }

    /// A legacy `positions.jsonl` line (pre-#176: `currentPrice` / `prevPrice`,
    /// no `priceState`) MUST load — but as `Unobserved`, never as an
    /// observation. The legacy mark may be the entry-price artifact this issue
    /// removes, so re-admitting it as a quote would re-open the very defect.
    #[test]
    fn a_legacy_snapshot_loads_unobserved_not_as_a_quote() {
        let state: PriceState = serde_json::from_str(r#"{"kind":"unobserved"}"#).unwrap();
        assert!(state.is_unobserved());

        // The serde default is the same state, which is what a missing field
        // becomes.
        assert_eq!(PriceState::default(), PriceState::Unobserved);
    }

    /// The persisted shape round-trips with the observation's facts intact.
    /// `price` is a JSON NUMBER (the `crate::decimal` spelling the rest of the
    /// position record uses), `at_ms` keeps its snake_case name on this
    /// camelCase container (the field is already unambiguous).
    #[test]
    fn observed_state_round_trips() {
        let s = PriceState::observed_bid(dec!(0.37), 42);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"observed","price":0.37,"source":"last_bid","atMs":42}"#
        );
        assert_eq!(serde_json::from_str::<PriceState>(&json).unwrap(), s);
        // Both spellings load: the camelCase name this build writes and the
        // snake_case one a hand-written fixture may carry.
        let snake = r#"{"kind":"observed","price":0.37,"source":"last_bid","at_ms":42}"#;
        assert_eq!(serde_json::from_str::<PriceState>(snake).unwrap(), s);
    }
}
