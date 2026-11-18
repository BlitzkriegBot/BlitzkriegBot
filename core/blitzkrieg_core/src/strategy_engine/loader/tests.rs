use super::hash::Sha256;
use super::policy::{is_machine_generated, repo_root_of, repo_roots, untrusted_dirs};
use super::*;
use std::path::{Path, PathBuf};

#[test]
fn policy_accepts_shared_libraries() {
    assert!(policy_allows(Path::new("strategies/sample_strategy.dylib")).is_ok());
    assert!(policy_allows(Path::new("strategies/sample_strategy.so")).is_ok());
}

#[test]
fn policy_rejects_credential_like_and_non_libraries() {
    assert!(policy_allows(Path::new("strategies/private_key.dylib")).is_err());
    assert!(policy_allows(Path::new("strategies/secret.so")).is_err());
    assert!(policy_allows(Path::new("strategies/strategy.toml")).is_err());
}

/// A path that passes policy but has no file on disk must report a load
/// FAILURE (dlopen could not open it), not a panic — and a path the policy
/// refuses must be reported as REJECTED, before anything is opened.
#[test]
fn missing_library_reports_failure_not_panic() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf();
    let approved_missing =
        repo.join("user_layer/strategies/target/release/libdoes_not_exist.dylib");
    let out = load_strategy(&approved_missing);
    assert!(matches!(out, LoadOutcome::Failed { .. }), "got {out:?}");

    // Outside every approved root, the verdict is policy, not dlopen.
    let out = load_strategy(Path::new("/nonexistent/dir/foo.dylib"));
    assert!(matches!(out, LoadOutcome::Rejected { .. }), "got {out:?}");
}

// --- #188 trust policy ---------------------------------------------------

/// FIPS 180-4 / RFC 6234 vectors. A hash that is wrong in one bit breaks
/// every approval silently, so the implementation is pinned to the standard
/// rather than to its own output.
#[test]
fn sha256_matches_the_standard_vectors() {
    let hex = |s: &str| {
        let mut h = Sha256::new();
        h.update(s.as_bytes());
        h.finish_hex()
    };
    assert_eq!(
        hex(""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        hex("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    // Multi-block input crosses the padding/compression boundary, where a
    // hand-rolled implementation is most likely to be wrong.
    let long = "a".repeat(1000);
    assert_eq!(
        hex(&long),
        "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
    );
}

#[test]
fn manifest_parses_both_orders_and_resolves_relative_paths() {
    let text = "\
# approvals\n\
\n\
ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  target/release/libspread_arb.dylib\n\
target/release/libother.dylib  *248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1\n\
not-a-hash target/release/ignored.dylib\n";
    let manifest = Path::new("/srv/strategies/approved.manifest");
    let entries = parse_manifest(text, manifest);
    assert_eq!(
        entries.len(),
        2,
        "comment, blank and hashless lines skipped"
    );
    assert_eq!(
        entries[0].path,
        PathBuf::from("/srv/strategies/target/release/libspread_arb.dylib")
    );
    assert_eq!(
        entries[0].sha256,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    // Reversed order + binary marker still resolves to a path, relative to
    // the manifest's own directory.
    assert_eq!(
        entries[1].path,
        PathBuf::from("/srv/strategies/target/release/libother.dylib")
    );
    assert_eq!(
        entries[1].sha256,
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

#[test]
fn machine_generated_paths_are_quarantined() {
    assert!(is_machine_generated(Path::new(
        "/repo/data/shadow_evolution/variant_7.dylib"
    )));
    assert!(is_machine_generated(Path::new(
        "/repo/user_layer/strategies/shadow_variant.dylib"
    )));
    assert!(is_machine_generated(Path::new("/repo/data/foo.dylib")));
    assert!(!is_machine_generated(Path::new(
        "/repo/user_layer/strategies/target/release/libalpha_strategy.dylib"
    )));
}

/// The acceptance shape of #188: a library under the shared temp directory
/// or under an artifact tree is refused, an approved root is not.
#[test]
fn policy_refuses_temp_and_artifacts_and_allows_the_strategy_tree() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf();
    let policy = TrustPolicy {
        approved_dirs: vec![
            repo.join(APPROVED_ROOT)
                .canonicalize()
                .unwrap_or_else(|_| repo.join(APPROVED_ROOT)),
        ],
        untrusted_dirs: untrusted_dirs(),
        manifest: None,
    };

    // Approved: the tree every strategy build lands in.
    let good = repo.join("user_layer/strategies/target/release/libalpha_strategy.dylib");
    assert!(
        policy.check(&good).is_ok(),
        "the strategy tree must stay loadable: {:?}",
        policy.check(&good)
    );

    // Refused: /tmp, whatever the file is.
    let tmp = std::env::temp_dir().join("evil.dylib");
    let err = policy.check(&tmp).expect_err("/tmp must be refused");
    assert!(
        err.contains("shared directory") || err.contains("never a strategy root"),
        "rejection must name the reason, got: {err}"
    );

    // Refused: a Shadow Evolution artifact, even inside the approved tree —
    // this is the human-approval gate.
    let artifact = repo.join("user_layer/strategies/shadow_evolution/variant_9.dylib");
    let err = policy
        .check(&artifact)
        .expect_err("an evolution artifact needs explicit approval");
    assert!(
        err.contains("machine-generated") && err.contains("approval manifest"),
        "got: {err}"
    );

    // A path outside every root, with no manifest, is refused too.
    let stray = Path::new("/opt/somewhere/libx.dylib");
    assert!(policy.check(stray).is_err());
}

/// #265: the tree the RUNNING BINARY sits in is an approved-root anchor.
///
/// The deployment shape is a release binary inside a checkout, started from
/// an unrelated cwd: `CARGO_MANIFEST_DIR` then names the BUILD machine's tree
/// (which need not exist here), `current_dir()` names something unrelated, and
/// every strategy was refused while the kernel booted anyway. The layouts below
/// are the ones this repository actually produces — the shipped binary and a
/// test binary (which sits one level deeper, in `deps/`), because an anchor
/// that only works at a fixed offset is the silent-drop this test exists for.
#[test]
fn the_running_binary_anchors_its_own_tree() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf();
    for layout in [
        "target/release/blitzkrieg-core",
        "target/debug/blitzkrieg-core",
        "target/debug/deps/blitzkrieg_core-0123456789abcdef",
    ] {
        assert_eq!(
            repo_root_of(&repo.join(layout)).as_deref(),
            Some(repo.as_path()),
            "{layout} must anchor {}",
            repo.display()
        );
    }
    // A copy installed outside every checkout anchors nothing on purpose: an
    // arbitrary install prefix is not a strategy root (the operator's escape
    // hatch for that is the allow-dirs variable, not a path guess).
    assert!(repo_root_of(Path::new("/usr/local/bin/blitzkrieg-core")).is_none());
    assert!(repo_root_of(Path::new("/tmp")).is_none());

    // And the anchor survives canonicalization in the real process: for a
    // binary inside this tree, `repo_roots()` must carry an approved root —
    // the property that was missing when it came back empty.
    let roots = repo_roots();
    assert!(
        !roots.is_empty(),
        "the running binary's tree must be a root"
    );
    assert!(
        roots.iter().any(|r| r.join(APPROVED_ROOT).is_dir()),
        "no candidate root carries an approved strategy tree: {roots:?}"
    );
}

/// Hash-pinned approval: listed + matching digest passes, a byte changed
/// afterwards does not. This is the acceptance for "哈希不匹配的库被拒绝".
///
/// The probe lives under `$HOME` rather than `$TMPDIR` on purpose: a shared
/// temp directory is refused *before* the manifest is consulted (see
/// `policy_refuses_temp_and_artifacts_and_allows_the_strategy_tree`), so it
/// could never demonstrate the manifest rule.
#[test]
fn manifest_approval_requires_a_matching_digest() {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return; // no HOME to write a probe into; the rule is still covered below
    };
    let dir = home.join(format!(".bk-manifest-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return; // unwritable HOME (locked-down CI): nothing to assert here
    }
    let lib = dir.join("approved_lib.dylib");
    std::fs::write(&lib, b"first build").expect("write lib");
    let manifest = dir.join("approved.manifest");
    let digest = sha256_file(&lib).expect("hash");
    std::fs::write(&manifest, format!("{digest}  approved_lib.dylib\n")).expect("write manifest");

    let policy = TrustPolicy {
        approved_dirs: vec![], // nothing is approved by location here
        untrusted_dirs: untrusted_dirs(),
        manifest: Some(manifest.clone()),
    };
    assert!(policy.check(&lib).is_ok(), "listed + matching digest loads");

    // The same path, different bytes: the approval does not carry over.
    std::fs::write(&lib, b"second build").expect("rewrite lib");
    let err = policy
        .check(&lib)
        .expect_err("a swapped library must lose its approval");
    assert!(err.contains("sha256 changed"), "got: {err}");

    // And an unlisted path stays refused.
    let other = dir.join("other.dylib");
    std::fs::write(&other, b"x").expect("write other");
    let err = policy.check(&other).expect_err("unlisted path is refused");
    assert!(err.contains("not listed"), "got: {err}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A world-writable library is refused wherever it lives: any local user
/// could replace it between approval and load.
#[test]
fn world_writable_libraries_are_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("bk-worldrw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let f = dir.join("libworld_writable.dylib");
    std::fs::write(&f, b"x").expect("write");
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o666)).expect("chmod");
    let policy = TrustPolicy {
        approved_dirs: vec![dir.canonicalize().unwrap()],
        untrusted_dirs: untrusted_dirs(),
        manifest: None,
    };
    let err = policy
        .check(&f)
        .expect_err("world-writable must be refused");
    assert!(err.contains("world-writable"), "got: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// #843 P1: the loader's OPTIONAL-symbol table and the shared capability
/// model's declaration map are the SAME surface. The map's C column was
/// pinned in strategy_api's own tests; here the LOADER proves it builds
/// against those exact constants (this file imports `BK_*_SYMBOL` from
/// strategy_api, so a map/loader drift compiles but must not pass).
#[cfg(feature = "strategy-loading")]
#[test]
fn capability_model_symbols_are_the_loader_symbols() {
    use blitzkrieg_strategy_api::{
        BK_ATOMIC_BATCH_SYMBOL, BK_BIND_EVAL_CTX_SYMBOL, BK_CONFIG_VIEW_SYMBOL,
        BK_DECLARE_MODES_SYMBOL, BK_ENTRY_PAIR_SYMBOL, BK_EVOLVABLE_KNOBS_SYMBOL,
        BK_GATE_EXEMPTIONS_SYMBOL, BK_ON_KLINE_SYMBOL, BK_SETTLEMENT_HOLDS_SYMBOL, DECLARATION_MAP,
        DeclCap,
    };
    let by_name = |cap: DeclCap| {
        DECLARATION_MAP
            .iter()
            .find(|r| r.cap == cap)
            .map(|r| r.c_abi_symbol)
            .expect("row present")
    };
    assert_eq!(by_name(DeclCap::Modes), Some(BK_DECLARE_MODES_SYMBOL));
    assert_eq!(
        by_name(DeclCap::GateExemptions),
        Some(BK_GATE_EXEMPTIONS_SYMBOL)
    );
    assert_eq!(
        by_name(DeclCap::EvolvableKnobs),
        Some(BK_EVOLVABLE_KNOBS_SYMBOL)
    );
    assert_eq!(by_name(DeclCap::BindEvalCtx), Some(BK_BIND_EVAL_CTX_SYMBOL));
    assert_eq!(by_name(DeclCap::ConfigView), Some(BK_CONFIG_VIEW_SYMBOL));
    assert_eq!(
        by_name(DeclCap::HoldsToSettlement),
        Some(BK_SETTLEMENT_HOLDS_SYMBOL)
    );
    assert_eq!(by_name(DeclCap::OnKline), Some(BK_ON_KLINE_SYMBOL));
    assert_eq!(by_name(DeclCap::EntryPair), Some(BK_ENTRY_PAIR_SYMBOL));
    assert_eq!(by_name(DeclCap::AtomicBatch), Some(BK_ATOMIC_BATCH_SYMBOL));
}

/// #843 P1: warnings between the shared verdict decoder and the loader's
/// probe path — how the encode/decode pair keeps out-of-range ABI ints
/// from widening an operator's policy (fail-closed across a reload too).
#[test]
fn trailing_verdict_decode_matches_the_loader_rule() {
    use blitzkrieg_strategy_api::TrailingExitVerdict;
    // The fixture's declared 1 → Trail; parity's absent symbol →
    // FollowKernel. Same table the manifest gate uses for the boolean.
    assert_eq!(
        TrailingExitVerdict::from_abi_int(1).to_option_bool(),
        Some(true)
    );
    assert_eq!(TrailingExitVerdict::from_abi_int(-1).to_option_bool(), None);
    assert_eq!(
        TrailingExitVerdict::from_abi_int(0).to_option_bool(),
        Some(false)
    );
    assert_eq!(
        TrailingExitVerdict::from_abi_int(7).to_option_bool(),
        None,
        "an out-of-range int is a malformed declaration, never a trail"
    );
}
