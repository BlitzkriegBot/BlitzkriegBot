//! Helpers shared by the boot-enablement wire tests (pure code motion).
//! `stored_strategy_set` reads the SQLite `account_settings.strategies` row.

/// 账户 → 策略集合行（SQLite 的 `account_settings.strategies`）：
/// 操作者裁决 2026-11-14 之后，集合的家是数据库，不是 `.env`。
pub(super) fn stored_strategy_set(dir: &std::path::Path, account: &str) -> Option<String> {
    crate::risk::account_store::AccountRiskDb::open(dir.join("store.sqlite"))
        .ok()?
        .load_settings()
        .ok()?
        .get(account)?
        .get(crate::risk::account_store::STRATEGIES_SETTING)
        .cloned()
}
