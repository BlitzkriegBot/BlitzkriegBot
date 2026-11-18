//! Round-duration + replay-wake methods on `Core`. Pure code motion from
//! `service.rs` (no logic edits).

use crate::position::{DAY_MS, utc_day_index};
use crate::service::Core;
use crate::strategies::HeldPosition;

impl Core {
    /// B1b: the round-length family a token belongs to — the market's declared
    /// duration when it carries one (#354), else the account's own grid
    /// (`ACCOUNT_<ID>_ROUND_DURATION_SEC`), else the default grid.
    /// `0` = the token belongs to no live market AND no grid is set anywhere;
    /// B1b readers (`effective_round_duration_sec`) treat a 0 as "no clock fact".
    pub(super) fn round_family_of_token(&self, account_id: &str, token_id: &str) -> i64 {
        let account_dur = self.account_round_duration(account_id);
        match self
            .engine
            .as_ref()
            .and_then(|e| e.market_for_token(token_id))
            .map(|m| m.round_duration_sec)
        {
            Some(d) if d > 0 => d,
            _ => account_dur,
        }
    }

    /// Resolve the round duration for a specific account: the account's own
    /// `ACCOUNT_<ID>_ROUND_DURATION_SEC` if set, else `default_round_duration_sec`.
    pub(super) fn account_round_duration(&self, account_id: &str) -> i64 {
        self.config
            .account_round_durations
            .get(account_id)
            .copied()
            .unwrap_or(self.config.default_round_duration_sec)
    }

    /// B1b: the round-length family ONE order was admitted under, resolved
    /// three-state through B1a's `effective_round_duration_sec` (read only):
    /// the order's OWN stamp when it carries one, else the market's declared
    /// duration while the engine still names the token's market, else the
    /// account's grid. `0` = no clock fact at all — the caller's gate
    /// decides what that means.
    ///
    /// Single-family discipline: with every stamp 0 (pre-B1b archives) and
    /// every market declaring the configured grid (or 0, the legacy corpora),
    /// this folds to the account's grid — the value every consumer read
    /// before B1b, byte-for-byte.
    pub(super) fn order_eff_round_sec(
        &self,
        account_id: &str,
        o: &crate::model::TrackedOrder,
    ) -> i64 {
        let market_decl = self
            .engine
            .as_ref()
            .and_then(|e| e.market_for_token(&o.token_id))
            .map(|m| m.round_duration_sec);
        o.effective_round_duration_sec(market_decl, self.account_round_duration(account_id))
    }

    /// B1b: the round-length family ONE position was opened under — the
    /// position-shaped twin of [`Core::order_eff_round_sec`]: same tri-state
    /// fold through B1a's `effective_round_duration_sec`, same single-family
    /// equivalence (eff == the account's grid when every stamp is 0 and no
    /// market contradicts the grid).
    pub(super) fn position_eff_round_sec(
        &self,
        account_id: &str,
        p: &crate::position::OpenPosition,
    ) -> i64 {
        let market_decl = self
            .engine
            .as_ref()
            .and_then(|e| e.market_for_token(&p.token_id))
            .map(|m| m.round_duration_sec);
        p.effective_round_duration_sec(market_decl, self.account_round_duration(account_id))
    }

    /// The open-position snapshot strategies see through `bk.holdings()`,
    /// rebuilt once per evaluation cycle (the #202 equity push's position
    /// sibling). Flattened to wire strings here — the engine slices it per
    /// strategy, so a package never reads another package's book. The round
    /// slot is re-derived from the entry time on the position's OWN family
    /// grid (B1b: the tri-state eff — own stamp > market declaration >
    /// global grid, floored at 1 — so a legacy unstamped row whose market
    /// declares a different family still projects onto the family the
    /// market actually trades; a single-family deployment folds eff == cfg,
    /// the historic formula byte-for-byte); a position cannot outlive its
    /// round, so the exact boundary the entry sat on is all the gap-fill
    /// loop needs.
    pub(super) fn held_position_view(&self) -> Vec<crate::strategies::HeldPosition> {
        self.positions
            .open_positions()
            .iter()
            .map(|p| {
                let duration = self
                    .position_eff_round_sec(&p.account_id.to_string(), p)
                    .max(1);
                HeldPosition {
                    strategy: p.strategy.clone(),
                    account_id: p.account_id.to_string(),
                    condition_id: p.condition_id.clone(),
                    token_id: p.token_id.clone(),
                    direction: p.direction.as_str().to_string(),
                    shares: p.shares.to_string(),
                    entry_price: p.entry_price.to_string(),
                    round_slot: p.entered_at_ms / 1000 / duration,
                }
            })
            .collect()
    }

    /// Issue #390: whether THIS core runs the replay maintenance scheduling —
    /// the deadline-driven gates the fast path pioneered, applied to the
    /// nofast replay. `backtest_fast` implies it (the fast path is the same
    /// scheduling plus the all-holder skip); the nofast replay opts in
    /// through `replay_scheduling`; a live kernel never does.
    pub(super) fn replay_sched(&self) -> bool {
        self.config.backtest_fast || self.config.replay_scheduling
    }

    /// Issue #390 (replay deadline jump): the earliest virtual instant any
    /// time-driven kernel gate can next produce output, when the replay
    /// scheduler is in force. `None` on a live kernel or an unconfigured
    /// clock — the caller must then keep stepping every tick. The driver
    /// lands its grid on this instant (clamped forward to the grid), runs
    /// the tick, and RE-READS the wake: every step re-arms at least one
    /// deadline here, so the loop cannot stall and never needs to guess
    /// when a gate's deadline moves.
    ///
    /// What must NOT appear here, by design:
    ///   * engine inputs (books/tops/rounds/verdicts) — they are events, and
    ///     the driver stops on the first grid tick after every event batch
    ///     regardless of this clock;
    ///   * strategy-signal exit consumption — intents enqueued by an
    ///     evaluation are consumed by the NEXT tick's exit pass by
    ///     construction (the driver forces the next grid tick after any
    ///     evaluation that produced one), never a scheduled wake;
    ///   * place/policy cooldowns and backoffs — submission-path checks on
    ///     the place call itself, never a timer sweep.
    pub fn next_replay_wake_ms(&self, now_ms: i64) -> Option<i64> {
        if !self.replay_sched() {
            return None;
        }
        // `now_ms` arrives as (next grid point - 1): a ">=" gate due exactly
        // at the next grid point must land THERE, so the probe filters on
        // t > now_ms (t >= next grid point), not t > next.
        let mut best = i64::MAX;
        let consider = |t: i64, best: &mut i64| {
            // `i64::MAX` (and the "never armed" 0, superseded by the first
            // tick's own re-arm) is no wake — the gate's owner armed nothing.
            if t > now_ms && t < *best && t != i64::MAX {
                *best = t;
            }
        };
        consider(self.fast_path.fast_next_timing_ms, &mut best);
        // The eval fallback fires at the first grid tick at or after
        // last_eval + window — the exact tick the every-tick walk used.
        consider(
            self.fast_path
                .fast_last_eval_ms
                .saturating_add(super::FAST_EVAL_FALLBACK_MS),
            &mut best,
        );
        consider(self.fast_path.fast_next_exit_deadline_ms, &mut best);
        consider(self.fast_path.fast_next_escalation_ms, &mut best);
        consider(self.fast_path.fast_next_expiry_ms, &mut best);
        consider(self.settlement.next_query_deadline_ms(), &mut best);
        consider(self.settlement.next_redemption_ms(), &mut best);
        for cd in self.place_cooldowns.values() {
            consider(cd.next_ok_ms, &mut best);
        }
        for until in self.policy_cooldowns.values() {
            consider(*until, &mut best);
        }
        for leg in &self.inflight_takers {
            consider(leg.execute_at_ms, &mut best);
            consider(leg.report_at_ms, &mut best);
        }
        consider(self.breaker.next_resume_ms(), &mut best);
        consider(
            self.audit
                .last_at_ms
                .checked_add(self.config.audit_interval_sec.max(0) * 1000)
                .unwrap_or(i64::MAX),
            &mut best,
        );
        consider((utc_day_index(now_ms) + 1) * DAY_MS, &mut best);
        if best == i64::MAX { None } else { Some(best) }
    }

    /// Issue #390 (replay deadline jump): the `evaluations` feed counter
    /// advances once per scheduled cycle (tick) even when the cycle's Lua
    /// evaluation is skipped — the sparse-cadence contract pins the SCHEDULED
    /// count, not the wall cost. A driver that lands on a subset of the grid
    /// must credit the cycles it did not run, or the report's feed block
    /// (and the tests that pin it) see a count that disagrees with the
    /// cycle schedule the replay simulated.
    pub fn replay_credit_cycles(&mut self, cycles: u64) {
        // `engine_evaluate` counts only when an engine is installed; the
        // credit must mirror that exactly or engine-less sweeps would see a
        // counter the base never produced.
        if self.engine.is_some() {
            self.stats.evaluations = self.stats.evaluations.saturating_add(cycles);
        }
    }

    /// Issue #390 (replay deadline jump): true when the driver may land on a
    /// subset of the maintenance grid. Two disqualifiers: replay scheduling
    /// itself is off (the every-tick walk IS the contract), or Shadow
    /// Evolution is enabled — its per-tick evaluation cadence is part of its
    /// observed behaviour, so a jumping driver must not skip cycles under it.
    pub fn replay_jump_allowed(&self) -> bool {
        self.replay_sched() && !self.shadow_evolution.is_enabled()
    }

    /// Issue #390 (replay deadline jump): true when THIS tick left state
    /// that the immediately following grid tick consumes — strategy exit
    /// intents awaiting their consuming exit pass, or a dirty flag
    /// (orders/positions) a scheduled pass would have observed there. The
    /// jumping driver forces that landing; skipping it could move a stamp
    /// or a verdict the every-tick walk produced.
    pub fn has_forced_next_tick(&self) -> bool {
        !self.strategy_exits.is_empty()
            || self.fast_path.fast_orders_dirty
            || self.fast_path.fast_positions_dirty
    }

    /// Issue #390 (replay nofast): true when the standing-candidate cache
    /// holds a suggestion the driver must not skip over — a skipped grid tick
    /// would not re-run it into the kernel-side gates the way the base walk
    /// did every tick (see `engine_replay_cycle`).
    pub fn has_standing_candidates(&self) -> bool {
        self.engine
            .as_ref()
            .is_some_and(|e| e.has_standing_candidates())
    }
}
