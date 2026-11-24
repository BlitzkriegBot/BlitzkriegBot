use super::prelude::*;
use crate::schema::*;

// The `trades` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

/// Page size when `limit` is absent — the historical default, unchanged.
pub(crate) const TRADES_DEFAULT_LIMIT: usize = 50;

/// Ceiling for one `trades.history` reply (#186). The trade log is
/// append-only, so a reply with no ceiling grows for the life of the process;
/// a page has to stay a page. Same order of magnitude as the two sibling verbs
/// that already bound themselves: `account.journal` clamps to 500,
/// `kline.history` to 1,000.
pub(crate) const TRADES_MAX_LIMIT: usize = 1000;

/// Resolve a `trades.history` `limit` (#186).
///
/// Absent and `0` both mean "no page size was asked for" and get the default
/// page. That second case is the whole point: `0` used to travel through to
/// `trade_db::read_page`, where it means **uncapped** — one `usize` field left
/// at its initializer turned every snapshot into a read of the entire log. A
/// zero is not a page size, and it must never buy an unbounded read.
///
/// Values above the ceiling are clamped rather than refused, matching the
/// sibling verbs (`account.journal`, `kline.history`): an over-large ask gets
/// a bounded answer, and no existing caller has to learn a new error code.
pub(crate) fn trades_page_limit(params: &Value) -> usize {
    match params.get("limit").and_then(|v| v.as_u64()) {
        None | Some(0) => TRADES_DEFAULT_LIMIT,
        Some(n) => n.min(TRADES_MAX_LIMIT as u64) as usize,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_trades(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::TRADES_HISTORY => {
            let limit = trades_page_limit(&params);
            // #1282: the optional page window, counted back from the newest
            // end (0 = latest). Absent = 0, which is byte-identical to the
            // pre-#1282 wire: every existing caller sends `limit` only and
            // keeps its exact response shape. Unlike `limit`, a huge `offset`
            // needs no ceiling: it walks past the oldest row and yields an
            // empty page, which is the page contract's own end-of-log signal.
            let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let trades = core.lock().await.recent_trades_page(limit, offset);
            Ok(serde_json::json!({
                "version": crate::schema::PROTOCOL_VERSION,
                "trades": trades,
            }))
        }
        method::TRADES_SUMMARY => {
            let account = params.get("accountId").and_then(|v| v.as_str());
            let summary = core.lock().await.trade_summary(account);
            Ok(serde_json::json!({
                "version": crate::schema::PROTOCOL_VERSION,
                "summary": summary,
            }))
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}
