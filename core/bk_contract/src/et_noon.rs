//! America/New_York noon-to-noon window used by Polymarket daily slugs.
//!
//! The slug start is the most recent ET noon `<= now` (see
//! `extensions/polymarket/src/gamma.rs` `slug_epoch_daily`). Noon itself is
//! never the NY DST fold or gap (those are at 02:00); the instant is taken
//! from tzdata (chrono-tz), never from a written UTC offset. Both the core
//! currentRound clock and the plugin slug builder must use these helpers so
//! a missing observed venue close cannot fall back to a UTC-midnight grid.

use chrono::{DateTime, TimeZone, Timelike};

/// Unix timestamp of the ET-noon that opens the daily window containing
/// `now_sec`. Before 12:00 ET that is yesterday's noon; from 12:00 ET on
/// it is today's noon.
pub fn et_noon_start_sec(now_sec: i64) -> Option<i64> {
    let et = DateTime::from_timestamp(now_sec, 0)?.with_timezone(&chrono_tz::America::New_York);
    let mut noon_date = et.date_naive();
    if et.hour() < 12 {
        noon_date -= chrono::Duration::days(1);
    }
    let naive = noon_date.and_hms_opt(12, 0, 0)?;
    match chrono_tz::America::New_York.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => Some(dt.timestamp()),
        // Fail closed: noon is never ambiguous or missing in
        // America/New_York, so any other result is a programming surprise.
        _ => None,
    }
}

/// Unix timestamp of the next ET noon after the window start — the close
/// of the daily window containing `now_sec`. On DST days this is 23h or
/// 25h after the start, not a flat 86400s UTC step.
pub fn et_noon_end_sec(now_sec: i64) -> Option<i64> {
    let start = et_noon_start_sec(now_sec)?;
    let start_et = DateTime::from_timestamp(start, 0)?.with_timezone(&chrono_tz::America::New_York);
    let next_date = start_et.date_naive() + chrono::Duration::days(1);
    let naive = next_date.and_hms_opt(12, 0, 0)?;
    match chrono_tz::America::New_York.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => Some(dt.timestamp()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live Gamma pin (2026-11-21): `btc-updown-1d-1795194000` is
    /// 2026-11-20 12:00 EST; the close is 2026-11-21 12:00 EST = 1795280400.
    /// 2026-11-21 08:57 UTC is 03:57 EST, still in that window.
    /// UTC midnight of that calendar day is 1795219200 and must never be
    /// published as the daily start.
    #[test]
    fn et_noon_window_matches_gamma_slug_before_noon() {
        let now = 1_795_194_000 + 15 * 3600 + 57 * 60; // 2026-11-21 08:57 UTC
        assert_eq!(now, 1_795_251_420);
        assert_eq!(et_noon_start_sec(now), Some(1_795_194_000));
        assert_eq!(et_noon_end_sec(now), Some(1_795_280_400));
        let utc_midnight = (now / 86_400) * 86_400;
        assert_eq!(utc_midnight, 1_795_219_200);
        assert_ne!(et_noon_start_sec(now), Some(utc_midnight));
    }

    #[test]
    fn et_noon_rolls_at_exactly_noon() {
        let noon = 1_795_280_400; // 2026-11-21 12:00 EST
        assert_eq!(et_noon_start_sec(noon), Some(noon));
        assert_eq!(et_noon_start_sec(noon - 1), Some(1_795_194_000));
        assert_eq!(et_noon_end_sec(noon), Some(1_795_366_800));
    }
}
