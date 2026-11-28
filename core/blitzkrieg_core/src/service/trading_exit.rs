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
        fn plan_pos_dec(
            changes: &mut Vec<ExitSetChange>,
            field: &'static str,
            from: Decimal,
            to: Option<Decimal>,
        ) -> CoreResult<()> {
            let Some(to) = to else { return Ok(()) };
            if to <= Decimal::ZERO {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("{field} must be > 0, got {to}; nothing was applied"),
                ));
            }
            changes.push(ExitSetChange {
                field,
                from: from.to_string(),
                to: to.to_string(),
                effect: "live",
            });
            Ok(())
        }
        fn plan_nonneg_dec(
            changes: &mut Vec<ExitSetChange>,
            field: &'static str,
            from: Decimal,
            to: Option<Decimal>,
        ) -> CoreResult<()> {
            let Some(to) = to else { return Ok(()) };
            if to < Decimal::ZERO {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("{field} must be >= 0, got {to}; nothing was applied"),
                ));
            }
            changes.push(ExitSetChange {
                field,
                from: from.to_string(),
                to: to.to_string(),
                effect: "live",
            });
            Ok(())
        }
        fn plan_nonneg_i64(
            changes: &mut Vec<ExitSetChange>,
            field: &'static str,
            from: i64,
            to: Option<i64>,
        ) -> CoreResult<()> {
            let Some(to) = to else { return Ok(()) };
            if to < 0 {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("{field} must be >= 0, got {to}; nothing was applied"),
                ));
            }
            changes.push(ExitSetChange {
                field,
                from: from.to_string(),
                to: to.to_string(),
                effect: "live",
            });
            Ok(())
        }
        fn plan_pos_i64(
            changes: &mut Vec<ExitSetChange>,
            field: &'static str,
            from: i64,
            to: Option<i64>,
        ) -> CoreResult<()> {
            let Some(to) = to else { return Ok(()) };
            if to <= 0 {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("{field} must be > 0, got {to}; nothing was applied"),
                ));
            }
            changes.push(ExitSetChange {
                field,
                from: from.to_string(),
                to: to.to_string(),
                effect: "live",
            });
            Ok(())
        }
        fn plan_bool(
            changes: &mut Vec<ExitSetChange>,
            field: &'static str,
            from: bool,
            to: Option<bool>,
        ) {
            if let Some(to) = to {
                changes.push(ExitSetChange {
                    field,
                    from: from.to_string(),
                    to: to.to_string(),
                    effect: "live",
                });
            }
        }

        plan_pos_dec(
            &mut changes,
            "stopLossPct",
            current.stop_loss_pct,
            patch.stop_loss_pct,
        )?;
        plan_pos_dec(
            &mut changes,
            "takeProfitPct",
            current.take_profit_pct,
            patch.take_profit_pct,
        )?;
        if let Some(v) = patch.ceiling_tp_price {
            if v < Decimal::ZERO || v > Decimal::ONE {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "ceilingTpPrice must be in [0, 1] (0 = off), got {v}; nothing was applied"
                    ),
                ));
            }
            changes.push(ExitSetChange {
                field: "ceilingTpPrice",
                from: current.ceiling_tp_price.to_string(),
                to: v.to_string(),
                effect: "live",
            });
        }
        plan_pos_dec(
            &mut changes,
            "holderStopPct",
            current.holder_stop_pct,
            patch.holder_stop_pct,
        )?;
        plan_nonneg_i64(
            &mut changes,
            "forceExitSec",
            current.force_exit_sec,
            patch.force_exit_sec,
        )?;
        plan_nonneg_i64(
            &mut changes,
            "minTimeLeftSec",
            current.min_time_left_sec,
            patch.min_time_left_sec,
        )?;
        plan_bool(
            &mut changes,
            "simpleExitEnabled",
            current.simple_exit_enabled,
            patch.simple_exit_enabled,
        );
        plan_bool(
            &mut changes,
            "dynamicStopEnabled",
            current.dynamic_stop_enabled,
            patch.dynamic_stop_enabled,
        );
        plan_nonneg_i64(
            &mut changes,
            "stopTightenStartSec",
            current.stop_tighten_start_sec,
            patch.stop_tighten_start_sec,
        )?;
        plan_pos_dec(
            &mut changes,
            "stopMinPct",
            current.stop_min_pct,
            patch.stop_min_pct,
        )?;
        plan_bool(
            &mut changes,
            "trailingEnabled",
            current.trailing_enabled,
            patch.trailing_enabled,
        );
        plan_nonneg_dec(
            &mut changes,
            "trailingMinHighPct",
            current.trailing_min_high_pct,
            patch.trailing_min_high_pct,
        )?;
        plan_nonneg_dec(
            &mut changes,
            "minTrailPct",
            current.min_trail_pct,
            patch.min_trail_pct,
        )?;
        plan_bool(
            &mut changes,
            "proportionalTrailEnabled",
            current.proportional_trail_enabled,
            patch.proportional_trail_enabled,
        );
        plan_nonneg_dec(
            &mut changes,
            "proportionalTrailPct",
            current.proportional_trail_pct,
            patch.proportional_trail_pct,
        )?;
        plan_nonneg_dec(
            &mut changes,
            "proportionalTrailMinPct",
            current.proportional_trail_min_pct,
            patch.proportional_trail_min_pct,
        )?;
        plan_nonneg_dec(
            &mut changes,
            "proportionalTrailMinGivebackPct",
            current.proportional_trail_min_giveback_pct,
            patch.proportional_trail_min_giveback_pct,
        )?;
        plan_bool(
            &mut changes,
            "tightStopEnabled",
            current.tight_stop_enabled,
            patch.tight_stop_enabled,
        );
        plan_pos_dec(
            &mut changes,
            "tightStopPct",
            current.tight_stop_pct,
            patch.tight_stop_pct,
        )?;
        plan_bool(
            &mut changes,
            "ratchetEnabled",
            current.ratchet_enabled,
            patch.ratchet_enabled,
        );
        if let Some(ticks) = patch.ratchet_confirm_ticks {
            if ticks == 0 {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    "ratchetConfirmTicks must be >= 1; nothing was applied".to_string(),
                ));
            }
            changes.push(ExitSetChange {
                field: "ratchetConfirmTicks",
                from: current.ratchet_confirm_ticks.to_string(),
                to: ticks.to_string(),
                effect: "live",
            });
        }
        plan_nonneg_dec(
            &mut changes,
            "ratchetConfirmTolerancePct",
            current.ratchet_confirm_tolerance_pct,
            patch.ratchet_confirm_tolerance_pct,
        )?;
        plan_nonneg_dec(
            &mut changes,
            "maxBidWickPct",
            current.max_bid_wick_pct,
            patch.max_bid_wick_pct,
        )?;
        plan_pos_i64(
            &mut changes,
            "maxLastPriceAgeSec",
            current.max_last_price_age_sec,
            patch.max_last_price_age_sec,
        )?;
        plan_nonneg_dec(
            &mut changes,
            "staleProfitPct",
            current.stale_profit_pct,
            patch.stale_profit_pct,
        )?;
        plan_nonneg_i64(
            &mut changes,
            "staleProfitBidUnchangedSec",
            current.stale_profit_bid_unchanged_sec,
            patch.stale_profit_bid_unchanged_sec,
        )?;
        plan_nonneg_dec(
            &mut changes,
            "stagnantProfitPct",
            current.stagnant_profit_pct,
            patch.stagnant_profit_pct,
        )?;
        plan_nonneg_i64(
            &mut changes,
            "stagnantDurationSec",
            current.stagnant_duration_sec,
            patch.stagnant_duration_sec,
        )?;
        plan_nonneg_dec(
            &mut changes,
            "depthCollapseThresholdPct",
            current.depth_collapse_threshold_pct,
            patch.depth_collapse_threshold_pct,
        )?;
        plan_nonneg_i64(
            &mut changes,
            "exitGraceSec",
            current.exit_grace_sec,
            patch.exit_grace_sec,
        )?;
        plan_bool(
            &mut changes,
            "makerExitsForTpOnly",
            current.maker_exits_for_tp_only,
            patch.maker_exits_for_tp_only,
        );
        plan_bool(
            &mut changes,
            "makerFirstExitEnabled",
            current.maker_first_exit_enabled,
            patch.maker_first_exit_enabled,
        );
        plan_bool(
            &mut changes,
            "strategyExitMakerFirst",
            current.strategy_exit_maker_first,
            patch.strategy_exit_maker_first,
        );
        if let Some(ms) = patch.exit_maker_timeout_ms {
            changes.push(ExitSetChange {
                field: "exitMakerTimeoutMs",
                from: current.exit_maker_timeout_ms.to_string(),
                to: ms.to_string(),
                effect: "live",
            });
        }
        plan_pos_i64(
            &mut changes,
            "maxBookAgeSec",
            current.max_book_age_sec,
            patch.max_book_age_sec,
        )?;
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
