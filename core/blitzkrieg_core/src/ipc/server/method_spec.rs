//! The IPC verb CLASSIFICATION table — the single source the read-only gate
//! derives from (issue #177).
//!
//! Before this module the gate was a hand-registered `READONLY_REFUSED`
//! whitelist (`server/mod.rs`, 27 entries, all configuration verbs). Its own
//! comment said "a new write verb must be added HERE", which is the defect:
//! registration was the ONLY thing standing between a new trading verb and a
//! read-only core that would happily simulate it — and the list had been
//! derived from "configuration verbs", so `orders.place`, `orders.cancel`,
//! `positions.exit`, `risk.kill`, `account.freeze`, `strategy.enable`, … were
//! never on it. A read-only core therefore produced simulated fills and
//! positions (second layer: `risk_admission/admission.rs` shared the
//! `Mode::Dry | Mode::ReadOnly` settle branch).
//!
//! The replacement contract: every verb carries a [`MethodKind`], and the gate
//! refuses every kind except [`MethodKind::Read`] in `Mode::ReadOnly` unless
//! the spec explicitly relaxes it ([`ReadonlyPolicy::Allow`], reserved for
//! [`MethodKind::Ingest`]). Three structural properties follow:
//!
//! 1. **Refusal needs no registration**: the gate reads `kind`, so a new verb
//!    classified `Write`/`Action`/`Ingest`/`Stream` is refused the moment it
//!    is classified — there is no second list to remember.
//! 2. **Write/Action/Stream can never be relaxed**: [`MethodSpec::allowed_in_readonly`]
//!    ignores `readonly` for those kinds, so a mis-registered `Allow` cannot
//!    reopen an execution verb. The only relaxable kind is `Ingest`.
//! 3. **Table ↔ dispatch parity is tested, not assumed**: a verb that the
//!    dispatch match serves but this table omits would fall through the gate
//!    (the match would serve it). `tests_readonly_surface` parses the dispatch
//!    match in `server/mod.rs` AND the constant declarations in
//!    `schema/methods.rs`, and fails when either names a verb with no entry
//!    here — so omission breaks the build's tests instead of penetrating
//!    silently.
//!
//! ## Taxonomy (judged from what the handler actually does)
//!
//! | kind | meaning |
//! |---|---|
//! | `Read` | observation only: no persisted state, ledger, position, order, gate or config moves |
//! | `Write` | the change is persisted (SQLite, `.env`, config files, trade/order logs, audit files) |
//! | `Action` | live kernel state moves without being a config write: orders, positions, ledger, risk gates, engine dispatch, jobs, extension lifecycle |
//! | `Ingest` | market data handed INTO the kernel (books, spot, round markets) |
//! | `Stream` | per-session push subscription lifecycle |
//!
//! ## Why every `Ingest` today is `Refuse`
//!
//! The contract permits relaxing `Ingest`, and #177 asks for the relaxation
//! argument either way. There is none here, because "ingest" is not
//! side-effect-free in this kernel:
//!
//! - `books.snapshot` / `books.top` land in `Core::book_snapshot`
//!   (`service/settlement_orders/settlement.rs`), which runs the maker-fill
//!   simulation over every resting order on that token. `restore_orders()`
//!   (`service/persistence.rs`) restores live orders from the order log in
//!   EVERY mode at boot, so "a read-only core has no resting orders" is not an
//!   invariant. Ingesting a book could therefore create a fill, a position and
//!   a ledger movement.
//! - `engine.book` / `engine.markets` / `spot.price` feed `engine_on_data`,
//!   i.e. the strategy layer's decision path, which places orders through
//!   `Core::place_inner` — it does not pass the IPC gate at all (that is the
//!   whole point of the second layer being fixed separately).
//!
//! Allowing any of these would reopen exactly the hole #177 closes, so all
//! five are `Refuse`. Relaxation stays possible for a future ingest verb whose
//! handler provably touches no book, no order and no ledger; that verb spells
//! `Allow` and carries the argument in its entry below.

use crate::schema::method;

/// What a verb does to the kernel's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    /// Observation only. Always served in read-only mode.
    Read,
    /// Persists a change (SQLite / `.env` / config files / logs / audits).
    Write,
    /// Moves live kernel state: orders, positions, ledger, risk gates,
    /// engine dispatch, jobs, extension lifecycle.
    Action,
    /// Market data handed into the kernel.
    Ingest,
    /// Per-session push subscription lifecycle.
    Stream,
}

/// Whether read-only mode may serve this verb anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadonlyPolicy {
    /// Refused in read-only mode. This is the default for every non-`Read` kind.
    Refuse,
    /// Served in read-only mode. Only meaningful for [`MethodKind::Ingest`];
    /// [`MethodSpec::allowed_in_readonly`] ignores it for every other kind.
    Allow,
}

/// One verb's classification: its name, what it does, and whether read-only
/// mode may relax the default refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodSpec {
    pub name: &'static str,
    pub kind: MethodKind,
    pub readonly: ReadonlyPolicy,
}

impl MethodSpec {
    /// The gate's single predicate.
    ///
    /// Structural by construction: `Write` / `Action` / `Stream` answer `false`
    /// no matter what `readonly` says, so a mis-registered `Allow` cannot
    /// reopen an execution verb. Only `Ingest` (an explicit `Allow`) relaxes.
    pub fn allowed_in_readonly(&self) -> bool {
        match self.kind {
            MethodKind::Read => true,
            MethodKind::Ingest => self.readonly == ReadonlyPolicy::Allow,
            MethodKind::Write | MethodKind::Action | MethodKind::Stream => false,
        }
    }
}

/// Look one verb up. A linear scan is deliberate: the table is a compile-time
/// constant of ~100 short strings, the gate calls this at most once per
/// request, and a scan keeps the table (not a second lookup structure) the one
/// list to edit.
pub fn get(name: &str) -> Option<&'static MethodSpec> {
    METHODS.iter().find(|s| s.name == name)
}

// Shorthands keep 100+ entries readable; each entry still names its kind.
const fn read(name: &'static str) -> MethodSpec {
    MethodSpec {
        name,
        kind: MethodKind::Read,
        readonly: ReadonlyPolicy::Refuse,
    }
}
/// A persistable change (SQLite / `.env` / files / logs).
const fn write(name: &'static str) -> MethodSpec {
    MethodSpec {
        name,
        kind: MethodKind::Write,
        readonly: ReadonlyPolicy::Refuse,
    }
}
/// A live-state change (orders / positions / gates / dispatch / jobs).
const fn action(name: &'static str) -> MethodSpec {
    MethodSpec {
        name,
        kind: MethodKind::Action,
        readonly: ReadonlyPolicy::Refuse,
    }
}
/// Market data handed into the kernel. `Refuse` today — see the module docs.
const fn ingest_refused(name: &'static str) -> MethodSpec {
    MethodSpec {
        name,
        kind: MethodKind::Ingest,
        readonly: ReadonlyPolicy::Refuse,
    }
}
/// Per-session push subscription.
const fn stream(name: &'static str) -> MethodSpec {
    MethodSpec {
        name,
        kind: MethodKind::Stream,
        readonly: ReadonlyPolicy::Refuse,
    }
}

/// Every verb the dispatch match in `server/mod.rs` can serve. MUST stay in
/// step with `schema::method` — `tests_readonly_surface::every_declared_method_has_a_spec`
/// parses that module and fails the build's tests when a verb is missing here,
/// so a new verb is classified before it can ship.
pub const METHODS: &[MethodSpec] = &[
    // ── observation surface: always served, never mutates ──────────────────
    read(method::PING),
    read(method::READY),
    read(method::CORE_FEE_QUOTE),
    read(method::SYSTEM_VERSION),
    read(method::RISK_LIMITS),
    read(method::ROUND_DURATION_GET),
    read(method::MARKET_PLUGIN_GET),
    read(method::MARKET_CREDENTIALS_STATUS),
    read(method::MARKET_LIST),
    read(method::NET_CHECK),
    read(method::ORDER_LIST),
    read(method::LEDGER_BALANCE),
    read(method::POSITIONS_LIST),
    read(method::POSITIONS_UNVERIFIED_EXPOSURE),
    read(method::TRADES_HISTORY),
    read(method::TRADES_SUMMARY),
    read(method::ENGINE_ROUND),
    read(method::ENGINE_BOOKS),
    read(method::ENGINE_STATS),
    read(method::STRATEGY_LIST),
    read(method::STRATEGY_PARAMS_GET),
    read(method::UNIFIED_LIST),
    read(method::UNIFIED_CANDIDATES),
    read(method::EXTENSION_LIST),
    read(method::SE_STATUS),
    read(method::SE_HISTORY),
    read(method::SE_PROPOSALS),
    read(method::KLINE_HISTORY),
    read(method::INTENT_AUDIT_TAIL),
    read(method::ACCOUNT_LIST),
    read(method::ACCOUNT_VENUE_GET),
    read(method::ACCOUNT_JOURNAL),
    read(method::EXECUTION_POLICY_LIST),
    read(method::EXECUTION_POLICY_GET),
    read(method::EXECUTION_POLICY_HISTORY),
    read(method::EXECUTION_POLICY_PREVIEW),
    read(method::BACKTEST_ONCHAIN_LIST),
    read(method::BACKTEST_STATUS),
    read(method::BACKTEST_RESULT),
    read(method::BACKTEST_EXPORT),
    read(method::BLUEPRINT_COMPILE),
    read(method::BLUEPRINT_LOAD),
    read(method::BLUEPRINT_LOAD_SOURCE),
    read(method::ARB_CONFIG_GET),
    // ── Action: live execution / gate / dispatch state ─────────────────────
    // These are the verbs #177 found penetrating: the old whitelist was a
    // configuration-verb list, so nothing here was ever registered.
    action(method::ORDER_PLACE),
    action(method::ORDER_CANCEL),
    action(method::ORDER_CANCEL_ALL),
    action(method::ORDER_CANCEL_REMAINING),
    action(method::ORDER_CLOSE_FILLED),
    action(method::ORDER_FORCE_CLOSE),
    action(method::ORDER_RECONCILE),
    action(method::POSITION_EXIT),
    action(method::POSITION_FORCE_CLOSE),
    action(method::RISK_KILL),
    action(method::RISK_RESUME),
    action(method::STRATEGY_LOAD),
    action(method::STRATEGY_UNLOAD),
    action(method::STRATEGY_RELOAD),
    action(method::STRATEGY_PACKAGES_RELOAD),
    action(method::EXTENSION_ENABLE),
    action(method::EXTENSION_DISABLE),
    action(method::SE_ENABLE),
    action(method::SE_DISABLE),
    action(method::SE_ROLLBACK),
    action(method::SE_APPLY),
    action(method::SE_DECIDE),
    action(method::SYSTEM_RESET_TRADING),
    action(method::ACCOUNT_SWITCH),
    action(method::ACCOUNT_STATUS),
    action(method::ACCOUNT_FREEZE),
    action(method::ACCOUNT_UNFREEZE),
    action(method::BACKTEST_ONCHAIN_PULL),
    action(method::BACKTEST_RUN),
    // ── Write: persisted configuration ────────────────────────────────────
    write(method::RISK_SET_LIMITS),
    write(method::RISK_SET_SYSTEMIC),
    write(method::RISK_SET_EXIT),
    write(method::ROUND_DURATION_SET),
    write(method::MARKET_PLUGIN_SET),
    write(method::MARKET_CREDENTIALS_SET),
    write(method::STRATEGY_ENABLE),
    write(method::STRATEGY_PARAMS_SET),
    write(method::UNIFIED_CONFIRM),
    write(method::UNIFIED_REVOKE),
    write(method::SE_SET_AUTO),
    write(method::SYSTEM_ENV_RELOAD),
    write(method::SYSTEM_UPDATE_CONFIGURE),
    write(method::SYSTEM_UPDATE_CHECK),
    write(method::SYSTEM_UPDATE_STAGE),
    write(method::ACCOUNT_VENUE_SET),
    write(method::ACCOUNT_ROUNDS_SET),
    write(method::ACCOUNT_TRADING_SET),
    write(method::ACCOUNT_STRATEGIES_SET),
    write(method::DISPLAY_ACCOUNT_SET),
    write(method::ACCOUNT_CREATE),
    write(method::ACCOUNT_REMOVE),
    write(method::EXECUTION_POLICY_SET),
    write(method::EXECUTION_POLICY_RESET),
    write(method::BLUEPRINT_SAVE),
    write(method::BLUEPRINT_SAVE_SOURCE),
    read(method::BLUEPRINT_SCHEMA),
    write(method::ARB_CONFIG_SET),
    // ── Ingest: refused today, relaxation argument in the module docs ──────
    ingest_refused(method::BOOK_SNAPSHOT),
    ingest_refused(method::TOP_OF_BOOK),
    ingest_refused(method::ENGINE_BOOK),
    ingest_refused(method::ENGINE_MARKETS),
    ingest_refused(method::SPOT_PRICE),
    // ── Stream: per-session push subscriptions ────────────────────────────
    stream(method::KLINE_SUBSCRIBE),
    stream(method::KLINE_UNSUBSCRIBE),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A verb registered twice would make the table ambiguous for
    /// [`get`] (first wins) and would quietly hide a stale entry.
    #[test]
    fn no_verb_is_registered_twice() {
        for (i, a) in METHODS.iter().enumerate() {
            for b in &METHODS[i + 1..] {
                assert_ne!(a.name, b.name, "duplicate MethodSpec entry");
            }
        }
    }

    /// The relaxable kind is `Ingest` and nothing else: this is what makes
    /// "a mis-registered Allow cannot open an execution verb" structural
    /// rather than a review habit.
    #[test]
    fn only_ingest_may_carry_allow() {
        for s in METHODS {
            if s.readonly == ReadonlyPolicy::Allow {
                assert_eq!(
                    s.kind,
                    MethodKind::Ingest,
                    "{} is not Ingest, so Allow could never take effect",
                    s.name
                );
            }
        }
    }

    /// Every non-Read verb is refused in read-only mode — the shipped table
    /// has no relaxation, so the surface is exactly the Read set.
    #[test]
    fn the_shipped_table_refuses_every_non_read_verb() {
        let relaxed: Vec<&str> = METHODS
            .iter()
            .filter(|s| s.kind != MethodKind::Read && s.allowed_in_readonly())
            .map(|s| s.name)
            .collect();
        assert!(
            relaxed.is_empty(),
            "read-only would serve non-Read verbs: {relaxed:?}"
        );
    }
}
