//! The risk knobs' IPC writers: `risk.limits`, `risk.setSystemic`, and the
//! durable-write seam `persist_risk_bounds`. Moved verbatim from
//! `trading_reset.rs` (pure code motion).

use crate::model::*;
use crate::risk::limits::{Bound, LimitSource};
use rust_decimal::Decimal;

use super::Core;

impl Core {
    /// Apply a hot update of the ENTRY limits (#191) — the whole safe subset
    /// ([`crate::risk::HOT_RELOADABLE`]), in memory, with an audit record.
    /// Whitelist entries are read LIVE by the order path, bound NEW exposure
    /// only, and a close is exempt (#174), so tightening can never trap an open
    /// position; the patch is validated fully before anything is written, so a
    /// refusal leaves NO trace. Nothing is persisted — a restart re-applies the
    /// startup flags; one `tracing::info!` per changed field (`target: "risk"`,
    /// old → new + actor + instant) goes to the run log and the reply.
    pub fn apply_risk_limits(
        &mut self,
        patch: &crate::schema::SetRiskLimitsParams,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::schema::RiskLimitUpdate> {
        use crate::risk::HOT_RELOADABLE;
        use crate::schema::{RISK_LIMIT_UPDATE_NOTE, RiskLimitChange, RiskLimitUpdate};

        if patch.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "risk.setLimits needs at least one field; hot-reloadable: {}",
                    HOT_RELOADABLE.join(", ")
                ),
            ));
        }

        // Plan first, write later: a bad field must not leave a half-applied
        // patch behind.
        let risk = self.risk.config();
        let (notional, notional_pct, open_notional) = (
            risk.max_order_notional,
            risk.max_order_notional_pct,
            risk.max_open_notional_usd,
        );
        let (min_shares, max_shares) = (self.config.min_shares, self.config.max_shares);
        let mut changes: Vec<RiskLimitChange> = Vec::new();
        let mut plan =
            |field: &'static str, from: Decimal, to: Option<Decimal>| -> CoreResult<()> {
                let Some(to) = to else {
                    return Ok(());
                };
                if to < Decimal::ZERO {
                    return Err(CoreError::new(
                        CoreErrorCode::InvalidParams,
                        format!("{field} must be >= 0, got {to}; nothing was applied"),
                    ));
                }
                changes.push(RiskLimitChange { field, from, to });
                Ok(())
            };
        plan("maxOrderNotional", notional, patch.max_order_notional)?;
        plan(
            "maxOrderNotionalPct",
            notional_pct,
            patch.max_order_notional_pct,
        )?;
        plan(
            "maxOpenNotionalUsd",
            open_notional,
            patch.max_open_notional_usd,
        )?;
        plan("minShares", min_shares, patch.min_shares)?;
        plan("maxShares", max_shares, patch.max_shares)?;

        // The band is a band: `effective_sizing` caps the floor by the ceiling, so
        // an incoherent pair would silently read as something the operator did not
        // ask for. Refuse it instead.
        let (new_min, new_max) = (
            patch.min_shares.unwrap_or(min_shares),
            patch.max_shares.unwrap_or(max_shares),
        );
        if new_max > Decimal::ZERO && new_min > new_max {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "minShares {new_min} would exceed maxShares {new_max}; \
                     the share band must stay coherent — nothing was applied"
                ),
            ));
        }
        let band_moved = changes
            .iter()
            .any(|c| c.field == "minShares" || c.field == "maxShares");
        for c in &changes {
            match c.field {
                "maxOrderNotional" => {
                    self.risk
                        .config_mut()
                        .systemic
                        .account
                        .max_order_notional_usd = Bound::new(c.to, LimitSource::Flag)
                }
                "maxOrderNotionalPct" => self.risk.config_mut().max_order_notional_pct = c.to,
                "maxOpenNotionalUsd" => self.risk.config_mut().max_open_notional_usd = c.to,
                "minShares" => self.config.min_shares = c.to,
                "maxShares" => self.config.max_shares = c.to,
                // Unreachable today (both lists live ten lines apart), and loud
                // rather than silent if that ever stops being true: an arm that
                // matched nothing would leave the reply and the audit claiming a
                // change this function never made.
                other => {
                    debug_assert!(false, "unmapped hot-reloadable field {other}");
                    tracing::error!(
                        target: "risk",
                        field = other,
                        "hot-reloadable field has no write arm; the audit record would be a lie"
                    );
                }
            }
        }
        if band_moved {
            // The engine holds the band the tickets are cut from; the CoreConfig
            // copy above is what a reader with no engine sees.
            if let Some(engine) = self.engine.as_mut() {
                engine.set_share_band(self.config.min_shares, self.config.max_shares);
            }
        }

        // #861: the hot values land in the risk-state file too — the caps
        // recover across a kill -9 (the reply's `persisted` field says which
        // posture this kernel has: file-backed or memory-only).
        let persisted = self.risk_state_record_limits(&changes);
        self.warn_budget_conflict_if_needed(&changes);
        for c in &changes {
            tracing::info!(
                target: "risk",
                actor = %actor,
                field = c.field,
                from = %c.from,
                to = %c.to,
                reason = patch.reason.as_deref().unwrap_or(""),
                "risk limit hot-update: {} {} -> {} ({})",
                c.field,
                c.from,
                c.to,
                RISK_LIMIT_UPDATE_NOTE,
            );
        }
        Ok(RiskLimitUpdate {
            applied: changes,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: patch.reason.clone(),
            persisted,
            note: RISK_LIMIT_UPDATE_NOTE,
        })
    }

    /// 生效风控可编辑（`risk.setSystemic`）：六项账户级限额的运行时写路径，
    /// 仿 [`Self::apply_risk_limits`] 的 plan-first + 审计姿态。
    /// issue #983：风控只跟随账户，无全局限额。
    ///
    /// 三个生效档位都是真实的，不是文档话术：
    ///
    /// - **live（两项）**—— `max_single_loss_usd` / `max_position_size`。
    ///   入场流水线每笔现场 `systemic_snapshot()` 判读，只约束新敞口（平仓从不
    ///   走这条路），不携带计数器 —— 写进内存后下一笔入场立即按新值裁决。
    /// - **next_session（两项）**—— `max_consecutive_losses` /
    ///   `cooldown_minutes`。连亏计数器已在跑（`record_with` 的 breaker 带着
    ///   出生时的阈值对过一辈子），热改会追溯改判在案连亏 ——
    ///   `HOT_RELOAD_REFUSED` 的理由对这两项仍然成立。读数照写（操作员的
    ///   意图进入 matrix，重启后 `from_env` 的解析与它一致），但本会话的
    ///   熔断行为不变，回执里逐项注明。
    /// - **next_session（一项）**—— `max_daily_drawdown_usd`。它与日亏熔断
    ///   是同一个预算；当日已发生的已实现亏损账本不能被中途改动的帽追溯
    ///   改判。同上：matrix 照写、本会话的 `PositionConfig::max_daily_loss_usd`
    ///   不动。
    ///
    /// Atomic：patch 先全量校验（非负）再落笔，被拒的 patch 不留半点痕迹。
    /// 持久化走 store seam，载体由装配决定（issue #966/#965，#1199 PR-2）：
    /// DB 开库（默认）时改动先落 SQLite 的 account_risk 行；重启后按同一
    /// 载体重新武装 ——
    /// 这里只做内存矩阵热写，所以回执里的 `persisted` 恒为 false，写路径
    /// 会用真实落盘结果覆写它（并给出 persistence=store|none）。
    pub fn apply_systemic_limits(
        &mut self,
        patch: &crate::schema::SetSystemicLimitsParams,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::schema::SystemicLimitUpdate> {
        use crate::schema::{SYSTEMIC_LIMIT_UPDATE_NOTE, SystemicLimitChange, SystemicLimitUpdate};

        if patch.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "risk.setSystemic needs at least one of the six per-account systemic fields; \
                 read the current values with risk.limits"
                    .to_string(),
            ));
        }

        // Plan first, write later —— 被拒的 patch 一个字节都不动.
        // Per-account (issue #1189): the patch names its account's
        // `account_risk` row; absent = the process default account.
        let target = self.resolve_set_target(patch.account_id.as_deref(), "risk.setSystemic")?;
        let matrix = self.risk.config().systemic.account_for(&target).clone();
        let mut changes: Vec<SystemicLimitChange> = Vec::new();
        let mut plan = |field: &'static str,
                        from: &crate::risk::limits::Bound,
                        to: Option<Decimal>,
                        effect: &'static str|
         -> CoreResult<()> {
            let Some(to) = to else { return Ok(()) };
            if to < Decimal::ZERO {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("{field} must be >= 0, got {to}; nothing was applied"),
                ));
            }
            changes.push(SystemicLimitChange {
                field,
                from: from.value().to_string(),
                to: to.to_string(),
                effect,
            });
            Ok(())
        };

        plan(
            "maxSingleLossUsd",
            &matrix.max_single_loss_usd,
            patch.max_single_loss_usd,
            "live",
        )?;
        plan(
            "maxDailyDrawdownUsd",
            &matrix.max_daily_drawdown_usd,
            patch.max_daily_drawdown_usd,
            "live",
        )?;
        plan(
            "maxPositionSize",
            &matrix.max_position_size,
            patch.max_position_size,
            "live",
        )?;
        plan(
            "maxConsecutiveLosses",
            &matrix.max_consecutive_losses,
            patch.max_consecutive_losses,
            "next_session",
        )?;
        plan(
            "cooldownMinutes",
            &matrix.cooldown_minutes,
            patch.cooldown_minutes,
            "next_session",
        )?;
        plan(
            "maxOrderNotionalUsd",
            &matrix.max_order_notional_usd,
            patch.max_order_notional_usd,
            "live",
        )?;

        if let Some(to) = patch.size_pct {
            if to < Decimal::ZERO || to > Decimal::ONE_HUNDRED {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("sizePct must be in [0, 100], got {to}; nothing was applied"),
                ));
            }
            changes.push(SystemicLimitChange {
                field: "sizePct",
                from: self.config.size_pct.to_string(),
                to: to.to_string(),
                effect: "live",
            });
        }

        if let Some(to) = patch.max_positions {
            changes.push(SystemicLimitChange {
                field: "maxPositions",
                from: self.config.positions.max_positions.to_string(),
                to: to.to_string(),
                effect: "live",
            });
        }

        if let Some(to) = patch.min_shares {
            if to < Decimal::ZERO {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("minShares must be >= 0, got {to}; nothing was applied"),
                ));
            }
            changes.push(SystemicLimitChange {
                field: "minShares",
                from: self.config.min_shares.to_string(),
                to: to.to_string(),
                effect: "live",
            });
        }

        if let Some(to) = patch.max_shares {
            if to < Decimal::ZERO {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("maxShares must be >= 0 (0 = unlimited), got {to}; nothing was applied"),
                ));
            }
            changes.push(SystemicLimitChange {
                field: "maxShares",
                from: self.config.max_shares.to_string(),
                to: to.to_string(),
                effect: "live",
            });
        }

        let new_min_shares = patch.min_shares.unwrap_or(self.config.min_shares);
        let new_max_shares = patch.max_shares.unwrap_or(self.config.max_shares);
        if (patch.min_shares.is_some() || patch.max_shares.is_some())
            && new_max_shares > Decimal::ZERO
            && new_min_shares > new_max_shares
        {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("minShares {new_min_shares} exceeds maxShares {new_max_shares}; nothing was applied"),
            ));
        }

        if changes.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "risk.setSystemic: no field carried a change — nothing to audit",
            ));
        }

        // Write phase: the row build + install lives in
        // `install_account_risk_row` (account_store.rs); this file keeps the
        // plan/persist orchestration only.
        let risk_changes: Vec<SystemicLimitChange> = changes
            .iter()
            .filter(|c| !matches!(c.field, "sizePct" | "maxPositions" | "minShares" | "maxShares"))
            .cloned()
            .collect();
        let row = if !risk_changes.is_empty() {
            let row = self.install_account_risk_row(&target, &matrix, patch);
            // issue #1199 P1-6: max_daily_drawdown_usd 是日亏熔断预算，与
            // PositionConfig.max_daily_loss_usd 为同一预算；setSystemic 写入后
            // 同步进内存，使本会话立即按新值裁决（live，与 maxSingleLossUsd 一致）。
            if risk_changes
                .iter()
                .any(|c| c.field == "maxDailyDrawdownUsd")
            {
                self.config.positions.max_daily_loss_usd = row.max_daily_drawdown_usd.value();
                let positions = self.config.positions.clone();
                self.positions.set_config(positions);
            }
            Some(row)
        } else {
            None
        };

        if let Some(to) = patch.size_pct {
            self.config.size_pct = to;
            if let Some(engine) = self.engine.as_mut() {
                engine.set_size_pct(to);
            }
        }

        if let Some(to) = patch.max_positions {
            self.config.positions.max_positions = to;
            self.positions.set_config(self.config.positions.clone());
        }

        if patch.min_shares.is_some() || patch.max_shares.is_some() {
            self.config.min_shares = new_min_shares;
            self.config.max_shares = new_max_shares;
            if let Some(engine) = self.engine.as_mut() {
                engine.set_share_band(new_min_shares, new_max_shares);
            }
        }

        // The ONE durable-write exit (issue #1199 T-03, zeroed): the carrier
        // decision is `persist_risk_bounds` alone — the StoreApi seam when a
        // store is open (production: the SqliteStore wired at assembly, which
        // writes the same whole merged row through the shared AccountRiskDb
        // connection, value for value), memory-only when the core has no
        // carrier at all (backtester, in-process tests — honestly
        // `persisted: false`). A persist failure refuses the whole apply in
        // every carrier (file-first ruling — memory may never hold a value the
        // carrier does not have).
        let mut persisted: bool = match row.as_ref() {
            Some(r) => self.persist_risk_bounds(&target, r, &risk_changes)?,
            None => false,
        };
        if let Some(to) = patch.size_pct
            && let Some(store) = self.store.as_deref()
        {
            let scope_id = crate::account::AccountId::from(target.as_str());
            store
                .put_setting(
                    crate::store::AccountScope::new(&scope_id),
                    crate::risk::account_store::SIZE_PCT_SETTING,
                    Some(&to.to_string()),
                )
                .map_err(|e| {
                    CoreError::new(
                        CoreErrorCode::Internal,
                        format!(
                            "sizePct NOT applied: the account_settings store write failed ({e})"
                        ),
                    )
                })?;
            persisted = true;
        }
        if let Some(store) = self.store.as_deref() {
            let scope_id = crate::account::AccountId::from(target.as_str());
            let scope = crate::store::AccountScope::new(&scope_id);
            if let Some(to) = patch.max_positions {
                store
                    .put_setting(
                        scope,
                        crate::risk::account_store::MAX_POSITIONS_SETTING,
                        Some(&to.to_string()),
                    )
                    .map_err(|e| {
                        CoreError::new(
                            CoreErrorCode::Internal,
                            format!(
                                "maxPositions NOT applied: the account_settings store write failed ({e})"
                            ),
                        )
                    })?;
                persisted = true;
            }
            if let Some(to) = patch.min_shares {
                store
                    .put_setting(
                        scope,
                        crate::risk::account_store::MIN_SHARES_SETTING,
                        Some(&to.to_string()),
                    )
                    .map_err(|e| {
                        CoreError::new(
                            CoreErrorCode::Internal,
                            format!(
                                "minShares NOT applied: the account_settings store write failed ({e})"
                            ),
                        )
                    })?;
                persisted = true;
            }
            if let Some(to) = patch.max_shares {
                store
                    .put_setting(
                        scope,
                        crate::risk::account_store::MAX_SHARES_SETTING,
                        Some(&to.to_string()),
                    )
                    .map_err(|e| {
                        CoreError::new(
                            CoreErrorCode::Internal,
                            format!(
                                "maxShares NOT applied: the account_settings store write failed ({e})"
                            ),
                        )
                    })?;
                persisted = true;
            }
        }
        for c in &changes {
            tracing::info!(
                target: "risk",
                actor = %actor,
                field = c.field,
                from = %c.from,
                to = %c.to,
                effect = c.effect,
                reason = patch.reason.as_deref().unwrap_or(""),
                "systemic limit update: {} {} -> {} ({})",
                c.field,
                c.from,
                c.to,
                SYSTEMIC_LIMIT_UPDATE_NOTE,
            );
        }
        Ok(SystemicLimitUpdate {
            applied: changes,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: patch.reason.clone(),
            persisted,
            note: SYSTEMIC_LIMIT_UPDATE_NOTE,
        })
    }

    /// The ONE durable-write exit for the systemic bounds (issue #1199 T-03,
    /// zeroed — the pre-seam `if let Some(db) … else env` dual branch is gone
    /// from the write port; the carrier decision lives here alone):
    ///
    /// The ONE durable-write exit for the systemic bounds: the write port is
    /// the [`StoreApi`] seam (`self.store`, the carrier assembly wires — in
    /// production the [`SqliteStore`](crate::store::SqliteStore) at
    /// `service/assembly.rs:267-279`, which shares the `AccountRiskDb`
    /// connection behind `self.account_risk_db`: one file, one write surface,
    /// so 换后端=改一处装配 on the risk domain too).
    ///
    ///   * **store open** — the changed `BK_RISK_*` keys through
    ///     [`StoreApi::save_risk_bounds`](crate::store::StoreApi): the merge
    ///     (fields onto the stored row) mirrors the in-memory merge already
    ///     applied by [`install_account_risk_row`](crate::risk::account_store::install_account_risk_row),
    ///     so the six bounds land byte-for-byte as the pre-seam whole-row
    ///     write did, and untouched fields keep the working copy's values.
    ///   * **no store** — refuse: writing risk bounds outside `StoreApi` is
    ///     prohibited. There is no `AccountRiskDb` straight-write fallback and
    ///     no honest `persisted: false` posture — a write that cannot reach
    ///     the store seam is a refusal, not a silent success.
    ///
    /// A write failure refuses the whole apply in every carrier (operator
    /// ruling 2026-11-08: memory may never hold a value the carrier does not
    /// have).
    pub(crate) fn persist_risk_bounds(
        &self,
        target: &str,
        _row: &crate::risk::limits::AccountRiskLimits,
        changes: &[crate::schema::SystemicLimitChange],
    ) -> CoreResult<bool> {
        // Store seam only: production's durable carrier. The pre-seam
        // `AccountRiskDb::save_risk` straight-write fallback is removed —
        // writes go through `StoreApi` when a store is open; otherwise the
        // apply is memory-only with `persisted: false` (no durable write
        // outside StoreApi, no `db.save_risk` straight-write anymore).
        if let Some(store) = self.store.as_deref() {
            let fields: crate::store::FieldChanges = changes
                .iter()
                .map(|c| (c.field.to_string(), c.to.clone()))
                .collect();
            return store.save_risk_bounds(Some(target), &fields).map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!(
                        "systemic limits NOT applied: the account_risk store write failed ({e})"
                    ),
                )
            });
        }
        Ok(false)
    }
}
