//! One evaluation context: the round's priceable books marshalled to the ABI,
//! plus the `BkEvalCtx` that borrows from them.
//! Moved verbatim from `foreign.rs` (pure code motion).

use super::book_marshal::BookMarshal;
use crate::model::{CryptoMarket, OrderbookSnapshot};
use blitzkrieg_strategy_api::{BkEvalCtx, BkLevel, BkMarket, BkRound, BkRoundView, BkTokenBook};
use std::ffi::CString;

/// Backing storage for one evaluation context: the round tokens' priceable
/// books marshalled to the ABI form, kept alive across the FFI call. The
/// `BkEvalCtx` handed to the library borrows from here.
pub(super) struct EvalCtxMarshal {
    /// Token id per row (the BkTokenBook.token pointer borrows from these).
    tokens: Vec<CString>,
    books: Vec<BookMarshal>,
}

/// The (ctx, rows, views) triple `build` hands out: `ctx` borrows from `rows`
/// and `views`, and all three must outlive the FFI call they were built for.
pub(super) type BoundCtx = (
    BkEvalCtx,
    Vec<BkTokenBook>,
    Vec<(Vec<BkLevel>, Vec<BkLevel>)>,
);

impl EvalCtxMarshal {
    /// Marshal the context exactly as an in-tree strategy would see it. The
    /// in-tree `StrategyCtx::fresh_book` returns Some(snapshot) for a priceable
    /// book and None for a stale/missing one — with no way to tell those two
    /// apart — so the bound context carries exactly the priceable rows
    /// (`fresh = 1`); a token with no row is "not priceable now", identical to
    /// what `fresh_book(..) == None` means on the trait side.
    pub(super) fn new(
        markets: &[CryptoMarket],
        fresh_book: &dyn Fn(&str) -> Option<OrderbookSnapshot>,
        now_ms: i64,
    ) -> Self {
        let mut tokens = Vec::new();
        let mut books = Vec::new();
        for m in markets {
            for token in [&m.up_token_id, &m.down_token_id] {
                let Some(snap) = fresh_book(token) else {
                    continue;
                };
                let Some(marshal) = BookMarshal::new(token, &m.asset, &snap) else {
                    continue;
                };
                tokens.push(CString::new(token.as_str()).unwrap_or_default());
                books.push(marshal);
            }
        }
        let _ = now_ms;
        Self { tokens, books }
    }

    /// Build the borrowed `BkEvalCtx` + the arrays it points into. The caller
    /// must keep the returned (ctx, rows, views) tuple alive across the FFI
    /// call — `ctx`'s pointers borrow from `rows` and `views`.
    pub(super) fn build(
        &self,
        round: BkRound,
        markets: *const BkMarket,
        market_count: usize,
    ) -> BoundCtx {
        let views: Vec<(Vec<BkLevel>, Vec<BkLevel>)> = self
            .books
            .iter()
            .map(|b| {
                let (bids, asks) = b.level_views();
                (bids, asks)
            })
            .collect();
        let rows: Vec<BkTokenBook> = self
            .tokens
            .iter()
            .zip(self.books.iter())
            .zip(views.iter())
            .map(|((token, b), (bids, asks))| BkTokenBook {
                token: token.as_ptr(),
                book: b.view(bids, asks, round.now_ms),
                fresh: 1,
            })
            .collect();
        let ctx = BkEvalCtx {
            view: BkRoundView {
                round,
                markets,
                market_count,
            },
            books: if rows.is_empty() {
                std::ptr::null()
            } else {
                rows.as_ptr()
            },
            book_count: rows.len(),
        };
        (ctx, rows, views)
    }
}
