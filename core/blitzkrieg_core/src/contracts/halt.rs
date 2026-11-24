//! # P1-7 · a halt names its scope, and a venue failure cannot escalate to global
//!
//! **Abolishes** the escalation the audit measured: `on_venue_self_check` and
//! `on_venue_reconcile_failed` call `freeze_trading` when
//! `account_ids_bound_to_venue` is empty, which becomes `risk.kill` +
//! `GLOBAL_KILL_ACCOUNT` (`core/blitzkrieg_core/src/service/risk_admission/freeze.rs:105`).
//! One venue's failure therefore freezes **every account and every strategy** —
//! new entries stop everywhere until an operator resumes or the kernel restarts.
//! The `risk.kill` receipt carries no scope field, so the kernel literally
//! cannot express "only this venue was meant to stop".
//!
//! **Replaces it with** [`HaltScope`], which makes the scope the first thing a
//! halt says, and [`Halt`], which carries it plus the reason and an optional
//! expiry. A venue health failure can only produce `HaltScope::Venue`; when no
//! account is bound to that venue the correct outcome is an unattributed alert,
//! **never** a global kill.
//!
//! **Issue:** P1-7. **Consuming task:** T10 (`freeze.rs` +
//! `service/accounts/reconcile.rs`). The audit marks T10 **R6** — this touches
//! the risk hard boundary, so implementation needs the operator's approval
//! before it starts. Freezing the type does not change any threshold.

use crate::contracts::ids::VenueId;
use crate::model::AccountId;

/// How far a halt reaches.
///
/// **Abolishes:** the process-level kill switch as the only shape of "stop
/// trading", which is why a single venue's problem stopped the whole kernel.
/// **Replaced by:** this enum. The variants are ordered from narrowest to
/// widest, and only the operator can pick the widest.
/// **Issue:** P1-7 / R7 (risk hard boundary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HaltScope {
    /// One (account, strategy) scope stops entering. Positions keep flowing to
    /// flat — a halt is never a teardown.
    Strategy {
        /// The strategy name, as registered and as the panel spells it.
        strategy: String,
        /// The account the scope belongs to. A strategy-only key would collide
        /// across accounts, which is the #1221 bug class.
        account: AccountId,
    },
    /// One account's new entries stop.
    Account(AccountId),
    /// One venue's new entries stop. **This is the widest a venue health
    /// failure may produce** — including when no account is bound to it, where
    /// the correct extra output is an unattributed alert, not escalation.
    Venue(VenueId),
    /// The whole kernel stops entering. Operator-only: the audit's rule is
    /// 「venue 健康失败只能产生 `HaltScope::Venue`；无绑定账户 → Unattributed +
    /// 告警，禁止升级为 Global」.
    GlobalOperator,
}

/// One active halt.
///
/// **Abolishes:** `freeze_trading`'s scope-less `risk.kill` receipt, which
/// forced every consumer to assume "halted means everything is stopped".
/// **Replaced by:** this struct. The scope is a required field, so a halt
/// without one cannot be constructed.
/// **Issue:** P1-7.
///
/// **Every** halt still allows exits. That is a property of the risk boundary,
/// not of this type; it is restated here so nobody reads `Halt` as "freeze the
/// world".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Halt {
    /// Who is halted. Required, first field on purpose.
    pub scope: HaltScope,
    /// Why, in the producer's own words. Surfaced to the operator verbatim —
    /// never replaced with a generic string (DEV §7).
    pub reason: String,
    /// When the halt lapses on its own, epoch milliseconds. `None` = it holds
    /// until an operator resumes.
    ///
    /// `None` is deliberately **not** `0` and not "no expiry by accident": an
    /// automatic expiry must be chosen, never defaulted.
    pub until_ms: Option<i64>,
}
