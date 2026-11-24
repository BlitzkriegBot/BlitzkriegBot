//! # P0-1 · an external fill must say which account it belongs to
//!
//! **Abolishes** the token-only attribution the audit measured: venue fills carry
//! no account, so unknown SELLs are matched with
//! `.find(|p| p.token_id == t.token_id)` over **all** positions — never comparing
//! `account_id` (`core/blitzkrieg_core/src/service/accounts/reconcile.rs:592-598`)
//! — and unknown BUYs are hardcoded to `default_account_id()`
//! (`same file:704`, and `core/blitzkrieg_core/src/market/host.rs:224`).
//!
//! The consequence is not a reconciliation difference, it is **one account's
//! money landing on another's book**: with A and B each holding 2000 shares of
//! the same token, a manual sell on B's venue closes A's position, books +$1,800
//! realized to A, and leaves B's cash missing with its position still open.
//!
//! **Replaces it with** [`VenueTrade`], whose `account` is a required field, and
//! [`FoldVerdict`], which makes "I cannot attribute this fill" a first-class
//! value ([`FoldVerdict::Parked`]) instead of a guess.
//!
//! **Issue:** P0-1. **Consuming task:** T2 — `reconcile.rs` matching +
//! `host.rs` + `ome.rs` + tests. The audit marks T2 **R6** (it touches closing
//! and bookkeeping), so implementation needs operator approval first.
//!
//! ## Why this module defines its own `VenueTrade`
//!
//! The tree already has one (`core/blitzkrieg_core/src/reconcile/types.rs:8`)
//! with 49 references across the kernel and its tests. T0 **must not** edit it —
//! that would be a call-site change, which the freeze forbids (R5: one commit,
//! one thing). So the frozen contract lives here with its own name, and T2
//! replaces the old struct at its 49 call sites. Until then, both exist:
//! `crate::reconcile::VenueTrade` (running) and
//! `crate::contracts::attribution::VenueTrade` (frozen).
//!
//! ## The one invariant T2 must not soften
//!
//! A fill whose account cannot be determined goes to [`FoldVerdict::Parked`] and
//! is reported as unverified exposure. It does **not** fall back to the default
//! account, and it does not pick "the first position with this token". The
//! `Parked` arm is the whole point of the type.

use rust_decimal::Decimal;

use crate::contracts::ids::{PositionId, VenueId};
use crate::model::{AccountId, Side, TokenId};

/// An external fill as the venue reported it, now carrying the account.
///
/// **Abolishes:** `crate::reconcile::VenueTrade`
/// (`core/blitzkrieg_core/src/reconcile/types.rs:8`), which has no account field
/// and therefore cannot be attributed.
/// **Replaced by:** this struct. `account` is required, so it cannot be omitted
/// at a construction site.
/// **Issue:** P0-1.
///
/// Every other field is the **existing** struct's field, verbatim — same name,
/// same type, same meaning — so T2's diff is "add the account, delete the
/// guessing", not "re-model the venue feed" (R5). `tx_hash` is kept because the
/// Polymarket feed reports it and dropping it would lose a reconcilable fact.
///
/// `Decimal`, never `f64`: this is a fill (DEV §7). `size` is the per-trade
/// executed size, **not** cumulative — the existing struct documents that and
/// the distinction is load-bearing for gap fills.
#[derive(Debug, Clone, PartialEq)]
pub struct VenueTrade {
    /// **The one new field.** The account the fill belongs to.
    ///
    /// A fill that cannot name its account is not a `VenueTrade` — it is an
    /// [`UnattributedFill`] and must be parked. The old code spent this field's
    /// absence guessing, which is how the cross-account bug happened.
    pub account: AccountId,
    /// The venue's order id this trade belongs to.
    pub venue_order_id: String,
    /// The venue's own trade id.
    pub trade_id: String,
    /// The token that traded.
    pub token_id: TokenId,
    /// The side of the **taker** fill.
    pub side: Side,
    /// Executed size of this trade (NOT cumulative).
    pub size: Decimal,
    /// Execution price.
    pub price: Decimal,
    /// When the trade executed, epoch milliseconds — the venue's event time,
    /// not the time we received it.
    pub ts_ms: i64,
    /// On-chain transaction hash, when the venue has one. A reconcilable fact:
    /// it ties the trade to a settlement the operator can look up independently.
    pub tx_hash: Option<String>,
    /// The venue's maker/taker report for this trade, when it has one. A
    /// synthesized gap fill must charge the fee the venue actually charged, so
    /// the role travels with the trade instead of being re-guessed.
    pub maker: Option<bool>,
}

/// A fill the kernel could not attribute to an account.
///
/// **Abolishes:** the `default_account_id()` fallback and the token-only position
/// match that invent an owner for a fill that has none.
/// **Replaced by:** this explicit value, which carries the facts so an operator
/// (or the reconciliation sweep) can attribute it later against real money
/// flows.
/// **Issue:** P0-1 / R7.1 (未核实敞口 must be a first-class state).
///
/// This is a **reporting** type, not a second trade record: it must never be
/// folded into the book (L2 — no synthesized fills). Its job is to sit in
/// unverified exposure until a human or a reconcile proves the owner.
#[derive(Debug, Clone, PartialEq)]
pub struct UnattributedFill {
    /// What the venue reported. Everything needed to attribute it later is
    /// here; nothing here may be edited to make it fit an account.
    pub trade: VenueTradeFacts,
    /// Why attribution failed, in the kernel's words — surfaced verbatim.
    pub reason: String,
}

/// The parts of a venue fill that are known even when the account is not.
///
/// **Abolishes:** the temptation to keep a half-populated [`VenueTrade`] with a
/// made-up account. A type that cannot express an account makes that
/// unrepresentable.
/// **Replaced by:** this fact bundle inside [`UnattributedFill`].
/// **Issue:** P0-1.
///
/// Every field here comes from the venue, never from the kernel's own state:
/// that is the difference between "unknown owner" and "invented owner".
#[derive(Debug, Clone, PartialEq)]
pub struct VenueTradeFacts {
    /// The market plugin whose venue reported this fill.
    pub venue: VenueId,
    /// The token that traded.
    pub token_id: TokenId,
    /// The side of the taker fill.
    pub side: Side,
    /// Executed size of this trade (NOT cumulative).
    pub size: Decimal,
    /// Execution price.
    pub price: Decimal,
    /// The venue's own trade id.
    pub trade_id: String,
    /// The venue order id, when the venue reported one.
    pub venue_order_id: Option<String>,
    /// When the trade executed, epoch milliseconds (venue event time).
    pub ts_ms: i64,
}

/// What reconciling one external fill decided.
///
/// **Abolishes:** `apply_external_fills` returning "best effort" with no way to
/// see that anything was guessed.
/// **Replaced by:** this verdict per fill. The audit's replacement signature is
/// `Vec<FoldVerdict>`, and every `Parked` item must reach the
/// `positions.unverified_exposure` report and a `RiskAlert`.
/// **Issue:** P0-1.
#[derive(Debug, Clone, PartialEq)]
pub enum FoldVerdict {
    /// The fill was folded onto a known position of a known account.
    Folded {
        /// Which position absorbed it.
        position: PositionId,
        /// Whose position it was. Restated here so a caller cannot report a fold
        /// without naming the account it landed on.
        account: AccountId,
    },
    /// The fill could not be attributed. **A first-class outcome, not an
    /// error**: the fill is real, the owner is unknown, and the kernel says so
    /// instead of choosing.
    ///
    /// Never convert this into `Folded` to keep the books tidy (R7.1 L2/L3).
    Parked(UnattributedFill),
}
