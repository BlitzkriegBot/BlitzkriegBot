//! Restore / persist + venue-binding methods on `Core`. Pure code motion from
//! `service.rs` (no logic edits).

use super::{CloseCredential, CloseCredentialLine};
use crate::jsonl;
#[cfg(test)]
use crate::model::AccountId;
use crate::model::{CoreError, CoreErrorCode, Side};
use crate::schema::Event;
use crate::service::Core;
use rust_decimal::Decimal;
use std::collections::VecDeque;

impl Core {
    /// Restore the OME from the durable order log (crash recovery). Call once at
    /// startup, BEFORE trading begins. Live orders are re-adopted (with their
    /// venue ids) so the startup sweep can reconcile them; terminal orders are
    /// dropped and the log compacted to just the restored set.
    ///
    /// Also restores the applied-fill idempotency table (issue #178) — without it,
    /// the venue's own re-delivery of an already-booked trade id books the fill a
    /// second time after every restart — and re-establishes the BUY reservations
    /// those restored orders still commit (issue #181), which a fresh Ledger would
    /// otherwise start without.
    pub fn restore_orders(&mut self) -> usize {
        self.restore_close_credentials();
        let applied = self.restore_applied();
        if applied.0 > 0 {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message: format!(
                    "restored {} applied-fill key(s) from {} after restart (duplicate venue \
                     trades stay idempotent)",
                    applied.0, applied.1
                ),
            });
        }
        let Some(db) = self.order_db.as_ref() else {
            return 0;
        };
        let loaded = db.load();
        if loaded.is_empty() {
            return 0;
        }
        let live: Vec<crate::model::TrackedOrder> =
            loaded.into_iter().filter(|o| o.status.is_live()).collect();
        let n = live.len();
        self.ome.restore(live.clone());
        // The restored orders keep their ORIGINAL ids and stay in the OME
        // registry, while a fresh process's id counter starts at 1. Without
        // this, the first new placements walk straight into the restored ids
        // and the dedup guard refuses the collision — production saw
        // `order already registered: dry_6` right after a restart that
        // recovered one live order. Same rule the position recovery already
        // applies (advance `next_id` past the restored ids).
        let mut max_restored: Option<u64> = None;
        for o in &live {
            if let Some((_, n)) = o.order_id.rsplit_once('_')
                && let Ok(v) = n.parse::<u64>()
            {
                max_restored = Some(max_restored.unwrap_or(0).max(v));
            }
        }
        if let Some(max_id) = max_restored
            && max_id + 1 > self.next_id
        {
            self.next_id = max_id + 1;
        }
        db.compact(&live);
        if n > 0 {
            // A re-adopted order still commits its unfilled notional. The local
            // reservation table is empty on a fresh process, so without this the
            // reserve gate would believe the whole balance is free and the next
            // entries would over-commit against cash the venue has already
            // spoken for (issue #181).
            let mut restored_notional = Decimal::ZERO;
            for o in &live {
                if o.side != Side::Buy {
                    continue;
                }
                let remaining = (o.size - o.filled_size).max(Decimal::ZERO);
                let notional = o.price * remaining;
                // E28: the reservation re-commits cash in the ORDER'S OWN book —
                // a restored order must never spend from another account.
                if let Ok(l) = self.accounts.get_mut(&o.account_id) {
                    l.sync_buy_reservation(&o.order_id, notional);
                }
                restored_notional += notional;
            }
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message: format!(
                    "recovered {n} live order(s) from the order log after restart \
                     ({restored_notional} of BUY notional re-reserved)"
                ),
            });
        }
        n
    }

    /// #463: replay the close-credential journal — pushes whose key carries no
    /// later `consumed` tombstone re-enter the FIFO in file order, so a FAILED
    /// arriving after the restart still finds its credential. Called from
    /// [`Core::restore_orders`], NOT the constructor: recovery here follows the
    /// same explicit pattern as the order and position books, and a constructor
    /// that reads the journal would make every in-process `Core::new` (whose
    /// default config points at the process cwd's `data/orders/`) silently
    /// adopt credentials from unrelated tests sharing that directory.
    fn restore_close_credentials(&mut self) {
        let Some(path) = self.close_credential_log.clone() else {
            return;
        };
        let mut q: VecDeque<(String, CloseCredential)> = VecDeque::new();
        for line in jsonl::load::<CloseCredentialLine>(
            &path,
            "close-credential journal: skipped unparseable lines",
        ) {
            match line {
                CloseCredentialLine::Push { trade_id, cred } => {
                    q.push_back((trade_id, *cred));
                }
                CloseCredentialLine::Consumed { trade_id } => {
                    if let Some(i) = q.iter().position(|(tid, _)| *tid == trade_id) {
                        q.remove(i);
                    }
                }
            }
        }
        if !q.is_empty() {
            tracing::warn!(
                path = %path.display(),
                restored = q.len(),
                "close-credential journal replayed — FAILED rollbacks survive the restart"
            );
            self.close_credentials = q;
        }
    }

    /// Fold the persisted applied-fill journal into the OME. Returns
    /// `(keys restored, path)`.
    fn restore_applied(&mut self) -> (usize, String) {
        let Some(log) = self.applied_log.as_ref() else {
            return (0, String::new());
        };
        let records = log.load();
        if records.is_empty() {
            return (0, log.path().display().to_string());
        }
        self.ome.restore_applied(records);
        let n = self.ome.applied_len();
        // The file was every mutation ever made; it is now the live table.
        self.ome.take_applied_journal();
        log.compact(&self.ome.applied_snapshot());
        (n, log.path().display().to_string())
    }

    /// Append whatever the OME journalled since the last drain. Called after every
    /// applied fill (the only thing that changes the table), so the file is
    /// crash-consistent with the in-memory table at all times.
    pub(super) fn persist_applied_journal(&mut self) {
        let records = self.ome.take_applied_journal();
        if records.is_empty() {
            return;
        }
        if let Some(log) = self.applied_log.as_ref() {
            log.append(&records);
        }
    }

    /// Persist one order's current snapshot. The store write result is
    /// returned explicitly; callers that cannot propagate (`emit_order`,
    /// `emit_fill`) log the error instead of discarding it.
    pub(super) fn persist_order(&self, id: &str) -> Result<(), CoreError> {
        if let (Some(db), Some(o)) = (self.order_db.as_ref(), self.ome.get(id)) {
            db.append(o);
        }
        Ok(())
    }

    /// Restore the OPEN position book from durable storage (crash recovery). Call
    /// once at startup, BEFORE trading begins. Without this a restart forgets
    /// every open position: it is no longer valued or exit-managed, so the trade
    /// drifts to expiry unmanaged. Mirrors `restore_orders`.
    pub fn restore_positions(&mut self) -> usize {
        let Some(db) = self.position_db.as_ref() else {
            return 0;
        };
        // The watermark outlives the position set: a flat book must still
        // remember which external fills were already accounted for.
        self.recon_watermark_ms = db.load_watermark();
        let loaded = db.load();
        // issue #942: the counter also clears the closed-trade history's own
        // max tail — the open log goes flat while the history keeps every
        // id ever issued, so the history is the surviving full memory.
        let history_max = self.trade_db.as_ref().map(|t| t.max_hft_tail());
        if loaded.is_empty() && history_max.unwrap_or(0) == 0 {
            return 0;
        }
        let n = loaded.len();
        self.positions.restore_open(loaded, history_max);
        if n > 0 {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message: format!(
                    "recovered {n} open position(s) from the position log after restart"
                ),
            });
        }
        // A settlement whose close never landed is a position that looks open but
        // is already paid: finish it before anything trades (issue #175).
        self.recover_settlements();
        n
    }

    /// #987: the unverified exposure report — positions closed via the fallback
    /// path whose exit price was a bookkeeping value (last known mid, else entry
    /// price) that no venue verdict ever confirmed. Separate from realized PnL;
    /// never merged into it.
    pub fn unverified_exposure_report(&self) -> crate::position::UnverifiedExposureReport {
        self.positions.unverified_exposure_report()
    }

    /// Test-only: rebind an account's venue. Market host/set tests keep
    /// `default` unbound so they are not affected by issue #1127's production
    /// binding (default -> polymarket); that business behaviour is exercised
    /// by account/mod.rs tests instead.
    #[cfg(test)]
    pub fn set_account_venue_for_test(&mut self, id: &AccountId, venue: &str) {
        let _ = self.accounts.set_venue(id, venue, 0);
    }

    /// #987, issue 991: reconcile backfill — mark fallback-closed positions on
    /// `condition_id` `source == "reconciled"` when venue cash truth arrives,
    /// keeping the fallback exit price (issue 991: a price that was estimated
    /// is never re-labeled `venue`). Returns the number of rows marked.
    pub fn backfill_fallback_to_venue(&mut self, condition_id: &str) -> usize {
        self.positions.backfill_fallback_to_venue(condition_id)
    }

    /// Persist the current OPEN position set (best effort). Positions are few and
    /// short-lived, so the whole set is rewritten on every change rather than
    /// appended (a close needs no tombstone).
    pub(super) fn persist_positions(&self) {
        if let Some(db) = self.position_db.as_ref() {
            db.save(self.positions.open_positions());
        }
    }
}
