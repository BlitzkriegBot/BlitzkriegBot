//! Market-plugin read/write types: `market.plugin.get` / `market.plugin.set`.
//! Moved verbatim from `schema/mod.rs` (pure code motion).

use serde::{Deserialize, Serialize};

/// `market.plugin.get` result: the running session's active plugin, the
/// compiled-in registry, and what a restart would select (the persisted
/// `marketPlugin` cell outranks first-registered but not an explicit
/// `--market-plugin` flag).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketPluginReadout {
    /// The plugin driving THIS session (the registry's active name).
    pub active_plugin: String,
    /// Every compiled-in plugin in registration order — the switchable set.
    pub registered: Vec<MarketPluginOption>,
    /// What the NEXT session will select: the persisted cell if one holds a
    /// REGISTERED name, else this session's active plugin. A cell naming an
    /// unregistered plugin is reported but NOT armed (the boot refuses it the
    /// same way), so the next session never silently differs.
    pub next_session_plugin: String,
    /// Whether the persisted cell holds a registered name (a write happened
    /// at some point; `market.plugin.set` with `reset` clears it).
    pub persisted: bool,
    /// True when an explicit `--market-plugin` flag resolved this session —
    /// the UI must say a write cannot move a flag-pinned session.
    pub flag_pinned: bool,
    /// Where THIS session's enablement came from — the boot's four-layer
    /// resolution, disclosed: `flag`（显式 --market-plugin）、`accounts`
    /// （绑定即启用：从账户 venue 绑定派生 —— 本层生效时 market.plugin.set
    /// 被拒）、`persisted`（旧 market.plugin.set 持久格，仅在无绑定时生效）、
    /// `fallback`（全空 —— 回落首个注册者）。
    pub source: &'static str,
    /// The derived venue set (deduped non-empty `Account.venue` bindings) the
    /// accounts layer produced — empty whenever `source` is not `accounts`.
    /// Provenance only, never a decision input.
    pub derived_venues: Vec<String>,
}

/// One row of the registered-plugin list.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketPluginOption {
    pub name: String,
    /// The plugin's market class (`prediction` / `spot` / …).
    #[serde(rename = "type")]
    pub kind: String,
    /// True when this plugin is driving the current session.
    pub active: bool,
}

/// `market.plugin.set` params: `name` must name a REGISTERED plugin (the
/// registry is compiled in — an unregistered name is refused here, plan-first,
/// not discovered as a broken boot later); `reset` instead clears the
/// persisted cell so the next session falls back to `--market-plugin`/
/// first-registered. Exactly one of the two must be given.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetMarketPluginParams {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub reset: bool,
    /// Audit trail only — echoed in the reply and the log line.
    pub reason: Option<String>,
}

/// `market.plugin.set` result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketPluginUpdate {
    /// What the next session will select after this write.
    pub next_session_plugin: String,
    /// `reset` clears the cell (falls back to boot resolution) instead of
    /// writing a value.
    pub reset: bool,
    pub at_ms: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub note: &'static str,
}

/// [`MarketPluginUpdate::note`] — one spelling, used by the reply and the log.
pub const MARKET_PLUGIN_SET_NOTE: &str = "next session: the data feed, discovery and executor are all built around the plugin chosen at boot, so a switch is a restart; an explicit --market-plugin flag still outranks this setting";
