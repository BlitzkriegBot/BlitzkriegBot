//! Trade database — persists closed trades in the SAME JSONL format the Node
//! side used (`data/trades/trades.jsonl` plus `summary.json`), so the existing
//! web panel and offline analysis scripts keep working after the core took over
//! trading. Field names and units mirror `src/strategies/crypto-hft/trade-db.ts`,
//! now deleted with the Node source layer (`62b16c88`) — the format is the
//! contract that outlives it.

use crate::position::ClosedPosition;
use crate::store::{MoneyFact, StoreApi, WriteOutcome};
use blitzkrieg_market_api::{AccountId, default_account_id};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Default ledger path per run mode. The trade log is the ONE file dry and
/// live share by name today, which is exactly how a live panel ends up
/// rendering dry-era probe rows: `data/trades/trades.jsonl` holds only dry-run
/// fills in every deployment so far, so the LIVE mode derives a separate
/// default (`trades-live.jsonl`) while DRY/readonly keep the historical
/// spelling — dry tooling (`scripts/dryrun-report.mjs`) and the 36 dry-era rows
/// already on disk stay exactly where they are, no migration. An explicit
/// `--trade-log` still wins over both defaults.
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
    /// write new).
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
    /// Issue #1002 (additive): the fallback close's price basis — `"current"`
    /// (last known mid) or `"entry_fallback"` (dead feed, entry price). Empty
    /// on venue closes and on rows written before the field existed; the
    /// trade-log JSONL is append-only, so serde(default) keeps every
    /// pre-#1002 line loadable without migration.
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

/// Aggregate summary mirroring the Node `summary.json`.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeSummary {
    pub total_trades: u64,
    pub wins: u64,
    pub losses: u64,
    /// 净利恒等于 0 的平仓笔数：既非盈也非亏，不进胜率分母（issue #1277 胜率 single-口径）。
    pub flat: u64,
    /// 胜率，**小数 0–1**（不是百分数）——issue #181 之后 wire 上只有这一种单位。
    /// 公式唯一载体是 [`crate::metrics::Tally::win_rate_of`]：`wins / (wins + losses)`，
    /// 平手不进分母。没有任何分出胜负的笔时这里是 `0.0`：`TradeSummary` 的字段是
    /// 非可选的历史形状，0 是它的「无数据」拼写——要区分「未知」的调用方看
    /// `wins + losses == 0`，不要从 0.0 反推。
    pub win_rate: f64,
    pub attributed_gross_pnl: f64,
    pub total_fees: f64,
    pub attributed_net_pnl: f64,
    pub avg_hold_time_sec: f64,
    pub best_trade_pnl: f64,
    pub worst_trade_pnl: f64,
    pub last_updated: i64,
}

fn f64_of(d: Decimal) -> f64 {
    use std::str::FromStr;
    f64::from_str(&d.to_string()).unwrap_or(0.0)
}

/// A trade log bound to a JSONL path.
pub struct TradeDb {
    jsonl_path: PathBuf,
    summary_path: PathBuf,
    summary: TradeSummary,
    store: Option<Arc<dyn StoreApi>>,
    /// Per-account slices of the same fold (rows carry `account_id`, E28 §9.2).
    /// The global summary mixes every book's realized PnL; the accounting audit
    /// needs ONE book's realized to compare against THAT book's balance — a
    /// sibling account's trade once shifted the audited identity of a book it
    /// never touched. Rebuilt from the JSONL at open, folded alongside the
    /// global summary on record, rebuilt from the kept rows on retract.
    by_account: std::collections::HashMap<AccountId, TradeSummary>,
    /// issue #942: the highest `hft-N` id tail in the history at boot. The
    /// position book's counter seeds past it (restore path), so a flat open
    /// book never reissues ids the closed-trade log already carries.
    max_hft_tail: u64,
}

impl TradeDb {
    /// Open (or create) the log. `jsonl_path` defaults to
    /// `data/trades/trades.jsonl`; the summary sits beside it.
    pub fn new(jsonl_path: impl AsRef<Path>) -> Self {
        let jsonl_path = jsonl_path.as_ref().to_path_buf();
        // The summary belongs to ITS ledger, not to the directory: two
        // mode-split ledgers share `data/trades/`, and both writing
        // `summary.json` would fold each mode's all-time totals into the other
        // mode's panel (the exact cross-contamination this split exists to
        // stop). The historical `trades.jsonl` keeps the Node-compatible
        // `summary.json` name so existing dry-run tooling reads on unchanged;
        // any other ledger (live's `trades-live.jsonl`, a renamed one) gets
        // `<stem>.summary.json` beside it.
        let summary_path = match jsonl_path.file_name() {
            Some(name) if name == "trades.jsonl" => jsonl_path
                .parent()
                .map(|p| p.join("summary.json"))
                .unwrap_or_else(|| PathBuf::from("summary.json")),
            _ => jsonl_path.with_extension("summary.json"),
        };
        let summary = std::fs::read_to_string(&summary_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        // Boot scan: the per-account buckets are derived state (not persisted)
        // — one pass over the JSONL the boot already knows how to read. The
        // same pass records the highest `hft-N` tail ever written here
        // (issue #942): the position book's id counter seeds from the OPEN
        // positions log alone, which resets wholesale on a flat book — a
        // fresh session then reissues ids that already live in THIS file's
        // history (9 duplicate ids observed in production) and the panel
        // rows collide. The closed-trade log is the full-id memory.
        let mut max_hft_tail: u64 = 0;
        let mut by_account: std::collections::HashMap<AccountId, TradeSummary> =
            std::collections::HashMap::new();
        if let Ok(text) = std::fs::read_to_string(&jsonl_path) {
            for line in text.lines() {
                if let Ok(rec) = serde_json::from_str::<TradeRecord>(line) {
                    if let Some(tail) = rec.id.strip_prefix("hft-")
                        && let Ok(n) = tail.parse::<u64>()
                    {
                        max_hft_tail = max_hft_tail.max(n);
                    }
                    let bucket = by_account.entry(rec.account_id.clone()).or_default();
                    Self::fold_into(bucket, &rec);
                }
            }
        }
        Self {
            jsonl_path,
            summary_path,
            summary,
            by_account,
            max_hft_tail,
            store: None,
        }
    }

    /// Wire the shared StoreApi — every recorded trade also lands as a
    /// `MoneyFact::Fill` fact beside the byte-identical JSONL append (R7).
    pub(crate) fn with_store(mut self, store: Option<Arc<dyn StoreApi>>) -> Self {
        self.store = store;
        self
    }

    /// issue #942: the highest `hft-N` tail found in this ledger's closed-trade
    /// history at boot (0 = no rows / no hft-prefixed ids). The position book
    /// seeds its id counter past this on restore so a flat book never reissues
    /// an id the history already carries.
    pub fn max_hft_tail(&self) -> u64 {
        self.max_hft_tail
    }

    /// Append a closed trade and update the summary (best effort, never panics).
    ///
    /// A durable write failure is logged but never interrupts trading: the
    /// in-memory summary is authoritative for the run, and the account keeps
    /// trading. The failure is reflected in `crate::jsonl::append_failures()`
    /// (surfaced via `engine_stats` / `jsonlWriteFailures`).
    pub fn record(&mut self, rec: &TradeRecord, now_ms: i64) {
        if !crate::jsonl::append(&self.jsonl_path, rec) {
            tracing::warn!(
                path = %self.jsonl_path.display(),
                "trade jsonl append failed — trade kept in memory, not durable; trading continues"
            );
        }
        if let Some(ref s) = self.store {
            match s.append_money_fact(&MoneyFact::Fill(rec)) {
                Ok(WriteOutcome::Persisted) => {}
                Ok(outcome) => tracing::warn!(
                    trade_id = %rec.id,
                    "trade money_fact not persisted ({outcome:?}) — JSONL row kept, trading continues"
                ),
                Err(e) => tracing::warn!(
                    trade_id = %rec.id,
                    "trade money_fact append refused — JSONL row kept, store write failed: {e:?}"
                ),
            }
        }
        let bucket = self.by_account.entry(rec.account_id.clone()).or_default();
        Self::fold_into(bucket, rec);
        Self::fold_into(&mut self.summary, rec);
        self.summary.last_updated = now_ms;
        self.persist_summary();
    }

    /// The account's own slice of the fold — the realized/trade-count leg the
    /// accounting audit compares against THAT account's book.
    pub fn summary_for(&self, account: &str) -> TradeSummary {
        self.by_account
            .iter()
            .find(|(k, _)| k.as_str() == account)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    /// The arithmetic shared by the global summary and the per-account buckets
    /// (a bucket fold is exactly the global fold restricted to one account).
    fn fold_into(summary: &mut TradeSummary, rec: &TradeRecord) {
        let net = f64_of(rec.net_pnl_usd);
        summary.total_trades += 1;
        // 胜负平的判定本身也走 [`crate::metrics::Outcome`]：如果这里自己
        // `match net`，那么「什么算平手」就有第二份实现，胜率公式单一只剩
        // 名义上的统一。
        match crate::metrics::Outcome::of_net(rec.net_pnl_usd) {
            crate::metrics::Outcome::Win => summary.wins += 1,
            crate::metrics::Outcome::Loss => summary.losses += 1,
            crate::metrics::Outcome::Flat => summary.flat += 1,
        }
        // 胜率只对「分得胜负」的笔数求比：平手不进分母，不算输也不算赢。
        // 单位是小数 0–1（issue #181：wire 上不许再有第二套单位）；汇总计数器
        // 是 u64，走 u64 版公式而不是先截成 u32。
        summary.win_rate =
            crate::metrics::Tally::win_rate_of(summary.wins, summary.losses).unwrap_or(0.0);
        summary.attributed_gross_pnl += f64_of(rec.gross_pnl_usd);
        summary.total_fees += f64_of(rec.fees_usd);
        summary.attributed_net_pnl += net;
        let n = summary.total_trades as f64;
        summary.avg_hold_time_sec =
            ((summary.avg_hold_time_sec * (n - 1.0)) + rec.hold_time_sec as f64) / n;
        summary.best_trade_pnl = summary.best_trade_pnl.max(net);
        summary.worst_trade_pnl = summary.worst_trade_pnl.min(net);
    }

    fn persist_summary(&self) {
        if let Ok(text) = serde_json::to_string_pretty(&self.summary) {
            // #463: atomic rewrite — a torn summary.json is what the panel
            // reconciles the headline against, not garbage to ignore.
            crate::jsonl::rewrite_atomic(&self.summary_path, text.as_bytes());
        }
    }

    /// Read a JSONL trade log, separating parseable records from lines that no
    /// longer deserialize. Broken lines are returned verbatim so a
    /// read-modify-write cycle never silently destroys audit history
    /// (issue #20). Empty lines are skipped.
    fn read_trade_log(text: &str) -> (Vec<TradeRecord>, Vec<String>) {
        let mut records = Vec::new();
        let mut broken = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<TradeRecord>(line) {
                Ok(rec) => records.push(rec),
                Err(_) => broken.push(line.to_string()),
            }
        }
        (records, broken)
    }

    /// Serialize kept records followed by verbatim broken lines, each on its
    /// own line. Returns `None` if a kept record cannot be serialized.
    fn render_log(records: &[TradeRecord], broken: &[String]) -> Option<String> {
        let mut lines: Vec<String> = Vec::with_capacity(records.len() + broken.len());
        for r in records {
            lines.push(serde_json::to_string(r).ok()?);
        }
        lines.extend_from_slice(broken);
        if lines.is_empty() {
            return Some(String::new());
        }
        Some(lines.join("\n") + "\n")
    }

    /// F4: withdraw a recorded trade whose venue execution later FAILED. The
    /// JSONL is append-only for history, so the retraction REWRITES the file
    /// without the offending line and rebuilds the summary from the remaining
    /// records — a trade that never happened must not live in the books, not
    /// even as a compensating entry the win-rate maths would count.
    ///
    /// Matches the record by (id, exit_time, net_pnl_usd): id alone can repeat
    /// across restarts, and the retraction must not eat a different close.
    /// No-op when no matching line exists (never recorded, already retracted).
    ///
    /// Issue #20: lines that fail to deserialize are preserved verbatim in the
    /// rewritten file — a retract must never silently delete audit history.
    pub fn retract(&mut self, rec: &TradeRecord, now_ms: i64) {
        let Ok(text) = std::fs::read_to_string(&self.jsonl_path) else {
            return;
        };
        let (mut remaining, broken) = Self::read_trade_log(&text);
        let Some(pos) = remaining.iter().position(|r| {
            r.id == rec.id && r.exit_time == rec.exit_time && r.net_pnl_usd == rec.net_pnl_usd
        }) else {
            return;
        };
        remaining.remove(pos);
        let Some(out) = Self::render_log(&remaining, &broken) else {
            return;
        };
        // #463: atomic rewrite — the retract window is exactly when a crash
        // used to be able to truncate the whole trade history.
        if !crate::jsonl::rewrite_atomic(&self.jsonl_path, out.as_bytes()) {
            return;
        }
        // Rebuild the summary — global and per-account buckets — from what is
        // actually on file now.
        self.summary = TradeSummary::default();
        self.by_account.clear();
        for r in &remaining {
            let bucket = self.by_account.entry(r.account_id.clone()).or_default();
            Self::fold_into(bucket, r);
            Self::fold_into(&mut self.summary, r);
        }
        self.summary.last_updated = now_ms;
        self.persist_summary();
    }

    pub fn summary(&self) -> &TradeSummary {
        &self.summary
    }

    /// The bound JSONL path — `reset_trading`'s pre/post self-check counts
    /// lines here (#458).
    pub fn jsonl_path(&self) -> &Path {
        &self.jsonl_path
    }

    /// #458: wipe the trade history — empty JSONL, zeroed summary (written
    /// through). The atomic rewrite keeps a crash mid-reset from leaving a
    /// torn summary behind.
    pub fn reset(&mut self) {
        let _ = crate::jsonl::rewrite_atomic(&self.jsonl_path, b"");
        self.summary = TradeSummary::default();
        self.by_account.clear();
        self.persist_summary();
    }

    /// Issue #991: amend ONE recorded trade's net PnL to venue truth. A
    /// fallback receivable's redemption moves cash by
    /// `venue_truth − booked_estimate`, and the anchored trade-leg identity
    /// demands Δrealized equal that delta — so the RECORD must move with the
    /// ledger, exactly the discipline a #995 receipt correction lives by.
    ///
    /// The amendment rewrites the matched line in place (matched by
    /// `(id, exit_time, net_pnl_usd)` — the same retract triple, so a
    /// reissued id can never eat a different close) and rebuilds the
    /// summaries. The gross/fees legs are re-derived from the new net the way
    /// [`Self::from_closed`] derives them, so the line stays internally
    /// consistent. Returns true when a line was amended.
    pub fn amend_net_pnl(
        &mut self,
        id: &str,
        exit_time: i64,
        booked_net: Decimal,
        delta: Decimal,
        now_ms: i64,
    ) -> bool {
        let Ok(text) = std::fs::read_to_string(&self.jsonl_path) else {
            return false;
        };
        let (mut remaining, broken) = Self::read_trade_log(&text);
        let Some(rec) = remaining
            .iter_mut()
            .find(|r| r.id == id && r.exit_time == exit_time && r.net_pnl_usd == booked_net)
        else {
            return false;
        };
        // Gross carries the same delta (fees are unchanged — the close's fee
        // legs are not venue truth's subject; only the payout was estimated).
        rec.net_pnl_usd += delta;
        rec.gross_pnl_usd += delta;
        let Some(out) = Self::render_log(&remaining, &broken) else {
            return false;
        };
        if !crate::jsonl::rewrite_atomic(&self.jsonl_path, out.as_bytes()) {
            return false;
        }
        // Rebuild the summaries from what is actually on file now (the same
        // rebuild `retract` runs — a fold-delta would drift win/loss counts
        // if the amendment ever crossed the zero line).
        self.summary = TradeSummary::default();
        self.by_account.clear();
        for r in &remaining {
            let bucket = self.by_account.entry(r.account_id.clone()).or_default();
            Self::fold_into(bucket, r);
            Self::fold_into(&mut self.summary, r);
        }
        self.summary.last_updated = now_ms;
        self.persist_summary();
        true
    }

    /// issue 561 账户维重置：只撤 `account` 的成交行 —— 其他账户的行与
    /// 全局 summary 累计值原样保留（重置 B 账户不能抹掉 A 的成交史）。
    /// 语义与 [`Self::retract`] 同族（rewrite_atomic 重写 + 从存留行重建
    /// summary），只是过滤器从「一行」换成「一个账户」：pre-E28 行
    /// （无 accountId）经 serde default 归入 default 账户，重置 default
    /// 时一并撤走，与 [`crate::schema::AccountView`] 的读数口径一致。
    /// 返回撤走的行数（自检/回执用）。
    pub fn retract_account(&mut self, account: &str, now_ms: i64) -> usize {
        let Ok(text) = std::fs::read_to_string(&self.jsonl_path) else {
            return 0;
        };
        let (records, broken) = Self::read_trade_log(&text);
        let (kept, removed): (Vec<TradeRecord>, Vec<TradeRecord>) = records
            .into_iter()
            .partition(|r| r.account_id.as_str() != account);
        if removed.is_empty() {
            return 0;
        }
        // #1253: a reset that leaves no file-level trace of the trades it
        // wipes is an unarchived erasure. The archive is a byte-level copy of
        // the original file (issue #20): damaged rows land in the archive
        // verbatim, not a re-serialized subset.
        let archive_path = self.jsonl_path.with_file_name(format!(
            "{}.reset-archive-{account}-{now_ms}.jsonl",
            self.jsonl_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("trades")
        ));
        if !crate::jsonl::rewrite_atomic(&archive_path, text.as_bytes()) {
            tracing::warn!(
                account = %account,
                "issue 1253: trade-log archive write failed — the rewrite is \
                 refused so the wipe cannot outrun its archive"
            );
            return 0;
        }
        let Some(out) = Self::render_log(&kept, &broken) else {
            return 0;
        };
        // 与 retract 同一原子重写纪律：重置窗口的崩溃不能撕开成交史。
        if !crate::jsonl::rewrite_atomic(&self.jsonl_path, out.as_bytes()) {
            tracing::warn!(
                account = %account,
                "issue 561: account trade-log rewrite failed — the on-disk \
                 rows survive; the summary stays authoritative"
            );
            return 0;
        }
        self.summary = TradeSummary::default();
        self.by_account.clear();
        for r in &kept {
            let bucket = self.by_account.entry(r.account_id.clone()).or_default();
            Self::fold_into(bucket, r);
            Self::fold_into(&mut self.summary, r);
        }
        self.summary.last_updated = now_ms;
        self.persist_summary();
        removed.len()
    }
}

/// Read the last `limit` records from a trades JSONL file (newest last).
/// Used by the IPC history endpoint so the panel can render closed trades.
pub fn read_recent(path: &Path, limit: usize) -> Vec<serde_json::Value> {
    read_page(path, 0, limit)
}

/// One page of the trades JSONL, counted back from the NEWEST end (#1282).
///
/// `offset` skips the most recent rows (offset=0 = latest, offset=200 = the
/// window before the latest 200), `limit` caps the window's length. Rows are
/// returned in file order (oldest → newest), exactly like `read_recent` —
/// only the window differs, so a caller can page the history without the
/// gateway ever having to drain the whole log.
///
/// `limit == 0` means "no cap": with offset=0 that is the whole log (same as
/// `read_recent(0)`), with offset>0 it is everything older than the skipped
/// window, down to the oldest row. The alternative reading (reject limit=0
/// with offset>0) would leave "fetch the remainder" needing a sentinel, and
/// the historical `read_recent(0)` = all-rows contract already fixed 0 as
/// "uncapped" — the page form inherits it rather than inventing a third one.
/// `offset` past the oldest row is an empty page, not an error: a paginator
/// detects the end by an empty/short page either way.
pub fn read_page(path: &Path, offset: usize, limit: usize) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let all: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect();
    let end = all.len().saturating_sub(offset);
    let start = if limit == 0 {
        0
    } else {
        end.saturating_sub(limit)
    };
    all[start..end].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ExitReason, OrderRole, SignalDirection};
    use crate::position::ClosedPosition;

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

    #[test]
    fn record_has_node_compatible_shape() {
        let rec = TradeRecord::from_closed(&closed());
        let v = serde_json::to_value(&rec).unwrap();
        // Fields the panel / analysis scripts read.
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

    #[test]
    fn writes_jsonl_and_updates_summary() {
        let dir = std::env::temp_dir().join(format!("bktrade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("trades.jsonl");
        let mut db = TradeDb::new(&path);
        db.record(&TradeRecord::from_closed(&closed()), 100);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(db.summary().total_trades == 1 && db.summary().wins == 1);
        // Second write appends and accumulates.
        db.record(&TradeRecord::from_closed(&closed()), 200);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
        assert_eq!(db.summary().total_trades, 2);
        // read_recent returns them in order.
        let recent = read_recent(&path, 1);
        assert_eq!(recent.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_routes_fill_through_store_and_keeps_jsonl_summary() {
        let dir = std::env::temp_dir().join(format!("bktrade-money-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("trades.jsonl");
        let (store, writes) = crate::store::test_support::RecordingStore::new();
        let mut db = TradeDb::new(&path).with_store(Some(store.clone()));

        let rec = TradeRecord::from_closed(&closed());
        db.record(&rec, 100);

        let log = writes.lock().unwrap().clone();
        assert!(
            log.iter().any(|s| s == "money:fill"),
            "record must route the fill: {log:?}"
        );
        // R7: JSONL row + summary untouched by the seam.
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
        assert_eq!(db.summary().total_trades, 1);
        assert_eq!(db.summary_for(rec.account_id.as_str()).total_trades, 1);

        // Store is consulted per record; the store handle is shared so the
        // test can observe both writes through one handle.
        let rec2 = TradeRecord {
            id: "hft-2".into(),
            ..rec.clone()
        };
        db.record(&rec2, 200);
        let log = writes.lock().unwrap().clone();
        assert_eq!(
            log.iter().filter(|s| s.as_str() == "money:fill").count(),
            2,
            "each record routes one money fact"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 按账户分槽（E28 §9.2）：兄弟账户的成交不得移动这本账的 realized ——
    /// 审计身份（trade_ledger_identity）曾因全局 summary 把别的账户的盈亏
    /// 算进这本账。record 分桶、retract_account 撤桶、boot 扫描重建，三条
    /// 路径同一断言。
    #[test]
    fn per_account_summaries_isolate_sibling_trades() {
        let dir = std::env::temp_dir().join(format!("bktrade-acct-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("trades.jsonl");
        let mut db = TradeDb::new(&path);
        // default 赢 1.97；poly 输 2.00。
        let mut poly_row = TradeRecord::from_closed(&closed());
        poly_row.id = "poly-1".into();
        poly_row.account_id = AccountId::from("poly");
        poly_row.net_pnl_usd = rust_decimal_macros::dec!(-2);
        db.record(&TradeRecord::from_closed(&closed()), 100);
        db.record(&poly_row, 200);

        let default_view = db.summary_for("default");
        assert_eq!(
            default_view.total_trades, 1,
            "sibling's row leaked into default's slice"
        );
        assert_eq!(default_view.attributed_net_pnl, 1.97);
        let poly_view = db.summary_for("poly");
        assert_eq!(poly_view.total_trades, 1);
        assert_eq!(poly_view.attributed_net_pnl, -2.0);
        // 全局口径仍是两行（面板的合计语义不变）。
        assert_eq!(db.summary().total_trades, 2);

        // 重启重建（boot 扫描）：分桶与全局一致。
        let reopened = TradeDb::new(&path);
        assert_eq!(reopened.summary_for("default").total_trades, 1);
        assert_eq!(reopened.summary_for("poly").total_trades, 1);

        // 账户维重置：撤 poly 的桶，default 原样（issue 561 语义 + 分桶）。
        let mut db2 = reopened;
        db2.retract_account("poly", 300);
        assert_eq!(db2.summary_for("poly").total_trades, 0);
        assert_eq!(db2.summary_for("default").total_trades, 1);
        assert_eq!(db2.summary().total_trades, 1);
        // #1253 + issue #20: the retract archives a byte-level copy of the
        // ORIGINAL file before the rewrite — damaged rows land verbatim, not a
        // re-serialized subset. The pre-reset history stays file-level auditable;
        // the main log carries only the survivor.
        let archive = dir.join("trades.reset-archive-poly-300.jsonl");
        let archived = std::fs::read_to_string(&archive).expect("archive exists");
        assert!(
            archived.contains("\"poly-1\""),
            "the removed row is in the archive"
        );
        // Byte-level copy = every original line (default + poly), not just the
        // removed rows.
        assert_eq!(archived.lines().count(), 2);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().count(),
            1,
            "the main log keeps only default's row"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #trades-mode-split: the DEFAULT ledger path is derived from the run
    /// mode — live derives `trades-live.jsonl`, dry/readonly keep the
    /// historical `trades.jsonl`. The bug this fixes: one shared filename made
    /// the live panel render the dry era's probe rows.
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

    /// A live ledger started BESIDE a populated dry ledger reads none of its
    /// rows: `read_recent` on the live default must come back empty while the
    /// dry file still serves its own rows — the panel verb reads exactly one
    /// path, so the path split IS the isolation.
    #[test]
    fn live_ledger_does_not_read_dry_rows() {
        let dir = std::env::temp_dir().join(format!("bktrade-split-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // The defaults are `data/trades/…` spellings — create the leaf so the
        // fixture's dry row can be planted at the real relative shape.
        std::fs::create_dir_all(dir.join("data/trades")).unwrap();
        let dry_path = dir.join(default_trade_log_path(crate::model::Mode::Dry));
        std::fs::write(
            &dry_path,
            format!(
                "{}\n",
                serde_json::to_string(&TradeRecord::from_closed(&closed())).unwrap()
            ),
        )
        .unwrap();
        let live_path = dir.join(default_trade_log_path(crate::model::Mode::Live));
        // Nothing has traded live yet — the file may not even exist.
        assert!(read_recent(&live_path, 50).is_empty());
        assert!(!read_recent(&dry_path, 50).is_empty());
        // One live trade lands in the LIVE file only; the dry row never
        // crosses over in either direction.
        let mut db = TradeDb::new(&live_path);
        let mut rec = TradeRecord::from_closed(&closed());
        rec.id = "live-1".into();
        db.record(&rec, 100);
        let live_rows = read_recent(&live_path, 50);
        assert_eq!(live_rows.len(), 1);
        assert_eq!(live_rows[0]["id"], "live-1");
        assert_eq!(read_recent(&dry_path, 50).len(), 1);
        // The two summaries stay apart: live's summary file is derived from
        // its own ledger name (sibling of the live jsonl) and folds only the
        // live row; dry's historical summary.json is never opened by the live
        // db, so the dry fold stays wherever the dry tooling left it.
        assert_eq!(db.summary().total_trades, 1);
        let live_summary: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(live_path.with_extension("summary.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(live_summary["totalTrades"], serde_json::json!(1));
        assert!(
            std::fs::read_to_string(dry_path.with_extension("summary.json"))
                .unwrap_or_default()
                .is_empty(),
            "the live db must not write beside the dry ledger"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1277 胜率 single-口径：净利 > 0 为胜，== 0 为平手（不进胜率分母，也不算负），< 0 为负。
    /// 100 笔折叠后 wins + flat + losses = 100；win_rate = wins / (wins + losses)，
    /// 平手笔稀释不进比。单位是小数 0–1（issue #181 统一了 wire 单位）。
    #[test]
    fn win_flat_loss_partitions_hundred_trades() {
        let dir = std::env::temp_dir().join(format!("bktrade-wfl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("trades.jsonl");
        let mut db = TradeDb::new(&path);

        let mut win_row = TradeRecord::from_closed(&closed());
        win_row.id = "wfl-win-1".into();
        win_row.net_pnl_usd = rust_decimal_macros::dec!(1);

        let mut flat_row = TradeRecord::from_closed(&closed());
        flat_row.id = "wfl-flat-1".into();
        flat_row.net_pnl_usd = rust_decimal_macros::dec!(0);

        let mut loss_row = TradeRecord::from_closed(&closed());
        loss_row.id = "wfl-loss-1".into();
        loss_row.net_pnl_usd = rust_decimal_macros::dec!(-1);

        for _ in 0..40 {
            db.record(&win_row, 100);
        }
        for _ in 0..30 {
            db.record(&flat_row, 100);
        }
        for _ in 0..30 {
            db.record(&loss_row, 100);
        }

        let s = db.summary();
        assert_eq!(s.total_trades, 100, "每一笔成交都进总数");
        assert_eq!(s.wins, 40, "净利大于 0 的笔才是胜");
        assert_eq!(s.flat, 30, "净利等于 0 的笔是平手，不进胜也进不了负");
        assert_eq!(s.losses, 30, "净利小于 0 的笔才是负");
        assert_eq!(s.wins + s.flat + s.losses, 100);
        assert!(
            (s.win_rate - (40.0 / 70.0)).abs() < 1e-12,
            "胜率只在分出胜负的笔上求比，平手不进分母；单位是小数 0–1，\
             与 metrics::Tally::win_rate_of 是同一个表达式（issue #181）"
        );

        // summary.json 透传 flat；老报文缺该键时 serde 回落默认 0（向后兼容）。
        let summary: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("summary.json")).unwrap())
                .unwrap();
        assert_eq!(summary["flat"], serde_json::json!(30));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1274: a durable write failure (read-only store) must not silence the
    /// trade — the in-memory summary keeps the trade, the account keeps
    /// trading (not frozen), and the failure counter moves.
    #[test]
    fn record_survives_readonly_store_keeps_memory_and_counter() {
        let dir = std::env::temp_dir().join(format!(
            "bktrade-ro-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("trades.jsonl");

        // Make the directory read-only BEFORE any file exists, so every append fails.
        use std::os::unix::fs::PermissionsExt as _;
        let mut p = std::fs::metadata(&dir).unwrap().permissions();
        p.set_mode(0o500);
        std::fs::set_permissions(&dir, p).unwrap();

        let mut db = TradeDb::new(&path);
        let before = crate::jsonl::append_failures();

        // All writes fail (read-only dir), but the in-memory summary still books the trade.
        db.record(&TradeRecord::from_closed(&closed()), 100);
        assert_eq!(
            db.summary().total_trades,
            1,
            "first trade booked in memory despite write failure"
        );
        assert!(
            crate::jsonl::append_failures() > before,
            "failure counter must move after first failure"
        );

        let mut rec2 = TradeRecord::from_closed(&closed());
        rec2.id = "hft-2".into();
        db.record(&rec2, 200);
        assert_eq!(
            db.summary().total_trades,
            2,
            "second trade still accepted — not frozen"
        );

        let mut rec3 = TradeRecord::from_closed(&closed());
        rec3.id = "hft-3".into();
        db.record(&rec3, 300);
        assert_eq!(
            db.summary().total_trades,
            3,
            "a third trade is still accepted — account keeps trading"
        );

        // Restore permissions for cleanup.
        let mut q = std::fs::metadata(&dir).unwrap().permissions();
        q.set_mode(0o700);
        std::fs::set_permissions(&dir, q).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1282 快照分页：`read_page` 的窗口从**最新端**往回数 —— offset=0 取
    /// 最新 limit 行，offset=200 取再往前 200 行。这是网关 `/api/trades`
    /// 翻页 verb 的取窗契约：翻页器不动文件顺序（最旧→最新），只动窗口。
    #[test]
    fn read_page_windows_count_back_from_the_newest_end() {
        let dir = std::env::temp_dir().join(format!("bktrade-page-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("trades.jsonl");
        let mut db = TradeDb::new(&path);
        for i in 1..=10i64 {
            let mut rec = TradeRecord::from_closed(&closed());
            rec.id = format!("p-{i}");
            db.record(&rec, i * 100);
        }
        let ids = |rows: &[serde_json::Value]| -> Vec<String> {
            rows.iter()
                .map(|r| r["id"].as_str().unwrap_or("").to_string())
                .collect()
        };
        // offset=0 → 最新 3 行，顺序仍是文件序（最旧在前）。
        assert_eq!(ids(&read_page(&path, 0, 3)), vec!["p-8", "p-9", "p-10"]);
        // offset=3 → 再往前 3 行。
        assert_eq!(ids(&read_page(&path, 3, 3)), vec!["p-5", "p-6", "p-7"]);
        // limit 窗口长于剩余行 → 一路取到最旧端为止。
        assert_eq!(
            ids(&read_page(&path, 3, 200)),
            vec!["p-1", "p-2", "p-3", "p-4", "p-5", "p-6", "p-7"]
        );
        // read_recent 原签名与 offset=0 的 read_page 逐行相同（老调用点
        // trading_reset 依赖这个等价）。
        assert_eq!(read_recent(&path, 3), read_page(&path, 0, 3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1282: offset 翻过最旧一行 = 空页（不是错误）—— 翻页器靠空页判底；
    /// limit=0 语义 = 不设上限（继承 read_recent(0)=全量的既有契约），
    /// offset>0 时即「从被跳过的窗口一路取到最旧行」。
    #[test]
    fn read_page_offset_past_the_oldest_row_is_empty_and_limit_zero_uncaps() {
        let dir = std::env::temp_dir().join(format!("bktrade-page2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("trades.jsonl");
        let mut db = TradeDb::new(&path);
        for i in 1..=5i64 {
            let mut rec = TradeRecord::from_closed(&closed());
            rec.id = format!("q-{i}");
            db.record(&rec, i * 100);
        }
        // 越界：空页。
        assert!(read_page(&path, 5, 3).is_empty());
        assert!(read_page(&path, 99, 3).is_empty());
        // limit=0 + offset=0 = 全量（与 read_recent(0) 同一契约）。
        assert_eq!(read_page(&path, 0, 0).len(), 5);
        assert_eq!(read_page(&path, 0, 0), read_recent(&path, 0));
        // limit=0 + offset>0 = 取到底（跳过最新 offset 行后一路取到最旧）。
        assert_eq!(read_page(&path, 2, 0).len(), 3);
        assert_eq!(read_page(&path, 2, 0)[0]["id"], "q-1");
        // limit=0 的窗口里不含被跳过的最新行。
        assert_eq!(read_page(&path, 2, 0)[2]["id"], "q-3");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #20: `retract` must preserve lines that fail to deserialize — a
    /// damaged row must never be silently deleted from the trade history.
    #[test]
    fn retract_preserves_unparseable_line() {
        let dir =
            std::env::temp_dir().join(format!("bktrade-retract-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trades.jsonl");

        let mut r1 = TradeRecord::from_closed(&closed());
        r1.id = "a".into();
        let mut r2 = TradeRecord::from_closed(&closed());
        r2.id = "b".into();
        let mut r3 = TradeRecord::from_closed(&closed());
        r3.id = "c".into();

        let mut text = String::new();
        text.push_str(&serde_json::to_string(&r1).unwrap());
        text.push('\n');
        text.push_str("{\"broken\"");
        text.push('\n');
        text.push_str(&serde_json::to_string(&r2).unwrap());
        text.push('\n');
        text.push_str(&serde_json::to_string(&r3).unwrap());
        text.push('\n');
        std::fs::write(&path, &text).unwrap();

        let mut db = TradeDb::new(&path);
        db.retract(&r2, 400);

        let final_text = std::fs::read_to_string(&path).unwrap();
        let final_lines: Vec<&str> = final_text.lines().collect();
        assert_eq!(
            final_lines.len(),
            3,
            "two valid records + the broken line must remain"
        );
        assert!(
            final_lines.iter().any(|l| l.contains("broken")),
            "the damaged line must be preserved"
        );
        assert!(
            final_lines.iter().any(|l| l.contains("\"id\":\"a\"")),
            "r1 must remain"
        );
        assert!(
            final_lines.iter().any(|l| l.contains("\"id\":\"c\"")),
            "r3 must remain"
        );
        assert!(
            !final_lines.iter().any(|l| l.contains("\"id\":\"b\"")),
            "the retracted record must be gone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #20: `retract_account` must preserve unparseable lines in BOTH
    /// the archive (byte-level copy) and the rewritten main log.
    #[test]
    fn retract_account_preserves_unparseable_line() {
        let dir = std::env::temp_dir().join(format!(
            "bktrade-retract-acct-broken-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trades.jsonl");

        let mut r1 = TradeRecord::from_closed(&closed());
        r1.id = "a".into();
        r1.account_id = AccountId::from("default");
        let mut r2 = TradeRecord::from_closed(&closed());
        r2.id = "b".into();
        r2.account_id = AccountId::from("poly");

        let mut text = String::new();
        text.push_str(&serde_json::to_string(&r1).unwrap());
        text.push('\n');
        text.push_str("{\"broken\"");
        text.push('\n');
        text.push_str(&serde_json::to_string(&r2).unwrap());
        text.push('\n');
        std::fs::write(&path, &text).unwrap();

        let mut db = TradeDb::new(&path);
        let n = db.retract_account("poly", 400);
        assert_eq!(n, 1, "poly's record was retracted");

        // Main log: r1 + broken line (r2 gone).
        let main_text = std::fs::read_to_string(&path).unwrap();
        let main_lines: Vec<&str> = main_text.lines().collect();
        assert_eq!(main_lines.len(), 2, "main log keeps r1 + broken line");
        assert!(
            main_lines.iter().any(|l| l.contains("broken")),
            "broken line preserved in main log"
        );
        assert!(
            main_lines.iter().any(|l| l.contains("\"id\":\"a\"")),
            "r1 remains in main log"
        );
        assert!(
            !main_lines.iter().any(|l| l.contains("\"id\":\"b\"")),
            "r2 removed from main log"
        );

        // Archive: byte-level copy of the ORIGINAL file (r1 + broken + r2).
        let archive = dir.join("trades.reset-archive-poly-400.jsonl");
        let archived = std::fs::read_to_string(&archive).expect("archive exists");
        let archived_lines: Vec<&str> = archived.lines().collect();
        assert_eq!(
            archived_lines.len(),
            3,
            "archive is a byte-level copy of the original"
        );
        assert!(
            archived_lines.iter().any(|l| l.contains("broken")),
            "broken line preserved in archive"
        );
        assert!(
            archived_lines.iter().any(|l| l.contains("\"id\":\"a\"")),
            "r1 in archive"
        );
        assert!(
            archived_lines.iter().any(|l| l.contains("\"id\":\"b\"")),
            "r2 in archive"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #20: when `rewrite_atomic` fails, `retract` must leave the file
    /// byte-identical (no partial rewrite, no silent data loss).
    #[test]
    fn retract_failed_rewrite_leaves_file_unchanged() {
        let dir = std::env::temp_dir().join(format!("bktrade-retract-ro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trades.jsonl");

        let mut r1 = TradeRecord::from_closed(&closed());
        r1.id = "a".into();
        let mut r2 = TradeRecord::from_closed(&closed());
        r2.id = "b".into();
        let text = format!(
            "{}\n{}\n",
            serde_json::to_string(&r1).unwrap(),
            serde_json::to_string(&r2).unwrap(),
        );
        std::fs::write(&path, &text).unwrap();

        // Make the directory read-only so the atomic rewrite's temp file
        // cannot be created.
        use std::os::unix::fs::PermissionsExt as _;
        let mut p = std::fs::metadata(&dir).unwrap().permissions();
        p.set_mode(0o500);
        std::fs::set_permissions(&dir, p).unwrap();

        let mut db = TradeDb::new(&path);
        db.retract(&r1, 400);

        let final_text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            final_text, text,
            "a failed rewrite must not damage the previous generation"
        );

        // Restore permissions for cleanup.
        let mut q = std::fs::metadata(&dir).unwrap().permissions();
        q.set_mode(0o700);
        std::fs::set_permissions(&dir, q).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
