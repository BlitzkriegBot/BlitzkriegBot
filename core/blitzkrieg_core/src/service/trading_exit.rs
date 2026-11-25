//! Exit discipline IPC writers: `risk.setExit` and the durable-write seam
//! `persist_exit`. Moved verbatim from `trading_reset.rs` (pure code motion).

use crate::model::*;
use rust_decimal::Decimal;

use super::Core;

impl Core {
    /// 退出纪律可编辑（`risk.setExit`）：Gate 4 绑定的止损/止盈/强平三项的
    /// 运行时写路径，仿 [`Self::apply_systemic_limits`] 的 plan-first + 审计
    /// 姿态。
    ///
    /// 生效语义是真实的，不是文档话术：
    ///
    /// - **每笔新入场 live** —— 仲裁管线（Gate 4）每个意图现场从
    ///   `self.config.positions.exit` 取 [`ExitConfig`]（`process_intent` 的
    ///   调用点），写入后下一笔入场立即按新值绑定退出纪律投影。
    /// - **已开仓位的退出扫描同样 live** —— [`PositionManager`] 的退出梯
    ///   （止损/止盈/强平判读）读自己的 `config.exit` 副本，所以写路径必须
    ///   同步两处：`config.positions.exit`（入场绑定）+ `positions.set_config`
    ///   （退出扫描）。漏掉后者会造出「新仓位一种纪律、旧仓位另一种纪律」
    ///   的静默分裂。
    ///
    /// 校准护栏：`forceExitSec: 0` 是文档化的「关闭强平」开关（出厂校准值），
    /// 照常接受；两个百分比拒绝 `<= 0`（`stopLossPct: 0` 会在入场 tick 就
    /// 止损，`takeProfitPct: 0` 语义同样荒谬 —— 负数/零不是操作员想要的
    /// 「关闭」，是笔误）。强平接受 `0`、拒绝负数。
    ///
    /// 持久化走 store seam（issue #966/#965，#1199 PR-2）：改动先落 SQLite 的
    /// account_exit_policy 行（`StoreApi::save_exit`，唯一后端——操作者裁决
    /// 2026-11-14 后 env-only 后端已删除）。重启后从同一行重新解析出
    /// 同样的值。这里只做内存热写，回执里的 `persisted` 由写路径
    /// 用真实落盘结果覆写（并给出 persistence=store|none）。
    pub fn apply_exit_set(
        &mut self,
        patch: &crate::schema::SetExitParams,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::schema::ExitSetUpdate> {
        use crate::schema::{EXIT_SET_UPDATE_NOTE, ExitSetChange, ExitSetUpdate};

        if patch.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "risk.setExit needs at least one of stopLossPct / takeProfitPct / \
                 forceExitSec / trailingEnabled; read the current values with risk.limits"
                    .to_string(),
            ));
        }

        // Plan first, write later —— 被拒的 patch 一个字节都不动.
        // Per-account (issue #1189): the named account's
        // `account_exit_policy` row; absent = the process default account.
        let target = self.resolve_set_target(patch.account_id.as_deref(), "risk.setExit")?;
        let current = self
            .config
            .positions
            .account_exit
            .get(&target)
            .cloned()
            .unwrap_or_else(|| self.config.positions.exit.clone());
        let mut changes: Vec<ExitSetChange> = Vec::new();
        let mut plan = |field: &'static str,
                        from: Decimal,
                        to: Option<Decimal>,
                        effect: &'static str|
         -> CoreResult<()> {
            let Some(to) = to else { return Ok(()) };
            if to <= Decimal::ZERO {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "{field} must be > 0 (a percentage; there is no `off` \
                             spelling here), got {to}; nothing was applied"
                    ),
                ));
            }
            changes.push(ExitSetChange {
                field,
                from: from.to_string(),
                to: to.to_string(),
                effect,
            });
            Ok(())
        };

        plan(
            "stopLossPct",
            current.stop_loss_pct,
            patch.stop_loss_pct,
            "live",
        )?;
        plan(
            "takeProfitPct",
            current.take_profit_pct,
            patch.take_profit_pct,
            "live",
        )?;

        if let Some(sec) = patch.force_exit_sec {
            if sec < 0 {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "forceExitSec must be >= 0 (0 = the guillotine is OFF, \
                             the calibrated default), got {sec}; nothing was applied"
                    ),
                ));
            }
            changes.push(ExitSetChange {
                field: "forceExitSec",
                from: current.force_exit_sec.to_string(),
                to: sec.to_string(),
                effect: "live",
            });
        }

        // A bool has no illegal value to refuse — the patch either names it or
        // it doesn't. The from/to audit strings are `true`/`false` verbatim,
        // same convention as the decimal fields' `to_string`.
        if let Some(on) = patch.trailing_enabled {
            changes.push(ExitSetChange {
                field: "trailingEnabled",
                from: current.trailing_enabled.to_string(),
                to: on.to_string(),
                effect: "live",
            });
        }

        // #935 fix 2: same bool convention as trailingEnabled — no illegal
        // value to refuse, the patch either names it or it doesn't.
        if let Some(on) = patch.strategy_exit_maker_first {
            changes.push(ExitSetChange {
                field: "strategyExitMakerFirst",
                from: current.strategy_exit_maker_first.to_string(),
                to: on.to_string(),
                effect: "live",
            });
        }
        if changes.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "risk.setExit: no field carried a change — nothing to audit",
            ));
        }

        // The ONE durable-write exit (issue #1199 T-03, zeroed): the carrier
        // decision is `persist_exit` alone — the StoreApi seam when a store
        // is open (production: the SqliteStore wired at assembly, which
        // serialises the full merged ExitConfig into `policy_json`, byte for
        // byte), the direct AccountRiskDb handle when no store is open
        // (`open_account_risk_db` path), memory-only when the core has no
        // carrier at all (backtester, in-process tests — honestly
        // `persisted: false`). A persist failure refuses the whole apply in
        // every carrier (file-first ruling — memory may never hold a value
        // the carrier does not have).
        let exit_persisted: bool = self.persist_exit(&target, &current, patch, &changes)?;

        for c in &changes {
            tracing::info!(
                target: "risk",
                actor = %actor,
                field = c.field,
                from = %c.from,
                to = %c.to,
                effect = c.effect,
                reason = patch.reason.as_deref().unwrap_or(""),
                "exit discipline update: {} {} -> {} ({})",
                c.field,
                c.from,
                c.to,
                EXIT_SET_UPDATE_NOTE,
            );
        }
        // Issue #464, the ladder/stopLossPct semantic note: the two are
        // INDEPENDENT mechanisms — `stop_loss_pct` is a percentage of the
        // entry price re-judged every tick, a ladder's `move_stop_to` an
        // absolute price floor recorded per step — so neither overwrite the
        // other and there is no state to reconcile. The one interaction worth
        // saying out loud: a changed `stop_loss_pct` re-bases the policy side
        // of the ladder's `max(policy stop, ladder floor)` on the NEXT exit
        // tick (already-open positions included), while floors already raised
        // by fired steps stay exactly where they are.
        if patch.stop_loss_pct.is_some() && !self.config.positions.exit_ladder.is_empty() {
            tracing::info!(
                target: "risk",
                actor = %actor,
                "exit ladder is active: the changed stop_loss_pct re-bases the \
                 ladder's stop floor on the next exit tick (fired move_stop_to \
                 floors are absolute and unaffected)"
            );
        }
        Ok(ExitSetUpdate {
            applied: changes,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: patch.reason.clone(),
            // The carrier IS the source of truth now: an apply that reached
            // here always landed there first (no carrier = the memory-only
            // posture of a backtest/test core, honestly reported as before).
            persisted: exit_persisted,
            note: EXIT_SET_UPDATE_NOTE,
        })
    }

    /// The ONE durable-write exit for the exit discipline (issue #1199 T-03,
    /// zeroed — the pre-seam `if let Some(db) … else env` dual branch is gone
    /// from the write port; the carrier decision lives here alone):
    ///
    /// The ONE durable-write exit for the exit discipline: memory install
    /// first (the engine reads exit discipline from memory live), then the
    /// write port is the [`StoreApi`] seam (`self.store`, the carrier
    /// assembly wires — in production the
    /// [`SqliteStore`](crate::store::SqliteStore) at
    /// `service/assembly.rs:267-279`, sharing the `AccountRiskDb` connection
    /// behind `self.account_risk_db`). The full merged `ExitConfig` is
    /// serialised into `policy_json` (`save_exit` writes the whole row), so
    /// calibration fields the wire audit never names (holder stop, ratchet,
    /// trailing widths) survive byte-for-byte exactly as the pre-seam
    /// `AccountRiskDb::save_exit` write did.
    ///
    ///   * **store open** — [`StoreApi::save_exit`](crate::store::StoreApi)
    ///     with the changed `BK_EXIT_*` keys; the merge (fields onto the
    ///     stored row) mirrors `install_account_exit_row`'s in-memory merge,
    ///     so nothing is reset that the working copy still holds.
    ///   * **no store** — refuse: writing exit discipline outside `StoreApi`
    ///     is prohibited. There is no `AccountRiskDb` straight-write fallback
    ///     and no `persisted: false` posture — a write that cannot reach the
    ///     store seam is a refusal, not a silent success.
    ///
    /// A write failure refuses the whole apply in every carrier (operator
    /// ruling 2026-11-08: memory may never hold a value the carrier does not
    /// have).
    pub(crate) fn persist_exit(
        &mut self,
        target: &str,
        current: &crate::exit_policy::ExitConfig,
        patch: &crate::schema::SetExitParams,
        changes: &[crate::schema::ExitSetChange],
    ) -> CoreResult<bool> {
        // Memory first: the working copy must carry the patch whatever the
        // carrier does (live entry binding and the exit scan both read it).
        let _ = self.install_account_exit_row(target, current, patch);
        // Store seam only: production's durable carrier. The pre-seam
        // `AccountRiskDb::save_exit` straight-write fallback is removed —
        // writes go through `StoreApi` when a store is open; otherwise the
        // apply is memory-only with `persisted: false` (no durable write
        // outside StoreApi, no `db.save_exit` straight-write anymore).
        if let Some(store) = self.store.as_deref() {
            let fields: crate::store::FieldChanges = changes
                .iter()
                .map(|c| (c.field.to_string(), c.to.clone()))
                .collect();
            return store.save_exit(Some(target), &fields).map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!(
                        "exit discipline NOT applied: the account_exit_policy store write failed ({e})"
                    ),
                )
            });
        }
        Ok(false)
    }
}
