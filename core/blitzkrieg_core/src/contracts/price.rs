//! # P0-2 · "unobserved" is a state, not a sentinel value
//!
//! **Abolishes** the two-scalar conflation the audit documents: `current_price`
//! is initialised to `entry_price` (`core/blitzkrieg_core/src/position/open.rs`,
//! `current_price: p.entry_price`) and the valuation path only overwrites it when
//! `val > 0`, so on a dead feed it stays at the entry price **forever**.
//! `fallback_binary_price` (`core/blitzkrieg_core/src/settlement/constants.rs`
//! `pub fn fallback_binary_price`) then turns any value above `0.5` into a `1.0`
//! settlement verdict. A position opened at `0.96` therefore settles at `1.0`
//! when the feed dies, booking a near-zero result instead of the full loss —
//! R7.1 L1, 用入场价冒充结算价.
//!
//! The same function also reuses `0` as "no value", so "we observed a price of
//! zero" and "we never observed anything" are the same byte.
//!
//! **Replaces them with** [`PriceState`], whose `Unobserved` variant makes "we
//! have not seen a price" structurally distinct from every observed price, and
//! [`EstimateBasis`], which **has no `EntryPrice` variant** so an entry price
//! cannot be named as an estimate basis at all.
//!
//! **Issue:** P0-2. **Consuming task:** the wiring is T2's neighbourhood
//! (`position/` + `settlement/` + `service/settlement_orders/fallback.rs`), and
//! the audit flags the `cash` policy removal as **R6** — it changes a real-money
//! path and needs operator approval before T0's contract is implemented against.
//!
//! ## What the freeze does *not* do
//!
//! It does not remove `UnverifiedExpiryPolicy::Cash`, `PRICE_SOURCE_ENTRY_FALLBACK`
//! or `EXPIRY_FALLBACK_AFTER_MS`. Those deletions are the consuming task's diff,
//! where the reviewer can see the money path change.

use rust_decimal::Decimal;

// Re-used, not redefined: the tree already has exactly this enum and its
// doc-comment rule ("never invent `Live` for a snapshot") is the one this
// contract depends on.
pub use crate::market::family_feed::PriceSource;

/// The kernel's price for one token, with the observation stamped on it.
///
/// **Abolishes:** `current_price: Decimal` initialised to `entry_price`, where
/// "no observation" had no representation at all.
/// **Replaced by:** this enum as the kernel's price cell. `Unobserved` must
/// propagate to the receivable / unverified-exposure report rather than being
/// converted into a number.
/// **Issue:** P0-2 / R7.1 L1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceState {
    /// We have never observed a price for this token in this window. **Not** a
    /// price of zero, and specifically **not** the entry price.
    ///
    /// Every consumer must handle this by reporting "unverified" — never by
    /// substituting a previous quote, a mean, a mid, or the entry price.
    Unobserved,
    /// We observed this price at this time.
    Observed {
        /// The observed price.
        price: Decimal,
        /// How it was obtained. Re-uses the existing
        /// [`crate::market::family_feed::PriceSource`]
        /// (`core/blitzkrieg_core/src/market/family_feed.rs:26`), whose doc
        /// already says 「Never invent `Live` for a snapshot」.
        source: PriceSource,
        /// When the observation was made, epoch milliseconds. Required: a price
        /// without a time is not an observation.
        at_ms: i64,
    },
}

/// What an estimate was derived from.
///
/// **Abolishes:** `PRICE_SOURCE_ENTRY_FALLBACK` and the practice of using the
/// entry price as an estimate input.
/// **Replaced by:** this enum, which **has no `EntryPrice` variant**. The
/// exclusion is the point: a later contributor cannot select an entry price as a
/// basis without editing a frozen contract.
/// **Issue:** P0-2 / R7.1 L1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EstimateBasis {
    /// Derived from an observed bid. A real market observation.
    LastBid,
    /// Derived from an observed two-sided book mid. A real market observation.
    BookMid,
    /// No observation was available, so there is no estimate. This variant
    /// exists so "we could not estimate" is a return value the caller must
    /// handle, rather than a code path that quietly produces `1.0` or `0`.
    NoObservation,
}
