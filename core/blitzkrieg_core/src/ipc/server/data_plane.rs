//! Market data-plane runtime (issue #780: split out of the parent module;
//! pure move).

use super::*;

/// The data plane's boot-resolved shape, shared with every session: the
/// mount flags (config is the flag's only channel — they never change
/// mid-session) and the ALREADY-MOUNTED discovery instances. `account.venue.set`
/// reads it to decide whether a re-binding needs a runtime mount.
///
/// B1d (轮次多族): the mount book is keyed on (venue, duration) — each
/// round-length family its own discovery instance (the plugin's
/// `DiscoveryConfig` bakes the duration in at start, so one instance per
/// family is the physical shape; kalshi selects its series from
/// (`asset`, `round_duration_sec`), polymarket its slug from
/// (`asset`, duration, now)). The book holds EVERY family the deployment
/// declares — the accounts' `rounds` sets plus the boot's global
/// single value — assembled once at boot (`at_boot`); a mount is
/// idempotent per PAIR, so the same venue can carry 900s and 3600s
/// instances side by side without one restarting the other. Feed mounts
/// stay per-venue (the feed is venue-anchored, tokens arrive later via
/// `subscribe_tokens`); `venue_mounted` (venue alone) keeps its meaning
/// for the FEED idempotence the venue.set/create guards rely on.
type DroppedBatchMap =
    std::collections::BTreeMap<(String, i64), Vec<blitzkrieg_market_api::MarketDescriptor>>;

#[derive(Clone)]
pub(super) struct DataPlaneRuntime {
    pub(super) feed_ws_enabled: bool,
    pub(super) discovery_enabled: bool,
    /// The boot's discovery parameters, snapshotted so a runtime mount can
    /// hand the plugin the SAME config boot would. Polymarket's discovery
    /// derives everything from the host on its own; kalshi's picks its
    /// series from (`asset`, `round_duration_sec`) — with the zero/empty
    /// values a mount used to pass, `series_ticker_for(_, 0)` is None and
    /// the venue discovers nothing forever (the feed then starves for want
    /// of subscription tokens). #535 production replay proved it: the mount
    /// reported `runtimeEnabled: true` while zero kalshi events ever landed.
    pub(super) discovery_assets: Vec<String>,
    pub(super) discovery_round_duration_sec: i64,
    /// #1165 L3 (发现层全族覆盖): every boot-active plugin's DECLARED round
    /// family table (`supported_round_secs`), venue → families. The soft-unload
    /// active set unions these in: a multi-family discovery instance feeds one
    /// Every (venue, duration) discovery family the deployment declares —
    /// boot actives on the global value, plus every account `rounds` row
    /// (B1d). The idempotence guard: an instance whose pair is already here
    /// is never restarted (a duplicate `disc.start` would evict the running
    /// one's host handle).
    pub(super) discovery_mounted:
        std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<(String, i64)>>>,
    /// B2 (轮次多族 软卸): discovery-poll batches whose (venue, duration)
    /// family has been soft-unloaded (removed from every account's active
    /// family set while the instance itself keeps running) are DROPPED at
    /// this gate instead of reaching the engine. Count = batches dropped
    /// since boot — the operator's 「软卸还在漏喂吗」 readout. A plain
    /// AtomicUsize surfaced through `core.status`, not a metrics framework:
    /// this is the only counter the feature needs.
    /// ponytail ceiling: no aggregation/history — if a second runtime gauge
    /// ever appears, promote both into a real register.
    pub(super) soft_unloaded_polling_instances: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// RFC-661 M2: the `feed_source` reuse plane — the declared consumer→
    /// source edges plus the upstream-mount counter. `None` in tests that
    /// assembled no reuse plane: a consumer binding then runs the HISTORICAL
    /// mounts (its own feed/discovery start) and registers no edges — the
    /// pipeline is strictly additive. The cell lives on the data plane so
    /// the runtime mount path (`enable_venue_at_runtime`) and the boot
    /// assembly share ONE source of truth; the concrete host holds a clone
    /// (the inner Arcs are shared) so its `on_round_markets` fan-out reads
    /// the same edges the mount path writes.
    pub(super) feed_reuse: Option<crate::market::host::FeedReuse>,
    /// Last discovery batch the soft-unload gate dropped, keyed by
    /// (venue, canonical family duration). Polymarket's discovery loop
    /// records a family as "already fed" after `on_round_markets` returns,
    /// even when the gate dropped the batch, and will not re-feed the same
    /// slot until the slot or token signature changes (up to an hour on
    /// 1h, a day on daily; also a mid-slot 5m/15m after a roll that
    /// happened while the family was unloaded). `round_duration.set`, which
    /// re-activates the family, replays this cache through the same host
    /// path so prices and the venue clock appear without waiting for the
    /// next slot.
    pub(super) last_dropped_batches: std::sync::Arc<std::sync::Mutex<DroppedBatchMap>>,
}

impl DataPlaneRuntime {
    /// The boot shape: every boot-selected plugin is mounted on the GLOBAL
    /// duration (the historical single-instance posture), every account
    /// `rounds` family gains its own discovery instance, and every PERSISTED
    /// per-account round-duration override gains one too (#1272: the override
    /// is a family the deployment runs, not a process-wide selection) — the
    /// boot half of 轮次多族's instance pool. Feed mounts stay per-venue and
    /// are visible through [`Self::venue_mounted`]. The reuse cell installs
    /// here seeded with the BOOT-ACTIVE consumers' declared edges only — an
    /// inactive plugin never gains a data-plane face (a later runtime bind
    /// appends its edge through [`Self::feed_reuse`]).
    pub(super) fn at_boot(
        config: &CoreConfig,
        actives: &[std::sync::Arc<dyn blitzkrieg_market_api::MarketPlugin>],
        accounts: &[crate::account::Account],
        registry: &crate::market::registry::MarketPluginRegistry,
    ) -> Self {
        let global = config.default_round_duration_sec;
        let mut pairs = std::collections::BTreeSet::new();
        for p in actives {
            pairs.insert((p.name().to_string(), global));
        }
        for a in accounts {
            for &dur in &a.rounds {
                if a.venue.is_empty() {
                    // An unbound account's declaration speaks for EVERY
                    // venue this boot enabled.
                    for p in actives {
                        pairs.insert((p.name().to_string(), dur));
                    }
                } else {
                    pairs.insert((a.venue.clone(), dur));
                }
            }
        }
        // #1272 (网格池化): a PERSISTED per-account round-duration override is
        // a family this deployment runs, so the boot's mount book carries it —
        // the venue reading is `active_families`'s (a bound account speaks for
        // ITS venue, an unbound one for every venue this boot enabled). Before
        // this row the override lived only in process memory, so a restart
        // silently dropped the family: the scanner admitted it and no instance
        // polled it. An override whose account left the book speaks for
        // nothing.
        for (account_id, &duration_sec) in &config.account_round_durations {
            let Some(acc) = accounts
                .iter()
                .find(|a| a.id.as_str() == account_id.as_str())
            else {
                continue;
            };
            if acc.venue.is_empty() {
                for p in actives {
                    pairs.insert((p.name().to_string(), duration_sec));
                }
            } else {
                pairs.insert((acc.venue.clone(), duration_sec));
            }
        }
        // RFC-661 M2: the boot-seeded reuse edges — every boot-ACTIVE plugin
        // whose `feed_source` names a COMPILED-IN plugin. A declaration whose
        // plugin is not enabled this boot registers NOTHING (inactive plugins
        // get no data plane); the runtime mount path appends the edge when an
        // account actually binds the consumer.
        let feed_reuse = crate::market::host::FeedReuse::default();
        for p in actives {
            if let Some(src) = p.feed_source().filter(|src| registry.get(src).is_some()) {
                feed_reuse.add_edge(p.name(), src);
            }
        }
        Self {
            feed_ws_enabled: config.feed_ws_enabled,
            discovery_enabled: config.engine_enabled && config.discovery_enabled,
            discovery_assets: config.assets.clone(),
            discovery_round_duration_sec: global,
            discovery_mounted: std::sync::Arc::new(std::sync::Mutex::new(pairs)),
            soft_unloaded_polling_instances: std::sync::Arc::new(
                std::sync::atomic::AtomicUsize::new(0),
            ),
            feed_reuse: Some(feed_reuse),
            last_dropped_batches: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::BTreeMap::new(),
            )),
        }
    }

    /// The FEED idempotence guard (per-venue: one feed per venue, its
    /// tokens arrive later) — the shape `account.venue.set` /
    /// `account.create` have always checked.
    pub(super) fn venue_mounted(&self, venue: &str) -> bool {
        self.discovery_mounted
            .lock()
            .expect("data plane mounted set poisoned")
            .iter()
            .any(|(v, _)| v == venue)
    }

    /// The DISCOVERY idempotence guard: this exact (venue, duration) family
    /// already runs its own instance.
    pub(super) fn discovery_mounted_at(&self, venue: &str, duration: i64) -> bool {
        self.discovery_mounted
            .lock()
            .expect("data plane mounted set poisoned")
            .contains(&(venue.to_string(), duration))
    }

    pub(super) fn mark_mounted(&self, venue: &str) {
        self.discovery_mounted
            .lock()
            .expect("data plane mounted set poisoned")
            .insert((venue.to_string(), self.discovery_round_duration_sec));
    }

    pub(super) fn mark_discovery_mounted(&self, venue: &str, duration: i64) {
        self.discovery_mounted
            .lock()
            .expect("data plane mounted set poisoned")
            .insert((venue.to_string(), duration));
    }

    // ── RFC-661 M2 (feed_source 复用管道) ────────────────────────────────────

    /// The reuse plane's cell, when this session assembled one.
    pub(super) fn feed_reuse(&self) -> Option<&crate::market::host::FeedReuse> {
        self.feed_reuse.as_ref()
    }

    /// Every declared consumer→source edge among `plugins` (the boot actives;
    /// a runtime bind appends its own edge through [`FeedReuse::add_edge`]).
    /// A plugin whose `feed_source` names a COMPILED-IN plugin is an edge; a
    /// declaration naming nothing registered is inert (the consumer then runs
    /// its own mounts — the declaration's absence-of-upstream contract simply
    /// has no upstream to reuse). An INACTIVE plugin appears in no edge list:
    /// it gets no data-plane face until an account binds it.
    pub(super) fn declared_feed_edges(
        plugins: &[std::sync::Arc<dyn blitzkrieg_market_api::MarketPlugin>],
        registry: &crate::market::registry::MarketPluginRegistry,
    ) -> Vec<crate::market::host::FeedReuseEdge> {
        let mut edges: Vec<crate::market::host::FeedReuseEdge> = plugins
            .iter()
            .filter_map(|p| {
                p.feed_source()
                    .filter(|src| registry.get(src).is_some())
                    .map(|src| crate::market::host::FeedReuseEdge {
                        consumer: p.name().to_string(),
                        source: src.to_string(),
                    })
            })
            .collect();
        edges.sort_by(|a, b| (&a.consumer, &a.source).cmp(&(&b.consumer, &b.source)));
        edges.dedup();
        edges
    }

    /// B2 (轮次多族 软卸): the ACTIVE family set — every family some account
    /// currently declares, UNION the boot's global family. Computed on read
    /// from the live account book (never a second stored copy that could
    /// drift), the same口径 the boot assembly used: a bound account speaks
    /// for ITS venue, an unbound one for EVERY venue it is paired with in
    /// `pairs`'s venue dimension (the boot's actives + runtime mounts).
    /// `pairs` is the caller's already-materialized (venue, duration) mount
    /// book — the venue dimension this session actually runs.
    #[allow(dead_code)] // trading/active-set helper; market-data admit no longer consults it
    fn active_families(
        &self,
        accounts: &[crate::account::Account],
        overrides: &std::collections::HashMap<String, i64>,
        pairs: &std::collections::BTreeSet<(String, i64)>,
    ) -> std::collections::BTreeSet<(String, i64)> {
        let mut active: std::collections::BTreeSet<(String, i64)> = pairs
            .iter()
            .filter(|(_, d)| *d == self.discovery_round_duration_sec)
            .cloned()
            .collect();
        for a in accounts {
            for &dur in &a.rounds {
                if a.venue.is_empty() {
                    // An unbound account's declaration speaks for EVERY
                    // venue this session runs (the boot's same reading).
                    for (v, _) in pairs.iter() {
                        active.insert((v.clone(), dur));
                    }
                } else {
                    active.insert((a.venue.clone(), dur));
                }
            }
            // #1165 (L2 契约): a live per-account override IS the grid the
            // account's scanner runs (the selection authority) — its family
            // is ACTIVE even though no `rounds` row declares it (the
            // override write path never touches the declaration). Without
            // this row, a runtime-mounted instance for the overridden family
            // would feed batches that this gate then drops as soft-unloaded
            // — the exact live starvation the override caused pre-#1165.
            // No override → resolved falls back to the global family, a row
            // the pairs filter above already carries (no behavioural change).
            let resolved = overrides
                .get(a.id.as_str())
                .copied()
                .unwrap_or(self.discovery_round_duration_sec);
            if a.venue.is_empty() {
                for (v, _) in pairs.iter() {
                    active.insert((v.clone(), resolved));
                }
            } else {
                active.insert((a.venue.clone(), resolved));
            }
        }
        active
    }

    /// Discovery poll-output gate for MARKET DATA. Every declared family
    /// (5m/15m/1h/daily — 300/900/3600/86400, with daily canonicalized
    /// from measured 23h–25h spans) of a mounted venue stays WARM in the
    /// engine: latest markets and prices are kept so `round_duration.set`
    /// can publish a priced currentRound on the first snapshot, with no
    /// blank cards. Soft-unload still stops trading and strategy evaluation
    /// for families no account anchors (the fail-closed duration-conflict
    /// and grid-pool gates). Duration is the descriptor's declared
    /// `round_duration_sec`, never the expires/slot quotient (a daily
    /// ET-noon window measures 86402; a 1-second 5m skew can measure 299).
    /// A batch that is not a declared family, or whose venue is not
    /// mounted, is still dropped and counted.
    pub(super) async fn admit_poll_batch(
        &self,
        core: &Arc<AsyncMutex<Core>>,
        markets: &[blitzkrieg_market_api::MarketDescriptor],
    ) -> bool {
        let _ = core;
        let Some(first) = markets.first() else {
            return true;
        };
        let global = self.discovery_round_duration_sec;
        let dur = poll_batch_family_duration(first, global);
        let venue = first.venue.as_str();
        if self.venue_mounted(venue) && is_declared_data_family(dur) {
            self.forget_dropped_batch(venue, dur);
            return true;
        }
        let first = self.remember_dropped_batch(venue, dur, markets);
        if first {
            self.soft_unloaded_polling_instances
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            eprintln!(
                "blitzkrieg-core: discovery batch for {venue}@{dur}s dropped — not a declared                  data family of a mounted venue"
            );
        }
        false
    }

    fn dropped_lock(
        &self,
    ) -> std::sync::MutexGuard<
        '_,
        std::collections::BTreeMap<(String, i64), Vec<blitzkrieg_market_api::MarketDescriptor>>,
    > {
        self.last_dropped_batches
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn remember_dropped_batch(
        &self,
        venue: &str,
        dur: i64,
        markets: &[blitzkrieg_market_api::MarketDescriptor],
    ) -> bool {
        if markets.is_empty() {
            return false;
        }
        let sig = Self::batch_signature(markets);
        let mut lock = self.dropped_lock();
        let key = (venue.to_string(), dur);
        let same = lock
            .get(&key)
            .is_some_and(|prev| Self::batch_signature(prev) == sig);
        lock.insert(key, markets.to_vec());
        !same
    }

    fn forget_dropped_batch(&self, venue: &str, dur: i64) {
        self.dropped_lock().remove(&(venue.to_string(), dur));
    }

    /// Take the last dropped batch for this family, if any, so a grid write
    /// can replay it through `on_round_markets` once the family is active.
    fn batch_signature(markets: &[blitzkrieg_market_api::MarketDescriptor]) -> String {
        markets
            .iter()
            .map(|m| format!("{}:{}|{}", m.asset, m.up_token_id, m.down_token_id))
            .collect::<Vec<_>>()
            .join(",")
    }

    pub(super) fn take_dropped_batch(
        &self,
        venue: &str,
        dur: i64,
    ) -> Option<Vec<blitzkrieg_market_api::MarketDescriptor>> {
        self.dropped_lock().remove(&(venue.to_string(), dur))
    }

    /// The gauge readout (`core.status`): how many discovery-poll batches
    /// soft-unload has dropped since boot.
    pub(super) fn soft_unloaded_count(&self) -> usize {
        self.soft_unloaded_polling_instances
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Mount ONE venue's runtime components (feed + discovery) — the same two
/// mounts `run` performs for every boot-selected plugin, reused verbatim so
/// a runtime enablement is byte-for-byte the boot shape (no second mechanism).
///
/// The caller decides WHICH flags gate the mounts and WHAT discovery config
/// the plugin gets (this function does not re-read them): boot passes
/// `config.feed_ws_enabled` / `config.engine_enabled && config.discovery_enabled`;
/// the venue.set/create writes pass the session's same resolved flags plus the
/// boot's snapshotted assets/round length, so a runtime binding follows the
/// boot's data-plane posture exactly. Passing an empty/zero discovery config
/// here is NOT an option: polymarket tolerates it (it derives everything from
/// the host), kalshi selects its series from (`asset`, `round_duration_sec`)
/// and would silently discover nothing forever.
///
/// B1d (轮次多族): `families` lists every (venue, duration) pair to mount —
/// the venue's GLOBAL family plus each family the account's `rounds` row
/// declares (the same set `at_boot` pre-records; pairs already mounted are
/// skipped by the caller's idempotence guard). ONE feed mount runs per
/// call (the feed is venue-anchored, duration-blind); discovery starts
/// once per family.
///
/// Executor: NOT mounted here. `mode.may_trade()` is a boot-frozen promise —
/// Dry/ReadOnly never construct egress, and a Live core's executor set is
/// built around the boot's own plugins (`ExecutorConfig.markets` etc.). A
/// venue bound at runtime gains market DATA (what the 行情 page starves on);
/// live egress for a late-bound venue is the next batch's contract, and the
/// boot-derived set already covers every venue bound BEFORE the process
/// started.
#[allow(clippy::too_many_arguments)]
pub(super) async fn enable_venue_at_runtime(
    plugin: &dyn blitzkrieg_market_api::MarketPlugin,
    host: std::sync::Arc<dyn blitzkrieg_market_api::MarketHost>,
    feed_ws_enabled: bool,
    discovery_enabled: bool,
    discovery_assets: &[String],
    discovery_round_duration_sec: i64,
    families: &[(String, i64)],
    registry: &crate::market::registry::MarketPluginRegistry,
    // RFC-661 M2: the session's data plane — its `discovery_mounted` set IS
    // the enabled-venues guard (boot actives pre-record every active venue's
    // families; runtime binds append), and its optional reuse cell carries
    // the declared edges + the upstream-mount counter.
    data_plane: &DataPlaneRuntime,
) {
    let name = plugin.name();
    // RFC-661 M2 (feed_source 复用管道): a consumer plugin owns NO upstream —
    // its own feed/discovery must NOT start (that would be a second upstream
    // connection, the exact thing the reuse pipeline exists to prevent).
    // Instead:
    //   * the consumer's venue joins the reuse plane's declared edges (its
    //     own `(venue, token)` rows re-register from the source's batches
    //     through the host's fan-out), and
    //   * the SOURCE plugin's data components mount here when the source is
    //     not already mounted (consumer-only enablement: the data plane is
    //     self-sufficient; the source's EXECUTOR is never constructed — this
    //     function never mounts executors, and the source reaches it as a
    //     data-only registry fetch).
    // The source-mounted guard is the data plane's own `discovery_mounted`
    // set: boot actives pre-record every active venue's families (at_boot),
    // so a source among the actives is already running and a second mount
    // would duplicate the upstream. Each source component mounts EXACTLY
    // once (N consumers of the same source still read one upstream stream —
    // the mount counter is the falsifiable claim's observable).
    if let Some(src) = plugin
        .feed_source()
        .filter(|src| registry.get(src).is_some())
    {
        let reuse = data_plane.feed_reuse();
        // The declared edge (idempotent): a re-bind of the same consumer
        // appends nothing.
        match reuse {
            Some(r) => r.add_edge(name, src),
            None => false,
        };
        let source_plugin = registry.get(src);
        let source_enabled = source_plugin
            .as_ref()
            .map(|sp| data_plane.venue_mounted(sp.name()))
            .unwrap_or(false);
        if let Some(sp) = source_plugin {
            // Whether the SOURCE's feed actually started this bind — the
            // venue-mounted mark at the bottom fires only on a real start,
            // so a failed start stays retryable by the next consumer bind.
            let mut feed_started = false;
            // The source's FEED mounts only when the source is not already
            // venue-mounted (boot actives / an earlier bind): the per-venue
            // guard keeps the upstream at ONE feed for any number of
            // consumers and re-binds.
            if !source_enabled {
                if feed_ws_enabled {
                    match sp.data_feed() {
                        Some(feed) => {
                            let cfg = blitzkrieg_market_api::DataFeedConfig::default();
                            let ok = match feed.start(host.clone(), cfg, Vec::new()).await {
                                Err(e) => {
                                    eprintln!(
                                        "blitzkrieg-core: data feed start failed for {src}: {e}"
                                    );
                                    false
                                }
                                Ok(()) => true,
                            };
                            if ok {
                                feed_started = true;
                                if let Some(r) = reuse {
                                    r.mounts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                                eprintln!(
                                    "blitzkrieg-core: feed started for {src} (consumer {name}'s \
                                     upstream; no executor mounted)"
                                );
                                // NOT mark_mounted here: the mount set is shared by
                                // the feed guard (`venue_mounted`) and the discovery
                                // guard (`discovery_mounted_at`) — marking the source
                                // now would insert (src, global) and make the
                                // discovery loop below read every family as
                                // already-mounted, skipping the source's discovery
                                // entirely.
                            }
                        }
                        None => {
                            eprintln!(
                                "blitzkrieg-core: {src} declares no data feed — nothing to mount"
                            );
                        }
                    }
                }
            } else {
                eprintln!(
                    "blitzkrieg-core: {name} consumes {src}'s stream (already mounted — no second \
                     upstream)"
                );
            }
            if discovery_enabled {
                for (_, duration) in families {
                    // Per-(source, family) idempotence: the same guard the
                    // venue's own mount uses — a consumer binding onto an
                    // already-mounted family must not duplicate the instance.
                    if data_plane.discovery_mounted_at(src, *duration) {
                        continue;
                    }
                    let Some(disc) = sp.discovery() else {
                        eprintln!(
                            "blitzkrieg-core: {src} declares no discovery — nothing to mount"
                        );
                        continue;
                    };
                    let cfg = blitzkrieg_market_api::DiscoveryConfig {
                        assets: discovery_assets.to_vec(),
                        round_duration_sec: *duration,
                        poll_sec: 5,
                    };
                    let ok = match disc.start(host.clone(), cfg).await {
                        Err(e) => {
                            eprintln!(
                                "blitzkrieg-core: discovery start failed for {src}@{duration}s: {e}"
                            );
                            false
                        }
                        Ok(()) => true,
                    };
                    if ok {
                        if let Some(r) = reuse {
                            r.mounts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        eprintln!(
                            "blitzkrieg-core: discovery started for {src}@{duration}s (consumer \
                             {name}'s upstream; no executor mounted)"
                        );
                        data_plane.mark_discovery_mounted(src, *duration);
                    }
                }
            }
            // The feed's idempotence mark lands AFTER the discovery loop:
            // the first bind mounts the feed and every family, THEN records
            // the source as venue-mounted — a later consumer bind (or a
            // re-bind) reads `venue_mounted(src)` == true and skips both
            // arms entirely. Marking before the loop would poison the
            // discovery guard (same set, same key shape); marking on a
            // FAILED start would wedge the source unmounted-but-marked.
            // No feed declared → nothing to retry: mark so the source still
            // counts as mounted (its discovery is the only component).
            if feed_started || (feed_ws_enabled && sp.data_feed().is_none()) {
                data_plane.mark_mounted(src);
            }
        }
        // The consumer's OWN data components never start: the fan-out is its
        // registrar. Fall through — the historical mounts below are skipped.
        return;
    }
    // #1165 (L2 契约): the feed is venue-anchored and duration-blind — an
    // already-mounted venue must never restart it. The historical callers
    // (venue.set/create) pre-checked `venue_mounted` outside; the
    // round_duration.set path calls this function to add a NEW FAMILY on a
    // venue whose feed is already live, so the guard moves inside where
    // every caller shares it. The boot shape is unchanged: boot never
    // enters this function.
    if feed_ws_enabled && !data_plane.venue_mounted(name) {
        match plugin.data_feed() {
            Some(feed) => {
                let cfg = blitzkrieg_market_api::DataFeedConfig::default();
                if let Err(e) = feed.start(host.clone(), cfg, Vec::new()).await {
                    eprintln!("blitzkrieg-core: data feed start failed for {name}: {e}");
                } else {
                    eprintln!("blitzkrieg-core: feed started for {name} (runtime enablement)");
                }
            }
            None => eprintln!("blitzkrieg-core: {name} declares no data feed — nothing to mount"),
        }
    }
    if discovery_enabled {
        for (_, duration) in families {
            match plugin.discovery() {
                Some(disc) => {
                    let cfg = blitzkrieg_market_api::DiscoveryConfig {
                        assets: discovery_assets.to_vec(),
                        round_duration_sec: *duration,
                        poll_sec: 5,
                    };
                    if let Err(e) = disc.start(host.clone(), cfg).await {
                        eprintln!(
                            "blitzkrieg-core: discovery start failed for {name}@{duration}s: {e}"
                        );
                    } else if *duration == discovery_round_duration_sec {
                        eprintln!(
                            "blitzkrieg-core: discovery started for {name} (runtime enablement)"
                        );
                    } else {
                        eprintln!(
                            "blitzkrieg-core: discovery started for {name} @ {duration}s family \
                             (runtime enablement)"
                        );
                    }
                }
                None => {
                    eprintln!("blitzkrieg-core: {name} declares no discovery — nothing to mount")
                }
            }
        }
    }
}

/// Resolve the family duration of a discovery poll batch.
///
/// Prefer the descriptor's declared `round_duration_sec`. The expiry/slot
/// quotient is only a fallback for older plugins that left the field at 0.
/// Daily windows are then canonicalized onto the declared 86400s key.
pub(super) fn poll_batch_family_duration(
    first: &blitzkrieg_market_api::MarketDescriptor,
    global: i64,
) -> i64 {
    let raw = if first.round_duration_sec > 0 {
        first.round_duration_sec
    } else if first.round_slot > 0 && first.expires_at_ms > 0 {
        first.expires_at_ms / 1000 / first.round_slot
    } else {
        global
    };
    canonical_family_duration(raw)
}

/// Map measured daily window lengths (ET-noon skew, DST 23h/25h, the live
/// 86402s soft-unload key) onto the declared 86400s family used by the
/// active-set gate and `round_duration.set`.
/// Families whose market and price rows stay warm even when no account
/// currently anchors them. Soft-unload must not discard these.
pub(super) fn is_declared_data_family(dur: i64) -> bool {
    matches!(canonical_family_duration(dur), 300 | 900 | 3600 | 86_400)
}

pub(super) fn canonical_family_duration(dur: i64) -> i64 {
    if (82_800..=90_000).contains(&dur) {
        86_400
    } else {
        dur
    }
}
