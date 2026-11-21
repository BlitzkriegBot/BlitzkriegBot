//! Core data shapes the strategy logic operates on: orderbook snapshots,
//! directions and token ids. Kept in sync with the kernel's `model` module —
//! the kernel's types are structurally identical, and the engine marshals its
//! snapshots into these before handing them to shared logic.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A token/contract identifier. Newtype-alias so signatures read the same as
/// the kernel's.
pub type TokenId = String;

/// Which side of a binary market a position/signal is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalDirection {
    Up,
    Down,
}

impl SignalDirection {
    pub fn as_str(&self) -> &'static str {
        match self {
            SignalDirection::Up => "up",
            SignalDirection::Down => "down",
        }
    }
}

/// A snapshot of the top-of-book plus derived depth/obi metrics. Mirrors the
/// kernel's `OrderbookSnapshot` (and the original TS `buildOrderbookSnapshot`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderbookSnapshot {
    pub token_id: TokenId,
    pub bids: Vec<(Decimal, Decimal)>,
    pub asks: Vec<(Decimal, Decimal)>,
    #[serde(with = "crate::decimal")]
    pub bid_depth: Decimal,
    #[serde(with = "crate::decimal")]
    pub ask_depth: Decimal,
    #[serde(with = "crate::decimal")]
    pub obi: Decimal,
    #[serde(with = "crate::decimal")]
    pub spread: Decimal,
    #[serde(with = "crate::decimal")]
    pub spread_pct: Decimal,
    #[serde(with = "crate::decimal")]
    pub best_bid: Decimal,
    #[serde(with = "crate::decimal")]
    pub best_ask: Decimal,
    #[serde(with = "crate::decimal")]
    pub mid_price: Decimal,
    pub timestamp: i64,
}

impl OrderbookSnapshot {
    /// Build from sorted level vectors, computing depth/obi/spread like the TS
    /// `buildOrderbookSnapshot`.
    pub fn from_levels(
        token_id: impl Into<TokenId>,
        mut bids: Vec<(Decimal, Decimal)>,
        mut asks: Vec<(Decimal, Decimal)>,
        timestamp: i64,
    ) -> Self {
        bids.sort_by_key(|b| std::cmp::Reverse(b.0));
        asks.sort_by_key(|a| a.0);
        Self::from_sorted_levels(token_id, bids, asks, timestamp)
    }

    /// The same construction for input the caller guarantees is already sorted:
    /// `bids` best-first (descending price), `asks` best-first (ascending).
    /// The BTreeMap-backed local book always produces levels in that order, so
    /// its hot per-event snapshot path skips the redundant O(n log n) sort.
    pub fn from_sorted_levels(
        token_id: impl Into<TokenId>,
        bids: Vec<(Decimal, Decimal)>,
        asks: Vec<(Decimal, Decimal)>,
        timestamp: i64,
    ) -> Self {
        let bid_depth: Decimal = bids.iter().map(|(_, s)| *s).sum();
        let ask_depth: Decimal = asks.iter().map(|(_, s)| *s).sum();
        let total = bid_depth + ask_depth;
        let obi = if total > Decimal::ZERO {
            (bid_depth - ask_depth) / total
        } else {
            Decimal::ZERO
        };
        let best_bid = bids.first().map(|(p, _)| *p).unwrap_or(Decimal::ZERO);
        // The ONE sentinel when no asks quote is pre-existing API semantics
        // (entry-side consumers treat "nothing for sale" as maximally
        // expensive); do not change it here — only the MID gains side-awareness.
        let best_ask = asks.first().map(|(p, _)| *p).unwrap_or(Decimal::ONE);
        // F6: the mid is only meaningful when BOTH sides quote. With no bids the
        // old `(0 + ask)/2` arithmetic manufactured a phantom price (ask 0.90 →
        // "mid" 0.45) that downstream consumers could mistake for a sellable
        // level; a one-sided book must not present a tradeable mid at all.
        let both_sides_quote = !bids.is_empty()
            && !asks.is_empty()
            && best_bid > Decimal::ZERO
            && best_ask > Decimal::ZERO;
        let mid_price = if both_sides_quote {
            (best_bid + best_ask) / Decimal::TWO
        } else {
            Decimal::ZERO
        };
        let spread = if both_sides_quote {
            best_ask - best_bid
        } else {
            Decimal::ZERO
        };
        let spread_pct = if mid_price > Decimal::ZERO {
            (spread / mid_price) * Decimal::ONE_HUNDRED
        } else {
            Decimal::ZERO
        };
        Self {
            token_id: token_id.into(),
            bids,
            asks,
            bid_depth,
            ask_depth,
            obi,
            spread,
            spread_pct,
            best_bid,
            best_ask,
            mid_price,
            timestamp,
        }
    }

    /// Widest spread at which `(best_bid + best_ask) / 2` is still a
    /// tradeable reference on a 0–1 binary. A near-expiry stub quote of
    /// 0.01 / 1.00 has mid 0.505, which is not a real market price (the
    /// venue midpoint is already 0.995 or 0.005). Consumers that would
    /// pass that figure off as live violate L1.
    pub const MAX_RELIABLE_MID_SPREAD: Decimal = rust_decimal_macros::dec!(0.20);

    /// Mid only when both sides quote a tight enough spread. One-sided
    /// books already have `mid_price == 0` (F6). A two-sided but
    /// pathologically wide book also returns `None` so a caller cannot
    /// treat 0.505 as a live quote.
    pub fn reliable_mid(&self) -> Option<Decimal> {
        if self.mid_price <= Decimal::ZERO {
            return None;
        }
        if self.bids.is_empty() || self.asks.is_empty() {
            return None;
        }
        let spread = self.best_ask - self.best_bid;
        if spread > Self::MAX_RELIABLE_MID_SPREAD {
            return None;
        }
        Some(self.mid_price)
    }
}

#[cfg(test)]
mod reliable_mid_tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn snap(bid: Decimal, ask: Decimal) -> OrderbookSnapshot {
        OrderbookSnapshot::from_levels("t", vec![(bid, dec!(10))], vec![(ask, dec!(10))], 0)
    }

    #[test]
    fn tight_binary_book_has_a_reliable_mid() {
        let s = snap(dec!(0.44), dec!(0.46));
        assert_eq!(s.mid_price, dec!(0.45));
        assert_eq!(s.reliable_mid(), Some(dec!(0.45)));
    }

    #[test]
    fn near_expiry_stub_book_is_not_a_reliable_mid() {
        // The observed panel failure: bid 0.01 / ask 1.00 → mid 0.505,
        // while the CLOB midpoint was 0.995 or 0.005.
        let s = snap(dec!(0.01), dec!(1.00));
        assert_eq!(s.mid_price, dec!(0.505));
        assert_eq!(s.reliable_mid(), None);
    }

    #[test]
    fn one_sided_book_has_no_reliable_mid() {
        let s = OrderbookSnapshot::from_levels("t", vec![(dec!(0.99), dec!(10))], vec![], 0);
        assert_eq!(s.mid_price, Decimal::ZERO);
        assert_eq!(s.reliable_mid(), None);
    }

    #[test]
    fn spread_at_the_cap_is_still_reliable() {
        let s = snap(dec!(0.40), dec!(0.60));
        assert_eq!(s.reliable_mid(), Some(dec!(0.50)));
    }

    #[test]
    fn spread_just_over_the_cap_is_not_reliable() {
        let s = snap(dec!(0.39), dec!(0.60));
        assert!(s.mid_price > Decimal::ZERO);
        assert_eq!(s.reliable_mid(), None);
    }
}
