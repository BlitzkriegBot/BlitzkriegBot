use super::super::typed;
use super::prelude::*;
use crate::schema::*;

// The `blueprint` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_blueprint(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::BLUEPRINT_COMPILE => {
            typed(params, |p: BlueprintCompileParams| async move {
                // Compile errors carry the node id by construction (#361);
                // InvalidParams is the wire class for "your document is wrong".
                let lua = crate::blueprint::compile(&p.json)
                    .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, e))?;
                Ok::<_, CoreError>(
                    serde_json::to_value(BlueprintCompileResult { lua }).unwrap_or(Value::Null),
                )
            })
            .await
        }
        method::BLUEPRINT_SAVE => {
            // The save root is configuration: snapshot the core's config before
            // the async move (a sync field clone, no lock held across await).
            let core_cfg = core.lock().await.config().clone();
            typed(params, |p: BlueprintSaveParams| async move {
                let receipt = blueprint_save(&p, &core_cfg)
                    .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, e))?;
                // Persisted on disk — register the package into the live
                // dispatch so the new parameters take effect this session
                // (seconds-to-live, like round_duration.set), instead of
                // waiting for the next explicit reload or restart.
                let _ = core.lock().await.reload_lua_packages();
                Ok::<_, CoreError>(serde_json::to_value(receipt).unwrap_or(Value::Null))
            })
            .await
        }
        method::BLUEPRINT_LOAD => {
            let core_cfg = core.lock().await.config().clone();
            typed(params, |p: BlueprintLoadParams| async move {
                let doc = blueprint_load(&p, &core_cfg)
                    .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, e))?;
                Ok::<_, CoreError>(serde_json::to_value(doc).unwrap_or(Value::Null))
            })
            .await
        }
        method::BLUEPRINT_LOAD_SOURCE => {
            let core_cfg = core.lock().await.config().clone();
            typed(params, |p: BlueprintLoadSourceParams| async move {
                let doc = blueprint_load_source(&p, &core_cfg)
                    .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, e))?;
                Ok::<_, CoreError>(serde_json::to_value(doc).unwrap_or(Value::Null))
            })
            .await
        }
        method::BLUEPRINT_SAVE_SOURCE => {
            let core_cfg = core.lock().await.config().clone();
            typed(params, |p: BlueprintSaveSourceParams| async move {
                let receipt = blueprint_save_source(&p, &core_cfg)
                    .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, e))?;
                Ok::<_, CoreError>(serde_json::to_value(receipt).unwrap_or(Value::Null))
            })
            .await
        }
        method::BLUEPRINT_SCHEMA => {
            Ok(serde_json::to_value(crate::blueprint::schema()).unwrap_or(Value::Null))
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}

/// The strategy-package root `blueprint.save` writes into.
///
/// The kernel's OWN discovery rule decides the path (not the spec's draft
/// wording): Lua packages live where `--lua-strategy-dir` scans, which is
/// `user_layer/strategies_lua` by default (`main::default_lua_strategy_dir`)
/// — `user_layer/strategies` is the DYLIB fixture tree and must never receive
/// a Lua package (docs/DEV_V0_3.md §1901). `BLITZKRIEG_LUA_STRATEGY_DIR`
/// overrides the default — the same deployment knob idea as the CLI flag,
/// kept as an env var so a host operator can redirect package writes without
/// an IPC-side path parameter (no caller may ever name a directory).
/// #362: validate a save request and write the strategy package.
///
/// Order is the safety property, mirroring the compile chain: the NAME is
/// screened first (it becomes a directory name and a `place_order`-adjacent
/// literal — the same `os.`/`io.`/`debug` wall the compiler applies to the
/// blueprint name applies here, plus a package must not be named like a path),
/// then the blueprint compiles fresh (a compile refusal means nothing was
/// written), then an EXISTING package of the same name refuses the save unless
/// the caller explicitly overwrote (a silent overwrite could replace a
/// strategy an operator has enabled), and only then do the three files land.
/// The save root, read from the core's OWN config (falling back to the
/// deployment default). An env-var read here would be a shared-process side
/// channel that races test parallelism for no production benefit: the kernel's
/// strategy root is configuration, so it is read as configuration.
fn blueprint_strategy_root(core_cfg: &crate::service::CoreConfig) -> String {
    core_cfg
        .lua_strategy_dir
        .clone()
        .unwrap_or_else(|| "user_layer/strategies_lua".to_string())
}

/// The package-name screen shared by `blueprint.save` and `blueprint.load`
/// (#393): the name becomes a directory name and a `place_order`-adjacent
/// literal — the same `os.`/`io.`/`debug` wall the compiler applies to the
/// blueprint name applies here, plus a package must not be named like a path.
/// The load path needs the SAME screen so no caller can steer its read
/// outside the strategy root.
fn validate_package_name(raw: &str) -> Result<String, String> {
    const FORBIDDEN: [&str; 3] = ["os.", "io.", "debug"];
    let name = raw.trim();
    if name.is_empty() {
        return Err("blueprint name must not be empty".to_string());
    }
    if name.len() > 64 {
        return Err(format!(
            "blueprint name is {} characters; the ceiling is 64",
            name.len()
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "blueprint name {name:?} may only hold ASCII letters, digits, '_' and '-' — \
             it becomes a strategy-package directory name"
        ));
    }
    if let Some(bad) = FORBIDDEN.iter().find(|bad| name.contains(**bad)) {
        return Err(format!(
            "blueprint name {bad:?} substring is refused: the name is embedded in the \
             generated source verbatim"
        ));
    }
    Ok(name.to_string())
}

/// #393: read one saved package's blueprint document back — the read-only
/// mirror of [`blueprint_save`], feeding the strategy page's 编辑策略 entry.
/// Hand-written packages carry no `blueprint.json` (they were never compiled
/// from one), and that fact is the ERROR — the caller says so, it does not
/// synthesize a graph the strategy never had.
fn blueprint_load(
    p: &BlueprintLoadParams,
    core_cfg: &crate::service::CoreConfig,
) -> Result<BlueprintLoadResult, String> {
    let name = validate_package_name(&p.name)?;
    let root = blueprint_strategy_root(core_cfg);
    let path = std::path::Path::new(&root)
        .join(&name)
        .join("blueprint.json");
    let json = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "no blueprint document for `{name}` under {root} — the package was not \
             written by blueprint.save, so there is no graph to preload"
        )
    })?;
    Ok(BlueprintLoadResult {
        blueprint_path: format!("{root}/{name}/blueprint.json"),
        json,
        name,
    })
}

/// 手写包的读回（`blueprint.loadSource`）：`blueprint.load` 在包没有
/// blueprint.json 时会拒绝，而编辑入口必须能打开**所有** Lua 包 —— 这里读
/// 回入口文件与 manifest 本身，编辑器的源码模式展示它们。同一名字闸门；
/// 入口文件按 manifest 的 `entry` 字段定位（缺省 `strategy.lua`），manifest
/// 缺失或字段缺失按原样报错 —— 不编造元数据。
fn blueprint_load_source(
    p: &BlueprintLoadSourceParams,
    core_cfg: &crate::service::CoreConfig,
) -> Result<BlueprintLoadSourceResult, String> {
    let name = validate_package_name(&p.name)?;
    let root = blueprint_strategy_root(core_cfg);
    let pkg = std::path::Path::new(&root).join(&name);
    let manifest_raw = std::fs::read_to_string(pkg.join("manifest.json")).map_err(|_| {
        format!(
            "no manifest for `{name}` under {root} — not a strategy package the loader \
             would scan"
        )
    })?;
    let manifest: Value = serde_json::from_str(&manifest_raw)
        .map_err(|e| format!("manifest for `{name}` is not valid JSON: {e}"))?;
    let entry = manifest
        .get("entry")
        .and_then(|v| v.as_str())
        .unwrap_or("strategy.lua")
        .to_string();
    if entry.contains("..") || entry.contains('/') || entry.contains('\\') {
        return Err(format!(
            "manifest entry {entry:?} for `{name}` must be a bare file name — refusing to \
             read outside the package directory"
        ));
    }
    let lua = std::fs::read_to_string(pkg.join(&entry))
        .map_err(|e| format!("cannot read {root}/{name}/{entry}: {e}"))?;
    let sha256 = manifest
        .get("sha256")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(BlueprintLoadSourceResult {
        package_dir: pkg.display().to_string(),
        lua_path: format!("{root}/{name}/{entry}"),
        manifest_path: format!("{root}/{name}/manifest.json"),
        lua,
        sha256,
        manifest,
        name,
    })
}

/// 手写包的写回（`blueprint.saveSource`）：`blueprint.loadSource` 的镜像 ——
/// 入口文件原样落盘，sha256 现场重算进 manifest（§6.4：装载器验证字节，漂移
/// 即拒载，所以 manifest 里的摘要必须跟随新字节）。目标包必须已存在（这个
/// 动词编辑、不创建），入口文件名沿用读回时的同一闸门。
fn blueprint_save_source(
    p: &BlueprintSaveSourceParams,
    core_cfg: &crate::service::CoreConfig,
) -> Result<BlueprintSaveSourceResult, String> {
    use sha2::Digest;

    let name = validate_package_name(&p.name)?;
    let root = blueprint_strategy_root(core_cfg);
    let pkg = std::path::Path::new(&root).join(&name);
    let manifest_path = pkg.join("manifest.json");
    if !manifest_path.is_file() {
        return Err(format!(
            "no manifest for `{name}` under {root} — saveSource edits an EXISTING package; \
             create one with blueprint.save first"
        ));
    }
    let manifest_raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("cannot read {root}/{name}/manifest.json: {e}"))?;
    let mut manifest: Value = serde_json::from_str(&manifest_raw)
        .map_err(|e| format!("manifest for `{name}` is not valid JSON: {e}"))?;
    let entry = manifest
        .get("entry")
        .and_then(|v| v.as_str())
        .unwrap_or("strategy.lua")
        .to_string();
    if entry.contains("..") || entry.contains('/') || entry.contains('\\') {
        return Err(format!(
            "manifest entry {entry:?} for `{name}` must be a bare file name — refusing to \
             write outside the package directory"
        ));
    }
    let lua_path = pkg.join(&entry);
    if !p.overwrite {
        let existing = std::fs::read_to_string(&lua_path).unwrap_or_default();
        if existing != p.lua {
            return Err(format!(
                "strategy package {root}/{name} exists with different source — writing over \
                 it must be explicit (overwrite: true)"
            ));
        }
    }
    std::fs::write(&lua_path, p.lua.as_bytes())
        .map_err(|e| format!("cannot write {root}/{name}/{entry}: {e}"))?;

    let mut hasher = sha2::Sha256::new();
    hasher.update(p.lua.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    manifest["sha256"] = Value::String(digest.clone());
    let manifest_body = format!(
        "{}\n",
        serde_json::to_string_pretty(&manifest)
            .map_err(|e| format!("manifest serialization failed: {e}"))?
    );
    std::fs::write(&manifest_path, manifest_body.as_bytes())
        .map_err(|e| format!("cannot write {root}/{name}/manifest.json: {e}"))?;

    Ok(BlueprintSaveSourceResult {
        package_dir: pkg.display().to_string(),
        lua_path: lua_path.display().to_string(),
        manifest_path: format!("{root}/{name}/manifest.json"),
        lua_sha256: digest,
        bytes: [p.lua.len() as u64, manifest_body.len() as u64],
        name,
    })
}

fn blueprint_save(
    p: &BlueprintSaveParams,
    core_cfg: &crate::service::CoreConfig,
) -> Result<BlueprintSaveResult, String> {
    use sha2::Digest;

    let name = validate_package_name(&p.name)?;

    // Compile FRESH — the receipt vouches for what THIS call produced, and a
    // structurally invalid blueprint writes nothing. The tunables ride along:
    // the numeric anchors the compiled source reads through `__param` are
    // exported into the manifest (with ±half-magnitude domains), which is
    // what registers the package as EVOLVABLE (#393 shape: the loader's
    // specs_from_tunables needs an explicit domain to declare a knob).
    let compiled = crate::blueprint::compile_full(&p.json)?;
    let lua = compiled.lua;
    let tunables = compiled.tunables;
    let meta = compiled.meta;

    let root = blueprint_strategy_root(core_cfg);
    let pkg = std::path::Path::new(&root).join(&name);
    if pkg.join("manifest.json").is_file() && !p.overwrite {
        return Err(format!(
            "strategy package {root}/{name} already exists — saving over a package the \
             loader scans must be explicit (overwrite: true)"
        ));
    }
    let pkg_was_dir = pkg.is_dir();
    std::fs::create_dir_all(&pkg).map_err(|e| format!("cannot create {root}/{name}: {e}"))?;

    let blueprint_path = pkg.join("blueprint.json");
    let lua_path = pkg.join("strategy.lua");
    let manifest_path = pkg.join("manifest.json");

    // Manifest bytes are computed in memory now, so staging below only ever
    // does pure IO — a staging failure replaces nothing on disk.
    let mut hasher = sha2::Sha256::new();
    hasher.update(lua.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    let mut tunables_map = serde_json::Map::new();
    for t in &tunables {
        tunables_map.insert(
            t.name.clone(),
            serde_json::json!({
                "type": "decimal",
                "default": t.default,
                "min": t.min,
                "max": t.max,
            }),
        );
    }
    let description = meta.description.clone().unwrap_or_else(|| {
        format!(
            "Blueprint-compiled strategy from blueprint.json (name {name:?}); every \
             decision is the whitelisted node graph it declares — the kernel \
             adjudicates, sizes and gates every intent."
        )
    });
    let mut manifest = serde_json::json!({
        "name": name,
        "version": "1.0.0",
        "api": "1.0",
        "entry": "strategy.lua",
        "sha256": digest,
        "author": "BlitzkriegBot",
        "description": description,
        "modes": [
            {
                "market_type": "prediction",
                "structure": "binary_outcome_wheel"
            }
        ],
    });
    if meta.holds_to_settlement {
        manifest["holds_to_settlement"] = Value::Bool(true);
    }
    if meta.entry_maker {
        manifest["entry_maker"] = Value::Bool(true);
    }
    if let Some(ge) = &meta.gate_exemptions {
        manifest["gate_exemptions"] = serde_json::to_value(ge).unwrap_or(Value::Null);
    }
    if !tunables_map.is_empty() {
        manifest["tunables"] = Value::Object(tunables_map);
    }
    let manifest_body = format!(
        "{}\n",
        serde_json::to_string_pretty(&manifest)
            .map_err(|e| format!("manifest serialization failed: {e}"))?
    );

    // Stage every target as an fsynced temp file in the SAME directory before
    // any target is touched: `rename` on one filesystem is atomic, so the
    // publish step either lands the whole new package or leaves the old one
    // byte-for-byte intact (no half-written blueprint/lua/manifest mix).
    let targets = [
        (&blueprint_path, p.json.as_bytes()),
        (&lua_path, lua.as_bytes()),
        (&manifest_path, manifest_body.as_bytes()),
    ];
    let mut staged: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    for (target, bytes) in targets {
        staged.push((stage_file(target, bytes)?, target.to_path_buf()));
    }

    // Remember what we displace so a failed publish can restore the previous
    // package: the new package lands, or the old one still does.
    let old_contents: Vec<Option<Vec<u8>>> = targets
        .iter()
        .map(|(target, _)| std::fs::read(target).ok())
        .collect();
    if let Err(e) = publish_staged(&staged, &old_contents) {
        // publish_staged already rolls back displaced targets and removes
        // stray temps; this is a defensive last sweep.
        for (temp, _) in &staged {
            let _ = std::fs::remove_file(temp);
        }
        // A brand-new package directory that never held a saved package is
        // an empty shell after a failed save — do not litter it.
        if !pkg_was_dir {
            let _ = std::fs::remove_dir_all(&pkg);
        }
        return Err(e);
    }

    Ok(BlueprintSaveResult {
        name: name.to_string(),
        package_dir: pkg.display().to_string(),
        blueprint_path: blueprint_path.display().to_string(),
        lua_path: lua_path.display().to_string(),
        manifest_path: manifest_path.display().to_string(),
        lua_sha256: digest,
        bytes: [
            p.json.len() as u64,
            lua.len() as u64,
            manifest_body.len() as u64,
        ],
        tunables: tunables.into_iter().map(|t| t.name).collect(),
    })
}

/// Write `bytes` to a temp file sibling of `target` and fsync it. The temp
/// lives on the same filesystem as `target`, so a later `rename` is atomic;
/// the caller swaps it into place with [`publish_staged`] and removes it on
/// any failure.
fn stage_file(target: &std::path::Path, bytes: &[u8]) -> Result<std::path::PathBuf, String> {
    use std::io::Write;
    let mut temp = target.to_path_buf();
    temp.set_extension("tmp");
    let mut file =
        std::fs::File::create(&temp).map_err(|e| format!("cannot stage {temp:?}: {e}"))?;
    file.write_all(bytes)
        .map_err(|e| format!("cannot stage {temp:?}: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("cannot fsync {temp:?}: {e}"))?;
    drop(file);
    Ok(temp)
}

/// Atomically swap every `temp → target` pair into place. If a later rename
/// fails, targets already displaced are rolled back (old bytes restored, or
/// the file removed when it did not exist before) and every stray temp is
/// removed — the package directory is left exactly as it was.
fn publish_staged(
    staged: &[(std::path::PathBuf, std::path::PathBuf)],
    old_contents: &[Option<Vec<u8>>],
) -> Result<(), String> {
    for (i, (temp, target)) in staged.iter().enumerate() {
        if let Err(e) = std::fs::rename(temp, target) {
            for j in (0..i).rev() {
                let (_, tgt) = &staged[j];
                match &old_contents[j] {
                    Some(old) => {
                        let _ = std::fs::write(tgt, old);
                    }
                    None => {
                        let _ = std::fs::remove_file(tgt);
                    }
                }
            }
            let _ = std::fs::remove_file(temp);
            for (stray, _) in staged.iter().skip(i + 1) {
                let _ = std::fs::remove_file(stray);
            }
            return Err(format!("cannot publish {target:?}: {e}"));
        }
    }
    // fsync the directory so the renames survive a crash; the rename
    // atomicity already keeps the package consistent at every instant.
    if let Some(dir) = staged.first().and_then(|(_, t)| t.parent()) {
        let _ = std::fs::File::open(dir).and_then(|f| f.sync_all());
    }
    Ok(())
}
