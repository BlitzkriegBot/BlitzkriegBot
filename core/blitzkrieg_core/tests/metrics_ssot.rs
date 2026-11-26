//! issue #181 — 指标口径单一事实源的证伪测试。
//!
//! 缺陷：同一个「胜率」在三个出口各有实现，且单位分叉（wire 上同时是
//! 0–100 的百分数与 0–1 的小数）：
//!
//! 1. 成交账本的折叠（当年是 `trade_db::fold_into` → `TradeSummary.winRate`）：
//!    百分数，flat 不进分母；
//! 2. `reports::trade_summary`（无 db / per-account 折叠路径）：百分数，
//!    flat 不进分母——但这是**第二份**实现，没有任何东西保证两边同步；
//! 3. `backtest` 的 `TradeStats.winRatePct`：百分数，**flat 进分母**
//!    （`trades.len()`）——同一批成交在这里报出的胜率比成交口径低。
//!
//! 修法：`metrics::Tally` 是唯一公式载体，三个出口都从它取数。本文件的断言
//! 之所以能「证伪」，是因为它对**同一组 100 笔（含 30 笔平手）**分别走三个
//! 出口，要求三个出口的胜率**逐位相等**（`to_bits()`）。任何一处回头自己
//! 实现公式、或把 flat 重新算进分母，这里立刻红。
//!
//! 出口 1 的载体自 schema v4 起是 SQL（`db::trades::TradeLog` 的聚合），
//! 不再是内存里折叠一个 JSONL 文件；被钉住的东西没变：那仍是「账本自己
//! 报出的胜率」，而出口 2 必须与它逐位一致。
//!
//! 编译门：本文件只用公开 API（`db::trades::TradeLog` / `Core::trade_summary`
//! / `TradeStats::of_trades`），不需要 `test-support` 特性——它是「操作者部署
//! 面」的口径断言，不是内部接缝的测试。

use blitzkrieg_core::backtest::{TradeLine, TradeStats};
use blitzkrieg_core::contracts::trades::{TradeRead, TradeScope};
use blitzkrieg_core::db::Database;
use blitzkrieg_core::db::trades::TradeLog;
use blitzkrieg_core::metrics::{Outcome, Tally};
use blitzkrieg_core::model::{ExitReason, Mode, OrderRole, SignalDirection};
use blitzkrieg_core::position::ClosedPosition;
use blitzkrieg_core::service::{Core, CoreConfig};
use blitzkrieg_core::trade::TradeRecord;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// 100 笔的固定形状：40 胜 / 30 平 / 30 负。
///
/// 40/30/30 是刻意的：分母错误（含 flat）会得到 40/100 = 0.4，正确口径是
/// 40/70 ≈ 0.5714…——两个数差得足够远，任何「差一点点」的实现在这里都过不去。
const WINS: usize = 40;
const FLATS: usize = 30;
const LOSSES: usize = 30;

/// The expected fraction, computed from the same integers the three exits see.
/// Not a hand-written literal: a typo'd constant would otherwise become the
/// thing the test pins.
fn expected_fraction() -> f64 {
    WINS as f64 / (WINS + LOSSES) as f64
}

// ── fixtures ────────────────────────────────────────────────────────────────

fn tmp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("bk-181-{tag}-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// One closed position with a chosen net PnL. Everything else is fixed so the
/// three exits see the same economics — only the sign of `net_pnl_usd` varies.
fn closed_with_net(id: &str, net: Decimal) -> ClosedPosition {
    ClosedPosition {
        id: id.to_string(),
        strategy: "ssot".into(),
        asset: "BTC".into(),
        direction: SignalDirection::Up,
        token_id: "tok".into(),
        condition_id: "cond".into(),
        entry_price: dec!(0.40),
        exit_price: dec!(0.60),
        shares: dec!(10),
        cost_usd: dec!(4),
        was_maker_entry: true,
        was_maker_exit: false,
        entry_fee_pct: Decimal::ZERO,
        exit_fee_pct: dec!(0.5),
        maker_rebate_usd: Decimal::ZERO,
        pnl_usd: dec!(2),
        pnl_pct: dec!(50),
        net_pnl_usd: net,
        net_pnl_pct: dec!(49.25),
        high_pnl_pct: dec!(52),
        low_pnl_pct: dec!(-3),
        hold_time_sec: 64,
        exit_reason: ExitReason::TakeProfit,
        entered_at_ms: 1_000,
        exited_at_ms: 65_000,
        entry_role: OrderRole::Maker,
        exit_role: OrderRole::Taker,
        dust_shares: Decimal::ZERO,
        account_id: blitzkrieg_core::model::default_account_id(),
        venue: String::new(),
        source: String::new(),
        original_fallback_price: None,
        price_source: String::new(),
    }
}

/// The 100-trade corpus every exit is fed: 40 wins (+1), 30 flats (0), 30
/// losses (−1), interleaved so no exit can accidentally benefit from ordering.
fn corpus() -> Vec<ClosedPosition> {
    let mut out = Vec::with_capacity(WINS + FLATS + LOSSES);
    for i in 0..(WINS + FLATS + LOSSES) {
        // Round-robin through the three buckets instead of three blocks — a
        // fold that truncates early would show it.
        let net = match i % 10 {
            0..=3 => dec!(1),
            4..=6 => Decimal::ZERO,
            _ => dec!(-1),
        };
        out.push(closed_with_net(&format!("ssot-{i}"), net));
    }
    out
}

// ── 断言 1 / 2：公式本体 ────────────────────────────────────────────────────

/// 规格断言一：`Tally { wins: 3, losses: 2, flats: 5 }.win_rate() == Some(0.6)`。
/// 平手不进分母——5 笔平手若进分母会得到 0.3。
#[test]
fn tally_win_rate_excludes_flats_from_the_denominator() {
    let t = Tally {
        wins: 3,
        losses: 2,
        flats: 5,
    };
    assert_eq!(t.win_rate(), Some(0.6));
    assert_eq!(t.decided(), 5, "已决 = 胜 + 负，平手不计");
    assert_eq!(t.total(), 10, "总数含平手");
}

/// 规格断言二：`Tally { wins: 0, losses: 0, flats: 7 }.win_rate() == None`。
/// 没有任何分出胜负的笔是「未知」，不是 0%——否则「还没交易」与「全输」
/// 在面板上不可分。
#[test]
fn tally_win_rate_is_none_when_nothing_was_decided() {
    let t = Tally {
        wins: 0,
        losses: 0,
        flats: 7,
    };
    assert_eq!(t.win_rate(), None);
    assert_eq!(Tally::default().win_rate(), None);
}

// ── 断言 3：三个出口逐位相等 ────────────────────────────────────────────────

/// 规格断言三：同一组 100 笔（含 40 flat）→ `trade_db`、`trade_summary`、
/// `backtest` 三个出口的胜率**逐位相等**。
///
/// 逐位比较用 `f64::to_bits()`，不是 `abs() < eps`：容差比较会放过「两个
/// 略有不同的实现」——而这正是本 issue 要钉死的东西（`40/70` 只有一条
/// `wins as f64 / decided as f64` 表达式时，三个出口必然给出同一个位型）。
#[test]
fn three_exits_report_the_same_win_rate_bit_for_bit() {
    let dir = tmp_dir("three-exits");
    let db_path = dir.join("store.sqlite");

    // 出口 1：账本落库 + SQL 聚合（schema v4 起的真实路径：`trades` 表，
    // 一次 `trade_totals` 求全部腿）。载体从 JSONL 换成 SQL，被钉住的
    // 东西没变——这仍是「账本自己报出的胜率」。
    let corpus = corpus();
    let log = TradeLog::new(Database::open(&db_path).expect("store opens"), Mode::Dry);
    for c in corpus.iter() {
        log.append(&TradeRecord::from_closed(c)).expect("row lands");
    }
    let totals = log
        .trade_totals(TradeScope::AllAccounts)
        .expect("the book aggregates");
    assert_eq!(
        totals.total_trades, 100,
        "100 笔全部进总数（每一笔都是已平仓成交）"
    );
    assert_eq!(
        (totals.tally.wins, totals.tally.flats, totals.tally.losses),
        (WINS as u32, FLATS as u32, LOSSES as u32),
        "胜负平三桶按净利符号划分"
    );
    let from_trade_db = totals.tally.win_rate().expect("70 笔已决");

    // 出口 2：`Core::trade_summary(None)` —— 挂同一个 store 的内核应答的
    // `trades.summary` payload。它走同一条 SQL 聚合，所以这一步同时钉住
    // 「wire 值与持久化值同源」。
    let core = Core::new(CoreConfig {
        db_path: Some(db_path.clone()),
        order_log_path: None,
        position_log_path: None,
        ..CoreConfig::default()
    });
    let wire = core.trade_summary(None);
    let from_trade_summary = wire["winRate"].as_f64().expect("winRate is a number");
    assert_eq!(
        wire["totalTrades"].as_u64(),
        Some(100),
        "wire 的 totalTrades 与账本聚合同源"
    );

    // 出口 2b：per-account 折叠路径（`recent_trades(0)` 全史重折）。这是
    // `trade_summary` 里**另一段**实现，也必须同口径——否则 #186 把
    // `limit: 0` 改成「默认一页」时，这条路径会静默改变数字。
    let per_account = core.trade_summary(Some(blitzkrieg_core::account::DEFAULT_ACCOUNT_ID));
    let from_per_account = per_account["winRate"]
        .as_f64()
        .expect("per-account winRate is a number");
    assert_eq!(
        per_account["totalTrades"].as_u64(),
        Some(100),
        "per-account 全史重折读到同一份 100 行"
    );

    // 出口 3：回测报告 —— `TradeStats::of_trades` 正是 `EventBacktester` 的
    // `trade_stats` 现在调用的那个函数（不是测试里的复刻）。
    let lines: Vec<TradeLine> = corpus
        .iter()
        .map(|c| TradeLine {
            id: c.id.clone(),
            asset: c.asset.clone(),
            direction: "up".into(),
            reason: "take_profit".into(),
            strategy: c.strategy.clone(),
            token_id: c.token_id.clone(),
            condition_id: c.condition_id.clone(),
            entry_price: c.entry_price,
            net_pnl_usd: c.net_pnl_usd,
            net_pnl_pct: c.net_pnl_pct,
            regime: "none".into(),
        })
        .collect();
    let stats = TradeStats::of_trades(&lines, Decimal::ZERO);
    assert_eq!(
        (stats.closed, stats.wins, stats.flat, stats.losses),
        (100, WINS as u64, FLATS as u64, LOSSES as u64),
        "回测三桶与成交口径同划分"
    );
    // 回测报告的字段是百分数（`winRatePct`，wire 上的既有拼写），所以这里
    // 比的是「小数 × 100」的投影——但它必须与另外两个出口的小数逐位同源。
    // 读数走**消费者的实际路径**：wire 上的百分数被读成 f64，再 /100 回到
    // 小数。不能在 Decimal 域做这个除法——`Decimal` 只有 28 位有效数字，
    // 非终止比值的百分数展开有 47 位，`pct / 100` 会在第 28 位先舍入、
    // 回到 f64 时高 1 ULP（`0.5714285714285715`，实测 bits …555）。
    // 没有消费者这么用：`serde` 把 Decimal 发成数字/字符串，读侧一律落到 f64。
    let pct_as_f64 = f64::try_from(stats.win_rate_pct).expect("fits f64");
    let from_backtest = pct_as_f64 / 100.0;
    // 另一条同源断言：这个百分数逐位就是同一条公式的投影，不是另算的。
    assert_eq!(
        stats.win_rate_pct,
        Tally::win_rate_pct_of(stats.wins, stats.losses).expect("decided"),
        "回测的 winRatePct 必须就是 Tally 百分数投影本身"
    );

    // 逐位相等。
    let want = expected_fraction();
    assert_eq!(
        from_trade_db.to_bits(),
        want.to_bits(),
        "trade_db 胜率 {from_trade_db} 必须是 {want} 的同一 f64 位型"
    );
    assert_eq!(
        from_trade_summary.to_bits(),
        want.to_bits(),
        "trades.summary 胜率 {from_trade_summary} 必须是 {want} 的同一 f64 位型"
    );
    assert_eq!(
        from_per_account.to_bits(),
        want.to_bits(),
        "per-account 折叠胜率 {from_per_account} 必须是 {want} 的同一 f64 位型"
    );
    assert_eq!(
        from_backtest.to_bits(),
        want.to_bits(),
        "backtest 胜率 {from_backtest} 必须是 {want} 的同一 f64 位型"
    );

    // 反向断言：错误的（含 flat）分母会给出 0.4，三个出口都不能是这个数。
    let wrong = (WINS as f64) / 100.0;
    assert_ne!(from_trade_db.to_bits(), wrong.to_bits());
    assert_ne!(from_trade_summary.to_bits(), wrong.to_bits());
    assert_ne!(from_per_account.to_bits(), wrong.to_bits());
    assert_ne!(from_backtest.to_bits(), wrong.to_bits());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 只有平手的一批成交：三个出口都必须落在「未知」的拼写 `0.0`，而不是
/// 把平手当负笔报出一个 < 1 的胜率（旧 `backtest` 分母 = 100 会给 0.0 但
/// 语义是「全输」；这里钉的是「0 笔已决」这条路也走同一个公式）。
#[test]
fn all_flat_corpus_is_unknown_zero_on_every_exit_not_a_loss_rate() {
    let dir = tmp_dir("all-flat");
    let corpus = vec![closed_with_net("flat-1", Decimal::ZERO)];

    let log = TradeLog::new(
        Database::open(dir.join("store.sqlite")).expect("store opens"),
        Mode::Dry,
    );
    for c in &corpus {
        log.append(&TradeRecord::from_closed(c)).expect("row lands");
    }
    let totals = log
        .trade_totals(TradeScope::AllAccounts)
        .expect("the book aggregates");
    assert_eq!(
        (totals.tally.wins, totals.tally.losses, totals.tally.flats),
        (0, 0, 1)
    );
    assert_eq!(
        totals.tally.win_rate(),
        None,
        "公式层保留「未知」——wire 上的 0.0 只是形状的拼写"
    );
    // The wire spelling itself, through the same adapter the panel reads.
    let core = Core::new(CoreConfig {
        db_path: Some(dir.join("store.sqlite")),
        order_log_path: None,
        position_log_path: None,
        ..CoreConfig::default()
    });
    let wire = core.trade_summary(None);
    assert_eq!(
        wire["winRate"].as_f64(),
        Some(0.0),
        "trades.summary 的 winRate 是非可选字段：无已决笔时用 0.0，且(0,0)可在旁区分"
    );
    assert_eq!(wire["totalTrades"].as_u64(), Some(1), "平手仍计入总数");
    assert_eq!(wire["flat"].as_u64(), Some(1));
    assert_eq!(
        Tally::win_rate_of(totals.tally.wins.into(), totals.tally.losses.into()),
        None,
        "公式层保留「未知」"
    );

    let lines: Vec<TradeLine> = corpus
        .iter()
        .map(|c| TradeLine {
            id: c.id.clone(),
            asset: c.asset.clone(),
            direction: "up".into(),
            reason: "take_profit".into(),
            strategy: c.strategy.clone(),
            token_id: c.token_id.clone(),
            condition_id: c.condition_id.clone(),
            entry_price: c.entry_price,
            net_pnl_usd: c.net_pnl_usd,
            net_pnl_pct: c.net_pnl_pct,
            regime: "none".into(),
        })
        .collect();
    let stats = TradeStats::of_trades(&lines, Decimal::ZERO);
    assert_eq!(stats.win_rate_pct, Decimal::ZERO);
    assert_eq!(
        Result::<_, ()>::Ok(stats.win_rate_pct / Decimal::ONE_HUNDRED),
        Ok(Decimal::ZERO),
        "回测出口同样落在 0，不是把平手算成负"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 口径判定的精确性：净利 `0.000000000000000001` 是胜，不是平手。
/// 浮点比较会把它折成 0（f64 精度 ~1e-16 量级的十进制字面量在转 f64 后
/// 可能落 0），从而从分母里静默消失。
#[test]
fn outcome_uses_decimal_exactness_not_float_rounding() {
    assert_eq!(Outcome::of_net(dec!(0.000000000000000001)), Outcome::Win);
    assert_eq!(Outcome::of_net(dec!(-0.000000000000000001)), Outcome::Loss);
    assert_eq!(Outcome::of_net(Decimal::ZERO), Outcome::Flat);

    let mut t = Tally::default();
    for c in [
        dec!(0.000000000000000001),
        dec!(-0.000000000000000001),
        Decimal::ZERO,
    ] {
        t.fold_net(c);
    }
    assert_eq!(
        t,
        Tally {
            wins: 1,
            losses: 1,
            flats: 1
        }
    );
    assert_eq!(t.win_rate(), Some(0.5));
}

/// wire 上只有一种单位：`trades.summary` 的 `winRate` 是小数 0–1。
///
/// 这个断言是**协议形状**的钉子，不是实现细节：前端 `format.ts` 的
/// `winRateValue` 已经丢弃服务端字段自行复算（2026-11-14/15 操作者裁决），
/// 所以单位翻转对面板透明；但任何别的消费者（脚本、外部客户端）读到的必须
/// 是 0–1，不能是 0–100。
#[test]
fn trades_summary_wire_carries_a_fraction_not_a_percentage() {
    let dir = tmp_dir("wire-unit");
    let db_path = dir.join("store.sqlite");
    let log = TradeLog::new(Database::open(&db_path).expect("store opens"), Mode::Dry);
    for c in corpus() {
        log.append(&TradeRecord::from_closed(&c))
            .expect("row lands");
    }

    let core = Core::new(CoreConfig {
        db_path: Some(db_path),
        order_log_path: None,
        position_log_path: None,
        ..CoreConfig::default()
    });
    let wire = core.trade_summary(None);
    let value = wire["winRate"].as_f64().expect("winRate is a number");
    assert!(
        (0.0..=1.0).contains(&value),
        "winRate 必须是 0–1 的小数，实际 {value}（≥1 说明还在发百分数）"
    );
    assert!(
        value > 0.5 && value < 0.6,
        "40 胜 / 30 负 = 0.5714…，实际 {value}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
