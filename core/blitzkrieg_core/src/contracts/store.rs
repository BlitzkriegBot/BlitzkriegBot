//! # P0-6 · the store seam, split into a read face and a write face
//!
//! **Abolishes** three things the audit measured at `gitea/main = 5b88cecf`:
//!
//! 1. **The "fake gate".** `crate::store::StoreApi`
//!    (`core/blitzkrieg_core/src/store/mod.rs:40`) is one trait of 24 methods,
//!    21 of which carry a default body that returns
//!    `Err("… no backend implements this yet")` (grep count in that file: 21).
//!    A new backend therefore compiles green while silently missing most of the
//!    seam, and the gap only appears at runtime.
//! 2. **The read/write conflation.** The same trait has 2 reads and 21 writes,
//!    so "what can I read" is not a question the type system can answer. The
//!    split below makes the read face implementable on its own.
//! 3. **`Result<bool>` as a money-write result.** `save_positions` /
//!    `save_risk_bounds` / `save_exit` collapse "no carrier" and "write failed"
//!    into `false`, which call sites turn into `warn!` and continue — the
//!    structural shape of R7.1 L4 (silent degradation).
//!
//! **Replaces it with** [`StoreRead`] + [`StoreWrite`], both **without default
//! bodies**, and a [`WriteOutcome`] that distinguishes *persisted*,
//! *refused-for-a-named-reason* and *unavailable-with-the-backend's-own-text*.
//! [`StoreApi`] is the union the assembly hands out.
//!
//! **Issue:** P0-6. **Consuming task:** T4 — and per the audit's §Step 2
//! execution discipline, "delete the 21 default bodies" and "migrate the 8 raw
//! SQL writers" are **two separate PRs**, not one.
//!
//! ## Why no method has a default body
//!
//! A default body is how "unimplemented" became a legal compile-time state.
//! Any implementor must now state what every method does; a store that genuinely
//! cannot persist reports [`WriteOutcome::Unavailable`] and says why, rather
//! than inheriting a silent `Err`.
//!
//! ## Anti-goals (must not happen in T4)
//!
//! - The read face must not read from the in-memory book. Reads come from the
//!   carrier; the projection is computed there.
//! - No `rusqlite` and no `fs::write` outside `store/` once T4 lands. That is a
//!   CI grep assertion in the audit's falsification test, not a style note.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::account::Account;
use crate::contracts::ids::VenueId;
use crate::exit_policy::ExitConfig;
use crate::model::AccountId;
use crate::risk::limits::AccountRiskLimits;

// The scope newtype already exists and is correct for this contract; re-using
// it keeps one spelling of "a write is scoped to exactly one account".
pub use crate::store::AccountScope;

/// Why a store operation failed.
///
/// **Abolishes:** `CoreError` as the store's error type. `CoreError` carries an
/// IPC code and a message shaped for the wire, which is why a backend failure
/// and a caller mistake are indistinguishable at the seam.
/// **Replaced by:** this type, which the IPC layer maps to a wire error at the
/// boundary instead of the store guessing.
/// **Issue:** P0-6.
///
/// Deliberately not `#[non_exhaustive]` in the frozen form: the consuming task
/// must decide whether it grows, and a freeze that pre-authorises growth is not
/// a freeze.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The carrier could not be reached or opened (path missing, lock held,
    /// DB file unreadable). The string is the carrier's own text, never a
    /// rewritten summary (DEV §7: 错误不得吞掉).
    Backend(String),
    /// The row the caller asked for does not exist. Distinct from
    /// `Backend` because "absent" is a fact, not a failure — see
    /// [`StoreRead::load_account`]'s `Option` and this variant's role for
    /// scoped reads that must exist.
    NotFound,
    /// The write was structurally refused before reaching the carrier
    /// (read-only posture, carrier not configured for writes).
    Refused(String),
    /// The value failed validation at the seam (e.g. an account id that is
    /// empty, a setting key outside the accepted grammar).
    Invalid(String),
}

/// One account's identity row as the store returns it.
///
/// **Abolishes:** `load_*` call sites receiving whatever row shape the carrier
/// happened to project, so the read face could not be implemented independently
/// of the SQLite schema.
/// **Replaced by:** this carrier-neutral row. It is `crate::account::Account`
/// unchanged — a union of the existing type, not a second account model — so
/// T4 must not change one byte of what a read returns.
/// **Issue:** P0-6.
pub type AccountRecord = Account;

/// A setting key as the store addresses it.
///
/// **Abolishes:** `put_setting(&self, scope, key: &str, value: Option<&str>)`
/// (`core/blitzkrieg_core/src/store/mod.rs:352`) — an untyped pair where `None`
/// means "clear", so a caller that meant "set to empty" writes a delete.
/// **Replaced by:** this key type plus [`SettingValue`], which spells clear as
/// its own variant.
/// **Issue:** P0-6.
///
/// Kept as a `String` newtype: the key grammar is the carrier's (`account_settings`
/// rows keyed by dotted names), and inventing an enum here would freeze the
/// current key list into the contract.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SettingKey(pub String);

/// What a setting write asks the carrier to do.
///
/// **Abolishes:** `Option<&str>` as the "set or clear" signal at
/// `core/blitzkrieg_core/src/store/mod.rs:352`.
/// **Replaced by:** an explicit variant per intent, so "store the empty string"
/// and "remove the row" cannot be confused.
/// **Issue:** P0-6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingValue<'a> {
    /// Upsert the key to this value. An empty string is a real value.
    Set(&'a str),
    /// Remove the key. Not "set to empty".
    Clear,
}

/// What one durable write actually did.
///
/// **Abolishes:** two shapes at once —
/// - `crate::store::WriteOutcome { Persisted, MemoryOnly { reason } }`
///   (`core/blitzkrieg_core/src/store/mod.rs:380`), which cannot say "the
///   backend was unavailable" or "when did this land"; and
/// - `Result<bool>` on `save_positions` / `save_risk_bounds` / `save_exit`,
///   which loses the reason entirely.
///
/// **Replaced by:** this three-way outcome. `Persisted` carries the timestamp
/// because an audit line needs to say *when*, and a boolean cannot.
/// **Issue:** P0-6 / R7.1 L4.
///
/// Deliberately **not** a `bool`, and deliberately **not** `Ok(())`: `true`
/// reads as "fine" at every call site, which is precisely how the four money
/// write faces lost the distinction in the first place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The fact is on the carrier and survives a restart. `at_ms` is the
    /// carrier's own commit timestamp (event time, not "now" sampled by the
    /// caller — DEV §7: 时间毫秒语义要明确，事件自带时间戳优先).
    Persisted { at_ms: i64 },
    /// The fact is authoritative for this run only. Every caller that gets this
    /// MUST surface it: a money fact that exists only in memory is an
    /// operator-visible fact, not a rounding error (R7.1 L4 — 未核实状态必须是
    /// 一等状态).
    Refused { reason: MemoryOnlyReason },
    /// The carrier was asked and could not answer. The string is the backend's
    /// own error text, passed through unaltered — never a generic replacement
    /// (DEV §7 / R12).
    Unavailable { reason: String },
}

/// Why a write did not become durable, when the kernel can name the reason.
///
/// **Abolishes:** `crate::store::MemoryOnlyReason`
/// (`core/blitzkrieg_core/src/store/mod.rs:396`) being a standalone enum with no
/// outcome that carries it into `Persisted`-vs-not decisions.
/// **Replaced by:** this enum as the payload of [`WriteOutcome::Refused`]. Same
/// three reasons, same meanings, moved into the outcome so a caller matches once.
/// **Issue:** P0-6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryOnlyReason {
    /// This core was assembled without a store (backtester, in-process tests).
    NoStoreConfigured,
    /// The process runs read-only; durable writes are structurally refused
    /// rather than attempted and lost.
    ReadOnlyMode,
    /// The backend was asked and declined. The string is the backend's own
    /// error text, passed through unaltered.
    BackendRejected(String),
}

/// A cluster's strategy enablement set, as the write face accepts it.
///
/// **Abolishes:** `strategy_state.json` as the third carrier of the enabled set
/// (`crate::strategy_state::save`, `core/blitzkrieg_core/src/strategy_state.rs:72`,
/// with four call sites the audit lists) and the raw `account_settings.strategies`
/// SQL writes that bypass the seam.
/// **Replaced by:** this type as the argument to
/// [`StoreWrite::save_strategy_set`]; the carrier decides where it lands, and
/// there is exactly one of it.
/// **Issue:** P0-6 (also P1-8 in the audit's numbering).
///
/// Empty is a real value: it means "this account runs no strategies", which
/// `Account::strategies` already documents as the explicit spelling. `T4` must
/// not turn an empty set into "absent row".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategySet {
    /// Enabled (account, strategy) names, sorted and deduplicated by the
    /// writer so the persisted form diffs stably.
    pub enabled: Vec<String>,
}

/// A ledger position as the store can report it, without the kernel's ledger
/// object.
///
/// **Abolishes:** reads that reach into `Ledger`'s in-memory reservation map
/// (`core/blitzkrieg_core/src/ledger.rs:278/287/295`) to answer a question that
/// is supposed to survive a restart.
/// **Replaced by:** this projection — the same three amounts the ledger already
/// exposes, plus the venue they belong to, so a read does not need the live
/// `Ledger` at all.
/// **Issue:** P0-6.
///
/// `Decimal`, never `f64`: money (DEV §7). `unverified_exposure` is a separate
/// field with no default, because R7.1 requires unverified amounts to be
/// reported **单列** and never folded into a spendable total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerProjection {
    /// Venue this projection was read for.
    pub venue: VenueId,
    /// Total cash the ledger holds for this scope.
    pub balance: Decimal,
    /// Cash held against open orders / reservations.
    pub reserved: Decimal,
    /// `balance - reserved`, computed by the carrier. Never recomputed by a
    /// caller: two subtractions are two chances to disagree.
    pub available: Decimal,
    /// Exposure the kernel cannot verify against real money flows, kept
    /// separate on purpose (R7.1「未核实敞口」单列). `None` = not reported.
    pub unverified_exposure: Option<Decimal>,
    /// When the projection was read, in epoch milliseconds.
    pub as_of_ms: i64,
}

/// The read face of the store.
///
/// **Abolishes:** the two `load_*` methods buried in the 24-method
/// `crate::store::StoreApi`, and every read that instead reached into an
/// in-memory book because the trait had no read for it.
/// **Replaced by:** this trait, implementable without touching [`StoreWrite`].
/// **Issue:** P0-6.
///
/// Every method here is **required** (no default body): a store that cannot
/// answer a read must say so, not inherit `Err`.
pub trait StoreRead: Send + Sync {
    /// The account's identity row, or `None` when the account is not configured.
    ///
    /// `None` is a real answer ("no such account"), not an error; a carrier
    /// failure is [`StoreError::Backend`].
    fn load_account(&self, id: &AccountId) -> Result<Option<AccountRecord>, StoreError>;

    /// Every setting row for the scope, verbatim.
    ///
    /// `Option<String>` per key preserves the distinction the old
    /// `Option<&str>` write side destroyed: `None` is an absent row, `Some("")`
    /// is an empty value that was written.
    fn load_settings(
        &self,
        scope: AccountScope<'_>,
    ) -> Result<BTreeMap<SettingKey, Option<String>>, StoreError>;

    /// The account's risk limits, or `None` when the account has none stored.
    ///
    /// `None` must NOT be replaced with defaults by any consumer: the default
    /// limits are the #1223 bug class (values ~10^5× the intended caps).
    fn load_risk(&self, scope: AccountScope<'_>) -> Result<Option<AccountRiskLimits>, StoreError>;

    /// The account's exit configuration, or `None` when none is stored.
    fn load_exit(&self, scope: AccountScope<'_>) -> Result<Option<ExitConfig>, StoreError>;

    /// The ledger position for the scope, computed by the carrier.
    ///
    /// Returns the projection rather than `Option`: a missing ledger row is a
    /// zero-balance projection the carrier reports, so callers never invent one.
    fn ledger_projection(&self, scope: AccountScope<'_>) -> Result<LedgerProjection, StoreError>;
}

/// The write face of the store.
///
/// **Abolishes:** the 21 default-`Err` write methods of
/// `crate::store::StoreApi` and the `Result<bool>` returns that made "no
/// carrier" and "failure" the same value.
/// **Replaced by:** this trait — small, required, and returning an outcome that
/// distinguishes persisted / refused / unavailable.
/// **Issue:** P0-6.
///
/// `tx` is the carrier's transaction handle. It is a parameter rather than a
/// method returning one so a composite write (limits + strategy set) commits
/// atomically; the audit's raw-SQL writers are the case that motivated it.
pub trait StoreWrite: Send + Sync {
    /// Upsert or clear one setting for the scope.
    fn put_setting(
        &self,
        tx: &mut StoreTx<'_>,
        scope: AccountScope<'_>,
        key: SettingKey,
        value: SettingValue<'_>,
    ) -> Result<WriteOutcome, StoreError>;

    /// Persist the account's risk limits.
    ///
    /// The limits themselves are unchanged by this contract: `AccountRiskLimits`
    /// is `crate::risk::limits::AccountRiskLimits`
    /// (`core/blitzkrieg_core/src/risk/limits.rs:131`). Nothing here changes a
    /// threshold value — R6 still governs every such change separately.
    fn save_risk(
        &self,
        tx: &mut StoreTx<'_>,
        scope: AccountScope<'_>,
        limits: &AccountRiskLimits,
    ) -> Result<WriteOutcome, StoreError>;

    /// Persist the account's strategy enablement set.
    fn save_strategy_set(
        &self,
        tx: &mut StoreTx<'_>,
        scope: AccountScope<'_>,
        set: StrategySet,
    ) -> Result<WriteOutcome, StoreError>;
}

/// The carrier transaction handle.
///
/// **Abolishes:** writes that each opened their own connection/transaction,
/// which is why a two-part write (limits + strategies) could half-land.
/// **Replaced by:** one handle threaded through every [`StoreWrite`] call in a
/// unit of work.
/// **Issue:** P0-6.
///
/// Opaque on purpose — a definition-only module cannot describe the carrier's
/// internals, and T4 owns that. The lifetime ties the handle to the borrow that
/// produced it so a transaction cannot outlive its carrier borrow.
///
/// The field is private and there is deliberately **no constructor here**: T0
/// cannot know how a carrier opens a transaction, and inventing a `new()` would
/// be inventing an implementation. T4 adds the constructor together with the
/// first real implementor, at which point this struct stops being a placeholder.
#[derive(Debug)]
pub struct StoreTx<'a> {
    /// Marker for the carrier borrow. Private: T4 replaces this with the real
    /// handle when it lands; keeping it private means no caller can depend on
    /// a shape that is about to change.
    _carrier: std::marker::PhantomData<&'a mut ()>,
}

/// The full store seam: everything readable plus everything writable.
///
/// **Abolishes:** `crate::store::StoreApi: Debug + Send + Sync`
/// (`core/blitzkrieg_core/src/store/mod.rs:40`) as the single 24-method trait.
/// **Replaced by:** this trait, defined as the union of the two faces, so the
/// seam has one name while the read and write halves are separately
/// implementable and separately reviewable.
/// **Issue:** P0-6.
///
/// Deliberately no `Debug` supertrait: requiring it is what forced the old trait
/// to oblige every implementor to be printable, and no consumer in the contract
/// needs to print the store.
///
/// Carries **no blanket impl** on purpose. T0 may not pre-authorise "any type
/// that happens to implement both faces is a store": T4 decides which types are
/// stores, and a blanket impl would silently make every test double one.
pub trait StoreApi: StoreRead + StoreWrite {}
