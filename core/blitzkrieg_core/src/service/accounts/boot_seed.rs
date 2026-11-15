//! Boot-time account-book migration helpers (#1199 第九轮): the removed-account
//! tombstone gate (fail-closed + logged skip) and the default-venue seed bind
//! (操作者指令 2026-11-15). Kept off `assembly.rs` so the constructor stays
//! readable and the file-size ratchet (#780) stays green.

use crate::account::{Account, AccountLedgers};
use crate::risk::account_store::AccountRiskDb;
use crate::store::{SqliteStore, StoreApi};

/// 墓碑读取 fail-closed：读失败 panic 拒启（与 `load_accounts` 读失败 panic
/// 拒启同一姿态）——静默当作空集会让已删除的账户从 `.env` 复活。
pub(crate) fn load_removed_accounts_fail_closed(db: &AccountRiskDb) -> Vec<String> {
    db.load_removed_accounts().unwrap_or_else(|e| {
        panic!(
            "refusing to boot: removed_account tombstones read failed ({e}) — the \
             durable book must be readable or the kernel must not trade"
        )
    })
}

/// boot 合并的 env 补账半边：`.env` 簿里 SQL 没有的账户补进簿，**但**墓碑
/// 名单里的账户跳过（已删除的账户不复活）并如实 warn —— 操作者看到账户
/// 从簿里消失时至少有一条线索指向墓碑。
pub(crate) fn merge_env_side_accounts(
    sql_accounts: &mut AccountLedgers,
    env_book: &[Account],
    tombstones: &[String],
) {
    for acc in env_book {
        let id = acc.id.as_str();
        if sql_accounts.account(&acc.id).is_some() {
            continue;
        }
        if tombstones.iter().any(|t| t == id) {
            tracing::warn!(
                account = %id,
                "skipping resurrected account: removal tombstone present in \
                 kernel_meta, .env re-materialization refused"
            );
            continue;
        }
        let _ = sql_accounts.insert(acc.clone());
    }
}

/// 操作者指令 2026-11-15：升级存量部署的 default 账户 venue 为空（0.4.1
/// 遗留行）→ 补绑出厂种子 venue（polymarket，#1127 §9.5）并落库。venue 空
/// 串只来自旧版本迁移（零配置新建的 default 本就绑 `DEFAULT_SEED_VENUE`）；
/// 落库失败不阻塞 boot，warn 后内存仍已绑定，下次 boot 重试。
pub(crate) fn seed_bind_default_venue(accounts: &mut AccountLedgers, s: &SqliteStore) {
    use crate::account::{DEFAULT_SEED_VENUE, default_account_id};
    let needs_bind = accounts
        .account(&default_account_id())
        .map(|a| a.venue.is_empty())
        .unwrap_or(false);
    if !needs_bind {
        return;
    }
    if let Some(acc) = accounts.account_mut(&default_account_id()) {
        acc.venue = DEFAULT_SEED_VENUE.into();
    }
    if let Err(e) =
        StoreApi::save_account_venue(s, default_account_id().as_str(), DEFAULT_SEED_VENUE)
    {
        tracing::warn!(
            error = %e.message,
            "default venue seed-bind persist failed — the in-memory bind stands, \
             the next boot retries"
        );
        return;
    }
    tracing::info!(
        venue = DEFAULT_SEED_VENUE,
        "default account was venue-less (upgrade residue) — bound to the factory \
         seed venue and persisted"
    );
}
