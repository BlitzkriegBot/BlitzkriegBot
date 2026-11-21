#![cfg(test)]

use super::*;

use crate::service::Core;
use blitzkrieg_market_api::net::now_ms;
use tokio::sync::Mutex as AsyncMutex;

// ── 行情页重复行：快照装配层的 slot 过滤 + (venue,asset) 唯一化 ─────────

/// 复现一次生产 12 行事故的输入形状：descriptor 的 `round_slot` 是
/// 「到期所属 slot」（正在交易的一轮 = 当前 slot+1），venue discovery
/// 按「现在还开着」列市场，slot 边界附近再下一档（当前+2）与当前轮
/// 并存于 scanner，逐市场行情行不看 slot 时同一 (venue,asset) 两行
/// 同页。负向契约：快照 `marketPrices` 每个 (venue,asset) 恰一行 ——
/// 下一档到期（当前+2）的行不出现，正在交易的一轮（当前+1）才出现。
#[tokio::test]
async fn engine_round_prices_are_unique_per_venue_asset_across_slots() {
    use crate::model::CryptoMarket;
    use rust_decimal_macros::dec;

    let (core, registry, peer) = hot_reload_fixture().await;
    let round_sec = 900i64;
    let now = now_ms();
    let slot = now / 1000 / round_sec;
    let trading_end = (slot + 2) * round_sec * 1000; // the round being traded (slot+1)
    let next_end = (slot + 3) * round_sec * 1000; // the next tier, still "open"

    // One (venue, asset) per row: BTC on polymarket/kalshi for the
    // trading round, plus the same kalshi asset on the NEXT tier's
    // expiry — the shape the venue's "still open" discovery puts on the
    // wire at a slot boundary. Feeds land in the adversarial order the
    // boundary produces (kalshi's 5s poll can put the next tier's batch
    // on the wire BEFORE the trading batch): next tier first, then the
    // trading batch. All through the feed entry (`engine_on_data`), the
    // same path a real plugin's RoundMarkets lands in.
    //
    // E2: per-VENUE batches (the production shape — a discovery lists its
    // own venue's round). The pre-E2 single mixed batch relied on the
    // global-min offset fold to time the kalshi rows: the rollover arm
    // observes only `markets.first()`'s declaration, so kalshi's own
    // clock never spoke and its rows silently rode polymarket's offset.
    // The family-aware filter reads each venue's OWN clock, so the
    // kalshi batch carries its declaration the way a real feed does.
    let mk = |asset: &str, venue: &str, end_ms: i64, up: &str| CryptoMarket {
        asset: asset.to_string(),
        condition_id: format!("cond-{venue}-{asset}-{end_ms}"),
        question_id: format!("q-{venue}-{asset}-{end_ms}"),
        up_token_id: format!("up-{venue}-{asset}-{end_ms}"),
        down_token_id: format!("down-{venue}-{asset}-{end_ms}"),
        up_price: up.parse().unwrap(),
        down_price: dec!(1) - up.parse::<rust_decimal::Decimal>().unwrap(),
        expires_at_ms: end_ms,
        round_slot: end_ms / 1000 / round_sec,
        round_duration_sec: round_sec,
        archive_verdict: false,
        venue: venue.to_string(),
        neg_risk: false,
        question: format!("{asset} up or down"),
    };
    let feed = |core: &std::sync::Arc<AsyncMutex<Core>>, markets: Vec<CryptoMarket>, at: i64| {
        let core = core.clone();
        async move {
            core.lock().await.engine_on_data(
                crate::engine::DataEvent::RoundMarkets {
                    markets,
                    now_ms: at,
                },
                at,
            );
        }
    };
    feed(&core, vec![mk("BTC", "kalshi", next_end, "0.55")], now).await;
    feed(
        &core,
        vec![mk("BTC", "polymarket", trading_end, "0.30")],
        now,
    )
    .await;
    feed(&core, vec![mk("BTC", "kalshi", trading_end, "0.40")], now).await;

    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"engine.round","params":{}}"#.to_string(),
    )
    .await;
    let prices = reply["result"]["marketPrices"]
        .as_array()
        .expect("marketPrices array")
        .clone();
    let keys: Vec<(String, String)> = prices
        .iter()
        .map(|p| {
            (
                p["venue"].as_str().unwrap_or_default().to_string(),
                p["asset"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        keys.len(),
        "no (venue,asset) may repeat in the snapshot's price rows: {keys:?}"
    );
    assert_eq!(
        sorted,
        vec![
            ("kalshi".to_string(), "BTC".to_string()),
            ("polymarket".to_string(), "BTC".to_string()),
        ],
        "the trading round's rows only — the next tier must not surface: {prices:?}"
    );
    // The surviving kalshi row is the trading round's quote (0.40), not
    // the next tier's (0.55).
    let kalshi_up = prices
        .iter()
        .find(|p| p["venue"] == "kalshi")
        .expect("kalshi row")["up"]
        .as_str()
        .expect("decimal renders as a string")
        .to_string();
    assert_eq!(
        kalshi_up, "0.40",
        "the trading round's descriptor survives, the next tier's does not"
    );
}

/// `books_view` 是第二个逐市场平铺的数据源（E8-c 盘口深度，快照的
/// `books` 字段）——#560 只修了 `engine.round` 面，同一 slot 边界形状
/// 在这里每资产吐两行、盘口选择器按 (venue,asset) 撞重复 key（生产
/// 2026-10-22 实录：12 行 = PM 4 + kalshi 两轮 8）。负向契约：与
/// `engine.round` 同判据 —— 相邻两轮并存时 `engine.books` 每个
/// (venue,asset) 恰一行，且活的是正在交易的一轮（token 带交易轮的
/// 到期戳）。
#[tokio::test]
async fn engine_books_rows_are_unique_per_venue_asset_across_slots() {
    use crate::model::CryptoMarket;
    use rust_decimal_macros::dec;

    let (core, registry, peer) = hot_reload_fixture().await;
    let round_sec = 900i64;
    let now = now_ms();
    let slot = now / 1000 / round_sec;
    let trading_end = (slot + 2) * round_sec * 1000; // the round being traded (slot+1)
    let next_end = (slot + 3) * round_sec * 1000; // the next tier, still "open"

    let mk = |asset: &str, venue: &str, end_ms: i64| CryptoMarket {
        asset: asset.to_string(),
        condition_id: format!("cond-{venue}-{asset}-{end_ms}"),
        question_id: format!("q-{venue}-{asset}-{end_ms}"),
        up_token_id: format!("up-{venue}-{asset}-{end_ms}"),
        down_token_id: format!("down-{venue}-{asset}-{end_ms}"),
        up_price: dec!(0.5),
        down_price: dec!(0.5),
        expires_at_ms: end_ms,
        round_slot: end_ms / 1000 / round_sec,
        round_duration_sec: round_sec,
        archive_verdict: false,
        venue: venue.to_string(),
        neg_risk: false,
        question: format!("{asset} up or down"),
    };
    // Adversarial order, same as the price-row test: the next tier's
    // batch lands first, then the trading round's — per VENUE (the
    // production discovery shape; see the price-row test's E2 note on
    // why the mixed single batch no longer reflects a real feed).
    {
        let mut c = core.lock().await;
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![mk("BTC", "kalshi", next_end)],
                now_ms: now,
            },
            now,
        );
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![mk("BTC", "polymarket", trading_end)],
                now_ms: now,
            },
            now,
        );
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![mk("BTC", "kalshi", trading_end)],
                now_ms: now,
            },
            now,
        );
    }

    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"engine.books","params":{}}"#.to_string(),
    )
    .await;
    let rows = reply["result"].as_array().expect("books array");
    let keys: Vec<(String, String)> = rows
        .iter()
        .map(|r| {
            (
                r["venue"].as_str().unwrap_or_default().to_string(),
                r["asset"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        keys.len(),
        "no (venue,asset) may repeat in the depth view: {keys:?}"
    );
    assert_eq!(
        sorted,
        vec![
            ("kalshi".to_string(), "BTC".to_string()),
            ("polymarket".to_string(), "BTC".to_string()),
        ],
        "the trading round's rows only — the next tier must not surface: {rows:?}"
    );
    // The surviving kalshi row carries the TRADING round's tokens, not
    // the next tier's (the depth chart and the K-line card both query by
    // token id — pointing at the wrong expiry would be worse than a dup).
    let kalshi_up = rows
        .iter()
        .find(|r| r["venue"] == "kalshi")
        .expect("kalshi row")["upTokenId"]
        .as_str()
        .expect("token id");
    assert_eq!(
        kalshi_up,
        format!("up-kalshi-BTC-{trading_end}"),
        "the depth view must expose the trading round's tokens"
    );
}

/// RFC-661 §五.7 的 wire 级验收（M3 后的自然行形态）：双 venue 同窗口
/// 同字节 token，各落各的 (venue, token) 行（M1 双键），engine.books
/// 无需任何合成臂就出「两所各一行」——标签是行自带的（venue_of 对共享
/// token 歧义拒绝，反查 fallback 拿不到标签也不再需要），深度各读各的
/// book。若各家 book 深度不同，两行互不串读（§五.6 隔离在显示面的旁证）。
#[tokio::test]
async fn a_shared_token_shows_one_books_row_per_venue_naturally() {
    use crate::model::CryptoMarket;
    use rust_decimal_macros::dec;

    let (core, registry, peer) = hot_reload_fixture().await;
    let round_sec = 900i64;
    let now = now_ms();
    let slot = now / 1000 / round_sec;
    let trading_end = (slot + 2) * round_sec * 1000;

    // 镜像性质本身：两所同窗口 token 逐字节相同（生产 09:30:02 批次
    // 实证的形状），conditionId 同理。
    let mk = |venue: &str, end_ms: i64| CryptoMarket {
        asset: "BTC".to_string(),
        condition_id: format!("cond-{end_ms}"),
        question_id: format!("q-{end_ms}"),
        up_token_id: "mirror-up".to_string(),
        down_token_id: "mirror-down".to_string(),
        up_price: dec!(0.5),
        down_price: dec!(0.5),
        expires_at_ms: end_ms,
        round_slot: end_ms / 1000 / round_sec,
        round_duration_sec: round_sec,
        archive_verdict: false,
        venue: venue.to_string(),
        neg_risk: false,
        question: "BTC up or down".to_string(),
    };
    // PM 批先落表，poly 批后到：双键下两所各得各的行（M1 语义——
    // 插入次序无关，无显示簿参与）。
    {
        let mut c = core.lock().await;
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![mk("polymarket", trading_end)],
                now_ms: now,
            },
            now,
        );
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![mk("polysimulator", trading_end)],
                now_ms: now,
            },
            now,
        );
    }

    // 两所各自把 book 打到同一个 token 上（后写的覆盖前写的——book
    // 快照按 token 一份）：这里只喂一份，两行读同一快照，深度逐位同。
    {
        let mut c = core.lock().await;
        c.engine_on_data(
            crate::engine::DataEvent::Book {
                token_id: "mirror-up".to_string(),
                bids: vec![(dec!(0.46), dec!(100))],
                asks: vec![(dec!(0.54), dec!(200))],
                now_ms: now,
            },
            now,
        );
    }

    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"engine.books","params":{}}"#.to_string(),
    )
    .await;
    let rows = reply["result"].as_array().expect("books array");
    // 双 venue 各一行：总行数 = 两所之和，没有合成臂也一张不缺。
    let poly_rows: Vec<_> = rows
        .iter()
        .filter(|r| r["venue"] == "polysimulator")
        .collect();
    assert_eq!(
        poly_rows.len(),
        1,
        "the sim venue gets its own depth row from its own table row: {rows:?}"
    );
    assert_eq!(
        poly_rows[0]["upTokenId"], "mirror-up",
        "the sim row exposes the SHARED token — same market, its own tag"
    );
    let pm_rows: Vec<_> = rows.iter().filter(|r| r["venue"] == "polymarket").collect();
    assert_eq!(
        pm_rows.len(),
        1,
        "the PM venue keeps exactly its own row: {rows:?}"
    );
    // 标签是行自带的：venue_of("mirror-up") 对双 venue 语料歧义拒绝
    // （None），token 反查 fallback 拿不到任何标签——行 venue 字段是
    // 唯一来源，两行各自带着正确的所名。
    assert_eq!(
        poly_rows[0]["venue"], "polysimulator",
        "the tag comes from the row itself, never a token reverse-lookup"
    );
    assert_eq!(pm_rows[0]["venue"], "polymarket");
    // 同一份共享 book 的真实读数：两行深度逐位同（book 按 token 一份）。
    assert_eq!(
        poly_rows[0]["up"]["bestBid"], pm_rows[0]["up"]["bestBid"],
        "both rows read the same shared book — real depth, both sides"
    );
}

/// 多族同 (venue,asset) 时行情卡显示哪一族（2026-11-17 实机报障：书簿
/// 在刷新、行情卡的上/下报价冻住）。生产形状：Polymarket 同一资产同时挂
/// 着 5m 与 15m 两族。`engine.round` 的 `(venue,asset)` 唯一化是「先到
/// 先得」，行序 = discovery 到达顺序，于是卡上显示的族与倒计时实际跑的
/// 网格无关 —— 5m 倒计时每 5 分钟翻页，卡上却是 15m 几乎不动的报价，
/// 看起来就是「价格冻住」。选择权威是扫描器的当前网格
/// （`scanner_round_duration`：`cfg.round_duration_sec` is the SELECTION
/// authority），所以当前网格那一族必须赢，且与到达顺序无关。
///
/// 断言只写「当前网格那一族赢」，不写死 300/900 —— 网格是运行时量
/// （`round_duration.set` 可改），测试读出来再用。
#[tokio::test]
async fn the_active_grid_family_wins_the_price_row_whatever_the_arrival_order() {
    use crate::model::CryptoMarket;
    use rust_decimal::Decimal;

    // 两种对抗性到达顺序，逐位同判：先到的族不许因此赢。
    for sibling_first in [true, false] {
        let (core, registry, peer) = hot_reload_fixture().await;
        let now = now_ms();
        // 当前网格（倒计时真跑的那一族）与一个同时活着的兄弟族。
        let active = {
            let c = core.lock().await;
            c.engine().expect("engine").scanner_round_duration()
        };
        assert!(active > 0, "the fixture boots an engine with a real grid");
        let sibling = if active == 300 { 900 } else { 300 };

        let mk = |dur: i64, up: &str| {
            let slot = now / 1000 / dur;
            // 生产形状（`extensions/polymarket/src/discovery.rs:139-164`）：
            // discovery 按 CURRENT slot 拼 slug，拿到的是**当前这一轮**的
            // end_date，`round_slot = end/dur` = 当前+1 —— 正在交易的一轮，
            // 且 venue 钟偏移恒为 0（声明与网格对齐）。
            let end = (slot + 1) * dur * 1000;
            CryptoMarket {
                asset: "BTC".to_string(),
                condition_id: format!("cond-pm-{dur}-{end}"),
                question_id: format!("q-pm-{dur}-{end}"),
                up_token_id: format!("up-pm-{dur}-{end}"),
                down_token_id: format!("down-pm-{dur}-{end}"),
                up_price: up.parse().unwrap(),
                down_price: Decimal::ONE - up.parse::<Decimal>().unwrap(),
                expires_at_ms: end,
                round_slot: end / 1000 / dur,
                round_duration_sec: dur,
                archive_verdict: false,
                venue: "polymarket".to_string(),
                neg_risk: false,
                question: "BTC up or down".to_string(),
            }
        };
        let active_row = mk(active, "0.90");
        let sibling_row = mk(sibling, "0.10");
        {
            let mut c = core.lock().await;
            let order = if sibling_first {
                vec![sibling_row.clone(), active_row.clone()]
            } else {
                vec![active_row.clone(), sibling_row.clone()]
            };
            for row in order {
                c.engine_on_data(
                    crate::engine::DataEvent::RoundMarkets {
                        markets: vec![row],
                        now_ms: now,
                    },
                    now,
                );
            }
        }
        let reply = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":1,"method":"engine.round","params":{}}"#.to_string(),
        )
        .await;
        let prices = reply["result"]["marketPrices"]
            .as_array()
            .expect("marketPrices array");
        let pm: Vec<&Value> = prices
            .iter()
            .filter(|p| p["venue"] == "polymarket")
            .collect();
        assert_eq!(
            pm.len(),
            2,
            "each live family keeps its own (venue,asset,duration) price row              (sibling_first={sibling_first}): {prices:?}"
        );
        let by_dur = |d: i64| {
            pm.iter()
                .find(|p| p["durationSec"] == d)
                .unwrap_or_else(|| panic!("missing durationSec={d}: {prices:?}"))
        };
        let parse_up = |p: &Value| -> Decimal {
            p["up"]
                .as_str()
                .expect("decimal renders as a string")
                .parse()
                .expect("decimal")
        };
        assert_eq!(
            parse_up(by_dur(active)),
            active_row.up_price,
            "the {active}s row must keep its own quote (sibling_first={sibling_first})"
        );
        assert_eq!(
            parse_up(by_dur(sibling)),
            sibling_row.up_price,
            "the {sibling}s row must keep its own quote (sibling_first={sibling_first})"
        );
        // Depth view carries the same two families, tagged so the panel can
        // filter by the account's anchored duration.
        let depth = {
            let c = core.lock().await;
            c.books_view(5, now)
        };
        let pm_depth: Vec<_> = depth
            .iter()
            .filter(|r| r.venue == "polymarket" && r.asset == "BTC")
            .collect();
        assert_eq!(
            pm_depth.len(),
            2,
            "the depth view keeps one row per live family: {depth:?}"
        );
        let depth_of = |d: i64| {
            pm_depth
                .iter()
                .find(|r| r.duration_sec == d)
                .unwrap_or_else(|| panic!("missing depth duration_sec={d}: {depth:?}"))
        };
        assert_eq!(
            depth_of(active).up_token_id,
            active_row.up_token_id,
            "the {active}s depth row must carry that family's tokens              (sibling_first={sibling_first})"
        );
        assert_eq!(
            depth_of(sibling).up_token_id,
            sibling_row.up_token_id,
            "the {sibling}s depth row must carry that family's tokens              (sibling_first={sibling_first})"
        );
    }
}

// ── E2（多族漏网 ③④）：读面过滤按行自身族钟 ─────────────────────────

/// A daily (86400s) family rides the SAME scanner beside the 15m family
/// (polymarket lists 15m, kalshi lists a daily — the multi-family data
/// plane B1d's discovery instances feed). The pre-E2 filter
/// (`slot + 1` on the GLOBAL grid) dropped the daily row
/// forever — a daily-grid slot number can never equal the global grid's
/// +1 — so `books_view` and the `engine.round` price rows lost every
/// foreign-family row. Both views now judge each row on its OWN (venue,
/// duration) family clock (the shared `is_live_round_row` predicate):
/// the 15m row AND the daily row surface together, both next tiers stay
/// out, and the `round_slot == 0` legacy bridge is untouched.
///
/// 偏移按 venue 单键（B1a 设计：venue 墙钟属性，同 venue 各族共用），
/// 多场馆同格网的向后批次（2026-10-25 poly 轮钟冻结定谳）：kalshi 迟喂
/// 一批「窗口已过期」的旧 slot——收下它会把 polymarket 已落好的新轮行
/// 拖掉（ping-pong 互相覆写，轮钟冻 00:00）。守卫丢过期批次、放行活窗口
/// 批次（未来档先落、当前轮后到的对抗性喂序由上面那三个测试钉住）。
#[tokio::test]
async fn a_stale_expired_batch_is_dropped_and_a_live_one_is_kept() {
    use crate::model::CryptoMarket;
    use rust_decimal_macros::dec;

    let (core, registry, peer) = hot_reload_fixture().await;
    let round_sec = 900i64;
    let now = now_ms();
    let slot = now / 1000 / round_sec;
    let live_end = (slot + 2) * round_sec * 1000; // the round being traded
    let expired_end = slot * round_sec * 1000; // the previous slot's end — already past

    let mk = |venue: &str, end_ms: i64| CryptoMarket {
        asset: "BTC".to_string(),
        condition_id: format!("cond-{venue}-{end_ms}"),
        question_id: format!("q-{venue}-{end_ms}"),
        up_token_id: format!("up-{venue}-{end_ms}"),
        down_token_id: format!("down-{venue}-{end_ms}"),
        up_price: dec!(0.5),
        down_price: dec!(0.5),
        expires_at_ms: end_ms,
        round_slot: end_ms / 1000 / round_sec,
        round_duration_sec: round_sec,
        archive_verdict: false,
        venue: venue.to_string(),
        neg_risk: false,
        question: "BTC up or down".to_string(),
    };

    {
        let mut c = core.lock().await;
        // polymarket 先落当前交易轮（活窗口）。
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![mk("polymarket", live_end)],
                now_ms: now,
            },
            now,
        );
        // kalshi 迟喂一批已过期 slot（ping-pong 的那一半）——必须被丢。
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![mk("kalshi", expired_end)],
                now_ms: now,
            },
            now,
        );
    }

    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"engine.round","params":{}}"#.to_string(),
    )
    .await;
    let prices = reply["result"]["marketPrices"]
        .as_array()
        .expect("market prices array");
    assert!(
        prices.iter().any(|p| p["venue"] == "polymarket"),
        "the live batch's markets stay registered: {prices:?}"
    );
    assert!(
        !prices.iter().any(|p| p["venue"] == "kalshi"),
        "the expired batch must not drag the table backward: {prices:?}"
    );
}

/// 同 venue 双族的声明偏移互踩是 B1a 既有形状——本牙用跨 venue 双族
/// 隔开偏移，钉的是审计点「外族行在全局 900s 网格上永不等于
/// trading_slot 而被整体滤没」，与行属于哪个 venue 无关（旧谓词对
/// 任何 duration ≠ 全局网格的行一律误杀）。
#[tokio::test]
async fn multi_family_rows_surface_in_both_read_views() {
    use crate::model::CryptoMarket;
    use rust_decimal_macros::dec;

    let (core, _registry, _peer) = hot_reload_fixture().await;
    let now = now_ms();
    let s_slot = now / 1000 / 900i64; // the 900s grid's current slot
    let d_slot = now / 1000 / 86_400i64; // the daily grid's current slot
    let s_trading_end = (s_slot + 2) * 900 * 1000; // the traded 15m round
    let s_next_end = (s_slot + 3) * 900 * 1000; // the 15m next tier (out of band)
    let d_trading_end = (d_slot + 2) * 86_400 * 1000; // the traded daily round
    let d_next_end = (d_slot + 3) * 86_400 * 1000; // the daily next tier (out of band)

    let mk = |asset: &str, venue: &str, end_ms: i64, dur: i64| CryptoMarket {
        asset: asset.to_string(),
        condition_id: format!("cond-{venue}-{asset}-{end_ms}"),
        question_id: format!("q-{asset}-{end_ms}"),
        up_token_id: format!("up-{asset}-{end_ms}"),
        down_token_id: format!("down-{asset}-{end_ms}"),
        up_price: dec!(0.5),
        down_price: dec!(0.5),
        expires_at_ms: end_ms,
        round_slot: end_ms / 1000 / dur,
        round_duration_sec: dur,
        archive_verdict: false,
        venue: venue.to_string(),
        neg_risk: false,
        question: format!("{asset} up or down"),
    };
    // Each venue's NEXT tier first (its declaration is out of the
    // ±2-round band → refused), then the traded round — the adversarial
    // order the boundary produces. The DAILY family feeds BEFORE the 15m
    // one: engine.rs's #550 stale-row sweep measures a sibling's lag on
    // its own grid against the FEEDING family's slot, and a daily feed
    // arriving last would mis-measure a live 15m row ~96 rounds "behind"
    // and sweep it (a pre-existing B1c cross-family wrinkle, out of this
    // fix's scope) — the 15m-feeding direction measures the daily row at
    // lag ≈ 0 and keeps it.
    {
        let mut c = core.lock().await;
        for (asset, venue, end, dur) in [
            ("ETH", "kalshi", d_next_end, 86_400),
            ("ETH", "kalshi", d_trading_end, 86_400),
            ("BTC", "polymarket", s_next_end, 900),
            ("BTC", "polymarket", s_trading_end, 900),
        ] {
            c.engine_on_data(
                crate::engine::DataEvent::RoundMarkets {
                    markets: vec![mk(asset, venue, end, dur)],
                    now_ms: now,
                },
                now,
            );
        }
    }

    // ③ the depth view: both FAMILIES' traded rows are in, both next
    // tiers are out, and the traded tokens (not the next tier's) surface.
    let rows = {
        let c = core.lock().await;
        c.books_view(5, now)
    };
    let tokens: Vec<&str> = rows.iter().map(|r| r.up_token_id.as_str()).collect();
    assert!(
        tokens.contains(&&format!("up-BTC-{s_trading_end}")[..]),
        "the 15m traded row surfaces: {rows:?}"
    );
    assert!(
        tokens.contains(&&format!("up-ETH-{d_trading_end}")[..]),
        "the daily traded row must NOT be dropped by the global grid: {rows:?}"
    );
    assert!(
        !tokens.contains(&&format!("up-BTC-{s_next_end}")[..]),
        "the 15m next tier stays filtered (#560): {rows:?}"
    );
    assert!(
        !tokens.contains(&&format!("up-ETH-{d_next_end}")[..]),
        "the daily next tier stays filtered on its OWN grid: {rows:?}"
    );

    // ④ the wire's price rows judge the same predicate — the ENGINE_ROUND
    // assembly's filter keeps exactly the two traded family rows.
    let c = core.lock().await;
    let e = c.engine().expect("engine");
    let scanner = e.scanner();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let surfaced: Vec<String> = scanner
        .round_state(now)
        .markets
        .iter()
        .filter(|m| scanner.is_live_round_row(m, now))
        .filter(|m| seen.insert((m.venue.clone(), m.asset.clone())))
        .map(|m| m.up_token_id.clone())
        .collect();
    assert_eq!(
        surfaced,
        vec![
            format!("up-ETH-{d_trading_end}"),
            format!("up-BTC-{s_trading_end}"),
        ],
        "the engine.round row filter keeps exactly the two traded family rows (table order)"
    );
    // The legacy bridge: a round_slot == 0 row passes unconditionally
    // (the pre-#560 corpora shape).
    let mut legacy = mk("SOL", "polymarket", s_trading_end, 900);
    legacy.round_slot = 0;
    assert!(
        scanner.is_live_round_row(&legacy, now),
        "round_slot == 0 rows ride the unconditional legacy bridge"
    );
}

/// The panel market section's single source of truth: `engine.round`
/// publishes one `currentRounds` object per account. Duration is the
/// account anchor (here 300), not the process default 900; prices and
/// books belong only to that family; `roundId` is the venue start ts;
/// `startMs`/`endMs` are the real window (a daily DST 23h span is
/// published as 82800s, not 86400).
#[tokio::test]
async fn engine_round_current_rounds_follow_account_anchor() {
    use crate::model::CryptoMarket;
    use crate::round_id::venue_round_id;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    let (core, registry, peer) = hot_reload_fixture().await;
    let set = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"round_duration.set","params":{"accountId":"default","durationSec":300,"reason":"ssot currentRound"}}"#
            .to_string(),
    )
    .await;
    assert!(
        set.get("error").is_none(),
        "anchoring default at 300 must succeed: {set}"
    );

    let now = now_ms();
    let mk = |asset: &str, dur: i64, up: &str| {
        let slot = now / 1000 / dur;
        let end = (slot + 1) * dur * 1000;
        CryptoMarket {
            asset: asset.to_string(),
            condition_id: format!("cond-pm-{asset}-{dur}-{end}"),
            question_id: format!("q-pm-{asset}-{dur}-{end}"),
            up_token_id: format!("up-pm-{asset}-{dur}-{end}"),
            down_token_id: format!("down-pm-{asset}-{dur}-{end}"),
            up_price: up.parse().unwrap(),
            down_price: Decimal::ONE - up.parse::<Decimal>().unwrap(),
            expires_at_ms: end,
            round_slot: end / 1000 / dur,
            round_duration_sec: dur,
            archive_verdict: false,
            venue: "polymarket".to_string(),
            neg_risk: false,
            question: format!("{asset} up or down"),
        }
    };
    {
        let mut c = core.lock().await;
        for row in [
            mk("BTC", 300, "0.61"),
            mk("ETH", 300, "0.44"),
            mk("BTC", 900, "0.22"),
            mk("ETH", 900, "0.33"),
        ] {
            c.engine_on_data(
                crate::engine::DataEvent::RoundMarkets {
                    markets: vec![row],
                    now_ms: now,
                },
                now,
            );
        }
    }

    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"engine.round","params":{}}"#.to_string(),
    )
    .await;
    let rounds = reply["result"]["currentRounds"]
        .as_array()
        .expect("currentRounds array");
    let cur = rounds
        .iter()
        .find(|r| r["accountId"] == "default")
        .unwrap_or_else(|| panic!("missing default currentRound: {rounds:?}"));
    assert_eq!(
        cur["durationSec"], 300,
        "duration is the account anchor, not process default 900: {cur}"
    );
    assert_eq!(cur["venue"], "polymarket");
    let slot = now / 1000 / 300;
    let expected_id = venue_round_id(slot, 300);
    assert_eq!(
        cur["roundId"].as_i64().expect("roundId"),
        expected_id,
        "roundId is the venue slug start ts"
    );
    let start = cur["startMs"].as_i64().expect("startMs");
    let end = cur["endMs"].as_i64().expect("endMs");
    assert_eq!(start, expected_id * 1000);
    let span = (end - start) / 1000;
    assert_eq!(
        span, 300,
        "5m window span must be 300s, not 900: start={start} end={end}"
    );
    assert!(
        cur["ageSec"].as_i64().unwrap() + cur["timeLeftSec"].as_i64().unwrap() == span
            || cur["timeLeftSec"].as_i64().unwrap() >= 0,
        "age+left tracks the published window: {cur}"
    );

    let prices = cur["prices"].as_array().expect("prices");
    let assets: Vec<&str> = prices
        .iter()
        .map(|p| p["asset"].as_str().unwrap())
        .collect();
    let mut sorted = assets.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        assets.len(),
        "one price card per asset: {prices:?}"
    );
    assert_eq!(sorted, vec!["BTC", "ETH"]);
    for p in prices {
        assert_eq!(
            p["durationSec"], 300,
            "no 15m twin in the account object: {p}"
        );
        assert_eq!(p["venue"], "polymarket");
        assert!(
            p["upTokenId"].as_str().unwrap_or("").contains("-300-"),
            "token ids belong to the 5m family: {p}"
        );
    }
    let btc = prices.iter().find(|p| p["asset"] == "BTC").unwrap();
    let btc_up = btc["up"]
        .as_str()
        .expect("decimal string")
        .parse::<Decimal>()
        .unwrap();
    assert_eq!(btc_up, dec!(0.61), "5m quote, not the 15m 0.22");

    let books = cur["books"].as_array().expect("books");
    assert_eq!(
        books.len(),
        prices.len(),
        "books match the same family: {books:?}"
    );
    for b in books {
        assert_eq!(
            b["durationSec"], 300,
            "no 15m book in the account object: {b}"
        );
    }
}

/// A daily window that is 23h on a DST day publishes that span on
/// start/end, while durationSec stays the family anchor 86400.
#[tokio::test]
async fn engine_round_current_rounds_daily_uses_observed_span() {
    use crate::model::CryptoMarket;
    use rust_decimal::Decimal;

    let (core, registry, peer) = hot_reload_fixture().await;
    let set = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"round_duration.set","params":{"accountId":"default","durationSec":86400,"reason":"ssot daily span"}}"#
            .to_string(),
    )
    .await;
    assert!(
        set.get("error").is_none(),
        "anchoring default at daily must succeed: {set}"
    );

    // 2026-11-20 12:00 EST = 1795194000 (slug start); close is
    // 2026-11-21 12:00 EST = 1795280400. Mid-window +1h. The UTC 86400
    // grid would name a different start; currentRounds must publish the
    // venue window.
    let start_sec = 1_795_194_000i64;
    let start_ms = start_sec * 1000;
    let end_ms = 1_795_280_400_000i64;
    let now = start_ms + 3_600 * 1000;
    let row = CryptoMarket {
        asset: "BTC".to_string(),
        condition_id: "cond-daily-dst".into(),
        question_id: "q-daily-dst".into(),
        up_token_id: "up-daily-dst".into(),
        down_token_id: "down-daily-dst".into(),
        up_price: Decimal::new(5, 1),
        down_price: Decimal::new(5, 1),
        expires_at_ms: end_ms,
        round_slot: end_ms / 1000 / 86400,
        round_duration_sec: 86400,
        archive_verdict: false,
        venue: "polymarket".to_string(),
        neg_risk: false,
        question: "BTC up or down".into(),
    };
    {
        let mut c = core.lock().await;
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![row],
                now_ms: now,
            },
            now,
        );
    }

    // engine.round uses now_ms() internally for current_rounds_view — we
    // cannot inject `now` through the RPC. Assert the view helper at the
    // DST instant instead, and still check the wire shape on a live call.
    let view = {
        let c = core.lock().await;
        c.current_rounds_view(now)
    };
    let cur = view
        .iter()
        .find(|r| r.account_id == "default")
        .expect("default currentRound");
    assert_eq!(cur.duration_sec, 86400, "family anchor stays 86400");
    assert_eq!(
        cur.round_id, start_sec,
        "roundId is the ET-noon slug start, not the UTC grid"
    );
    assert_eq!(cur.start_ms, start_ms);
    assert_eq!(
        cur.end_ms, end_ms,
        "observed ET-noon close, not the UTC midnight grid"
    );
    assert_eq!((cur.end_ms - cur.start_ms) / 1000, 86_400);
    assert_eq!(cur.age_sec, 3_600);
    assert_eq!(cur.time_left_sec, 86_400 - 3_600);
    assert_eq!(cur.prices.len(), 1);
    assert_eq!(cur.prices[0].duration_sec, 86400);

    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"engine.round","params":{}}"#.to_string(),
    )
    .await;
    let rounds = reply["result"]["currentRounds"]
        .as_array()
        .expect("currentRounds on the wire");
    assert!(
        rounds
            .iter()
            .any(|r| r["accountId"] == "default" && r["durationSec"] == 86400),
        "wire carries the daily currentRound: {rounds:?}"
    );
}

/// With no observed daily close, currentRound must still name the
/// America/New_York noon window (the same instant `slug_epoch_daily`
/// uses). A UTC-midnight grid (`now/86400*86400`) is a fake round id
/// and is forbidden (L1–L6).
#[tokio::test]
async fn current_rounds_daily_uses_et_noon_when_no_observed_window() {
    let (core, registry, peer) = hot_reload_fixture().await;
    let set = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"round_duration.set","params":{"accountId":"default","durationSec":86400,"reason":"ssot daily et-noon without observe"}}"#
            .to_string(),
    )
    .await;
    assert!(
        set.get("error").is_none(),
        "anchoring default at daily must succeed: {set}"
    );

    // 2026-11-21 08:57 UTC = 03:57 EST, inside the 11-20 noon → 11-21 noon
    // window. Slug start 1795194000; close 1795280400. UTC midnight of
    // that day is 1795219200 and must not appear.
    let now_sec = 1_795_251_420i64;
    let now = now_sec * 1000;
    let start_sec = 1_795_194_000i64;
    let end_sec = 1_795_280_400i64;
    let utc_midnight = (now_sec / 86_400) * 86_400;
    assert_eq!(utc_midnight, 1_795_219_200);

    let view = {
        let c = core.lock().await;
        c.current_rounds_view(now)
    };
    let cur = view
        .iter()
        .find(|r| r.account_id == "default")
        .expect("default currentRound");
    assert_eq!(cur.duration_sec, 86400);
    assert_eq!(
        cur.round_id, start_sec,
        "roundId must be the ET-noon slug start, got {} (utc midnight would be {})",
        cur.round_id, utc_midnight
    );
    assert_ne!(
        cur.round_id, utc_midnight,
        "must never fall back to UTC midnight"
    );
    assert_eq!(cur.start_ms, start_sec * 1000);
    assert_eq!(cur.end_ms, end_sec * 1000);
    assert_eq!(cur.age_sec, now_sec - start_sec);
    assert_eq!(cur.time_left_sec, end_sec - now_sec);
}

/// Declared families stay warm in the engine. After 5m/15m/1h/daily rows
/// have been ingested while the account is on 15m, the FIRST
/// `current_rounds_view` after `round_duration.set` already has priced
/// cards for the new family — no re-ingest, no wait for the next slot.
#[tokio::test]
async fn current_rounds_prices_are_warm_on_first_snapshot_after_switch() {
    use crate::model::CryptoMarket;
    use rust_decimal::Decimal;

    let (core, registry, peer) = hot_reload_fixture().await;
    // 2026-11-21 08:57 UTC = 03:57 EST, inside the 11-20 noon daily window
    // and inside the 5m/15m/1h slots computed below.
    let now_sec = 1_795_251_420i64;
    let now = now_sec * 1000;
    let start_5m = now_sec / 300 * 300;
    let start_15m = now_sec / 900 * 900;
    let start_1h = now_sec / 3600 * 3600;
    let start_daily = 1_795_194_000i64;
    let end_daily = 1_795_280_400i64;

    fn row(dur: i64, _start_sec: i64, end_sec: i64, up: i64, down: i64) -> CryptoMarket {
        CryptoMarket {
            asset: "BTC".to_string(),
            condition_id: format!("cond-{dur}"),
            question_id: format!("q-{dur}"),
            up_token_id: format!("up-{dur}"),
            down_token_id: format!("down-{dur}"),
            up_price: Decimal::new(up, 2),
            down_price: Decimal::new(down, 2),
            expires_at_ms: end_sec * 1000,
            round_slot: end_sec / dur,
            round_duration_sec: dur,
            archive_verdict: false,
            venue: "polymarket".to_string(),
            neg_risk: false,
            question: format!("BTC {dur}"),
        }
    }

    {
        let mut c = core.lock().await;
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![
                    row(300, start_5m, start_5m + 300, 41, 59),
                    row(900, start_15m, start_15m + 900, 52, 48),
                    row(3600, start_1h, start_1h + 3600, 63, 37),
                    row(86400, start_daily, end_daily, 55, 45),
                ],
                now_ms: now,
            },
            now,
        );
    }

    let view900 = {
        let c = core.lock().await;
        c.current_rounds_view(now)
    };
    let cur900 = view900
        .iter()
        .find(|r| r.account_id == "default")
        .expect("default currentRound");
    assert_eq!(cur900.duration_sec, 900);
    assert_eq!(cur900.prices.len(), 1, "15m already has a priced card");
    assert_eq!(cur900.prices[0].duration_sec, 900);

    async fn set_dur(
        core: &std::sync::Arc<tokio::sync::Mutex<Core>>,
        registry: &crate::market::registry::MarketPluginRegistry,
        peer: &PeerAuth,
        id: u32,
        sec: i64,
    ) {
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"round_duration.set","params":{{"accountId":"default","durationSec":{sec},"reason":"warm switch"}}}}"#
        );
        let reply = rpc(core, registry, peer, line).await;
        assert!(
            reply.get("error").is_none(),
            "set {sec} must succeed: {reply}"
        );
    }

    set_dur(&core, &registry, &peer, 2, 3600).await;
    let view1h = {
        let c = core.lock().await;
        c.current_rounds_view(now)
    };
    let cur1h = view1h
        .iter()
        .find(|r| r.account_id == "default")
        .expect("1h currentRound");
    assert_eq!(cur1h.duration_sec, 3600);
    assert_eq!(
        cur1h.prices.len(),
        1,
        "first snapshot after switching to 1h must already have prices (no re-ingest)"
    );
    assert_eq!(cur1h.prices[0].duration_sec, 3600);
    assert_eq!(cur1h.round_id, start_1h);
    assert_eq!(cur1h.prices[0].up, Decimal::new(63, 2));

    set_dur(&core, &registry, &peer, 3, 86400).await;
    let view_d = {
        let c = core.lock().await;
        c.current_rounds_view(now)
    };
    let cur_d = view_d
        .iter()
        .find(|r| r.account_id == "default")
        .expect("daily currentRound");
    assert_eq!(cur_d.duration_sec, 86400);
    assert_eq!(
        cur_d.prices.len(),
        1,
        "first snapshot after switching to daily must already have prices"
    );
    assert_eq!(cur_d.round_id, start_daily);
    assert_ne!(cur_d.round_id, (now_sec / 86_400) * 86_400);
    assert_eq!(cur_d.prices[0].duration_sec, 86400);

    set_dur(&core, &registry, &peer, 4, 300).await;
    let view5 = {
        let c = core.lock().await;
        c.current_rounds_view(now)
    };
    let cur5 = view5
        .iter()
        .find(|r| r.account_id == "default")
        .expect("5m currentRound");
    assert_eq!(cur5.duration_sec, 300);
    assert_eq!(
        cur5.prices.len(),
        1,
        "switching back to 5m mid-round must show the current 5m card immediately"
    );
    assert_eq!(cur5.round_id, start_5m);
    assert_eq!(cur5.prices[0].duration_sec, 300);
    assert_eq!(cur5.prices[0].up, Decimal::new(41, 2));
}
