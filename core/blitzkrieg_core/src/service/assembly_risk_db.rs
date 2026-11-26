//! Risk DB overlay: `open_account_risk_db`, `install_risk_db`,
//! `overlay_account_settings_from_db`, and the `account_risk_db_*` queries,
//! plus the cockpit re-derive gate and the budget-ratio conflict warn.
//! Extracted from `assembly.rs` — no logic edits.

use super::*;

impl Core {
    /// Open the per-account risk/exit store (issue #1189) and overlay its
    /// rows onto the working copies. The env-derived rows loaded at boot stay
    /// in place; the store is the stronger source, so its rows win. A missing
    /// file is an empty store (the env fallback stays). Idempotent: a second
    /// open replaces the first store and re-reads it.
    pub fn open_account_risk_db(
        &mut self,
        path: impl Into<std::path::PathBuf>,
    ) -> Result<(), String> {
        // 装配已经打开过库（生产路径，`config.db_path`）→ 不再二次打开：
        // 同一个句柄已经在位，重复 open 只会制造第二个连接。
        if self.account_risk_db.is_some() {
            return Ok(());
        }
        let db = crate::risk::account_store::AccountRiskDb::open(path)
            .map_err(|e| format!("account risk store open failed: {e}"))?;
        self.install_risk_db(db)
    }

    /// 把一个已打开的 `AccountRiskDb` 安装进核心：风控行、退出纪律、账户
    /// 设置（策略集合 / 轮次锚点）与进化开关句柄，全部来自同一连接。
    /// 生产装配（`with_account_book`）与测试入口（`open_account_risk_db`）
    /// 共用这一段——overlay 语义只有一份。
    pub(super) fn install_risk_db(
        &mut self,
        db: crate::risk::account_store::AccountRiskDb,
    ) -> Result<(), String> {
        // issue #1199 P1-1: a risk read failure is never silent-grown into an
        // empty map (the boot chain would read that as "use the env global"
        // and keep trading with widened limits). Fail-closed: refuse boot.
        let risks = db.load_risk()?;
        let exits = db.load_exits()?;
        let default_id = crate::model::default_account_id();
        if let Some(row) = risks.get(default_id.as_str()) {
            self.risk.config_mut().systemic.account = row.clone();
            // issue #1199 P1-6: 矩阵 max_daily_drawdown_usd 即日亏熔断预算（risk/limits.rs §4.2：
            // "THE SAME BUDGET … no second daily-loss counter exists"）。库里的行到位后同步进
            // PositionConfig，使日亏熔断器（daily_loss_gate.rs）读到持久化后的真实值；0 = off，
            // 语义与熔断器一致。
            self.config.positions.max_daily_loss_usd = row.max_daily_drawdown_usd.value();
        }
        if let Some(exit) = exits.get(default_id.as_str()) {
            self.config.positions.exit = exit.clone();
        }
        self.risk.config_mut().systemic.accounts.extend(risks);
        self.config.positions.account_exit.extend(exits);
        self.positions.set_config(self.config.positions.clone());
        // 操作者裁决 2026-11-14：轮次锚点的家是数据库，不是 `.env`。库里有行
        // 就以行为准（`.env` 的 `ACCOUNT_*_ROUND_DURATION_SEC` 只是旧部署的
        // 兼容回退）；写入面见 `round_duration.set`。这一层不读 env：env 已由
        // `with_account_book` 先铺好，行覆盖它。
        self.overlay_account_settings_from_db(&db)?;
        // 进化引擎的两个开关也是「库里的行才是真相」：把句柄交给引擎，
        // 运行时翻转直接落 `kernel_meta`，不再写 `.env`。
        self.shadow_evolution.attach_settings_db(db.clone());
        self.account_risk_db = Some(db);
        Ok(())
    }

    /// Overlay the `account_settings` rows onto the in-memory book:
    /// per-account round anchors (and, for the default account, the process
    /// default grid) plus the strategy enablement sets.
    ///
    /// Fail-closed on a value that would change the trading grid into
    /// something no plugin declared or the account did not subscribe to:
    /// the same refusal the env grammar applies, so a stale row can never
    /// arm a grid the operator did not choose. A merely unparseable row is a
    /// warn-and-skip (the env value stands) — one bad cell must not brick a
    /// boot, and it cannot widen anything.
    fn overlay_account_settings_from_db(
        &mut self,
        db: &crate::risk::account_store::AccountRiskDb,
    ) -> Result<(), String> {
        use crate::risk::account_store::{
            MAX_POSITIONS_SETTING, MAX_SHARES_SETTING, MIN_SHARES_SETTING, ROUND_DURATION_SETTING,
            SIZE_PCT_SETTING, STRATEGIES_SETTING,
        };
        let settings = db
            .load_settings()
            .map_err(|e| format!("account_settings read failed: {e}"))?;
        let default_id = crate::model::default_account_id();
        for (account_id, keys) in &settings {
            let account_id_owned = account_id.clone();
            let id = blitzkrieg_market_api::AccountId::from(account_id.as_str());
            if account_id.as_str() == default_id.as_str()
                && let Some(raw) = keys.get(SIZE_PCT_SETTING)
            {
                match raw.trim().parse::<Decimal>() {
                    Ok(v) if v >= Decimal::ZERO && v <= Decimal::ONE_HUNDRED => {
                        self.config.size_pct = v;
                        if let Some(engine) = self.engine.as_mut() {
                            engine.set_size_pct(v);
                        }
                    }
                    _ => tracing::warn!(
                        account = %account_id,
                        value = %raw,
                        "account_settings BK_SIZE_PCT is out of [0, 100] or unparseable — boot value stands"
                    ),
                }
            }
            if account_id.as_str() == default_id.as_str()
                && let Some(raw) = keys.get(MAX_POSITIONS_SETTING)
            {
                match raw.trim().parse::<usize>() {
                    Ok(n) => {
                        self.config.positions.max_positions = n;
                        self.positions.set_config(self.config.positions.clone());
                    }
                    Err(_) => tracing::warn!(
                        account = %account_id,
                        value = %raw,
                        "account_settings positions.max_positions is unparseable — boot value stands"
                    ),
                }
            }
            if account_id.as_str() == default_id.as_str() {
                let mut band_changed = false;
                if let Some(raw) = keys.get(MIN_SHARES_SETTING) {
                    match raw.trim().parse::<Decimal>() {
                        Ok(v) if v >= Decimal::ZERO => {
                            self.config.min_shares = v;
                            band_changed = true;
                        }
                        _ => tracing::warn!(
                            account = %account_id,
                            value = %raw,
                            "account_settings engine.min_shares is unparseable — boot value stands"
                        ),
                    }
                }
                if let Some(raw) = keys.get(MAX_SHARES_SETTING) {
                    match raw.trim().parse::<Decimal>() {
                        Ok(v) if v > Decimal::ZERO => {
                            self.config.max_shares = v;
                            band_changed = true;
                        }
                        _ => tracing::warn!(
                            account = %account_id,
                            value = %raw,
                            "account_settings engine.max_shares is unparseable — boot value stands"
                        ),
                    }
                }
                if band_changed {
                    if self.config.min_shares > self.config.max_shares {
                        self.config.min_shares = self.config.max_shares;
                    }
                    if let Some(engine) = self.engine.as_mut() {
                        engine.set_share_band(self.config.min_shares, self.config.max_shares);
                    }
                }
            }
            // Round anchor: only applied to accounts that exist in the book.
            if let Some(raw) = keys.get(ROUND_DURATION_SETTING) {
                let sec: i64 = match raw.trim().parse() {
                    Ok(sec) => sec,
                    Err(_) => {
                        tracing::warn!(
                            account = %account_id,
                            value = %raw,
                            "account_settings round anchor is not an integer — the boot-time                              env value stands for this account"
                        );
                        continue;
                    }
                };
                if !self.config.allowed_round_secs.contains(&sec) {
                    return Err(format!(
                        "refusing to boot: account `{account_id}`'s stored round anchor {sec}s is                          not declared by any market plugin (declared union: {:?}) — fix the                          account_settings row or add the duration to a plugin",
                        self.config.allowed_round_secs
                    ));
                }
                if let Some(acc) = self.accounts.account_mut(&id) {
                    let subscribed = acc.rounds.clone();
                    if !subscribed.is_empty() && !subscribed.contains(&sec) {
                        return Err(format!(
                            "refusing to boot: account `{account_id}`'s stored round anchor {sec}s is                              not in its subscribed round families ({subscribed:?}) — fix the                              account_settings row or the account's ROUNDS"
                        ));
                    }
                }
                // 操作者裁决：per-account 覆盖与进程默认网格分离。default 账户的
                // 覆盖只进 account_round_durations，不改 default_round_duration_sec
                // （gate 合同：effectiveSec 恒为出厂默认 900，perAccount.default=300）。
                self.config
                    .account_round_durations
                    .insert(account_id_owned.clone(), sec);
            }
            // 策略集合：行存在即为真相（显式空数组 = 本账户什么都不跑）。
            if let Some(raw) = keys.get(STRATEGIES_SETTING) {
                match serde_json::from_str::<Vec<String>>(raw) {
                    Ok(set) => {
                        if let Err(e) = self.accounts.set_strategies(&id, set, crate::db::now_ms())
                        {
                            tracing::warn!(
                                account = %account_id,
                                error = %e.message,
                                "account_settings strategies row could not be applied — \
                                 the boot-time env spelling stands for this account"
                            );
                        }
                    }
                    Err(e) => tracing::warn!(
                        account = %account_id,
                        error = %e,
                        "account_settings strategies row is not a JSON array — the boot-time \
                         env spelling stands for this account"
                    ),
                }
            }
        }
        Ok(())
    }

    /// True when a durable per-account store is open. `false` = env fallback.
    pub fn account_risk_db_open(&self) -> bool {
        self.account_risk_db.is_some()
    }

    /// The open durable store handle (`None` = no store on this core). The
    /// session boot uses it for the credential-row injection.
    pub fn account_risk_db_handle(&self) -> Option<crate::risk::account_store::AccountRiskDb> {
        self.account_risk_db.clone()
    }

    /// issue #1199 R2: whether the OPEN database carries no data at all —
    /// zero `account_risk` rows AND no `import_v1` marker. Only meaningful
    /// with a store open (an env-fallback core answers `false`: there is no
    /// database to be empty). A read error answers `false` as well — the
    /// probe must never turn a diagnostic into a boot failure, and the WARN
    /// it feeds is a diagnostic.
    pub fn account_risk_db_is_empty(&self) -> bool {
        self.account_risk_db
            .as_ref()
            .and_then(|db| db.db().is_empty().ok())
            .unwrap_or(false)
    }

    /// issue #1199 (the env watcher asymmetry): whether the env-file watcher
    /// may re-derive the cockpit working copies from `.env`. The DB is the
    /// cockpit carrier when one is open — a file re-derive would clobber the
    /// DB-sourced working copies with the file's stale `BK_*` / `BK_EXIT_*`
    /// keys on ANY unrelated `.env` write (an account toggle, a credential
    /// save). The re-derive is the env-carrier backend's freshness mechanism
    /// only; with the DB open the panel write path (the store seam) is the
    /// way in, and env is read-only at runtime (issue #1199 ruling).
    pub(crate) fn cockpit_env_rederive_allowed(&self) -> bool {
        self.account_risk_db.is_none()
    }

    /// issue #1189: `budget_ratio = 1` lets the execution policy size an order
    /// at 100% of the account's available balance, while
    /// `BK_MAX_ORDER_NOTIONAL_PCT < 100` refuses any single order above that
    /// share of equity. The two are individually legal and jointly a guaranteed
    /// per-order refusal at the exact size the policy asked for, so the save
    /// path names the conflict instead of letting the operator discover it as
    /// a rejected order.
    pub(crate) fn warn_if_budget_ratio_conflicts(&self, account_id: &str) {
        let Some(policy) = self.execution_policy.as_ref() else {
            return;
        };
        let ratio = policy.section_for(account_id).budget_ratio;
        let pct = self.risk.config().max_order_notional_pct;
        if ratio == Decimal::ONE && pct < Decimal::from(100) {
            tracing::warn!(
                target: "risk",
                account = %account_id,
                budget_ratio = %ratio,
                max_order_notional_pct = %pct,
                "budget_ratio=1.0 conflicts with max_order_notional_pct<100: the policy will size an \
                 order at 100% of available balance, then the per-order equity cap will refuse it. \
                 Raise BK_MAX_ORDER_NOTIONAL_PCT to 100 or lower the account's budget_ratio."
            );
        }
    }
}
