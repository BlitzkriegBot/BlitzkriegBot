//! Configuration-gate derivation (`reconcile_config_gates` / `_for`): the
//! per-account mapping from config posture (ReadOnly / Suspended / Frozen /
//! trading_enabled=false) to an `AccountGate`. Pure code motion from
//! `service.rs` (no logic edits).

use crate::account::AccountStatus;
use crate::model::AccountId;
use crate::service::Core;
use crate::service::risk_admission::GateReason;

pub(crate) fn reconcile_config_gates(core: &mut Core, now_ms: i64) {
    let ids: Vec<String> = core
        .accounts
        .account_list()
        .iter()
        .map(|a| a.id.as_str().to_owned())
        .collect();
    for id in ids {
        reconcile_config_gate_for(core, &AccountId::from(id.as_str()), now_ms);
    }
}

/// 单账户版本的配置态 → gate 推导。用于 `set_status` /
/// `set_trading` / `unfreeze` 写回之后的即时重算，避免「配置拦但
/// gate 不在」的瞬时不一致。
///
/// 规则：
/// - 配置姿态拒新单（非 Active，或 trading_enabled=false）：设置或
///   升级对应的 Config* 闸门（优先级高者覆盖低者，Manual 等运行时
///   闸门不被降级）。
/// - 配置姿态允新单（Active 且 trading_enabled=true）：移除由配置
///   派生的闸门（ConfigReadOnly / ConfigSuspended / TradingDisabled /
///   ConfigFrozen），保留 Manual / DailyLoss / VenueRejection / KillSwitch
///   等运行时闸门不动。
pub(crate) fn reconcile_config_gate_for(core: &mut Core, id: &AccountId, now_ms: i64) {
    let Some(account) = core.accounts.account(id) else {
        return;
    };
    let id_str = id.to_string();
    let blocks_entries = !account.status.allows_entries() || !account.trading_enabled;

    if blocks_entries {
        let (kind, reason) = if !account.status.allows_entries() {
            match account.status {
                AccountStatus::ReadOnly => (
                    GateReason::ConfigReadOnly,
                    "account read_only in config - new entries and closes refused at Gate 2"
                        .to_string(),
                ),
                AccountStatus::Suspended { ref reason } => (
                    GateReason::ConfigSuspended,
                    format!("account suspended in config - {reason}"),
                ),
                AccountStatus::Frozen => (
                    GateReason::Manual,
                    "account frozen in config - new entries refused, exits keep flowing"
                        .to_string(),
                ),
                AccountStatus::Active => unreachable!("Active allows entries"),
            }
        } else {
            (
                GateReason::TradingDisabled,
                "trading switch off in config - new entries refused, exits keep flowing"
                    .to_string(),
            )
        };
        // 只升不覆：优先级更高的运行时闸门（Manual / KillSwitch 等）不会
        // 被配置态闸门降级；同优先级的“先到者胜”保持稳定。
        let _ = core.set_account_gate(&id_str, reason, kind, None, "config", now_ms, None);
    } else {
        // 配置态允新单：移除由配置派生的闸门，保留运行时闸门不动。
        if let Some(g) = core.account_gate(&id_str) {
            let should_remove = matches!(
                g.reason_kind,
                GateReason::ConfigReadOnly
                    | GateReason::ConfigSuspended
                    | GateReason::TradingDisabled
            );
            if should_remove {
                core.account_gates.remove(&id_str);
            }
        }
    }
}
