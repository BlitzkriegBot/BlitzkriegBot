//! Trust policy (#188): where a strategy library may be loaded from, the
//! approval-manifest gate, and path canonicalization. Pure code motion from
//! `loader.rs` (no logic edits).

use std::path::{Path, PathBuf};

use super::hash::sha256_file;

pub const ENV_ALLOW_DIRS: &str = "BLITZKRIEG_STRATEGY_ALLOW_DIRS";
pub const ENV_MANIFEST: &str = "BLITZKRIEG_STRATEGY_MANIFEST";
/// The primary strategy tree every repository ships.
pub const APPROVED_ROOT: &str = "user_layer/strategies";
/// Every approved root, relative to a repository root.
pub const APPROVED_ROOTS: [&str; 2] = [APPROVED_ROOT, "user_layer/parity_strategy"];
/// Operator-written approval list, looked up under each repository root.
pub const DEFAULT_MANIFEST: &str = "user_layer/strategies/approved.manifest";
/// Where machine-generated (Shadow Evolution, soak, download) artifacts land.
/// Never an approved root — see [`path_policy`].
const ARTIFACT_ROOTS: [&str; 2] = ["data", "shadow_evolution"];

/// The trust decision for one candidate library path, with the full policy
/// resolved once so a caller can inspect it (tests, receipts, logs).
#[derive(Debug, Clone, Default)]
pub struct TrustPolicy {
    /// Canonical directories that may hold loadable strategies.
    pub approved_dirs: Vec<PathBuf>,
    /// Explicitly untrusted directories (shared temp, downloads, …). Listed for
    /// the rejection message; a path here is refused unless an approved root is
    /// a prefix (a repository checked out under `$TMPDIR` is still a repository).
    pub untrusted_dirs: Vec<PathBuf>,
    /// Approval manifest, when one exists.
    pub manifest: Option<PathBuf>,
}

impl TrustPolicy {
    /// Resolve the policy from the environment, the process working directory
    /// and the compile-time repository location.
    pub fn from_env() -> Self {
        let mut approved_dirs = repo_roots()
            .into_iter()
            .flat_map(|r| APPROVED_ROOTS.into_iter().map(move |a| r.join(a)))
            .collect::<Vec<_>>();
        if let Ok(extra) = std::env::var(ENV_ALLOW_DIRS) {
            approved_dirs.extend(
                extra
                    .split(':')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from),
            );
        }
        let mut approved_dirs = approved_dirs
            .into_iter()
            .filter_map(|p| canonical_dir(&p))
            .collect::<Vec<_>>();
        approved_dirs.sort();
        approved_dirs.dedup();

        let manifest = std::env::var(ENV_MANIFEST)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_file())
            .or_else(|| {
                repo_roots()
                    .into_iter()
                    .map(|r| r.join(DEFAULT_MANIFEST))
                    .find(|p| p.is_file())
            });

        Self {
            approved_dirs,
            untrusted_dirs: untrusted_dirs(),
            manifest,
        }
    }

    /// Check one candidate library path.
    ///
    /// Order is load-bearing: a path inside an approved root short-circuits the
    /// untrusted check (developers and CI check the repository out wherever they
    /// like, including `$TMPDIR`), while everything else needs either an approved
    /// root or a matching manifest entry.
    pub fn check(&self, path: &Path) -> Result<(), String> {
        let absolute = absolutize(path);
        let inside_approved = self
            .approved_dirs
            .iter()
            .any(|root| absolute.starts_with(root));

        // 1. Never load a library any local user can rewrite.
        #[cfg(unix)]
        if let Ok(meta) = std::fs::metadata(&absolute) {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o002 != 0 {
                return Err(format!(
                    "strategy library is world-writable ({mode:04o}): {}",
                    absolute.display()
                ));
            }
        }

        // 2. Machine-generated artifacts (Shadow Evolution variants, soak
        //    outputs, downloaded files) are quarantined: an approved root alone
        //    is not enough, they need an explicit hash-pinned approval. This is
        //    the "进化产物必须人工审批" rule.
        if is_machine_generated(&absolute) {
            return self.approval_or_refuse(&absolute, "machine-generated artifact");
        }

        // 3. Shared temp / download directories.
        if !inside_approved
            && let Some(bad) = self.untrusted_dirs.iter().find(|d| absolute.starts_with(d))
        {
            return Err(format!(
                "{} lives under {} — a shared directory is never a strategy root; move the \
                 library under {} or declare its directory in {} (see SECURITY.md)",
                absolute.display(),
                bad.display(),
                self.approved_dirs
                    .first()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| APPROVED_ROOT.to_string()),
                ENV_ALLOW_DIRS
            ));
        }

        // 4. Approved root.
        if inside_approved {
            return Ok(());
        }

        // 5. Otherwise: an explicit, hash-pinned approval.
        self.approval_or_refuse(&absolute, "outside every approved strategy root")
    }

    /// Require a manifest entry whose digest matches the file on disk.
    fn approval_or_refuse(&self, absolute: &Path, why: &str) -> Result<(), String> {
        let Some(manifest) = &self.manifest else {
            return Err(format!(
                "{} is rejected ({why}) and no approval manifest exists; approve it by adding a \
                 `sha256 <path>` line to {} (or set {})",
                absolute.display(),
                DEFAULT_MANIFEST,
                ENV_MANIFEST
            ));
        };
        let text = std::fs::read_to_string(manifest)
            .map_err(|e| format!("cannot read approval manifest {}: {e}", manifest.display()))?;
        let Ok(actual) = sha256_file(absolute) else {
            return Err(format!(
                "cannot hash {} for approval against {}",
                absolute.display(),
                manifest.display()
            ));
        };
        for entry in parse_manifest(&text, manifest) {
            if entry.path == absolute {
                return if entry.sha256 == actual {
                    Ok(())
                } else {
                    Err(format!(
                        "{} is listed in {} but its sha256 changed (approved {}, found {actual}); \
                         re-review the library and update the manifest",
                        absolute.display(),
                        manifest.display(),
                        entry.sha256
                    ))
                };
            }
        }
        Err(format!(
            "{} is rejected ({why}) and is not listed in {}",
            absolute.display(),
            manifest.display()
        ))
    }
}

/// One `sha256 <path>` line of an approval manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub sha256: String,
    pub path: PathBuf,
}

/// Parse an approval manifest.
///
/// Deliberately tolerant about layout, strict about content: blank lines and
/// `#` comments are ignored, `sha256sum` order (`<hash> <path>`) and the reverse
/// are both accepted, a leading `*` (binary marker) is dropped, and relative
/// paths resolve against the manifest's own directory so a committed manifest
/// stays portable. A line with no 64-hex-char field is skipped rather than
/// quietly approving something.
pub fn parse_manifest(text: &str, manifest: &Path) -> Vec<ManifestEntry> {
    let base = manifest
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(first), Some(second)) = (fields.next(), fields.next()) else {
            continue;
        };
        // `sha256sum` writes `<hash>  <path>` and, in binary mode, `<hash>  *<path>`.
        // The reverse order is accepted too, so a hand-written manifest cannot be
        // "wrongly formatted" in a way that silently approves nothing.
        let (hash, raw_path) = if is_sha256_hex(first) {
            (first.to_lowercase(), second)
        } else if is_sha256_hex(second) {
            (second.to_lowercase(), first)
        } else if is_sha256_hex(first.strip_prefix('*').unwrap_or(first)) {
            (
                first.strip_prefix('*').unwrap_or(first).to_lowercase(),
                second,
            )
        } else if is_sha256_hex(second.strip_prefix('*').unwrap_or(second)) {
            (
                second.strip_prefix('*').unwrap_or(second).to_lowercase(),
                first,
            )
        } else {
            continue;
        };
        let raw_path = raw_path.strip_prefix('*').unwrap_or(raw_path);
        if raw_path.is_empty() {
            continue;
        }
        let p = Path::new(raw_path);
        let path = if p.is_absolute() {
            p.to_path_buf()
        } else {
            base.join(p)
        };
        out.push(ManifestEntry {
            sha256: hash,
            path: absolutize(&path),
        });
    }
    out
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether a path is a machine-generated artifact rather than a checked-in or
/// hand-built strategy: anything under a `data/` tree, or any component naming
/// Shadow Evolution. These never satisfy the approved-root rule on their own.
pub(super) fn is_machine_generated(path: &Path) -> bool {
    path.components().any(|c| {
        let name = c.as_os_str().to_string_lossy().to_ascii_lowercase();
        ARTIFACT_ROOTS.contains(&name.as_str())
            || name.starts_with("shadow")
            || name.starts_with("evolution")
    })
}

/// Candidate repository roots, canonicalized and de-duplicated.
///
/// Three anchors, because each one matters at a different time: the compile-time
/// crate location (`<root>/core/blitzkrieg_core`) covers the build tree, the
/// running binary's own tree covers a DEPLOYED binary started from anywhere
/// (#265), and the runtime working directory covers the shell that starts a
/// build-tree binary from the repository root.
pub(super) fn repo_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(build_root) = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2) {
        roots.push(build_root.to_path_buf());
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(root) = repo_root_of(&exe)
    {
        roots.push(root);
    }
    if let Ok(cwd) = std::env::current_dir() {
        let mut cur: Option<&Path> = Some(&cwd);
        // Bounded walk: a kernel started deep in a tree still finds its root,
        // and a kernel started at `/` does not scan the filesystem.
        for _ in 0..8 {
            let Some(dir) = cur else { break };
            if APPROVED_ROOTS.iter().any(|a| dir.join(a).is_dir()) || dir.join(".git").exists() {
                roots.push(dir.to_path_buf());
                break;
            }
            cur = dir.parent();
        }
        roots.push(cwd);
    }
    let mut out: Vec<PathBuf> = roots
        .into_iter()
        .filter_map(|p| canonical_dir(&p))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The repository tree a binary at `exe` lives in, if any (#265).
///
/// This is the runtime counterpart of the compile-time anchor above, and it is
/// the one that holds on a DEPLOYMENT: the release binary is copied into a
/// checkout (or started from one through a shim) and the process working
/// directory may be anywhere. `CARGO_MANIFEST_DIR` then names the build machine's
/// tree — a path that need not exist here — so `canonical_dir` silently drops it
/// and `approved_dirs` can end up EMPTY while the binary sits inside a perfectly
/// good checkout. Every strategy is refused at that point and the kernel used to
/// keep running anyway.
///
/// Deliberately a search for a MARKER rather than "two levels up": the deploy
/// layout (`<checkout>/target/release/blitzkrieg-core`) and a build layout
/// (`<checkout>/target/debug/deps/<test binary>`) differ in depth, and a fixed
/// offset silently yields a directory that carries no approved root. The markers
/// are the same ones the cwd walk uses, plus `user_layer` — the tree a strategy
/// library ships in is exactly this codebase's definition of "the repository".
///
/// A binary that lives outside any such tree (a copy in `/usr/local/bin`, a
/// cargo target dir with no checkout above it) contributes nothing, on purpose:
/// approving an arbitrary install prefix as a strategy root would be a trust
/// change, not a path fix — the operator's escape hatch for that is
/// [`ENV_ALLOW_DIRS`].
pub(super) fn repo_root_of(exe: &Path) -> Option<PathBuf> {
    let mut cur = exe.parent();
    // Bounded walk, same bound as the cwd anchor: a binary installed at the
    // filesystem root does not scan the whole tree.
    for _ in 0..8 {
        let dir = cur?;
        if APPROVED_ROOTS.iter().any(|a| dir.join(a).is_dir())
            || dir.join("user_layer").is_dir()
            || dir.join(".git").exists()
        {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// Make a path comparable with the canonical policy directories.
///
/// `canonicalize` needs every component to exist, and a rejected candidate often
/// does not — so the deepest existing ancestor is canonicalized and the missing
/// tail re-appended. On macOS this is what makes `$TMPDIR` (`/var/folders/…`)
/// match the canonical `/private/var/folders/…`; skipping it would leave the
/// temp-directory rule silently ineffective.
fn absolutize(path: &Path) -> PathBuf {
    if let Ok(c) = path.canonicalize() {
        return c;
    }
    let base = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(d) => d.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur: &Path = &base;
    loop {
        if let Ok(canonical) = cur.canonicalize() {
            let mut out = canonical;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                cur = parent;
            }
            _ => return base,
        }
    }
}

fn canonical_dir(p: &Path) -> Option<PathBuf> {
    let c = p.canonicalize().ok()?;
    c.is_dir().then_some(c)
}

/// Directories that are never strategy roots: shared scratch space and the two
/// folders a browser puts downloads in.
pub(super) fn untrusted_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = [
        "/tmp",
        "/var/tmp",
        "/private/tmp",
        "/private/var/tmp",
        "/Users/Shared",
    ]
    .into_iter()
    .filter_map(|p| canonical_dir(Path::new(p)))
    .collect();
    if let Some(tmp) = canonical_dir(&std::env::temp_dir()) {
        dirs.push(tmp);
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for sub in ["Downloads", "Desktop", "Library/Mobile Documents"] {
            if let Some(d) = canonical_dir(&home.join(sub)) {
                dirs.push(d);
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// The trust gate for one candidate library. Thin wrapper over
/// [`TrustPolicy::from_env`] + [`TrustPolicy::check`] so every load path shares
/// one decision.
pub fn path_policy(path: &Path) -> Result<(), String> {
    TrustPolicy::from_env().check(path)
}

/// Static name policy: credential-looking filenames and non-library extensions
/// are refused before any dlopen. Pure and testable.
pub fn policy_allows(path: &Path) -> Result<(), String> {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    for bad in [".env", "private", "secret", "key", "credential"] {
        if name.to_lowercase().contains(bad) {
            return Err(format!(
                "strategy library name looks like it bundles credentials: {name}"
            ));
        }
    }
    let ext_ok = name.ends_with(".so") || name.ends_with(".dylib") || name.ends_with(".dll");
    if !ext_ok {
        return Err(format!("not a shared library: {name}"));
    }
    Ok(())
}
