//! #180 — the StoreApi seam's two teeth, pinned as behaviour.
//!
//! The audit finding: `StoreApi` advertised 21 writes and enforced none of
//! them — each shipped a default body returning
//! `Err("… no backend implements this yet")`, so "this backend does not
//! carry this domain" was a state the COMPILER accepted and only the
//! operator's first write discovered. This file pins the two halves of the
//! fix so they cannot silently regress:
//!
//!  * `the_trait_declares_every_write_without_a_default_body` — the trait's
//!    method inventory, read from the source, with every write face a bare
//!    declaration. A re-added default body would compile clean and pass
//!    every other test in this repo; only a source assertion catches it.
//!  * `bare_sql_writers_do_not_grow` — the call-site ratchet. The audit
//!    enumerated 8 production direct-SQL writers; #180 converged exactly one
//!    (the `"defaults"` literal in `execution_policy`, see below) and the
//!    remaining 7 are recorded here with the reason each one needs a T0
//!    contract decision rather than a mechanical swap. The assertion is an
//!    EXACT set, so new debt reds and a retired site also reds until the
//!    expectation is narrowed — the ratchet only shrinks.
//!
//! Why a source scan and not a behavioural test for the first two: a default
//! method body is invisible at runtime (the real backend overrides it), so no
//! in-process test can tell the difference between "every backend implements
//! this" and "the trait quietly supplies a refusal". Reading the declaration
//! is the only honest way to pin it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The kernel's own `src/` tree (this test lives in `tests/`).
fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` file under `src/`, minus the two carve-outs the issue names:
/// the store implementations themselves (they are the carriers) and test
/// modules (a `#[cfg(test)]` fixture is not a runtime write path).
fn production_sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                // `store/` = the carrier; `tests/` = fixtures, not writers.
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == "store" || name == "tests" {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                // `tests.rs` / `*_tests.rs` are the inline suites of this tree.
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == "tests.rs" || name.ends_with("_tests.rs") {
                    continue;
                }
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&src_root(), &mut out);
    out.sort();
    out
}

/// Strip the comment tail so a doc line that *names* a writer (this file's
/// own explanations do) is never read as a call site.
fn code_part(line: &str) -> &str {
    match line.find("//") {
        Some(idx) => &line[..idx],
        None => line,
    }
}

#[test]
fn the_trait_declares_every_write_without_a_default_body() {
    let path = src_root().join("store").join("mod.rs");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let start = text
        .find("pub trait StoreApi:")
        .expect("the StoreApi trait is declared in store/mod.rs");
    let body_end = text[start..]
        .find("\n}\n")
        .expect("the trait body is brace-closed before the contract types");
    let body = &text[start..start + body_end];

    // The method inventory. `account_writes_enabled` is the ONE capability
    // probe and keeps its honest `false` default; the 21 write faces plus the
    // two `load_*` read-backs must all be declarations only.
    let declared: BTreeSet<&str> = body
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let rest = line
                .strip_prefix("fn ")
                .or_else(|| line.strip_prefix("pub fn "))?;
            Some(rest.split('(').next().unwrap_or("").trim())
        })
        .collect();
    let expected: BTreeSet<&str> = [
        "account_writes_enabled",
        "save_account_venue",
        "save_account_create",
        "remove_account",
        "save_account_trading",
        "save_account_status",
        "save_account_active",
        "save_display_account",
        "save_account_strategies",
        "save_account_rounds",
        "save_account_opening_equity",
        "save_positions",
        "save_risk_bounds",
        "save_exit",
        "save_arb_config",
        "clear_arb_config",
        "load_arb_config",
        "save_policy_section",
        "save_account_round_duration",
        "save_credentials",
        "save_market_plugin",
        "load_market_plugin",
        // #192 (operator ruling 2026-11-25) DELETED `append_money_fact`:
        // the `money_facts` table was write-only, so it was not a ledger.
        // #180's core claim still holds for every face above — each is
        // declaration-only, so a backend that forgets one fails to compile.
        "put_setting",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        declared, expected,
        "the StoreApi method inventory changed — update this expectation \
         deliberately (a silently added face is a domain the backends may \
         not answer for)"
    );

    // No write face may carry a body. A `{` after the signature is how a
    // default body starts; `;` is the declaration-only spelling.
    for line in body.lines() {
        let trimmed = line.trim();
        if !(trimmed.starts_with("fn ") || trimmed.starts_with("pub fn ")) {
            continue;
        }
        let name = trimmed
            .strip_prefix("fn ")
            .or_else(|| trimmed.strip_prefix("pub fn "))
            .and_then(|rest| rest.split('(').next())
            .unwrap_or("");
        if name == "account_writes_enabled" {
            // The capability probe: its `false` default IS its answer.
            continue;
        }
        // A signature may wrap across lines; the terminator line carries the
        // `;` or `{`. Read forward to the first of either.
        let idx = body
            .lines()
            .position(|l| l.trim() == trimmed)
            .expect("the line was just read from this body");
        let mut terminator = None;
        for line in body.lines().skip(idx) {
            let code = code_part(line);
            if let Some(pos) = code.find(';').or_else(|| code.find('{')) {
                terminator = Some(code.as_bytes()[pos] as char);
                break;
            }
        }
        assert_eq!(
            terminator,
            Some(';'),
            "`{name}` must be a bare declaration: a default body makes \
             \"this backend does not carry this domain\" compile-legal, \
             which is the #180 finding"
        );
    }

    // And the specific string the audit counted is gone from the trait.
    assert!(
        !body.contains("no backend implements this yet"),
        "the default `Err(\"… no backend implements this yet\")` bodies are \
         the defect #180 removed — none may come back"
    );
}

#[test]
fn bare_sql_writers_do_not_grow() {
    // The audit's 8 production direct-SQL writers, minus the one #180
    // converged. Each survivor is HERE because a mechanical swap would be
    // wrong or unsafe, not because it was skipped:
    //
    //   assembly_boot.rs `save_account_row`      — no StoreApi face exists
    //       for a whole account row (T0: `save_account_book`); boot path.
    //   assembly_boot.rs `save_credential_names` — no face exists for the
    //       var-NAME half of credentials (T0: `save_credential_keys`).
    //   assembly_boot.rs `save_setting`          — the first-boot env-book
    //       materialization; `save_account_strategies` writes
    //       `strategy_assignments`, which NOTHING in production reads, so
    //       swapping would silently break the boot read-back.
    //   trading_round_duration.rs `save_setting` — `save_account_round_duration`
    //       exists and writes the same row; migrating is a behaviour change
    //       on the seconds-cut/flatten path, out of scope for a seam commit.
    //   strategies/enable.rs `save_setting`      — same `strategies` row as
    //       the boot read-back; see the `strategy_assignments` note above.
    //   accounts/persist.rs `save_setting`       — same, the
    //       `account.strategies.set` face.
    //   trading_reset.rs `save_setting`          — same `strategies` row as
    //       the boot read-back (Gitea #285 Option B: reset_trading revokes
    //       persisted strategy authorization so restart stays disarmed).
    //   shadow_evolution/mod.rs `save_kernel_meta` — process-scoped switch,
    //       no StoreApi face (T0: kernel-meta domain).
    let expected: BTreeSet<(&str, &str)> = [
        ("service/assembly_boot.rs", "save_account_row"),
        ("service/assembly_boot.rs", "save_credential_names"),
        ("service/assembly_boot.rs", "save_setting"),
        ("service/trading_reset.rs", "save_setting"),
        ("service/trading_round_duration.rs", "save_setting"),
        ("service/strategies/enable.rs", "save_setting"),
        ("service/accounts/persist.rs", "save_setting"),
        ("shadow_evolution/mod.rs", "save_kernel_meta"),
    ]
    .into_iter()
    .collect();

    let symbol = |call: &str| -> &'static str {
        match call {
            "save_setting" => "save_setting",
            "save_account_row" => "save_account_row",
            "save_credential_names" => "save_credential_names",
            "save_kernel_meta" => "save_kernel_meta",
            other => panic!("unknown writer symbol {other}"),
        }
    };
    let mut found: BTreeSet<(String, &'static str)> = BTreeSet::new();
    for path in production_sources() {
        let rel = path
            .strip_prefix(src_root())
            .expect("walked from src")
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for line in text.lines() {
            let code = code_part(line);
            for call in [
                "save_setting",
                "save_account_row",
                "save_credential_names",
                "save_kernel_meta",
            ] {
                // `.name(` = a call through a handle; `fn name(` = the
                // writer's own definition (the definition is a carrier, not
                // a call site, and lives behind the store carve-out).
                if code.contains(&format!(".{call}(")) {
                    found.insert((rel.clone(), symbol(call)));
                }
            }
        }
    }

    let found_pairs: BTreeSet<(&str, &str)> = found.iter().map(|(f, s)| (f.as_str(), *s)).collect();
    assert_eq!(
        found_pairs, expected,
        "\nthe production bare-SQL writer set changed.\n\
         A NEW entry = new store-layering debt: route it through StoreApi or \
         add it here with its T0 reason.\n\
         A MISSING entry = the ratchet shrank: narrow `expected` in this test \
         so the win is locked in.\n\
         found: {found_pairs:#?}"
    );

    // The one site #180 DID converge: the #1253 self-heal must write through
    // the typed seam. `"defaults"` was a bare string literal standing in for
    // an account id — exactly the #1221/#1223 bug class `AccountScope` exists
    // to make unspellable.
    let policy_path = src_root().join("execution_policy").join("mod.rs");
    let policy = std::fs::read_to_string(&policy_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", policy_path.display()));
    assert!(
        !policy
            .lines()
            .any(|l| code_part(l).contains(".save_setting(")),
        "execution_policy must write the #1253 clamp through put_setting, not \
         AccountRiskDb::save_setting"
    );
    assert!(
        policy.contains(".put_setting("),
        "execution_policy must write the #1253 clamp through put_setting"
    );
    assert!(
        policy.contains("AccountScope::new("),
        "put_setting takes a typed AccountScope — the self-heal must name the \
         reserved defaults selector through it, not a bare string literal"
    );
    assert!(
        !policy
            .lines()
            .any(|l| code_part(l).contains("save_setting(\"defaults\"")),
        "the literal `\"defaults\"` as an account id is the #1221/#1223 bug \
         class — it must not come back (comments naming the old spelling are \
         fine; a CALL carrying it is not)"
    );
}

/// The #1253 self-heal's write-back, end to end: a poisoned `defaults` cap is
/// clamped in memory AND persisted through the store seam onto the reserved
/// `defaults` row (the FK scaffold `ledger_book_from_rows` refuses to
/// materialize as an account).
///
/// RESIDUAL, reported not fixed: the persisted KEY is `max_budget_usd`
/// (lowercase), while the loader reads `POLICY_MAX_BUDGET_USD`. So the healed
/// VALUE is durable but the next boot still re-reads the poisoned cell and
/// clamps again. Renaming the key would make a healed cap actually apply —
/// that is a risk-threshold semantics change (R6), so it stays out of this
/// seam commit and is reported for a decision.
#[test]
fn the_defaults_self_heal_lands_on_the_reserved_row_through_the_seam() {
    use blitzkrieg_core::execution_policy::{MAX_BUDGET_CEILING_USD, Policy};
    use blitzkrieg_core::store::{DEFAULTS_ACCOUNT_ID, SqliteStore};

    let dir = std::env::temp_dir().join(format!(
        "bk-store-seam-selfheal-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let store = SqliteStore::open(dir.join("store.sqlite")).expect("store opens");

    // The production poison: a "cap" at the magnitude that disables itself.
    let poisoned = "10222221221331331".to_string();
    let mut keys = std::collections::BTreeMap::new();
    keys.insert("POLICY_MAX_BUDGET_USD".to_string(), poisoned.clone());
    let mut settings = std::collections::BTreeMap::new();
    settings.insert(DEFAULTS_ACCOUNT_ID.to_string(), keys);

    let mut policy = Policy::default();
    policy
        .overlay_sql_settings(&settings, Some(&store))
        .expect("a poisoned default row must not brick the boot");

    assert_eq!(
        policy.defaults.max_budget_usd, MAX_BUDGET_CEILING_USD,
        "the in-memory cap is clamped to the sanity ceiling"
    );

    let rows = store.risk_db().load_settings().expect("settings read back");
    let row = rows
        .get(DEFAULTS_ACCOUNT_ID)
        .expect("the self-heal wrote onto the reserved defaults row");
    assert_eq!(
        row.get("max_budget_usd").map(String::as_str),
        Some(MAX_BUDGET_CEILING_USD.to_string().as_str()),
        "the clamp landed durably through put_setting: {row:?}"
    );
    assert_ne!(
        row.get("max_budget_usd").map(String::as_str),
        Some(poisoned.as_str()),
        "the poisoned value must not survive the heal"
    );

    // The reserved-row marker (`ensure_account` stamps it so no reader
    // mistakes this scaffold for an unbound account) is pinned by
    // `store::sqlite::tests::defaults_row_is_marked_reserved_not_unbound` —
    // the readback here has no status_reason face, and adding one to the
    // public API for a test would be scope this commit does not own.

    let _ = std::fs::remove_dir_all(&dir);
}

/// `RecordingStore`'s contract, local to the seam: it records the two faces
/// the money-write tests route through it, and it REFUSES — by name — every
/// domain it does not model. Before #180 the refusals came from the trait's
/// anonymous default bodies; a recording double that silently accepted an
/// unmodelled write would let a test "pass" while the routing it claims to
/// prove never happened.
#[cfg(feature = "test-support")]
#[test]
fn the_recording_store_records_its_own_faces_and_names_what_it_does_not() {
    use blitzkrieg_core::account::AccountId;
    use blitzkrieg_core::store::test_support::RecordingStore;
    use blitzkrieg_core::store::{AccountScope, WriteOutcome};

    let (store, writes) = RecordingStore::new();
    let alice = AccountId::from("alice");

    assert_eq!(
        store
            .put_setting(AccountScope::new(&alice), "K", Some("v"))
            .expect("put_setting is a modelled face"),
        WriteOutcome::Persisted
    );
    assert!(
        store
            .save_account_strategies("alice", &["s".to_string()])
            .is_err(),
        "an unmodelled domain must refuse rather than record"
    );
    let log = writes.lock().unwrap().clone();
    assert_eq!(log, vec!["setting:alice=K=Some(\"v\")".to_string()]);
}
