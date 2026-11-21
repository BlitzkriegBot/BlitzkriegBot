//! Market scanner — round timing, clock offset and tradeable-window gates.
//!
//! Venue-agnostic round math: slot derivation, expiry countdown, venue clock
//! offset, timing gates. Polymarket slug/Gamma JSON lives in the `gamma`
//! extension module.

use crate::model::CryptoMarket;
use std::collections::{HashMap, HashSet};

#[cfg(test)]
mod scanner_tests;
mod scanner_types;

pub use scanner_types::*;

pub struct Scanner {
    cfg: ScannerConfig,
    markets: Vec<CryptoMarket>,
    /// Round-grid pool: `(venue, duration)` families this deployment runs.
    /// Empty = historic posture (each market rides its own family clock,
    /// nothing refused). The venue dim is NOT decoration: same duration on two
    /// venues = two grids, each flooring to its own wall clock; the pool
    /// records WHO anchored a family, so a per-account write never speaks for
    /// a sibling account or venue.
    grids: HashMap<(String, i64), Grid>,
    /// RFC-661 M1: `(venue, token)` → row index over `markets`. Same token
    /// under two venues = TWO independent instruments; rebuilt wholesale by
    /// `set_markets`; supersedes #627's "one row per token globally".
    token_index: HashMap<(String, String), usize>,
    /// token → venues holding it: distinguishes "unique candidate" from
    /// "ambiguous" (≥2 venues — refuse, never first-hit by insertion order).
    token_venues: HashMap<String, Vec<String>>,
    /// Venue-reported round end (ms), keyed by **(venue, duration sec)** — the
    /// multi-family clock key (B1a): a 15m close never freezes a 1h sibling's
    /// countdown; one clock per venue (Kalshi never moves Polymarket's expiry).
    /// Empty venue = legacy bucket, keyed `(DEFAULT_VENUE, cfg.round_duration_sec)`.
    actual_end_time_ms: HashMap<(String, i64), i64>,
    /// Venue time − local time (ms), keyed by VENUE only — shared by every
    /// family the venue lists.
    clock_offset_ms: HashMap<String, i64>,
    /// `(venue, family duration)` pairs already logged at INFO when refused for
    /// naming no anchored grid. `&self` gate every tick → interior mutability;
    /// repeats are debug, never read by the refusal.
    off_pool_refusal_logged: std::sync::Mutex<HashSet<(String, i64)>>,
}

impl Scanner {
    pub fn new(cfg: ScannerConfig) -> Self {
        Self {
            cfg,
            markets: Vec::new(),
            grids: HashMap::new(),
            token_index: HashMap::new(),
            token_venues: HashMap::new(),
            actual_end_time_ms: HashMap::new(),
            clock_offset_ms: HashMap::new(),
            off_pool_refusal_logged: std::sync::Mutex::new(HashSet::new()),
        }
    }

    pub fn set_config(&mut self, cfg: ScannerConfig) {
        self.cfg = cfg;
    }

    /// Anchor ONE grid: the family an account (or the boot) runs — additive by
    /// design, so anchoring 300s for account A never evicts B's 900s grid.
    /// Empty `venue` anchors the legacy bucket.
    pub fn anchor_grid(&mut self, venue: &str, duration_sec: i64) {
        if duration_sec <= 0 {
            return;
        }
        self.grids
            .insert((venue.to_string(), duration_sec), Grid::new(duration_sec));
    }

    /// Replace the whole pool with `families` (boot path: complete truth at
    /// once).
    pub fn set_grids(&mut self, families: impl IntoIterator<Item = (String, i64)>) {
        self.grids.clear();
        for (venue, duration_sec) in families {
            self.anchor_grid(&venue, duration_sec);
        }
    }

    /// The anchored grids, sorted: legacy bucket first, then venue, then duration.
    pub fn grid_keys(&self) -> Vec<(String, i64)> {
        let mut keys: Vec<(String, i64)> = self.grids.keys().cloned().collect();
        keys.sort();
        keys
    }

    /// The process default grid duration — countdown length when no per-account
    /// override anchors a different family; `cfg.round_duration_sec`, exposed
    /// read-only so callers outside the module (`round_duration.get`,
    /// price-row family-selection) can name "the grid actually ticking".
    pub fn round_duration_sec(&self) -> i64 {
        self.cfg.round_duration_sec
    }

    /// Whether `(venue, duration_sec)` is a grid this deployment runs. Empty
    /// pool → nothing refused; once anchored, the pool is the admission table —
    /// enterable iff some anchored grid runs that duration.
    pub fn admits_family(&self, venue: &str, duration_sec: i64) -> bool {
        if self.grids.is_empty() {
            return true;
        }
        if self.grids.contains_key(&(venue.to_string(), duration_sec)) {
            return true;
        }
        self.grids.values().any(|g| g.duration_sec == duration_sec)
    }

    /// Slot of `duration_sec`'s grid on `venue`'s wall clock; every slot
    /// derivation goes through here — grids are per (venue, duration).
    pub fn slot_at(&self, venue: &str, duration_sec: i64, local_now_ms: i64) -> i64 {
        let offset = self.clock_offset_ms.get(venue).copied().unwrap_or(0);
        let venue_now = local_now_ms + offset;
        venue_now / 1000 / duration_sec
    }

    pub fn markets(&self) -> &[CryptoMarket] {
        &self.markets
    }

    pub fn market(&self, asset: &str) -> Option<&CryptoMarket> {
        let up = asset.to_uppercase();
        self.markets.iter().find(|m| m.asset == up)
    }
    /// RFC-661 M1 venue-qualified lookup: a same-byte token under two venues
    /// resolves to the venue's OWN row; a venue holding no row returns `None`
    /// even if a sibling lists it.
    pub fn market_for_token_on_venue(&self, venue: &str, token: &str) -> Option<&CryptoMarket> {
        let idx = *self
            .token_index
            .get(&(venue.to_string(), token.to_string()))?;
        self.markets
            .get(idx)
            .filter(|m| m.venue == venue && (m.up_token_id == token || m.down_token_id == token))
    }

    /// RFC-661 §4.3 legacy lookup: resolves ONLY on a unique candidate (the
    /// whole table holds exactly one row carrying this token). Ambiguity
    /// (≥2 venues) refuses `None` — never first-hit by insertion order (#638).
    pub fn market_for_token(&self, token: &str) -> Option<&CryptoMarket> {
        let venues = self.token_venues.get(token)?;
        if venues.len() != 1 {
            return None;
        }
        self.market_for_token_on_venue(&venues[0], token)
    }

    /// The venues whose rows carry `token` (sorted, deterministic) — the
    /// ambiguity diagnostic naming candidate venues for "unique candidate"
    /// vs "refused as ambiguous".
    pub fn venues_holding_token(&self, token: &str) -> Vec<String> {
        let mut v = self.token_venues.get(token).cloned().unwrap_or_default();
        v.sort();
        v
    }

    /// Rebuild both indexes from the row list. `set_markets` is the only
    /// write path, so the indexes are always the exact projection of
    /// `markets`.
    fn rebuild_token_index(&mut self) {
        self.token_index.clear();
        self.token_venues.clear();
        for (i, m) in self.markets.iter().enumerate() {
            for t in [&m.up_token_id, &m.down_token_id] {
                self.token_index.insert((m.venue.clone(), t.clone()), i);
                let venues = self.token_venues.entry(t.clone()).or_default();
                if !venues.contains(&m.venue) {
                    venues.push(m.venue.clone());
                }
            }
        }
    }

    pub fn set_markets(&mut self, markets: Vec<CryptoMarket>) {
        self.markets = markets;
        self.rebuild_token_index();
    }

    pub fn clock_offset_ms(&self) -> i64 {
        self.clock_offset_ms
            .get(DEFAULT_VENUE)
            .copied()
            .or_else(|| self.clock_offset_ms.values().copied().min())
            .unwrap_or(0)
    }

    /// Round clock without a markets copy: slot, expiry and the two window
    /// gates — the shape the evaluation hot path consumes (it walks the live
    /// market list by reference; `round_state` remains for snapshot callers).
    ///
    /// Global clock is the EARLIEST observed expiry (grid default when none): a
    /// round is tradeable only while every venue's market is open, so the most
    /// conservative expiry gates the shared window. One venue = old
    /// single-value behaviour (observed end replaces the grid, never min'ed
    /// with it).
    pub fn round_timing(&self, local_now_ms: i64) -> RoundTiming {
        self.round_timing_for_venue(DEFAULT_VENUE, local_now_ms)
    }

    /// Same clock, refined to ONE venue's observed expiry (empty venue = legacy
    /// bucket). Callers that know the market's venue (per-candidate gates,
    /// per-token exits) use this so one venue's observed drift never freezes a
    /// sibling venue's trading.
    ///
    /// 单族兼容（B1a 回放逐字节不变的关键）：此处 = `round_timing_for` 于
    /// `(venue, cfg.round_duration_sec)` 的同族折叠；旧调用方的观察全带 cfg
    /// duration，单族语料下同族 min ≡ 旧全表 min，断言钉在
    /// `legacy_venue_entry_equals_pre_b1a_semantics`。
    pub fn round_timing_for_venue(&self, venue: &str, local_now_ms: i64) -> RoundTiming {
        self.round_timing_for(venue, self.cfg.round_duration_sec, local_now_ms)
    }

    /// E3 (轮次多族收尾): the legacy VENUE bucket's clock at an explicit
    /// duration — `round_timing()` with only the duration refined. Strategy
    /// sites (`evaluate`/`replay_candidates`/`confirmed_diagnostics`) name no
    /// venue, so the venue dimension stays the historical legacy bucket while
    /// the duration grid follows the market rows' own declared family. A
    /// single-family table resolves the duration to the configured grid —
    /// verbatim `round_timing()`.
    pub fn round_timing_legacy_at(&self, duration_sec: i64, local_now_ms: i64) -> RoundTiming {
        self.round_timing_for(DEFAULT_VENUE, duration_sec, local_now_ms)
    }

    /// Multi-family clock (B1a): round timing of ONE (venue, duration) family.
    /// Slot grid uses the family's duration (venue-offset adjusted); the
    /// family's own expiry declaration is authoritative; conservative fallback
    /// folds ONLY same-duration observations — a PM 1h market's `time_left`
    /// never pinned to 0 by a kalshi 15m sibling's expiry (cross-family decay
    /// is not contagion).
    pub fn round_timing_for(
        &self,
        venue: &str,
        duration_sec: i64,
        local_now_ms: i64,
    ) -> RoundTiming {
        let slot = self.slot_at(venue, duration_sec, local_now_ms);
        let grid = (slot + 1) * duration_sec * 1000;
        // Same-family fold only: the family key carries the duration, so a
        // sibling family's nearer expiry never gates this clock. No own
        // declaration → earliest same-duration expiry (conservative shared
        // window), else the family grid; single-family deployments fold the
        // whole table, byte-identical with pre-B1a `round_timing_for_venue`.
        let earliest_in_family = self
            .actual_end_time_ms
            .iter()
            .filter(|((_, d), t)| *d == duration_sec && **t > 0)
            .map(|(_, t)| *t)
            .min();
        let declared = match self
            .actual_end_time_ms
            .get(&(venue.to_string(), duration_sec))
        {
            // The venue's own declaration (in this family) is authoritative
            // for its markets.
            Some(t) if *t > 0 => Some(*t),
            // No own declaration: the conservative family clock — the earliest
            // observed expiry across same-duration venues.
            _ => earliest_in_family,
        };
        // A stored end at or before `local_now` is the previous window's
        // close. After a roll that close equals the new start, so using it
        // pins `time_left` to 0 and `age` to the full duration while `slot`
        // / the venue round-id have already moved — the panel then shows
        // the current 5m id with a dead countdown. Fall forward to the
        // unoffset family grid until a fresh observation lands. A still-open
        // end (including a Polymarket daily close at America/New_York noon)
        // is kept even if a sibling family's observe moved this venue's
        // clock_offset and shifted `slot_at`.
        let wall_grid = (local_now_ms / 1000 / duration_sec + 1) * duration_sec * 1000;
        let effective = match declared {
            Some(t) if t > local_now_ms => t,
            _ => wall_grid.max(grid),
        };
        let time_left = ((effective - local_now_ms) / 1000).max(0);
        RoundTiming {
            slot,
            expires_at_ms: effective,
            age_sec: duration_sec - time_left,
            time_left_sec: time_left,
        }
    }

    /// The (venue, duration) clocks that have SPOKEN (an in-band expiry
    /// declaration landed in their slot), as ordered unique pairs — the key set
    /// of `round_timing_for`; B1a wire assembly renders one row per family with
    /// `durationSec` carried. Sort deterministic: legacy venue first, then venue
    /// name lexicographic, then duration ascending — gates can assert row order
    /// byte-for-byte. `observed_venue_keys` (legacy mouthpiece) derives unique
    /// venues from here.
    pub fn observed_clock_keys(&self) -> Vec<(String, i64)> {
        let mut keys: Vec<(String, i64)> = self
            .actual_end_time_ms
            .iter()
            .filter(|(_, t)| **t > 0)
            .map(|((v, d), _)| (v.clone(), *d))
            .collect();
        keys.sort_by(|a, b| {
            if a.0.is_empty() {
                if b.0.is_empty() {
                    a.1.cmp(&b.1)
                } else {
                    std::cmp::Ordering::Less
                }
            } else if b.0.is_empty() {
                std::cmp::Ordering::Greater
            } else {
                a.cmp(b)
            }
        });
        keys
    }

    /// The venues that have SPOKEN plus the legacy bucket when it holds a value
    /// — the key set of `round_timing_for_venue`. Read-only views walk this
    /// instead of guessing: a venue without a declaration has no authoritative
    /// clock, and inventing one for the panel would contradict the conservative
    /// window the trading side consumes.
    ///
    /// Sort deterministic: legacy bucket first, then venue name lexicographic —
    /// gates can assert row order byte-for-byte.
    pub fn observed_venue_keys(&self) -> Vec<String> {
        let mut venues: Vec<String> = Vec::new();
        for (v, _) in self.observed_clock_keys() {
            if !venues.contains(&v) {
                venues.push(v);
            }
        }
        venues
    }

    pub fn round_state(&self, local_now_ms: i64) -> RoundState {
        let t = self.round_timing(local_now_ms);
        RoundState {
            slot: t.slot,
            expires_at_ms: t.expires_at_ms,
            age_sec: t.age_sec,
            time_left_sec: t.time_left_sec,
            markets: self.markets.clone(),
        }
    }

    /// Tradeable window: markets present, round old enough, not too close to expiry.
    pub fn can_trade(&self, local_now_ms: i64) -> Result<(), String> {
        self.can_trade_reason(local_now_ms)
            .map_err(|b| b.to_string())
    }

    /// Same check as [`Self::can_trade`] as a structured reason: waivable window
    /// gate vs structural precondition (E2-b / #27). Expiry gate reads the
    /// EARLIEST expiry across venues (any-venue market about to close → not
    /// tradeable); presence/age stay global — the whole-table verdict replay
    /// wake math relies on.
    pub fn can_trade_reason(&self, local_now_ms: i64) -> Result<(), TimingBlock> {
        let t = self.round_timing(local_now_ms);
        if self.markets.is_empty() {
            return Err(TimingBlock::NoMarkets);
        }
        if t.age_sec < self.cfg.min_round_age_sec {
            return Err(TimingBlock::TooYoung {
                age_sec: t.age_sec,
                min_age_sec: self.cfg.min_round_age_sec,
            });
        }
        if t.time_left_sec < self.cfg.min_time_left_sec {
            return Err(TimingBlock::TooCloseToExpiry {
                time_left_sec: t.time_left_sec,
                min_time_left_sec: self.cfg.min_time_left_sec,
            });
        }
        Ok(())
    }

    /// Per-market [`can_trade_reason`]: the expiry gate reads the market's own
    /// venue clock — one venue running hot never freezes a sibling's candidates
    /// (multi-plugin data plane); presence/age stay global.
    ///
    /// B1c-family refinement (E2): clock is the market's own (venue, duration)
    /// family — `round_timing_for` on `round_duration_sec` (`0` = legacy →
    /// configured grid, exactly pre-E2). Old shape folded to
    /// `(venue, cfg.round_duration_sec)`, which an 86400s declaration never
    /// speaks under, so every verdict ran off a stale grid. Single-family tables
    /// resolve every row to `cfg.round_duration_sec` — identical to pre-E2.
    pub fn can_trade_reason_for(
        &self,
        market: &CryptoMarket,
        local_now_ms: i64,
    ) -> Result<(), TimingBlock> {
        if self.markets.is_empty() {
            return Err(TimingBlock::NoMarkets);
        }
        let family_duration = if market.round_duration_sec > 0 {
            market.round_duration_sec
        } else {
            self.cfg.round_duration_sec
        };
        // 查表即准入：市场的 (venue, 族时长) 必须是本部署锚定的网格之一。
        // 结构性前置（`NoMarkets` 永不可豁免）；池里没有这个族 = 无账户声明
        // 过它，配置事实而非本账户与市场的冲突（后者由风控闸门判，#1140）。
        if !self.admits_family(&market.venue, family_duration) {
            // INFO once per (venue, family); repeats are debug. Drop the lock
            // before logging so a subscriber cannot re-enter.
            let first = {
                let mut seen = self
                    .off_pool_refusal_logged
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                seen.insert((market.venue.clone(), family_duration))
            };
            if first {
                tracing::info!(
                    target: "engine",
                    market = %market.asset,
                    venue = %market.venue,
                    token = %market.up_token_id,
                    declared = market.round_duration_sec,
                    family = family_duration,
                    "round grid pool: no account anchors this market's round family — entry refused (add the family to an account's ROUNDS or set its round duration)"
                );
            } else {
                tracing::debug!(
                    target: "engine",
                    market = %market.asset,
                    venue = %market.venue,
                    token = %market.up_token_id,
                    declared = market.round_duration_sec,
                    family = family_duration,
                    "round grid pool: no account anchors this market's round family — entry refused"
                );
            }
            return Err(TimingBlock::NoMarkets);
        }
        let t = self.round_timing_for(&market.venue, family_duration, local_now_ms);
        if t.age_sec < self.cfg.min_round_age_sec {
            return Err(TimingBlock::TooYoung {
                age_sec: t.age_sec,
                min_age_sec: self.cfg.min_round_age_sec,
            });
        }
        if t.time_left_sec < self.cfg.min_time_left_sec {
            return Err(TimingBlock::TooCloseToExpiry {
                time_left_sec: t.time_left_sec,
                min_time_left_sec: self.cfg.min_time_left_sec,
            });
        }
        Ok(())
    }

    /// Whether `market` is a row of the round currently being traded — shared
    /// predicate of `books_view` and `engine.round`'s `marketPrices` rows.
    /// Judges each row on ITS OWN (venue, duration) family clock: declared
    /// `round_duration_sec` (0 = legacy → configured grid) picks the family,
    /// `round_timing_for` derives that family's slot, live iff row's slot =
    /// family's slot + 1.
    ///
    /// The #560 contract (`round_slot == 0` legacy rows always pass; expiry
    /// slot = clock's current slot + 1) judged every row on the GLOBAL grid, so
    /// foreign-family rows (a PM daily stamping `(slot, 86400)`) never matched
    /// the 900s grid's `+1` and were silently dropped. `round_slot == 0` stays
    /// an unconditional pass (old corpora, #560 bridge); single-family
    /// deployments resolve the family clock to exactly the global grid —
    /// bit-identical with pre-E2 `slot + 1`.
    ///
    /// #1165 L4: the grid pool is an ADMISSION table only
    /// (`can_trade_reason_for`) — it never filters read views. An in-flight
    /// round whose family was never anchored stays visible until natural
    /// settlement (#1129).
    pub fn is_live_round_row(&self, market: &CryptoMarket, local_now_ms: i64) -> bool {
        if market.round_slot == 0 {
            return true;
        }
        let family_duration = if market.round_duration_sec > 0 {
            market.round_duration_sec
        } else {
            self.cfg.round_duration_sec
        };
        let t = self.round_timing_for(&market.venue, family_duration, local_now_ms);
        market.round_slot == t.slot + 1
    }

    /// 唯一选族入口：`engine.round` 行情行与 `engine.books` 深度行都从此取，
    /// 同一页两读面指向同一族、同一份书。两步：(1) 按行自身族钟过滤正在
    /// 交易的一轮（[`Self::is_live_round_row`]）；(2) 多族同 (venue, asset) 时，
    /// 把当前网格族 (cfg.round_duration_sec = 权威) 稳定排最前，再按 (venue,
    /// asset) 先到先得。单族部署排序键恒 0，行序与结论不变。
    ///
    /// 旧「先到先得」让卡上显示一个账户没在交易的族：5m 倒计时每 5 分钟
    /// 翻页，卡上却是 15m 几乎不动的报价，看起来「书簿在刷、价格冻住」；
    /// 两读面还互相矛盾（盘口深度显示 A 族、行情卡显示 B 族）。
    pub fn live_round_rows(&self, local_now_ms: i64) -> Vec<&CryptoMarket> {
        let active_grid = self.cfg.round_duration_sec;
        let mut rows: Vec<&CryptoMarket> = self
            .markets
            .iter()
            .filter(|m| self.is_live_round_row(m, local_now_ms))
            .collect();
        rows.sort_by_key(|m| {
            let family = if m.round_duration_sec > 0 {
                m.round_duration_sec
            } else {
                active_grid
            };
            u8::from(family != active_grid)
        });
        let mut seen: HashSet<(String, String)> = HashSet::new();
        rows.retain(|m| seen.insert((m.venue.clone(), m.asset.clone())));
        rows
    }

    /// Record the venue-reported end time and derive the clock offset — in the
    /// reporting market's OWN venue slot.
    ///
    /// A declaration more than two rounds out (in either direction) is REFUSED
    /// rather than adopted: a discovery bug feeding a strike-ladder expiry
    /// (years ahead) or a long-stale timestamp would otherwise hijack the round
    /// clock, and every timing decision (`time_left_sec`, the exit policy's
    /// clock, the D-31 floor) drifts with it. The slot grid keeps running on
    /// its own cadence until an in-band declaration arrives.
    pub fn observe_end_time(&mut self, end_ms: i64, local_now_ms: i64) {
        self.observe_end_time_for_venue(DEFAULT_VENUE, end_ms, local_now_ms);
    }

    /// The same observation, stamped into `venue`'s clock slot. An empty
    /// venue lands in the legacy bucket, keeping #498-era corpora identical.
    ///
    /// B1a 兼容委托：duration 取 cfg 的全局轮长 —— 旧调用方（engine.rs
    /// 的 merge/rollover 观察点）零改动可编译，写入的键落在
    /// `(venue, cfg.round_duration_sec)`，即旧单族语义的精确化身。多族
    /// 写入走 [`Self::observe_end_time_for`]。
    pub fn observe_end_time_for_venue(&mut self, venue: &str, end_ms: i64, local_now_ms: i64) {
        self.observe_end_time_for(venue, self.cfg.round_duration_sec, end_ms, local_now_ms);
    }

    /// Record the venue-reported end time into the (venue, duration) family
    /// slot — the multi-family write path (B1a). The guard semantics are the
    /// per-venue path's, verbatim: non-positive declarations ignored, and a
    /// declaration more than two rounds (of THIS family's duration) out in
    /// either direction REFUSED rather than adopted, so a discovery bug or a
    /// long-stale timestamp cannot hijack the family clock. The clock offset
    /// stays keyed by venue alone — it is the venue's wall-clock property,
    /// shared by every family the venue runs.
    pub fn observe_end_time_for(
        &mut self,
        venue: &str,
        duration_sec: i64,
        end_ms: i64,
        local_now_ms: i64,
    ) {
        if end_ms <= 0 || duration_sec <= 0 {
            return;
        }
        let round_ms = duration_sec * 1000;
        if (end_ms - local_now_ms).abs() > 2 * round_ms {
            return;
        }
        self.actual_end_time_ms
            .insert((venue.to_string(), duration_sec), end_ms);
        let local_slot = (local_now_ms / 1000) / duration_sec;
        let local_expected_end = (local_slot + 1) * duration_sec * 1000;
        self.clock_offset_ms
            .insert(venue.to_string(), end_ms - local_expected_end);
    }
}
