//! LP lock state detection tests.
//!
//! Tests the `analyse_lp_lock` function across:
//! - 100% burned LP
//! - 0% burned, 0% locked
//! - Streamflow locker holds LP tokens
//! - Jupiter Lock holds LP tokens
//! - Non-existent LP mint (graceful no-op)
//! - Zero total supply
//! - Partial burn (50%)
//!
//! All tests use MockSolanaRpc — no network I/O.
// Tests live in src/locker.rs#[cfg(test)].
// This file provides additional cross-module locker tests.

use mg_onchain_common::chain::Chain;
use mg_onchain_token_registry::locker::analyse_lp_lock;
use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;
use mg_onchain_token_registry::rpc::{DecodedMint, TokenAccountBalance};
use mg_onchain_token_registry::programs::{BURN_ADDRESS, JUPITER_LOCK};
use rust_decimal::Decimal;

fn mint(supply: u64) -> DecodedMint {
    DecodedMint { supply: supply as u128, decimals: 9, mint_authority: None, freeze_authority: None, is_token2022: false, raw_account_data: vec![0u8; 82] }
}

fn account(addr: &str, amount: u64) -> TokenAccountBalance {
    TokenAccountBalance { address: addr.to_owned(), amount: amount.to_string(), ui_amount_string: None, decimals: 9 }
}

#[tokio::test]
async fn lp_fully_burned_integration() {
    let rpc = MockSolanaRpc {
        mint_account: Some(Ok(Some(mint(1_000_000)))),
        largest_accounts: Some(Ok(vec![account(BURN_ADDRESS, 1_000_000)])),
        ..Default::default()
    };
    let state = analyse_lp_lock(&rpc, "LpMint11111111111111111111111111111111111111", Chain::Solana).await;
    assert_eq!(state.lp_burned_pct, Decimal::from(100));
    assert!(state.lockers.is_empty());
}

#[tokio::test]
async fn lp_locked_in_jupiter_lock() {
    let escrow = "JupLockEscrow11111111111111111111111111111111";
    let rpc = MockSolanaRpc {
        mint_account: Some(Ok(Some(mint(1_000_000)))),
        largest_accounts: Some(Ok(vec![account(escrow, 1_000_000)])),
        token_account_owner: Some(Ok(Some(JUPITER_LOCK.to_owned()))),
        ..Default::default()
    };
    let state = analyse_lp_lock(&rpc, "LpMint11111111111111111111111111111111111111", Chain::Solana).await;
    assert_eq!(state.lp_burned_pct, Decimal::ZERO);
    assert_eq!(state.lockers.len(), 1);
    let locker_name = state.lockers[0].locker_name.as_deref().unwrap_or("");
    assert!(locker_name.contains("Jupiter"), "expected Jupiter locker name, got: {locker_name}");
    assert!(state.lockers[0].unlock_at.is_none(), "MVP: unlock_at always None");
}

#[tokio::test]
async fn lp_not_locked_not_burned_returns_empty() {
    let rpc = MockSolanaRpc {
        mint_account: Some(Ok(Some(mint(500_000)))),
        largest_accounts: Some(Ok(vec![account("SomeWallet11111111111111111111111111111111", 500_000)])),
        token_account_owner: Some(Ok(Some("SomeRandomProgram1111111111111111111111111111".to_owned()))),
        ..Default::default()
    };
    let state = analyse_lp_lock(&rpc, "LpMint11111111111111111111111111111111111111", Chain::Solana).await;
    assert_eq!(state.lp_burned_pct, Decimal::ZERO);
    assert!(state.lockers.is_empty());
}
