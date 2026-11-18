//! Blueprint editor surface: `blueprint.compile` / `blueprint.save` /
//! `blueprint.load` / `blueprint.loadSource` / `blueprint.saveSource` params
//! and results. Moved verbatim from `schema/mod.rs` (pure code motion).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `blueprint.compile` params: `{ "json": "<blueprint document>" }`.
///
/// The document travels as a STRING, not a nested JSON object, so the compile
/// errors keep quoting the exact bytes the editor sent — the same property
/// `--blueprint-compile <file>` has on the CLI side.
#[derive(Debug, Clone, Deserialize)]
pub struct BlueprintCompileParams {
    pub json: String,
}

/// `blueprint.compile` result: the generated Lua source, verbatim.
#[derive(Debug, Clone, Serialize)]
pub struct BlueprintCompileResult {
    pub lua: String,
}

/// `blueprint.save` params. `name` is the package identity (directory name =
/// manifest name, §6.4); `json` is the blueprint document, compiled fresh by
/// the save arm — the caller sending a stale `lua` is not a thing: the kernel
/// compiles, and what it compiled is what lands on disk. `overwrite` must be
/// explicit to replace a package the loader already scans (a silent overwrite
/// could swap a strategy an operator believes they know).
#[derive(Debug, Clone, Deserialize)]
pub struct BlueprintSaveParams {
    pub name: String,
    pub json: String,
    #[serde(default)]
    pub overwrite: bool,
}

/// `blueprint.save` result: the package receipt. Every path is RELATIVE (the
/// kernel's own strategy root), so a UI can show it without learning the host
/// filesystem.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlueprintSaveResult {
    pub name: String,
    pub package_dir: String,
    pub blueprint_path: String,
    pub lua_path: String,
    pub manifest_path: String,
    /// The sha256 of the written `strategy.lua`, hex — the same digest the
    /// manifest's `sha256` field carries, so the receipt is checkable against
    /// the file without rehashing.
    pub lua_sha256: String,
    /// Bytes written per file, in blueprint/lua/manifest order — the receipt
    /// the IPC contract promises (a save that "succeeded" without saying WHAT
    /// it wrote is not auditable).
    pub bytes: [u64; 3],
    /// Names of the evolvable knobs the manifest exports (`tunables`) — empty
    /// when the blueprint has no numeric anchors. The editor shows this count
    /// so the operator knows the saved strategy is evolution-ready.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tunables: Vec<String>,
}

/// `blueprint.load` params (#393): `{ "name": "<package name>" }` — the
/// strategy whose blueprint document the editor wants back.
#[derive(Debug, Clone, Deserialize)]
pub struct BlueprintLoadParams {
    pub name: String,
}

/// `blueprint.load` result: the package's blueprint document, verbatim, plus
/// its RELATIVE path (same convention as the save receipt). Read-only — it
/// answers from the strategy root the kernel already scans, so the editor can
/// preload a package without the UI ever touching the filesystem.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlueprintLoadResult {
    pub name: String,
    pub blueprint_path: String,
    pub json: String,
}

/// `blueprint.loadSource` params: same shape as `blueprint.load` — the
/// hand-written package whose Lua source the editor's source mode wants back.
#[derive(Debug, Clone, Deserialize)]
pub struct BlueprintLoadSourceParams {
    pub name: String,
}

/// `blueprint.loadSource` result: the package's entry file verbatim + the
/// manifest fields the editor needs to re-seal a write-back. `manifestPath`
/// is relative (the save-receipt convention). `modes`/`author`/`description`
/// travel as raw JSON so the editor can round-trip them untouched.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlueprintLoadSourceResult {
    pub name: String,
    pub package_dir: String,
    pub lua_path: String,
    pub manifest_path: String,
    pub lua: String,
    /// The manifest's `sha256` of the CURRENT entry file, hex — the editor
    /// shows it and the write-back re-derives it from the new bytes.
    pub sha256: String,
    pub manifest: Value,
}

/// `blueprint.saveSource` params: the package to overwrite (must already
/// exist — this verb EDITS, it does not create; creation is a blueprint save)
/// and the new entry-file body. `overwrite` must be explicit, matching
/// `blueprint.save`.
#[derive(Debug, Clone, Deserialize)]
pub struct BlueprintSaveSourceParams {
    pub name: String,
    pub lua: String,
    #[serde(default)]
    pub overwrite: bool,
}

/// `blueprint.saveSource` result: what landed, in the save-receipt shape
/// (relative paths + the NEW sha256 the manifest now carries + byte counts).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlueprintSaveSourceResult {
    pub name: String,
    pub package_dir: String,
    pub lua_path: String,
    pub manifest_path: String,
    pub lua_sha256: String,
    pub bytes: [u64; 2],
}
