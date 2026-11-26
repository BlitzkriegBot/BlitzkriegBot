//! # The trades domain of the store seam — closed-trade truth in SQL
//!
//! ## The defect this closes
//!
//! Closed trades are the ONLY money fact with no SQL carrier. Everything a book
//! needs to answer "what did this account realize" lives in
//! `data/trades/trades.jsonl`, read by whole-file `std::fs::read_to_string` and
//! folded in memory on every call:
//!
//! * `crate::store::StoreApi` (`core/blitzkrieg_core/src/store/mod.rs:49`) has
//!   **no trade method at all** — grep `fn .*trade` over that file: 0 hits. The
//!   trait cannot express this domain, so no backend can carry it.
//! * `grep -c trades core/blitzkrieg_core/src/store/sqlite.rs` → **0**. Even if
//!   it could, nothing implements it.
//! * `grep -c trades core/blitzkrieg_core/src/db/schema.rs` → the v1 DDL and its
//!   v2 `DROP`. `schema.rs:14-20` records why: `orders` / `fills` / `positions` /
//!   `trades` / `ledger_entries` "never had a runtime writer … Keeping them made
//!   the schema a lying surface — it advertised storage nothing reads."
//!
//! So the gap is **designed, not forgotten**: at v2 the team chose "delete the
//! table rather than let it lie", and the trade log stayed in JSONL. That was
//! the right call for a table nobody wrote; it is the wrong end state for the
//! one ledger the panel's headline number comes from, because it leaves the
//! books with **two shapes of truth** — SQL for funding, JSONL for realized PnL
//! — and no way to ask one question across both.
//!
//! ## What this contract adds
//!
//! Three reads and four writes, all on the frozen [`StoreRead`] / [`StoreWrite`]
//! faces from [`super::store`] so there is still exactly one seam:
//!
//! | Face | Method | Replaces |
//! |---|---|---|
//! | read | [`TradeRead::load_trades`] | `trade_db::read_page` (whole-file read) |
//! | read | [`TradeRead::trade_totals`] | `TradeDb::fold_into` + `reports.rs`'s second fold |
//! | read | [`TradeRead::max_trade_id_tail`] | `TradeDb::max_hft_tail` (boot scan) |
//! | write | [`TradeWrite::append_trade`] | `TradeDb::record` (`closing.rs:25`) |
//! | write | [`TradeWrite::retract_trade`] | `TradeDb::retract` |
//! | write | [`TradeWrite::amend_trade_net_pnl`] | `TradeDb::amend_net_pnl` (`fallback.rs:364`) |
//! | write | [`TradeWrite::retract_account_trades`] | `TradeDb::retract_account` (`trading_reset.rs:305`) |
//!
//! **Issue:** P0-6 follow-on (the trades domain T4 left out). **Consuming
//! task:** T4-trades. **Operator ruling this follows:** 「契约先行：凡涉及核心
//! 接口（如 StoreApi、IPC、DataFeed）的变更，必须先提交 Trait/Struct 定义，
//! 经运营者审批后再写实现」.
//!
//! ## Three口径 divergences this contract forces into the open
//!
//! The contract does not invent new numbers. It names places where TWO live
//! producers already disagree, because a carrier-computed projection is the only
//! way to stop the disagreement from being re-created in SQL:
//!
//! 1. **`avgHoldTimeSec`** — `TradeDb::fold_into`
//!    (`core/blitzkrieg_core/src/trade_db.rs:321-322`) divides the running mean
//!    by `total_trades`; `reports.rs` (`:112` sums, `:128` divides) sums every
//!    row's `holdTimeSec`
//!    then divides by `decided` (wins + losses, `:115`). Same field, two
//!    answers, and the second one is not a mean of the closed set. [`TradeTotals`]
//!    states one:
//!    the mean over **all closed trades**, because that is what "average hold
//!    time" means.
//! 2. **`bestTradePnl` / `worstTradePnl`** — `fold_into` starts from `0.0`
//!    (`TradeSummary::default`), so an all-losing account reports
//!    `best = 0.00`, a trade it never made. `reports.rs` starts from
//!    `±INFINITY` and maps empty to `0.0`. [`TradeTotals`] uses `Option`, so
//!    "no trades" is `None` and no consumer can render a fabricated zero
//!    (R7.1 L4/L5).
//! 3. **`winRate` when nothing is decided** — `TradeSummary.win_rate` is `f64`
//!    and documents `0.0` as its "no data" spelling, which is the same value a
//!    real 0-win record produces. [`TradeTotals`] derives the rate from
//!    [`Tally::win_rate`] → `Option<f64>`, so 「还没有分胜负的笔」 is `None`.
//!
//! ## The R7.1 constraint that shapes every field
//!
//! Fallback-closed rows (`source == "fallback"`, `price_source ==
//! "unobserved"`) are trades whose exit price has **no venue answer**. They must
//! be visible as their own line (「未核实敞口」单列) and they must **stay inside
//! the totals**: [`TradeTotals::unverified`] is a SUBSET of
//! [`TradeTotals::net_pnl`], never something a consumer subtracts. Excluding
//! them from the headline would make a book look better than it is, which is
//! R7.1 L3 — the `#969` / `5d75f58a` failure mode, where an unverifiable loss
//! got dressed up until it read as a smaller loss.
//!
//! ## What this contract deliberately does NOT do
//!
//! - **No wire change.** `TradeSummary` stays the DTO `trades.summary` returns
//!   until T9 re-derives it from [`TradeTotals`]; that adapter is T9's, and
//!   changing a wire field here would be a second change hiding in a contract.
//! - **No 记账口径 change (R7).** The same rows, the same net PnL, a different
//!   carrier. Nothing in this module decides what a trade is worth.
//! - **No risk or funding surface.** No threshold, no limit, no sizing — R6
//!   untouched.
//! - **No cursor.** The live wire speaks `limit` + `offset`; the frozen
//!   [`super::order::PageRequest`] speaks an opaque cursor. Inventing a third
//!   pagination vocabulary here would leave two frozen shapes for one question,
//!   so this contract takes plain arguments and leaves the cursor migration to
//!   T9 (which already owns it). `NonZeroU32` is adopted from `PageRequest`
//!   because it is the one part of that contract that must not wait: it makes
//!   "unbounded read" unrepresentable.

use std::num::NonZeroU32;

use rust_decimal::Decimal;

use crate::contracts::metrics::Tally;
use crate::contracts::store::{AccountScope, StoreError, StoreTx, WriteOutcome};
use crate::model::AccountId;

// The row type is the EXISTING one, aliased rather than redefined — the same
// move `super::store` makes with `AccountRecord = Account`. A second trade row
// shape would be a second thing to keep in sync with the panel's wire format.
//
// T4-trades: `TradeRecord` moved out of `crate::trade_db` (the retired JSONL
// carrier, now deleted) into [`crate::trade`] — the row shape is the surviving
// contract, so its home is not the module it replaced.
pub use crate::trade::TradeRecord;

/// Which rows a trade read addresses.
///
/// **Abolishes:** the implicit "no filter" of `trade_db::read_page`, which
/// always returned every account's rows because a JSONL file has no account
/// dimension to filter on — so the panel's global headline and the per-account
/// accounting audit read the SAME call and had to re-filter in memory
/// (`reports.rs:54-66`).
/// **Replaced by:** an explicit scope at the seam, so "this account's trades"
/// and "the whole book" are two different questions in the type.
/// **Issue:** P0-6 follow-on.
///
/// `AllAccounts` is a real state, not a legacy wart: `trades.summary` with no
/// `accountId` is the panel's headline, and it is defined as the fold over every
/// book. The two variants exist because the answers differ, not because one is
/// deprecated.
#[derive(Debug, Clone, Copy)]
pub enum TradeScope<'a> {
    /// Exactly one account's rows (#1221/#1223 bug class: an unscoped money
    /// question is the defect).
    Account(AccountScope<'a>),
    /// Every account in this carrier's book. Only for the global headline.
    AllAccounts,
}

/// The address of one closed-trade row.
///
/// **Abolishes:** `TradeDb::retract`'s match triple `(id, exit_time,
/// net_pnl_usd)` (`core/blitzkrieg_core/src/trade_db.rs:387`), which had to
/// include the moving field `net_pnl_usd` because a JSONL line has no stable
/// identity — the extra predicate was the only way to avoid eating a different
/// close that reused the id across restarts (issue #942 records nine such
/// collisions in production).
/// **Replaced by:** this two-field address, scoped by account. In a table the
/// uniqueness is a constraint, not a predicate, so the address must not contain
/// a field that [`TradeWrite::amend_trade_net_pnl`] mutates.
/// **Issue:** P0-6 follow-on / #942.
///
/// `(id, exit_time)` is the existing `TradeSummary`-era identity minus the
/// mutable leg. The migration from JSONL MUST detect rows that collide on
/// `(account, id, exit_time)` and **refuse to import**, naming the collisions —
/// silently collapsing them would delete a real close, which is R7.1 L2
/// (a trade that happened must not vanish from the books).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradeIdentity {
    /// The position book's id at close (`hft-N`), reused across restarts by
    /// design — see [`TradeRead::max_trade_id_tail`].
    pub id: String,
    /// The close's own timestamp in epoch milliseconds. Two closes of the same
    /// id cannot share it, and it does not move under an amendment.
    pub exit_time: i64,
}

/// Closed-trade totals for one scope, computed by the carrier.
///
/// **Abolishes:** two in-memory folds with drifting formulas —
/// `TradeDb::fold_into` (global + per-account buckets) and
/// `reports.rs`'s per-account re-fold over wire JSON — plus the
/// `recent_trades(0)` "read the whole log to count it" dependency that
/// `reports.rs:49` explicitly flags as version-sensitive.
/// **Replaced by:** this projection, so a count is a `COUNT(*)` and a sum is a
/// `SUM()` in the carrier, not a full read into kernel memory.
/// **Issue:** P0-6 follow-on. See the module docs for the three口径 forks it
/// settles.
///
/// Money is `Decimal` (DEV §7): the carrier sums decimal strings, it does not
/// float. The wire's `f64` spellings stay in `TradeSummary`, converted once at
/// the boundary by T9.
///
/// Partially ordered only (`PartialEq`, no `Eq`): `avg_hold_time_sec` is the one
/// `f64` leg — a mean, computed by the carrier. It is genuinely approximate, so
/// claiming total equality would be a false precision claim; every money leg
/// stays exact `Decimal`.
#[derive(Debug, Clone, PartialEq)]
pub struct TradeTotals {
    /// Which scope this was computed for. `None` = [`TradeScope::AllAccounts`].
    /// Carried so a consumer cannot attach a per-account number to the wrong
    /// row (the #1221/#1223 shape).
    pub account: Option<AccountId>,
    /// When the carrier computed this, epoch milliseconds (its own clock —
    /// event time, never "now" sampled by the caller).
    pub as_of_ms: i64,
    /// Wins / losses / flats. The win-rate input; the rate itself is derived,
    /// not stored, so the two can never disagree. Flats are excluded from the
    /// denominator by [`Tally::win_rate`] (#1277 single-口径).
    pub tally: Tally,
    /// Every row in scope, including flats and unverified closes.
    pub total_trades: u64,
    /// Realized PnL before fees, summed over every row in scope.
    pub gross_pnl: Decimal,
    /// Fees paid, summed over every row in scope.
    pub fees: Decimal,
    /// **The headline realized figure, summed over every row in scope.**
    ///
    /// Includes the unverified closes in [`Self::unverified`] — they are real
    /// booked moves and this number is already the ugly/conservative one.
    /// Never subtract them to get a smaller loss (R7.1 L3).
    pub net_pnl: Decimal,
    /// The unverified slice, **broken out and already counted inside
    /// [`Self::net_pnl`]** — R7.1「未核实敞口」单列. `None` = no unverified
    /// close in scope (a fact); never a zeroed placeholder.
    pub unverified: Option<UnverifiedTrades>,
    /// Mean hold time over every closed trade in scope. `None` = no rows
    /// (unknown, not `0.0`).
    pub avg_hold_time_sec: Option<f64>,
    /// Best single net PnL. `None` = no rows — so an all-losing account reports
    /// its least-bad loss instead of a fabricated `0.00`
    /// (see module docs, fork 2).
    pub best_trade_pnl: Option<Decimal>,
    /// Worst single net PnL. `None` = no rows.
    pub worst_trade_pnl: Option<Decimal>,
}

/// The unverified-closed slice of a [`TradeTotals`].
///
/// **Abolishes:** `TradeRecord.source` / `price_source` travelling to the panel
/// as two loose strings that no aggregation reads — so a book whose headline is
/// partly made of unverifiable closes is indistinguishable from one that is
/// fully venue-backed (`#987` / issue #1002 wrote the fields; nothing reported
/// them in aggregate).
/// **Replaced by:** this breakdown, so the fact is a first-class line in the
/// report the operator reads (R7.1「在权益与报表单列『未核实敞口』」).
/// **Issue:** P0-6 follow-on / R7.1.
///
/// **`net_pnl` here is a SUBSET of [`TradeTotals::net_pnl`], not a sibling of
/// it.** A consumer that wants "verified only" must subtract — and must then
/// label the result, because a verified-only figure is not the book's realized
/// PnL and presenting it as one is exactly the `#969` / `5d75f58a` move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnverifiedTrades {
    /// How many closes in scope carry no venue answer for their exit price.
    pub trades: u64,
    /// Their summed net PnL as booked (the conservative figure that is already
    /// inside `TradeTotals::net_pnl`).
    pub net_pnl: Decimal,
}

/// The read face of the trades domain.
///
/// **Abolishes:** the whole-file read
/// (`trade_db::read_page`, `core/blitzkrieg_core/src/trade_db.rs:572`:
/// `std::fs::read_to_string` then a slice) and the two reads that exist only
/// because a JSONL file cannot answer them — the per-account fold and the boot
/// scan for the highest id.
/// **Replaced by:** these three methods, each answerable by the carrier.
/// **Issue:** P0-6 follow-on.
///
/// Like [`super::store::StoreRead`], every method is **required** (no default
/// body): a carrier that cannot answer must say so, not inherit `Err`.
pub trait TradeRead: Send + Sync {
    /// One page of closed trades, newest window first in the result.
    ///
    /// `from_newest` skips that many of the most recent rows (`0` = the latest
    /// window), `limit` caps the window. Rows come back in **chronological
    /// order (oldest → newest)** inside the window, preserving the paging
    /// contract `trades.history` already publishes — this method replaces that
    /// implementation without changing what a page means.
    ///
    /// `limit` is `NonZeroU32` **because `0` meant "no cap"**
    /// (`core/blitzkrieg_core/src/trade_db.rs:564-568`) and that one initializer
    /// turned every snapshot into a read of the entire log. There is no spelling
    /// for "unbounded" here; a caller that wants a count asks
    /// [`Self::trade_totals`].
    ///
    /// `from_newest` past the oldest row is an **empty page, not an error** —
    /// that is the paginator's end-of-log signal (an absent row is a fact;
    /// [`StoreError::NotFound`] is for address-specific reads only).
    fn load_trades(
        &self,
        scope: TradeScope<'_>,
        from_newest: u64,
        limit: NonZeroU32,
    ) -> Result<Vec<TradeRecord>, StoreError>;

    /// Totals for the scope, computed by the carrier.
    ///
    /// An empty scope is a real answer (`total_trades == 0`, every `Option`
    /// `None`), not [`StoreError::NotFound`]: "this account has no closed
    /// trades" is a fact the panel must be able to render.
    fn trade_totals(&self, scope: TradeScope<'_>) -> Result<TradeTotals, StoreError>;

    /// The highest `hft-N` id tail this carrier has ever recorded, or `None`
    /// when it holds no `hft-` rows.
    ///
    /// Unscoped on purpose: this is the **ledger's** id memory, not an account's
    /// — `service/persistence.rs:195` seeds the position book's counter from it
    /// so a flat book does not reissue ids the closed history already carries
    /// (issue #942, 9 collisions observed in production). Filtering it by
    /// account would re-open that defect for a multi-account book.
    fn max_trade_id_tail(&self) -> Result<Option<u64>, StoreError>;
}

/// The write face of the trades domain.
///
/// **Abolishes:** the JSONL mutations that each rewrote the whole file through
/// `crate::jsonl::rewrite_atomic` and then **re-folded every surviving row in
/// memory** to rebuild the summary — `retract` (`trade_db.rs:397`),
/// `amend_net_pnl` (`:470`), `retract_account` (`:529`) — an O(file) operation
/// per trade, and the reason `scripts/store-layer-baseline.txt` carries **six**
/// `trade_db.rs` json-direct entries as known debt (`:331`, `:397`, `:427`,
/// `:470`, `:517`, `:529`).
/// **Replaced by:** row-level statements inside one [`StoreTx`], with the
/// totals derived by the carrier.
/// **Issue:** P0-6 follow-on / R7.1.
///
/// Every method takes `tx` for the same reason [`super::store::StoreWrite`]
/// does: a close is a multi-part fact (the row + whatever the caller's unit of
/// work already holds), and a half-landed unit is what the transaction handle
/// exists to prevent.
pub trait TradeWrite: Send + Sync {
    /// Append one closed trade.
    ///
    /// `scope` must address the same account as `record.account_id`; a mismatch
    /// is [`StoreError::Invalid`] — it means the caller is writing one book's
    /// trade into another's scope, which is the #1221/#1223 class.
    ///
    /// Re-appending an existing [`TradeIdentity`] is [`StoreError::Refused`],
    /// not an upsert: a close is an event, and silently replacing one would
    /// rewrite history (R7.1 L2). The intent ledger
    /// (`crate::db::intents`) is the precedent for "duplicate is an answer, not
    /// a write".
    ///
    /// `closing.rs:25` is the call site this replaces; it currently does
    /// `db.record(&rec, now_ms)` and swallows the outcome.
    fn append_trade(
        &self,
        tx: &mut StoreTx<'_>,
        scope: AccountScope<'_>,
        record: &TradeRecord,
    ) -> Result<WriteOutcome, StoreError>;

    /// Remove exactly one recorded trade — the retraction of a close that
    /// should never have been booked.
    ///
    /// No row at `identity` is [`StoreError::NotFound`]: the caller distinguishes
    /// "already retracted / never recorded" from a carrier failure, which the
    /// JSONL version could not do (it returned `()` and `return`ed silently).
    ///
    /// The row is **deleted, not compensated**: a trade that never happened must
    /// not live in the books as an offsetting entry the win-rate maths still
    /// counts (`TradeDb::retract`'s own wording, kept).
    fn retract_trade(
        &self,
        tx: &mut StoreTx<'_>,
        scope: AccountScope<'_>,
        identity: &TradeIdentity,
        now_ms: i64,
    ) -> Result<WriteOutcome, StoreError>;

    /// Move one recorded trade's net PnL to venue truth (issue #991).
    ///
    /// A fallback receivable's redemption moves cash by
    /// `venue_truth − booked_estimate`, and the anchored trade-leg identity
    /// demands Δrealized equal that delta — so the RECORD must move with the
    /// ledger.
    ///
    /// `expected_net` is a **compare-and-swap**: the amendment applies only if
    /// the row still carries the booked value the caller is correcting. This
    /// preserves the guard the JSONL version got from its match triple
    /// (`trade_db.rs:459`) now that the address no longer includes the moving
    /// field. A mismatch is [`StoreError::Refused`] and moves nothing.
    ///
    /// `delta` is added to both `net_pnl_usd` and `gross_pnl_usd`; the fee legs
    /// are unchanged — only the payout was estimated, not the fees
    /// (`fallback.rs:364` is the call site).
    fn amend_trade_net_pnl(
        &self,
        tx: &mut StoreTx<'_>,
        scope: AccountScope<'_>,
        identity: &TradeIdentity,
        expected_net: Decimal,
        delta: Decimal,
        now_ms: i64,
    ) -> Result<WriteOutcome, StoreError>;

    /// Erase one account's closed-trade history, leaving every other account's
    /// rows untouched (issue #561).
    ///
    /// Returns the receipt **and the archive that makes the erasure auditable**:
    /// issue #1253 established that "a reset which leaves no file-level trace of
    /// the trades it wipes is an unarchived erasure", and the JSONL version
    /// refuses the rewrite when the archive write fails
    /// (`trade_db.rs:517-523`). This method keeps that posture: the archive must
    /// land **before** the delete, and a failure is [`StoreError::Refused`] with
    /// the carrier's own text — never a partial wipe.
    ///
    /// Zero rows in scope is a valid receipt with `removed == 0`, not
    /// [`StoreError::NotFound`]: resetting an account that never traded is a
    /// success.
    fn retract_account_trades(
        &self,
        tx: &mut StoreTx<'_>,
        scope: AccountScope<'_>,
        now_ms: i64,
    ) -> Result<TradeRetraction, StoreError>;
}

/// What an account-history erase actually did.
///
/// **Abolishes:** `TradeDb::retract_account`'s bare `usize` return
/// (`core/blitzkrieg_core/src/trade_db.rs:495`), which reports the count but not
/// **where the erased rows went** — so `trading_reset.rs:305` drops the archive
/// path on the floor and the caller cannot tell an archived erase from an
/// unarchived one.
/// **Replaced by:** this receipt, which pairs the count with the archive
/// location. A reset the operator cannot audit is not a reset (issue #1253).
/// **Issue:** P0-6 follow-on / #561 / #1253.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradeRetraction {
    /// How many rows were removed. `0` is a legitimate success.
    pub removed: u64,
    /// Where the removed rows were preserved before deletion — a carrier-named
    /// location (a table, or an export path), never empty.
    ///
    /// Required, not `Option`: [`TradeWrite::retract_account_trades`] refuses
    /// rather than return a receipt with nowhere to find the erased rows.
    pub archived_to: String,
    /// When the erase committed, epoch milliseconds.
    pub at_ms: i64,
}
