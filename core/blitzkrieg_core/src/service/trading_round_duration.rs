//! Round duration IPC writer: `round_duration.set` (per-account grid anchor,
//! seconds-cut/flatten/switch). Moved verbatim from `trading_reset.rs` (pure
//! code motion).

use crate::model::*;

use super::Core;

impl Core {
    /// 轮次时间周期的运行时写路径（`round_duration.set`，UI 设置卡）：per-account
    /// 白名单校验后把 `ACCOUNT_<ID>_ROUND_DURATION_SEC` 落进 SQLite
    /// `account_settings`（key=`round.duration_sec`，操作者裁决 2026-11-14：
    /// `.env` 对内核只读）并更新内存，秒切秒换（先撤单/强平，再切网格）。
    /// 红线：撤单/强平 fail-closed，未全成网格不切；`reset` 清覆盖回落默认网格；
    /// `durationSec` 与 `reset` 恰二选一；无 SQLite 库的内核（回测器/
    /// 进程内测试）只在本会话生效并如实告知「未持久化」。
    ///
    /// 2026-11-13 操作者指令：切换即「秒切秒换」——不再有 immediate 开关。
    /// 写入一律先撤该账户在途轮市场的全部挂单，再按市价真实强平其仓位
    /// （PnL 按真实成交价，台账写 `operator_grid_switch`，审计日志带 actor），
    /// 最后切入新网格。任一步骤 fail-closed：撤单失败中止；强平不全如实
    /// 入账并报错，网格不切；`round_duration_source == "cli_or_env"`
    /// （flagLocked）时破坏性切换被如实拒绝。
    pub fn apply_round_duration_set(
        &mut self,
        patch: &crate::schema::SetRoundDurationParams,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::schema::RoundDurationUpdate> {
        use crate::schema::{ROUND_DURATION_SET_NOTE, RoundDurationUpdate};
        let account_id = &patch.account_id;
        if patch.reset && patch.duration_sec.is_some() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "round_duration.set: give durationSec or reset, not both".to_string(),
            ));
        }
        let sec = patch.duration_sec;
        if !patch.reset && sec.is_none() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "round_duration.set needs durationSec (one of {:?}) or reset: true".to_string(),
            ));
        }
        // Validate the account exists in the book, and keep ITS venue: a grid
        // anchor speaks for the account's own venue (an unbound account for the
        // venue-agnostic bucket) — never for the whole deployment.
        let account_venue = {
            let acc = self
                .accounts
                .account(&blitzkrieg_market_api::AccountId::from(account_id.as_str()))
                .ok_or_else(|| {
                    CoreError::new(
                        CoreErrorCode::InvalidParams,
                        format!("round_duration.set: no such account `{account_id}`"),
                    )
                })?;
            acc.venue.clone()
        };
        // 秒切秒换：flagLocked（显式启动参数在管）时破坏性切换不赢，如实拒绝——在任何账簿动作之前判定。
        if self.config.round_duration_source == "cli_or_env" {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "round_duration.set: 当前网格由 --round-sec / BK_ROUND_SEC \
                 显式锁定，写入会立即撤单并市价强平在途仓位，被拒绝——去掉该启动参数后重试"
                    .to_string(),
            ));
        }
        // duration_sec 的白名单与该账户 ROUNDS 订阅允许列表：先于一切账簿动作
        // 校验，这样写入的撤单/强平绝不会在随后被拒而留下半边天。
        if let Some(sec) = sec {
            if !self.config.allowed_round_secs.contains(&sec) {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "round_duration.set: {sec} is not declared by any market plugin \
                         (declared union: {:?}) — nothing was applied",
                        self.config.allowed_round_secs
                    ),
                ));
            }
            let acc = self
                .accounts
                .account(&blitzkrieg_market_api::AccountId::from(account_id.as_str()))
                .unwrap();
            if !acc.rounds.is_empty() && !acc.rounds.contains(&sec) {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "round_duration.set: {sec} is not in account `{account_id}`'s subscribed \
                         round families ({:?}) — nothing was applied",
                        acc.rounds
                    ),
                ));
            }
        }
        let persisted = self.account_risk_db.is_some();
        // 操作者裁决 2026-11-14：轮次落 SQLite `account_settings`（key=round.duration_sec），不再写 `.env`；持久优先失败即拒绝，无库仅内存生效。
        if let Some(db) = self.account_risk_db.clone() {
            let value = sec.map(|s| s.to_string());
            db.save_setting(
                account_id,
                crate::risk::account_store::ROUND_DURATION_SETTING,
                value.as_deref(),
            )
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("round_duration.set: SQLite 落盘失败，未做任何变更（{e}）"),
                )
            })?;
        } else {
            tracing::warn!(target: "risk", account = %account_id,
                "round_duration.set: no SQLite store — grid moves for this session only, nothing persisted");
        }

        // 撤单 → 市价强平 → 切网格的前两步，fail-closed（一律执行，秒切秒换）。
        // reset 与写入同样先强平：真实被平仓位数带回执（flattened_positions）。
        let flattened = self.cancel_and_flatten_account(account_id, now_ms, actor)?;
        // reset: clear the account's override from memory (already removed above).
        if patch.reset {
            self.config.account_round_durations.remove(account_id);
            // Scanner grid pool is unchanged (it records which families this
            // deployment runs, not the account's current pick). The engine's
            // account-anchor table MUST be re-pushed so evaluation un-scopes
            // this account immediately and falls back to the process default.
            if let Some(engine) = self.engine.as_mut() {
                engine.set_account_round_durations(self.config.account_round_durations.clone());
            }
            tracing::info!(
                target: "risk",
                actor = %actor,
                account = %account_id,
                reason = patch.reason.as_deref().unwrap_or(""),
                "round duration override cleared for account — falls back to default grid \
                 ({ROUND_DURATION_SET_NOTE})"
            );
            self.sync_family_feed_from_accounts();
            return Ok(RoundDurationUpdate {
                account_id: account_id.clone(),
                next_session_sec: self.config.default_round_duration_sec,
                reset: true,
                persisted,
                flattened_positions: flattened,
                at_ms: now_ms,
                actor: actor.to_string(),
                reason: patch.reason.clone(),
                note: ROUND_DURATION_SET_NOTE,
            });
        }
        // Write path: duration_sec is Some (guaranteed by validation above).
        // SQLite persist already done above (persist-first); now update memory and
        // anchor this account's family in the scanner's grid pool.
        let sec = sec.unwrap();
        self.config
            .account_round_durations
            .insert(account_id.clone(), sec);
        if let Some(engine) = self.engine.as_mut() {
            engine.anchor_scanner_grid(&account_venue, sec);
            // Re-scope this account's hosted strategies onto the new family
            // immediately — otherwise they keep seeing sibling-duration
            // markets and trip the admission conflict check.
            engine.set_account_round_durations(self.config.account_round_durations.clone());
        }
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %account_id,
            from = self
                .config
                .account_round_durations
                .get(account_id)
                .copied()
                .unwrap_or(self.config.default_round_duration_sec),
            to = sec,
            reason = patch.reason.as_deref().unwrap_or(""),
            "round duration set for account — live this session, persisted \
             ({ROUND_DURATION_SET_NOTE})"
        );
        self.sync_family_feed_from_accounts();
        Ok(RoundDurationUpdate {
            account_id: account_id.clone(),
            next_session_sec: sec,
            reset: false,
            persisted,
            flattened_positions: flattened,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: patch.reason.clone(),
            note: ROUND_DURATION_SET_NOTE,
        })
    }
}
