//! # P1-4 / P1-6 · order state carries its evidence; history is paged, not dumped
//!
//! ## P1-4 — the evidence layer was flattened
//!
//! **Abolishes** two conflations the audit measured:
//!
//! 1. `Core::cancel` queues the venue cancel and then **immediately** releases
//!    the reservation and marks the order `Cancelled`. Queued is not cancelled:
//!    a fill inside the next 250 ms arrives against released money, the budget
//!    has already been spent on a new order, and the same token can be entered
//!    twice with a position opened after the fact.
//! 2. `resolve_place_timeouts` locally cancels orders with no venue id, on the
//!    assumption that "venue never acked = venue has no order". A POST timeout
//!    proves nothing, and ghost detection skips those orders too, so a fill in
//!    that window is a ghost.
//!
//! Also: `OrderStatus` (`core/bk_contract/src/types.rs:117`) has a single
//! `Cancelled` for both "we withdrew successfully" and "we gave up waiting".
//!
//! **Replaces them with** [`OrderState`] (explicit `Placing` / `Resting` /
//! `Probing` / `Terminal`) and [`TerminalEvidence`], which makes the *proof*
//! that terminalised an order a required part of being terminal. The audit's
//! stated constraint rides along: **reservation release may only happen after
//! `Terminal { evidence }` is durable**.
//!
//! ## P1-6 — `limit = 0` meant "everything"
//!
//! **Abolishes** the unbounded reads: `trade_db` documents `limit == 0` as "no
//! cap" (`core/blitzkrieg_core/src/trade_db.rs:579-585`), the TUI, the desktop
//! embed and the gateway dispatcher all pass `0`, `orders.list` has no paging at
//! all, and the `trades.history` handler's `unwrap_or(50)` has no clamp
//! (`core/blitzkrieg_core/src/ipc/server/handlers/trades.rs:20`) while the gateway
//! comments that "the kernel clamps". Both sides thought the other would stop it.
//!
//! **Replaces them with** [`PageRequest`], whose limit is `NonZeroU32` so
//! "everything" is unrepresentable, and [`Page<T>`], which carries the cursor
//! and the total so the caller can decide without a second full read.
//!
//! **Issue:** P1-4 / P1-6. **Consuming tasks:** T7 (`ome.rs`, `exits.rs`,
//! `reconcile.rs`) and T9 (`trades` / `order` / `position` handlers + three
//! frontend entry points). The audit marks T7 **R6** — order lifecycle — so its
//! implementation needs approval.

use std::num::NonZeroU32;

use crate::contracts::ids::VenueOrderId;

/// The venue's acknowledgement of a place, as evidence.
///
/// **Abolishes:** "the place was accepted" travelling as a bare `Option<String>`
/// venue id with no time, so a caller could not tell an ack from a probe result.
/// **Replaced by:** this struct inside [`OrderState::Placing`].
/// **Issue:** P1-4.
///
/// A definition-only placeholder: T7 decides which ack fields the venue adapters
/// actually carry. It is a struct rather than a bare `VenueOrderId` so that
/// adding a field later does not re-break every match arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VenueAck {
    /// The id the venue assigned.
    pub venue_id: VenueOrderId,
    /// When the ack arrived, epoch milliseconds.
    pub acked_at_ms: i64,
}

/// What kind of terminal an order reached.
///
/// **Abolishes:** the single `OrderStatus::Cancelled` meaning both "venue
/// confirmed the cancel" and "the kernel stopped waiting"
/// (`core/bk_contract/src/types.rs:117`).
/// **Replaced by:** this kind, paired with [`TerminalEvidence`] so the *why*
/// travels with the *what*.
/// **Issue:** P1-4.
///
/// The eight existing `OrderStatus` variants are not redefined here — T7 maps
/// them onto this enum, and that mapping is where the reviewer sees the
/// behaviour change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKind {
    /// The order is fully filled.
    Filled,
    /// The venue acknowledged a cancel.
    Cancelled,
    /// The venue rejected the order outright.
    Rejected,
    /// The venue reported the order failed after submission.
    Failed,
    /// The kernel closed it itself (round expiry / reconciled ghost) without an
    /// honest venue receipt.
    ForceClosed,
}

/// The proof that made an order terminal.
///
/// **Abolishes:** terminalising on an assumption — the queued cancel treated as
/// effective, and the POST timeout treated as "no order exists".
/// **Replaced by:** a required field of [`OrderState::Terminal`]. An order
/// cannot be terminal without stating which of these it has.
/// **Issue:** P1-4 / R7.1 L1 (no verdict without evidence).
///
/// This is the type that makes 「入队 ≠ 已撤」 expressible: `VenueCancelAck` means
/// the venue confirmed, and nothing else does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvidence {
    /// The venue acknowledged the cancel for this order id.
    VenueCancelAck(VenueOrderId),
    /// The venue reported a fill for this order id.
    VenueFill(VenueOrderId),
    /// A probe established the venue never accepted the place. A probe result is
    /// required: "no ack" alone is not evidence.
    PlaceNeverAccepted { probe: ProbeResult },
    /// Reconciliation proved the order does not exist at the venue.
    ReconciledGhost,
}

/// The outcome of probing the venue for an order it never acked.
///
/// **Abolishes:** the local cancel on a place timeout, which assumed a missing
/// ack meant a missing order.
/// **Replaced by:** this result as the payload of
/// [`TerminalEvidence::PlaceNeverAccepted`], so terminating requires an actual
/// venue answer.
/// **Issue:** P1-4.
///
/// Definition-only placeholder: T7 owns the real probe surface (client-id lookup
/// against the venue). Three variants state the only three answers a probe can
/// give, and `Unknown` exists so that "the probe itself failed" cannot be read
/// as "the order does not exist".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeResult {
    /// The venue has the order.
    Found,
    /// The venue answered and does not have the order.
    Absent,
    /// The probe could not get an answer. **Not** evidence of absence.
    Unknown,
}

/// Where an order is in its lifecycle, with the evidence for where it is.
///
/// **Abolishes:** one flat status enum that cannot express "we queued a cancel
/// but have no ack" or "we are probing for an order we may have placed".
/// **Replaced by:** this state machine. `Terminal` requires
/// [`TerminalEvidence`].
/// **Issue:** P1-4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderState {
    /// Submitted, no venue ack yet.
    Placing {
        /// Our own id for the place. A `String` because the client id is
        /// allocated before the order exists and is what a probe looks up.
        client_id: String,
        /// When the place was posted, epoch milliseconds.
        posted_at_ms: i64,
        /// The ack, once it arrives. `None` = still waiting — which is exactly
        /// the state the old code collapsed into "cancelled".
        venue_ack: Option<VenueAck>,
    },
    /// Resting at the venue.
    Resting {
        /// The venue's id for the order.
        venue_id: VenueOrderId,
        /// When the venue acked it, epoch milliseconds.
        acked_at_ms: i64,
    },
    /// Place timed out; the kernel is probing the venue by client id instead of
    /// cancelling locally.
    Probing {
        /// The client id being looked up.
        client_id: String,
        /// How many probes have been made. Bounded by T7, not by this contract.
        probes: u8,
    },
    /// Terminal, with the evidence. Reservation release may only follow a
    /// durable write of this state (audit P1-4 constraint).
    Terminal {
        /// What terminal state was reached.
        kind: TerminalKind,
        /// The proof for it.
        evidence: TerminalEvidence,
    },
}

/// One page of history.
///
/// **Abolishes:** `limit == 0` meaning "everything", and unbounded
/// `orders.list` / `recent_trades(0)` reads that put the whole log into a
/// snapshot or an SSE frame.
/// **Replaced by:** this request shape. `NonZeroU32` makes "everything"
/// unrepresentable, so the caller must choose a size.
/// **Issue:** P1-6.
///
/// The audit's wire-level numbers (`default 200`, hard cap `1000`, anything else
/// `INVALID_PARAMS`) are T9's, not the frozen type's: the cap is a policy the
/// handler enforces, and freezing it here would hide it from review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    /// Page size. `None` = the handler's documented default. There is no
    /// spelling for "unbounded".
    pub limit: Option<NonZeroU32>,
    /// Opaque continuation token from a previous [`Page::next_cursor`]. `None`
    /// = start from the newest end.
    ///
    /// Opaque on purpose: a cursor the caller can parse is a cursor the caller
    /// can forge.
    pub cursor: Option<String>,
}

/// One page of results.
///
/// **Abolishes:** responses that returned only rows, leaving the caller unable
/// to tell "that was all" from "that was the cap".
/// **Replaced by:** this envelope, whose `next_cursor` states the difference and
/// whose `total` lets a caller show a count without re-reading.
/// **Issue:** P1-6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    /// The rows for this page, in the producer's documented order.
    pub rows: Vec<T>,
    /// Cursor for the next page, or `None` when this page is the last one.
    /// `None` is the only spelling of "end" — an empty `rows` with a cursor is a
    /// legal intermediate page.
    pub next_cursor: Option<String>,
    /// Total matching rows, when the carrier can report it without a second
    /// full scan. `None` = unknown, and a consumer must render 「未知」 rather
    /// than treat it as zero (R7.1 L4).
    pub total: Option<u64>,
}
