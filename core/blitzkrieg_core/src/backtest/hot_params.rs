//! #916: `--backtest-knob` counterfactual dispatch on the strategy's OWN
//! declaration (string tunable vs Decimal tunable).
//! Pure code motion from `backtest.rs` (no logic edits).

use crate::registry::ParamRegistry;
use rust_decimal::Decimal;
use strategy_logic::params::StrategyParams;

use super::driver::EventBacktester;

impl EventBacktester {
    /// #916: apply the `--backtest-knob` counterfactuals held back from the
    /// constructor. Dismissal is a returned `Err` with every offending knob
    /// named — the replay refuses rather than runs a comparison one arm of
    /// which silently used the shipped value.
    pub(crate) fn apply_hot_params(
        &mut self,
        pending: Vec<(String, String, String)>,
    ) -> Result<(), String> {
        let mut string_bags: std::collections::BTreeMap<String, Vec<(String, String)>> =
            std::collections::BTreeMap::new();
        let mut decimals: std::collections::BTreeMap<String, StrategyParams> =
            std::collections::BTreeMap::new();
        let mut failures: Vec<String> = Vec::new();
        for (strategy, knob, value) in &pending {
            let (string_names, declared) = match self.core.engine_knob_declarations(strategy) {
                Some(d) => d,
                None => {
                    failures.push(format!(
                        "--backtest-knob {strategy}:{knob}={value:?}: no loaded strategy \
                             carries this name (the kernel does not invent knob types; a \
                             counterfactual that would be silently ignored must refuse)"
                    ));
                    continue;
                }
            };
            if string_names.iter().any(|n| n == knob) {
                string_bags
                    .entry(strategy.clone())
                    .or_default()
                    .push((knob.clone(), value.clone()));
            } else if declared.iter().any(|n| n == knob) {
                match Decimal::from_str_exact(value.trim()) {
                    Ok(parsed) => {
                        decimals
                            .entry(strategy.clone())
                            .or_default()
                            .set(knob, parsed);
                    }
                    Err(_) => failures.push(format!(
                        "--backtest-knob {strategy}:{knob}={value:?} is not a decimal \
                         and the strategy does not declare it a string tunable \
                         (refused, #228/#441)"
                    )),
                }
            } else {
                failures.push(format!(
                    "--backtest-knob {strategy}:{knob}: the strategy declares no such \
                     tunable (refused — an undeclared override would ride the shipped \
                     config, #228/#441)"
                ));
            }
        }
        if !failures.is_empty() {
            let list = failures
                .iter()
                .map(|f| format!("  {f}"))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(format!("counterfactual knobs refused:\n{list}"));
        }
        for (strategy, overrides) in &string_bags {
            if !self.core.set_engine_string_params(strategy, overrides) {
                return Err(format!(
                    "counterfactual knobs refused: {strategy}: the strategy went \
                     missing between declaration and application"
                ));
            }
        }
        if !decimals.is_empty() {
            let registry = std::sync::Arc::new(ParamRegistry::new());
            for (strategy, params) in decimals {
                registry.publish(&strategy, params);
            }
            self.core.set_engine_hot_params(Some(registry));
        }
        Ok(())
    }
}
