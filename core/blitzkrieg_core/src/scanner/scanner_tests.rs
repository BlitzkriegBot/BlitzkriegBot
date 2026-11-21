//! Scanner unit tests. Moved verbatim from `scanner.rs` `mod tests` (pure code motion).
use crate::model::CryptoMarket;
use crate::scanner::scanner_types::*;
use crate::scanner::*;
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

/// After a 5m roll, the previous window's close equals the new start.
/// Keeping that stored end would report time_left=0 / age=300 on the new
/// slot (the panel shows the current venue ts with a frozen countdown).
/// The clock must fall back to the new grid until a fresh observation.
#[test]
fn stale_end_at_previous_close_does_not_zero_the_new_slot() {
    let mut s = Scanner::new(ScannerConfig {
        round_duration_sec: 300,
        ..Default::default()
    });
    // Slot 5984120: 11:40–11:45. Observe its close.
    let prev_end = 1_795_236_300_000i64;
    let during_prev = prev_end - 60_000;
    s.observe_end_time_for("polymarket", 300, prev_end, during_prev);
    let mid = s.round_timing_for("polymarket", 300, during_prev);
    assert_eq!(
        mid.time_left_sec, 60,
        "live window still uses the observation"
    );
    assert_eq!(mid.expires_at_ms, prev_end);

    // 40s into the next 5m window (11:45–11:50); no new observation yet
    // (discovery was soft-unloaded, or the first post-roll poll missed).
    let into_next = prev_end + 40_000;
    let rolled = s.round_timing_for("polymarket", 300, into_next);
    assert_eq!(
        rolled.slot,
        into_next / 1000 / 300,
        "wall-clock slot must already be the new window"
    );
    assert_eq!(
        rolled.expires_at_ms,
        (rolled.slot + 1) * 300 * 1000,
        "stale previous close must not remain the family end"
    );
    assert_eq!(rolled.time_left_sec, 260, "countdown must keep rolling");
    assert_eq!(rolled.age_sec, 40);
}

/// A Polymarket daily close at America/New_York noon is not UTC-aligned.
/// While that end is still in the future it must be kept, even though it
/// does not equal the UTC 86400 grid.
#[test]
fn daily_et_noon_end_is_kept_while_still_open() {
    let mut s = Scanner::new(ScannerConfig {
        round_duration_sec: 86_400,
        ..Default::default()
    });
    // 2026-11-20 12:00 EST = 17:00 UTC = 1795194000; close is 2026-11-21
    // 12:00 EST = 1795280400. Mid-window (the live 5m-stuck report's now).
    let end = 1_795_280_400_000i64;
    let now = 1_795_236_160_000i64;
    s.observe_end_time_for("polymarket", 86_400, end, now);
    let t = s.round_timing_for("polymarket", 86_400, now);
    assert_eq!(
        t.expires_at_ms, end,
        "ET-noon close must not be replaced by the UTC midnight grid"
    );
    assert!(
        t.time_left_sec > 0,
        "daily countdown must remain open: {t:?}"
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
