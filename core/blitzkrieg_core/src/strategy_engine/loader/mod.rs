//! Dynamic strategy loader — dlopen user-layer strategy libraries (`.dylib` /
//! `.so` / `.dll`) implementing C ABI **v2** and wrap them as a full
//! [`crate::strategies::foreign::ForeignStrategy`] (which itself implements the
//! same `EngineStrategy` contract as an in-tree strategy).
//!
//! ARCHITECTURAL GUARANTEE: a loaded strategy only ever receives borrowed
//! read-only market/context views and returns intents as data. It has no handle
//! to credentials, the CLOB client, the order manager or the UDS socket — none
//! of those cross the dynamic boundary. [`policy_allows`] additionally refuses
//! credential-looking filenames before dlopen.
//!
//! ## TRUST MODEL — read this before promising anybody a "sandbox" (#188)
//!
//! A strategy library is **native code loaded into the kernel process** by
//! `dlopen`. It shares the kernel's address space, its file descriptors and its
//! environment — including the venue credentials the live executor reads. There
//! is no sandbox here and none can be built at this layer: a library that wants
//! to exfiltrate keys simply calls `getenv`. The controls below are therefore
//! about *where a library may come from* and *who approved it*, not about
//! limiting what an approved library can do:
//!
//!   1. **Path policy** ([`path_policy`]) — the library must live under an
//!      approved root (`<repo>/user_layer/strategies` by default, plus anything
//!      declared in `BLITZKRIEG_STRATEGY_ALLOW_DIRS`). Shared temp directories
//!      (`/tmp`, `/var/tmp`, `$TMPDIR`), `~/Downloads`, `~/Desktop` and
//!      `/Users/Shared` are never approved roots.
//!   2. **Approval manifest** ([`TrustPolicy`]) — an operator-written list of
//!      `sha256 <path>` lines. A library outside an approved root, or produced by
//!      Shadow Evolution, loads ONLY when the manifest lists it AND the file's
//!      digest still matches. Swapping the bytes in place invalidates the
//!      approval.
//!   3. **Not world-writable** — a library any local user may rewrite is refused
//!      regardless of location.
//!
//! Residual risk, stated plainly: an attacker who can write into an approved
//! directory (or edit the manifest) is inside the trust boundary, because the
//! approval is a local file and not a signature. Process isolation (a strategy
//! child process with a restricted IPC channel) is the only real fix and is
//! tracked separately; until then this is a *gate*, not a sandbox.
//!
//! Negotiation order is deliberate: policy → dlopen → read the version symbol
//! → read the vtable. Version is settled BEFORE the vtable layout is trusted, so
//! a v1 library is rejected on version and never misread as v2.
//!
//! Runtime flow:
//!   1. [`policy_allows`] — static name policy; [`path_policy`] — trust policy.
//!   2. `dlopen` the library.
//!   3. `bk_strategy_abi_version()` — MUST equal 2 (clean break, no v1 shim).
//!   4. resolve `bk_strategy_free_string` and `bk_strategy_create` → vtable.
//!   5. verify vtable.abi_version/min_abi and the REQUIRED hooks.
//!   6. `create()` → handle, then wrap as `ForeignStrategy`.
//!
//! Split note: `policy`, `hash` and `foreign` are domain children of this hub;
//! the public surface is re-exported flat below so `crate::strategy_engine::loader::*`
//! paths keep resolving.

#[cfg(feature = "strategy-loading")]
pub mod foreign;
pub mod hash;
pub mod policy;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

#[cfg(feature = "strategy-loading")]
pub use foreign::{LoadedForeign, load_foreign};
pub use hash::sha256_file;
pub use policy::{APPROVED_ROOT, APPROVED_ROOTS, DEFAULT_MANIFEST, ENV_ALLOW_DIRS, ENV_MANIFEST};
pub use policy::{ManifestEntry, TrustPolicy, parse_manifest, path_policy, policy_allows};

/// Outcome of attempting to load a strategy library.
pub enum LoadOutcome {
    /// Policy rejected the path before any dlopen.
    Rejected { path: PathBuf, reason: String },
    /// Library opened, negotiated and registered.
    Loaded {
        path: PathBuf,
        name: String,
        version: String,
    },
    /// Policy passed but load/negotiate/create failed.
    Failed { path: PathBuf, reason: String },
}

impl std::fmt::Debug for LoadOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadOutcome::Rejected { path, reason } => {
                write!(
                    f,
                    "Rejected {{ path: {}, reason: {} }}",
                    path.display(),
                    reason
                )
            }
            LoadOutcome::Loaded {
                path,
                name,
                version,
            } => {
                write!(
                    f,
                    "Loaded {{ path: {}, name: {}, version: {} }}",
                    path.display(),
                    name,
                    version
                )
            }
            LoadOutcome::Failed { path, reason } => {
                write!(
                    f,
                    "Failed {{ path: {}, reason: {} }}",
                    path.display(),
                    reason
                )
            }
        }
    }
}

/// Diagnostic summary of loading a library (does not register anywhere).
#[cfg(feature = "strategy-loading")]
pub fn load_strategy(path: &Path) -> LoadOutcome {
    match load_foreign(path) {
        Ok(LoadedForeign { name, version, .. }) => LoadOutcome::Loaded {
            path: path.to_path_buf(),
            name,
            version,
        },
        Err(outcome) => outcome,
    }
}

/// Without the feature, report the library as unsupported (builtins still run).
#[cfg(not(feature = "strategy-loading"))]
pub fn load_strategy(path: &Path) -> LoadOutcome {
    // The trust policy is enforced with or without the loader feature: a build
    // that cannot load libraries must not report "not a library" for a path the
    // production build would refuse outright.
    if let Err(reason) = policy_allows(path).and_then(|()| path_policy(path)) {
        return LoadOutcome::Rejected {
            path: path.to_path_buf(),
            reason,
        };
    }
    LoadOutcome::Failed {
        path: path.to_path_buf(),
        reason: "dynamic strategy loading not compiled in (enable feature `strategy-loading`)"
            .into(),
    }
}
