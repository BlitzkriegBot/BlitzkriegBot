//! Account-level & per-strategy trading-hours / session-window risk gate.
//!
//! Prediction markets run 24×7, yet the underlying crypto spot/perp markets
//! inherit traditional finance session regimes (Asia quiet hours vs. US
//! pre-market / equity open two-sided wicks). This module lets operators gate
//! **new entries** by time-of-day and weekday at two composable scopes:
//!
//! 1. **Account-level policy** (global safety net for the account);
//! 2. **Strategy-level policy** (per-strategy override, e.g. pausing late-round
//!    favorites during US open while keeping breakout or arb strategies active).
//!
//! When both scopes are configured, entry requires **both** to allow
//! (fail-closed intersection). Exits, stop-losses, `thesis_death_exit`, and
//! expiry settlement are NEVER gated here — the way OUT always stays open.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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

/// One time-of-day window in the policy's configured timezone offset.
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

/// One trading-window policy (applied either at account scope or strategy scope).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradingWindowPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub mode: TradingWindowMode,
    /// Fixed UTC offset in minutes (e.g. `480` = UTC+8, `-300` = EST, `-240` = EDT).
    #[serde(default = "default_tz_offset_min")]
    pub tz_offset_min: i32,
    /// Optional display timezone label (e.g. `"Asia/Shanghai"`, `"America/New_York"`).
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
        // Pure integer calendar arithmetic: Unix epoch 1970-01-01 00:00 UTC was
        // Thursday (ISO weekday 4 -> (0 + 3) % 7 + 1 = 4).
        let local_min = now_ms.div_euclid(60_000) + i64::from(self.tz_offset_min);
        let day_idx = local_min.div_euclid(1440);
        let minute_of_day = local_min.rem_euclid(1440) as u16;
        let iso_day = ((day_idx + 3).rem_euclid(7) + 1) as u8;
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

/// Complete trading-window configuration persisted per account under
/// `account_settings` key `risk.trading_windows`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AccountTradingWindows {
    /// Account-wide trading window (applies to every strategy on this account).
    #[serde(default)]
    pub account: Option<TradingWindowPolicy>,
    /// Strategy-specific trading windows (intersected with `account` when both enabled).
    #[serde(default)]
    pub strategies: BTreeMap<String, TradingWindowPolicy>,
}

impl AccountTradingWindows {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(p) = &self.account {
            p.validate()?;
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

    /// Fail-closed intersection: both account-level and strategy-level policies
    /// must allow entry at `now_ms`.
    pub fn check_entry(&self, strategy: &str, now_ms: i64) -> Result<(), String> {
        if let Some(acct_pol) = &self.account {
            acct_pol.check_entry("account", now_ms)?;
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
    /// Howard Hinnant's civil-from-days inverse: UTC (y, mo, d, h, m) -> epoch ms.
    fn utc_ms(y: i32, mo: u32, d: u32, h: u32, m: u32) -> i64 {
        let y_adj = i64::from(y) - if mo <= 2 { 1 } else { 0 };
        let era = y_adj.div_euclid(400);
        let yoe = y_adj.rem_euclid(400);
        let m_adj = i64::from(mo) + if mo > 2 { -3 } else { 9 };
        let doy = (153 * m_adj + 2) / 5 + i64::from(d) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        ((days * 24 + i64::from(h)) * 60 + i64::from(m)) * 60_000
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
        // Friday 23:00 UTC+8 (15:00 UTC) -> blocked
        assert!(
            pol.check_entry("account", utc_ms(2026, 11, 27, 15, 0))
                .is_err()
        );
        // Saturday 01:30 UTC+8 (Friday 17:30 UTC) -> blocked (belongs to Friday's wrap)
        assert!(
            pol.check_entry("account", utc_ms(2026, 11, 27, 17, 30))
                .is_err()
        );
        // Saturday 02:00 UTC+8 (Friday 18:00 UTC) -> allowed
        assert!(
            pol.check_entry("account", utc_ms(2026, 11, 27, 18, 0))
                .is_ok()
        );
    }

    #[test]
    fn allow_mode_and_two_layer_intersection() {
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
            ..Default::default()
        };
        tw.strategies.insert(
            "consensus_favorite".into(),
            TradingWindowPolicy {
                enabled: true,
                mode: TradingWindowMode::Deny,
                tz_offset_min: 480,
                timezone: Some("Asia/Shanghai".into()),
                intervals: vec![TimeInterval {
                    start_min: 14 * 60,
                    end_min: 16 * 60,
                    weekdays: vec![],
                }],
                label: None,
            },
        );
        // 10:00 UTC+8 (02:00 UTC): both allow
        assert!(
            tw.check_entry("consensus_favorite", utc_ms(2026, 11, 27, 2, 0))
                .is_ok()
        );
        // 15:00 UTC+8 (07:00 UTC): account allows, consensus_favorite denies -> blocked for consensus_favorite, allowed for cross_venue_arb
        assert!(
            tw.check_entry("consensus_favorite", utc_ms(2026, 11, 27, 7, 0))
                .is_err()
        );
        assert!(
            tw.check_entry("cross_venue_arb", utc_ms(2026, 11, 27, 7, 0))
                .is_ok()
        );
        // 21:00 UTC+8 (13:00 UTC): account denies -> blocked for ALL strategies
        assert!(
            tw.check_entry("cross_venue_arb", utc_ms(2026, 11, 27, 13, 0))
                .is_err()
        );
    }
}
