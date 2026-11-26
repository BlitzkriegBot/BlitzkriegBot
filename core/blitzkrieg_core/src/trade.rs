//! The closed-trade ROW — the record type and the JSONL export spelling.
//!
//! # Why this module is not `trade_db` any more
//!
//! Until schema v4 this file's predecessor (`trade_db.rs`) also CARRIED the
//! ledger: `TradeDb` appended JSONL lines, kept a `summary.json` beside them,
//! and rebuilt every total by re-folding the whole file in memory. The store
//! seam replaced all of that:
//!
//! | concern | old carrier | now |
//! |---|---|---|
//! | durable row | `trades.jsonl` append | `trades` table, [`crate::db::trades::TradeLog`] |
//! | totals | `TradeSummary` fold over the file | one SQL aggregate (`trade_totals`) |
//! | page read | `read_page` (whole-file read) | `load_trades` (indexed window) |
//! | mutation | rewrite the file + re-fold everything | `DELETE` / `UPDATE` in a transaction |
//!
//! What survives here is exactly the part that is still true: the ROW SHAPE.
//! The panel, the analysis scripts and the export format all read these
//! camelCase keys, so the type is the contract that outlives every carrier
//! (field names and units mirror the retired Node `trade-db.ts`, deleted with
//! the Node source layer at `62b16c88`).
//!
//! # JSONL is an export format, not a ledger
//!
//! Nothing in the kernel reads a trade from a file on the boot/query path any
//! more. [`crate::db::trades::TradeLog::export_jsonl`] writes one for offline
//! tooling, and [`crate::db::trades::TradeLog::import_jsonl`] ingests a legacy
//! one — refusing the WHOLE file when any row collides, because two different
//! closes at the same `(account, id, exit_time)` are an operator decision, not
//! something a loader may silently resolve.
//!
//! [`default_trade_log_path`] still answers the historical spelling so the
//! export lands where the dry-era tooling already looks.

use crate::position::ClosedPosition;
use blitzkrieg_market_api::{AccountId, default_account_id};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Default export path per run mode. The trade log is the ONE file dry and
/// live shared by name in the JSONL era, which is exactly how a live panel
/// ended up rendering dry-era probe rows; that defect is now closed by `mode`
/// being part of the SQL row identity, and this function only names where an
/// EXPORT lands. LIVE derives a separate spelling (`trades-live.jsonl`) so an
/// operator who exports both books never overwrites one with the other, while
/// DRY/readonly keep the historical path dry tooling
/// (`scripts/dryrun-report.mjs`) and the 36 dry-era rows already on disk
/// expect. An explicit `--trade-log` still wins over both defaults.
pub fn default_trade_log_path(mode: crate::model::Mode) -> &'static str {
    if mode.may_trade() {
        "data/trades/trades-live.jsonl"
    } else {
        "data/trades/trades.jsonl"
    }
}

/// One closed trade, wire-compatible with the Node `TradeRecord`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeRecord {
    pub id: String,
    pub strategy: String,
    pub asset: String,
    pub direction: String,
    pub token_id: String,
    pub condition_id: String,
    /// E28 (§9.2): the account the trade settled in. A row written before E28
    /// carries no `accountId` and reads as `default` (serde default — no
    /// migration); every new record writes the field explicitly (read old,
    /// write new). In SQL this column is part of the row address.
    #[serde(default = "default_account_id")]
    pub account_id: AccountId,
    #[serde(with = "crate::decimal")]
    pub entry_price: Decimal,
    #[serde(with = "crate::decimal")]
    pub exit_price: Decimal,
    #[serde(with = "crate::decimal")]
    pub shares: Decimal,
    #[serde(with = "crate::decimal")]
    pub cost_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub gross_pnl_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub entry_fee_pct: Decimal,
    #[serde(with = "crate::decimal")]
    pub exit_fee_pct: Decimal,
    #[serde(with = "crate::decimal")]
    pub fees_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub net_pnl_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub net_pnl_pct: Decimal,
    pub hold_time_sec: i64,
    /// Milliseconds since epoch (matches the Node record).
    pub entry_time: i64,
    pub exit_time: i64,
    pub exit_reason: String,
    pub was_maker_entry: bool,
    pub was_maker_exit: bool,
    #[serde(with = "crate::decimal")]
    pub market_price_at_entry: Decimal,
    #[serde(with = "crate::decimal")]
    pub market_price_at_exit: Decimal,
    #[serde(with = "crate::decimal")]
    pub high_pnl_pct: Decimal,
    #[serde(with = "crate::decimal")]
    pub low_pnl_pct: Decimal,
    /// #427: the venue the trade's market listed on. A row written before the
    /// field existed reads as undeclared (serde default — no migration).
    #[serde(default)]
    pub venue: String,
    /// Provenance of the close: `venue` when the fill price came from a
    /// venue-backed verdict (settlement or reconcile backfill), `fallback`
    /// when the close price is a bookkeeping value with no venue answer
    /// (unverifiable expiry). Never fabricated, never merged into realized
    /// PnL as if it were a venue verdict — see issue #987.
    #[serde(default)]
    pub source: String,
    /// #987: when a fallback close is later overwritten by a venue verdict
    /// (reconcile backfill), this holds the ORIGINAL fallback exit price so
    /// auditors can see what was booked before the venue's truth arrived.
    #[serde(default, with = "crate::decimal::opt")]
    pub original_fallback_price: Option<Decimal>,
    /// Issue #1002 / #176 (additive): the fallback close's price basis —
    /// `"binary_threshold"` (a real book observation thresholded into the
    /// binary `{0, 1}` domain) or `"unobserved"` (no quote ever arrived, the
    /// booked price is 0 and the notional rides the receivable pool). Issue
    /// #176 deleted the `"entry_fallback"` basis itself. Empty on venue closes
    /// and on rows written before the field existed; serde(default) keeps
    /// every pre-#1002 exported line loadable without migration.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub price_source: String,
}

impl TradeRecord {
    /// Build from a closed position (no I/O).
    pub fn from_closed(c: &ClosedPosition) -> Self {
        let entry_fee_usd = (c.entry_fee_pct / Decimal::ONE_HUNDRED) * c.entry_price * c.shares;
        let exit_fee_usd = (c.exit_fee_pct / Decimal::ONE_HUNDRED) * c.exit_price * c.shares;
        // The old Node record derived gross from the same fee-aware formula.
        let gross = c.net_pnl_usd + entry_fee_usd + exit_fee_usd;
        Self {
            id: c.id.clone(),
            strategy: c.strategy.clone(),
            asset: c.asset.clone(),
            direction: c.direction.as_str().to_string(),
            token_id: c.token_id.clone(),
            condition_id: c.condition_id.clone(),
            account_id: c.account_id.clone(),
            entry_price: c.entry_price,
            exit_price: c.exit_price,
            shares: c.shares,
            cost_usd: c.cost_usd,
            gross_pnl_usd: gross,
            entry_fee_pct: c.entry_fee_pct,
            exit_fee_pct: c.exit_fee_pct,
            fees_usd: entry_fee_usd + exit_fee_usd,
            net_pnl_usd: c.net_pnl_usd,
            net_pnl_pct: c.net_pnl_pct,
            hold_time_sec: c.hold_time_sec,
            entry_time: c.entered_at_ms,
            exit_time: c.exited_at_ms,
            // ExitReason derives serde(snake_case); use it so reasons match the
            // Node engine's values exactly (trailing_stop, take_profit, ...).
            exit_reason: serde_json::to_value(c.exit_reason)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| format!("{:?}", c.exit_reason).to_lowercase()),
            was_maker_entry: c.was_maker_entry,
            was_maker_exit: c.was_maker_exit,
            market_price_at_entry: c.entry_price,
            market_price_at_exit: c.exit_price,
            high_pnl_pct: c.high_pnl_pct,
            low_pnl_pct: c.low_pnl_pct,
            venue: c.venue.clone(),
            source: c.source.clone(),
            original_fallback_price: c.original_fallback_price,
            price_source: c.price_source.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ExitReason, OrderRole, SignalDirection};

    fn closed() -> ClosedPosition {
        ClosedPosition {
            id: "hft-1".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: SignalDirection::Up,
            token_id: "tok".into(),
            condition_id: "cond".into(),
            entry_price: rust_decimal_macros::dec!(0.40),
            exit_price: rust_decimal_macros::dec!(0.60),
            shares: rust_decimal_macros::dec!(10),
            cost_usd: rust_decimal_macros::dec!(4),
            was_maker_entry: true,
            was_maker_exit: false,
            entry_fee_pct: Decimal::ZERO,
            exit_fee_pct: rust_decimal_macros::dec!(0.5),
            maker_rebate_usd: Decimal::ZERO,
            pnl_usd: rust_decimal_macros::dec!(2),
            pnl_pct: rust_decimal_macros::dec!(50),
            net_pnl_usd: rust_decimal_macros::dec!(1.97),
            net_pnl_pct: rust_decimal_macros::dec!(49.25),
            high_pnl_pct: rust_decimal_macros::dec!(52),
            low_pnl_pct: rust_decimal_macros::dec!(-3),
            hold_time_sec: 64,
            exit_reason: ExitReason::TakeProfit,
            entered_at_ms: 1000,
            exited_at_ms: 65000,
            entry_role: OrderRole::Maker,
            exit_role: OrderRole::Taker,
            dust_shares: Decimal::ZERO,
            account_id: crate::model::default_account_id(),
            venue: String::new(),
            source: String::new(),
            original_fallback_price: None,
            price_source: String::new(),
        }
    }

    /// The wire shape the panel and the analysis scripts read. This used to
    /// live in `trade_db.rs` beside the fold that produced it; the fold is
    /// gone (SQL aggregates it now), the shape is the surviving contract.
    #[test]
    fn record_has_node_compatible_shape() {
        let rec = TradeRecord::from_closed(&closed());
        let v = serde_json::to_value(&rec).unwrap();
        for k in [
            "id",
            "strategy",
            "asset",
            "direction",
            "entryPrice",
            "exitPrice",
            "netPnlUsd",
            "netPnlPct",
            "exitReason",
            "entryTime",
            "exitTime",
            "holdTimeSec",
        ] {
            assert!(v.get(k).is_some(), "missing field {k}");
        }
        assert_eq!(v["direction"], "up");
        assert_eq!(v["exitReason"], "take_profit");
        assert!(v["netPnlUsd"].as_f64().unwrap() > 1.9);
    }

    /// The export destination keeps the mode split the JSONL carrier needed,
    /// so exporting a live book never overwrites a dry book's file.
    #[test]
    fn default_trade_log_path_splits_by_mode() {
        assert_eq!(
            default_trade_log_path(crate::model::Mode::Live),
            "data/trades/trades-live.jsonl"
        );
        assert_eq!(
            default_trade_log_path(crate::model::Mode::Dry),
            "data/trades/trades.jsonl"
        );
        assert_eq!(
            default_trade_log_path(crate::model::Mode::ReadOnly),
            "data/trades/trades.jsonl"
        );
    }
}
