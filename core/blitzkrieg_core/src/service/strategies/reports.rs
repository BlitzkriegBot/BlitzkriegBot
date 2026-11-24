//! Extension registry listing and trade-summary reports
//! (`extension_list`, `trade_summary`, `recent_trades`). Moved verbatim from
//! `service/strategies.rs` (pure code motion).

use super::Core;

impl Core {
    pub fn extension_list(
        &self,
    ) -> Vec<(
        String,
        crate::extension::ExtensionType,
        crate::extension::ExtensionState,
    )> {
        self.extensions.list()
    }

    /// All-time closed-trade summary for the UI (totalTrades / wins / losses /
    /// totalNetPnl / …). Prefers the persisted summary tracked by the trade db;
    /// falls back to totals recomputed over the JSONL (or in-memory history)
    /// when no db is attached.
    ///
    /// `account = None` is the historical all-accounts call, byte-identical to
    /// pre-H behaviour. `account = Some(id)` recomputes the SAME aggregation
    /// over only that account's rows: the persisted summary is a GLOBAL
    /// accumulator (it cannot answer "this account alone"), so an account query
    /// always folds rows — the persisted file stays the all-accounts headline.
    /// Rows predating E28 carry no accountId and read as the default account,
    /// so an account query on a legacy log still sees them.
    pub fn trade_summary(&self, account: Option<&str>) -> serde_json::Value {
        if account.is_none()
            && let Some(db) = self.trade_db.as_ref()
        {
            return serde_json::to_value(db.summary()).unwrap_or(serde_json::Value::Null);
        }
        // Fallback / per-account path: aggregate over whatever history we can
        // reach, filtered to the requested account when one was named. The
        // account comparison goes through the record's serde-defaulted
        // accountId: empty on pre-E28 rows = the default account.
        let want = account.unwrap_or(crate::account::DEFAULT_ACCOUNT_ID);
        let empty = Vec::new();
        // `limit == 0` = 「整份日志」（`trade_db::read_page` 的 no-cap 契约），
        // 因此这一行是 per-account 路径的全史重折——**这条依赖是版本敏感的**：
        // issue #186 / T9 会把 `limit: 0` 重定义成「默认一页」（约 50 行），
        // 到那时这里会静默退化成只折最近 50 行，per-account summary 的
        // totalTrades/net 会凭空缩水且没有任何报错。那个改动落地时必须把本行
        // 改成显式的「要全量」拼写（PageRequest 的 None / 一个 no-cap 常量），
        // 不能沿用 0。
        let all_rows = self.recent_trades(0);
        let rows: Vec<serde_json::Value> = match account {
            None => all_rows,
            Some(_) => all_rows
                .into_iter()
                .filter(|r| {
                    let rid = r
                        .get("accountId")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    // An unattributed (pre-E28) row belongs to the default
                    // account; every other row matches by exact id.
                    if rid.is_empty() {
                        want == crate::account::DEFAULT_ACCOUNT_ID
                    } else {
                        rid == want
                    }
                })
                .collect(),
        };
        let rows = if rows.is_empty() { &empty } else { &rows };
        let mut s = serde_json::json!({
            "totalTrades": 0u64, "wins": 0u64, "losses": 0u64, "flat": 0u64, "winRate": 0.0,
            "attributedGrossPnl": 0.0, "totalFees": 0.0, "attributedNetPnl": 0.0,
            "avgHoldTimeSec": 0.0, "bestTradePnl": 0.0, "worstTradePnl": 0.0,
        });
        let (
            mut wins,
            mut losses,
            mut flats,
            mut gross,
            mut fees,
            mut net,
            mut best,
            mut worst,
            mut holds,
        ) = (
            0u64,
            0u64,
            0u64,
            0.0,
            0.0,
            0.0,
            f64::NEG_INFINITY,
            f64::INFINITY,
            0.0,
        );
        for r in rows.iter() {
            let num = |k: &str| r.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let net_i = num("netPnlUsd");
            // 胜负平归类走唯一判定（`metrics::Outcome`）——成交行是 wire
            // JSON，`netPnlUsd` 在这里已经是 f64，用同口径的 f64 域入口，
            // 不让本出口自己写一遍符号比较。
            match crate::metrics::Outcome::of_net_f64(net_i) {
                crate::metrics::Outcome::Win => wins += 1,
                crate::metrics::Outcome::Flat => flats += 1,
                crate::metrics::Outcome::Loss => losses += 1,
            }
            gross += num("grossPnlUsd");
            fees += num("feesUsd");
            net += net_i;
            best = best.max(net_i);
            worst = worst.min(net_i);
            holds += num("holdTimeSec");
        }
        let decided = wins + losses;
        let n = decided as f64;
        s["totalTrades"] = serde_json::json!(decided + flats);
        s["wins"] = serde_json::json!(wins);
        s["losses"] = serde_json::json!(losses);
        s["flat"] = serde_json::json!(flats);
        // 胜率：小数 0–1，唯一公式（issue #181）。没有任何分出胜负的笔时
        // 报 0.0 —— 这个 JSON 形状的 `winRate` 是非可选字段（老面板按数字
        // 读），「未知」由 `totalTrades`/`flat` 与本键的组合表达，不靠 null。
        s["winRate"] =
            serde_json::json!(crate::metrics::Tally::win_rate_of(wins, losses).unwrap_or(0.0));
        s["attributedGrossPnl"] = serde_json::json!(gross);
        s["totalFees"] = serde_json::json!(fees);
        s["attributedNetPnl"] = serde_json::json!(net);
        s["avgHoldTimeSec"] = serde_json::json!(if n > 0.0 { holds / n } else { 0.0 });
        s["bestTradePnl"] = serde_json::json!(if rows.is_empty() { 0.0 } else { best });
        s["worstTradePnl"] = serde_json::json!(if rows.is_empty() { 0.0 } else { worst });
        s
    }

    /// Recent closed trades (for the UI). Reads the persisted log when present,
    /// else falls back to the in-memory closed positions.
    pub fn recent_trades(&self, limit: usize) -> Vec<serde_json::Value> {
        self.recent_trades_page(limit, 0)
    }

    /// One page of closed trades, counted back from the NEWEST end (#1282).
    ///
    /// `offset` skips the most recent rows (0 = latest), `limit` caps the
    /// window; `limit == 0` = uncapped (whole log at offset=0, everything
    /// older at offset>0) — the same contract `trade_db::read_page` documents.
    /// `recent_trades` is exactly this at offset=0, so the two stay one
    /// surface. The in-memory fallback (no trade log attached) windows
    /// `closed_positions` the same way: newest last, window cut from the end,
    /// so the page contract holds whichever branch serves the rows.
    pub fn recent_trades_page(&self, limit: usize, offset: usize) -> Vec<serde_json::Value> {
        if let Some(path) = self.config.trade_log_path.as_ref() {
            let p = std::path::Path::new(path);
            if p.exists() {
                let rows = crate::trade_db::read_page(p, offset, limit);
                if !rows.is_empty() || offset > 0 {
                    return rows;
                }
                // offset == 0 with an empty log keeps the historical fallback:
                // a fresh boot may hold closed positions in memory that the
                // JSONL has not picked up yet. offset > 0 does NOT fall back —
                // an empty page there means "the page ran past the oldest
                // logged row", and splicing the in-memory window in would
                // corrupt the page contract (a paginator would loop forever).
            }
        }
        let closed = self.positions.closed_positions();
        let end = closed.len().saturating_sub(offset);
        let start = if limit == 0 {
            0
        } else {
            end.saturating_sub(limit)
        };
        closed[start..end]
            .iter()
            .map(|c| {
                serde_json::to_value(crate::trade_db::TradeRecord::from_closed(c))
                    .unwrap_or(serde_json::Value::Null)
            })
            .collect()
    }
}
