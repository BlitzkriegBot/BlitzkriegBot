//! Strategy enable/scope/compat handshake (E27): global toggle, per-account
//! scoped enablement, plugin-mode handshake. Moved verbatim from
//! `service/strategies.rs` (pure code motion).

use crate::model::{CoreError, CoreErrorCode, CoreResult};
use crate::risk::account_store::STRATEGIES_SETTING;
use blitzkrieg_market_api::net::now_ms;

use super::Core;

impl Core {
    pub fn strategy_names(&self) -> Vec<String> {
        let mut names = self
            .engine
            .as_ref()
            .map(|e| e.supported_strategies())
            .unwrap_or_default();
        if let Some(e) = self.engine.as_ref() {
            for (name, _) in e.scope_sources() {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }

    /// Enabled strategy names.
    pub fn enabled_strategy_names(&self) -> Vec<String> {
        self.engine
            .as_ref()
            .map(|e| e.enabled_strategies())
            .unwrap_or_default()
    }

    /// Toggle a strategy on the driving engine. Returns false when the engine
    /// is not attached or the name is unknown.
    ///
    /// A toggle changes which strategies Shadow Evolution watches (it registers
    /// around the enabled set), so the manager is rewired immediately: switching a
    /// strategy on brings its parameters and variants in, switching it off drops
    /// them. No restart, and nothing is traded by a strategy that is off.
    ///
    /// 账户↔venue 解耦语义: this is the GLOBAL strategy switch (the master
    /// power). The per-account 启用按钮 rides
    /// [`Self::set_scoped_strategy_enabled`] below.
    ///
    /// PERSISTENCE ASYMMETRY (issue 552 audit, honest wording): this toggle is
    /// a SESSION-level fact on the default scope's enabled flag — it does not
    /// persist. The durable per-account truth is only the store's
    /// enablement set (SQLite `strategy_assignments`): a kernel restart replays it, so a scope named in an
    /// account's set boots ENABLED even if the session toggled the global
    /// switch off. To keep a strategy off across restarts, remove it from
    /// every account's set (the panel's 启用按钮 / `account.strategies.set`).
    pub fn set_strategy_enabled(&mut self, name: &str, enabled: bool) -> bool {
        let ok = self
            .engine
            .as_mut()
            .map(|e| e.set_strategy_enabled(name, enabled))
            .unwrap_or(false);
        if ok {
            self.rewire_hot_params(now_ms());
            // Record the operator's intent: the next boot replays this set.
            // `None` (backtests, most tests) writes nothing.
            //
            // #188 (R7.1 L4): the mirror's failure is surfaced, never
            // swallowed. The toggle itself already took effect on the live
            // engine, so this is not an error the caller can undo — it is a
            // divergence between the running core and what the next boot
            // replays, reported at ERROR because silence is precisely how the
            // two carriers drifted apart before.
            if let Some(path) = &self.config.strategy_state_path
                && let Err(e) = crate::strategy_state::save(
                    std::path::Path::new(path),
                    &self.enabled_strategy_names(),
                )
            {
                tracing::error!(
                    path = %path,
                    error = %e,
                    "strategy state mirror NOT written after a global toggle — the \
                     running enabled-set and the next boot's replay have diverged; \
                     SQLite account_settings.strategies remains the authority"
                );
            }
        }
        ok
    }

    /// The per-account half of `strategy.enable {name, accountId?, reason?}`:
    /// set ONE (strategy, account) scope's enablement — persist to the
    /// account book's `strategies` set (memory first, disk after, one
    /// transaction) and drive the engine's scope state from it.
    ///
    /// Enable: the strategy joins the account's enablement set; a scope that
    /// does not exist yet is instantiated (Lua sources only — the same
    /// rebuild the boot uses) and arrives enabled; an existing
    /// exits-only-demoted scope returns to normal. Disable: the scope leaves
    /// the set; a flat scope is torn down, a scope holding positions is
    /// demoted EXITS-ONLY (no new entries, exits keep flowing to flat) —
    /// `kept_exits_only` names what was demoted, for the reply.
    ///
    /// `id` MUST be a configured account (the standard unknown-account
    /// refusal otherwise). `default` is a legal target: its scopes are the
    /// historical global-toggle shape, and its enablement set rides the same
    /// file. The GLOBAL toggle (`set_strategy_enabled`) is NOT touched —
    /// this is per-account state only.
    ///
    /// Returns `(kept_exits_only, set_now)` — the demoted names, and the
    /// account's enablement set as it now stands (the reply's echo).
    pub fn set_scoped_strategy_enabled(
        &mut self,
        id: &crate::account::AccountId,
        name: &str,
        enabled: bool,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> CoreResult<(Vec<String>, Vec<String>)> {
        use crate::schema::ACCOUNT_STRATEGIES_SET_NOTE;
        // Existence first, with the standard unknown-account refusal, before
        // anything touches disk.
        self.accounts.require(id)?;
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "strategy.enable: an empty strategy name cannot be addressed".to_string(),
            ));
        }
        // The assignable set is what `strategy.list` reports: every package
        // LOADED this session. Enabling an uninstalled strategy is refused —
        // a silent never-trading scope is the exact production accident the
        // rebuilt-scopes-start-ENABLED rule exists against.
        if self
            .engine
            .as_ref()
            .is_none_or(|e| !e.strategy_registered(trimmed))
        {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "strategy.enable: strategy `{trimmed}` is not registered — \
                     the assignable set is what `strategy.list` reports"
                ),
            ));
        }
        // The account's current enablement set (the book is the truth), then
        // the toggle.
        let mut set: Vec<String> = self
            .accounts
            .account(id)
            .map(|a| a.strategies.clone())
            .unwrap_or_default();
        let present = set.iter().any(|s| s == trimmed);
        if enabled && !present {
            set.push(trimmed.to_string());
        } else if !enabled {
            set.retain(|s| s != trimmed);
        }
        // 持久优先：先落 SQLite（`account_settings.strategies`），失败则两侧都不动。
        // 操作者裁决 2026-11-14：策略集合不再写 `.env`。
        let Some(db) = self.account_risk_db.clone() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "strategy.enable: no SQLite store on this core — persistence is \
                 disabled (backtests and in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        let rendered = serde_json::to_string(&set).map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("strategy.enable: the set does not render as JSON: {e}"),
            )
        })?;
        db.save_setting(id.as_str(), STRATEGIES_SETTING, Some(&rendered))
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("strategy.enable: SQLite 落盘失败，未做任何变更（{e}）"),
                )
            })?;
        self.accounts.set_strategies(id, set.clone(), now_ms)?;
        // #537: the default account's scoped write keeps the persisted
        // enabled-set in lockstep (the same mirror `set_account_strategies_
        // persisting` runs): the boot migrates the state file ONLY while the
        // default account has no `strategies` key, so a stale state file
        // would otherwise re-import a set the operator already emptied.
        //
        // #188 (R7.1 L4): the SQLite row above is the authority and it has
        // already committed — this mirror failing does NOT roll it back, so
        // the honest report is an ERROR naming the divergence, not a warn
        // that scrolls away. The scoped write itself still succeeded.
        if *id == crate::account::default_account_id()
            && let Some(state_path) = self.config.strategy_state_path.as_deref()
            && let Err(e) = crate::strategy_state::save(std::path::Path::new(state_path), &set)
        {
            tracing::error!(
                path = %state_path,
                account = %id,
                error = %e,
                "strategy state mirror NOT written after a scoped toggle — the SQLite \
                 account_settings.strategies row IS committed and authoritative; the \
                 legacy mirror is now stale and a boot with no strategies row would \
                 replay the old set"
            );
        }
        // Engine rebuild over the new set: removed-and-flat scopes drop,
        // removed-and-held scopes demote exits-only, added scopes
        // instantiate enabled.
        let held: Vec<String> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id == *id)
            .map(|p| p.strategy.clone())
            .collect();
        let kept_exits_only = self.rebuild_account_scopes(id, &set, &held);
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            strategy = %trimmed,
            enabled,
            set = ?set,
            exits_only = ?kept_exits_only,
            reason = %reason,
            "per-account strategy enablement written — scope rebuilt from the \
             account's enablement set, re-read at next boot ({ACCOUNT_STRATEGIES_SET_NOTE})",
        );
        Ok((kept_exits_only, set))
    }

    /// The per-account enablement readout: the accounts whose enablement set
    /// names this strategy, sorted, plus the default scope when the global
    /// toggle is ON and default carries no key of its own (the zero-config
    /// shape has no enablement-set row to point at). This is the WRITE-path
    /// truth ("which accounts does this strategy trade on"), kept for the
    /// boot-replay teeth and any caller that wants enablement, not hosting.
    ///
    /// NOTE the deliberate difference from `Engine::strategy_scopes` — what
    /// `strategy.list`'s per-row `accounts` field serves (the coordination
    /// contract, wired in server.rs): the HOSTED scope list, exits-only
    /// demotions included. A scope pulled out of the enablement set while
    /// holding positions must keep showing on the matrix — the panel needs
    /// to see "this account still manages its exits here" — while this
    /// function answers the narrower "whose enablement button names it".
    pub fn strategy_enabled_accounts(&self, name: &str) -> Vec<String> {
        let mut accounts: Vec<String> = self
            .accounts
            .account_list()
            .into_iter()
            .filter(|a| a.strategies.iter().any(|s| s == name))
            .map(|a| a.id.as_str().to_string())
            .collect();
        // The zero-config shape: a `default` with no enablement key whose
        // global toggle is ON still runs the strategy — its row says so.
        let default_id = crate::account::default_account_id();
        let default_has_key = self
            .accounts
            .account(&default_id)
            .is_some_and(|a| !a.strategies.is_empty());
        if !default_has_key
            && self
                .engine
                .as_ref()
                .is_some_and(|e| e.enabled_strategies().iter().any(|n| n == name))
        {
            accounts.push(default_id.as_str().to_string());
        }
        accounts.sort();
        accounts.dedup();
        accounts
    }

    /// The handshake verdict for one declaration, as an associated function so
    /// the load path can run it while the engine borrow is live (only
    /// field-disjoint access is legal there). `None` = compatible: an
    /// undeclared strategy participates not (§8.1 — every 0.2 library's
    /// state), and no active plugin (backtester, tests) leaves nothing to
    /// refuse against.
    ///
    /// Multi-plugin: the declaration is judged against EVERY snapshot and the
    /// strategy loads when ANY ONE plugin can serve it; the refusal names each
    /// plugin's own mismatch so the diagnostic stays complete. Single-plugin
    /// books render exactly the historical message (no per-plugin prefix).
    pub(super) fn modes_refusal(
        plugins: &[crate::compat::PluginModes],
        name: &str,
        modes: &[blitzkrieg_strategy_api::StrategyMode],
    ) -> Option<String> {
        if modes.is_empty() {
            return None;
        }
        if plugins.is_empty() {
            return None;
        }
        let refusals: Vec<String> = plugins
            .iter()
            .filter_map(|p| crate::compat::check_compatibility(name, modes, p).err())
            .collect();
        if refusals.len() < plugins.len() {
            // At least one plugin passed (its refusal is absent): ANY one
            // satisfiable venue loads the strategy — a venue the strategy
            // cannot serve must not block the venues it can.
            return None;
        }
        // Every plugin refused: the diagnostic names each mismatch. ONE plugin
        // → the bare historical message, byte-identical; several → prefixed
        // and joined so the full mismatch surface is visible.
        if plugins.len() == 1 {
            return refusals.into_iter().next();
        }
        Some(format!(
            "incompatible modes: strategy '{name}' is not served by any active plugin — {}",
            refusals.join(" | ")
        ))
    }

    /// The handshake verdict for a strategy already in the dispatch, by name.
    pub(crate) fn compatibility_refusal(&self, name: &str) -> Option<String> {
        match self.strategy_declarations.get(name) {
            Some(modes) => Self::modes_refusal(&self.plugin_modes, name, modes),
            None => None,
        }
    }

    /// Inject every active plugin's declaration snapshot (server assembly,
    /// BEFORE any strategy is loaded or enabled, so all three §8.2 handshake
    /// sites judge against the same view). Empty leaves the handshake inert
    /// (backtester, tests).
    pub fn set_plugin_modes(&mut self, modes: Vec<crate::compat::PluginModes>) {
        self.plugin_modes = modes;
    }

    /// Test-only: record a declaration for a strategy the loader would have
    /// registered (the test adapters carry no C ABI declaration of their
    /// own). Same map the real load path fills.
    #[cfg(test)]
    pub(crate) fn set_strategy_declarations_for_test(
        &mut self,
        name: &str,
        modes: Vec<blitzkrieg_strategy_api::StrategyMode>,
    ) {
        self.strategy_declarations.insert(name.to_string(), modes);
    }

    /// The enable path with the handshake. Returns `(found, applied, reason)`:
    /// `found` says the name exists in the dispatch, `applied` says the toggle
    /// took effect, and `reason` carries the §8.2 diagnostic when the
    /// handshake refused an enable. Disabling is never gated — standing down
    /// is always allowed.
    pub fn set_strategy_enabled_checked(
        &mut self,
        name: &str,
        enabled: bool,
    ) -> (bool, bool, Option<String>) {
        let found = self.strategy_names().iter().any(|n| n == name);
        if enabled
            && found
            && let Some(reason) = self.compatibility_refusal(name)
        {
            return (true, false, Some(reason));
        }
        let applied = self.set_strategy_enabled(name, enabled);
        (found, applied, None)
    }

    /// E27 (§8.3): the `strategy.list` view for one strategy — the declared
    /// modes as the §2.3 wire objects (`null` = undeclared), the compatible
    /// flag and the refusal reason when incompatible. Declarations describe
    /// live dispatch members only (recorded at load, cleared at unload).
    pub fn strategy_compat_view(&self, name: &str) -> (serde_json::Value, bool, Option<String>) {
        match self.strategy_declarations.get(name) {
            None => (serde_json::Value::Null, true, None),
            Some(modes) => {
                let wire: Vec<serde_json::Value> = modes
                    .iter()
                    .map(blitzkrieg_strategy_api::modes::mode_wire_json)
                    .collect();
                let refusal = Self::modes_refusal(&self.plugin_modes, name, modes);
                (serde_json::Value::Array(wire), refusal.is_none(), refusal)
            }
        }
    }
}
