//! Strategy tunable edit types: `strategy.params.get` / `strategy.params.set`.
//! Moved verbatim from `schema/mod.rs` (pure code motion).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// `strategy.params.get` params: `{ "name": "<strategy>" }`.
#[derive(Debug, Clone, Deserialize)]
pub struct StrategyParamsGetParams {
    pub name: String,
}

/// One declared tunable as the wire sees it: the strategy's OWN declaration
/// (name + the admissible `[min, max]` domain), the value in force, and the
/// type tag the manifest carries (`decimal` / `int` — an `int` knob's set
/// value must be a whole number). All numbers cross as decimal STRINGS.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TunableView {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<&'static str>,
    /// Current value in force (registry cell or manifest default), decimal string.
    pub value: String,
    pub min: String,
    pub max: String,
}

/// `strategy.params.get` result: the strategy's declared tunables plus the
/// effective mode. `paramsAttached` is true when the registry cell the write
/// path publishes to is also what the strategy READS this session (evolution
/// on, or an operator write has pinned the overlay); `false` means the write
/// path still works but the value takes the stage only via the attach below
/// the kernel performs on the next set — never a silent divergence.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StrategyParamsReadout {
    pub name: String,
    pub found: bool,
    /// Empty for a strategy that declared no tunables (not evolvable) — the
    /// editor renders "该策略没有可调参数" rather than an empty table.
    pub tunables: Vec<TunableView>,
    pub evolvable: bool,
    pub params_attached: bool,
    /// Session-level note for the operator (why a set would/would not stick).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `strategy.params.set` params: one strategy, the knobs to move (decimal
/// strings — the shared wire rule), and the audit note. Multi-knob patches
/// are atomic: one bad value refuses the WHOLE patch.
#[derive(Debug, Clone, Deserialize)]
pub struct StrategyParamsSetParams {
    pub name: String,
    /// knob name → new value (decimal string or JSON number; both parse
    /// exactly, no float path).
    pub params: BTreeMap<String, Value>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// One knob's audited change inside a `strategy.params.set` reply. All
/// tunables are `live`: the strategy merges its registry cell over the
/// manifest defaults at every evaluate, so the next cycle reads the new
/// value — no restart, no per-position binding to name.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StrategyParamChange {
    pub knob: String,
    pub from: String,
    pub to: String,
    pub effect: &'static str,
}

/// `strategy.params.set` result: the audited change list (the
/// `risk.setExit` receipt shape, so the panel's edit cards read alike).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StrategyParamsSetUpdate {
    pub name: String,
    pub applied: Vec<StrategyParamChange>,
    pub at_ms: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Always false today: the write lands in the in-memory registry cell.
    /// Restarts re-resolve from the manifest defaults (+ any adopted
    /// evolution promotions), which the note says out loud.
    pub persisted: bool,
    pub note: &'static str,
}

/// [`StrategyParamsSetUpdate::note`].
pub const STRATEGY_PARAMS_SET_NOTE: &str = "live from the strategy's next evaluation cycle (the registry cell merges over \
     the manifest defaults); a restart re-resolves from the manifest defaults and any adopted \
     evolution promotions (in-memory, not persisted)";
