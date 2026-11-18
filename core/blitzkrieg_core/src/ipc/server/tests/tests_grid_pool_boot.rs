#![cfg(test)]

//! #1272 (网格池化): the BOOT mount book carries every persisted per-account
//! round-duration override.
//!
//! The override's durable home is the account settings row (`round_duration`
//! setting → `CoreConfig::account_round_durations`). Before this seeding the
//! boot's mount book was built from the process default family plus each
//! account's `rounds` declarations only, so a restarted core ran no discovery
//! instance for an overridden family — the family was admitted by the scanner
//! and polled by nobody. The venue reading is `active_families`'s: a bound
//! account speaks for ITS venue, an unbound one for every venue this boot
//! enabled.

use super::*;

fn stub(name: &str) -> std::sync::Arc<dyn blitzkrieg_market_api::MarketPlugin> {
    std::sync::Arc::new(VenueStubPlugin {
        name: name.to_string(),
    })
}

fn account(id: &str, venue: &str) -> crate::account::Account {
    crate::account::Account {
        id: crate::account::AccountId::from(id),
        name: id.into(),
        market_type: blitzkrieg_market_api::MarketType::Prediction,
        status: crate::account::AccountStatus::Active,
        credential_keys: crate::account::CredentialKeys::default(),
        venue: venue.into(),
        trading_enabled: true,
        strategies: Vec::new(),
        rounds: Vec::new(),
        opening_equity: None,
        updated_at_ms: 0,
    }
}

#[test]
fn the_boot_mount_book_carries_every_persisted_per_account_family() {
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let actives = vec![stub("poly"), stub("kalshi")];
    let accounts = vec![account("default", ""), account("sib", "kalshi")];
    let mut cfg = CoreConfig {
        engine_enabled: true,
        discovery_enabled: true,
        round_duration_sec: 900,
        default_round_duration_sec: 900,
        ..Default::default()
    };
    // An UNBOUND account's override speaks for every boot-active venue.
    cfg.account_round_durations.insert("default".into(), 300);
    // A BOUND account's override speaks for its own venue only.
    cfg.account_round_durations.insert("sib".into(), 3600);
    // An override whose account left the book speaks for nothing.
    cfg.account_round_durations.insert("gone".into(), 60);

    let data_plane = DataPlaneRuntime::at_boot(&cfg, &actives, &accounts, &registry);
    let book = data_plane
        .discovery_mounted
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();

    for pair in [
        ("poly", 900),
        ("kalshi", 900),
        ("poly", 300),
        ("kalshi", 300),
        ("kalshi", 3600),
    ] {
        assert!(
            book.contains(&(pair.0.to_string(), pair.1)),
            "the boot book must carry {pair:?}: {book:?}"
        );
    }
    assert!(
        !book.contains(&("poly".to_string(), 3600)),
        "a bound account's override must not leak onto a sibling venue: {book:?}"
    );
    assert!(
        !book.iter().any(|(_, d)| *d == 60),
        "an override for an account no longer in the book mounts nothing: {book:?}"
    );
}
