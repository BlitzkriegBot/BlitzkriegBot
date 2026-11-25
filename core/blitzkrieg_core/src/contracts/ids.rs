//! Identity newtypes the contracts need that the tree does **not** have yet.
//!
//! Every type in this module is marked `TODO(T0): 待对齐` on purpose. The
//! contracts in the sibling modules name these types, but the repository at
//! `gitea/main = 5b88cecf` spells the same concepts as bare `String` fields:
//!
//! | Contract type | Current spelling in the tree | Anchor |
//! |---|---|---|
//! | [`VenueId`] | `Account::venue: String` (and every `venue: &str` hop) | `core/blitzkrieg_core/src/account/mod.rs:252` |
//! | [`PositionId`] | `OpenPosition::id: String` / `ClosedPosition::id: String` | `core/blitzkrieg_core/src/position/mod.rs:666` |
//! | [`VenueOrderId`] | `VenueTrade::venue_order_id: String`, `TrackedOrder::venue_order_id: Option<String>` | `core/blitzkrieg_core/src/reconcile/types.rs:9` |
//!
//! They are defined here as `String` newtypes rather than aliases so that a
//! consuming task (T2 / T7 / T10) cannot silently keep passing a token id where
//! a venue order id belongs — the mistake class P0-1 is built on.
//!
//! **Do not** wire these into existing signatures as part of T0. Aligning them
//! with the live types (`Account::venue`, `OpenPosition::id`, …) is part of the
//! consuming task's diff, which is where the reviewer sees the behaviour change.
//!
//! NOTE: `AccountId`, `TokenId`, `Side` already exist for real and are
//! **re-used** from their canonical modules instead of being redefined here —
//! see each module's `use` lines.

use serde::{Deserialize, Serialize};

/// Market-plugin name that a venue-level fact belongs to (e.g. `polymarket`).
///
/// **Abolishes:** the bare `venue: String` / `&str` spelling that lets any
/// string be passed as a venue and lets `""` (UNBOUND) travel indistinguishably
/// from a real venue.
/// **Replaced by:** this newtype at every venue boundary the consuming task
/// touches.
/// **Issue:** P1-7 (`HaltScope::Venue`), P0-1 (`VenueTrade.venue`).
///
/// TODO(T0): 待对齐 — `Account::venue` is `String` today
/// (`core/blitzkrieg_core/src/account/mod.rs:252`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VenueId(pub String);

/// Identity of one open/closed position, as carried by the book.
///
/// **Abolishes:** `PositionId` living only as a `String` field, which is what
/// lets P0-1 return "the first position matching this token" and call it
/// attributable.
/// **Replaced by:** this newtype in `FoldVerdict::Folded`.
/// **Issue:** P0-1.
///
/// TODO(T0): 待对齐 — `OpenPosition::id` is `String`
/// (`core/blitzkrieg_core/src/position/mod.rs:666`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PositionId(pub String);

/// The venue's own order id (what the exchange returns on ack, and what
/// cancels and fills quote back).
///
/// **Abolishes:** `venue_order_id: String` / `Option<String>` hopping between
/// the OME, reconcile and the schema views without a type that says "this is
/// the venue's handle, not our client id".
/// **Replaced by:** this newtype in `OrderState::Resting` and
/// `TerminalEvidence`.
/// **Issue:** P1-4.
///
/// TODO(T0): 待对齐 — `VenueTrade::venue_order_id` is `String`
/// (`core/blitzkrieg_core/src/reconcile/types.rs:9`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VenueOrderId(pub String);
