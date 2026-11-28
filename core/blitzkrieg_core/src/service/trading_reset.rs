//! The risk knobs' IPC writers and the kernel's error/daily-budget emitters:
//! `system.reset_trading`, `risk.limits`, `systemic.limits`, `exit.set`,
//! `round_duration.set`, the intent-decision audit and the daily-loss trip.
//! Moved verbatim from `service.rs` (pure code motion).

use crate::model::*;
use crate::position::EquityBasis;
use crate::schema::Event;
use blitzkrieg_market_api::net::now_ms;
use rust_decimal::Decimal;

use super::config::{AuditRuntime, LastError};
use super::{Core, GateReason};

impl Core {
    /// #458: 交易环境重置（`system.reset_trading`，UI 设置卡 + 确认词）。
    /// 把本核拥有的交易状态整表清零并重新 seed —— 停用全部已启用策略、撤销
    /// 全部 resting 挂单、清空订单/成交/持仓记录（内存 + 磁盘）、按
    /// `dry_seed_balance` 复原每个账户现金、重置连亏熔断。
    ///
    /// 三道门：Live 拒绝（真实资金记录是 append-only 证据，不是 UI 可擦的
    /// 演示盘）；`confirm` 必须逐字等于 `RESET`（自由文本确认门，不是勾选框
    /// —— 误触必须不可能被归咎于双击）；`manual_ops_audit_path: None`
    /// （回测器/进程内测试）拒绝 —— manual-ops 审计行是重置留下的唯一痕迹，
    /// 无处审计的擦除必须失败而不是静默发生。
    ///
    /// 保留（显式不动）：配置、策略文件、全部既有审计流、
    /// strategy-state.json（停用策略会走 `set_strategy_enabled` 的既有持久化
    /// 路径重写 enabled 集 —— 这是行为语义的一部分，不是泄漏）。
    pub fn reset_trading(
        &mut self,
        params: &crate::schema::ResetTradingParams,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::schema::ResetTradingResult> {
        use crate::runtime_config::append_manual_op;
        use crate::schema::{RESET_TRADING_LIVE_REFUSAL, ResetPreSummary, ResetTradingResult};

        if self.config.mode == Mode::Live {
            return Err(CoreError::new(
                CoreErrorCode::RiskRejected,
                RESET_TRADING_LIVE_REFUSAL.to_string(),
            ));
        }
        if params.confirm != "RESET" {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "system.reset_trading: confirm must be exactly \"RESET\" — \
                 this wipes every trading record this core owns"
                    .to_string(),
            ));
        }
        // issue 561 账户维隔离：params.account_id 在派发层已填缺省
        // （session → display → 进程 default 链，RAW params 判定显式 id），
        // 这里照字面执行。显式拒绝未知账户 —— 一个静默的空清洗比拒绝更糟。
        let account_id = match params.account_id.as_deref() {
            Some(id) => {
                let id = crate::account::AccountId::from(id);
                if self.accounts.account(&id).is_none() {
                    return Err(CoreError::new(
                        CoreErrorCode::InvalidParams,
                        format!(
                            "system.reset_trading: unknown account \"{id}\" — \
                             the reset scope must name a configured account"
                        ),
                    ));
                }
                id
            }
            // 兜底（直连内核、不经 IPC 派发层的调用方）：进程 display 格。
            None => crate::account::AccountId::from(self.accounts.display_id()),
        };
        let audit_path = self
            .config
            .manual_ops_audit_path
            .clone()
            .map(std::path::PathBuf::from);
        let Some(audit_path) = audit_path.as_deref() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "system.reset_trading: no manual-ops audit path configured — \
                 a wipe with no audit trail is refused (backtests and \
                 in-process tests carry None)"
                    .to_string(),
            ));
        };
        let audit_path = std::path::Path::new(audit_path);

        // 重置前快照：行数 + 余额 + 活跃集合。这是审计行与回执里唯一存活的
        // 「删了什么」的证据 —— 记录本身不会留下。issue 561: 全部行数按
        // 账户过滤 —— 全局数会谎报清洗范围（重置 B 不删 A 的行）。
        let pre = ResetPreSummary {
            order_log_lines: self
                .order_db
                .as_ref()
                .map(|db| {
                    db.load()
                        .iter()
                        .filter(|o| o.account_id.as_str() == account_id.as_str())
                        .count() as u64
                })
                .unwrap_or(0),
            trade_log_lines: self
                .trade_log
                .as_ref()
                .map(|l| {
                    // Count from the carrier, not from a file: this is the number
                    // the reset's own pre/post self-check compares, so reading a
                    // stale export would make a failed wipe look successful.
                    use crate::contracts::trades::{TradeRead, TradeScope};
                    match l.trade_totals(TradeScope::Account(
                        crate::contracts::store::AccountScope {
                            account: &account_id,
                        },
                    )) {
                        Ok(t) => t.total_trades,
                        Err(e) => {
                            // The count is the check's whole evidence; a silent 0
                            // would report "nothing was there" for an unreadable
                            // ledger. Loud, and 0 is the conservative reading of a
                            // number we could not obtain.
                            tracing::error!(
                                account = %account_id,
                                error = ?e,
                                "trade ledger count unavailable for the reset self-check"
                            );
                            0
                        }
                    }
                })
                .unwrap_or(0),
            position_log_lines: self
                .position_db
                .as_ref()
                .map(|db| {
                    db.load()
                        .iter()
                        .filter(|p| p.account_id.as_str() == account_id.as_str())
                        .count() as u64
                })
                .unwrap_or(0),
            open_positions: self
                .positions
                .open_positions()
                .iter()
                .filter(|p| p.account_id.as_str() == account_id.as_str())
                .count(),
            live_orders: self
                .ome
                .live_orders()
                .iter()
                .filter(|o| o.account_id.as_str() == account_id.as_str())
                .count(),
            balance: self
                .accounts
                .ledger_of(&account_id)
                .map(|l| l.balance())
                .unwrap_or(Decimal::ZERO),
        };

        // 1) 撤销目标账户的策略授权（Gitea #285 方案 B 裁决）：重置 = 彻底归零停手，
        //    同步停用该账户的引擎 scopes、将 SQLite `account_settings.strategies`
        //    写为 `Some("[]")` 并清空内存账户簿；重启后同样保持全关，直到操作员显式
        //    重新启用。账户维隔离：只动目标账户，其他账户的策略原样保留。
        //    持久化写入必须 fail-closed（R7.1 L4）：写失败则整次重置报错，绝不静默吞错。
        let account_scopes: Vec<String> = self
            .engine
            .as_ref()
            .map(|e| {
                e.strategy_scopes_all()
                    .into_iter()
                    .filter(|(a, _)| a == account_id.as_str())
                    .map(|(_, n)| n)
                    .collect()
            })
            .unwrap_or_default();
        let disabled: Vec<String> = {
            let mut names: Vec<String> = Vec::new();
            if let Some(e) = self.engine.as_ref() {
                for name in &account_scopes {
                    if e.enabled_strategies().iter().any(|n| n == name) {
                        names.push(name.clone());
                    }
                }
            }
            names.sort();
            names.dedup();
            names
        };
        if let Some(db) = self.account_risk_db.clone() {
            db.save_setting(
                account_id.as_str(),
                crate::risk::account_store::STRATEGIES_SETTING,
                Some("[]"),
            )
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!(
                        "system.reset_trading: failed to persist revoked strategies for account \
                         \"{account_id}\": {e}"
                    ),
                )
            })?;
        }
        for name in &disabled {
            self.set_strategy_enabled(name, false);
        }
        self.accounts
            .set_strategies(&account_id, Vec::new(), now_ms)
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!(
                        "system.reset_trading: failed to clear in-memory strategies for account \
                         \"{account_id}\": {e}"
                    ),
                )
            })?;

        // 2) 撤销目标账户的 resting 挂单。逐单走 `cancel` 释放 BUY 预留
        //    （在订单自己的账簿里，E28）、清 engine 的 pending-token 闩、记
        //    fast_orders_dirty；随后 remove_account_orders 负责「连记录都
        //    消失」的部分。账户维裁决：token=None 改为账户过滤，其他账户
        //    的挂单原样保留。
        let cancelled = {
            let ids: Vec<String> = self
                .ome
                .live_orders()
                .into_iter()
                .filter(|o| o.account_id.as_str() == account_id.as_str())
                .map(|o| o.order_id.clone())
                .collect();
            let mut n = 0usize;
            for id in &ids {
                if let Err(e) = self.cancel(id, now_ms) {
                    tracing::warn!(error = %e, order = %id, "reset_trading: cancel failed mid-reset");
                } else {
                    n += 1;
                }
            }
            n
        };

        // 3) 内存态按账户维清洗（issue 561 清单 A 项）+ 全局维照旧（清单
        //    B 项）。OME 不再整替：整替会把其他账户的订单/幂等表一起抹掉，
        //    只拿掉目标账户的订单行。副作用（tx 事件）不重放：重置不是一
        //    个需要逐单通知的操作。
        let live_before = pre.live_orders;
        self.ome.remove_account_orders(account_id.as_str());
        self.inflight_takers.clear();
        self.fast_path.fast_dirty_tokens.clear();
        self.fast_path.fast_eval_wake = false;
        self.fast_path.fast_orders_dirty = true;
        self.fast_path.fast_positions_dirty = true;
        // exit 路由三态：exit_reasons 的键是 token——只清仍归属目标账户的
        // token（其他账户可能持有同一 token 的独立行，账户↔venue 解耦后
        // 双账户可同持一 token）。strategy_exits 按意图自带 account_id 过滤。
        let account_tokens: std::collections::HashSet<String> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id.as_str() == account_id.as_str())
            .map(|p| p.token_id.clone())
            .chain(
                self.ome
                    .all()
                    .iter()
                    .filter(|o| o.account_id.as_str() == account_id.as_str())
                    .map(|o| o.token_id.clone()),
            )
            .collect();
        // 摘除该账户持仓/订单前先记下它的 token 集（上面），此刻 positions
        // 还未动 —— remove_account_positions 在第 4 步执行，token 集先取。
        let doomed_tokens: Vec<String> = {
            let before = self
                .positions
                .open_positions()
                .iter()
                .filter(|p| p.account_id.as_str() == account_id.as_str())
                .map(|p| p.token_id.clone())
                .collect::<std::collections::HashSet<_>>();
            account_tokens.union(&before).cloned().collect()
        };
        for t in &doomed_tokens {
            if let Some(reason) = self.exit_reason_ledger.exit_reasons.remove(t) {
                self.exit_reason_ledger.exit_reasons_evicted += 0; // 计数语义未变：清的是重置面
                let _ = reason;
            }
            self.exit_reason_ledger.exit_reason_order.retain(|k| k != t);
        }
        self.strategy_exits
            .retain(|x| x.account_id.as_str() != account_id.as_str());
        self.place_cooldowns.clear();
        self.escalation_skips.clear();
        self.pending_venue_cancels.clear();
        self.books.clear();
        // #834 cycle 1: the freeze/circuit-breaker five-liner collapsed into
        // the state book that owns it.
        self.freeze_book.reset_for_account(account_id.as_str());
        self.last_error = None;
        // #537: the strategy page's PnL counters are the OTHER survivor —
        // trade_db and the cash ledger reset, but these per-(strategy,account)
        // slices kept their closed_trades/wins/net_pnl_usd, so the panel's
        // 「真实余额 = 本金 + 净利」 kept folding a history the reset had
        // erased. A wiped environment reports zero for every counter.
        // issue 561: 只摘目标账户的切片（键尾 `\u{1}account`），其他账户
        // 的计数原样保留。
        let acct_suffix = format!("\u{1}{}", account_id.as_str());
        self.strategy_accounting
            .retain(|k, _| !k.ends_with(&acct_suffix));
        if let Some(l) = self.applied_log.as_ref() {
            l.compact(&[]);
        }
        self.breaker = crate::risk::LossBreakers::new(
            self.config.max_consecutive_losses,
            self.config.breaker_cooldown_sec,
        );
        self.risk.resume();
        // #1045: 入场闸门纯内存、重启即清，不落风险状态文件——这里只清
        // 该账户的入场闸门（与旧 freeze_book.scrub 同语义），不碰账本任何
        // 数字（不改 realized、不改 cap、不清亏损）。
        self.clear_account_gate(account_id.as_str());

        // Resetting a leg account clears the cross-venue arb config and pair book
        // so stale arb entries do not linger after an operator resets trading.
        if self.arb_leg_config.as_ref().is_some_and(|c| {
            c.leg_a_account.trim() == account_id.as_str()
                || c.leg_b_account.trim() == account_id.as_str()
        }) {
            if let Some(store) = self.store.as_deref()
                && let Err(e) = store.clear_arb_config()
            {
                tracing::warn!(
                    account = %account_id,
                    error = %e.message,
                    "reset_trading: clear_arb_config failed"
                );
            }
            self.arb_leg_config = None;
            self.pair_book = crate::pair::PairBook::default();
            if let Some(e) = self.engine.as_mut() {
                e.set_arb_leg_config(None);
            }
        }

        // 4) 持仓簿账户维摘除 + 磁盘记录按账户截断（不重启进程）。
        let open_before = pre.open_positions;
        let (removed_open, _removed_closed) =
            self.positions.remove_account_positions(account_id.as_str());
        let _ = removed_open;
        if let Some(db) = &self.position_db {
            // 保留其他账户的行原样落盘 —— 整簿 save(&[]) 会把 A 的持仓也
            // 抹掉（账户维裁决）。
            let remaining: Vec<crate::position::OpenPosition> =
                self.positions.open_positions().to_vec();
            db.save(&remaining);
        }
        if let Some(db) = &self.order_db {
            db.compact_account(account_id.as_str());
        }
        if let Some(log) = &self.trade_log {
            // The receipt names WHERE the erased rows were preserved (issue
            // #1253). The JSONL version returned a bare `usize`, so the archive
            // location was dropped on the floor and a reset could not be told
            // apart from an unarchived erasure.
            match log.retract_account(&account_id) {
                Ok(receipt) => tracing::info!(
                    account = %account_id,
                    removed = receipt.removed,
                    archived_to = %receipt.archived_to,
                    "account trade history erased, rows archived"
                ),
                Err(e) => tracing::error!(
                    account = %account_id,
                    error = ?e,
                    "account trade history erase FAILED — the ledger still carries this \
                     account's closes after a reset that reported success elsewhere"
                ),
            }
        }
        // #178/#175 的幂等簿与结算簿：重置后历史结算不再需要去重（记录文件
        // 已空，结算簿对着空文件重放会凭空恢复 claims）。全局维（清单 B）：
        // 结算簿键无账户归属，保持整簿重置 —— 账户维清算绝不产生跨账户
        // claims 复活。
        self.settlement = crate::settlement::SettlementBook::new(None);
        self.recon_watermark_ms = 0;

        // 记账审计与交易记录同生共死：trade_db 清零后，带着旧 realized 的锚点
        // 会让 trade_ledger_identity 永久假红（锚点不动但账目已归零），
        // halt_entries 把之后每一笔意图都拦在门外。重置即回到 boot 同款
        // AuditRuntime（保留 audit_log_path）——下一次审计在干净账目上重新
        // 取锚，与「重置=回到全新环境」的语义一致。
        let audit_log_path = self.audit.audit_log_path.clone();
        self.audit = AuditRuntime {
            audit_log_path,
            ..Default::default()
        };

        // 5) 重新 seed 目标账户到配置值（issue 561 账户维原语）。语义与
        //    启动 seed 一致（余额回到种子值），journal 重写为单行 `Reset`
        //    行 —— boot 回放后 balance == 种子（与自检断言一致，裁决 3 修
        //    复），其他账户的 journal 一字不动。
        let journal_rewrote = self
            .accounts
            .reset_account_to_seed(&account_id, self.config.dry_seed_balance)?;
        if !journal_rewrote {
            tracing::warn!(
                account = %account_id.as_str(),
                "issue 561: account journal rewrite failed at reset — the \
                 next boot may replay pre-reset rows"
            );
        }
        // #1253: the ledger was re-seeded above — re-open the day's loss book
        // on that same seed number, or the breaker keeps enforcing against
        // the pre-reset day (ledger said 0, the book said −7.33: two truths).
        self.positions.reset_daily_for_account(
            account_id.as_str(),
            now_ms,
            self.config.dry_seed_balance,
            self.equity_basis(),
        );
        // issue 723: the reset rewrote this account's journal into a single
        // `Reset` row — a NEW evidence generation with its own row-seq space.
        // The bound venues' cursors die here: the pre-reset numbers must
        // never judge post-reset facts.
        for v in self
            .venue_fact_cursor
            .keys()
            .filter(|v| self.venue_cash_account(v.as_str()).as_ref() == Some(&account_id))
            .cloned()
            .collect::<Vec<_>>()
        {
            self.venue_fact_cursor.remove(&v);
        }

        // 6) 自校验：目标账户 live 索引空 / 该账户 open 持仓空 / 该账户在
        //    三个记录文件中的行数归零 / 余额 == seed。其他账户的行不在断言
        //    面 —— 它们本就不该被清洗。
        let mut problems: Vec<String> = Vec::new();
        let leftover_live = self
            .ome
            .live_orders()
            .iter()
            .filter(|o| o.account_id.as_str() == account_id.as_str())
            .count();
        if leftover_live != 0 {
            problems.push(format!(
                "account {account_id} live index not empty after reset ({leftover_live} orders)"
            ));
        }
        let leftover_open = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id.as_str() == account_id.as_str())
            .count();
        if leftover_open != 0 {
            problems.push(format!(
                "account {account_id} open positions not empty after reset ({leftover_open} remain)"
            ));
        }
        if let Some(db) = &self.order_db {
            let n = db
                .load()
                .iter()
                .filter(|o| o.account_id.as_str() == account_id.as_str())
                .count();
            if n != 0 {
                problems.push(format!(
                    "account {account_id} order log still has {n} lines after reset"
                ));
            }
        }
        if let Some(l) = &self.trade_log {
            use crate::contracts::trades::{TradeRead, TradeScope};
            // A ledger we cannot read cannot be certified clean: "unreadable"
            // is reported as a problem, never folded into "0 rows left".
            match l.trade_totals(TradeScope::Account(crate::contracts::store::AccountScope {
                account: &account_id,
            })) {
                Ok(t) if t.total_trades != 0 => problems.push(format!(
                    "account {account_id} trade ledger still has {} rows after reset",
                    t.total_trades
                )),
                Ok(_) => {}
                Err(e) => problems.push(format!(
                    "account {account_id} trade ledger could not be read after reset ({e:?}) — \
                     the wipe cannot be certified"
                )),
            }
        }
        if let Some(db) = &self.position_db {
            let n = db
                .load()
                .iter()
                .filter(|p| p.account_id.as_str() == account_id.as_str())
                .count();
            if n != 0 {
                problems.push(format!(
                    "account {account_id} position log still has {n} lines after reset"
                ));
            }
        }
        let balances: Vec<(String, Decimal)> = vec![(
            account_id.as_str().to_string(),
            self.accounts
                .ledger_of(&account_id)
                .map(|l| l.balance())
                .unwrap_or(Decimal::ZERO),
        )];
        for (id, bal) in &balances {
            if *bal != self.config.dry_seed_balance {
                problems.push(format!(
                    "account {id} balance {bal} != seed {} after reset",
                    self.config.dry_seed_balance
                ));
            }
        }
        if !problems.is_empty() {
            let msg = format!(
                "system.reset_trading: post-reset self-check FAILED — {}",
                problems.join("; ")
            );
            // 自检失败也要留审计行（审计写失败再叠加错误）。
            let _ = append_manual_op(
                audit_path,
                &serde_json::json!({
                    "op": "reset_trading",
                    "phase": "self_check_failed",
                    "atMs": now_ms,
                    "actor": actor,
                    "account": account_id.as_str(),
                    "reason": params.reason,
                    "problems": problems,
                }),
            );
            return Err(CoreError::new(CoreErrorCode::Internal, msg));
        }

        // 7) manual-ops 审计行：重置留下的唯一痕迹。写失败 = 整体失败
        //    （状态已清但审计缺席是「不可追溯的擦除」，必须显式暴露）。
        append_manual_op(
            audit_path,
            &serde_json::json!({
                "op": "reset_trading",
                "atMs": now_ms,
                "actor": actor,
                "account": account_id.as_str(),
                "reason": params.reason,
                "mode": format!("{:?}", self.config.mode).to_lowercase(),
                "pre": {
                    "orderLogLines": pre.order_log_lines,
                    "tradeLogLines": pre.trade_log_lines,
                    "positionLogLines": pre.position_log_lines,
                    "openPositions": pre.open_positions,
                    "liveOrders": pre.live_orders,
                    "balance": pre.balance.to_string(),
                },
                "post": {
                    "disabledStrategies": disabled,
                    "cancelledOrders": cancelled,
                    "balances": balances.iter()
                        .map(|(id, b)| serde_json::json!({
                            "accountId": id, "balance": b.to_string()
                        }))
                        .collect::<Vec<_>>(),
                },
            }),
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!("system.reset_trading: manual-ops audit write failed: {e}"),
            )
        })?;

        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %account_id.as_str(),
            live_orders = live_before,
            open_positions = open_before,
            cancelled = cancelled,
            disabled = disabled.len(),
            "trading environment reset (account-scoped) — the account's \
             records wiped, its cash re-seeded; other accounts untouched"
        );

        Ok(ResetTradingResult {
            disabled_strategies: disabled,
            cancelled_orders: cancelled,
            cleared_open_positions: open_before,
            cleared_closed_trades: pre.trade_log_lines as usize,
            account_id: account_id.as_str().to_string(),
            balances,
            pre_summary: pre,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: params.reason.clone(),
        })
    }

    /// The ONE writer of the panel-visible last-error slot (#180). Every
    /// internal refusal path funnels through here — directly, or via
    /// [`Core::emit_error`] — so the slot cannot be written with a message no
    /// code classifies, and cannot be skipped because one path forgot. It is
    /// also what makes the slot a *fresh* fact: each write carries the instant
    /// the failure was observed, so a reader can tell a new failure from an old
    /// banner (`tsMs` moves, `message` is the newest reason).
    pub(super) fn note_error(&mut self, err: &CoreError, now_ms: i64) {
        self.last_error = Some(LastError {
            ts_ms: now_ms,
            code: err.code,
            message: err.message.clone(),
        });
    }

    /// Broadcast a structured error to Node (never swallowed) AND record it in
    /// the one panel-visible error slot.
    pub fn emit_error(&mut self, e: CoreError) {
        self.emit_error_at(e, now_ms());
    }

    /// [`Core::emit_error`] with the caller's own instant, for error paths that
    /// carry the event time (a sweep at its `ts`) rather than the time of this
    /// call.
    pub fn emit_error_at(&mut self, e: CoreError, now_ms: i64) {
        self.note_error(&e, now_ms);
        self.emit_error_event(e);
    }

    /// E25 (#331): ONE audit line per arbitrated suggestion — never folded,
    /// never blocking (a failed append warns once and the trade goes on) —
    /// plus AT MOST one `INTENT_DECISION` push per `(strategy, gate, reason)`
    /// per second (§12.3): the PUSH folds, the audit does not.
    pub(super) fn record_intent_decision(
        &mut self,
        intent: &crate::arbitration::StrategyIntent,
        outcome: &crate::arbitration::Outcome,
        now_ms: i64,
    ) {
        let intent_json = serde_json::to_value(&intent.request).unwrap_or(serde_json::Value::Null);
        let record = self.intent_audit.record(
            now_ms,
            intent.request.account_id.as_str(),
            &intent.request.strategy,
            &intent_json,
            &outcome.decision,
            &outcome.gates,
            outcome.latency_us,
        );
        // The throttle key is (strategy, gate, reason); for a non-rejection the
        // "reason" slot carries the status, so an approval storm folds too.
        let (gate, key_reason) = match &outcome.decision {
            crate::arbitration::Decision::Rejected { gate, reason, .. } => (
                format!("{gate:?}").to_uppercase(),
                format!("{reason:?}").to_uppercase(),
            ),
            _ => (
                outcome
                    .gates
                    .last()
                    .map(|g| format!("{:?}", g.gate).to_uppercase())
                    .unwrap_or_else(|| "UNKNOWN".into()),
                outcome.decision.status().to_string(),
            ),
        };
        if let Some(count) =
            self.intent_audit
                .throttle(&intent.request.strategy, &gate, &key_reason, now_ms)
        {
            let push = crate::arbitration::AuditSink::push_for(&record, count);
            self.emit(Event::IntentDecision {
                account_id: record.account_id.clone(),
                strategy: push.strategy,
                intent_id: push.intent_id,
                status: push.status,
                gate: push.gate,
                detail: push.detail,
                ts_ms: push.ts_ms,
                count: push.count,
            });
        }
    }

    /// Emit an error event WITHOUT touching the slot, for a consequence whose
    /// cause is already recorded: the slot must keep showing the error an
    /// operator has to act on (the raw venue refusal), not the kernel's own
    /// reaction to it (the freeze). Everything else goes through
    /// [`Core::emit_error`].
    pub(super) fn emit_error_event(&self, e: CoreError) {
        self.emit(Event::Error { error: e });
    }

    /// Broadcast a risk alert.
    pub fn emit_risk_alert(&self, code: CoreErrorCode, message: String) {
        self.emit(Event::RiskAlert { code, message });
    }

    /// Roll the daily-loss budget when the UTC day changes (P0 #173).
    ///
    /// The budget's base is the account's CASH EQUITY — the ledger's
    /// venue-reconciled balance (host `seed_balance`/`venue_free_balance` keep
    /// it honest in live, `dry_seed_balance` in dry). Only the kernel can read
    /// it: the percentage cap is relative to the day's opening equity, so a
    /// fixed dollar default stops being meaningless on a small book. Called
    /// once per tick, before the exit checks, so a position closed right after
    /// the boundary is already counted in the new day.
    /// Compute an account's total equity (`cash + open position cost basis`).
    /// Used by both `roll_daily_budget` and `unfreeze_account` so daily-loss
    /// caps never shrink when cash is deployed into open positions.
    pub(super) fn account_total_equity(&self, account_id: &crate::account::AccountId) -> Decimal {
        let cash = self
            .accounts
            .ledger_of(account_id)
            .map(|l| l.balance())
            .unwrap_or_default();
        let open_cost: Decimal = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| &p.account_id == account_id && p.shares > Decimal::ZERO)
            .map(|p| p.entry_price * p.shares)
            .sum();
        cash + open_cost
    }

    pub(super) fn roll_daily_budget(&mut self, now_ms: i64) {
        // `balance` is already gross of local reservations — adding `reserved`
        // here would double-count resting BUY commitments. However, filled
        // open positions have already debited `balance` at entry cost, so
        // equity MUST include `open_cost` (`entry_price * shares`); otherwise
        // opening positions looks like a 50%+ cash loss and `reconcile_same_day_base`
        // shrinks `opening_equity_usd` mid-day and falsely trips DailyLoss.
        let accounts: Vec<(crate::account::AccountId, Decimal)> = self
            .accounts
            .account_list()
            .iter()
            .filter(|a| a.status == crate::account::AccountStatus::Active)
            .map(|a| (a.id.clone(), self.account_total_equity(&a.id)))
            .collect();
        for (id, equity) in accounts {
            if let Some(roll) =
                self.positions
                    .roll_daily(id.as_str(), now_ms, equity, self.equity_basis())
            {
                let message = match roll.previous_day_index {
                    Some(prev) => format!(
                        "daily loss budget: UTC day {prev} closed at ${} realized{} — day {} opens with equity ${}, cap ${}",
                        roll.previous_realized_pnl_usd,
                        if roll.previous_tripped {
                            " (TRIPPED: entries were frozen)"
                        } else {
                            ""
                        },
                        roll.day_index,
                        roll.opening_equity_usd,
                        roll.limit_usd
                    ),
                    None => format!(
                        "daily loss budget: UTC day {} opens with equity ${}, cap ${} (realized loss resets at the UTC day boundary)",
                        roll.day_index, roll.opening_equity_usd, roll.limit_usd
                    ),
                };
                if roll.previous_tripped {
                    tracing::warn!(day = roll.day_index, account = %id.as_str(), "{}", message);
                } else {
                    tracing::info!(day = roll.day_index, account = %id.as_str(), "{}", message);
                }
                self.emit_risk_alert(
                    CoreErrorCode::RiskRejected,
                    format!("[{}] {}", id.as_str(), message),
                );
                // UTC 日边界真正翻过：日内熔断闸门自动到期，该账户新单入场恢复。
                // 账本不动，只解闸；若 realized 仍超上限，下一 tick 的
                // emit_daily_trip 会按账本重新上 DailyLoss 闸门——这是「按账本
                // 推导」，不是「回放冻结」。Manual / ConfigReadOnly /
                // ConfigSuspended 闸门不受此清除（它们不是日内熔断）。
                if roll.previous_day_index.is_some()
                    && let Some(gate) = self.account_gate(id.as_str())
                    && gate.reason_kind == GateReason::DailyLoss
                {
                    self.clear_account_gate(id.as_str());
                }
            }
        }
    }

    /// Which book the equity handed to [`Self::roll_daily_budget`] came from
    /// (P1 #235): the day's percentage cap is a share of an account, and the
    /// ledger balance means a different thing in each of them.
    ///
    /// Derived from the SAME predicate `Core::new` uses to decide which book the
    /// ledger is seeded from — a locally-settling mode gets `dry_seed_balance`,
    /// live gets the venue's money — so the label cannot drift from the number
    /// it describes. A mode that settles locally is trading the simulation even
    /// when it is called read-only, and it must not be told its base came from
    /// the venue.
    pub(super) fn equity_basis(&self) -> EquityBasis {
        if self.config.mode.settles_locally() {
            EquityBasis::Dry
        } else {
            EquityBasis::Live
        }
    }

    /// Report every exit withheld from becoming an order (P0 #177, F6): the
    /// audit trail must show "should have triggered" instead of a silent hold.
    ///
    /// The log line names the cause instead of calling all of them a suppressed
    /// protective stop (#267): a held TRAILING stop is not a held protective
    /// stop, and a reviewer reading "protective stop suppressed" about a profit
    /// rule is being told the wrong story. The wording lives on the event
    /// (`SuppressedStopEvent::message`), so the panel and the log cannot drift.
    pub(super) fn emit_suppressed_stops(&mut self) {
        for ev in self.positions.drain_suppressed_stops() {
            let message = ev.message();
            // #268 item 2: past `N x max_book_age_sec` unpriceable, this is no
            // longer a routine hold — it is a position that cannot act, and it
            // is raised to error level so it survives a warn filter. Two call
            // sites rather than a dynamic level: tracing's level is a compile-
            // time property of the callsite.
            if ev.escalated {
                tracing::error!(
                    position = %ev.position_id,
                    token = %ev.token_id,
                    cause = ?ev.cause,
                    bid = %ev.bid,
                    mid = %ev.mid,
                    stop_pct = %ev.stop_pct,
                    book_age_ms = ?ev.book_age_ms,
                    unpriceable_for_ms = ev.unpriceable_for_ms,
                    "{}",
                    message
                );
            } else {
                tracing::warn!(
                    position = %ev.position_id,
                    token = %ev.token_id,
                    cause = ?ev.cause,
                    bid = %ev.bid,
                    mid = %ev.mid,
                    stop_pct = %ev.stop_pct,
                    book_age_ms = ?ev.book_age_ms,
                    unpriceable_for_ms = ev.unpriceable_for_ms,
                    "{}",
                    message
                );
            }
            self.emit_risk_alert(CoreErrorCode::RiskRejected, message);
        }
    }
}
