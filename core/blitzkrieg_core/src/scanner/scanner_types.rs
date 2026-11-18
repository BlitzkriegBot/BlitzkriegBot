//! Scanner data types: config, timing blocks, round state and grid.
//! Venue-agnostic round math primitives shared by [`crate::scanner::Scanner`].
//! Moved verbatim from `scanner.rs` (pure code motion).

use crate::model::CryptoMarket;

/// The clock bucket for markets (and corpora) that carry no venue tag — the
/// pre-multi-plugin behaviour. Empty string == this bucket everywhere the
/// scanner keys by venue.
pub(crate) const DEFAULT_VENUE: &str = "";

/// Duration labels live with the plugins that name them: a plugin declares
/// `MarketPlugin::round_labels`, the registry aggregates them, and boot
/// threads the map into `CoreConfig::round_labels` — `round_duration.get`
/// renders exactly what the plugins claimed. The kernel keeps no label table;
/// the Polymarket extension's `gamma::duration_label` is its own copy of its
/// own vocabulary. What stays here is venue-agnostic slot-grid math.
#[derive(Debug, Clone)]
pub struct ScannerConfig {
    pub assets: Vec<String>,
    pub round_duration_sec: i64,
    pub min_round_age_sec: i64,
    pub min_time_left_sec: i64,
}

impl Default for ScannerConfig {
    fn default() -> Self {
        Self {
            assets: vec!["BTC".into(), "ETH".into(), "SOL".into(), "XRP".into()],
            round_duration_sec: 900,
            min_round_age_sec: 30,
            min_time_left_sec: 180,
        }
    }
}

/// Why the tradeable window is closed, as data rather than prose. The
/// distinction matters for E2-b (#27): a strategy may exempt the two *window*
/// gates, but NOT [`TimingBlock::NoMarkets`] — a round with no discovered
/// market has no priceable token, so nothing can be exempted into existing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingBlock {
    /// No markets discovered for the current round.
    NoMarkets,
    /// Round younger than `min_round_age_sec`.
    TooYoung { age_sec: i64, min_age_sec: i64 },
    /// Fewer than `min_time_left_sec` seconds remain in the round.
    TooCloseToExpiry {
        time_left_sec: i64,
        min_time_left_sec: i64,
    },
}

impl TimingBlock {
    /// Whether a per-strategy declaration may waive this block. A structural
    /// precondition (no market at all) is never waivable.
    pub fn exemptible(&self) -> bool {
        !matches!(self, TimingBlock::NoMarkets)
    }
}

impl std::fmt::Display for TimingBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TimingBlock::NoMarkets => write!(f, "No active markets"),
            TimingBlock::TooYoung {
                age_sec,
                min_age_sec,
            } => {
                write!(f, "Round too young ({age_sec}s < {min_age_sec}s)")
            }
            TimingBlock::TooCloseToExpiry {
                time_left_sec,
                min_time_left_sec,
            } => {
                write!(
                    f,
                    "Too close to expiry ({time_left_sec}s < {min_time_left_sec}s)"
                )
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct RoundState {
    pub slot: i64,
    pub expires_at_ms: i64,
    pub age_sec: i64,
    pub time_left_sec: i64,
    pub markets: Vec<CryptoMarket>,
}

/// [`RoundState`] without the owned markets snapshot — the slot/window clock
/// alone. Hot-path callers (engine evaluation, timing gates) read only these
/// fields; the copy this variant avoids is a heap allocation per market per
/// call (E14).
#[derive(Debug, Clone, Copy)]
pub struct RoundTiming {
    pub slot: i64,
    pub expires_at_ms: i64,
    pub age_sec: i64,
    pub time_left_sec: i64,
}

/// One anchored round grid — a `(venue, duration)` family this deployment
/// runs. The pool replaces a single global round length as the admission
/// authority: an account's grid write adds ITS family only, so a sibling's
/// family keeps its grid instead of being closed off by a deployment-wide
/// switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    /// Slot length of this family's rounds, in seconds — the divisor every
    /// slot derivation for the family floors to.
    pub duration_sec: i64,
}

impl Grid {
    pub(crate) fn new(duration_sec: i64) -> Self {
        Self { duration_sec }
    }
}
