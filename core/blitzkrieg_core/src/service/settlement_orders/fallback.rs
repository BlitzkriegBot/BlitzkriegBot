//! The unverifiable-expiry fallback family (issue #991 + #1236): the
//! force-settle sweep (过期 10s 强制结算，操作者裁决 2026-11-15), policy-
//! switched cash-vs-receivable booking, receivable aging banner feed, and
//! local redemption at venue truth (resolution / reconcile / token priced).
//! Split verbatim from `settlement.rs` (pure code motion, no logic edits) —
//! the file-size ratchet (#780) does not rule content, it rules length.

use crate::model::*;
use crate::schema::Event;
use crate::service::config::UnverifiedExpiryPolicy;
use crate::settlement::{
    EXPIRY_ESTIMATE_SETTLE_AFTER_MS, PRICE_SOURCE_UNOBSERVED, UNVERIFIED_EXPIRY_EXIT_REASON,
    fallback_binary_price,
};
use rust_decimal::Decimal;

use super::Core;

impl Core {
    /// #1236 操作者裁决 2026-11-15：**过期轮次强制结算——轮次过期 10s 内
    /// 完成**，不区分 venue 是否应答（原 120s 沉默窗一并取代）。binary 市场
    /// 到期时价格天然收敛到 1/0，持仓上最后已知的报价（feed 在过期前最后
    /// 几秒写下）就是裁决结果的最佳估计；过期 10s 后平台侧结算已可确认。
    ///
    /// whose expiry is long past. The close is deliberately NOT a settlement:
    ///   * price = the last known mid THRESHOLDED for a binary market:
    ///     `> 0.5` → 1, `<= 0.5` → 0 (the settlement value domain is {0, 1},
    ///     never 0.x; the exact-0.5 tie falls to 0). `0` (a dead feed) is
    ///     indistinguishable from "never observed": the price is booked as 0 at
    ///     `PRICE_SOURCE_UNOBSERVED` and the notional rides the receivable
    ///     pool (or credits zero cash under the cash policy) — entry price is
    ///     never substituted, so no fake cost recovery is invented.
    ///   * NO redemption claim is booked (`SettlementBook::book` is not
    ///     called): with no verified verdict, redeeming a "winner" would be
    ///     redeeming an invention. The venue's own cash truth folds in through
    ///     the existing reconcile/realign sweep, which is exactly why this
    ///     price may stay conservative;
    ///   * the trade record's exit reason is `unverifiable_expiry`
    ///     ([`UNVERIFIED_EXPIRY_EXIT_REASON`]), so history can never be read
    ///     as a venue verdict.
    ///
    /// Idempotent by construction: the position leaves the open book on the
    /// first close, so the second pass finds nothing; the watch list forgets
    /// the market through [`Self::sync_settlement_watch`] once its last
    /// position is gone.
    pub(crate) fn run_expiry_fallback_sweep(&mut self, now_ms: i64) {
        // #1236 操作者裁决 2026-11-15：**过期轮次必须强制结算——轮次过期
        // 10s 内完成**，不再区分 venue 是否应答（原 120s 沉默窗 #930/#684
        // 一并取代）。binary 市场到期时价格天然收敛到 1/0（实机截图：过期
        // 仓位 Cur = 0.99 / 0.01 / 0.97），持仓上最后已知的报价就是裁决结果
        // 的最佳估计——它正是 feed 在过期前最后几秒写下的价格。过期 10s
        // 后平台侧结算已可确认，等待只会产出幽灵仓位。裁决晚到的真实现金
        // 仍由 reconcile sweep 对平，与 #991 的现金裁决点一致。
        let targets: Vec<crate::position::OpenPosition> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| {
                // A position without a condition id has no settlement path at
                // all (nothing to query, nothing to reconcile against) — that
                // is a data-integrity defect, not this sweep's to guess at.
                // The same for an expiry that PRE-DATES the entry by more than
                // EXPIRY_TRUST_MS: the recorded end is not a market end
                // (track_markets' own exclusion — settling it would price one
                // market with another market's verdict).
                !p.condition_id.is_empty()
                    && p.expires_at_ms > 0
                    && p.entered_at_ms - p.expires_at_ms <= crate::settlement::EXPIRY_TRUST_MS
                    && now_ms - p.expires_at_ms > EXPIRY_ESTIMATE_SETTLE_AFTER_MS
            })
            .cloned()
            .collect();
        if targets.is_empty() {
            return;
        }
        let closed_count = targets.len();
        for pos in &targets {
            // Binary market fallback price, derived from the position's TYPED
            // observation state (issue #176): the settlement value domain is
            // {0, 1}, never 0.x — the last observed quote direction decides.
            // An observation above 0.5 settles 1; at-or-below settles 0 (the
            // exact-0.5 tie falls to 0; venue truth corrects at redeem time).
            // `Unobserved` yields `None`: there is no quote to threshold, and
            // entry_price / last mid / any mean are structurally unavailable
            // here — the booked price is zero at PRICE_SOURCE_UNOBSERVED, the
            // notional rides the receivable pool, and venue truth redeems it
            // later. The 0.5 threshold is hard-coded per issue #1238 — not an
            // env/toml knob.
            let (price, price_source) = match fallback_binary_price(&pos.price_state) {
                Some((price, basis)) => (price, basis.as_price_source()),
                // No observation: book 0 at `unobserved`. This is the row R7.1
                // L1 requires — an unknown printed as unknown, with the
                // notional carried as a receivable instead of a verdict.
                None => (Decimal::ZERO, PRICE_SOURCE_UNOBSERVED),
            };
            let closed = self.positions.close_priced(
                &pos.id,
                price,
                UNVERIFIED_EXPIRY_EXIT_REASON,
                true,
                now_ms,
                "fallback",
                price_source,
            );
            if let Some(closed) = closed {
                // The position left the open book, so the audit identity's
                // spent/received folds shrink by its flows. Something must
                // cover that walk — the policy decides WHAT (issue #991):
                //   * `receivable` (the only surviving policy): the notional
                //     becomes a claim in the settlement receivable pool —
                //     estimated money is NOT tradable money — and redeems into
                //     cash (with #998 correction rows for the venue-vs-estimate
                //     delta) when venue truth arrives through the
                //     resolution/reconcile path. Issue #176 removed the `cash`
                //     arm entirely: it credited `price × shares` to spendable
                //     cash at a price nobody observed.
                self.book_expiry_fallback(&closed, price, price_source, now_ms);
                // Trade record first, then the snapshot — the same order the
                // settlement path closes with (the audit reads the record).
                self.on_position_closed(&closed, now_ms);
                tracing::warn!(
                    position = %closed.id,
                    condition = %closed.condition_id,
                    account = %closed.account_id.as_str(),
                    shares = %closed.shares,
                    fallback_price = %price,
                    price_source = %price_source,
                    policy = %self.config.unverified_expiry_policy.as_str(),
                    expired_ago_ms = now_ms - pos.expires_at_ms,
                    "position force-settled past expiry (>10s, no venue verdict): the last \
                     pre-expiry quote on a binary market is the best estimate of the 1/0 \
                     outcome; booked per the unverifiable-expiry policy"
                );
            }
        }
        self.persist_positions();
        // A market whose LAST position left via the force settle: the watch
        // would only forget it on the next drain's `sync_settlement_watch` —
        // forget it NOW (the same thing `on_market_resolution` does), so the
        // panel's `trackedMarkets` reads 0 immediately and nothing keeps
        // naming a market we no longer hold.
        for pos in &targets {
            if self
                .positions
                .open_positions()
                .iter()
                .all(|p| p.condition_id != pos.condition_id)
            {
                self.settlement.forget_market(&pos.condition_id);
            }
        }
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::Internal,
            message: format!(
                "expiry force settle: closed {} position(s) past expiry (>10s) without a \
                 venue verdict (unverifiable-expiry estimate at the last pre-expiry quote, \
                 booked per policy {})",
                closed_count,
                self.config.unverified_expiry_policy.as_str()
            ),
        });
    }

    /// Issue #991 boundary 1: the receivable-aging warn. When the OLDEST open
    /// fallback claim has waited longer than `BK_RECEIVABLE_MAX_AGE_MS`
    /// (default 48h), say so — once per episode: the warn fires on the
    /// crossing and stays quiet until the claim pool empties (redeemed) and
    /// refills past the bound again. The panel's amber banner is the
    /// CONTINUOUS face (`oldestFallbackClaimAgeMs` on every snapshot); this
    /// is the journal/log edge. Amber, never red: an aging receivable is
    /// attention, not a trading-down condition (red is reserved for the
    /// freeze and the failed self-check).
    pub(crate) fn check_receivable_aging(&mut self, now_ms: i64) {
        if self.config.unverified_expiry_policy != UnverifiedExpiryPolicy::Receivable {
            return;
        }
        let max_age = self.config.receivable_max_age_ms;
        if max_age <= 0 {
            return; // 0/negative = the check is off
        }
        let oldest = self.settlement.oldest_fallback_claim_age_ms(now_ms);
        match oldest {
            Some(age) if age > max_age && self.receivable_warned_age_ms == 0 => {
                self.receivable_warned_age_ms = age;
                let overdue = self
                    .settlement
                    .fallback_claims()
                    .filter(|c| now_ms - c.created_at_ms > max_age)
                    .count();
                let message = format!(
                    "receivable aging: {} fallback claim(s) overdue (oldest {age}ms > {max_age}ms) — \
                     the venue still has not answered; the receivable stays OUT of spendable cash \
                     until venue truth redeems it",
                    overdue.max(1)
                );
                tracing::warn!("{message}");
                self.emit(Event::RiskAlert {
                    code: blitzkrieg_market_api::CoreErrorCode::Internal,
                    message,
                });
            }
            None => {
                // The pool emptied (redeemed): re-arm the edge.
                self.receivable_warned_age_ms = 0;
            }
            _ => {}
        }
    }

    /// #684 shared ledger leg of the fallback close — THE one place both
    /// fallback call sites land. Without a compensating entry the position
    /// walks out of the open book and `trade_ledger_identity` drifts by
    /// exactly the closed notional; the receivable claim compensates it.
    ///
    /// Issue #176: this is the ONLY arm left. The `cash` policy
    /// (`credit_expiry_fallback_cash`) credited `price × dust_shares` straight
    /// into spendable cash at a price derived from `current_price` — which the
    /// open path seeded with `entry_price`, so a dead feed minted cash from the
    /// cost basis (R7.1 L1). Estimated money must never be tradable money; the
    /// cash policy had no way to keep that promise, so it is gone rather than
    /// gated.
    ///
    /// The notional becomes a minimal `ExpiryFallbackClaim` in the settlement
    /// receivable pool (the #461 merge-claim shape). Nothing credits cash. The
    /// claim rides the SAME pool (`receivable_usd_for`) the settled claims
    /// ride, so the audit's cash identity, the panel and the day budget all see
    /// it as the receivable it is. It redeems locally at venue truth
    /// (`redeem_fallback_claims_for_condition` / `_for_token`).
    ///
    /// The receivable arm is the ONLY policy the config can hold (#176 removed
    /// `Cash`); the guard below is therefore a structural assertion rather than
    /// a branch point, and a non-receivable policy reaching here would be a
    /// construction bug — it fails closed instead of silently crediting cash.
    pub(crate) fn book_expiry_fallback(
        &mut self,
        closed: &crate::position::ClosedPosition,
        price: Decimal,
        price_source: &str,
        now_ms: i64,
    ) {
        // Receivable model: book the claim from the position's OWN facts.
        // `dust_shares` is the un-sold remainder `close` priced at `price`;
        // `closed.net_pnl_usd` / `closed.exited_at_ms` are the retract triple
        // the redemption amends the trade record with.
        self.settlement.book_fallback_claim(
            &closed.id,
            &closed.condition_id,
            &closed.token_id,
            closed.account_id.as_str(),
            closed.dust_shares,
            price,
            price_source,
            closed.net_pnl_usd,
            closed.exited_at_ms,
            now_ms,
        );
    }

    /// Issue #991 — redeem fallback receivable claims at VENUE TRUTH (the
    /// receivable policy's redemption leg). A claim redeems when the venue
    /// prices its token: a resolution for the condition (the venue's own
    /// verdict), or a venue SELL fill on the token from the reconcile sweep
    /// (venue cash moved at a real price). NOT a on-chain redeem — a fallback
    /// claim's collateral was never verified, so there is no CTF call and no
    /// tx hash; this is the accounting event that converts receivable into
    /// cash at the first venue-confirmed price.
    ///
    /// The booking is the #998 two-row shape (never double-counts):
    ///   * the claim's own `Redemption` row credits the BOOKED (estimated)
    ///     amount — exactly what the receivable carried;
    ///   * a correction `Redemption` row moves
    ///     `venue_truth − booked` when the two differ. A ZERO-DELTA
    ///     redemption books NO correction row (a correction of nothing is
    ///     not a fact — `apply` skips zero amounts, and the comment on
    ///     [`crate::ledger::Ledger::credit_redemption_with_correction`]
    ///     records the deliberate choice).
    ///
    /// The anchored trade-leg identity closes the loop: Δcash must equal
    /// Δrealized, so the recorded trade's net is AMENDED by the same delta
    /// (matched by the retract triple — position id, exit time, booked net —
    /// captured at booking time). The amend keeps `source`/`price_source`
    /// untouched: the row stays flagged as an estimated close whose price
    /// was never a venue verdict.
    ///
    /// `skip` names a position whose close record was amended by this same
    /// sweep already (one redemption per position per pass).
    pub(crate) fn redeem_fallback_claims_at(
        &mut self,
        truth: &dyn Fn(&str) -> Option<Decimal>,
        now_ms: i64,
    ) -> usize {
        if self.config.unverified_expiry_policy != UnverifiedExpiryPolicy::Receivable {
            return 0;
        }
        // (position_id, account_id) pairs — the account is read BEFORE the claim
        // is consumed, so an unconfigured book refuses the redemption instead of
        // letting `redeem_fallback_claim` drop the claim into the void.
        let pending: Vec<(String, String)> = self
            .settlement
            .fallback_claims()
            .map(|c| (c.position_id.clone(), c.account_id.clone()))
            .collect();
        let mut redeemed = 0usize;
        for (position_id, claim_account) in pending {
            let Some(venue_price) = truth(&position_id) else {
                continue;
            };
            // The account lookup is part of the redemption's precondition, not a
            // post-hoc error branch: `redeem_fallback_claim` REMOVES the claim and
            // journals the redemption before returning, so a failure discovered
            // only after it (the old shape) left the receivable gone, the cash
            // uncredited and `redeemed` still incremented — a claim that vanished
            // with a "success" count next to it. Refusing first keeps the claim
            // in the pool for the next sweep, where it stays visible in
            // `pendingFallbackClaims` / `oldestFallbackClaimAgeMs` instead of
            // silently disappearing.
            let account = AccountId::from(claim_account.as_str());
            if self.accounts.get_mut(&account).is_err() {
                tracing::error!(
                    position = %position_id,
                    account = %claim_account,
                    "fallback claim names an unconfigured account — redemption refused, \
                     claim KEPT in the pool (cash not credited, receivable unchanged)"
                );
                continue;
            }
            let Some((account_id, booked, booked_net, closed_at_ms, shares, fallback_price)) =
                self.settlement.redeem_fallback_claim(&position_id, now_ms)
            else {
                continue;
            };
            let venue_usd = venue_price * shares;
            let delta = venue_usd - booked;
            let a = AccountId::from(account_id.as_str());
            match self.accounts.get_mut(&a) {
                Ok(l) => {
                    l.credit_redemption_with_correction(booked, venue_usd, &position_id);
                }
                Err(e) => {
                    // UNREACHABLE by construction: this pass resolved the same
                    // account above, before consuming the claim, and the whole
                    // redemption runs under one `&mut self` with no await in
                    // between. Kept as a loud error rather than an `expect`
                    // because panicking in a money path is worse than a row the
                    // operator has to read: if it ever fires, the claim IS
                    // already consumed (the pre-check was defeated), so this is
                    // the one branch that must page a human.
                    tracing::error!(
                        position = %position_id,
                        account = %account_id,
                        error = %e,
                        "fallback claim redemption lost its account between the \
                         precondition and the credit — claim consumed, cash NOT \
                         credited: manual reconciliation required"
                    );
                }
            }
            // The trade record's net moves with the cash (Δrealized == Δcash),
            // matched on the identity + the booked net captured at close.
            //
            // Every non-persisted outcome is LOUD: cash has already been
            // credited above, so a trade row that did not move means the ledger
            // identity now drifts by `delta` — a number an operator has to
            // reconcile by hand, never a warning to scroll past (R7.1 L4).
            if delta != Decimal::ZERO
                && let Some(log) = self.trade_log.as_ref()
            {
                let identity = crate::contracts::TradeIdentity {
                    id: position_id.clone(),
                    exit_time: closed_at_ms,
                };
                let account = AccountId::from(account_id.as_str());
                match log.amend_net_pnl(&account, &identity, booked_net, delta) {
                    Ok(crate::contracts::store::WriteOutcome::Persisted { .. }) => {}
                    Ok(other) => tracing::error!(
                        position = %position_id,
                        booked_net = %booked_net,
                        delta = %delta,
                        outcome = ?other,
                        "fallback redemption could not durably amend the trade record — \
                         trade_ledger_identity drifts by {delta}"
                    ),
                    Err(e) => tracing::error!(
                        position = %position_id,
                        booked_net = %booked_net,
                        delta = %delta,
                        error = ?e,
                        "fallback redemption could not amend the trade record — \
                         trade_ledger_identity drifts by {delta}; manual reconciliation required"
                    ),
                }
            }
            tracing::info!(
                position = %position_id,
                account = %account_id,
                booked_usd = %booked,
                venue_usd = %venue_usd,
                delta = %delta,
                fallback_price = %fallback_price,
                venue_price = %venue_price,
                "fallback receivable redeemed at venue truth (zero-delta redemptions book no correction row)"
            );
            self.emit(Event::RiskAlert {
                code: blitzkrieg_market_api::CoreErrorCode::Internal,
                message: format!(
                    "fallback receivable redeemed: position {position_id} booked {booked} USDC, \
                     venue truth {venue_usd} USDC (Δ{delta})",
                ),
            });
            redeemed += 1;
        }
        redeemed
    }

    /// Issue #991: redeem every fallback claim the RESOLUTION prices — the
    /// venue's own verdict for the condition, the strongest truth there is.
    /// A resolution that does not cover the claim's token leaves the claim
    /// open (the same posture an unpriced open position takes).
    pub(crate) fn redeem_fallback_claims_for_resolution(
        &mut self,
        resolution: &blitzkrieg_market_api::MarketResolution,
        now_ms: i64,
    ) -> usize {
        // Truth map first (an &self read), redemption second (&mut self) —
        // the same two-phase shape the settlement booking uses.
        let truth: std::collections::HashMap<String, Decimal> = self
            .settlement
            .fallback_claims()
            .filter(|c| c.condition_id == resolution.condition_id)
            .filter_map(|c| {
                resolution
                    .payout_per_share(&c.token_id)
                    .map(|p| (c.position_id.clone(), p))
            })
            .collect();
        if truth.is_empty() {
            return 0;
        }
        self.redeem_fallback_claims_at(&move |position_id| truth.get(position_id).copied(), now_ms)
    }
}
