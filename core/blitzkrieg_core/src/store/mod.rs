//! The storage seam (issue #1199): the kernel's write paths persist
//! through [`StoreApi`] instead of naming `.env` files and env keys
//! directly. 操作者裁决 2026-11-14（第六轮）：`.env` 对内核只读，唯一的
//! 后端是 [`SqliteStore`]（`config.db_path` 打开）；`.env` 后端已整体
//! 删除——任何写回 `.env` 的代码都是违约。
//!
//! Why a trait on `Core` (`Option<Box<dyn StoreApi>>`) and not an enum:
//! the implementation set is one backend (SqliteStore) — SQLite is the sole
//! durable carrier now — and the dispatch never needs to inspect a variant —
//! call sites stay blind to the carrier, which is the whole point of the ruling.
//!
//! `None` = persistence disabled for this core (backtester, most
//! in-process tests) — the same honest refusal the writers gave when their
//! path pair was `None`, now decided once at assembly instead of per call.

mod sqlite;

pub use sqlite::SqliteStore;

use rust_decimal::Decimal;
use std::collections::BTreeMap;
use std::fmt::Debug;

use crate::account::config::CrossVenueArbConfig;
use crate::account::{AccountId, AccountStatus, CredentialKeys};
use crate::model::{CoreError, CoreErrorCode};

/// One domain write's field→value changes, in the wire audit's camelCase
/// spelling (`maxSingleLossUsd`, `stopLossPct`, …). The store maps them to
/// its carrier's key grammar; the service layer only ever speaks fields.
pub type FieldChanges = Vec<(String, String)>;

/// The durable face of every cockpit write (issue #1199).
///
/// Domain-grouped, carrier-neutral: no method name or type mentions `.env`,
/// keys or files. One method = one domain write; composite invariants
/// (plan-first validation, persist-before-memory ordering, audit lines)
/// stay in the service layer exactly where they were.
pub trait StoreApi: Debug + Send + Sync {
    // ── Account book ─────────────────────────────────────────────────────

    /// Whether this store accepts account-book writes at all. `false` =
    /// the core has no account book carrier (backtester, most in-process
    /// tests) — the writers' standard "persistence is disabled" refusal.
    /// The IPC strategy-enable mirror gate reads this instead of probing
    /// the config pair per call (the pre-seam `account_env_write_paths()`
    /// check, moved behind the seam).
    fn account_writes_enabled(&self) -> bool {
        false
    }

    /// The account↔venue binding write (`account.venue.set`); an empty
    /// `venue` unbinds. Refuses while the legacy accounts toml exists.
    fn save_account_venue(&self, _account_id: &str, _venue: &str) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "save_account_venue: no backend implements this yet".to_string(),
        ))
    }

    /// `account.create`: append the id with its display name, optional
    /// venue binding and the plugin's credential VARIABLE NAMES (values
    /// belong to [`StoreApi::save_credentials`], never here).
    fn save_account_create(
        &self,
        _account_id: &str,
        _name: Option<&str>,
        _venue: Option<&str>,
        _credential_keys: &CredentialKeys,
    ) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "save_account_create: no backend implements this yet".to_string(),
        ))
    }

    /// `account.remove`: drop the id and sweep every row it owns. Refuses
    /// an unknown id, an emptying removal, or a legacy-toml presence; the
    /// display pointer is re-derived when it named the removed id.
    fn remove_account(&self, _account_id: &str) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "remove_account: no backend implements this yet".to_string(),
        ))
    }

    /// The trading-switch write (`account.trading.set`). `true` clears the
    /// explicit-off row (absence is the "trading" spelling).
    fn save_account_trading(&self, _account_id: &str, _enabled: bool) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "save_account_trading: no backend implements this yet".to_string(),
        ))
    }

    /// The lifecycle-status write (`account.status.set`). `Active` clears
    /// the row (absence is "active").
    fn save_account_status(
        &self,
        _account_id: &str,
        _status: &AccountStatus,
    ) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "save_account_status: no backend implements this yet".to_string(),
        ))
    }

    /// The unfreeze write (`account.unfreeze`): `Active` + trading-on are
    /// both spelled by absence, so this is ONE removal — no half-state.
    fn save_account_active(&self, _account_id: &str) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "save_account_active: no backend implements this yet".to_string(),
        ))
    }

    /// The display-pointer write (`display.account.set`).
    fn save_display_account(&self, _account_id: &str) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "save_display_account: no backend implements this yet".to_string(),
        ))
    }

    /// The strategy-enablement write (`account.strategies.set`). An empty
    /// list clears the row (absence = unrestricted).
    fn save_account_strategies(
        &self,
        _account_id: &str,
        _strategies: &[String],
    ) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "save_account_strategies: no backend implements this yet".to_string(),
        ))
    }

    /// The round-family set write (`account.rounds.set`). An empty list
    /// clears the row (absence = no declared family set).
    fn save_account_rounds(&self, _account_id: &str, _rounds: &[i64]) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "save_account_rounds: no backend implements this yet".to_string(),
        ))
    }

    /// The venue's first reported equity (`realign_venue_cash`): the
    /// account's opening principal with source + timestamp, written exactly
    /// once (an already-recorded value is never overwritten).
    fn save_account_opening_equity(
        &self,
        _account_id: &str,
        _equity: Decimal,
        _at_ms: i64,
    ) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "save_account_opening_equity: no backend implements this yet".to_string(),
        ))
    }

    // ── Risk: the six systemic bounds ────────────────────────────────────

    /// The systemic-limit write (`risk.setSystemic`) for one account (the
    /// process default when `None`). Returns whether the write landed —
    /// false = no carrier on this core (the caller's memory-only posture,
    /// honestly reported by its own `persisted`).
    fn save_risk_bounds(
        &self,
        _account_id: Option<&str>,
        _fields: &FieldChanges,
    ) -> Result<bool, CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "save_risk_bounds: no backend implements this yet".to_string(),
        ))
    }

    // ── Exit discipline ──────────────────────────────────────────────────

    /// The exit-discipline write (`risk.setExit`) for one account. Same
    /// return contract as [`StoreApi::save_risk_bounds`].
    fn save_exit(
        &self,
        _account_id: Option<&str>,
        _fields: &FieldChanges,
    ) -> Result<bool, CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "save_exit: no backend implements this yet".to_string(),
        ))
    }

    // ── Cross-venue arb legs ─────────────────────────────────────────────

    /// `arb.config.set`: the five arb-leg facts as one write.
    fn save_arb_config(&self, _cfg: &CrossVenueArbConfig) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "save_arb_config: no backend implements this yet".to_string(),
        ))
    }

    /// `arb.config.clear`: remove the arb-leg facts.
    fn clear_arb_config(&self) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "clear_arb_config: no backend implements this yet".to_string(),
        ))
    }

    /// `arb.config.get`: the boot read-back. Returns `None` when no section is
    /// persisted (the honest "unconfigured" answer). Reads the single
    /// `arb_pairs` row (id = 1).
    fn load_arb_config(&self) -> Result<Option<crate::account::CrossVenueArbConfig>, CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "load_arb_config: no backend implements this yet".to_string(),
        ))
    }

    // ── Execution-policy sections ────────────────────────────────────────

    /// `execution_policy.set/.reset` for one account: the section's whole
    /// footprint (field → value, `None` = swept) as one sweep-then-write.
    /// Refuses while the legacy policy toml exists (the kernel's own
    /// bootstrap template is swept instead — the one exception the old
    /// write face owned). Returns the footprint for the reload snapshot.
    fn save_policy_section(
        &self,
        _account_id: &str,
        _state: &BTreeMap<String, Option<String>>,
    ) -> Result<(), String> {
        Err("save_policy_section: no backend implements this yet".into())
    }

    // ── Per-account round duration ───────────────────────────────────────

    /// `round_duration.set`: `Some(seconds)` upserts the account's round
    /// length; `None` clears the override (reset to the default grid).
    fn save_account_round_duration(
        &self,
        _account_id: &str,
        _seconds: Option<i64>,
    ) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "save_account_round_duration: no backend implements this yet".to_string(),
        ))
    }

    // ── Credentials ──────────────────────────────────────────────────────

    /// `market.credentials.set`: upsert the plugin's declared variable
    /// names → values, or remove them all (`reset`). Values never appear
    /// in logs — only the key names do.
    ///
    /// `reset_keys` is the REMOVE set: empty = upsert `vars`; non-empty =
    /// remove exactly those keys (the caller passes the plugin's declared
    /// table on reset). The two branches are mutually exclusive — the
    /// pre-seam semantics of `market.credentials.set` (an empty
    /// `reset_keys` is NOT a reset request).
    fn save_credentials(
        &self,
        _vars: &BTreeMap<String, String>,
        _reset_keys: &[&str],
    ) -> Result<(), CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "save_credentials: no backend implements this yet".to_string(),
        ))
    }

    // ── Market plugin persisted cell ─────────────────────────────────────

    /// `market.plugin.set`: upsert the persisted market-plugin choice, or clear
    /// it (`None`). Lands in `kernel_meta.runtime_config_json` (issue #47:
    /// config.json 直写替换为 SQLite persist 路径)。
    fn save_market_plugin(&self, plugin: Option<&str>) -> Result<(), CoreError>;

    /// `market.plugin.get`: read back the persisted market-plugin choice from
    /// `kernel_meta.runtime_config_json`. `None` = never persisted.
    fn load_market_plugin(&self) -> Result<Option<String>, CoreError>;

    // ── Money facts (issue #1278 — CONTRACT STAGE, NO CALLERS) ───────────
    //
    // 下面两个方法是 #1278 的**接口定义**，不是实现：本提交里没有任何
    // 调用方，也没有任何 backend 覆盖它们。把 `account_store.rs` 的裸 SQL
    // 与四个 JSONL 资金写面接到这两个方法上，必须等运营者审批本契约之后
    // 再做（Issue #1278：「审批前不得改 `risk/account_store.rs` 实现」）。
    //
    // 为什么默认实现返回 `Err` 而不是 `Ok(MemoryOnly { .. })`：本仓当前
    // 最贵的一类缺陷正是「写盘失败静默成功」——`jsonl::append` 返回
    // `bool`，四个资金写面（订单 / 成交 / 账本 / 结算）全部丢弃它；
    // `Core::store` 是 `Option<Box<dyn StoreApi>>`，「没有 store」是一个
    // 合法且无声的运行态。默认实现若返回 `Ok`，一个忘记实现的 backend
    // 就会继续静默降级；返回 `Err` 把它变成一次响亮的失败。审批后应
    // **删掉默认实现**，强制每个 backend 表态（见
    // `docs/rust-core/RFC-1278-storeapi-v2-contract.md` §4 待决 D-1）。

    /// Append one money fact durably. `WriteOutcome` forces every caller to
    /// distinguish "landed" from "memory only" — the whole point of #1278:
    /// today's four money write faces (order snapshot, closed-trade record,
    /// ledger journal row, settlement journal line) cannot tell the
    /// difference, because they all drop `jsonl::append`'s `bool`.
    ///
    /// `Err` = the backend was ASKED and refused/failed. `Ok(MemoryOnly)` =
    /// the fact is authoritative in memory for this run and is NOT durable —
    /// honest degradation, never silent success. [`SqliteStore`](crate::store::SqliteStore)
    /// lands every fact in the append-only `money_facts` table and reports
    /// [`WriteOutcome::Persisted`]; a core assembled without a store (`None`)
    /// is reported as [`MemoryOnlyReason::NoStoreConfigured`] by the caller
    /// seam, never as silent success.
    fn append_money_fact(&self, fact: &MoneyFact<'_>) -> Result<WriteOutcome, CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            format!(
                "append_money_fact({}): no backend implements this yet",
                fact.kind()
            ),
        ))
    }

    /// Upsert (`Some(value)`) or delete (`None`) one account-scoped setting.
    /// [`SqliteStore`](crate::store::SqliteStore) lands it in `account_settings`
    /// (ensuring the account row first) and reports [`WriteOutcome::Persisted`].
    ///
    /// `AccountScope` rather than a bare `&str` because an unscoped setting
    /// write is exactly the bug class #1221/#1223 were: a write that lands on
    /// a guessed account. Making the account a required, typed argument means
    /// a future caller cannot forget it.
    ///
    /// Never the VALUE in error text: a setting write may carry a secret-shaped
    /// string (DEV §7 / R2), and an error text is a log line.
    fn put_setting(
        &self,
        scope: AccountScope<'_>,
        key: &str,
        value: Option<&str>,
    ) -> Result<WriteOutcome, CoreError> {
        Err(CoreError::new(
            CoreErrorCode::Internal,
            format!(
                "put_setting({}, {key}, {}): no backend implements this yet",
                scope.account.as_str(),
                if value.is_some() { "upsert" } else { "delete" }
            ),
        ))
    }
}

// ── #1278 contract types (no callers yet) ────────────────────────────────
//
// 这一组类型的存在理由是**消灭 silent degradation**，不是方便：现状里
// 「写没写下去」在四个资金写面上不可区分（`jsonl::append` → `bool`，
// 调用方丢弃），所以调用方只能假装成功。`WriteOutcome` 把这件事变成
// 一个必须被 `match` 的返回值。

/// What one durable write actually did. Deliberately NOT a `bool`: `true`
/// reads as "fine" at every call site, which is how the current four money
/// write faces lost the distinction in the first place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The fact is on disk (or in the carrier) and survives a restart.
    Persisted,
    /// The fact is authoritative for this run only. Every caller that gets
    /// this MUST surface it — a money fact that exists only in memory is an
    /// operator-visible fact, not a rounding error.
    MemoryOnly { reason: MemoryOnlyReason },
}

/// Why a write did not become durable. `NoStoreConfigured` is the
/// `Core::store == None` posture (backtester, most in-process tests) — legal
/// but never silent; `ReadOnlyMode` is the `--readonly` structural refusal;
/// `BackendRejected` carries the carrier's own message verbatim (R12 / DEV §7:
/// never replace a raw error with a generic string).
#[allow(dead_code)] // #1278 契约阶段，待审批后接线
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryOnlyReason {
    /// This core was assembled without a store (`Core::store == None`).
    NoStoreConfigured,
    /// The process runs in the read-only posture; durable writes are
    /// structurally refused rather than attempted and lost.
    ReadOnlyMode,
    /// The backend was asked and declined. The string is the backend's own
    /// error text, passed through unaltered.
    BackendRejected(String),
}

/// The account a scoped write targets. A struct rather than a bare `&str` so
/// that adding scope (venue, book, round) later does not re-break every
/// implementor, and so a call site physically cannot omit the account.
#[derive(Debug, Clone, Copy)]
pub struct AccountScope<'a> {
    pub account: &'a AccountId,
}

impl<'a> AccountScope<'a> {
    #[allow(dead_code)] // 契约预留：未来扩展作用域时启用
    pub fn new(account: &'a AccountId) -> Self {
        Self { account }
    }
}

/// The four money facts the kernel currently persists as JSONL, each naming
/// the type it ALREADY writes — this is a UNION OF EXISTING TYPES, not a
/// second ledger model (R7: 记账口径不可擅动). No variant carries new
/// arithmetic, a new field, or a new spelling; wiring the JSONL call sites
/// onto this enum must not change one byte of what lands on disk.
///
/// - `Order` — `order_db::OrderDb::append` (`src/order_db.rs`)
/// - `Fill` — `trade_db::TradeDb::record` (`src/trade_db.rs`)
/// - `Ledger` — `Ledger::apply_tagged`'s journal append (`src/ledger.rs`)
/// - `Settlement` — `SettlementJournal::append` (`src/settlement.rs`)
#[derive(Debug, Clone, Copy)]
pub enum MoneyFact<'a> {
    Order(&'a crate::model::TrackedOrder),
    Fill(&'a crate::trade_db::TradeRecord),
    Ledger(&'a crate::ledger::JournalEntry),
    Settlement(&'a crate::settlement::SettlementRecord),
}

impl MoneyFact<'_> {
    /// The variant's name, for error text only. Never parsed, never logged
    /// as data — it exists so a refusal names WHAT was refused.
    pub fn kind(&self) -> &'static str {
        match self {
            MoneyFact::Order(_) => "order",
            MoneyFact::Fill(_) => "fill",
            MoneyFact::Ledger(_) => "ledger",
            MoneyFact::Settlement(_) => "settlement",
        }
    }
}

/// Test-only seam for the carrier decision (the same gating pattern as
/// `strategies::test_support`: compiled for in-process tests or with the
/// `test-support` feature, never in the production binary). Production
/// assembly wires exactly one backend — the `SqliteStore` from
/// `config.db_path` — so a non-SQLite `StoreApi` implementation has no way
/// in; the backend-swap proof (`tests/store_backend_parity.rs`) drives the
/// SAME seam the production writers use with a `MemoryStore` carrier, and
/// this face exists for that alone.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::{AccountScope, MoneyFact, StoreApi, WriteOutcome};
    use crate::model::CoreError;
    use std::sync::Arc;

    /// Install an arbitrary [`StoreApi`] backend on a core (replacing any
    /// assembly-wired store). The caller keeps its own read-back handle —
    /// the core has no store getter, by design: production reads go through
    /// the domain faces, not the carrier.
    pub fn install_store(core: &mut crate::service::Core, store: Arc<dyn StoreApi>) {
        core.store = Some(store);
    }

    /// Test-only recording carrier: `append_money_fact`/`put_setting` record
    /// the call and report `Persisted`; every other domain keeps the trait's
    /// honest `Err` default. The four money-write faces' wiring tests use it
    /// to assert the route without touching a file.
    #[derive(Debug)]
    pub struct RecordingStore {
        writes: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl RecordingStore {
        #[allow(clippy::new_ret_no_self)]
        pub fn new() -> (Arc<dyn StoreApi>, Arc<std::sync::Mutex<Vec<String>>>) {
            let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    writes: writes.clone(),
                }) as Arc<dyn StoreApi>,
                writes,
            )
        }

        fn log(&self, what: &str) {
            self.writes.lock().unwrap().push(what.to_string());
        }
    }

    impl StoreApi for RecordingStore {
        fn append_money_fact(&self, fact: &MoneyFact<'_>) -> Result<WriteOutcome, CoreError> {
            self.log(&format!("money:{}", fact.kind()));
            Ok(WriteOutcome::Persisted)
        }

        fn put_setting(
            &self,
            scope: AccountScope<'_>,
            key: &str,
            value: Option<&str>,
        ) -> Result<WriteOutcome, CoreError> {
            self.log(&format!(
                "setting:{}={key}={value:?}",
                scope.account.as_str()
            ));
            Ok(WriteOutcome::Persisted)
        }

        fn save_market_plugin(&self, _plugin: Option<&str>) -> Result<(), CoreError> {
            self.log("market_plugin:set");
            Ok(())
        }

        fn load_market_plugin(&self) -> Result<Option<String>, CoreError> {
            self.log("market_plugin:get");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FillPolicy, OrderRole, OrderStatus, Side, TrackedOrder};
    use rust_decimal_macros::dec;

    /// The in-memory test backend: tests that want to observe writes
    /// without touching a file implement the trait over a recording map.
    /// Every method accepts and records.
    #[derive(Debug, Default)]
    struct MemStore {
        accounts_enabled: bool,
        writes: std::sync::Mutex<Vec<String>>,
    }

    impl MemStore {
        fn recording(accounts_enabled: bool) -> Self {
            Self {
                accounts_enabled,
                writes: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn log(&self, what: String) {
            self.writes.lock().unwrap().push(what);
        }

        fn writes(&self) -> Vec<String> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl StoreApi for MemStore {
        fn account_writes_enabled(&self) -> bool {
            self.accounts_enabled
        }

        fn save_account_venue(&self, account_id: &str, venue: &str) -> Result<(), CoreError> {
            self.log(format!("venue:{account_id}={venue}"));
            Ok(())
        }

        fn save_account_create(
            &self,
            account_id: &str,
            name: Option<&str>,
            venue: Option<&str>,
            credential_keys: &CredentialKeys,
        ) -> Result<(), CoreError> {
            self.log(format!(
                "create:{account_id}/{}/{}/{}/{}",
                name.unwrap_or(""),
                venue.unwrap_or(""),
                credential_keys.api_key_env.as_deref().unwrap_or(""),
                credential_keys.secret_env.as_deref().unwrap_or("")
            ));
            Ok(())
        }

        fn remove_account(&self, account_id: &str) -> Result<(), CoreError> {
            self.log(format!("remove:{account_id}"));
            Ok(())
        }

        fn save_account_trading(&self, account_id: &str, enabled: bool) -> Result<(), CoreError> {
            self.log(format!("trading:{account_id}={enabled}"));
            Ok(())
        }

        fn save_account_status(
            &self,
            account_id: &str,
            status: &AccountStatus,
        ) -> Result<(), CoreError> {
            self.log(format!("status:{account_id}={status}"));
            Ok(())
        }

        fn save_account_active(&self, account_id: &str) -> Result<(), CoreError> {
            self.log(format!("active:{account_id}"));
            Ok(())
        }

        fn save_display_account(&self, account_id: &str) -> Result<(), CoreError> {
            self.log(format!("display:{account_id}"));
            Ok(())
        }

        fn save_account_strategies(
            &self,
            account_id: &str,
            strategies: &[String],
        ) -> Result<(), CoreError> {
            self.log(format!("strategies:{account_id}={}", strategies.join(",")));
            Ok(())
        }

        fn save_account_rounds(&self, account_id: &str, rounds: &[i64]) -> Result<(), CoreError> {
            self.log(format!(
                "rounds:{account_id}={}",
                rounds
                    .iter()
                    .map(|r| r.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            Ok(())
        }

        fn save_account_opening_equity(
            &self,
            account_id: &str,
            equity: Decimal,
            at_ms: i64,
        ) -> Result<(), CoreError> {
            self.log(format!("equity:{account_id}={equity}@{at_ms}"));
            Ok(())
        }

        fn save_risk_bounds(
            &self,
            account_id: Option<&str>,
            fields: &FieldChanges,
        ) -> Result<bool, CoreError> {
            self.log(format!(
                "risk:{}={}",
                account_id.unwrap_or("default"),
                fields
                    .iter()
                    .map(|(f, v)| format!("{f}={v}"))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            Ok(true)
        }

        fn save_exit(
            &self,
            account_id: Option<&str>,
            fields: &FieldChanges,
        ) -> Result<bool, CoreError> {
            self.log(format!(
                "exit:{}={}",
                account_id.unwrap_or("default"),
                fields
                    .iter()
                    .map(|(f, v)| format!("{f}={v}"))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            Ok(true)
        }

        fn save_arb_config(&self, cfg: &CrossVenueArbConfig) -> Result<(), CoreError> {
            self.log(format!("arb:{}/{}", cfg.leg_a_venue, cfg.leg_b_venue));
            Ok(())
        }

        fn clear_arb_config(&self) -> Result<(), CoreError> {
            self.log("arb-clear".to_string());
            Ok(())
        }

        fn load_arb_config(
            &self,
        ) -> Result<Option<crate::account::CrossVenueArbConfig>, CoreError> {
            self.log("arb-load".to_string());
            Ok(None)
        }

        fn save_policy_section(
            &self,
            account_id: &str,
            state: &BTreeMap<String, Option<String>>,
        ) -> Result<(), String> {
            self.log(format!(
                "policy:{account_id}:{}",
                state
                    .iter()
                    .map(|(k, v)| format!("{k}={}", v.as_deref().unwrap_or("<swept>")))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            Ok(())
        }

        fn save_account_round_duration(
            &self,
            account_id: &str,
            seconds: Option<i64>,
        ) -> Result<(), CoreError> {
            self.log(format!(
                "round-duration:{account_id}={}",
                seconds.map(|s| s.to_string()).unwrap_or("reset".into())
            ));
            Ok(())
        }

        fn save_credentials(
            &self,
            vars: &BTreeMap<String, String>,
            reset_keys: &[&str],
        ) -> Result<(), CoreError> {
            self.log(format!(
                "credentials:{:?}/{:?}",
                vars.keys().collect::<Vec<_>>(),
                reset_keys
            ));
            Ok(())
        }

        fn append_money_fact(&self, fact: &MoneyFact<'_>) -> Result<WriteOutcome, CoreError> {
            self.log(format!("money:{}", fact.kind()));
            Ok(WriteOutcome::Persisted)
        }

        fn put_setting(
            &self,
            scope: AccountScope<'_>,
            key: &str,
            value: Option<&str>,
        ) -> Result<WriteOutcome, CoreError> {
            self.log(format!(
                "setting:{}={key}={value:?}",
                scope.account.as_str()
            ));
            Ok(WriteOutcome::Persisted)
        }

        fn save_market_plugin(&self, _plugin: Option<&str>) -> Result<(), CoreError> {
            self.log("market-plugin".to_string());
            Ok(())
        }

        fn load_market_plugin(&self) -> Result<Option<String>, CoreError> {
            self.log("market-plugin-load".to_string());
            Ok(None)
        }
    }

    #[test]
    fn mem_store_records_every_domain_write() {
        let s = MemStore::recording(true);
        assert!(s.account_writes_enabled());
        s.save_account_venue("paper", "kalshi").unwrap();
        s.save_account_trading("paper", false).unwrap();
        s.save_account_status("paper", &AccountStatus::Frozen)
            .unwrap();
        s.save_account_active("paper").unwrap();
        s.save_display_account("paper").unwrap();
        s.save_account_strategies("paper", &["spread_arb".into()])
            .unwrap();
        s.save_account_rounds("paper", &[900, 3600]).unwrap();
        s.save_account_opening_equity("paper", Decimal::new(1234, 2), 7)
            .unwrap();
        s.save_risk_bounds(
            Some("paper"),
            &vec![("maxSingleLossUsd".into(), "25".into())],
        )
        .unwrap();
        s.save_exit(Some("paper"), &vec![("stopLossPct".into(), "5".into())])
            .unwrap();
        s.save_arb_config(&CrossVenueArbConfig {
            leg_a_venue: "kalshi".into(),
            leg_a_account: "paper".into(),
            leg_b_venue: "polymarket".into(),
            leg_b_account: "paper".into(),
            pairs: vec!["BTC".into()],
        })
        .unwrap();
        s.clear_arb_config().unwrap();
        s.save_policy_section(
            "paper",
            &BTreeMap::from([("POLICY_BUDGET_RATIO_PAPER".to_string(), Some("0.1".into()))]),
        )
        .unwrap();
        s.save_account_round_duration("paper", Some(3600)).unwrap();
        s.save_account_round_duration("paper", None).unwrap();
        s.save_credentials(
            &BTreeMap::from([("KALSHI_API_KEY".to_string(), "k".into())]),
            &[],
        )
        .unwrap();
        s.append_money_fact(&MoneyFact::Order(&order_for_money_test()))
            .unwrap();
        s.put_setting(
            AccountScope::new(&AccountId::from("paper")),
            "POLICY_TEST",
            Some("1"),
        )
        .unwrap();
        let w = s.writes();
        assert_eq!(w.len(), 18, "one line per domain write: {w:?}");
        assert_eq!(w[0], "venue:paper=kalshi");
        assert_eq!(w[1], "trading:paper=false");
        assert_eq!(w[2], "status:paper=frozen");
        assert_eq!(w[3], "active:paper");
        assert_eq!(w[4], "display:paper");
        assert_eq!(w[5], "strategies:paper=spread_arb");
        assert_eq!(w[6], "rounds:paper=900,3600");
        assert_eq!(w[7], "equity:paper=12.34@7");
        assert_eq!(w[8], "risk:paper=maxSingleLossUsd=25");
        assert_eq!(w[9], "exit:paper=stopLossPct=5");
        assert_eq!(w[10], "arb:kalshi/polymarket");
        assert_eq!(w[11], "arb-clear");
        assert_eq!(w[12], "policy:paper:POLICY_BUDGET_RATIO_PAPER=0.1");
        assert_eq!(w[13], "round-duration:paper=3600");
        assert_eq!(w[14], "round-duration:paper=reset");
        assert_eq!(w[15], "credentials:[\"KALSHI_API_KEY\"]/[]");
        assert_eq!(w[16], "money:order");
        assert_eq!(w[17], "setting:paper=POLICY_TEST=Some(\"1\")");
    }

    #[test]
    fn mem_store_can_report_persistence_disabled() {
        let s = MemStore::recording(false);
        assert!(!s.account_writes_enabled());
    }

    /// A complete TrackedOrder for money-fact tests (order_db.rs keeps an
    /// identical private helper — the two modules must not share fixtures).
    fn order_for_money_test() -> TrackedOrder {
        TrackedOrder {
            order_id: "o1".into(),
            internal_key: "k1".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            token_id: "tok".into(),
            condition_id: "cond".into(),
            side: Side::Buy,
            mode: FillPolicy::MakerThenTaker,
            role: OrderRole::Maker,
            price: dec!(0.43),
            size: dec!(10),
            filled_size: Decimal::ZERO,
            avg_fill_price: None,
            status: OrderStatus::Pending,
            round_slot: 1,
            submitted_at_ms: 1,
            updated_at_ms: 1,
            decision_at_ms: 1,
            execute_at_ms: 1,
            report_at_ms: 1,
            venue_order_id: Some("0xabc".into()),
            place_uncertain: false,
            escalate_at_ms: None,
            maker_timeout_ms: 0,
            account_id: AccountId::from("paper"),
            venue: String::new(),
            round_duration_sec: 0,
            rejected_reason: None,
        }
    }
}
