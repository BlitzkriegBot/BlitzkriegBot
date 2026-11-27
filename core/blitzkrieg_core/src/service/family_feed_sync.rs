//! Keep [`crate::market::family_feed::FamilyFeedPlane`] in sync with the
//! account book, and expose the inspect / stamp helpers the host and the
//! `currentRounds` view use.

use super::Core;
use crate::market::family_feed::{FamilyFeedInspect, FamilyFeedPlane};

impl Core {
    /// Rebuild the plane from the configured account book. Venues with no
    /// account binding drop out (no placeholder, no subscription). Live
    /// refs are the union of every account's anchored family on its venue
    /// — two accounts on the same (venue, family) share one subscription.
    pub fn sync_family_feed_from_accounts(&self) {
        let default_dur = self.config.round_duration_sec;
        let mut users: Vec<(String, String, i64)> = self
            .accounts
            .account_list()
            .into_iter()
            .map(|a| {
                let dur = self
                    .config
                    .account_round_durations
                    .get(a.id.as_str())
                    .copied()
                    .unwrap_or(default_dur);
                (format!("account:{}", a.id.as_str()), a.venue.clone(), dur)
            })
            .collect();
        // Cross-venue arb: ensure BOTH leg venues keep live orderbooks for the
        // union of both leg accounts' anchored round durations (e.g. when leg A
        // is kalshi@900s and leg B is polymarket@300s, polymarket also keeps
        // 900s live so the two venues share a tradable 900s pair).
        if let Some(arb) = &self.arb_leg_config {
            let dur_a = self
                .config
                .account_round_durations
                .get(arb.leg_a_account.as_str())
                .copied()
                .unwrap_or(default_dur);
            let dur_b = self
                .config
                .account_round_durations
                .get(arb.leg_b_account.as_str())
                .copied()
                .unwrap_or(default_dur);
            let mut arb_durs = vec![dur_a];
            if dur_b != dur_a {
                arb_durs.push(dur_b);
            }
            for dur in arb_durs {
                if !arb.leg_a_venue.is_empty() {
                    users.push((format!("arb:leg_a:{dur}"), arb.leg_a_venue.clone(), dur));
                }
                if !arb.leg_b_venue.is_empty() {
                    users.push((format!("arb:leg_b:{dur}"), arb.leg_b_venue.clone(), dur));
                }
            }
        }
        let venues: Vec<String> = users
            .iter()
            .map(|(_, v, _)| v.clone())
            .filter(|v| !v.is_empty())
            .collect();
        let mut plane = self.family_feed.lock().unwrap_or_else(|e| e.into_inner());
        plane.set_venues_with_accounts(venues);
        plane.rebuild_live_from_accounts(users);
    }

    pub fn family_feed(&self) -> std::sync::MutexGuard<'_, FamilyFeedPlane> {
        self.family_feed.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn family_feed_inspect(&self) -> FamilyFeedInspect {
        self.family_feed().inspect()
    }

    pub fn stamp_family_feed_books(&self, tokens: &[String], now_ms: i64) {
        self.family_feed().stamp_book_batch(tokens, now_ms);
    }

    /// Record a discovered batch onto the plane and stamp only inactive
    /// tokens as `snapshot`. Live tokens keep their last live stamp so a
    /// discovery poll cannot relabel a high-freq mid as a placeholder.
    pub fn note_family_feed_markets(&self, markets: &[crate::model::CryptoMarket], now_ms: i64) {
        use std::collections::{HashMap, HashSet};
        let mut groups: HashMap<(String, i64), (Vec<String>, Vec<String>)> = HashMap::new();
        let mut stamp_tokens = Vec::new();
        for m in markets {
            if m.venue.is_empty() || m.round_duration_sec <= 0 {
                continue;
            }
            let e = groups
                .entry((m.venue.clone(), m.round_duration_sec))
                .or_default();
            e.0.push(m.asset.clone());
            e.1.push(m.up_token_id.clone());
            e.1.push(m.down_token_id.clone());
            stamp_tokens.push(m.up_token_id.clone());
            stamp_tokens.push(m.down_token_id.clone());
        }
        let mut plane = self.family_feed();
        for ((venue, dur), (assets, tokens)) in groups {
            plane.replace_family_tokens(&venue, dur, assets, tokens);
            plane.mark_placeholder_refreshed(&venue, dur, now_ms);
        }
        let live: HashSet<String> = plane.live_tokens().into_iter().collect();
        let snap: Vec<String> = stamp_tokens
            .into_iter()
            .filter(|t| !live.contains(t))
            .collect();
        plane.stamp_snapshot(&snap, now_ms);
    }
}
