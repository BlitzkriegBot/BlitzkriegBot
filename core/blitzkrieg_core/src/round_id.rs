//! Venue round identifiers — one number for display and the ledger.
//!
//! Polymarket (and the other epoch-style venues) name a round by its start
//! Unix timestamp, e.g. `btc-updown-5m-1795230000`, `btc-updown-1h-1795230000`,
//! `btc-updown-1d-1795194000`. The scanner's historical `round_slot` is the
//! *index* `start / duration`. The two are related by a duration-generic
//! identity: `start = slot * duration`. This module is the single conversion
//! (and the idempotent migration / read-time mapping) so display and stored
//! ledger rows do not fork.
//!
//! The scanner clock itself still uses the index: slot arithmetic
//! (`slot - other`, soft-unload `expires / slot`) stays on that scale.
//! Persisted orders, positions and trades store the venue start timestamp;
//! expiry of those rows uses [`round_end_ms`], which accepts both forms.

/// Values at or above this are treated as Unix start timestamps, not slot
/// indices. 1e9 is 2001-09-09; current 5m indices are ~6e6, 15m ~2e6,
/// 1h ~5e5, daily ~2e4, so the cut is unambiguous.
pub const VENUE_TS_FLOOR: i64 = 1_000_000_000;

/// True when `id` is a legacy scanner slot index (or the 0 "no slot" sentinel).
pub fn is_legacy_slot_index(id: i64) -> bool {
    id < VENUE_TS_FLOOR
}

/// Venue start-Unix-ts identifier for a scanner slot on `duration_sec`.
/// Idempotent: a value already in the venue-ts range is returned unchanged.
/// `duration_sec <= 0` or `slot <= 0` leaves the input alone (the 0 sentinel
/// means "no round" and must not become a fake timestamp).
pub fn venue_round_id(slot: i64, duration_sec: i64) -> i64 {
    if slot <= 0 || duration_sec <= 0 {
        return slot;
    }
    if !is_legacy_slot_index(slot) {
        return slot;
    }
    let ts = slot.saturating_mul(duration_sec);
    // A tiny test/sentinel index would produce a mid-range product still
    // below the floor; expiry would keep treating it as an index. Leave
    // those rows on the index scale. Live Polymarket slots always yield
    // a Unix ts ≥ 1e9.
    if ts < VENUE_TS_FLOOR { slot } else { ts }
}

/// End of the round, in milliseconds since the Unix epoch.
///
/// * Legacy index N covers `[N·dur, (N+1)·dur)`.
/// * Venue start ts S covers `[S, S+dur)`.
///
/// A non-positive id or duration yields 0 (callers treat 0 as "no expiry
/// fact", same as the historic `round_slot == 0` exemption).
pub fn round_end_ms(id: i64, duration_sec: i64) -> i64 {
    if id <= 0 || duration_sec <= 0 {
        return 0;
    }
    if is_legacy_slot_index(id) {
        (id + 1).saturating_mul(duration_sec).saturating_mul(1000)
    } else {
        id.saturating_add(duration_sec).saturating_mul(1000)
    }
}

/// End of the round, in seconds since the Unix epoch. Same split as
/// [`round_end_ms`].
pub fn round_end_sec(id: i64, duration_sec: i64) -> i64 {
    round_end_ms(id, duration_sec) / 1000
}

/// Common slug labels used in Polymarket (and similar) event URLs.
/// Generic across 5m / 15m / 1h / 4h / daily — not a 300/900-only table.
/// Daily uses `1d` to match the current `btc-updown-1d-{start}` slug.
pub fn duration_label(duration_sec: i64) -> Option<&'static str> {
    match duration_sec {
        300 => Some("5m"),
        900 => Some("15m"),
        3600 => Some("1h"),
        14_400 => Some("4h"),
        86_400 => Some("1d"),
        _ => None,
    }
}

/// Path under polymarket.com for an Up/Down event, e.g.
/// `btc-updown-5m-1795230000`. Asset is lowercased. `None` when the
/// duration has no known label or the round id is empty.
pub fn polymarket_updown_slug(asset: &str, duration_sec: i64, round_id: i64) -> Option<String> {
    let label = duration_label(duration_sec)?;
    if round_id <= 0 {
        return None;
    }
    let id = venue_round_id(round_id, duration_sec);
    if id < VENUE_TS_FLOOR {
        return None;
    }
    let asset = asset.trim().to_ascii_lowercase();
    if asset.is_empty() {
        return None;
    }
    Some(format!("{asset}-updown-{label}-{id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venue_id_is_slot_times_duration_for_every_family() {
        // Not hardcoded to 5m/15m: 1h and daily must use the same identity
        // so a later plugin restore of those families does not fork ids.
        let cases = [
            (5_984_100, 300, 1_795_230_000),
            (1_994_700, 900, 1_795_230_000),
            (498_675, 3600, 1_795_230_000),
            (124_668, 14_400, 1_795_219_200),
            (20_778, 86_400, 1_795_219_200),
        ];
        for (slot, dur, ts) in cases {
            assert_eq!(venue_round_id(slot, dur), ts, "slot {slot} × {dur}");
            assert_eq!(venue_round_id(ts, dur), ts, "idempotent on {ts}");
        }
    }

    #[test]
    fn sentinels_and_bad_durations_are_left_alone() {
        assert_eq!(venue_round_id(0, 300), 0);
        assert_eq!(venue_round_id(-1, 300), -1);
        assert_eq!(venue_round_id(100, 0), 100);
        assert_eq!(venue_round_id(100, -900), 100);
    }

    #[test]
    fn round_end_accepts_both_identifier_forms() {
        // Index 5984100 of a 5m family ends at 1795230300.
        assert_eq!(round_end_sec(5_984_100, 300), 1_795_230_300);
        // The venue start ts for the same round ends at the same instant.
        assert_eq!(round_end_sec(1_795_230_000, 300), 1_795_230_300);
        assert_eq!(
            round_end_ms(5_984_100, 300),
            round_end_ms(1_795_230_000, 300)
        );
        // 1h family: index and start ts agree.
        let hour_slot = 1_795_230_000 / 3600;
        assert_eq!(
            round_end_sec(hour_slot, 3600),
            round_end_sec(1_795_230_000, 3600)
        );
        assert_eq!(round_end_ms(0, 300), 0);
        assert_eq!(round_end_ms(10, 0), 0);
    }

    #[test]
    fn duration_labels_cover_epoch_families_including_1h_and_daily() {
        assert_eq!(duration_label(300), Some("5m"));
        assert_eq!(duration_label(900), Some("15m"));
        assert_eq!(duration_label(3600), Some("1h"));
        assert_eq!(duration_label(14_400), Some("4h"));
        assert_eq!(duration_label(86_400), Some("1d"));
        assert_eq!(duration_label(60), None);
    }

    #[test]
    fn polymarket_slug_is_generic_across_durations() {
        assert_eq!(
            polymarket_updown_slug("BTC", 300, 5_984_100).as_deref(),
            Some("btc-updown-5m-1795230000")
        );
        assert_eq!(
            polymarket_updown_slug("ETH", 900, 1_795_230_000).as_deref(),
            Some("eth-updown-15m-1795230000")
        );
        assert_eq!(
            polymarket_updown_slug("BTC", 3600, 1_795_230_000).as_deref(),
            Some("btc-updown-1h-1795230000")
        );
        assert_eq!(
            polymarket_updown_slug("btc", 86_400, 1_795_194_000).as_deref(),
            Some("btc-updown-1d-1795194000")
        );
        assert_eq!(polymarket_updown_slug("BTC", 300, 0), None);
    }

    #[test]
    fn tiny_indices_stay_indices_so_expiry_cannot_be_ambiguous() {
        assert_eq!(venue_round_id(1, 900), 1);
        assert_eq!(venue_round_id(2, 3600), 2);
        assert_eq!(round_end_sec(2, 3600), 3 * 3600);
    }
}
