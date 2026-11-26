//! The carrier transaction handle: [`StoreTx`], the concrete type behind
//! [`crate::contracts::store::StoreTx`].
//!
//! # Why it lives here and not in `contracts/`
//!
//! The contract (`contracts/store.rs`) froze `StoreTx` as an **opaque** handle
//! with no fields and no constructor: T0 could not know how a carrier opens a
//! transaction, so inventing an implementation there would have been inventing
//! policy. It also stated an anti-goal in its own module doc — *no `rusqlite`
//! outside the store implementation once T4 lands*, and `contracts/store.rs` is
//! a definition-only module, not a store implementation.
//!
//! So the fields and the constructor live here, where the SQLite carrier lives
//! ([`crate::db::trades`] is the first real implementor), and `contracts/store.rs`
//! re-exports this type. Call sites still name `contracts::store::StoreTx`; the
//! type is the same one, so no seam signature changed and no call site moved.
//!
//! # What stays hidden
//!
//! `inner` is private and both accessors are `pub(crate)`: a `TradeWrite`
//! implementor in this crate can bind parameters through the domain methods,
//! but no code path can hand the raw connection to arbitrary callers — the
//! "no raw SQL outside the store" rule stays enforceable.

/// The carrier transaction handle.
///
/// **Abolishes:** writes that each opened their own connection/transaction,
/// which is why a two-part write (limits + strategies) could half-land.
/// **Replaced by:** one handle threaded through every
/// [`crate::contracts::store::StoreWrite`] call in a unit of work.
/// **Issue:** P0-6.
///
/// The lifetime ties the handle to the borrow of the transaction that opened
/// it, so a handle cannot outlive its unit of work.
pub struct StoreTx<'a> {
    /// The live SQLite transaction. Private: the only ways in are the domain
    /// methods that this crate owns, so a `TradeWrite` implementor can bind
    /// parameters but can never hand the connection to arbitrary code.
    inner: &'a rusqlite::Transaction<'a>,
}

impl<'a> StoreTx<'a> {
    /// Wrap an open transaction. `pub(crate)`: the carrier that began it owns
    /// its lifetime and its commit/rollback decision.
    pub(crate) fn new(inner: &'a rusqlite::Transaction<'a>) -> Self {
        Self { inner }
    }

    /// The connection inside the transaction, for this crate's own domain
    /// statements. Deliberately not `pub`: the implementors of
    /// [`crate::contracts::store::StoreWrite`] live in this crate, and a public
    /// accessor would make the "no raw SQL outside the store" rule unenforceable.
    pub(crate) fn connection(&self) -> &rusqlite::Connection {
        self.inner
    }
}

impl std::fmt::Debug for StoreTx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The transaction itself is not printable in a useful way; naming the
        // type is what a log line needs, and it must not leak a carrier path.
        f.write_str("StoreTx(<open transaction>)")
    }
}
