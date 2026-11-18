//! #1276 contract phase — the event envelope (`seq` / `scope` / `filter`).
//!
//! Split out of `views.rs` rather than appended to it: `views.rs` sits at
//! 1349 lines against the 1500-line ratchet (`scripts/file-size-check.mjs`),
//! and this contract plus its shape tests is ~270 lines — appending would have
//! reddened that gate. The types are re-exported from `schema` exactly as if
//! they lived in `views.rs` (see `views.rs`'s `pub use`), so consumers see one
//! flat `crate::schema::*` surface.
//!
//! See `docs/rust-core/RFC-1276-event-envelope.md` for the contract itself,
//! the field-ownership table, and the operator decisions still open.

use super::*;

// ── Event envelope / scope / filter — #1276 CONTRACT-FIRST ──────────────────
//
// Introduced by the #1276 contract phase (`docs/rust-core/RFC-1276-event-envelope.md`).
// NOT WIRED: nothing constructs or matches these types yet — they exist so the
// operator can approve the interface before any behaviour moves. The wiring
// phase (`session.rs` forwarding loop, `Core::emit`, `SessionState`) is blocked
// on that approval and is enumerated as file:line in the RFC's §6.
//
// These are ADDITIVE and PARALLEL to `Event`: the enum keeps its exact wire
// shape (it is golden-locked by `tests/schema_family_golden.rs` family 21, and
// adding a `seq` field to every variant would touch 119 construction/match
// sites). The envelope wraps an `Event`; it does not replace one.
//
// Why the seq does not live on `Event` itself: three of the four defects in
// #1276 are properties of the STREAM (monotonic ordering, gap detection,
// per-account routing), not of any single event payload. A field on the
// payload would have to be filled by all 51 `self.emit(...)` call sites;
// an envelope is filled once, at the sink.

/// Which `Event` variant a payload is, without carrying the payload.
///
/// The wire vocabulary is `Event`'s own tag values (`ORDER_UPDATE`, `FILL`, …)
/// so a client can filter on the same strings it already sees in `kind`. Kept
/// as a separate unit enum rather than splitting `Event` into one type per
/// variant: the enum is the frozen wire contract, and 20 structs would be 20
/// new golden-locked families for zero consumer benefit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[allow(dead_code)] // #1276 contract phase: approved interface, no consumer yet.
pub enum EventKind {
    Ready,
    OrderUpdate,
    Fill,
    PositionClosed,
    RiskAlert,
    ReconcileReport,
    Error,
    EvolutionSignal,
    EvolutionApplied,
    EvolutionRejected,
    EvolutionProposed,
    EvolutionCycle,
    IntentDecision,
    KlineUpdate,
}

impl Event {
    /// The `EventKind` of this payload.
    ///
    /// Exhaustive on purpose: a new `Event` variant must not silently fall
    /// outside every `EventFilter` (that would be an unfilterable event, i.e.
    /// the #1276 defect re-created one variant at a time), so the compiler
    /// forces this mapping to be updated alongside the enum.
    #[allow(dead_code)] // #1276 contract phase: approved interface, no consumer yet.
    pub fn kind(&self) -> EventKind {
        match self {
            Event::Ready { .. } => EventKind::Ready,
            Event::OrderUpdate { .. } => EventKind::OrderUpdate,
            Event::Fill { .. } => EventKind::Fill,
            Event::PositionClosed { .. } => EventKind::PositionClosed,
            Event::RiskAlert { .. } => EventKind::RiskAlert,
            Event::ReconcileReport { .. } => EventKind::ReconcileReport,
            Event::Error { .. } => EventKind::Error,
            Event::EvolutionSignal { .. } => EventKind::EvolutionSignal,
            Event::EvolutionApplied { .. } => EventKind::EvolutionApplied,
            Event::EvolutionRejected { .. } => EventKind::EvolutionRejected,
            Event::EvolutionProposed { .. } => EventKind::EvolutionProposed,
            Event::EvolutionCycle { .. } => EventKind::EvolutionCycle,
            Event::IntentDecision { .. } => EventKind::IntentDecision,
            Event::KlineUpdate { .. } => EventKind::KlineUpdate,
        }
    }
}

/// The account (and, when the payload has one, the venue) an event belongs to.
///
/// Filled by the kernel at the emit site, never by the consumer: `OrderUpdate`
/// / `Fill` / `PositionClosed` already carry `accountId` inside the payload, but
/// `venue` is NOT on `TrackedOrder` (see `crate::model`), so a consumer cannot
/// derive the scope without a market lookup. `venue` is `None` for events that
/// are not venue-scoped (kernel lifecycle, risk, evolution).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // #1276 contract phase: approved interface, no consumer yet.
pub struct EventScope {
    pub account: AccountId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venue: Option<String>,
}

/// One event plus its stream position and scope.
///
/// `Serialize` only, matching `Event`: the kernel is the sole producer of the
/// event stream and nothing in Rust deserializes it (the consumers are JSON
/// clients).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // #1276 contract phase: approved interface, no consumer yet.
pub struct EventEnvelope {
    /// Process-local, starts at 1, `+1` per emitted event, resets to 1 on
    /// restart. NOT persisted — see RFC-1276 §3.1 for why the durable ordering
    /// belongs to the journal, not to the notification stream.
    pub seq: u64,
    /// `None` = the event is not attributable to one account (kernel lifecycle,
    /// risk alerts, evolution rounds, market data).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<EventScope>,
    pub event: Event,
}

/// What a session wants to receive.
///
/// Empty vector = that axis is UNCONSTRAINED (receive everything), so
/// `EventFilter::default()` is pass-through and reproduces today's full
/// broadcast exactly. There is deliberately no "deny everything" spelling: a
/// filter must not be able to silence a kill-switch alert, and a caller that
/// wants no events closes its connection. Rationale and the rejected
/// fail-closed alternative: RFC-1276 §4.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // #1276 contract phase: approved interface, no consumer yet.
pub struct EventFilter {
    /// Accounts to receive events for. Empty = every account.
    #[serde(default)]
    pub accounts: Vec<AccountId>,
    /// Venues to receive events for. Empty = every venue. An event whose
    /// `scope` is `None` is not venue-scoped and passes any venue constraint.
    #[serde(default)]
    pub venues: Vec<String>,
    /// Kinds to receive. Empty = every kind.
    #[serde(default)]
    pub kinds: Vec<EventKind>,
}

#[cfg(test)]
mod event_contract_tests {
    //! #1276 contract phase: these pin the SHAPE the operator is being asked to
    //! approve (field names, tag vocabulary, empty-filter default), not any
    //! behaviour — nothing is wired yet. A change here is a wire change and
    //! must go back through the RFC.

    use super::*;

    /// A bar-shaped payload for the envelope fixtures. `Kline` has no `Default`
    /// (its `close_time_ms` is derived, never passed in), so build the fields.
    fn bar() -> crate::kline::Kline {
        crate::kline::Kline {
            symbol: "T".into(),
            interval: KlineInterval::Min1,
            open_time_ms: 0,
            close_time_ms: 59_999,
            open: Decimal::ZERO,
            high: Decimal::ZERO,
            low: Decimal::ZERO,
            close: Decimal::ZERO,
            volume: Decimal::ZERO,
            trade_count: 0,
            is_closed: true,
        }
    }

    fn envelope() -> EventEnvelope {
        EventEnvelope {
            seq: 42,
            scope: Some(EventScope {
                account: AccountId("default".into()),
                venue: Some("polymarket".into()),
            }),
            event: Event::KlineUpdate { kline: bar() },
        }
    }

    #[test]
    fn envelope_is_flat_camel_case_with_seq_scope_event() {
        let json = serde_json::to_string(&envelope()).expect("serialize envelope");
        assert!(
            json.starts_with(r#"{"seq":42,"scope":{"account":"default","venue":"polymarket"}"#),
            "envelope keys/order drifted: {json}"
        );
    }

    /// `scope: None` must disappear from the wire, not serialize as `null`:
    /// a client cannot tell "unscoped event" from "scope I failed to parse" if
    /// the key is present with a null value.
    #[test]
    fn unscoped_envelope_omits_the_scope_key() {
        let json = serde_json::to_string(&EventEnvelope {
            seq: 1,
            scope: None,
            event: Event::Ready {
                version: "test".into(),
                mode: Mode::Dry,
            },
        })
        .expect("serialize unscoped envelope");
        assert!(!json.contains("scope"), "scope key must be absent: {json}");
    }

    /// The filter's `kinds` vocabulary must be `Event`'s own tag strings, so a
    /// client filters on exactly the `kind` values it receives.
    #[test]
    fn event_kind_wire_values_match_the_event_tag_vocabulary() {
        let json = serde_json::to_string(&vec![
            EventKind::OrderUpdate,
            EventKind::PositionClosed,
            EventKind::KlineUpdate,
        ])
        .expect("serialize kinds");
        assert_eq!(json, r#"["ORDER_UPDATE","POSITION_CLOSED","KLINE_UPDATE"]"#);
        let back: Vec<EventKind> = serde_json::from_str(&json).expect("deserialize kinds");
        assert_eq!(
            back,
            vec![
                EventKind::OrderUpdate,
                EventKind::PositionClosed,
                EventKind::KlineUpdate
            ]
        );
    }

    /// Pins the correspondence between every `Event` variant and its kind, so a
    /// variant can never end up unnameable by a filter.
    #[test]
    fn every_event_variant_maps_to_its_own_kind() {
        assert_eq!(
            Event::Ready {
                version: String::new(),
                mode: Mode::Dry
            }
            .kind(),
            EventKind::Ready
        );
        assert_eq!(
            Event::KlineUpdate { kline: bar() }.kind(),
            EventKind::KlineUpdate
        );
        assert_ne!(EventKind::Fill, EventKind::PositionClosed);
    }

    /// The safety default under review: an empty filter must be pass-through
    /// (today's behaviour), so a session that never configures a filter is not
    /// silently blinded.
    #[test]
    fn default_filter_is_empty_on_every_axis() {
        let f = EventFilter::default();
        assert!(f.accounts.is_empty());
        assert!(f.venues.is_empty());
        assert!(f.kinds.is_empty());
        assert_eq!(
            serde_json::to_string(&f).expect("serialize filter"),
            r#"{"accounts":[],"venues":[],"kinds":[]}"#
        );
    }

    /// A filter payload round-trips: the wiring phase will parse it from a
    /// session command, so it must be `Deserialize` as well as `Serialize`.
    #[test]
    fn filter_round_trips_through_json() {
        let f = EventFilter {
            accounts: vec![AccountId("a1".into()), AccountId("a2".into())],
            venues: vec!["polymarket".into()],
            kinds: vec![EventKind::Fill, EventKind::RiskAlert],
        };
        let json = serde_json::to_string(&f).expect("serialize filter");
        let back: EventFilter = serde_json::from_str(&json).expect("deserialize filter");
        assert_eq!(back, f);
    }
}
