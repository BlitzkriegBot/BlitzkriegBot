//! One venue book marshalled into the ABI `BkBookView` form: every CString
//! is owned here, and the raw level pointers handed across the ABI borrow
//! from `bid_levels`/`ask_levels`, which outlive the FFI call.
//! Moved verbatim from `foreign.rs` (pure code motion).

use crate::model::OrderbookSnapshot;
use blitzkrieg_strategy_api::{BkBookView, BkLevel};
use rust_decimal::Decimal;
use std::ffi::CString;

/// Keeps every CString backing one `BkBookView` alive for the duration of the
/// FFI call. The view's pointers borrow from this struct.
pub(super) struct BookMarshal {
    symbol: CString,
    asset: CString,
    best_bid: CString,
    best_ask: CString,
    mid: CString,
    bid_depth: CString,
    ask_depth: CString,
    obi: CString,
    spread: CString,
    spread_pct: CString,
    /// Owning storage for every ladder level's price/size CStrings. The raw
    /// pointers handed across the ABI in [`Self::view`] borrow from these, so
    /// they must outlive the FFI call (they do — self lives across it).
    bid_levels: Vec<(CString, CString)>,
    ask_levels: Vec<(CString, CString)>,
}

impl BookMarshal {
    pub(super) fn new(token_id: &str, asset: &str, s: &OrderbookSnapshot) -> Option<Self> {
        let cs = |d: Decimal| CString::new(d.to_string()).unwrap_or_default();
        let mk = |v: &[(Decimal, Decimal)]| {
            v.iter()
                .map(|(p, sz)| (cs(*p), cs(*sz)))
                .collect::<Vec<_>>()
        };
        let bid_levels = mk(&s.bids);
        let ask_levels = mk(&s.asks);
        Some(Self {
            symbol: CString::new(token_id).ok()?,
            asset: CString::new(asset).ok()?,
            best_bid: cs(s.best_bid),
            best_ask: cs(s.best_ask),
            mid: cs(s.mid_price),
            bid_depth: cs(s.bid_depth),
            ask_depth: cs(s.ask_depth),
            obi: cs(s.obi),
            spread: cs(s.spread),
            spread_pct: cs(s.spread_pct),
            bid_levels,
            ask_levels,
        })
    }

    /// Raw FFI level arrays borrowing from self's owned CStrings. The caller
    /// must bind the returned vecs and keep them alive across the FFI call.
    pub(super) fn level_views(&self) -> (Vec<BkLevel>, Vec<BkLevel>) {
        let to = |v: &[(CString, CString)]| {
            v.iter()
                .map(|(p, sz)| BkLevel {
                    price: p.as_ptr(),
                    size: sz.as_ptr(),
                })
                .collect()
        };
        (to(&self.bid_levels), to(&self.ask_levels))
    }

    pub(super) fn view(&self, bids: &[BkLevel], asks: &[BkLevel], timestamp_ms: i64) -> BkBookView {
        BkBookView {
            symbol: self.symbol.as_ptr(),
            asset: self.asset.as_ptr(),
            bids: if bids.is_empty() {
                std::ptr::null()
            } else {
                bids.as_ptr()
            },
            bid_count: bids.len(),
            asks: if asks.is_empty() {
                std::ptr::null()
            } else {
                asks.as_ptr()
            },
            ask_count: asks.len(),
            best_bid: self.best_bid.as_ptr(),
            best_ask: self.best_ask.as_ptr(),
            mid: self.mid.as_ptr(),
            bid_depth: self.bid_depth.as_ptr(),
            ask_depth: self.ask_depth.as_ptr(),
            obi: self.obi.as_ptr(),
            spread: self.spread.as_ptr(),
            spread_pct: self.spread_pct.as_ptr(),
            timestamp_ms,
        }
    }
}
