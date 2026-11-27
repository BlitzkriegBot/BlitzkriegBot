//! Account-level, strategy-group & per-strategy trading-hours / session-window risk gate.
//!
//! Prediction markets run 24×7, yet the underlying crypto spot/perp markets
//! inherit traditional finance session regimes (Asia quiet hours vs. London/NY
//! opens & two-sided wicks). This module lets operators gate **new entries** by
//! time-of-day, weekday, and IANA timezone (with automatic US/EU DST) at three
//! composable scopes:
//!
//! 1. **Account-level policy** (global safety net for the account);
//! 2. **Strategy-group policies** (shared session rules bound to a group of
//!    strategies, e.g. `"asia_session"` covering 4 favorite/breakout bots);
//! 3. **Strategy-level policy** (per-strategy override).
//!
//! When multiple scopes apply to a strategy, entry requires **all** matching
//! enabled scopes to allow (fail-closed intersection). Exits, stop-losses,
//! `thesis_death_exit`, and expiry settlement are NEVER gated here — the way
//! OUT always stays open.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Whether intervals list permitted hours (`Allow` / whitelist) or blocked
/// hours (`Deny` / blacklist, e.g. blocking US equity open whipsaw hours).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum TradingWindowMode {
    /// Whitelist: new entries are permitted ONLY inside `intervals`.
    Allow,
    /// Blacklist: new entries are BLOCKED inside `intervals` and allowed outside.
    #[default]
    Deny,
}

/// One time-of-day window in the policy's configured timezone.
///
/// Supports overnight wrap when `start_min > end_min` (e.g., `22:00..02:00` =
/// `start_min: 1320, end_min: 120`).
/// `weekdays` uses ISO-8601 numbering (`1 = Mon .. 7 = Sun`); an empty list
/// matches all 7 days.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeInterval {
    pub start_min: u16,
    pub end_min: u16,
    #[serde(default)]
    pub weekdays: Vec<u8>,
}

impl TimeInterval {
    pub fn validate(&self) -> Result<(), String> {
        if self.start_min >= 1440 {
            return Err(format!(
                "interval startMin={} must be in 0..1440",
                self.start_min
            ));
        }
        if self.end_min > 1440 {
            return Err(format!(
                "interval endMin={} must be in 0..=1440",
                self.end_min
            ));
        }
        if self.start_min == self.end_min {
            return Err(format!(
                "interval startMin ({}) and endMin ({}) must differ",
                self.start_min, self.end_min
            ));
        }
        for &d in &self.weekdays {
            if !(1..=7).contains(&d) {
                return Err(format!("weekday {d} must be in 1..=7 (Mon..Sun)"));
            }
        }
        Ok(())
    }

    fn day_matches(&self, iso_day: u8) -> bool {
        self.weekdays.is_empty() || self.weekdays.contains(&iso_day)
    }

    /// Whether `(iso_day, minute_of_day)` falls inside `[start_min, end_min)`.
    pub fn contains(&self, iso_day: u8, minute_of_day: u16) -> bool {
        if self.start_min < self.end_min {
            self.day_matches(iso_day)
                && minute_of_day >= self.start_min
                && minute_of_day < self.end_min
        } else {
            // Overnight wrap (e.g. 22:00 -> 02:00):
            // - late-night half belongs to today's weekday
            // - post-midnight half belongs to yesterday's weekday
            let prev_day = if iso_day == 1 { 7 } else { iso_day - 1 };
            (self.day_matches(iso_day) && minute_of_day >= self.start_min)
                || (self.day_matches(prev_day) && minute_of_day < self.end_min)
        }
    }

    pub fn format_hhmm(&self) -> String {
        format!(
            "{:02}:{:02}-{:02}:{:02}",
            self.start_min / 60,
            self.start_min % 60,
            self.end_min / 60,
            self.end_min % 60
        )
    }
}

/// Howard Hinnant's civil-from-days: days since 1970-01-01 -> `(year, month 1..=12, day 1..=31)`.
fn civil_from_days(day_idx: i64) -> (i64, u32, u32) {
    let z = day_idx + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year, m as u32, d as u32)
}

/// Howard Hinnant's days-from-civil: `(year, month 1..=12, day 1..=31)` -> days since 1970-01-01.
fn days_from_civil(y: i64, mo: u32, d: u32) -> i64 {
    let y_adj = y - if mo <= 2 { 1 } else { 0 };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj.rem_euclid(400);
    let m_adj = i64::from(mo) + if mo > 2 { -3 } else { 9 };
    let doy = (153 * m_adj + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, mo: u32) -> u32 {
    match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
            if leap { 29 } else { 28 }
        }
        _ => 30,
    }
}

fn iso_weekday(day_idx: i64) -> u8 {
    ((day_idx + 3).rem_euclid(7) + 1) as u8
}

fn nth_sunday(year: i64, month: u32, n: u32) -> u32 {
    let w1 = u32::from(iso_weekday(days_from_civil(year, month, 1)));
    let first_sun = 1 + (7 - w1) % 7;
    first_sun + (n - 1) * 7
}

fn last_sunday(year: i64, month: u32) -> u32 {
    let dim = days_in_month(year, month);
    let w_last = u32::from(iso_weekday(days_from_civil(year, month, dim)));
    dim - (w_last % 7)
}

fn us_dst_offset(now_ms: i64, std_offset_min: i32, dst_offset_min: i32) -> i32 {
    let day_idx = now_ms.div_euclid(86_400_000);
    let (year, _, _) = civil_from_days(day_idx);
    let start_day = days_from_civil(year, 3, nth_sunday(year, 3, 2));
    let end_day = days_from_civil(year, 11, nth_sunday(year, 11, 1));
    // 02:00 local standard time -> UTC ms
    let start_ms = start_day * 86_400_000 + i64::from(2 * 60 - std_offset_min) * 60_000;
    // 02:00 local daylight time -> UTC ms
    let end_ms = end_day * 86_400_000 + i64::from(2 * 60 - dst_offset_min) * 60_000;
    if now_ms >= start_ms && now_ms < end_ms {
        dst_offset_min
    } else {
        std_offset_min
    }
}

fn eu_dst_offset(now_ms: i64, std_offset_min: i32, dst_offset_min: i32) -> i32 {
    let day_idx = now_ms.div_euclid(86_400_000);
    let (year, _, _) = civil_from_days(day_idx);
    let start_day = days_from_civil(year, 3, last_sunday(year, 3));
    let end_day = days_from_civil(year, 10, last_sunday(year, 10));
    // EU/UK DST transitions simultaneously at 01:00 UTC.
    let start_ms = start_day * 86_400_000 + 3_600_000;
    let end_ms = end_day * 86_400_000 + 3_600_000;
    if now_ms >= start_ms && now_ms < end_ms {
        dst_offset_min
    } else {
        std_offset_min
    }
}

/// Resolve the active UTC offset in minutes for an optional IANA timezone name
/// at `now_ms`, automatically handling US/EU Daylight Saving Time. Falls back
/// to `fallback_offset_min` for custom or unrecognized zone strings.
pub fn resolve_tz_offset_min(timezone: Option<&str>, fallback_offset_min: i32, now_ms: i64) -> i32 {
    match timezone.map(str::trim).unwrap_or("") {
        "Asia/Shanghai" | "Asia/Hong_Kong" | "Asia/Singapore" | "Asia/Taipei" => 480,
        "Asia/Tokyo" | "Asia/Seoul" => 540,
        "UTC" | "Etc/UTC" | "GMT" => 0,
        "America/New_York" => us_dst_offset(now_ms, -300, -240),
        "America/Chicago" => us_dst_offset(now_ms, -360, -300),
        "America/Los_Angeles" => us_dst_offset(now_ms, -480, -420),
        "Europe/London" => eu_dst_offset(now_ms, 0, 60),
        "Europe/Berlin" | "Europe/Paris" | "Europe/Zurich" => eu_dst_offset(now_ms, 60, 120),
        _ => fallback_offset_min,
    }
}

/// One trading-window policy (applied at account, group, or strategy scope).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradingWindowPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub mode: TradingWindowMode,
    /// Fallback fixed UTC offset in minutes when `timezone` is absent or custom.
    #[serde(default = "default_tz_offset_min")]
    pub tz_offset_min: i32,
    /// Optional IANA timezone label (e.g. `"Asia/Shanghai"`, `"America/New_York"`, `"Europe/London"`).
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub intervals: Vec<TimeInterval>,
    /// Optional operator label / preset name.
    #[serde(default)]
    pub label: Option<String>,
}

fn default_tz_offset_min() -> i32 {
    480
}

impl Default for TradingWindowPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: TradingWindowMode::Deny,
            tz_offset_min: default_tz_offset_min(),
            timezone: Some("Asia/Shanghai".to_string()),
            intervals: Vec::new(),
            label: None,
        }
    }
}

impl TradingWindowPolicy {
    pub fn validate(&self) -> Result<(), String> {
        if !(-720..=840).contains(&self.tz_offset_min) {
            return Err(format!(
                "tzOffsetMin={} out of range (-720..=840)",
                self.tz_offset_min
            ));
        }
        if self.intervals.len() > 24 {
            return Err("too many trading window intervals (max 24)".to_string());
        }
        for iv in &self.intervals {
            iv.validate()?;
        }
        Ok(())
    }

    /// Evaluate whether a new entry at `now_ms` is permitted by THIS policy.
    pub fn check_entry(&self, scope_label: &str, now_ms: i64) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        let offset_min =
            resolve_tz_offset_min(self.timezone.as_deref(), self.tz_offset_min, now_ms);
        // Pure integer calendar arithmetic: Unix epoch 1970-01-01 00:00 UTC was
        // Thursday (ISO weekday 4 -> (0 + 3) % 7 + 1 = 4).
        let local_min = now_ms.div_euclid(60_000) + i64::from(offset_min);
        let day_idx = local_min.div_euclid(1440);
        let minute_of_day = local_min.rem_euclid(1440) as u16;
        let iso_day = iso_weekday(day_idx);
        let matched = self
            .intervals
            .iter()
            .find(|iv| iv.contains(iso_day, minute_of_day));

        let tz_tag = self.timezone.as_deref().unwrap_or("UTC");
        let now_hhmm = format!("{:02}:{:02}", minute_of_day / 60, minute_of_day % 60);
        match self.mode {
            TradingWindowMode::Deny => {
                if let Some(iv) = matched {
                    return Err(format!(
                        "{scope_label} trading window deny: {now_hhmm} ({tz_tag}) inside blocked {}",
                        iv.format_hhmm()
                    ));
                }
                Ok(())
            }
            TradingWindowMode::Allow => {
                if matched.is_some() {
                    Ok(())
                } else {
                    Err(format!(
                        "{scope_label} trading window allow: {now_hhmm} ({tz_tag}) outside permitted intervals"
                    ))
                }
            }
        }
    }
}

/// Named time-window strategy group binding multiple strategies to one shared
/// session/timezone schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradingWindowGroup {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub strategies: Vec<String>,
    #[serde(default)]
    pub policy: TradingWindowPolicy,
}

impl TradingWindowGroup {
    pub fn validate(&self) -> Result<(), String> {
        let id = self.id.trim();
        if id.is_empty() {
            return Err("trading window group id must not be empty".to_string());
        }
        for s in &self.strategies {
            if s.trim().is_empty() {
                return Err(format!(
                    "trading window group `{id}` contains an empty strategy name"
                ));
            }
        }
        self.policy
            .validate()
            .map_err(|e| format!("group `{id}`: {e}"))?;
        Ok(())
    }

    pub fn display_label(&self) -> &str {
        let n = self.name.trim();
        if n.is_empty() { self.id.trim() } else { n }
    }
}

/// Complete trading-window configuration persisted per account under
/// `account_settings` key `risk.trading_windows`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AccountTradingWindows {
    /// Account-wide trading window (applies to every strategy on this account).
    #[serde(default)]
    pub account: Option<TradingWindowPolicy>,
    /// Named strategy groups sharing a timezone/session schedule.
    #[serde(default)]
    pub groups: Vec<TradingWindowGroup>,
    /// Strategy-specific trading windows (intersected with `account` and matching `groups`).
    #[serde(default)]
    pub strategies: BTreeMap<String, TradingWindowPolicy>,
}

impl AccountTradingWindows {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(p) = &self.account {
            p.validate()?;
        }
        let mut seen_ids = BTreeSet::new();
        for g in &self.groups {
            g.validate()?;
            if !seen_ids.insert(g.id.trim().to_string()) {
                return Err(format!(
                    "duplicate trading window group id `{}`",
                    g.id.trim()
                ));
            }
        }
        for (name, p) in &self.strategies {
            if name.trim().is_empty() {
                return Err("strategy name in trading windows must not be empty".to_string());
            }
            p.validate()
                .map_err(|e| format!("strategy `{name}`: {e}"))?;
        }
        Ok(())
    }

    /// Fail-closed intersection: account-level, every matching enabled group,
    /// and strategy-level override must ALL allow entry at `now_ms`.
    pub fn check_entry(&self, strategy: &str, now_ms: i64) -> Result<(), String> {
        if let Some(acct_pol) = &self.account {
            acct_pol.check_entry("account", now_ms)?;
        }
        for g in &self.groups {
            if g.strategies.iter().any(|s| s == strategy) {
                g.policy
                    .check_entry(&format!("group:{}", g.display_label()), now_ms)?;
            }
        }
        if let Some(strat_pol) = self.strategies.get(strategy) {
            strat_pol.check_entry(&format!("strategy:{strategy}"), now_ms)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc_ms(y: i32, mo: u32, d: u32, h: u32, m: u32) -> i64 {
        ((days_from_civil(i64::from(y), mo, d) * 24 + i64::from(h)) * 60 + i64::from(m)) * 60_000
    }

    #[test]
    fn deny_mode_blocks_us_open_in_beijing_time_and_allows_outside() {
        let pol = TradingWindowPolicy {
            enabled: true,
            mode: TradingWindowMode::Deny,
            tz_offset_min: 480, // UTC+8
            timezone: Some("Asia/Shanghai".into()),
            intervals: vec![TimeInterval {
                start_min: 20 * 60,    // 20:00
                end_min: 23 * 60 + 30, // 23:30
                weekdays: vec![1, 2, 3, 4, 5],
            }],
            label: Some("避开美盘插针".into()),
        };
        // Friday 2026-11-27 13:15 UTC == 21:15 UTC+8 -> blocked
        assert!(
            pol.check_entry("account", utc_ms(2026, 11, 27, 13, 15))
                .is_err()
        );
        // Friday 2026-11-27 16:00 UTC == Saturday 00:00 UTC+8 -> allowed
        assert!(
            pol.check_entry("account", utc_ms(2026, 11, 27, 16, 0))
                .is_ok()
        );
        // Saturday 2026-11-28 13:15 UTC == Saturday 21:15 UTC+8 -> allowed (weekend excluded)
        assert!(
            pol.check_entry("account", utc_ms(2026, 11, 28, 13, 15))
                .is_ok()
        );
    }

    #[test]
    fn overnight_wrap_spans_midnight_correctly() {
        let pol = TradingWindowPolicy {
            enabled: true,
            mode: TradingWindowMode::Deny,
            tz_offset_min: 480,
            timezone: Some("Asia/Shanghai".into()),
            intervals: vec![TimeInterval {
                start_min: 22 * 60,
                end_min: 2 * 60,
                weekdays: vec![5], // Friday night into Saturday 02:00
            }],
            label: None,
        };
        assert!(
            pol.check_entry("account", utc_ms(2026, 11, 27, 15, 0))
                .is_err()
        );
        assert!(
            pol.check_entry("account", utc_ms(2026, 11, 27, 17, 30))
                .is_err()
        );
        assert!(
            pol.check_entry("account", utc_ms(2026, 11, 27, 18, 0))
                .is_ok()
        );
    }

    #[test]
    fn automatic_us_and_uk_dst_offsets() {
        // America/New_York: 2026 DST starts Sun Mar 8 07:00 UTC, ends Sun Nov 1 06:00 UTC.
        assert_eq!(
            resolve_tz_offset_min(Some("America/New_York"), 0, utc_ms(2026, 3, 8, 6, 59)),
            -300
        );
        assert_eq!(
            resolve_tz_offset_min(Some("America/New_York"), 0, utc_ms(2026, 3, 8, 7, 0)),
            -240
        );
        assert_eq!(
            resolve_tz_offset_min(Some("America/New_York"), 0, utc_ms(2026, 11, 1, 5, 59)),
            -240
        );
        assert_eq!(
            resolve_tz_offset_min(Some("America/New_York"), 0, utc_ms(2026, 11, 1, 6, 0)),
            -300
        );

        // Europe/London: 2026 DST starts Sun Mar 29 01:00 UTC, ends Sun Oct 25 01:00 UTC.
        assert_eq!(
            resolve_tz_offset_min(Some("Europe/London"), 0, utc_ms(2026, 3, 29, 0, 59)),
            0
        );
        assert_eq!(
            resolve_tz_offset_min(Some("Europe/London"), 0, utc_ms(2026, 3, 29, 1, 0)),
            60
        );
        assert_eq!(
            resolve_tz_offset_min(Some("Europe/London"), 0, utc_ms(2026, 10, 25, 0, 59)),
            60
        );
        assert_eq!(
            resolve_tz_offset_min(Some("Europe/London"), 0, utc_ms(2026, 10, 25, 1, 0)),
            0
        );
    }

    #[test]
    fn group_policy_binds_multiple_strategies_and_intersects_with_account_and_strategy() {
        let mut tw = AccountTradingWindows {
            account: Some(TradingWindowPolicy {
                enabled: true,
                mode: TradingWindowMode::Allow,
                tz_offset_min: 480,
                timezone: Some("Asia/Shanghai".into()),
                intervals: vec![TimeInterval {
                    start_min: 8 * 60,
                    end_min: 20 * 60,
                    weekdays: vec![],
                }],
                label: None,
            }),
            groups: vec![TradingWindowGroup {
                id: "asia_quiet".into(),
                name: "亚盘温和组".into(),
                strategies: vec![
                    "late_favorite".into(),
                    "consensus_favorite".into(),
                    "fresh_breakout".into(),
                ],
                policy: TradingWindowPolicy {
                    enabled: true,
                    mode: TradingWindowMode::Allow,
                    tz_offset_min: 480,
                    timezone: Some("Asia/Shanghai".into()),
                    intervals: vec![TimeInterval {
                        start_min: 8 * 60,
                        end_min: 16 * 60,
                        weekdays: vec![],
                    }],
                    label: None,
                },
            }],
            ..Default::default()
        };
        tw.strategies.insert(
            "fresh_breakout".into(),
            TradingWindowPolicy {
                enabled: true,
                mode: TradingWindowMode::Deny,
                tz_offset_min: 480,
                timezone: Some("Asia/Shanghai".into()),
                intervals: vec![TimeInterval {
                    start_min: 8 * 60,
                    end_min: 12 * 60,
                    weekdays: vec![],
                }],
                label: None,
            },
        );

        // 10:00 UTC+8 (02:00 UTC): account allows, asia_quiet allows, but fresh_breakout denies morning.
        assert!(
            tw.check_entry("late_favorite", utc_ms(2026, 11, 27, 2, 0))
                .is_ok()
        );
        assert!(
            tw.check_entry("consensus_favorite", utc_ms(2026, 11, 27, 2, 0))
                .is_ok()
        );
        assert!(
            tw.check_entry("fresh_breakout", utc_ms(2026, 11, 27, 2, 0))
                .is_err()
        );

        // 14:00 UTC+8 (06:00 UTC): all three allow.
        assert!(
            tw.check_entry("fresh_breakout", utc_ms(2026, 11, 27, 6, 0))
                .is_ok()
        );

        // 17:00 UTC+8 (09:00 UTC): account allows, asia_quiet blocks grouped strategies, ungrouped strategy allowed.
        let err = tw
            .check_entry("late_favorite", utc_ms(2026, 11, 27, 9, 0))
            .unwrap_err();
        assert!(err.contains("group:亚盘温和组"));
        assert!(
            tw.check_entry("cross_venue_arb", utc_ms(2026, 11, 27, 9, 0))
                .is_ok()
        );
    }
}
