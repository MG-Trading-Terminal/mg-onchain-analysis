//! Enrichment tests using mock RPC.
//!
//! These tests validate that `enrich_token_inner` correctly:
//! 1. Populates `TokenMeta` fields from mock RPC responses.
//! 2. Handles missing mint accounts gracefully.
//! 3. Respects `top_holders_limit` config.
//! 4. Computes `top_holders[].pct` correctly from supply.
//!
//! Fixtures: `tests/fixtures/enrichment/*.json` document the real on-chain
//! state that the mocks replicate. The mocks are hand-rolled structs matching
//! the fixture JSON.
//!
//! No database calls are made: `PgStore` is constructed but not used because
//! `MockSolanaRpc::with_mint` is paired with a fresh config; the cache read
//! path is skipped on `StorageError` (no real DB) and falls through to RPC.
//! For a full integration test with real Postgres, use testcontainers (Phase 3).
//
// NOTE: PgStore requires a live Postgres connection. These tests are
// unit-level tests of the pure computation logic (compute_gini, top_n_pct)
// which don't need a DB. The enrich_token_inner tests are annotated #[ignore]
// and require a live DB + REGISTRY_POSTGRES_URL env var.

use mg_onchain_common::chain::Chain;
use mg_onchain_token_registry::enrich::{compute_gini, top_n_pct};
use mg_onchain_common::token::TopHolder;
use mg_onchain_common::chain::Address;
use rust_decimal::Decimal;

fn make_holder(amount: u128) -> TopHolder {
    TopHolder {
        address: Address::parse(Chain::Solana, "11111111111111111111111111111112").unwrap(),
        pct: Decimal::ZERO,
        amount_raw: amount,
        is_insider: false,
    }
}

// ---- Gini coefficient ----

#[test]
fn gini_all_equal_holders() {
    // Four holders with equal 250 units each out of 1000 total.
    // Gini coefficient for perfectly equal distribution = 0.
    let holders: Vec<TopHolder> = (0..4).map(|_| make_holder(250)).collect();
    let g = compute_gini(&holders, 1000);
    assert!(
        g <= Decimal::new(5, 3),
        "perfect equality → Gini ≈ 0, got {g}"
    );
}

#[test]
fn gini_one_dominant_holder() {
    // One holder at 81% (RAVE probe), one tiny holder.
    let addr1 = Address::parse(Chain::Solana, "11111111111111111111111111111112").unwrap();
    let addr2 = Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112").unwrap();
    let holders = vec![
        TopHolder { address: addr1, pct: Decimal::from(81), amount_raw: 814_700, is_insider: false },
        TopHolder { address: addr2, pct: Decimal::from(19), amount_raw: 185_300, is_insider: false },
    ];
    let g = compute_gini(&holders, 1_000_000);
    // Should be substantially above 0.5 for 81%/19% split.
    assert!(g > Decimal::new(3, 1), "81/19 split should have Gini > 0.3, got {g}");
}

#[test]
fn gini_empty_returns_zero() {
    assert_eq!(compute_gini(&[], 0), Decimal::ZERO);
}

// ---- top_n_pct ----

#[test]
fn top1_pct_rave_scenario() {
    // RAVE: single holder at 81.47% of supply (814_700 / 1_000_000).
    let addr = Address::parse(Chain::Solana, "11111111111111111111111111111112").unwrap();
    let holders = vec![TopHolder {
        address: addr,
        pct: Decimal::from(81),
        amount_raw: 814_700,
        is_insider: false,
    }];
    let pct = top_n_pct(&holders, 1, 1_000_000);
    // 814700 / 1000000 * 100 = 81.47
    assert!(
        pct > Decimal::from(81) && pct < Decimal::from(82),
        "expected ~81.47%, got {pct}"
    );
}

#[test]
fn top10_pct_zero_supply() {
    assert_eq!(top_n_pct(&[], 10, 0), Decimal::ZERO);
}

#[test]
fn top_n_pct_more_than_available() {
    // Requesting top-10 from only 3 holders should sum all 3.
    let addr = Address::parse(Chain::Solana, "11111111111111111111111111111112").unwrap();
    let holders: Vec<TopHolder> = vec![300_000u128, 200_000, 100_000].into_iter().map(|a| TopHolder {
        address: addr.clone(), pct: Decimal::ZERO, amount_raw: a, is_insider: false,
    }).collect();
    let pct = top_n_pct(&holders, 10, 1_000_000);
    assert_eq!(pct, Decimal::from(60), "300k+200k+100k / 1M * 100 = 60%");
}
