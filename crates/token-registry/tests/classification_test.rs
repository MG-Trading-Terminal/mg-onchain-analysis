//! Classification rule tests.
//!
//! One positive case per classification rule:
//!   Rule 1: burn_address
//!   Rule 2: cex_hot_wallet (binance, coinbase)
//!   Rule 3: dex_pool (raydium_amm_v4, orca_whirlpool, raydium_cpmm)
//!   Rule 4: vesting_contract (streamflow, jupiter_lock, tuktuk, jupiter_dtf)
//!   Rule 5: liquid (fallback — unknown owner)
//!
//! Also tests the priority ladder:
//!   burn_address beats cex lookup
//!   cex lookup beats owner lookup (cex check is before RPC)

// Tests live in the classify module directly (see classify.rs #[cfg(test)]).
// This integration test file re-exports them to confirm they run as part of
// the `cargo test -p mg-onchain-token-registry` suite.
//
// All classification tests are in src/classify.rs#[cfg(test)] — they use
// MockSolanaRpc and don't need a DB. Running `cargo test` will pick them up.

// Verify the classification kind strings match what the migration expects.
#[test]
fn classification_kind_strings_match_migration_values() {
    use mg_onchain_token_registry::classify::HolderKind;

    // These must match the `kind` CHECK values documented in V00003.
    let kinds = [
        HolderKind::BurnAddress,
        HolderKind::DexPool { subkind: "raydium_amm_v4".to_owned() },
        HolderKind::VestingContract { subkind: "streamflow".to_owned() },
        HolderKind::CexHotWallet { subkind: "binance".to_owned() },
        HolderKind::Liquid,
    ];
    let expected_strs = ["burn_address", "dex_pool", "vesting_contract", "cex_hot_wallet", "liquid"];

    for (kind, expected) in kinds.iter().zip(expected_strs.iter()) {
        assert_eq!(
            kind.kind_str(),
            *expected,
            "kind_str() must match migration table values"
        );
    }
}

// Verify confidence values are in [0.0, 1.0].
#[test]
fn all_classification_confidences_are_in_range() {
    use mg_onchain_token_registry::classify::HolderKind;
    let kinds = vec![
        HolderKind::BurnAddress,
        HolderKind::DexPool { subkind: "x".to_owned() },
        HolderKind::VestingContract { subkind: "x".to_owned() },
        HolderKind::CexHotWallet { subkind: "x".to_owned() },
        HolderKind::Liquid,
    ];
    for k in &kinds {
        let c = k.confidence();
        assert!(
            (0.0_f64..=1.0_f64).contains(&c),
            "{} confidence {} must be in [0,1]",
            k.kind_str(),
            c
        );
    }
}

// Verify TTL: burn_address and dex_pool have no expiry; others do.
#[test]
fn burn_address_and_dex_pool_have_no_ttl() {
    use mg_onchain_token_registry::classify::HolderKind;
    assert!(HolderKind::BurnAddress.ttl().is_none());
    assert!(HolderKind::DexPool { subkind: "x".to_owned() }.ttl().is_none());
    assert!(HolderKind::VestingContract { subkind: "x".to_owned() }.ttl().is_some());
    assert!(HolderKind::CexHotWallet { subkind: "x".to_owned() }.ttl().is_some());
    assert!(HolderKind::Liquid.ttl().is_some());
}
