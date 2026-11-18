//! Dynamic-library loading: dlopen + C ABI v2 negotiation. Pure code motion
//! from `loader.rs` (no logic edits).

use std::path::Path;

use crate::strategies::foreign::LoadedLibrary;

use super::LoadOutcome;

/// A loaded, negotiated v2 strategy before registration.
pub struct LoadedForeign {
    pub strategy: crate::strategies::foreign::ForeignStrategy,
    pub name: String,
    pub version: String,
    /// Shared entry gates this library declared it does not need (E2-b / #27).
    /// Default (nothing declared, or no such symbol) = fully gated.
    pub gate_exemptions: crate::strategies::GateExemptions,
    /// The knobs this library declared evolvable (E2-c / #28). Empty = **not
    /// evolvable** (no symbol, or nothing declared), which is an explicit
    /// declaration and is reported as such at registration.
    pub evolvable_knobs: Vec<strategy_logic::params::KnobSpec>,
    /// Hold-to-settlement declaration: the strategy's positions are meant to be
    /// redeemed at expiry, not sold on the exit ladder. Default (no symbol) =
    /// false; reported at registration so the operator sees it.
    pub holds_to_settlement: bool,
    /// Strategy-API-1.0 mode declaration (E24 / #330), read once at load
    /// through the OPTIONAL `bk_strategy_declare_modes` symbol. Empty = no
    /// symbol, or the symbol returned NULL — the "undeclared" of §7.4, which
    /// is every 0.2 library's state. An INVALID payload never reaches here:
    /// the load is refused earlier with the validator's `index`/`got` error.
    pub declared_modes: Vec<blitzkrieg_strategy_api::StrategyMode>,
    /// RFC 568 pair-declaration default (#642), read through the OPTIONAL
    /// `bk_strategy_entry_pair` symbol: the `{key, leg}` label pair attached
    /// to every entry whose evaluate JSON carries no per-entry `pair` of its
    /// own. `None` = no symbol / NULL / malformed payload = no default —
    /// exactly the pre-existing behaviour of a library without the symbol.
    /// The core `TradePairDecl` (the same type the Lua pass-through carries),
    /// not a mirror.
    pub entry_pair: Option<crate::signal::TradePairDecl>,
    /// Three-state trailing-stop verdict (#642), read through the OPTIONAL
    /// `bk_strategy_trailing_exit` symbol and decoded by the SHARED
    /// capability model's `TrailingExitVerdict` (issue #843). `None` = no
    /// symbol (or the -1 return) = follow the kernel-wide `risk.setExit`
    /// trailingEnabled value, which is every existing library's behaviour
    /// byte-identically.
    pub wants_trailing_exit: Option<bool>,
    /// E-VA atomic-batch declaration (#642), read through the OPTIONAL
    /// `bk_strategy_atomic_batch` symbol. `false` = no symbol (or the 0
    /// return) = the per-order path, unchanged. Arms nothing by itself: the
    /// pot comes from `CoreConfig::virtual_limits`.
    pub atomic_batch: bool,
}

/// Load and negotiate a v2 strategy library, returning it boxed as the full
/// `EngineStrategy` contract ready for `Engine::register_user_strategy`.
pub fn load_foreign(path: &Path) -> Result<LoadedForeign, LoadOutcome> {
    // Both static policy stages run BEFORE dlopen: a rejected path must never
    // reach the dynamic linker, because opening the library already runs its
    // initialisers.
    let policy_error = |path: &Path| -> Option<String> {
        super::policy::policy_allows(path)
            .err()
            .or_else(|| super::policy::path_policy(path).err())
    };
    let fail = |reason: String| -> Result<LoadedForeign, LoadOutcome> {
        Err(if let Some(policy) = policy_error(path) {
            LoadOutcome::Rejected {
                path: path.to_path_buf(),
                reason: format!("{policy} (load failed: {reason})"),
            }
        } else {
            LoadOutcome::Failed {
                path: path.to_path_buf(),
                reason,
            }
        })
    };

    if let Err(reason) = super::policy::policy_allows(path) {
        return Err(LoadOutcome::Rejected {
            path: path.to_path_buf(),
            reason,
        });
    }
    if let Err(reason) = super::policy::path_policy(path) {
        return Err(LoadOutcome::Rejected {
            path: path.to_path_buf(),
            reason,
        });
    }

    // SAFETY: opening a shared library runs its initialisers. We only load
    // libraries the operator placed in the strategy directory after policy.
    let lib = match unsafe { libloading::Library::new(path) } {
        Ok(l) => l,
        Err(e) => return fail(format!("dlopen failed: {e}")),
    };

    // 1) Mandatory version symbol — negotiate before trusting the vtable layout.
    let version_fn = match unsafe {
        lib.get::<unsafe extern "C" fn() -> u32>(blitzkrieg_strategy_api::BK_VERSION_SYMBOL)
    } {
        Ok(f) => f,
        Err(_) => {
            return fail(format!(
                "missing symbol bk_strategy_abi_version; a v1/pre-v2 library? rebuild against strategy-api ABI v{}",
                blitzkrieg_strategy_api::BK_ABI_VERSION
            ));
        }
    };
    let reported = unsafe { version_fn() };
    if reported != blitzkrieg_strategy_api::BK_ABI_VERSION {
        return fail(format!(
            "ABI version mismatch: library exports {reported}, kernel requires {}; \
             rebuild the strategy against strategy-api ABI v{} (no v1 shim)",
            blitzkrieg_strategy_api::BK_ABI_VERSION,
            blitzkrieg_strategy_api::BK_ABI_VERSION
        ));
    }

    // 2) JSON deallocator, resolved from THIS library.
    let free_string = unsafe {
        lib.get::<unsafe extern "C" fn(*mut std::ffi::c_char)>(
            blitzkrieg_strategy_api::BK_FREE_STRING_SYMBOL,
        )
    }
    .ok()
    .map(|s| *s)
    .unwrap_or(
        blitzkrieg_strategy_api::bk_strategy_free_string
            as unsafe extern "C" fn(*mut std::ffi::c_char),
    );

    // 2b) OPTIONAL per-strategy gate exemption declaration (E2-b / #27). Absent
    // symbol = nothing declared = fully gated, which is why adding this
    // capability needs no ABI bump (the vtable layout is untouched).
    let gate_exemptions_fn = unsafe {
        lib.get::<blitzkrieg_strategy_api::BkGateExemptionsFn>(
            blitzkrieg_strategy_api::BK_GATE_EXEMPTIONS_SYMBOL,
        )
    }
    .ok()
    .map(|s| *s);

    // 2c) OPTIONAL per-strategy evolvable-knob declaration (E2-c / #28). Same rule:
    // absent symbol = declares nothing = NOT evolvable, so no ABI bump and older
    // libraries keep loading unchanged.
    let evolvable_knobs_fn = unsafe {
        lib.get::<blitzkrieg_strategy_api::BkEvolvableKnobsFn>(
            blitzkrieg_strategy_api::BK_EVOLVABLE_KNOBS_SYMBOL,
        )
    }
    .ok()
    .map(|s| *s);

    // 3) Factory → vtable. Take the raw fn pointer out of the symbol so the
    // library handle can be moved into the shared wrapper below (the `Symbol`
    // itself borrows the `Library`).
    let create_fn: unsafe extern "C" fn() -> *const blitzkrieg_strategy_api::BkStrategyVtable =
        match unsafe {
            lib.get::<unsafe extern "C" fn() -> *const blitzkrieg_strategy_api::BkStrategyVtable>(
                blitzkrieg_strategy_api::BK_CREATE_SYMBOL,
            )
        } {
            Ok(s) => *s,
            Err(e) => return fail(format!("missing symbol bk_strategy_create: {e}")),
        };
    let vt_ptr = unsafe { create_fn() };
    if vt_ptr.is_null() {
        return fail("bk_strategy_create returned null".into());
    }
    // SAFETY: library guarantees a pointer to a static vtable; copy by value.
    let vtable = unsafe { std::ptr::read(vt_ptr) };
    if vtable.abi_version != blitzkrieg_strategy_api::BK_ABI_VERSION {
        return fail(format!(
            "vtable ABI mismatch: vtable={}, kernel={}",
            vtable.abi_version,
            blitzkrieg_strategy_api::BK_ABI_VERSION
        ));
    }
    if vtable.min_abi > blitzkrieg_strategy_api::BK_ABI_VERSION
        || vtable.abi_version < blitzkrieg_strategy_api::BK_MIN_ABI_VERSION
    {
        return fail(format!(
            "ABI range unsupported: library needs >= {}, speaks {}; kernel speaks {}",
            vtable.min_abi,
            vtable.abi_version,
            blitzkrieg_strategy_api::BK_ABI_VERSION
        ));
    }

    // 4) Required hooks. Presence only — the copied vtable carries the pointers.
    let missing = |hook: &str| format!("v2 vtable missing required hook: {hook}");
    let required = [
        (vtable.create.is_some(), "create"),
        (vtable.destroy.is_some(), "destroy"),
        (vtable.on_book.is_some(), "on_book"),
        (vtable.on_round.is_some(), "on_round"),
        (vtable.evaluate.is_some(), "evaluate"),
    ];
    let missing_hooks: Vec<String> = required
        .into_iter()
        .filter(|(present, _)| !*present)
        .map(|(_, h)| missing(h))
        .collect();
    if !missing_hooks.is_empty() {
        return fail(missing_hooks.join("; "));
    }

    // 4b) OPTIONAL fresh-book binder (E-parity). Absent = the library keeps the
    // plain v2 contract and prices off its own `on_book` stream only.
    let bind_eval_ctx_fn = unsafe {
        lib.get::<blitzkrieg_strategy_api::BkBindEvalCtxFn>(
            blitzkrieg_strategy_api::BK_BIND_EVAL_CTX_SYMBOL,
        )
    }
    .ok()
    .map(|s| *s);
    // 4c) OPTIONAL effective-config reporter (E-parity). Absent = the strategy
    // declares no config view; observability omits the field.
    let config_view_fn = unsafe {
        lib.get::<blitzkrieg_strategy_api::BkConfigViewFn>(
            blitzkrieg_strategy_api::BK_CONFIG_VIEW_SYMBOL,
        )
    }
    .ok()
    .map(|s| *s);
    // 4d) OPTIONAL hold-to-settlement declaration. Absent symbol = not declared
    // = the strategy keeps its normal exit ladder, so no ABI bump and older
    // libraries keep loading unchanged.
    let settlement_holds_fn = unsafe {
        lib.get::<blitzkrieg_strategy_api::BkSettlementHoldsFn>(
            blitzkrieg_strategy_api::BK_SETTLEMENT_HOLDS_SYMBOL,
        )
    }
    .ok()
    .map(|s| *s);

    // 4e) OPTIONAL mode declaration (E24 / #330, API 1.0 §2.3). Absent symbol =
    // "undeclared", which is every 0.2 library's state and needs no ABI bump.
    // The payload is read just below through a THROWAWAY probe instance
    // (`create` → read → `destroy`) and VALIDATED before anything is
    // registered: a declared-but-invalid payload is a configuration error and
    // refuses the load (§7.4) — only the absence of the symbol (or a NULL
    // return) means "undeclared".
    let declare_modes_fn = unsafe {
        lib.get::<blitzkrieg_strategy_api::BkDeclareModesFn>(
            blitzkrieg_strategy_api::BK_DECLARE_MODES_SYMBOL,
        )
    }
    .ok()
    .map(|s| *s);

    // 4f) OPTIONAL closed-bar hook (#642). Absent = the library is never
    // driven by the K-line path — exactly the pre-existing default, so no ABI
    // bump and older libraries keep loading unchanged.
    let on_kline_fn = unsafe {
        lib.get::<blitzkrieg_strategy_api::BkOnKlineFn>(blitzkrieg_strategy_api::BK_ON_KLINE_SYMBOL)
    }
    .ok()
    .map(|s| *s);
    // 4g) OPTIONAL pair-declaration default (#642, RFC 568). Absent = no
    // default; entries without their own per-entry `pair` field then carry
    // None, byte-identically to today.
    let entry_pair_fn = unsafe {
        lib.get::<blitzkrieg_strategy_api::BkEntryPairFn>(
            blitzkrieg_strategy_api::BK_ENTRY_PAIR_SYMBOL,
        )
    }
    .ok()
    .map(|s| *s);
    // 4h) OPTIONAL three-state trailing verdict (#642). Absent = follow the
    // kernel-wide `risk.setExit` trailingEnabled value.
    let trailing_exit_fn = unsafe {
        lib.get::<blitzkrieg_strategy_api::BkTrailingExitFn>(
            blitzkrieg_strategy_api::BK_TRAILING_EXIT_SYMBOL,
        )
    }
    .ok()
    .map(|s| *s);
    // 4i) OPTIONAL atomic-batch declaration (#642, E-VA). Absent = the
    // per-order path. Arms nothing by itself (the pot is host config).
    let atomic_batch_fn = unsafe {
        lib.get::<blitzkrieg_strategy_api::BkAtomicBatchFn>(
            blitzkrieg_strategy_api::BK_ATOMIC_BATCH_SYMBOL,
        )
    }
    .ok()
    .map(|s| *s);

    let name = unsafe { cstr_to_string(vtable.name) }.unwrap_or_else(|| "unnamed".into());
    let version = unsafe { cstr_to_string(vtable.version) }.unwrap_or_else(|| "0.0.0".into());

    tracing::info!(
        strategy = %name,
        symbol_present = declare_modes_fn.is_some(),
        "mode declaration: begin read"
    );
    // 4e, read) Read the §2.3 declaration through a THROWAWAY probe instance
    // (`create` → symbol → `destroy`) and VALIDATE it before step 5 builds
    // anything persistent: a declared-but-invalid payload is a configuration
    // error and refuses the load with the validator's `index`/`got`
    // diagnostic (§7.4). The probe exists because the declaration is a
    // read-once, load-time property — plumbing a ninth optional symbol through
    // every instance (and every shadow twin) would buy nothing. `create` and
    // `destroy` were verified present in step 4; a null `create` result leaves
    // the declaration unreadable, and an unreadable optional declaration is
    // "undeclared" — the same rule a NULL return already gets.
    let declared_modes = match declare_modes_fn {
        None => Vec::new(),
        Some(f) => {
            // SAFETY: required-hook presence verified in step 4; the probe is
            // driven exactly like the real instance (create → read → destroy).
            let probe = unsafe { vtable.create.unwrap_or_else(|| unreachable!())() };
            if probe.is_null() {
                Vec::new()
            } else {
                let raw = unsafe { f(probe) };
                // Copy (and free through the LIBRARY's own deallocator) BEFORE
                // destroying the handle that owns the string.
                let payload = if raw.is_null() {
                    None
                } else {
                    let text = unsafe { std::ffi::CStr::from_ptr(raw) }
                        .to_str()
                        .map(str::to_string)
                        .map_err(|_| "declaration payload is not valid UTF-8".to_string());
                    unsafe { blitzkrieg_strategy_api::bk_strategy_free_string(raw) };
                    Some(text)
                };
                // SAFETY: the probe was produced by this vtable's `create`; the
                // pairing with `destroy` is the documented contract.
                unsafe { vtable.destroy.unwrap_or_else(|| unreachable!())(probe) };
                match payload {
                    // Symbol present but NULL return: "declares nothing" — the
                    // same state as a 0.2 library without the symbol at all.
                    None => Vec::new(),
                    Some(Err(reason)) => {
                        return fail(format!("invalid modes declaration: {reason}"));
                    }
                    Some(Ok(text)) => {
                        match blitzkrieg_strategy_api::modes::parse_strategy_modes(&text) {
                            Ok(modes) => {
                                tracing::info!(
                                    strategy = %name,
                                    modes = ?modes,
                                    "strategy declared market modes (API 1.0)"
                                );
                                modes
                            }
                            Err(e) => {
                                tracing::warn!(
                                    strategy = %name,
                                    error = %e,
                                    "invalid modes declaration; refusing load"
                                );
                                return fail(format!("invalid modes declaration: {e}"));
                            }
                        }
                    }
                }
            }
        }
    };
    if declare_modes_fn.is_none() {
        tracing::info!(
            strategy = %name,
            "no mode declaration symbol (undeclared; not participating in the compatibility handshake)"
        );
    }
    tracing::info!(
        strategy = %name,
        symbol_present = declare_modes_fn.is_some(),
        count = declared_modes.len(),
        "mode declaration: end read"
    );

    // 5) The library handle is shared: the live instance and every shadow twin it
    // spawns hold the same Arc, so the mapping outlives them all.
    // SAFETY: negotiation above established a v2 library with a static vtable,
    // a matching allocator and the optional symbols resolved from it.
    let shared = unsafe {
        LoadedLibrary::new(
            lib,
            create_fn,
            Some(free_string),
            gate_exemptions_fn,
            evolvable_knobs_fn,
            bind_eval_ctx_fn,
            config_view_fn,
            settlement_holds_fn,
            on_kline_fn,
            entry_pair_fn,
            trailing_exit_fn,
            atomic_batch_fn,
        )
    };

    // SAFETY: as above — the shared library is valid, the vtable is static and the
    // instance is driven single-threaded and destroys its handle in Drop.
    let strategy = unsafe {
        crate::strategies::foreign::ForeignStrategy::from_loaded(
            shared,
            name.clone(),
            version.clone(),
            blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        )
    };
    // Read the OPTIONAL declarations once, here, so the load report can state
    // them: an opt-out (E2-b) or "not evolvable" (E2-c) must be visible at
    // registration, not only inferred later.
    let gate_exemptions = crate::strategies::EngineStrategy::gate_exemptions(&strategy);
    let evolvable_knobs = crate::strategies::EngineStrategy::evolvable_knobs(&strategy);
    let holds_to_settlement = crate::strategies::EngineStrategy::holds_to_settlement(&strategy);
    // #642: the four capability declarations read through the trait, whose
    // impls degrade to the pre-existing defaults when the symbol is absent
    // (no pair default / follow-kernel trailing / per-order batching).
    let entry_pair = crate::strategies::EngineStrategy::entry_pair(&strategy);
    let wants_trailing_exit = crate::strategies::EngineStrategy::wants_trailing_exit(&strategy);
    let atomic_batch = crate::strategies::EngineStrategy::atomic_batch(&strategy);

    Ok(LoadedForeign {
        strategy,
        name,
        version,
        gate_exemptions,
        evolvable_knobs,
        holds_to_settlement,
        declared_modes,
        entry_pair,
        wants_trailing_exit,
        atomic_batch,
    })
}

/// Read a NUL-terminated C string into an owned `String` (None for null).
unsafe fn cstr_to_string(p: *const std::ffi::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    unsafe { std::ffi::CStr::from_ptr(p) }
        .to_str()
        .ok()
        .map(|s| s.to_string())
}
