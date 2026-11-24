//! # P0-3 · the read-only gate derived from a method table
//!
//! **Abolishes** the hand-maintained `READONLY_REFUSED` whitelist at
//! `core/blitzkrieg_core/src/ipc/server/mod.rs:356` — 27 configuration verbs
//! listed by hand, with a comment that literally says a new write verb must be
//! added there or it silently passes the gate. The audit measured 27 refused
//! verbs against 35 write verbs reachable on the read-only surface, so the
//! whitelist is not a gate: it is a list of what somebody remembered.
//!
//! **Replaces it with** one declarative table: every IPC method carries its
//! [`MethodKind`], and the read-only check is a *derivation* over that kind
//! instead of a comparison against a second list. A verb that is not in the
//! table does not exist; a write verb cannot be "forgotten" because there is no
//! second place to forget it in.
//!
//! **Issue:** P0-3. **Consuming task:** T3 (`server/mod.rs` +
//! `admission.rs`), which is not part of this freeze.
//!
//! ## What T3 must preserve
//!
//! Read-only remains a **structural** refusal (R1: `Mode::ReadOnly` refuses to
//! construct a live executor). This contract only changes how the refusal set
//! is computed; it may not weaken what the mode refuses.

/// What a method does to kernel state.
///
/// The classification exists so the read-only gate can be derived rather than
/// hand-listed. The five kinds are the whole vocabulary — a method that fits
/// none of them does not belong on the IPC surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MethodKind {
    /// Reads state and returns it. Safe on any mode, including read-only.
    Read,
    /// Mutates persistent configuration (risk limits, policy, blueprints,
    /// account bindings). The kind the current `READONLY_REFUSED` list was
    /// trying to enumerate.
    Write,
    /// Changes runtime behaviour without a configuration round-trip (enable a
    /// strategy, reload packages, freeze/unfreeze). Also refused on read-only.
    Action,
    /// Accepts inbound facts from outside the kernel (feed snapshots, external
    /// fill reports). The only kind that may be granted on a read-only core —
    /// see [`ReadonlyPolicy::Allow`].
    Ingest,
    /// Opens a long-lived output channel (event/SSE stream). Read-only safe,
    /// but must not be confused with `Read`: it never returns a closed value.
    Stream,
}

/// What the read-only posture does with a given [`MethodKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadonlyPolicy {
    /// Refused while the core runs read-only. The default for `Write` and
    /// `Action`.
    Refuse,
    /// Permitted while the core runs read-only.
    ///
    /// Only `Ingest` may choose this: a read-only core still has to accept the
    /// facts the outside world reports (there is no way to observe the world
    /// otherwise), and ingesting a fact does not authorise the kernel to act on
    /// it. Granting `Allow` to `Write` or `Action` would restore exactly the
    /// hole P0-3 documents.
    Allow,
}

/// One row of the method table: the verb, what it does, and how read-only
/// treats it.
///
/// **Abolishes:** membership in `READONLY_REFUSED`
/// (`core/blitzkrieg_core/src/ipc/server/mod.rs:356`) as the only statement of
/// "this verb is refused when read-only".
/// **Replaced by:** this row, colocated with the method's identity, so the
/// verb's kind and its gate decision cannot drift apart.
/// **Issue:** P0-3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodSpec {
    /// The wire verb exactly as it appears in `ipc::method` (e.g.
    /// `"risk.setLimits"`). `&'static str` because the table is static: a
    /// runtime-built verb is a verb nobody reviewed.
    pub name: &'static str,
    /// What the verb does to kernel state.
    pub kind: MethodKind,
    /// How the read-only posture treats it. Must be consistent with `kind`
    /// (`Ingest` may be `Allow` or `Refuse`; every other kind is `Refuse`).
    pub readonly: ReadonlyPolicy,
}
