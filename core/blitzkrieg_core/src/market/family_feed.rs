//! Traffic-economy first: one high-freq subscription per (venue, family).
//!
//! Multi-tenant accounts and display users share that single subscription
//! through a refcount. The last user leaving unsubscribes. Inactive families
//! of a venue that has a configured account keep one placeholder row per
//! asset (plus a REST snapshot every [`PLACEHOLDER_REFRESH_MS`], and an
//! immediate rediscovery when that family's window rolls). A venue with no
//! configured account gets no placeholder and no connection.
//!
//! This plane lives in-process. Plugin-level `feed_source` reuse (RFC-661)
//! is a separate share (one upstream stream per compiled-in source). A
//! second kernel process still has its own feed socket — there is no
//! cross-process feed proxy.

use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// Inactive-family REST refresh interval (5 minutes). A family's own
/// window end still triggers an immediate rediscovery, so a placeholder
/// is never a previous round.
pub const PLACEHOLDER_REFRESH_MS: i64 = 300_000;

/// How a published mid was obtained. Never invent `Live` for a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PriceSource {
    Snapshot,
    Live,
}

impl PriceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            PriceSource::Snapshot => "snapshot",
            PriceSource::Live => "live",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceFreshness {
    pub source: PriceSource,
    pub fetched_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FamilyKey {
    pub venue: String,
    pub duration_sec: i64,
}

impl FamilyKey {
    pub fn new(venue: impl Into<String>, duration_sec: i64) -> Self {
        Self {
            venue: venue.into(),
            duration_sec,
        }
    }
}

/// Inspectable economy snapshot for tests, `engine.round`, and the E2E.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FamilyFeedInspect {
    pub venues_with_accounts: Vec<String>,
    pub live_families: Vec<FamilyInspect>,
    pub placeholder_families: Vec<FamilyInspect>,
    pub live_token_count: usize,
    pub placeholder_token_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FamilyInspect {
    pub venue: String,
    pub duration_sec: i64,
    pub refs: usize,
    pub token_count: usize,
}

#[derive(Debug, Default)]
pub struct FamilyFeedPlane {
    /// Venues that have at least one configured (non-empty) account binding.
    venues_with_accounts: HashSet<String>,
    /// Live users per (venue, family). A user is `account:{id}`.
    live_refs: HashMap<FamilyKey, HashSet<String>>,
    /// Discovered tokens per family (up + down ids). One market contributes
    /// two tokens; uniqueness is by token id.
    family_tokens: HashMap<FamilyKey, HashSet<String>>,
    /// Assets seen per family — one placeholder row per (venue, family, asset).
    family_assets: HashMap<FamilyKey, HashSet<String>>,
    freshness: HashMap<String, PriceFreshness>,
    last_placeholder_refresh_ms: HashMap<FamilyKey, i64>,
}

impl FamilyFeedPlane {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_venues_with_accounts(&mut self, venues: impl IntoIterator<Item = String>) {
        self.venues_with_accounts = venues.into_iter().filter(|v| !v.is_empty()).collect();
        // Drop live refs and tokens for venues that no longer have an account.
        self.live_refs
            .retain(|k, _| self.venues_with_accounts.contains(&k.venue));
        self.family_tokens
            .retain(|k, _| self.venues_with_accounts.contains(&k.venue));
        self.family_assets
            .retain(|k, _| self.venues_with_accounts.contains(&k.venue));
    }

    pub fn venue_is_configured(&self, venue: &str) -> bool {
        !venue.is_empty() && self.venues_with_accounts.contains(venue)
    }

    /// Rebuild live refs from the authoritative account list. `users` is
    /// `(account_id, venue, duration_sec)`.
    pub fn rebuild_live_from_accounts<I, S1, S2>(&mut self, users: I)
    where
        I: IntoIterator<Item = (S1, S2, i64)>,
        S1: AsRef<str>,
        S2: AsRef<str>,
    {
        self.live_refs.clear();
        for (account_id, venue, duration_sec) in users {
            let venue = venue.as_ref();
            if venue.is_empty() || !self.venues_with_accounts.contains(venue) {
                continue;
            }
            if duration_sec <= 0 {
                continue;
            }
            self.retain_live(venue, duration_sec, account_id.as_ref());
        }
    }

    /// A user (typically `account:{id}`) wants this family high-freq.
    /// Returns true when the refcount goes 0 → 1 (the caller must subscribe).
    pub fn retain_live(&mut self, venue: &str, duration_sec: i64, user: &str) -> bool {
        if !self.venue_is_configured(venue) || duration_sec <= 0 || user.is_empty() {
            return false;
        }
        let key = FamilyKey::new(venue, duration_sec);
        let set = self.live_refs.entry(key).or_default();
        let first = set.is_empty();
        set.insert(user.to_string());
        first
    }

    /// User leaves. Returns true when the refcount hits zero (unsubscribe).
    pub fn release_live(&mut self, venue: &str, duration_sec: i64, user: &str) -> bool {
        let key = FamilyKey::new(venue, duration_sec);
        let Some(set) = self.live_refs.get_mut(&key) else {
            return false;
        };
        set.remove(user);
        if set.is_empty() {
            self.live_refs.remove(&key);
            self.demote_family_to_snapshot(venue, duration_sec);
            true
        } else {
            false
        }
    }

    pub fn is_live(&self, venue: &str, duration_sec: i64) -> bool {
        self.live_refs
            .get(&FamilyKey::new(venue, duration_sec))
            .is_some_and(|s| !s.is_empty())
    }

    pub fn live_ref_count(&self, venue: &str, duration_sec: i64) -> usize {
        self.live_refs
            .get(&FamilyKey::new(venue, duration_sec))
            .map(|s| s.len())
            .unwrap_or(0)
    }

    /// Record discovered tokens (and the asset they belong to) for one family.
    /// Replaces the previous token set for that family so a roll does not
    /// keep the previous round's tokens. No-op for a venue with no account.
    pub fn record_family_market(
        &mut self,
        venue: &str,
        duration_sec: i64,
        asset: &str,
        tokens: impl IntoIterator<Item = String>,
    ) {
        if !self.venue_is_configured(venue) || duration_sec <= 0 {
            return;
        }
        let key = FamilyKey::new(venue, duration_sec);
        let tokens: HashSet<String> = tokens.into_iter().filter(|t| !t.is_empty()).collect();
        // If this is a new asset, just union. If the family's token set is
        // being rebuilt for the same assets (a roll), the caller should use
        // `replace_family_tokens` after walking the new batch.
        self.family_assets
            .entry(key.clone())
            .or_default()
            .insert(asset.to_string());
        self.family_tokens.entry(key).or_default().extend(tokens);
    }

    /// Replace the token set for a family (used when a new discovery batch
    /// for that family arrives, including a roll).
    pub fn replace_family_tokens(
        &mut self,
        venue: &str,
        duration_sec: i64,
        assets: impl IntoIterator<Item = String>,
        tokens: impl IntoIterator<Item = String>,
    ) {
        if !self.venue_is_configured(venue) || duration_sec <= 0 {
            return;
        }
        let key = FamilyKey::new(venue, duration_sec);
        self.family_assets.insert(
            key.clone(),
            assets.into_iter().filter(|a| !a.is_empty()).collect(),
        );
        self.family_tokens
            .insert(key, tokens.into_iter().filter(|t| !t.is_empty()).collect());
    }

    pub fn live_tokens(&self) -> Vec<String> {
        let mut out = HashSet::new();
        for (key, refs) in &self.live_refs {
            if refs.is_empty() {
                continue;
            }
            if let Some(tokens) = self.family_tokens.get(key) {
                out.extend(tokens.iter().cloned());
            }
        }
        let mut v: Vec<String> = out.into_iter().collect();
        v.sort();
        v
    }

    pub fn placeholder_tokens(&self) -> Vec<String> {
        let mut out = HashSet::new();
        for (key, tokens) in &self.family_tokens {
            if !self.venue_is_configured(&key.venue) {
                continue;
            }
            if self.is_live(&key.venue, key.duration_sec) {
                continue;
            }
            out.extend(tokens.iter().cloned());
        }
        let mut v: Vec<String> = out.into_iter().collect();
        v.sort();
        v
    }

    pub fn live_tokens_for(&self, venue: &str) -> Vec<String> {
        let mut out = std::collections::HashSet::new();
        for (key, refs) in &self.live_refs {
            if key.venue != venue || refs.is_empty() {
                continue;
            }
            if let Some(tokens) = self.family_tokens.get(key) {
                out.extend(tokens.iter().cloned());
            }
        }
        let mut v: Vec<String> = out.into_iter().collect();
        v.sort();
        v
    }

    pub fn placeholder_tokens_for(&self, venue: &str) -> Vec<String> {
        if !self.venue_is_configured(venue) {
            return Vec::new();
        }
        let mut out = std::collections::HashSet::new();
        for (key, tokens) in &self.family_tokens {
            if key.venue != venue || self.is_live(&key.venue, key.duration_sec) {
                continue;
            }
            out.extend(tokens.iter().cloned());
        }
        let mut v: Vec<String> = out.into_iter().collect();
        v.sort();
        v
    }

    pub fn live_families(&self, venue: &str) -> Vec<i64> {
        let mut v: Vec<i64> = self
            .live_refs
            .iter()
            .filter(|(k, refs)| k.venue == venue && !refs.is_empty())
            .map(|(k, _)| k.duration_sec)
            .collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn placeholder_families(&self, venue: &str) -> Vec<i64> {
        if !self.venue_is_configured(venue) {
            return Vec::new();
        }
        let mut v: Vec<i64> = self
            .family_tokens
            .keys()
            .filter(|k| k.venue == venue && !self.is_live(venue, k.duration_sec))
            .map(|k| k.duration_sec)
            .collect();
        // Also include known declared families even before tokens land? The
        // host plan lists families we already have tokens for; discovery
        // itself walks the plugin table and consults cadence separately.
        v.sort();
        v.dedup();
        v
    }

    pub fn stamp_live(&mut self, tokens: &[String], now_ms: i64) {
        for t in tokens {
            if t.is_empty() {
                continue;
            }
            self.freshness.insert(
                t.clone(),
                PriceFreshness {
                    source: PriceSource::Live,
                    fetched_at_ms: now_ms,
                },
            );
        }
    }

    pub fn stamp_snapshot(&mut self, tokens: &[String], now_ms: i64) {
        for t in tokens {
            if t.is_empty() {
                continue;
            }
            self.freshness.insert(
                t.clone(),
                PriceFreshness {
                    source: PriceSource::Snapshot,
                    fetched_at_ms: now_ms,
                },
            );
        }
    }

    /// Relabel a demoted family's tokens as snapshot (honest: no longer live).
    /// Keeps `fetched_at_ms` so the panel does not invent a fresh time.
    pub fn demote_family_to_snapshot(&mut self, venue: &str, duration_sec: i64) {
        let key = FamilyKey::new(venue, duration_sec);
        let Some(tokens) = self.family_tokens.get(&key) else {
            return;
        };
        for t in tokens {
            if let Some(f) = self.freshness.get_mut(t) {
                f.source = PriceSource::Snapshot;
            }
        }
    }

    /// Stamp a just-applied book batch: live tokens get `live`, others
    /// (placeholder) get `snapshot`. Never marks a placeholder token live.
    pub fn stamp_book_batch(&mut self, tokens: &[String], now_ms: i64) {
        let live: HashSet<String> = self.live_tokens().into_iter().collect();
        let mut live_batch = Vec::new();
        let mut snap_batch = Vec::new();
        for t in tokens {
            if t.is_empty() {
                continue;
            }
            if live.contains(t) {
                live_batch.push(t.clone());
            } else {
                snap_batch.push(t.clone());
            }
        }
        self.stamp_live(&live_batch, now_ms);
        self.stamp_snapshot(&snap_batch, now_ms);
    }

    pub fn freshness(&self, token: &str) -> Option<&PriceFreshness> {
        self.freshness.get(token)
    }

    /// Placeholder REST is due when never refreshed, or 300s have elapsed.
    pub fn placeholder_due(&self, venue: &str, duration_sec: i64, now_ms: i64) -> bool {
        if !self.venue_is_configured(venue) || self.is_live(venue, duration_sec) {
            return false;
        }
        match self
            .last_placeholder_refresh_ms
            .get(&FamilyKey::new(venue, duration_sec))
        {
            None => true,
            Some(last) => now_ms.saturating_sub(*last) >= PLACEHOLDER_REFRESH_MS,
        }
    }

    pub fn mark_placeholder_refreshed(&mut self, venue: &str, duration_sec: i64, now_ms: i64) {
        self.last_placeholder_refresh_ms
            .insert(FamilyKey::new(venue, duration_sec), now_ms);
    }

    /// A placeholder must be rediscovered when its current window has ended,
    /// so the stored row is never a previous round.
    pub fn placeholder_roll_due(
        &self,
        venue: &str,
        duration_sec: i64,
        window_end_ms: i64,
        now_ms: i64,
    ) -> bool {
        if !self.venue_is_configured(venue) || self.is_live(venue, duration_sec) {
            return false;
        }
        window_end_ms > 0 && now_ms >= window_end_ms
    }

    pub fn asset_count(&self, venue: &str, duration_sec: i64) -> usize {
        self.family_assets
            .get(&FamilyKey::new(venue, duration_sec))
            .map(|s| s.len())
            .unwrap_or(0)
    }

    pub fn live_subscription_count(&self) -> usize {
        self.live_refs.values().filter(|s| !s.is_empty()).count()
    }

    pub fn inspect(&self) -> FamilyFeedInspect {
        let mut venues: Vec<String> = self.venues_with_accounts.iter().cloned().collect();
        venues.sort();
        let mut live_families: Vec<FamilyInspect> = self
            .live_refs
            .iter()
            .filter(|(_, refs)| !refs.is_empty())
            .map(|(k, refs)| FamilyInspect {
                venue: k.venue.clone(),
                duration_sec: k.duration_sec,
                refs: refs.len(),
                token_count: self.family_tokens.get(k).map(|s| s.len()).unwrap_or(0),
            })
            .collect();
        live_families.sort_by(|a, b| {
            a.venue
                .cmp(&b.venue)
                .then(a.duration_sec.cmp(&b.duration_sec))
        });
        let live_token_set: HashSet<String> = self.live_tokens().into_iter().collect();
        let mut placeholder_families: Vec<FamilyInspect> = self
            .family_tokens
            .iter()
            .filter(|(k, _)| {
                self.venue_is_configured(&k.venue) && !self.is_live(&k.venue, k.duration_sec)
            })
            .map(|(k, tokens)| FamilyInspect {
                venue: k.venue.clone(),
                duration_sec: k.duration_sec,
                refs: 0,
                token_count: tokens.len(),
            })
            .collect();
        placeholder_families.sort_by(|a, b| {
            a.venue
                .cmp(&b.venue)
                .then(a.duration_sec.cmp(&b.duration_sec))
        });
        FamilyFeedInspect {
            venues_with_accounts: venues,
            live_token_count: live_token_set.len(),
            placeholder_token_count: self.placeholder_tokens().len(),
            live_families,
            placeholder_families,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane_with_poly() -> FamilyFeedPlane {
        let mut p = FamilyFeedPlane::new();
        p.set_venues_with_accounts(["polymarket".into()]);
        p
    }

    fn record_btc(p: &mut FamilyFeedPlane, family: i64, tok_prefix: &str) {
        p.replace_family_tokens(
            "polymarket",
            family,
            ["BTC".into()],
            [format!("{tok_prefix}-up"), format!("{tok_prefix}-down")],
        );
    }

    #[test]
    fn two_accounts_on_the_same_family_share_one_subscription() {
        let mut p = plane_with_poly();
        record_btc(&mut p, 300, "5m");
        assert!(p.retain_live("polymarket", 300, "account:default"));
        assert!(
            !p.retain_live("polymarket", 300, "account:paper"),
            "second account must not open a second subscription"
        );
        assert_eq!(p.live_subscription_count(), 1);
        assert_eq!(p.live_ref_count("polymarket", 300), 2);
        assert_eq!(p.live_tokens(), vec!["5m-down", "5m-up"]);
    }

    #[test]
    fn last_user_leaving_unsubscribes() {
        let mut p = plane_with_poly();
        record_btc(&mut p, 900, "15m");
        p.retain_live("polymarket", 900, "account:a");
        p.retain_live("polymarket", 900, "account:b");
        p.stamp_live(&["15m-up".into(), "15m-down".into()], 1_000);
        assert!(!p.release_live("polymarket", 900, "account:a"));
        assert_eq!(p.live_subscription_count(), 1);
        assert!(p.release_live("polymarket", 900, "account:b"));
        assert_eq!(p.live_subscription_count(), 0);
        assert!(p.placeholder_tokens().contains(&"15m-up".to_string()));
        assert_eq!(
            p.freshness("15m-up").map(|f| f.source),
            Some(PriceSource::Snapshot),
            "demote must not keep calling the last mid live"
        );
        assert_eq!(p.freshness("15m-up").map(|f| f.fetched_at_ms), Some(1_000));
    }

    #[test]
    fn switch_promotes_new_family_and_demotes_old() {
        let mut p = plane_with_poly();
        record_btc(&mut p, 900, "15m");
        record_btc(&mut p, 3600, "1h");
        p.retain_live("polymarket", 900, "account:default");
        p.stamp_live(&["15m-up".into(), "15m-down".into()], 2_000);
        p.stamp_snapshot(&["1h-up".into(), "1h-down".into()], 1_500);

        assert!(p.release_live("polymarket", 900, "account:default"));
        assert!(p.retain_live("polymarket", 3600, "account:default"));

        assert!(!p.is_live("polymarket", 900));
        assert!(p.is_live("polymarket", 3600));
        assert_eq!(p.live_tokens(), vec!["1h-down", "1h-up"]);
        assert_eq!(p.placeholder_tokens(), vec!["15m-down", "15m-up"]);
        assert_eq!(p.freshness("15m-up").unwrap().source, PriceSource::Snapshot);
        assert_eq!(
            p.freshness("1h-up").unwrap().source,
            PriceSource::Snapshot,
            "first snapshot after switch is still the placeholder"
        );
        assert_eq!(p.freshness("1h-up").unwrap().fetched_at_ms, 1_500);

        // High-freq books arrive: only the live family is marked live.
        p.stamp_book_batch(&["1h-up".into(), "15m-up".into()], 2_500);
        assert_eq!(p.freshness("1h-up").unwrap().source, PriceSource::Live);
        assert_eq!(p.freshness("15m-up").unwrap().source, PriceSource::Snapshot);
        assert_eq!(p.freshness("1h-up").unwrap().fetched_at_ms, 2_500);
    }

    #[test]
    fn venue_without_account_gets_no_placeholder_and_no_subscription() {
        let mut p = plane_with_poly();
        p.replace_family_tokens(
            "kalshi",
            900,
            ["BTC".into()],
            ["k-up".into(), "k-down".into()],
        );
        assert!(!p.retain_live("kalshi", 900, "account:default"));
        assert!(p.placeholder_tokens().is_empty());
        assert!(p.live_tokens().is_empty());
        assert!(!p.venue_is_configured("kalshi"));
        assert!(p.placeholder_families("kalshi").is_empty());
    }

    #[test]
    fn placeholder_refresh_is_due_every_300s_not_before() {
        let mut p = plane_with_poly();
        record_btc(&mut p, 86400, "d");
        let t0 = 10_000;
        assert!(p.placeholder_due("polymarket", 86400, t0));
        p.mark_placeholder_refreshed("polymarket", 86400, t0);
        assert!(!p.placeholder_due("polymarket", 86400, t0 + PLACEHOLDER_REFRESH_MS - 1));
        assert!(p.placeholder_due("polymarket", 86400, t0 + PLACEHOLDER_REFRESH_MS));
        // A live family is not on the placeholder cadence.
        p.retain_live("polymarket", 86400, "account:default");
        assert!(!p.placeholder_due("polymarket", 86400, t0 + PLACEHOLDER_REFRESH_MS * 2));
    }

    #[test]
    fn placeholder_rediscovers_when_the_window_rolls() {
        let p = {
            let mut p = plane_with_poly();
            record_btc(&mut p, 300, "5m");
            p
        };
        let end = 1795251900_000;
        assert!(!p.placeholder_roll_due("polymarket", 300, end, end - 1));
        assert!(p.placeholder_roll_due("polymarket", 300, end, end));
        assert!(p.placeholder_roll_due("polymarket", 300, end, end + 1));
    }

    #[test]
    fn one_placeholder_row_per_asset_per_family_per_venue() {
        let mut p = plane_with_poly();
        p.record_family_market(
            "polymarket",
            300,
            "BTC",
            ["btc-up".into(), "btc-down".into()],
        );
        p.record_family_market(
            "polymarket",
            300,
            "BTC",
            ["btc-up".into(), "btc-down".into()],
        );
        p.record_family_market(
            "polymarket",
            300,
            "ETH",
            ["eth-up".into(), "eth-down".into()],
        );
        assert_eq!(p.asset_count("polymarket", 300), 2);
        assert_eq!(p.placeholder_tokens().len(), 4);
    }

    #[test]
    fn rebuild_from_accounts_is_authoritative() {
        let mut p = plane_with_poly();
        record_btc(&mut p, 300, "5m");
        record_btc(&mut p, 900, "15m");
        p.retain_live("polymarket", 300, "account:default");
        p.rebuild_live_from_accounts([("default", "polymarket", 900)]);
        assert!(!p.is_live("polymarket", 300));
        assert!(p.is_live("polymarket", 900));
        assert_eq!(p.live_ref_count("polymarket", 900), 1);
    }
}
