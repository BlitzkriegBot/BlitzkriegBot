//! late_favorite acceptance checks — the REAL package (`user_layer/strategies_lua/
//! late_favorite`) driven through `LuaEngineAdapter`.

use std::path::PathBuf;

use blitzkrieg_core::strategies::{EngineStrategy, HeldPosition, StrategyCtx};
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use blitzkrieg_core::{kline::Kline, model::CryptoMarket, model::OrderbookSnapshot};
use blitzkrieg_market_api::kline::KlineInterval;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

// Aligned to a 300s boundary: 1_700_000_100_000 (1_700_000_100 % 300 == 0)
const ROUND_START_MS: i64 = 1_700_000_100_000;
const EXPIRES_MS: i64 = ROUND_START_MS + 300_000;
const SLOT: i64 = ROUND_START_MS / 300_000;
const UP: &str = "up-tok";
const DOWN: &str = "down-tok";

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../user_layer/strategies_lua/late_favorite")
}

fn adapter() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("late_favorite package must load");
    LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
}

fn market(asset: &str) -> CryptoMarket {
    CryptoMarket {
        asset: asset.into(),
        condition_id: "cond-1".into(),
        question_id: "q-1".into(),
        up_token_id: UP.into(),
        down_token_id: DOWN.into(),
        up_price: dec!(0.84),
        down_price: dec!(0.16),
        expires_at_ms: EXPIRES_MS,
        round_slot: SLOT,
        round_duration_sec: 300,
        archive_verdict: false,
        venue: "polymarket".into(),
        neg_risk: false,
        question: "BTC up or down".into(),
    }
}

fn token_book(token: &str, bid: Decimal, ask: Decimal, ts: i64) -> OrderbookSnapshot {
    OrderbookSnapshot::from_levels(
        token.to_string(),
        vec![(bid, dec!(50))],
        vec![(ask, dec!(50))],
        ts,
    )
}

fn feed_sec1_series(adapter: &mut LuaEngineAdapter, start_open_ms: i64, prices: &[f64]) {
    for (i, &px) in prices.iter().enumerate() {
        let open_ms = start_open_ms + (i as i64) * 1000;
        let dec_px = Decimal::from_str_exact(&format!("{px:.2}")).expect("decimal price");
        let k = Kline {
            symbol: "BTC".into(),
            interval: KlineInterval::Sec1,
            open_time_ms: open_ms,
            close_time_ms: open_ms + 999,
            open: dec_px,
            high: dec_px,
            low: dec_px,
            close: dec_px,
            volume: dec!(1),
            trade_count: 1,
            is_closed: true,
        };
        adapter.on_kline(&k);
    }
}

/// Build 900s of sec1 prices ending at ROUND_START_MS + 240s (60s left):
/// 660s pre-round oscillating slightly (~$1.5/step => realistic ~1.5e-5/sqrt(s) vol),
/// then 240s inside the round.
fn warmup_prices() -> Vec<f64> {
    let mut prices = Vec::with_capacity(900);
    let mut px = 90000.0_f64;
    for i in 0..660 {
        px += if i % 2 == 0 { 2.0 } else { -2.0 };
        prices.push(px);
    }
    // 240s in-round (indices 660..900) starting at ROUND_START_MS
    for i in 0..240 {
        if i < 200 {
            px += if i % 2 == 0 { 2.0 } else { -2.0 };
        } else {
            // +$4/s rally over 40s -> +$160 above open (~+0.18%, ~4.5 sigma at 60s left)
            px += 4.0;
        }
        prices.push(px);
    }
    prices
}

#[test]
fn positive_bs_edge_and_momentum_enters_once_and_does_not_reenter() {
    let mut a = adapter();
    let prices = warmup_prices();
    feed_sec1_series(&mut a, ROUND_START_MS - 660_000, &prices);

    a.on_round(SLOT, 299, ROUND_START_MS + 1000);
    let now_ms = EXPIRES_MS - 60_000;
    let m = vec![market("BTC")];
    let books = |tok: &str| -> Option<OrderbookSnapshot> {
        match tok {
            UP => Some(token_book(UP, dec!(0.83), dec!(0.84), now_ms)),
            DOWN => Some(token_book(DOWN, dec!(0.15), dec!(0.17), now_ms)),
            _ => None,
        }
    };
    let c1 = a.find_candidates(&StrategyCtx::new(&m, SLOT, 60, now_ms, &books));
    assert_eq!(c1.len(), 1, "should enter positive-edge favorite once");
    assert_eq!(c1[0].token_id, UP);
    assert_eq!(c1[0].price, dec!(0.84));
    assert_eq!(c1[0].reason, "late-taker");

    // Second tick in same round must not re-enter
    let c2 = a.find_candidates(&StrategyCtx::new(&m, SLOT, 55, now_ms + 5000, &books));
    assert!(
        c2.is_empty(),
        "must not re-enter same condition in same round"
    );
}

#[test]
fn negative_low_edge_waits_then_enters_when_spot_separates() {
    let mut a = adapter();
    // Flat around 90000: at t=60s left, spot_now == spot_open (fair = 0.50 < 0.84)
    let mut prices = Vec::new();
    let mut px = 90000.0_f64;
    for i in 0..840 {
        px += if i % 2 == 0 { 2.0 } else { -2.0 };
        prices.push(px);
    }
    feed_sec1_series(&mut a, ROUND_START_MS - 660_000, &prices);
    a.on_round(SLOT, 299, ROUND_START_MS + 1000);

    let now_60 = ROUND_START_MS + 180_000;
    let m = vec![market("BTC")];
    let books_60 = |tok: &str| -> Option<OrderbookSnapshot> {
        match tok {
            UP => Some(token_book(UP, dec!(0.83), dec!(0.84), now_60)),
            DOWN => Some(token_book(DOWN, dec!(0.15), dec!(0.17), now_60)),
            _ => None,
        }
    };
    let c_wait = a.find_candidates(&StrategyCtx::new(&m, SLOT, 120, now_60, &books_60));
    assert!(c_wait.is_empty(), "zero-edge tick must stay out");

    // Now spot rallies sharply over the next 60s (from 180s to 240s)
    let mut rally = Vec::new();
    for _ in 0..60 {
        px += 3.0;
        rally.push(px);
    }
    feed_sec1_series(&mut a, ROUND_START_MS + 180_000, &rally);
    let now_240 = EXPIRES_MS - 60_000;
    let books_240 = |tok: &str| -> Option<OrderbookSnapshot> {
        match tok {
            UP => Some(token_book(UP, dec!(0.83), dec!(0.84), now_240)),
            DOWN => Some(token_book(DOWN, dec!(0.15), dec!(0.17), now_240)),
            _ => None,
        }
    };
    let c_enter = a.find_candidates(&StrategyCtx::new(&m, SLOT, 60, now_240, &books_240));
    assert_eq!(
        c_enter.len(),
        1,
        "should enter once BS edge + momentum clear later in same round"
    );
}

#[test]
fn negative_adverse_15s_momentum_and_stale_spot_and_partial_round_and_btc_only() {
    // 1. Adverse 15s momentum (spot rallied early, then dipped over last 15s)
    let mut a = adapter();
    let mut prices = Vec::new();
    let mut px = 90000.0_f64;
    for i in 0..660 {
        px += if i % 2 == 0 { 2.0 } else { -2.0 };
        prices.push(px);
    }
    for i in 0..240 {
        if i < 225 {
            px += 1.0; // up +225
        } else {
            px -= 1.0; // -15 pullback over last 15s (still +210 above open!)
        }
        prices.push(px);
    }
    feed_sec1_series(&mut a, ROUND_START_MS - 660_000, &prices);
    a.on_round(SLOT, 299, ROUND_START_MS + 1000);
    let now_ms = EXPIRES_MS - 60_000;
    let m_btc = vec![market("BTC")];
    let books = |tok: &str| -> Option<OrderbookSnapshot> {
        match tok {
            UP => Some(token_book(UP, dec!(0.83), dec!(0.84), now_ms)),
            DOWN => Some(token_book(DOWN, dec!(0.15), dec!(0.17), now_ms)),
            _ => None,
        }
    };
    assert!(
        a.find_candidates(&StrategyCtx::new(&m_btc, SLOT, 60, now_ms, &books))
            .is_empty(),
        "adverse 15s spot momentum must veto UP entry"
    );

    // 2. Stale spot (> 5s old) must fail closed
    let mut a2 = adapter();
    let warm = warmup_prices();
    feed_sec1_series(&mut a2, ROUND_START_MS - 660_000, &warm);
    a2.on_round(SLOT, 299, ROUND_START_MS + 1000);
    assert!(
        a2.find_candidates(&StrategyCtx::new(&m_btc, SLOT, 50, now_ms + 10_000, &books))
            .is_empty(),
        "stale spot feed > 5s must fail closed"
    );

    // 3. Partial startup round (max_tl never >= 240) must fail closed
    let mut a3 = adapter();
    feed_sec1_series(&mut a3, ROUND_START_MS - 660_000, &warm);
    a3.on_round(SLOT, 100, EXPIRES_MS - 100_000);
    assert!(
        a3.find_candidates(&StrategyCtx::new(&m_btc, SLOT, 60, now_ms, &books))
            .is_empty(),
        "partial round joined mid-round must fail closed"
    );

    // 4. btc_only default filters non-BTC even if BTC spot was fed
    let m_eth = vec![market("ETH")];
    assert!(
        a2.find_candidates(&StrategyCtx::new(&m_eth, SLOT, 60, now_ms, &books))
            .is_empty(),
        "btc_only=1 default must ignore ETH"
    );

    // 5. Thesis death stays disabled (holds_to_settlement = true)
    let held = vec![HeldPosition {
        strategy: "late_favorite".into(),
        account_id: blitzkrieg_core::model::default_account_id().to_string(),
        condition_id: "cond-1".into(),
        token_id: UP.into(),
        direction: "up".into(),
        shares: "10".into(),
        entry_price: "0.84".into(),
        round_slot: SLOT,
    }];
    let exit_ctx =
        StrategyCtx::new(&m_btc, SLOT, 30, now_ms + 30_000, &books).with_positions(&held);
    let _ = a2.find_candidates(&exit_ctx);
    assert!(
        a2.take_exit_intents().is_empty(),
        "bk_thesis_death must remain false"
    );
}

#[test]
fn price_cap_rejects_above_086_and_accepts_075() {
    let mut a = adapter();
    let prices = warmup_prices();
    feed_sec1_series(&mut a, ROUND_START_MS - 660_000, &prices);
    a.on_round(SLOT, 299, ROUND_START_MS + 1000);
    let now_ms = EXPIRES_MS - 60_000;
    let m = vec![market("BTC")];
    let too_high = |tok: &str| -> Option<OrderbookSnapshot> {
        match tok {
            UP => Some(token_book(UP, dec!(0.87), dec!(0.88), now_ms)),
            DOWN => Some(token_book(DOWN, dec!(0.11), dec!(0.13), now_ms)),
            _ => None,
        }
    };
    assert!(
        a.find_candidates(&StrategyCtx::new(&m, SLOT, 60, now_ms, &too_high))
            .is_empty(),
        "0.88 ask exceeds default 0.86 max_px cap"
    );
    let in_band = |tok: &str| -> Option<OrderbookSnapshot> {
        match tok {
            UP => Some(token_book(UP, dec!(0.74), dec!(0.76), now_ms)),
            DOWN => Some(token_book(DOWN, dec!(0.23), dec!(0.25), now_ms)),
            _ => None,
        }
    };
    // now_ms + 3 s is 57 s left — pass the same number the kernel's
    // floor((expires-now)/1000) would, so freshness (end_sec - time_left)
    // agrees with the wall clock the books were stamped with.
    let c = a.find_candidates(&StrategyCtx::new(&m, SLOT, 57, now_ms + 3000, &in_band));
    assert_eq!(
        c.len(),
        1,
        "0.76 ask inside 0.75-0.86 with high BS edge must enter"
    );
    assert_eq!(c[0].price, dec!(0.76));
}
