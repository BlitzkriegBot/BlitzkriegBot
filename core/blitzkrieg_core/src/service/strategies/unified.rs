//! The unified cross-venue mapping ledger's IPC verbs (propose/candidates/
//! confirm/revoke). Moved verbatim from `service/strategies.rs` (pure code
//! motion).

use super::Core;
use crate::service::{confirm_error_text, listing_json};

impl Core {
    pub fn unified_list(&self) -> Vec<serde_json::Value> {
        self.engine
            .as_ref()
            .map(|e| {
                e.unified_mappings()
                    .values()
                    .map(|ev| {
                        serde_json::json!({
                            "id": ev.id,
                            "title": ev.title,
                            "status": ev.status,
                            "settlementSource": ev.settlement_source,
                            "venues": ev
                                .listings
                                .values()
                                .map(|l| serde_json::json!({
                                    "venue": l.venue,
                                    "conditionId": l.condition_id,
                                    "asset": l.asset,
                                    "expiresAtMs": l.expires_at_ms,
                                    "status": l.status,
                                }))
                                .collect::<Vec<_>>(),
                            "discrepancies": ev
                                .discrepancies
                                .iter()
                                .map(|d| d.as_str())
                                .collect::<Vec<_>>(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// ④ 统一映射：一轮 RoundMarkets 到达后，把这轮每个市场转成
    /// [`PlatformListing`] 喂给映射账本的候选匹配器（只提议、绝不自动
    /// confirm —— ledger 的合同是 "automatic matching proposes, the operator
    /// disposes"，自动配对会违反 cross_venue_replay_check 的 0 订单负向门）。
    /// settlement 字段发现链路拿不到：空 rules + Unknown source（双空规则在
    /// matcher 里不捏造冲突）。结果存 `unified_candidates` 供
    /// `unified.candidates` 读；每个新轮全量替换（旧轮的过期候选不留）。
    pub(crate) fn propose_unified_candidates(
        &mut self,
        markets: &[crate::model::CryptoMarket],
        now_ms: i64,
    ) {
        use blitzkrieg_market_api::unified::{
            ListingStatus, PlatformListing, SettlementSource, Venue,
        };
        self.unified_candidates.clear();
        let venue_of = |s: &str| match s {
            "polymarket" => Some(Venue::Polymarket),
            "kalshi" => Some(Venue::Kalshi),
            "predictfun" => Some(Venue::PredictFun),
            "polysimulator" => Some(Venue::PolySimulator),
            _ => None, // undeclared venue (old archives): nothing to pair
        };
        let mut by_venue: std::collections::BTreeMap<Venue, Vec<PlatformListing>> =
            Default::default();
        for m in markets {
            let Some(v) = venue_of(m.venue.as_str()) else {
                continue;
            };
            by_venue.entry(v).or_default().push(PlatformListing {
                venue: v,
                condition_id: m.condition_id.clone(),
                up_token_id: m.up_token_id.clone(),
                down_token_id: m.down_token_id.clone(),
                // matcher 的资产信号是归一化小写（"bitcoin"）；发现链路给的是 "BTC"。
                asset: Some(m.asset.to_lowercase()),
                title: m.question.clone(),
                settlement_rules: String::new(),
                settlement_source: SettlementSource::Unknown,
                expires_at_ms: m.expires_at_ms,
                status: if m.expires_at_ms > now_ms {
                    ListingStatus::Active
                } else {
                    ListingStatus::Expired
                },
            });
        }
        if by_venue.len() < 2 {
            return;
        }
        let buckets: Vec<Vec<PlatformListing>> = by_venue.into_values().collect();
        for i in 0..buckets.len() {
            for j in (i + 1)..buckets.len() {
                for cand in self
                    .unified_ledger
                    .candidates(&buckets[i], &buckets[j], 10 * 60 * 1000)
                {
                    tracing::info!(
                        target: "risk",
                        score = cand.score,
                        a_venue = cand.a.venue.as_str(),
                        b_venue = cand.b.venue.as_str(),
                        title_a = %cand.a.title,
                        title_b = %cand.b.title,
                        "unified: cross-venue mapping candidate proposed — confirm via unified.confirm"
                    );
                    self.unified_candidates.push(cand);
                }
            }
        }
    }

    /// `unified.candidates` (④): the latest proposal snapshot as read-only
    /// JSON. Empty when the current round has fewer than two declared venues.
    pub fn unified_candidates(&self) -> Vec<serde_json::Value> {
        self.unified_candidates
            .iter()
            .map(|c| {
                serde_json::json!({
                    "a": listing_json(&c.a),
                    "b": listing_json(&c.b),
                    "score": c.score,
                    "expiryDeltaMs": c.expiry_delta_ms,
                })
            })
            .collect()
    }

    /// `unified.confirm` (④): the operator confirms a candidate pair as a
    /// cross-venue mapping. The ledger revalidates (title similarity, asset
    /// identity, one-listing-one-event) and persists; the confirmed event is
    /// pushed into the Engine so strategies see it immediately, and the
    /// confirmed listings leave the proposal shelf.
    pub fn unified_confirm(
        &mut self,
        title: &str,
        actor: &str,
        listings: Vec<blitzkrieg_market_api::unified::PlatformListing>,
        at_ms: i64,
    ) -> Result<blitzkrieg_market_api::unified::UnifiedEvent, String> {
        let event = self
            .unified_ledger
            .confirm(title, actor, &listings, at_ms)
            .map_err(confirm_error_text)?;
        let confirmed_ids: std::collections::HashSet<&str> = event
            .listings
            .values()
            .map(|l| l.condition_id.as_str())
            .collect();
        self.unified_candidates.retain(|c| {
            !confirmed_ids.contains(c.a.condition_id.as_str())
                && !confirmed_ids.contains(c.b.condition_id.as_str())
        });
        if let Some(e) = self.engine.as_mut() {
            e.put_unified_mapping(event.clone());
        }
        Ok(event)
    }

    /// `unified.revoke` (④): withdraw a confirmed mapping. The ledger drops
    /// the event (audited, persisted); the Engine's copy is removed here too
    /// — the Engine has only `put` semantics, so revocation is the host's
    /// job.
    pub fn unified_revoke(
        &mut self,
        event_id: &str,
        actor: &str,
        at_ms: i64,
    ) -> Result<(), String> {
        self.unified_ledger
            .revoke(event_id, actor, at_ms)
            .map_err(|e| format!("unified revoke refused: {e:?}"))?;
        if let Some(e) = self.engine.as_mut() {
            e.remove_unified_mapping(event_id);
        }
        Ok(())
    }
}
