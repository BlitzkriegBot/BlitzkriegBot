//! Arb totals contract (#577 frontend consumption): arbNetPnlUsd / arbWins /
//! arbLosses / arbClosedTrades / arbActivePairs ride the cross_venue_arb
//! row's GLOBAL fold on every account slice; non-arb rows carry no arb* keys.
//! Moved verbatim from `dispatch_cap_gate_tests.rs` (pure code motion).

use super::super::*;
use super::helpers::{
    confirm_cross_venue_mapping, core_with_cross_venue_arb, cross_venue_markets,
    feed_cross_venue_books,
};

// 套利合计字段（前端消费契约，PR #577）：arbNetPnlUsd / arbWins /
// arbLosses / arbClosedTrades / arbActivePairs —— 只有 cross_venue_arb
// 的行携带；无论账户切片选哪个账户，五个字段都读套利单元的全局合计
// （双腿平仓各记在 (cross_venue_arb, 各腿账户) 键下，全局 fold 是和）。
// 非套利行不带 arb* 键 —— 前端按行内有无字段 feature-detect。
#[test]
fn arb_totals_ride_every_account_slice_of_the_arb_row_only() {
    let mut c = core_with_cross_venue_arb(Some(dec!(1000)));
    // A≠B 的腿配置（跨所配对的声明闸要求两腿盖章到不同账户）：套利
    // 单元 = 实例主账户 default + 腿账户 arb-b。arb-b 是内存账簿的新
    // 账户（set 校验要求账户已配置）。
    c.accounts_mut()
        .insert(crate::account::Account {
            id: crate::account::AccountId::from("arb-b"),
            name: "arb-b".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys::default(),
            venue: String::new(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            opening_equity: None,
            updated_at_ms: 0,
        })
        .expect("arb-b does not exist yet");
    let _ = c
        .accounts_mut()
        .get_mut(&crate::account::AccountId::from("arb-b"))
        .expect("arb-b just inserted")
        .set_balance(dec!(1000));
    c.arb_config_set(&crate::account::CrossVenueArbConfig {
        leg_a_venue: "polymarket".into(),
        leg_a_account: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        leg_b_venue: "kalshi".into(),
        leg_b_account: "arb-b".into(),
        pairs: vec!["BTC".into()],
    })
    .expect("the A≠B leg configuration lands");
    let now = 1_700_000_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: cross_venue_markets(now),
            now_ms: now,
        },
        now,
    );
    confirm_cross_venue_mapping(&mut c, now);
    feed_cross_venue_books(&mut c, now + 1_000);
    assert_eq!(c.engine_evaluate(now + 2_000), 2, "both legs place");
    // 声明受理：跨所配对真的进了配对簿（在册读数的前提）。
    assert_eq!(
        c.pair_book_all().len(),
        1,
        "the A≠B pair declaration lands in the book"
    );

    // 两腿平仓：生产 close 路径（close → on_position_closed），净额一
    // 正一负，wins/losses 各记一笔；合计字段读全局 fold = 两腿之和
    // （含各腿真实手续费 —— 合计字段是 fold 的别名，不是手抄数）。
    let legs: Vec<crate::position::OpenPosition> = c.positions().open_positions().to_vec();
    assert_eq!(legs.len(), 2, "fixture premise: the pair is open");
    let mut net_sum = Decimal::ZERO;
    for held in &legs {
        // 价差驱动平仓价：正腿 +0.40/股，负腿 −0.30/股。
        let delta = if held.entry_price < dec!(0.43) {
            dec!(0.40)
        } else {
            dec!(-0.30)
        };
        let closed = c
            .positions
            .close(
                &held.id,
                held.entry_price + delta,
                crate::model::ExitReason::TakeProfit,
                false,
                1,
                "venue",
            )
            .expect("the leg closes through the production close path");
        net_sum += closed.net_pnl_usd;
        c.on_position_closed(&closed, 1);
    }
    assert!(net_sum.is_sign_positive(), "the fixture books a net profit");

    // ── 全局行：arb* = 该行 fold 值（双腿各账户键之和）──────────────
    let global = c.strategy_stats();
    let g = strategy_entry(&global, "cross_venue_arb");
    assert_eq!(
        dec_of(&g["arbNetPnlUsd"]),
        net_sum,
        "the arb row's total is both legs' sum: {g}"
    );
    assert_eq!(g["arbWins"], serde_json::json!(1));
    assert_eq!(g["arbLosses"], serde_json::json!(1));
    assert_eq!(g["arbClosedTrades"], serde_json::json!(2));
    assert_eq!(
        g["arbActivePairs"],
        serde_json::json!(1),
        "the pair sits in the book until the round rolls: {g}"
    );
    assert_eq!(
        dec_of(&g["netPnlUsd"]),
        net_sum,
        "the plain counter and the arb total agree on the global row"
    );

    // ── 账户切片行：arb* 仍读全局合计；平计数是该账户的腿切片 ──
    // （一正一负 ⇒ 任一切片都只看得见自己那腿的胜负，但 arb* 都是 1/1。）
    for account in [blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "arb-b"] {
        let sliced = c.strategy_stats_for_account(account);
        let s = strategy_entry(&sliced, "cross_venue_arb");
        assert_eq!(
            dec_of(&s["arbNetPnlUsd"]),
            net_sum,
            "every account's page shows the arb unit's combined take ({account}): {s}"
        );
        assert_eq!(s["arbWins"], serde_json::json!(1), "{account}");
        assert_eq!(s["arbLosses"], serde_json::json!(1), "{account}");
        assert_eq!(s["arbClosedTrades"], serde_json::json!(2), "{account}");
        assert_eq!(s["arbActivePairs"], serde_json::json!(1), "{account}");
        // 切片的平计数是本账户腿的（两腿账户不同 ⇒ 各自 1 笔）。
        assert_eq!(
            s["closedTrades"],
            serde_json::json!(1),
            "the slice's plain counters stay per-account ({account}): {s}"
        );
    }

    // ── 非套利行不带 arb* 键 ──
    let g_plain = strategy_entry(&global, "spread_arb");
    assert!(
        g_plain.get("arbNetPnlUsd").is_none(),
        "a non-arb row carries no arb totals: {g_plain}"
    );
    let s_plain_slice = c.strategy_stats_for_account("arb-b");
    let s_plain = strategy_entry(&s_plain_slice, "spread_arb");
    assert!(
        s_plain.get("arbNetPnlUsd").is_none(),
        "a non-arb row carries no arb totals on a slice either: {s_plain}"
    );
}
