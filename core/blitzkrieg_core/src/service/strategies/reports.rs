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
        // The carrier path: one SQL aggregate per scope, no row folding in
        // kernel memory. This replaces BOTH the persisted-summary read (global)
        // and the per-account JSONL re-fold (which read the whole log to count
        // it — the version-sensitive dependency the old comment here warned
        // about).
        if let Some(log) = self.trade_log.as_ref() {
            use crate::contracts::trades::{TradeRead, TradeScope};
            let scope = match account {
                None => TradeScope::AllAccounts,
                Some(id) => TradeScope::Account(crate::contracts::store::AccountScope {
                    account: &crate::model::AccountId::from(id),
                }),
            };
            match log.trade_totals(scope) {
                Ok(t) => return trade_totals_to_wire(&t),
                Err(e) => {
                    // An unreadable ledger must not render as a zeroed book:
                    // "no data" and "cannot read" look identical to an operator
                    // and only one of them is a bug (R7.1 L4).
                    tracing::error!(
                        account = account.unwrap_or("<all>"),
                        error = ?e,
                        "trade totals unavailable — reporting the summary as unknown"
                    );
                    return serde_json::Value::Null;
                }
            }
        }
        // No store wired (backtester / in-process tests): aggregate over the
        // in-memory closed positions. This is a REAL source — the same closes
        // the write path would have persisted — not a fabricated fallback.
        //
        // The account comparison goes through the record's serde-defaulted
        // accountId: empty on pre-E28 rows = the default account.
        let want = account.unwrap_or(crate::account::DEFAULT_ACCOUNT_ID);
        let empty = Vec::new();
        // `limit == 0` is this path's existing "the whole set" spelling; it feeds
        // only the in-memory window below, never a carrier read.
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
        // `recent_trades_page` expresses "unreadable" as `None`; this convenience
        // entry point collapses it to an empty page. It is only used by the
        // in-memory fallback path (never fails) and by tests — the IPC
        // trade-history handler calls `recent_trades_page` directly so it can
        // emit `null` on the wire instead of a fabricated `[]` (L4).
        self.recent_trades_page(limit, 0).unwrap_or_default()
    }

    /// One page of closed trades, counted back from the NEWEST end (#1282).
    ///
    /// `offset` skips the most recent rows (0 = latest), `limit` caps the
    /// window; `limit == 0` = uncapped (the whole set at offset=0, everything
    /// older at offset>0). `recent_trades` is exactly this at offset=0, so the
    /// two stay one surface.
    ///
    /// The `limit == 0` case is spelled out rather than passed through: the
    /// carrier's read takes a `NonZeroU32` precisely so "unbounded" is
    /// unrepresentable. Here the page size is derived from the carrier's own
    /// count (`total − offset`), so an uncapped request is a bounded read of a
    /// known size — not an accidental whole-file load.
    pub fn recent_trades_page(
        &self,
        limit: usize,
        offset: usize,
    ) -> Option<Vec<serde_json::Value>> {
        if let Some(log) = self.trade_log.as_ref() {
            use crate::contracts::trades::{TradeRead, TradeScope};
            let total = match log.trade_totals(TradeScope::AllAccounts) {
                Ok(t) => t.total_trades,
                Err(e) => {
                    // A ledger we cannot count is not an empty ledger. Returning
                    // an empty page would make a read failure look like "no
                    // trades", which is the silent-degradation pattern (L4).
                    // Emit `None` so the IPC trade-history handler can render
                    // `"trades": null` — "unreadable", distinguishable from an
                    // honest empty page.
                    tracing::error!(error = ?e, "trade page count unavailable");
                    return None;
                }
            };
            let end = total.saturating_sub(offset as u64);
            if end == 0 {
                // Past the oldest row: an empty page is the paginator's
                // end-of-log signal, not an error.
                return Some(Vec::new());
            }
            let window = if limit == 0 {
                end
            } else {
                (limit as u64).min(end)
            };
            let window = u32::try_from(window).unwrap_or(u32::MAX);
            let Some(window) = std::num::NonZeroU32::new(window) else {
                return Some(Vec::new());
            };
            return match log.load_trades(TradeScope::AllAccounts, offset as u64, window) {
                Ok(rows) => Some(
                    rows.iter()
                        .map(|r| serde_json::to_value(r).unwrap_or(serde_json::Value::Null))
                        .collect(),
                ),
                Err(e) => {
                    tracing::error!(error = ?e, "trade page read failed");
                    None
                }
            };
        }
        // No store wired: window the in-memory closed positions the same way
        // (newest last, window cut from the end), so the page contract holds
        // whichever branch serves the rows.
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
                serde_json::to_value(crate::trade::TradeRecord::from_closed(c))
                    .unwrap_or(serde_json::Value::Null)
            })
            .collect::<Vec<_>>()
            .into()
    }
}

/// Render [`crate::contracts::trades::TradeTotals`] as the `trades.summary`
/// wire object.
///
/// # Why this adapter exists
///
/// The wire shape is FROZEN for this change: the panel reads these exact keys,
/// so re-spelling them here would be a second, hidden change smuggled into a
/// carrier swap. Every key below is the one `TradeSummary` emits today.
///
/// # What it deliberately changes
///
/// Only the two places where the OLD producer emitted a fabricated number:
///
/// * `bestTradePnl` / `worstTradePnl` for an account with no trades were `0.0`
///   from `TradeSummary::default()`, so an all-losing account reported a best
///   trade of `0.00` — a trade it never made. The carrier reports `None`, and
///   `null` is the honest spelling. A consumer that renders `null` as `0.00`
///   reintroduces the defect on its own side, which is why the panel's
///   formatter treats `null` as 「未知」.
/// * `winRate` stays `0.0` when nothing is decided because the field is
///   non-optional on the wire (see `TradeSummary::win_rate`'s doc): the
///   「unknown」 signal is `wins + losses == 0`, which the carrier exposes
///   through the tally. Emitting `null` here would break every existing
///   reader, so the spelling is preserved and the ambiguity documented.
fn trade_totals_to_wire(t: &crate::contracts::trades::TradeTotals) -> serde_json::Value {
    fn f64_of(d: &rust_decimal::Decimal) -> f64 {
        use std::str::FromStr;
        f64::from_str(&d.to_string()).unwrap_or(0.0)
    }
    let win_rate = t.tally.win_rate().unwrap_or(0.0);
    serde_json::json!({
        "totalTrades": t.total_trades,
        "wins": u64::from(t.tally.wins),
        "losses": u64::from(t.tally.losses),
        "flat": u64::from(t.tally.flats),
        "winRate": win_rate,
        "attributedGrossPnl": f64_of(&t.gross_pnl),
        "totalFees": f64_of(&t.fees),
        "attributedNetPnl": f64_of(&t.net_pnl),
        // Mean over ALL closed trades (the contract's one answer). `null` when
        // there are no rows, never 0.0.
        "avgHoldTimeSec": t.avg_hold_time_sec,
        "bestTradePnl": t.best_trade_pnl.as_ref().map(f64_of),
        "worstTradePnl": t.worst_trade_pnl.as_ref().map(f64_of),
        "lastUpdated": t.as_of_ms,
        // R7.1「未核实敞口」单列: the unverified slice of the numbers above.
        // A SUBSET of attributedNetPnl, never something a consumer subtracts to
        // get a prettier headline.
        "unverifiedTrades": t.unverified.as_ref().map(|u| u.trades),
        "unverifiedNetPnl": t.unverified.as_ref().map(|u| f64_of(&u.net_pnl)),
    })
}
