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
            match net_i.total_cmp(&0.0) {
                std::cmp::Ordering::Greater => wins += 1,
                std::cmp::Ordering::Equal => flats += 1,
                std::cmp::Ordering::Less => losses += 1,
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
        s["winRate"] = serde_json::json!(if n > 0.0 {
            wins as f64 * 100.0 / n
        } else {
            0.0
        });
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
        if let Some(path) = self.config.trade_log_path.as_ref() {
            let rows = crate::trade_db::read_recent(std::path::Path::new(path), limit);
            if !rows.is_empty() {
                return rows;
            }
        }
        let closed = self.positions.closed_positions();
        let start = if limit > 0 && closed.len() > limit {
            closed.len() - limit
        } else {
            0
        };
        closed[start..]
            .iter()
            .map(|c| {
                serde_json::to_value(crate::trade_db::TradeRecord::from_closed(c))
                    .unwrap_or(serde_json::Value::Null)
            })
            .collect()
    }
}
