//! Core assembly: the constructors, boot-time account/policy loading, the
//! execution-policy surface (IPC + reload) and the event sink. Pure code
//! motion from `service.rs` (#780) — no logic edits.

use super::*;

impl Core {
    /// #363: the boot-time policy load. Malformed input is audited
    /// (`load-refused`) and logged as a warning — the boot continues with the
    /// neutral (no-policy) kernel instead of dying. This mirrors the cockpit
    /// families' posture (warn + keep last-known-good): a bad POLICY_* key in
    /// `.env` must not take the whole kernel down, and the operator can fix
    /// the file and hot-reload without a restart.
    pub(super) fn load_execution_policy_at_boot(
        config: &CoreConfig,
    ) -> Option<crate::execution_policy::Policy> {
        let path = config
            .execution_policy_path
            .as_deref()
            .map(std::path::Path::new)?;
        // Single-source read (issue 751): the launcher folds .env into the
        // process environment, but the file face is read too so a same-checkout
        // tool spawn (no launcher) still sees the operator's POLICY_* keys.
        let env_file = config.env_file_path.as_deref().map(std::path::Path::new);
        match crate::execution_policy::Policy::load_single_source(path, env_file) {
            Ok(policy) => Some(policy),
            Err(reason) => {
                crate::execution_policy::append_audit_to(
                    &Self::execution_policy_audit_file(config),
                    &crate::execution_policy::PolicyAuditRecord {
                        ts_ms: now_ms(),
                        actor: crate::execution_policy::ACTOR_BOOT.to_string(),
                        action: "load-refused".to_string(),
                        account_id: "*".to_string(),
                        before: None,
                        after: None,
                        error: Some(reason.clone()),
                    },
                );
                tracing::warn!(
                    "execution_policy boot load refused ({reason}); continuing with the \
                     neutral kernel — fix .env and hot-reload, no restart needed"
                );
                None
            }
        }
    }

    // ── #363: execution policy surface (IPC + reload) ──────────────────────

    /// One section as the wire reports it: the EFFECTIVE view (own overrides
    /// folded over the globals), decimals as strings so the shortest-decimal
    /// rule survives the JSON hop.
    pub fn execution_policy_section_view(
        &self,
        account_id: &str,
    ) -> Option<crate::schema::ExecutionPolicySectionView> {
        let policy = self.execution_policy.as_ref()?;
        let s = policy.section_for(account_id);
        Some(crate::schema::ExecutionPolicySectionView {
            budget_ratio: Some(s.budget_ratio.to_string()),
            min_budget_usd: Some(s.min_budget_usd.to_string()),
            max_budget_usd: Some(s.max_budget_usd.to_string()),
            min_equity_usd: Some(s.min_equity_usd.to_string()),
            max_positions_per_asset: s.max_positions_per_asset,
            rules: Some(
                s.rules
                    .iter()
                    .map(|r| serde_json::to_value(r).unwrap_or(serde_json::Value::Null))
                    .collect(),
            ),
        })
    }

    /// The account ids the loaded policy names with a section of their own.
    pub fn execution_policy_account_ids(&self) -> Vec<String> {
        match &self.execution_policy {
            Some(p) => {
                let mut ids: Vec<String> = p.accounts.keys().cloned().collect();
                ids.sort();
                ids
            }
            None => Vec::new(),
        }
    }

    /// The journal file this config's execution-policy audit lines land in
    /// (#386): the config's private override, or the shipped
    /// `data/audit/execution_policy.jsonl` spelling when unset.
    fn execution_policy_audit_file(config: &CoreConfig) -> std::path::PathBuf {
        config
            .execution_policy_audit_path
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(crate::execution_policy::EXECUTION_POLICY_AUDIT_PATH)
            })
    }

    /// Whether the policy is loaded at all (the zero-config deployment has
    /// none — the read arms report that instead of inventing a section).
    pub fn execution_policy_loaded(&self) -> bool {
        self.execution_policy.is_some()
    }

    /// Append one audit line to THIS core's journal (#386). The wire set/
    /// reset arms and the reload refusal all land through here, so a test
    /// core with a private journal never touches the checkout's data dir.
    pub fn execution_policy_append_audit(
        &self,
        record: &crate::execution_policy::PolicyAuditRecord,
    ) -> bool {
        crate::execution_policy::append_audit_to(
            &Self::execution_policy_audit_file(&self.config),
            record,
        )
    }

    /// The audit journal for the history arm — read from THIS core's own
    /// journal file (#386), not a process-wide fixed path.
    pub fn execution_policy_audit(&self) -> Vec<crate::execution_policy::PolicyAuditRecord> {
        crate::execution_policy::load_audit_from(&Self::execution_policy_audit_file(&self.config))
    }

    /// #364: replay the CURRENT section over the recent closed trades of one
    /// account — the read-only fact behind the UI's "these rules would skip
    /// N of the last M entries". Every historical close supplies the facts a
    /// live entry would carry (balance is the LIVE ledger, not a per-trade
    /// snapshot: a ledger is a book, not a tape — disclosed in the row, not
    /// pretended into history), and `evaluate` — the ONE verdict function —
    /// judges it. The preview can never disagree with the kernel because it
    /// does not re-implement the kernel.
    pub fn execution_policy_preview(
        &self,
        account_id: &str,
        limit: usize,
    ) -> crate::schema::ExecutionPolicyPreviewResult {
        use crate::execution_policy::PolicyOutput;
        let now = now_ms();
        // Closed trades of THIS account, oldest first, capped at `limit` —
        // the same rows `trades.history` reports, judged instead of shown.
        let mut closed: Vec<&crate::position::ClosedPosition> = self
            .positions
            .closed_positions()
            .iter()
            .filter(|c| c.account_id.as_str() == account_id)
            .collect();
        if closed.len() > limit {
            closed.drain(0..closed.len() - limit);
        }
        // The live book facts (the parts of PolicyInput that are not
        // per-trade): balance/equity from the account's own ledger, the
        // trailing-hour PnL, the open count, the strategy streaks.
        let balance = self
            .accounts
            .ledger_of(&blitzkrieg_market_api::AccountId::from(account_id))
            .map(|l| l.balance())
            .unwrap_or_default();
        let open_for = self
            .positions
            .open_positions()
            .iter()
            // The preview replays the entry gates: the count must match the
            // entry path's LIVE-only rule (expired rounds are settlement-dead).
            .filter(|p| p.account_id.as_str() == account_id && p.expires_at_ms > now)
            .count() as u32;
        let mut considered = 0usize;
        let mut skipped = 0usize;
        let mut budgets: Vec<Decimal> = Vec::new();
        let mut rows = Vec::with_capacity(closed.len());
        for c in &closed {
            considered += 1;
            let facts = crate::execution_policy::PolicyInput {
                available_balance: balance,
                total_equity: balance,
                open_positions: open_for,
                current_price: c.entry_price,
                // The close is long over; the round it belonged to has
                // resolved. Zero is the honest "no clock running" fact.
                time_left_sec: 0,
                symbol: c.asset.clone(),
                recent_pnl_1h: self.positions.recent_pnl_1h_for(account_id, now),
                consecutive_losses: self.breaker.consecutive_losses(&c.strategy),
            };
            let (verdict, detail) = match self
                .execution_policy
                .as_ref()
                .map(|p| crate::execution_policy::evaluate(p, account_id, &facts))
            {
                None => ("refused".to_string(), Some("no policy loaded".to_string())),
                Some(Err(refusal)) => ("refused".to_string(), Some(refusal.reason)),
                Some(Ok(PolicyOutput::Place { budget_usd, .. })) => {
                    budgets.push(budget_usd);
                    ("place".to_string(), Some(budget_usd.to_string()))
                }
                Some(Ok(PolicyOutput::Skip { reason })) => {
                    skipped += 1;
                    ("skip".to_string(), Some(reason))
                }
                Some(Ok(PolicyOutput::Cooldown { seconds })) => {
                    skipped += 1;
                    ("cooldown".to_string(), Some(format!("{seconds}s")))
                }
            };
            let symbol = facts.symbol.clone();
            rows.push(crate::schema::ExecutionPolicyPreviewRow {
                ts_ms: c.exited_at_ms,
                symbol,
                balance,
                price: c.entry_price,
                verdict,
                detail,
            });
        }
        // `in` conditions on symbols judge the asset the trade named; nothing
        // special-cased here — `evaluate` already handles every field.
        let avg_budget_usd = if budgets.is_empty() {
            None
        } else {
            let sum = budgets.iter().copied().fold(Decimal::ZERO, |a, b| a + b);
            let n = Decimal::from(budgets.len() as u64);
            // Shortest-decimal discipline applies to the WIRE (the `to_string`
            // below is what crosses IPC), not to the Decimal itself — no
            // `normalize()` here, it would shave 10.00 down to "10" and break
            // the money-string convention every other field keeps.
            Some((sum / n).to_string())
        };
        crate::schema::ExecutionPolicyPreviewResult {
            considered,
            skipped,
            avg_budget_usd,
            rows,
        }
    }

    /// #757: remember the keys the IPC section write just made authoritative
    /// (`Some` = the value written, `None` = the key was swept). The write
    /// face calls this BEFORE [`Self::reload_execution_policy`] so the reload
    /// resolves those keys from memory rather than the stale startup export
    /// of the process environment.
    pub fn execution_policy_record_env_write(
        &mut self,
        written: std::collections::BTreeMap<String, Option<String>>,
    ) {
        self.execution_policy_env_writes.extend(written);
    }

    /// Replace the in-memory policy wholesale after a file write (set/reset
    /// arms). Re-derives the per-asset cap the way boot did — CLI flag wins
    /// over the file — and lands one `load-refused` audit line + keeps the
    /// OLD policy when the new file does not parse: fail-closed means a
    /// broken write never trades unguarded, it keeps the last good guard.
    pub fn reload_execution_policy(&mut self) -> Result<(), String> {
        let path = self
            .config
            .execution_policy_path
            .clone()
            .ok_or_else(|| "execution policy disabled (no config path)".to_string())?;
        // Single-source read (issue 751): the write face lands POLICY_* keys
        // in the .env the launcher owns — a same-process reload must see them
        // before the next restart folds the file into the process env. The
        // in-memory write snapshot (#757) rides between the file and the
        // process env so a stale boot export can neither shadow a write nor
        // resurface after a reset.
        let env_file = self
            .config
            .env_file_path
            .as_deref()
            .map(std::path::Path::new)
            .ok_or_else(|| "env_file_path not set".to_string())?;
        // SQL 行先取（本会话的写入已即时落库）；读取失败 fail-closed 保留
        // 旧策略（一次坏读不许让守卫消失）。
        let sql_settings = match self.account_risk_db.as_ref() {
            Some(db) => Some(
                db.load_settings()
                    .map_err(|e| format!("the SQLite policy sections are unreadable ({e})"))?,
            ),
            None => None,
        };
        let reload = match sql_settings.as_ref() {
            Some(sql) => crate::execution_policy::Policy::load_for_reload_with_sql(
                std::path::Path::new(&path),
                env_file,
                &self.execution_policy_env_writes,
                sql,
                |key| std::env::var(key).ok().filter(|v| !v.trim().is_empty()),
            ),
            None => crate::execution_policy::Policy::load_for_reload(
                std::path::Path::new(&path),
                env_file,
                &self.execution_policy_env_writes,
            ),
        };
        match reload {
            Ok(policy) => {
                let mut positions = self.config.positions.clone();
                positions.max_positions_per_asset = policy.defaults.max_positions_per_asset;
                if let Some(v) = self.config.max_positions_per_asset {
                    positions.max_positions_per_asset = v;
                }
                self.positions.set_config(positions);
                self.execution_policy = Some(policy);
                Ok(())
            }
            Err(reason) => {
                self.execution_policy_append_audit(&crate::execution_policy::PolicyAuditRecord {
                    ts_ms: now_ms(),
                    actor: crate::execution_policy::ACTOR_IPC.to_string(),
                    action: "load-refused".to_string(),
                    account_id: "*".to_string(),
                    before: None,
                    after: None,
                    error: Some(reason.clone()),
                });
                Err(reason)
            }
        }
    }

    /// Reload account configuration. 操作者裁决 2026-11-14：库里的
    /// `accounts` 表非空 → SQL 是权威（与 boot 同一规则，行从
    /// [`Self::ledger_book_from_rows`] 重建）；空表 → `.env` 的 ACCOUNT_*
    /// 兼容回退（只读）。Fails closed if the source is malformed, keeping
    /// the current configuration.
    pub fn reload_accounts(&mut self) -> Result<(), String> {
        let now_ms = blitzkrieg_market_api::net::now_ms();
        let allowed = &self.config.allowed_round_secs;
        let sql_rows = match self.account_risk_db.as_ref() {
            Some(db) => Some(db.load_accounts().map_err(|e| {
                format!("the durable account book is unreadable ({e}) — refusing the reload")
            })?),
            None => None,
        };
        let accounts = match &sql_rows {
            Some(rows) if !rows.is_empty() => Self::ledger_book_from_rows(
                rows,
                self.account_risk_db.as_ref().expect("checked above"),
                now_ms,
            ),
            _ => {
                let env_file = self
                    .config
                    .env_file_path
                    .as_deref()
                    .map(std::path::Path::new)
                    .ok_or_else(|| "env_file_path not set".to_string())?;
                // Env-only (issue 923): no legacy toml fallback — a .env without
                // ACCOUNT_LIST reloads into the zero-config single default.
                crate::account::config::load_account_ledgers_for_reload(env_file, now_ms, allowed)
                    .map_err(|e| e.message)?
            }
        };
        self.accounts.reload_config(accounts)
    }

    /// Reload shadow evolution switches. 操作者裁决 2026-11-14（#1230 起）：
    /// 开关的家是 `kernel_meta`（`.env` 的 SHADOW_* 只是旧部署回退，只读）。
    /// 库里有行读行；没有行才解析 env。
    pub fn reload_shadow(&mut self) -> Result<(), String> {
        let (enabled, auto_evolve) = if let Some(db) = self.account_risk_db.as_ref() {
            let enabled = db
                .load_kernel_meta(crate::risk::account_store::SHADOW_ENABLED_META)
                .map_err(|e| format!("kernel_meta read failed: {e}"))?;
            let auto = db
                .load_kernel_meta(crate::risk::account_store::SHADOW_AUTO_EVOLVE_META)
                .map_err(|e| format!("kernel_meta read failed: {e}"))?;
            if enabled.is_some() || auto.is_some() {
                let parse = |name: &str, v: Option<String>| match v.as_deref() {
                    Some("true") => Ok(true),
                    Some("false") => Ok(false),
                    Some(other) => Err(format!("{name} must be true/false, got: {other}")),
                    None => Ok(false),
                };
                (
                    parse("shadow_enabled", enabled)?,
                    parse("shadow_auto_evolve", auto)?,
                )
            } else {
                Self::shadow_switches_from_env(self.config.env_file_path.as_deref())?
            }
        } else {
            Self::shadow_switches_from_env(self.config.env_file_path.as_deref())?
        };
        if enabled {
            self.shadow_evolution
                .enable(blitzkrieg_market_api::net::now_ms() as i64);
        } else {
            self.shadow_evolution.disable();
        }
        self.shadow_evolution.set_auto_evolve(auto_evolve);
        Ok(())
    }

    /// The legacy env spelling of the two switches (read-only fallback).
    fn shadow_switches_from_env(env_path: Option<&str>) -> Result<(bool, bool), String> {
        let path = env_path
            .map(std::path::Path::new)
            .ok_or_else(|| "env_file_path not set".to_string())?;
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("failed to read .env: {e}"))?;
        let mut enabled = false;
        let mut auto_evolve = false;
        for line in content.lines() {
            if let Some((key, value)) = crate::env_file::parse_line(line) {
                match key.as_str() {
                    "SHADOW_ENABLED" => {
                        enabled = match value.as_str() {
                            "true" => true,
                            "false" => false,
                            other => {
                                return Err(format!(
                                    "SHADOW_ENABLED must be true/false, got: {other}"
                                ));
                            }
                        };
                    }
                    "SHADOW_AUTO_EVOLVE" => {
                        auto_evolve = match value.as_str() {
                            "true" => true,
                            "false" => false,
                            other => {
                                return Err(format!(
                                    "SHADOW_AUTO_EVOLVE must be true/false, got: {other}"
                                ));
                            }
                        };
                    }
                    _ => {}
                }
            }
        }
        Ok((enabled, auto_evolve))
    }

    pub fn set_event_sink(&mut self, tx: mpsc::UnboundedSender<Event>) {
        self.tx = Some(tx);
    }
}
