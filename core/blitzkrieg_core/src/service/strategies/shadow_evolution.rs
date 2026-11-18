//! The shadow-evolution lifecycle (`shadow_evolution_apply`, enable/disable,
//! status, rollback, variants, history, proposals, decide, on_round/on_tick,
//! evaluate). Moved verbatim from `service/strategies.rs` (pure code motion).

use crate::model::CryptoMarket;
use crate::schema::Event;
use crate::shadow_evolution::{EvolutionOutcome, EvolutionStatus, MutableParams, ShadowEvolution};
use blitzkrieg_market_api::net::now_ms;

use super::Core;

impl Core {
    // ── Shadow Evolution (opt-in) ───────────────────────────────────────────
    pub fn shadow_evolution(&self) -> &ShadowEvolution {
        &self.shadow_evolution
    }

    /// Operator override for ONE strategy: validate + hot-swap + audit. `params`
    /// must name exactly one strategy; a multi-strategy bag is refused rather than
    /// partially applied.
    pub fn shadow_evolution_apply(
        &mut self,
        params: MutableParams,
        now_ms: i64,
    ) -> Result<(), String> {
        self.shadow_evolution.apply_params(params, now_ms)
    }

    pub fn shadow_evolution_enable(&mut self, now_ms: i64) -> bool {
        self.shadow_evolution.enable(now_ms);
        self.rewire_hot_params(now_ms);
        true
    }

    pub fn shadow_evolution_disable(&mut self) -> bool {
        self.shadow_evolution.disable();
        self.rewire_hot_params(now_ms());
        true
    }

    /// Aggregate status across every evolvable strategy (kept for callers that
    /// predate per-strategy status). `shadow_evolution_status_for` is the precise
    /// per-strategy query.
    pub fn shadow_evolution_status(&self, now_ms: i64) -> EvolutionStatus {
        self.shadow_evolution.aggregate_status(now_ms)
    }

    /// One strategy's status (`None` = not evolvable).
    pub fn shadow_evolution_status_for(
        &self,
        strategy: &str,
        now_ms: i64,
    ) -> Option<EvolutionStatus> {
        self.shadow_evolution.status(strategy, now_ms)
    }

    /// Roll back ONE strategy to the parameters in force before its last change.
    pub fn shadow_evolution_rollback(
        &mut self,
        strategy: &str,
        now_ms: i64,
    ) -> Result<EvolutionOutcome, String> {
        self.shadow_evolution.rollback(strategy, now_ms)
    }

    pub fn shadow_evolution_variants(
        &mut self,
        now_ms: i64,
    ) -> Vec<crate::shadow_evolution::VariantView> {
        self.shadow_evolution.variant_views(now_ms)
    }

    /// Audit history. `None` = every strategy; `Some(name)` = that strategy's file.
    pub fn shadow_evolution_history(
        &self,
        strategy: Option<&str>,
        limit: usize,
    ) -> Vec<crate::shadow_evolution::audit::AuditRecord> {
        self.shadow_evolution.history(strategy, limit)
    }

    /// E13: every known proposal's latest state, newest first (IPC surface).
    pub fn shadow_evolution_proposals(
        &self,
        limit: usize,
    ) -> Vec<crate::shadow_evolution::EvolutionProposal> {
        self.shadow_evolution.all_proposals(limit)
    }

    /// E13: the operator's verdict on one held proposal. String is the IPC-side
    /// verb ("accept"/"reject"/"defer"); anything else is refused loudly.
    pub fn shadow_evolution_decide(
        &mut self,
        id: &str,
        decision: &str,
        now_ms: i64,
    ) -> Result<crate::shadow_evolution::DecisionResult, String> {
        let decision = match decision {
            "accept" | "accepted" => crate::shadow_evolution::Decision::Accept,
            "reject" | "rejected" => crate::shadow_evolution::Decision::Reject,
            "defer" | "deferred" => crate::shadow_evolution::Decision::Defer,
            other => return Err(format!("unknown decision '{other}' (accept|reject|defer)")),
        };
        self.shadow_evolution.decide(id, decision, now_ms)
    }

    /// E13: the auto-evolve switch (the UIs' checkbox). Persisted across restarts.
    pub fn shadow_evolution_set_auto(&mut self, on: bool) -> bool {
        self.shadow_evolution.set_auto_evolve(on);
        self.shadow_evolution.auto_evolve()
    }

    /// E13: the 72h deep-evolution clock. Fires a compound-mutation round when
    /// the interval has elapsed; the returned event, if any, is emitted.
    pub fn shadow_evolution_maybe_cycle(&mut self, now_ms: i64) {
        if let Some(ev) = self.shadow_evolution.maybe_evolution_cycle(now_ms) {
            self.emit(Event::EvolutionCycle {
                cycle_seq: ev.cycle_seq,
                dims: ev.dims,
                strategies: ev.strategies,
                at_ms: ev.at_ms,
            });
        }
    }

    /// Feed the evolution engine a round's markets plus the opening book seeds the
    /// live engine is about to replay, so every twin's confirmation clock starts on
    /// the same tick as the live strategy's.
    pub fn shadow_evolution_on_round(
        &mut self,
        markets: &[CryptoMarket],
        seeds: &[(String, crate::model::OrderbookSnapshot)],
        now_ms: i64,
    ) {
        self.shadow_evolution.on_round(markets, seeds, now_ms);
    }

    /// Feed the evolution engine a book tick (observation only). Twins derive
    /// their own trend state from the books they see, exactly as an external
    /// strategy does, so no externally-computed confirmation flag is passed in.
    pub fn shadow_evolution_on_tick(
        &mut self,
        token_id: &str,
        book: &crate::model::OrderbookSnapshot,
        now_ms: i64,
    ) {
        self.shadow_evolution.on_tick(token_id, book, now_ms);
    }

    /// Run one evolution evaluation and map outcomes to kernel events.
    ///
    /// Called every tick regardless of the engine switch, because the pass is not
    /// only "evaluate": it also expires undecided proposals past their TTL, which
    /// is bookkeeping the operator's "7 天未决自动过期" is a promise about. The
    /// switch itself is the manager's own gate (#249) — one place, so a disabled
    /// engine can neither evaluate nor adopt, and cannot silently stop ageing the
    /// queue it left behind either.
    pub fn shadow_evolution_evaluate(&mut self, now_ms: i64) {
        for outcome in self.shadow_evolution.evaluate(now_ms) {
            self.emit_shadow_outcome(outcome);
        }
    }
}
