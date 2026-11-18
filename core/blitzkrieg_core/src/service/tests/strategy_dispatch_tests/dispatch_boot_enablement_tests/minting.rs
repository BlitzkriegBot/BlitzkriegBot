//! `account.create` on a locally-settling core seeds the runtime mint with
//! the boot seed, and the funding row is durable across restart; LIVE mints
//! stay at zero. Moved verbatim from `dispatch_boot_enablement_tests.rs`
//! (pure code motion).

use super::super::*;

/// 生产 16:29 形状的端到端牙（多账户 PR A 缺陷）：
/// `account.create` on a locally-settling core seeds the minted account
/// with `dry_seed_balance` — the same waking balance a boot account gets.
/// The gap this closes (生产 probe2 实锤): a runtime mint used to start at
/// zero, strategy sizing read `equity 0 below min_equity_usd` and the
/// scope never traded — and no boot repaired it, because the boot seed is
/// skipped wholesale once ANY account has a journal (an established
/// deployment always does). LIVE mints stay at zero: the venue's real
/// cash is the balance there.
///
/// The second half is the DURABILITY of that funding (生产 probe2 二次
/// 实锤: balance 1000 → restart → 0): the minted ledger gets its journal
/// path attached at mint time, so the funding row (and every later cash
/// event) lands in `data/ledger/<account>.journal.jsonl` and the next
/// boot's `attach_journals` replay restores it.
#[test]
fn account_create_seeds_a_runtime_mint_on_a_locally_settling_core() {
    let dir = std::env::temp_dir().join(format!("bk-mint-seed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    let journal_dir = dir.join("ledger");
    std::fs::create_dir_all(&journal_dir).expect("journal dir");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default\nACCOUNT_DEFAULT_NAME=\"main\"\n",
    )
    .expect("fixture");
    // Dry settles locally: the mint gets the seed.
    let mut c = Core::with_account_book(
        CoreConfig {
            mode: Mode::Dry,
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            ledger_journal_dir: Some(journal_dir.to_string_lossy().into_owned()),
            // 持久化后端是 SQLite（`.env` 只读）：account.create 的 mint 落 store。
            db_path: Some(dir.join("store.sqlite")),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path,
            Some(&env_path),
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    c.open_account_risk_db(dir.join("store.sqlite"))
        .expect("settings store opens");
    let probe = crate::account::AccountId::from("probe2");
    c.create_account_persisting(
        &probe,
        Some("probe two"),
        Some("polymarket"),
        crate::account::CredentialKeys::default(),
        ("test", "mint"),
        5,
    )
    .expect("the mint lands");
    assert_eq!(
        c.accounts
            .ledger_of(&probe)
            .expect("minted account addressable")
            .balance(),
        c.config.dry_seed_balance,
        "the mint wakes with the configured seed, not zero"
    );

    // The funding row is DURABLE (issue #941: into the DRY side's file —
    // a dry mint never writes the live book): the mint attached the journal
    // path, so the row landed on disk and a fresh boot's `attach_journals`
    // replay restores the balance — the restart does not vaporize it.
    let journal_path = journal_dir.join(crate::account::AccountLedgers::journal_file_name(
        false, "probe2",
    ));
    assert!(
        !journal_dir
            .join(crate::account::AccountLedgers::journal_file_name(
                true, "probe2"
            ))
            .exists(),
        "a dry mint never creates the live side's file"
    );
    let rows: Vec<crate::ledger::JournalEntry> = crate::jsonl::load(&journal_path, "test journal");
    let seed_row = rows
        .iter()
        .find(|r| r.ref_ == "account.create")
        .expect("the funding row is ON DISK");
    assert_eq!(seed_row.amount, c.config.dry_seed_balance);

    // LIVE: the venue owns the cash — a mint stays at zero (and no journal
    // path is needed to remember a venue truth, but attaching one is
    // harmless; the balance stays 0 and no row is written).
    let accounts_path_live = dir.join("accounts-live.toml"); // the LEGACY path — never written
    let env_path_live = dir.join("live.env");
    let journal_dir_live = dir.join("ledger-live");
    std::fs::create_dir_all(&journal_dir_live).expect("live journal dir");
    std::fs::write(
        &env_path_live,
        "ACCOUNT_LIST=default\nACCOUNT_DEFAULT_NAME=\"main\"\n",
    )
    .expect("fixture");
    let mut live = Core::with_account_book(
        CoreConfig {
            mode: Mode::Live,
            accounts_config_path: Some(accounts_path_live.to_string_lossy().into_owned()),
            ledger_journal_dir: Some(journal_dir_live.to_string_lossy().into_owned()),
            // 持久化后端是 SQLite（`.env` 只读）：LIVE mint 的账户行落独立 store。
            db_path: Some(dir.join("store-live.sqlite")),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path_live,
            Some(&env_path_live),
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    let probe_live = crate::account::AccountId::from("probe3");
    live.create_account_persisting(
        &probe_live,
        None,
        None,
        crate::account::CredentialKeys::default(),
        ("test", "mint live"),
        6,
    )
    .expect("the live mint lands");
    assert_eq!(
        live.accounts
            .ledger_of(&probe_live)
            .expect("minted live account addressable")
            .balance(),
        rust_decimal::Decimal::ZERO,
        "LIVE mints stay at zero — venue cash is the balance"
    );
    assert!(
        !journal_dir_live
            .join(crate::account::AccountLedgers::journal_file_name(
                true, "probe3"
            ))
            .exists()
            && !journal_dir_live
                .join(crate::account::AccountLedgers::journal_file_name(
                    false, "probe3"
                ))
                .exists(),
        "a zero funding records no journal row, so no file is created"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
