//! In-kernel accounting audit (issue #189).
//! Moved verbatim from `reconcile.rs` (pure code motion).
//!
//! The sweep above repairs against a venue snapshot the CALLER fetches, which in
//! practice meant an external script (`scripts/account-drift-check.mjs`) polling a
//! running core over IPC. Nothing inside the kernel noticed a drifting ledger, and
//! the drift sources this PR fixes (a reservation that never reached zero, an
//! idempotency table that forgot a trade across a restart) accumulate silently.
//!
//! This section is the in-kernel counterpart: pure logic over a snapshot `Core`
//! assembles from its own books, so it is unit-testable and, above all, ALERTING —
//! a failing audit halts NEW ENTRIES, persists a record and reaches the panel.
//!
//! Three independent views of the same money, deliberately of different strength:
//!
//!   structural  the core's own books against each other, or against another LOCAL
//!               record read under the same lock. No external fetch, no race, so
//!               a failure is believed on the first audit. Halts immediately.
//!   cross-source the venue's free cash against the ledger — the one figure that
//!               arrives asynchronously and can be a stale read, so two failures
//!               in a row are required to halt.
//!
//! The cash identity is the one `account-drift-check.mjs` asserts, in its anchored
//! form:
//!
//!   balance − anchor.balance == (realized − anchor.realized)   trade records
//!                             − (spent    − anchor.spent)      cash open positions paid
//!                             + (received − anchor.received)   cash they already returned
//!
//! The anchor telescopes the unknown opening cash out (a LIVE core has no seed, and
//! a restarted DRY core re-seeds its ledger without replaying history), and the
//! deltas chain back to the first audit — so it is exactly as strong as the global
//! form over a session while needing no knowledge of where the money started.
//!
//! It IS a real check, not a formality: cash that moved between two audits with no
//! trade behind it — a fill the WS never delivered and the venue silently healed,
//! a reservation that was never released — is exactly the residual it reports, and
//! the script's own docs call that out as the drift it exists to catch.

use crate::model::*;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Tolerance for a comparison that crosses the f64 boundary (the trade summary
/// is f64 on disk) or a venue report rounded to cents. 1e-4 USD is a hundredth
/// of a cent: far below any real drift, far above transport noise.
pub const AUDIT_TOL_USD: Decimal = Decimal::from_parts(1, 0, 0, false, 4);

/// Tolerance for the ONE cross-source leg: the venue's reported free cash. Two
/// cents — the same figure the plugin host already uses for this comparison
/// (`market::host::venue_free_balance`) — because the two sides are computed by
/// different parties (our reservation table vs the venue's own notion of what is
/// locked), and demanding exact equality there would halt entries over a cent of
/// rounding. Everything compared WITHIN the core uses [`AUDIT_TOL_USD`].
pub const AUDIT_VENUE_TOL_USD: Decimal = Decimal::from_parts(2, 0, 0, false, 2);

/// How long a venue's own ledger correction stays eligible to explain a
/// trade-leg residual (issue #921). The audit interval default is 30s and the
/// plugin host re-reports on every sweep/reconnect, so a real correction is
/// always re-applied long before this expires; what expires is a one-time
/// snapshot kept forever by the `last_realign` slot, which is exactly the
/// "standing green light" the one-shot attribution must not become.
pub const REALIGN_EXPLAIN_TTL_MS: i64 = 120_000;

/// The cash identity at one instant, for the anchor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CashIdentity {
    #[serde(with = "crate::decimal")]
    pub balance: Decimal,
    /// All-time realized net PnL from the trade log (the running summary, not a
    /// truncated history window).
    #[serde(with = "crate::decimal")]
    pub realized: Decimal,
    /// Cash open positions have paid (entry cost + entry fees).
    #[serde(with = "crate::decimal")]
    pub spent: Decimal,
    /// Cash open positions have returned (proceeds − exit fees).
    #[serde(with = "crate::decimal")]
    pub received: Decimal,
    /// Settled-but-unredeemed payout: collateral a resolved position has earned
    /// but that is still conditional tokens on-chain, not cash (issue #175).
    ///
    /// It belongs to the identity's TOTAL, and moves into `balance` only when the
    /// redeem transaction confirms — crediting it before that would claim money
    /// the venue cannot show, which is exactly the drift the live audit leg
    /// exists to catch.
    #[serde(default, with = "crate::decimal")]
    pub receivable: Decimal,
}

impl CashIdentity {
    /// Cash plus what is owed to us: the value the trade ledger must be able to
    /// explain.
    pub fn total(&self) -> Decimal {
        self.balance + self.receivable
    }

    /// What this identity says the total should be, given an anchor.
    fn expected_after(&self, anchor: &CashIdentity) -> Decimal {
        anchor.total() + (self.realized - anchor.realized) - (self.spent - anchor.spent)
            + (self.received - anchor.received)
    }
}

/// Everything the pure audit needs, assembled by `Core` from its own state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuditInput {
    pub now_ms: i64,
    /// True for the modes that trade against a venue (so a venue report exists
    /// to compare with).
    pub live: bool,
    pub identity: CashIdentity,
    /// What the ledger's reservation table currently totals.
    pub reserved: Decimal,
    /// `available + reserved == balance` (structural, see `Ledger::is_balanced`).
    pub ledger_balanced: bool,
    /// Reserved notional the local balance does not cover.
    pub unfunded_reserved: Decimal,
    /// Σ `limit_price * remaining` over live BUY orders — what the reservation
    /// table must equal exactly (issue #181).
    pub open_buy_notional: Decimal,
    pub open_buys: usize,
    /// Reservation entries whose order is not a tracked live BUY: a hold nobody
    /// can release. Reported by name so the operator can act.
    pub stray_reservations: Vec<(OrderId, Decimal)>,
    /// Last free-cash figure the venue plugin reported, with its timestamp.
    pub venue_free: Option<Decimal>,
    pub venue_free_at_ms: Option<i64>,
    /// The last time the ledger's cash was SET from the venue instead of moved by
    /// a fill: `(at_ms, delta)`. The plugin host does this on purpose (when
    /// nothing rests, the venue's free cash IS our cash), so a trade-leg residual
    /// that equals this delta is the venue's own correction rather than a local
    /// inconsistency — see the trade-leg check.
    ///
    /// Issue #921: the excuse is ONE-SHOT and FRESH. Once an audit attributes a
    /// residual to this correction, the attribution is recorded in
    /// `realign_explained_at` and the same correction never excuses a second
    /// residual; a fresher correction (the venue reporting again overwrites the
    /// slot) re-arms it. This is the consumption mark's OTHER half: the audit
    /// run finds it here.
    pub ledger_realigned: Option<(i64, Decimal)>,
    /// When an audit last attributed a trade-leg residual to the correction
    /// above (audit-instant ms; None = this correction has not been spent yet,
    /// also the shape for a venue that never realigned). Set by the audit run,
    /// cleared when the venue overwrites the slot with a newer correction: a
    /// NEW correction is fresh evidence, spendable once.
    pub realign_explained_at: Option<i64>,
    pub open_positions: usize,
    pub seen_trades: u64,
    /// False when no trade log is configured (the backtester, most in-process
    /// tests). The trade-leg check is then not covered rather than reported as a
    /// failure: with no log there is no record of realized PnL, and a closed
    /// position would look like cash that vanished.
    pub has_trade_log: bool,
    /// The active venue is a SIMULATED-capability site (its plugin declared
    /// `MarketCapabilities::SIMULATED` — a paper venue that settles against its
    /// own model). The venue leg is skipped for it: a paper venue freezes its
    /// own reported free cash when a BUY rests, while `balance − reserved` nets
    /// the commitment out, so the two figures are fee-level cents apart by
    /// construction and the leg falsely failed every audit (two strikes halted
    /// new entries on a venue where no drift exists). Every other check —
    /// ledger balance, trade leg, reservations — still runs.
    pub venue_simulated: bool,
}

/// One line of the audit: what was compared, against what, and by how much it
/// missed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditCheck {
    /// Stable machine name (`cash_identity`, `reservations_match_open_buys`, …).
    pub name: String,
    pub ok: bool,
    /// A local (same-source) failure is believed at once; a cross-source one
    /// needs a second consecutive failure before it halts.
    pub structural: bool,
    #[serde(with = "crate::decimal")]
    pub expected: Decimal,
    #[serde(with = "crate::decimal")]
    pub actual: Decimal,
    #[serde(with = "crate::decimal")]
    pub drift: Decimal,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offenders: Vec<String>,
}

impl AuditCheck {
    fn new(
        name: &str,
        structural: bool,
        expected: Decimal,
        actual: Decimal,
        detail: String,
    ) -> Self {
        Self::with_tol(name, structural, expected, actual, detail, AUDIT_TOL_USD)
    }

    /// Same check with an explicit tolerance — used by the venue leg, which is
    /// compared across two independent sources.
    fn with_tol(
        name: &str,
        structural: bool,
        expected: Decimal,
        actual: Decimal,
        detail: String,
        tol: Decimal,
    ) -> Self {
        let drift = actual - expected;
        Self {
            name: name.to_string(),
            ok: drift.abs() <= tol,
            structural,
            expected,
            actual,
            drift,
            detail,
            offenders: Vec::new(),
        }
    }

    fn pass(name: &str, structural: bool, detail: String, value: Decimal) -> Self {
        Self {
            name: name.to_string(),
            ok: true,
            structural,
            expected: value,
            actual: value,
            drift: Decimal::ZERO,
            detail,
            offenders: Vec::new(),
        }
    }
}

/// The audit's verdict for one instant. Serializes straight into the audit JSONL
/// and into the panel's `engineStats.accountingAudit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditReport {
    pub at_ms: i64,
    /// True when a venue is in play (the venue leg was evaluated).
    pub live: bool,
    pub ok: bool,
    /// A structural check failed: the core's own books disagree.
    pub structural_failure: bool,
    /// The largest absolute drift across all failing checks.
    #[serde(with = "crate::decimal")]
    pub drift_usd: Decimal,
    /// This audit's own verdict: halt new entries or not.
    pub halt_entries: bool,
    pub checks: Vec<AuditCheck>,
    /// The baseline this audit compared against (`None` on the first audit: the
    /// anchored identity is not defined until a clean audit takes the anchor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<CashIdentity>,
    /// Issue #921: this audit attributed the trade-leg residual to the venue's
    /// own ledger correction stamped at this instant. Serialized so the JSONL
    /// shows the excuse being claimed exactly once; `None` = no attribution
    /// was made (residual unexplained, or nothing matched).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realign_claimed_at: Option<i64>,
    pub open_positions: usize,
    pub open_buys: usize,
    pub seen_trades: u64,
    pub note: String,
}

impl AuditReport {
    /// One-line summary for the log/panel.
    pub fn summary(&self) -> String {
        let failed: Vec<&str> = self
            .checks
            .iter()
            .filter(|c| !c.ok)
            .map(|c| c.name.as_str())
            .collect();
        if failed.is_empty() {
            format!(
                "ok balance={} reserved_open_buys={} open_positions={}",
                self.checks
                    .first()
                    .map(|c| c.actual.to_string())
                    .unwrap_or_default(),
                self.checks
                    .iter()
                    .find(|c| c.name == "reservations_match_open_buys")
                    .map(|c| c.expected.to_string())
                    .unwrap_or_default(),
                self.open_positions
            )
        } else {
            format!(
                "FAILED {} drift={} {}",
                failed.join(","),
                self.drift_usd,
                self.note
            )
        }
    }
}

/// Compare the core's accounting views. Pure: no I/O, no clocks, no mutation.
///
/// Returns, alongside the report, the attribution mark for the caller to carry
/// into the NEXT audit's input: the instant THIS audit attributed a trade-leg
/// residual to the venue's own correction (issue #921). Pure code cannot set
/// it back into `input` — the CALLER owns the bookkeeping (see
/// `AuditInput.realign_explained_at`).
pub fn audit_accounting(input: &AuditInput, anchor: Option<&CashIdentity>) -> AuditReport {
    let id = &input.identity;
    let mut checks: Vec<AuditCheck> = Vec::new();
    // The instant whose correction, if any, is claimed by this report — None
    // until trade_leg settles (filled below out of the report assembly).
    let mut realign_spent: Option<i64> = None;

    // 1) The ledger's own cash identity. Structural: both sides are the same
    //    struct, so a failure is a bug in the ledger, not a stale read.
    checks.push(AuditCheck {
        name: "cash_identity".into(),
        ok: input.ledger_balanced && input.unfunded_reserved <= AUDIT_TOL_USD,
        structural: true,
        expected: Decimal::ZERO,
        actual: input.unfunded_reserved,
        drift: input.unfunded_reserved,
        detail: format!(
            "available ({}) + reserved ({}) == balance ({}); unfunded {}",
            id.balance - input.reserved,
            input.reserved,
            id.balance,
            input.unfunded_reserved
        ),
        offenders: Vec::new(),
    });

    // 2) Every reservation is exactly the outstanding notional of a live BUY
    //    order — no residue from a settled fill, no phantom hold (issue #181).
    let mut reservation = AuditCheck::new(
        "reservations_match_open_buys",
        true,
        input.open_buy_notional,
        input.reserved,
        format!(
            "reserved must equal the unfilled notional of {} live BUY order(s)",
            input.open_buys
        ),
    );
    if !input.stray_reservations.is_empty() {
        reservation.ok = false;
        reservation.offenders = input
            .stray_reservations
            .iter()
            .map(|(id, v)| format!("{id}={v}"))
            .collect();
        reservation.detail = format!(
            "{} reservation(s) held for orders that are not live BUYs: {}",
            input.stray_reservations.len(),
            reservation.offenders.join(", ")
        );
    }
    checks.push(reservation);

    // 3) The trade ledger against the cash ledger, anchored. Structural: both
    //    records are local and read under the same lock, so there is no race to
    //    forgive — and a manual venue fill the reconcile sweep folded into cash
    //    without a trade record is a real (and permanent) inconsistency an
    //    operator must see. The remedy is explicit: fix the trade record, or
    //    restart (which re-anchors), or turn `audit_halt_entries` off.
    //
    //    One exception makes it cross-source: the plugin host SETS the ledger's
    //    cash from the venue's free cash when nothing rests (`set_balance`), so a
    //    residual that equals that correction is the VENUE's number, not ours.
    //    It stays a failure — the trade record genuinely disagrees with the
    //    money — but it halts on a repeat like the venue leg instead of on the
    //    first audit, because the correcting party is external.
    if !input.has_trade_log {
        checks.push(AuditCheck::pass(
            "trade_ledger_identity",
            true,
            "no trade log configured: not covered".into(),
            id.balance,
        ));
    } else {
        match anchor {
            None => checks.push(AuditCheck::pass(
                "trade_ledger_identity",
                true,
                "anchor pending (first clean audit establishes the baseline)".into(),
                id.balance,
            )),
            Some(a) => {
                let expected = id.expected_after(a);
                // Issue #921: the venue's own correction explains a residual
                // ONCE, within a freshness window. The slot is never cleared by
                // its writer, so without the two brackets below any later
                // residual that happens to equal the standing delta would keep
                // borrowing it as an excuse — forever, and once per audit —
                // and the cross-source "repeat to halt" would never strike.
                // Two brackets:
                //  - one-shot: the attribution mark (`realign_explained_at`,
                //    set by the caller after a previous audit that used this
                //    correction) declines a second use;
                //  - fresh: a correction older than REALIGN_EXPLAIN_TTL_MS is
                //    declined too (the venue reports every sweep, so the
                //    standing truth is always reapplied in practice).
                //
                // A DECLINED matcher is still a failure and now STRUCTURAL:
                // the residual is no longer believed to be the venue's number,
                // so there is "no fetch race to forgive" and entries halt at
                // once instead of on a repeat that explained-away would never
                // let arrive. Either way the detail states the offered
                // correction and the refusal reason — loud, not silent.
                let matcher = input
                    .ledger_realigned
                    .filter(|(_, d)| (id.total() - expected - d).abs() <= AUDIT_TOL_USD);
                let declined: Option<&str> = matcher.as_ref().and_then(|(at_ms, _)| {
                    if input
                        .realign_explained_at
                        .map(|t| t >= *at_ms)
                        .unwrap_or(false)
                    {
                        Some("already spent on an earlier residual (not overwritten since)")
                    } else if input.now_ms - *at_ms > REALIGN_EXPLAIN_TTL_MS {
                        Some("stale: older than the fresh window")
                    } else {
                        None
                    }
                });
                let explained = matcher.is_some() && declined.is_none();
                let mut c = AuditCheck::new(
                    "trade_ledger_identity",
                    // Structural = NO acceptable explanation: the books are
                    // local and same-source, halting at once is right. The
                    // ONLY thing that downgrades to cross-source is an
                    // accepted (one-shot, fresh) venue correction.
                    !explained,
                    expected,
                    id.total(),
                    format!(
                        "balance+receivable ({}) since anchor vs trade records: realized {} → {} ({:+}), \
                         open cash paid {} → {} ({:+}), returned {} → {} ({:+}), receivable {} → {} ({:+})",
                        id.total(),
                        a.realized,
                        id.realized,
                        id.realized - a.realized,
                        a.spent,
                        id.spent,
                        id.spent - a.spent,
                        a.received,
                        id.received,
                        id.received - a.received,
                        a.receivable,
                        id.receivable,
                        id.receivable - a.receivable,
                    ),
                );
                if let Some((at_ms, d)) = matcher {
                    if explained {
                        c.detail.push_str(&format!(
                            "; residual is the venue's own ledger correction ({d:+} at {at_ms}): \
                             cash was set from the venue, the trade record has not caught up — \
                             ONE-SHOT excuse: it is claimed for this audit (issue #921), \
                             the slot is marked spent and a lingering residual next audit \
                             comes back unexplained"
                        ));
                        realign_spent = Some(at_ms);
                    } else {
                        c.detail.push_str(&format!(
                            "; residual matches the venue's ledger correction (Δ{d:+} at {at_ms}) \
                             but the excuse is REFUSED: {} — downgraded back to STRUCTURAL (issue #921)",
                            declined.unwrap_or_default()
                        ));
                    }
                }
                if input.seen_trades == 0 {
                    // Nothing has been closed yet, so the trade record says nothing
                    // about the cash beyond "no realized PnL" — still a real check
                    // (an unexplained cash move shows up immediately), but say so.
                    c.detail.push_str("; no closed trades yet");
                }
                checks.push(c);
            }
        }
    }

    // 4) The venue's free cash against what the ledger believes is free
    //    (cross-source: a fetch race or a stale figure is possible, and the two
    //    reporting conventions differ while orders rest — see the plugin host).
    //    Skipped entirely for a SIMULATED venue: the paper site freezes its own
    //    reported free cash on a resting BUY and never reports the reservation
    //    back, so the gap is structural, fee-level and forever — skipping (not
    //    covering) keeps the other checks untouched while refusing to halt
    //    entries over arithmetic that cannot agree.
    if input.live && !input.venue_simulated {
        match input.venue_free {
            None => checks.push(AuditCheck::pass(
                "venue_free_cash",
                false,
                "no venue balance reported yet".into(),
                id.balance,
            )),
            Some(free) => {
                let expected = id.balance - input.reserved;
                let mut c = AuditCheck::with_tol(
                    "venue_free_cash",
                    false,
                    expected,
                    free,
                    format!(
                        "ledger free cash vs venue free cash (reported {}ms ago)",
                        input
                            .venue_free_at_ms
                            .map(|t| (input.now_ms - t).max(0))
                            .unwrap_or(0)
                    ),
                    AUDIT_VENUE_TOL_USD,
                );
                if input.open_buys > 0 {
                    // With orders resting, the venue may or may not net their
                    // commitments out of the figure it reports, so only a large
                    // gap is evidence — and it is still cross-source.
                    c.detail
                        .push_str("; resting BUY orders: the venue may not net them out");
                }
                checks.push(c);
            }
        }
    }

    let drift_usd = checks
        .iter()
        .filter(|c| !c.ok)
        .map(|c| c.drift.abs())
        .max()
        .unwrap_or(Decimal::ZERO);
    let structural_failure = checks.iter().any(|c| !c.ok && c.structural);
    let ok = checks.iter().all(|c| c.ok);
    let note = if ok {
        format!(
            "balance={} reserved={} open_buys={} open_positions={}",
            id.balance, input.reserved, input.open_buys, input.open_positions
        )
    } else {
        checks
            .iter()
            .filter(|c| !c.ok)
            .map(|c| format!("{}: {} (drift {})", c.name, c.detail, c.drift))
            .collect::<Vec<_>>()
            .join(" | ")
    };
    AuditReport {
        at_ms: input.now_ms,
        live: input.live,
        ok,
        structural_failure,
        drift_usd,
        // The caller may still downgrade this (a first cross-source failure is
        // not yet a halt) — `Core` owns the consecutive-failure policy. What the
        // audit states is whether this instant's evidence is bad enough to halt.
        halt_entries: structural_failure,
        checks,
        anchor: anchor.copied(),
        realign_claimed_at: realign_spent,
        open_positions: input.open_positions,
        open_buys: input.open_buys,
        seen_trades: input.seen_trades,
        note,
    }
}
