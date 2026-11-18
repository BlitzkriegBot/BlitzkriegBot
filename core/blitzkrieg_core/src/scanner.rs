//! Market scanner — round timing, clock offset and tradeable-window gates.
//!
//! Venue-agnostic round math: slot derivation, expiry countdown, venue clock
//! offset, timing gates. Polymarket slug/Gamma JSON lives in the `gamma`
//! extension module.

use crate::model::CryptoMarket;
use std::collections::{HashMap, HashSet};

/// The clock bucket for markets (and corpora) that carry no venue tag — the
/// pre-multi-plugin behaviour. Empty string == this bucket everywhere the
/// scanner keys by venue.
const DEFAULT_VENUE: &str = "";

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
    fn new(duration_sec: i64) -> Self {
        Self { duration_sec }
    }
}

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
        let effective = match self
            .actual_end_time_ms
            .get(&(venue.to_string(), duration_sec))
        {
            // The venue's own declaration (in this family) is authoritative
            // for its markets.
            Some(t) if *t > 0 => *t,
            // No own declaration: the conservative family clock — the earliest
            // observed expiry across same-duration venues, or the family grid
            // when none has spoken.
            _ => earliest_in_family.unwrap_or(grid),
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

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::sync::{Arc, Mutex};

    /// 网格池的准入语义：锚定一个账户的族，绝不关闭同部署里别的族。
    /// 这是 #1272 的根因所在 —— 单张全局网格下，账户 A 写 300s 会让账户 B
    /// 的 900s 市场全部 NoMarkets；池化后 A 的写入只 ADD 自己的族。
    #[test]
    fn anchoring_a_sibling_grid_never_evicts_another_family() {
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        assert!(s.admits_family("polymarket", 900));
        assert!(s.admits_family("polymarket", 300));

        s.anchor_grid("polymarket", 900);
        s.anchor_grid("polymarket", 300);
        assert!(
            s.admits_family("polymarket", 900),
            "the sibling account's 900s family keeps its grid"
        );
        assert!(s.admits_family("polymarket", 300));
        assert!(
            !s.admits_family("polymarket", 60),
            "a family nobody anchored is not enterable"
        );

        // 池以 (venue, duration) 为键：同族在别的 venue 上锚定同样有效（venue 记录谁锚定的，不是谁可以交易）。
        s.set_grids(vec![("kalshi".to_string(), 3600)]);
        assert_eq!(
            s.grid_keys(),
            vec![("kalshi".to_string(), 3600)],
            "set_grids replaces the pool wholesale (the boot path)"
        );
        assert!(s.admits_family("polymarket", 3600));
        assert!(
            !s.admits_family("polymarket", 900),
            "a wholesale boot pool speaks only for what it lists"
        );

        s.anchor_grid("polymarket", 0);
        s.anchor_grid("polymarket", -300);
        assert_eq!(s.grid_keys().len(), 1);
    }

    /// slot 计算一律带键：同一 venue 上，不同族各自按自己的网格取整，共用该 venue 的墙钟偏移（偏移属 venue 而非族）。
    #[test]
    fn slot_at_floors_each_family_on_its_own_grid() {
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 1_000_000i64;
        assert_eq!(s.slot_at("polymarket", 900, now), 1);
        assert_eq!(s.slot_at("polymarket", 300, now), 3);
        assert_eq!(s.slot_at("polymarket", 3600, now), 0);

        // venue 偏移对该 venue 的每个族一致生效（这里 +500 ms 不跨边界）。
        s.observe_end_time_for_venue("polymarket", 1_800_500, now);
        assert_eq!(s.clock_offset_ms.get("polymarket").copied(), Some(500));
        assert_eq!(s.slot_at("polymarket", 900, now), 1);
        assert_eq!(s.slot_at("polymarket", 300, now), 3);
        // 一个从未开口的 venue 没有偏移，按本地钟走。
        assert_eq!(s.slot_at("kalshi", 300, now), 3);
    }

    #[test]
    fn slot_and_round_state_math() {
        let s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 1_000_000i64;
        assert_eq!(s.slot_at(DEFAULT_VENUE, 900, now), 1);
        let r = s.round_state(now);
        assert_eq!(r.expires_at_ms, 1_800_000);
        assert_eq!(r.time_left_sec, 800);
        assert_eq!(r.age_sec, 100);
    }

    #[test]
    fn clock_offset_from_reported_end_time() {
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 1_000_000i64;
        s.observe_end_time(1_800_500, now);
        assert_eq!(s.clock_offset_ms(), 500);
    }

    #[test]
    fn observed_venue_keys_lists_who_spoke_legacy_first_and_deterministically() {
        // #550 症状②：per-venue 轮次行只列「已开口」的 venue，legacy bucket 恒
        // 排最前，其余字典序；未开口的 venue 绝不出现。
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 1_000_000i64;
        assert!(
            s.observed_venue_keys().is_empty(),
            "no declaration → no venue spoke → no rows"
        );
        s.observe_end_time_for_venue("kalshi", 1_800_500, now);
        s.observe_end_time_for_venue("polymarket", 1_799_800, now);
        assert_eq!(
            s.observed_venue_keys(),
            vec!["kalshi".to_string(), "polymarket".to_string()],
            "non-legacy keys are lexicographic"
        );
        // The legacy bucket (empty venue) sorts FIRST when it holds a value.
        s.observe_end_time_for_venue("", 1_800_200, now);
        assert_eq!(
            s.observed_venue_keys(),
            vec![
                String::new(),
                "kalshi".to_string(),
                "polymarket".to_string()
            ],
            "the legacy bucket leads the list"
        );
        s.observe_end_time_for_venue("ghost", now + 10 * 365 * 86_400_000, now);
        assert!(
            !s.observed_venue_keys().contains(&"ghost".to_string()),
            "a refused declaration must not open a clock slot"
        );
    }

    #[test]
    fn out_of_band_end_time_declarations_are_refused() {
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 1_000_000i64;
        let observed = |s: &Scanner| {
            s.actual_end_time_ms
                .get(&(DEFAULT_VENUE.to_string(), 900))
                .copied()
                .unwrap_or(0)
        };
        // A strike-ladder expiry years ahead must not hijack the round clock.
        s.observe_end_time(now + 10 * 365 * 86_400_000, now);
        assert_eq!(observed(&s), 0, "out-of-band declaration refused");
        assert_eq!(s.clock_offset_ms(), 0);
        s.observe_end_time(now - 3 * 900_000, now);
        assert_eq!(observed(&s), 0);
        s.observe_end_time(now + 1_800_000, now);
        assert_eq!(observed(&s), now + 1_800_000);
    }

    #[test]
    fn can_trade_enforces_age_and_time_left() {
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            min_round_age_sec: 30,
            min_time_left_sec: 180,
            ..Default::default()
        });
        assert!(s.can_trade(1_000_000).is_err());
        s.set_markets(vec![CryptoMarket {
            asset: "BTC".into(),
            condition_id: "c".into(),
            question_id: "q".into(),
            up_token_id: "u".into(),
            down_token_id: "d".into(),
            up_price: dec!(0.5),
            down_price: dec!(0.5),
            expires_at_ms: 1_800_000,
            round_slot: 1,
            round_duration_sec: 900,
            archive_verdict: false,
            venue: String::new(),
            neg_risk: true,
            question: "".into(),
        }]);
        // age 100s, time_left 800s → ok
        assert!(s.can_trade(1_000_000).is_ok());
        // Very close to expiry → blocked.
        assert!(s.can_trade(1_799_900).is_err());
    }

    #[test]
    fn the_timing_block_is_structured_and_keeps_the_prose_form() {
        // E2-b (#27): the engine needs the reason as data (to decide whether a
        // strategy may waive it) while every existing caller keeps the string.
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            min_round_age_sec: 30,
            min_time_left_sec: 180,
            ..Default::default()
        });
        assert_eq!(s.can_trade_reason(1_000_000), Err(TimingBlock::NoMarkets));
        assert_eq!(s.can_trade(1_000_000).unwrap_err(), "No active markets");

        s.set_markets(vec![CryptoMarket {
            asset: "BTC".into(),
            condition_id: "c".into(),
            question_id: "q".into(),
            up_token_id: "u".into(),
            down_token_id: "d".into(),
            up_price: dec!(0.5),
            down_price: dec!(0.5),
            expires_at_ms: 1_800_000,
            round_slot: 1,
            round_duration_sec: 900,
            archive_verdict: false,
            venue: String::new(),
            neg_risk: true,
            question: "".into(),
        }]);
        assert_eq!(
            s.can_trade_reason(900_000),
            Err(TimingBlock::TooYoung {
                age_sec: 0,
                min_age_sec: 30
            })
        );
        assert_eq!(
            s.can_trade_reason(1_799_900),
            Err(TimingBlock::TooCloseToExpiry {
                time_left_sec: 0,
                min_time_left_sec: 180
            })
        );
        assert!(s.can_trade_reason(1_000_000).is_ok());
    }

    #[test]
    fn only_the_window_gates_are_exemptible() {
        // Per-strategy declarations may waive window gates; no market at all is
        // a structural precondition — never waivable.
        assert!(!TimingBlock::NoMarkets.exemptible());
        assert!(
            TimingBlock::TooYoung {
                age_sec: 0,
                min_age_sec: 30
            }
            .exemptible()
        );
        assert!(
            TimingBlock::TooCloseToExpiry {
                time_left_sec: 0,
                min_time_left_sec: 180
            }
            .exemptible()
        );
        assert!(
            TimingBlock::TooCloseToExpiry {
                time_left_sec: 120,
                min_time_left_sec: 180
            }
            .to_string()
            .contains("Too close to expiry (120s < 180s)")
        );
    }

    #[test]
    fn cross_family_clocks_get_their_own_slot_and_expiry() {
        // 同一时刻，900s 族与 3600s 族各推各的 slot/expiry：now = 1_000_000
        // 远离两条网格的 slot 边界，两族必须得出不同的 slot 与到期 —
        // slot 网格跟族走，不再全局一根尺。
        let s = Scanner::new(ScannerConfig {
            round_duration_sec: 3600,
            ..Default::default()
        });
        let now = 1_000_000i64;
        let f15 = s.round_timing_for("polymarket", 900, now);
        let f1h = s.round_timing_for("polymarket", 3600, now);
        // 900s: slot 1, expiry 1_800_000。3600s: slot 0, expiry 3_600_000。
        assert_eq!(f15.slot, 1);
        assert_eq!(f15.expires_at_ms, 1_800_000);
        assert_eq!(f15.time_left_sec, 800);
        assert_eq!(f15.age_sec, 100);
        assert_eq!(f1h.slot, 0);
        assert_eq!(f1h.expires_at_ms, 3_600_000);
        assert_eq!(f1h.time_left_sec, 2_600);
        assert_eq!(f1h.age_sec, 1_000);
        assert_ne!(f15.slot, f1h.slot);
        assert_ne!(f15.expires_at_ms, f1h.expires_at_ms);
    }

    #[test]
    fn a_15m_observation_never_pollutes_a_1h_clock() {
        // 族内折叠隔离：kalshi@900 的到期声明落表后，polymarket@3600 的
        // 兜底折叠只扫同 duration 键，15m 的 observed expiry 不得把 1h 的
        // time_left 钉死。
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 1_000_000i64;
        // kalshi 15m declaration, in-band for the 900s family; polymarket 1h
        // too — the 1h family has its own voice.
        s.observe_end_time_for("kalshi", 900, 1_800_500, now);
        s.observe_end_time_for("polymarket", 3600, 3_600_200, now);

        // PM@3600 reads its own declaration (authoritative per family).
        let pm_1h = s.round_timing_for("polymarket", 3600, now);
        assert_eq!(
            pm_1h.expires_at_ms, 3_600_200,
            "own family declaration wins"
        );
        assert_eq!(pm_1h.time_left_sec, 2_600);

        // A 1h venue that has NOT spoken folds ONLY same-duration clocks:
        // the 15m expiry must not appear in its fold.
        let other_1h = s.round_timing_for("kalshi", 3600, now);
        assert_eq!(
            other_1h.expires_at_ms, 3_600_200,
            "the 1h fold sees the only 1h declaration, never the 15m one"
        );

        // The 15m family keeps its own clock despite the 1h presence.
        let k15 = s.round_timing_for("kalshi", 900, now);
        assert_eq!(k15.expires_at_ms, 1_800_500);
        assert_eq!(k15.time_left_sec, 800);
    }
    #[test]
    fn family_fallback_folds_within_the_same_duration_only() {
        // 兜底 earliest_observed 折叠同族：唯一声明在 15m 族 → 1h 兜底走
        // 自己的网格，不是 15m 的 observed。
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 1_000_000i64;
        s.observe_end_time_for("kalshi", 900, 1_800_300, now);
        let silent_1h = s.round_timing_for("polymarket", 3600, now);
        assert_eq!(
            silent_1h.expires_at_ms, 3_600_000,
            "a family with no declarations falls back to ITS OWN grid, never a sibling family's expiry"
        );
        assert_eq!(
            silent_1h.slot, 0,
            "the 1h slot grid runs on the family duration"
        );
        let fold_15 = s.round_timing_for("polymarket", 900, now);
        assert_eq!(
            fold_15.expires_at_ms, 1_800_300,
            "same-family fold reaches the sibling's declaration"
        );
    }

    #[test]
    fn legacy_venue_entry_equals_pre_b1a_semantics() {
        // 关键证明：多 venue 单族观察表下，旧入口与升级前语义一致：slot =
        // 全局网格，折叠 = 全表 `>0` min，own declaration 权威；期望值为
        // 升级前语义的手工演算，不是改动后的输出。
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 900_000i64;
        s.observe_end_time_for_venue("polymarket", 1_800_500, now);
        s.observe_end_time_for_venue("kalshi", 1_799_800, now);
        s.observe_end_time_for_venue("", 1_800_200, now);

        // Own declaration authoritative; offset-derived slot grid unchanged.
        let pm = s.round_timing_for_venue("polymarket", now);
        assert_eq!(pm.slot, 1, "offset-adjusted slot: (900_000 + 500ms) / 900s");
        assert_eq!(pm.expires_at_ms, 1_800_500);
        assert_eq!(
            pm.time_left_sec, 900,
            "(1_800_500 − 900_000)ms / 1000, floored"
        );
        assert_eq!(pm.age_sec, 0, "age = duration − time_left");

        let k = s.round_timing_for_venue("kalshi", now);
        assert_eq!(k.expires_at_ms, 1_799_800);
        assert_eq!(k.time_left_sec, 899);

        // A venue with no voice folds the whole single-family table's min.
        let ghost = s.round_timing_for_venue("ghost", now);
        assert_eq!(
            ghost.expires_at_ms, 1_799_800,
            "single-family fold ≡ the pre-B1a whole-table min"
        );

        // Same values through the new keyed entry — legacy entry IS the
        // (venue, cfg.duration) projection.
        let pm2 = s.round_timing_for("polymarket", 900, now);
        assert_eq!(pm.slot, pm2.slot);
        assert_eq!(pm.expires_at_ms, pm2.expires_at_ms);
        assert_eq!(pm.time_left_sec, pm2.time_left_sec);

        // Global entry (`round_timing`, legacy bucket) byte-untouched:
        // empty-venue clock folds the same single-family table.
        let global = s.round_timing(now);
        assert_eq!(global.slot, 1);
        assert_eq!(global.expires_at_ms, 1_800_200);
        assert_eq!(global.time_left_sec, 900);
    }

    #[test]
    fn observe_end_time_for_writes_family_keys_and_keeps_offset_per_venue() {
        // 写路径：多族声明各落各族键；偏移仍按 venue 一份，不另立族表。
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 1_000_000i64;
        s.observe_end_time_for("polymarket", 900, 1_800_500, now);
        s.observe_end_time_for("polymarket", 3600, 3_600_900, now);

        // The 900s family value lands under its family key; assert via read path (map private).
        assert_eq!(
            s.round_timing_for("polymarket", 900, now).expires_at_ms,
            1_800_500
        );
        assert_eq!(
            s.round_timing_for("polymarket", 3600, now).expires_at_ms,
            3_600_900
        );

        // Offset stays per venue: last declaration refreshes single entry — 3_600_900 − expected 1h end (3_600_000) → offset 900.
        assert_eq!(s.clock_offset_ms(), 900);

        // Guard semantics copied verbatim: non-positive refused.
        s.observe_end_time_for("polymarket", 900, 0, now);
        assert_eq!(
            s.round_timing_for("polymarket", 900, now).expires_at_ms,
            1_800_500,
            "0 refused"
        );
        // Out-of-band refused: ghost's key absent; read falls back to the sibling's same-family clock.
        s.observe_end_time_for("ghost", 900, now + 10 * 365 * 86_400_000, now);
        assert_eq!(
            s.round_timing_for("ghost", 900, now).expires_at_ms,
            1_800_500,
            "the refused declaration leaves no ghost key"
        );
        // A non-positive duration is malformed — ignored wholesale.
        s.observe_end_time_for("polymarket", 0, 1_800_500, now);
        assert!(
            !s.observed_clock_keys()
                .iter()
                .any(|(v, d)| v == "polymarket" && *d == 0),
            "duration ≤ 0 never opens a family key"
        );
    }

    #[test]
    fn observed_clock_keys_dedupe_and_order_deterministically() {
        // 去重保序：legacy venue 恒最前，其余 (venue,duration) 字典序，同 (venue,duration) 去重，未开口的 venue/族绝不出现。
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        assert!(
            s.observed_clock_keys().is_empty(),
            "no declaration → no rows"
        );
        let now = 1_000_000i64;
        s.observe_end_time_for("polymarket", 3600, 3_600_500, now);
        s.observe_end_time_for("kalshi", 900, 1_800_500, now);
        s.observe_end_time_for("kalshi", 3600, 3_600_200, now);
        s.observe_end_time_for("kalshi", 900, 1_800_600, now);
        s.observe_end_time_for("", 900, 1_800_200, now);
        assert_eq!(
            s.observed_clock_keys(),
            vec![
                (String::new(), 900),
                ("kalshi".to_string(), 900),
                ("kalshi".to_string(), 3600),
                ("polymarket".to_string(), 3600),
            ],
            "legacy first, then (venue, duration) lexicographic, deduped"
        );
        // A refused declaration never opens a key.
        s.observe_end_time_for("ghost", 900, now + 10 * 365 * 86_400_000, now);
        assert!(!s.observed_clock_keys().iter().any(|(v, _)| v == "ghost"));
        assert_eq!(
            s.observed_venue_keys(),
            vec![
                String::new(),
                "kalshi".to_string(),
                "polymarket".to_string()
            ],
            "unique venues in the clock-key order"
        );
    }

    #[test]
    fn out_of_band_family_declarations_are_refused_per_family_band() {
        // 守卫带按族轮长算：3600s 族 ±2 轮 = ±7200s，900s 族拒收 30 分钟外声明，30 分钟外在 1h 族 in-band。
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 1_000_000i64;
        s.observe_end_time_for("kalshi", 900, now + 3_600_000, now);
        assert!(
            !s.observed_clock_keys()
                .contains(&("kalshi".to_string(), 900)),
            "outside the 15m family's ±2-round band"
        );
        s.observe_end_time_for("kalshi", 3600, now + 3_600_000, now);
        assert_eq!(
            s.round_timing_for("kalshi", 3600, now).expires_at_ms,
            now + 3_600_000,
            "the same declaration is inside the 1h family's band"
        );
    }

    /// Local row fixture: 900s-declared BTC row expiring at `end_ms` on `venue`.
    fn gate_market(end_ms: i64, venue: &str) -> CryptoMarket {
        CryptoMarket {
            asset: "BTC".into(),
            condition_id: format!("cond-{venue}"),
            question_id: format!("q-{venue}"),
            up_token_id: format!("{venue}-up"),
            down_token_id: format!("{venue}-down"),
            up_price: rust_decimal_macros::dec!(0.6),
            down_price: rust_decimal_macros::dec!(0.4),
            expires_at_ms: end_ms,
            round_slot: end_ms / 1000 / 900,
            round_duration_sec: 900,
            archive_verdict: false,
            venue: venue.into(),
            neg_risk: true,
            question: "BTC up or down".into(),
        }
    }

    /// An 86400s market's declaration lands in its (venue, 86400) family clock
    /// — pre-E2's keyed entry could never consult it, so `can_trade_reason_for`
    /// mis-timed every verdict; the gate reads the row's OWN family.
    #[test]
    fn entry_timing_gate_judges_a_daily_market_on_its_own_family_clock() {
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            min_round_age_sec: 0,
            min_time_left_sec: 180,
            ..Default::default()
        });
        let now = 1_000_000i64; // 900s grid: 100s into slot 1 (800s left).
        // The daily market's declaration, in-band for the 86400s family: the
        // round ends one day after the grid slot now sits in.
        let daily_end = (now / 1000 / 86_400 + 1) * 86_400 * 1000;
        s.observe_end_time_for("polymarket", 86_400, daily_end, now);
        let mut daily = gate_market(daily_end, "polymarket");
        daily.round_duration_sec = 86_400;
        daily.round_slot = daily_end / 1000 / 86_400;
        s.set_markets(vec![daily.clone()]);

        // Pre-E2 the gate read (polymarket, 900): the earliest 900s-grid
        // fallback — nothing had spoken there — pinned time_left to the 900s
        // grid's phase (800s), while the OWN family shows ~86_300s. The
        // family clock is what trades.
        let t = s.can_trade_reason_for(&daily, now);
        assert!(
            t.is_ok(),
            "a daily market inside its own family's active window trades: {t:?}"
        );
        assert_eq!(
            s.round_timing_for("polymarket", 86_400, now).time_left_sec,
            (daily_end - now) / 1000,
            "the family clock carries the daily declaration, not the 900s grid"
        );

        // Gate stays honest within the family: shift the declaration to end
        // 100s out (100s < 100_000ms band) and the same row is refused.
        let mut s2 = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            min_round_age_sec: 0,
            min_time_left_sec: 180,
            ..Default::default()
        });
        let near_end = now + 100_000;
        s2.observe_end_time_for("polymarket", 86_400, near_end, now);
        let mut hot = gate_market(near_end, "polymarket");
        hot.round_duration_sec = 86_400;
        hot.round_slot = near_end / 1000 / 86_400;
        s2.set_markets(vec![hot.clone()]);
        assert_eq!(
            s2.can_trade_reason_for(&hot, now),
            Err(TimingBlock::TooCloseToExpiry {
                time_left_sec: 100,
                min_time_left_sec: 180
            }),
            "inside the family's force-exit window the gate still blocks"
        );
    }

    /// Single-family legacy path is byte-identical: a row declaring 0 (or the
    /// configured 900s) answers to the (venue, cfg) family — the exact clock
    /// `round_timing_for_venue` folded to before E2 — every pre-E2 verdict
    /// (including sibling-venue per-market refinement) reproduces bit for bit.
    #[test]
    fn entry_timing_gate_legacy_rows_still_answer_to_the_global_family() {
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            min_round_age_sec: 0,
            min_time_left_sec: 180,
            ..Default::default()
        });
        let poly_end = 1_800_000i64;
        let kalshi_end = 1_950_000i64;
        s.observe_end_time_for_venue("polymarket", poly_end, 900_000);
        s.observe_end_time_for_venue("kalshi", kalshi_end, 900_000);
        let poly = gate_market(poly_end, "polymarket");
        let kalshi = gate_market(kalshi_end, "kalshi");
        s.set_markets(vec![poly.clone(), kalshi.clone()]);

        // Pre-E2 per-venue verdict, verbatim: at 1_630_000 polymarket close is
        // 170s out (< 180 → blocked) and kalshi's 320s out (ok) — the exact
        // numbers the pre-E2 test pinned.
        let now = 1_630_000i64;
        assert_eq!(
            s.can_trade_reason_for(&poly, now),
            Err(TimingBlock::TooCloseToExpiry {
                time_left_sec: 170,
                min_time_left_sec: 180
            }),
            "the polymarket row keeps its own-window refusal"
        );
        assert!(
            s.can_trade_reason_for(&kalshi, now).is_ok(),
            "the kalshi row keeps its own-window admission"
        );

        // A zero-declared row (legacy archive) folds to the cfg grid — the
        // same verdict the pre-E2 read produced.
        let mut ghost = gate_market(poly_end, "ghost");
        ghost.round_duration_sec = 0;
        assert_eq!(
            s.can_trade_reason_for(&ghost, now),
            Err(TimingBlock::TooCloseToExpiry {
                time_left_sec: 170,
                min_time_left_sec: 180
            }),
            "undeclared rows ride the configured grid, exactly as before"
        );
    }

    /// #1272: admission authority is the grid pool (lookup table), not "the
    /// one current grid". Pins: after account A anchors 300s, account B's 900s
    /// market still enters — cross-account bleed fixed (a single global grid
    /// would return `NoMarkets`, since write is per-account while the grid
    /// was global). Also pins no-write behavior: no grids anchored → a
    /// 300s-declared market trades its own clock even when the default is 900s.
    #[test]
    fn pool_membership_is_the_admission_authority() {
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            min_round_age_sec: 0,
            min_time_left_sec: 0,
            ..Default::default()
        });
        let now = 1_000_000i64;
        s.set_markets(vec![gate_market(1_800_000, "polymarket")]);

        // Nothing anchored: historic posture, a declared market rides its
        // own clock.
        let mut m5 = gate_market(1_800_000, "polymarket");
        m5.round_duration_sec = 300;
        assert!(
            s.can_trade_reason_for(&m5, now).is_ok(),
            "nothing anchored: a declared market trades its own clock"
        );

        // Boot backfill (process default 900s) + account A's per-account
        // write anchors 300s.
        s.set_grids(vec![
            ("polymarket".to_string(), 900),
            ("polymarket".to_string(), 300),
        ]);

        // Account B's 900s market: pool has 900 → enters. #1272 regression.
        let mut m15 = gate_market(1_800_000, "polymarket");
        m15.round_duration_sec = 900;
        assert!(
            s.can_trade_reason_for(&m15, now).is_ok(),
            "a sibling account's 900s market keeps entering after A anchors 300s"
        );
        // Account A's 300s market: pool has 300 → enters.
        assert!(
            s.can_trade_reason_for(&m5, now).is_ok(),
            "the anchoring account's own family enters"
        );
        // A family nobody anchors → structural NoMarkets (not exemptible).
        let mut m1h = gate_market(3_600_000, "polymarket");
        m1h.round_duration_sec = 3600;
        assert_eq!(
            s.can_trade_reason_for(&m1h, now),
            Err(TimingBlock::NoMarkets),
            "a family nobody anchors is not enterable"
        );
        // Undeclared rows ride the cfg grid (900), pool has it → enters.
        let mut m0 = gate_market(1_800_000, "polymarket");
        m0.round_duration_sec = 0;
        assert!(
            s.can_trade_reason_for(&m0, now).is_ok(),
            "undeclared rows ride the configured grid"
        );

        // #1165 L4: pool is admission-only, not a read-view filter —
        // in-flight rounds stay visible. 5m family slot at now=1000s is 3,
        // so round_slot 4 is in-flight.
        let mut m5_inflight = gate_market(1_350_000, "polymarket");
        m5_inflight.round_duration_sec = 300;
        m5_inflight.round_slot = 4;
        assert!(
            !s.is_live_round_row(&m5, now),
            "a 5m row whose family round already settled is not live"
        );
        assert!(
            s.is_live_round_row(&m5_inflight, now),
            "the in-flight 5m round stays visible until natural settlement"
        );
        assert!(
            s.is_live_round_row(&m15, now),
            "the 900s row is live on the 900s grid"
        );
    }

    /// 拒绝日志去重口径：一个 `(venue, 族时长)` 只 INFO 一次（重复是 DEBUG），
    /// 换 venue 或换族再 INFO 一次。去重键是「族」而非「token」。
    #[test]
    fn off_pool_refusal_info_is_once_per_venue_and_family() {
        use tracing::Level;
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::registry;

        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 300,
            min_round_age_sec: 0,
            min_time_left_sec: 0,
            ..Default::default()
        });
        s.anchor_grid("polymarket", 300);
        let now = 1_000_000i64;
        let mut sol = gate_market(1_800_000, "polymarket");
        sol.asset = "SOL".into();
        sol.up_token_id = "sol-up".into();
        sol.down_token_id = "sol-down".into();
        sol.round_duration_sec = 900;
        s.set_markets(vec![sol.clone()]);

        let (capture, got) = OffPoolLogCapture::new();
        tracing::subscriber::with_default(registry().with(capture), || {
            for _ in 0..3 {
                assert_eq!(
                    s.can_trade_reason_for(&sol, now),
                    Err(TimingBlock::NoMarkets),
                    "an unanchored family stays refused on every tick"
                );
            }
        });
        let levels = got.lock().expect("capture").clone();
        assert_eq!(
            levels.iter().filter(|l| **l == Level::INFO).count(),
            1,
            "INFO once for the repeated (venue, family): {levels:?}"
        );
        assert_eq!(
            levels.iter().filter(|l| **l == Level::DEBUG).count(),
            2,
            "the two repeats are debug, not info: {levels:?}"
        );
        assert_eq!(
            s.off_pool_refusal_logged.lock().expect("set").len(),
            1,
            "the dedupe set grows once for the repeated family"
        );

        let mut eth = sol.clone();
        eth.asset = "ETH".into();
        eth.up_token_id = "eth-up".into();
        let (capture, got) = OffPoolLogCapture::new();
        tracing::subscriber::with_default(registry().with(capture), || {
            assert_eq!(
                s.can_trade_reason_for(&eth, now),
                Err(TimingBlock::NoMarkets)
            );
        });
        let levels = got.lock().expect("capture").clone();
        assert_eq!(
            levels.iter().filter(|l| **l == Level::INFO).count(),
            0,
            "the same (venue, family) does not log INFO again: {levels:?}"
        );
        assert_eq!(s.off_pool_refusal_logged.lock().expect("set").len(), 1);

        let mut kalshi = sol.clone();
        kalshi.venue = "kalshi".into();
        let (capture, got) = OffPoolLogCapture::new();
        tracing::subscriber::with_default(registry().with(capture), || {
            assert_eq!(
                s.can_trade_reason_for(&kalshi, now),
                Err(TimingBlock::NoMarkets)
            );
        });
        let levels = got.lock().expect("capture").clone();
        assert_eq!(
            levels.iter().filter(|l| **l == Level::INFO).count(),
            1,
            "a sibling venue's same family logs INFO once: {levels:?}"
        );
        assert_eq!(s.off_pool_refusal_logged.lock().expect("set").len(), 2);

        s.anchor_grid("polymarket", 900);
        assert!(
            s.can_trade_reason_for(&sol, now).is_ok(),
            "an anchored family is not a refusal"
        );
        assert_eq!(
            s.off_pool_refusal_logged.lock().expect("set").len(),
            2,
            "an admitted row does not join the dedupe set"
        );
    }

    /// `target = "engine"` events for the grid-pool refusal line.
    #[derive(Clone)]
    struct OffPoolLogCapture(Arc<Mutex<Vec<tracing::Level>>>);

    impl OffPoolLogCapture {
        fn new() -> (Self, Arc<Mutex<Vec<tracing::Level>>>) {
            let buf = Arc::new(Mutex::new(Vec::new()));
            (Self(buf.clone()), buf)
        }

        #[allow(dead_code)]
        fn levels(&self) -> Vec<tracing::Level> {
            self.0.lock().expect("capture").clone()
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for OffPoolLogCapture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() != "engine" {
                return;
            }
            let mut msg = String::new();
            event.record(&mut OffPoolMessage(&mut msg));
            if msg.contains("round grid pool") {
                self.0
                    .lock()
                    .expect("capture")
                    .push(*event.metadata().level());
            }
        }
    }

    struct OffPoolMessage<'a>(&'a mut String);

    impl tracing::field::Visit for OffPoolMessage<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0.push_str(&format!("{value:?}"));
            }
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                self.0.push_str(value);
            }
        }
    }
}
