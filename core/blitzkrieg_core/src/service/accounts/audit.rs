//! The in-kernel accounting audit and its journal (#780).

use crate::model::*;
use crate::reconcile::{AuditInput, AuditReport, CashIdentity, TradeLogStatus};
use crate::schema::Event;
use rust_decimal::Decimal;

// Only the test seam below names the ledger type directly; keep the import
// on the same gate so the default build carries neither (#825).
#[cfg(any(test, feature = "test-support"))]
use crate::ledger::Ledger;

use crate::service::AUDIT_HEARTBEAT_MS;

use super::Core;

impl Core {
    /// Assemble the audit's input for the active account.
    #[allow(dead_code)]
    pub(crate) fn build_audit_input(&self, now_ms: i64) -> AuditInput {
        let active_id = self.accounts.active_id().clone();
        self.build_audit_input_for(&active_id, now_ms)
    }

    /// Assemble the audit's input for a specific account's book. Cheap enough
    /// to run on the maintenance tick for every active account: a handful of
    /// sums over live orders, open positions and the reservation table.
    pub(crate) fn build_audit_input_for(&self, account_id: &AccountId, now_ms: i64) -> AuditInput {
        let mut open_buy_notional = Decimal::ZERO;
        let mut open_buys = 0usize;
        for o in self.ome.live_orders() {
            if o.side != Side::Buy || o.account_id != *account_id {
                continue;
            }
            open_buys += 1;
            open_buy_notional += o.price * (o.size - o.filled_size).max(Decimal::ZERO);
        }
        let ledger = self.accounts.ledger_of(account_id).ok();
        // A reservation is "stray" when the order behind it is not a tracked live
        // BUY: nobody can ever release it, so it is a permanent phantom hold.
        let stray_reservations: Vec<(OrderId, Decimal)> = ledger
            .map(|l| {
                l.reservations_snapshot()
                    .into_iter()
                    .filter(|(id, _)| {
                        !self
                            .ome
                            .get(id)
                            .map(|o| o.side == Side::Buy && o.status.is_live())
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default();
        // E28 (§9.2): every fold below scopes to `account_id` — the identity
        // describes ONE book, so a sibling account's realized PnL, pending
        // redemption, or open position must not move it.
        let (realized, seen_trades, trade_log) = match self.trade_log.as_ref() {
            Some(log) => {
                use crate::contracts::trades::{TradeRead, TradeScope};
                match log.trade_totals(TradeScope::Account(crate::contracts::store::AccountScope {
                    account: account_id,
                })) {
                    Ok(t) => (t.net_pnl, t.total_trades, TradeLogStatus::Readable),
                    Err(e) => {
                        tracing::error!(
                            account = %account_id,
                            error = ?e,
                            "accounting audit cannot read the trade ledger — \
                             the trade leg of the cash identity is UNVERIFIED"
                        );
                        (Decimal::ZERO, 0, TradeLogStatus::Unreadable)
                    }
                }
            }
            None => (Decimal::ZERO, 0, TradeLogStatus::Unconfigured),
        };
        let mut spent = Decimal::ZERO;
        let mut received = Decimal::ZERO;
        for p in self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id == *account_id)
        {
            spent += p.flows.entry_cost_usd + p.flows.entry_fee_usd;
            received += p.flows.proceeds_usd - p.flows.exit_fee_usd;
            received += p.flows.maker_rebate_usd;
        }
        let open_positions = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id == *account_id)
            .count();
        let account_venue = self
            .accounts
            .account(account_id)
            .map(|a| a.venue.clone())
            .unwrap_or_default();
        let venue_free = self
            .freeze_book
            .venue_free_by_venue
            .get(&account_venue)
            .map(|(t, b)| (*t, *b));
        let ledger_realigned = self.audit.last_realign.get(&account_venue).copied();
        let venue_simulated = self
            .plugin_modes
            .iter()
            .find(|p| p.name == account_venue)
            .map(|p| {
                p.modes.iter().any(|m| {
                    m.capabilities
                        .satisfies(blitzkrieg_market_api::MarketCapabilities::SIMULATED)
                })
            })
            .unwrap_or(false);
        AuditInput {
            now_ms,
            live: self.config.mode == Mode::Live,
            identity: CashIdentity {
                balance: ledger.map(|l| l.balance()).unwrap_or(Decimal::ZERO),
                receivable: self.settlement.receivable_usd_for(account_id.as_str())
                    + self
                        .settlement
                        .merge_receivable_usd_for(account_id.as_str()),
                realized,
                spent,
                received,
            },
            reserved: ledger.map(|l| l.reserved()).unwrap_or(Decimal::ZERO),
            ledger_balanced: ledger.map(|l| l.is_balanced()).unwrap_or(true),
            unfunded_reserved: ledger
                .map(|l| l.unfunded_reserved())
                .unwrap_or(Decimal::ZERO),
            open_buy_notional,
            open_buys,
            stray_reservations,
            venue_free: venue_free.map(|(_, b)| b),
            venue_free_at_ms: venue_free.map(|(t, _)| t),
            ledger_realigned,
            realign_explained_at: self
                .audit
                .realign_explained
                .get(account_venue.as_str())
                .copied(),
            open_positions,
            seen_trades,
            trade_log,
            venue_simulated,
        }
    }

    /// Run the accounting audit across EVERY active account (and always the
    /// active display account) so a non-active live account (e.g.
    /// `polysimulator` while `active_id` is an empty `default` book) is never
    /// blind to drift, stray reservations, or ledger mismatches.
    ///
    /// Returns the active account's report (preserving the single-account
    /// return contract for callers and tests).
    pub fn run_accounting_audit(&mut self, now_ms: i64) -> AuditReport {
        let active_id = self.accounts.active_id().clone();
        let mut target_ids: Vec<AccountId> = self
            .accounts
            .account_list()
            .iter()
            .filter(|a| a.status == crate::account::AccountStatus::Active)
            .map(|a| a.id.clone())
            .collect();
        if !target_ids.contains(&active_id) {
            target_ids.push(active_id.clone());
        }

        let mut active_report: Option<AuditReport> = None;

        for account_id in target_ids {
            let acct_key = account_id.as_str().to_string();
            let is_active = account_id == active_id;
            let account_venue = self
                .accounts
                .account(&account_id)
                .map(|a| a.venue.clone())
                .unwrap_or_default();
            let input = self.build_audit_input_for(&account_id, now_ms);
            let anchor = self
                .audit
                .anchors_by_account
                .get(&acct_key)
                .or(if is_active {
                    self.audit.anchor.as_ref()
                } else {
                    None
                });
            let report = crate::reconcile::audit_accounting(&input, anchor);

            if let Some(at_ms) = report.realign_claimed_at
                && !account_venue.is_empty()
            {
                self.audit
                    .realign_explained
                    .insert(account_venue.clone(), at_ms);
                tracing::info!(
                    account = %acct_key,
                    venue = %account_venue,
                    correction_at_ms = at_ms,
                    "trade-leg residual attributed to the venue's own ledger correction — \
                     one-shot excuse claimed; a lingering residual next audit comes back unexplained (issue #921)"
                );
            }

            if !self.audit.anchors_by_account.contains_key(&acct_key) && !report.structural_failure
            {
                self.audit
                    .anchors_by_account
                    .insert(acct_key.clone(), input.identity);
                if is_active {
                    self.audit.anchor = Some(input.identity);
                }
            }

            if is_active {
                self.audit.runs += 1;
            }
            let cross_failures = self
                .audit
                .cross_source_failures_by_account
                .entry(acct_key.clone())
                .or_insert(0);
            if report.ok {
                *cross_failures = 0;
            } else {
                self.audit.failures += 1;
                if report.structural_failure {
                    *cross_failures = 0;
                } else {
                    *cross_failures += 1;
                }
            }
            let cross_strike = *cross_failures >= 2;
            if is_active {
                self.audit.cross_source_failures = *cross_failures;
            }

            let should_halt = !report.ok
                && (report.structural_failure || cross_strike)
                && self.config.audit_halt_entries;
            let was_halted = self.audit.halted_accounts.contains_key(&acct_key);
            let mut resumed = false;

            if should_halt && !was_halted {
                self.audit
                    .halted_accounts
                    .insert(acct_key.clone(), report.note.clone());
                let message = format!(
                    "accounting audit FAILED [{}] — new entries blocked until it passes: {}",
                    acct_key, report.note
                );
                tracing::error!(account = %acct_key, drift = %report.drift_usd, "{}", message);
                eprintln!("core: accounting audit FAILED — see the error log line above");
                self.note_error(
                    &CoreError::new(CoreErrorCode::Internal, message.clone()),
                    now_ms,
                );
                self.emit(Event::RiskAlert {
                    code: CoreErrorCode::Internal,
                    message,
                });
            } else if should_halt {
                self.audit
                    .halted_accounts
                    .insert(acct_key.clone(), report.note.clone());
            } else if was_halted {
                self.audit.halted_accounts.remove(&acct_key);
                resumed = true;
                let message = format!(
                    "accounting audit recovered [{}] — new entries resumed ({})",
                    acct_key, report.note
                );
                tracing::info!(account = %acct_key, "{message}");
                eprintln!("core: accounting audit recovered — see the info log line above");
                self.emit(Event::RiskAlert {
                    code: CoreErrorCode::Internal,
                    message,
                });
            } else if !report.ok {
                let message = if report.realign_claimed_at.is_some() {
                    format!(
                        "accounting audit drift [{}] (venue correction claimed as the residual, issue 921): {}",
                        acct_key, report.note
                    )
                } else {
                    format!("accounting audit drift [{}]: {}", acct_key, report.note)
                };
                tracing::warn!(account = %acct_key, drift = %report.drift_usd, "{}", message);
                eprintln!("core: accounting audit drift — see the warn log line above");
                self.emit(Event::RiskAlert {
                    code: CoreErrorCode::Internal,
                    message,
                });
            }

            if is_active {
                let heartbeat = self.audit.last_heartbeat_ms == 0
                    || now_ms - self.audit.last_heartbeat_ms >= AUDIT_HEARTBEAT_MS;
                if !report.ok || should_halt || was_halted || resumed || heartbeat {
                    self.audit.last_heartbeat_ms = now_ms;
                    self.persist_audit_report(&report);
                }
                active_report = Some(report);
            } else if !report.ok || should_halt || resumed {
                self.persist_audit_report(&report);
            }
        }

        // Sync process-level halt view with per-account halt map.
        if let Some(reason) = self.audit.halted_accounts.get(active_id.as_str()).cloned() {
            self.audit.halted = true;
            self.audit.halt_reason = reason;
        } else if let Some((acct, reason)) = self.audit.halted_accounts.iter().next() {
            self.audit.halted = true;
            self.audit.halt_reason = format!("[{acct}] {reason}");
        } else {
            self.audit.halted = false;
            self.audit.halt_reason.clear();
        }

        let report = active_report.expect("active account is always audited");
        self.audit.report = Some(report.clone());
        self.audit.last_at_ms = now_ms;
        report
    }

    /// The latest audit verdict, for the panel (and for a caller that wants the
    /// report without running one).
    pub fn accounting_audit(&self) -> Option<&AuditReport> {
        self.audit.report.as_ref()
    }

    /// New entries are blocked by a failing accounting audit (issue #189).
    pub fn audit_blocks_entries(&self) -> bool {
        self.audit.halted || !self.audit.halted_accounts.is_empty()
    }

    /// Why new entries are blocked (empty when they are not).
    pub fn audit_halt_reason(&self) -> &str {
        &self.audit.halt_reason
    }

    /// The gate itself: refuses a NEW entry for `account_id` while that
    /// account's accounting audit is failing (or when a process-wide halt is
    /// active with no per-account map populated). Exits (SELL) never consult
    /// this — a drifted ledger must not be able to trap the bot in a position.
    pub(crate) fn audit_entry_gate_for(&self, account_id: &AccountId) -> CoreResult<()> {
        if let Some(reason) = self.audit.halted_accounts.get(account_id.as_str()) {
            return Err(CoreError::new(
                CoreErrorCode::RiskRejected,
                format!("accounting audit halted new entries [{account_id}]: {reason}"),
            ));
        }
        if self.audit.halted && self.audit.halted_accounts.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::RiskRejected,
                format!(
                    "accounting audit halted new entries: {}",
                    self.audit.halt_reason
                ),
            ));
        }
        Ok(())
    }

    /// Active-account entry gate wrapper kept for existing callers/tests.
    #[allow(dead_code)]
    pub(crate) fn audit_entry_gate(&self) -> CoreResult<()> {
        self.audit_entry_gate_for(self.accounts.active_id())
    }

    /// Mutable ledger access, for tests that must put the books into the exact
    /// state a bug leaves behind (a stranded reservation, cash that moved with no
    /// trade) and assert the audit NOTICES it. Gated behind cfg(test) or the
    /// opt-in `test-support` feature (#825: never in default), so no production
    /// build can reach it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn ledger_mut(&mut self) -> &mut Ledger {
        // E28: tests poking the books target the ACTIVE account's ledger —
        // the same default a 0.2 deployment has exactly one of.
        self.accounts.active_ledger_mut()
    }

    /// Append one audit verdict (or a late-fill record) to the audit log.
    pub(crate) fn persist_audit_value(&self, value: serde_json::Value) {
        let Some(path) = self.audit_log_path() else {
            return;
        };
        crate::jsonl::append(std::path::Path::new(path), &value);
    }

    pub(super) fn persist_audit_report(&self, report: &AuditReport) {
        match serde_json::to_value(report) {
            Ok(v) => self.persist_audit_value(v),
            Err(e) => tracing::warn!(error = %e, "cannot serialize audit report"),
        }
    }

    pub(crate) fn audit_log_path(&self) -> Option<&str> {
        self.audit.audit_log_path.as_deref()
    }
}
