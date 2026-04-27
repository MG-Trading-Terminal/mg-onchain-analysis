//! LP-lock state detection for `MarketInfo.lockers[]`.
//!
//! For each pool/market associated with a token, this module:
//! 1. Computes `lp_burned_pct` — the fraction of LP tokens sent to the burn address.
//! 2. Checks known locker programs for locked LP positions.
//! 3. Populates `LockerInfo` records consumed by the rug-pull detector (D2).
//!
//! # Why this matters (RAVE probe §Gap 1)
//!
//! The D2 detector fires on active drain events (LP burn > threshold in window).
//! The RAVE probe identified this as a *trailing* indicator — the token can be
//! structurally ready to rug without any event having fired yet. The LP lock
//! state is the *leading* indicator: if `lp_burned_pct < 0.80` AND
//! `total_locked_pct < 0.50` AND `lp_provider_count <= 2`, the rug pre-condition
//! is fully met. That check is done by detector D2; this module just populates
//! the `lockers[]` field that D2 reads.
//!
//! # LP burn detection
//!
//! An LP token is "burned" when the LP mint's total supply decreases OR when
//! LP tokens are transferred to the Solana null key (11111...1111). We detect
//! the second form here: look up the LP mint's largest accounts and check if
//! the burn address holds LP tokens.
//!
//! The fraction burned = `burn_address_lp_balance / lp_total_supply`.
//!
//! # Locker program check
//!
//! We check whether any of the LP mint's largest token accounts are owned by
//! known locker programs (Streamflow, Jupiter Lock). If so, the locked amount
//! and a mock `unlock_at` (None for MVP — Phase 3 reads the on-chain lock state)
//! are returned as `LockerInfo` entries.
//!
//! Note: `unlock_at = None` is conservative — it means "we found a lock but
//! couldn't determine when it expires". Detectors should treat `unlock_at = None`
//! as "potentially still locked" (lower rug risk) rather than "expired".
//!
//! # Re-seed invariant (DG-D02-4)
//!
//! TODO(sprint-4): DG-D02-4 re-seed scenario. If `lp_burned_pct` comes back at 100%
//! from a cached snapshot but the pool's on-chain `liquidity_usd` subsequently
//! recovers above `min_pool_usd` (e.g. new LP injected after a previous rug), the
//! D02 detector will suppress Signal B (dead-pool guard) even though the token is
//! now live again. Mitigation: when `is_pool_dead()` returns true but `liquidity_usd`
//! is above `min_pool_usd`, force a fresh `fetch_lp_lock_state` call to re-compute
//! `lp_burned_pct` from chain rather than relying on the cached registry value.
//!
//! Sources:
//!   - Streamflow lock detection: https://docs.streamflow.finance
//!   - LROO (Shoaei et al. 2026): LP lock state is the #1 predictor of rug pull

use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use tracing::{debug, instrument};

use mg_onchain_common::chain::{Address, Chain};
use mg_onchain_common::token::LockerInfo;

use crate::programs::{is_burn_address, classify_vesting_owner};
use crate::rpc::SolanaRpc;

/// Result of LP lock state analysis for a single LP mint.
#[derive(Debug, Clone)]
pub struct LpLockState {
    /// Percentage of LP supply that has been permanently burned (0.0–100.0).
    pub lp_burned_pct: Decimal,

    /// Locker entries found (zero or more).
    pub lockers: Vec<LockerInfo>,

    /// Total LP token supply in raw units.
    pub lp_total_supply: u128,
}

/// Analyse the lock state of an LP mint.
///
/// `lp_mint` is the Base58 address of the LP token mint.
/// `token_decimals` is the LP token's decimal count (used for amount formatting).
///
/// Returns the LP lock state — `lp_burned_pct` and any `LockerInfo` records.
/// If the RPC call fails or the mint doesn't exist, returns an empty state
/// (0% burned, no lockers) rather than propagating the error. The enrichment
/// pipeline logs warnings and continues with partial data.
#[instrument(skip(rpc), fields(lp_mint))]
pub async fn analyse_lp_lock(
    rpc: &dyn SolanaRpc,
    lp_mint: &str,
    chain: Chain,
) -> LpLockState {
    // Fetch the LP mint account to get total supply.
    let total_supply = match rpc.get_mint_account(lp_mint).await {
        Ok(Some(decoded)) => decoded.supply,
        Ok(None) => {
            debug!(lp_mint, "LP mint account not found — treating as no lock");
            return LpLockState {
                lp_burned_pct: Decimal::ZERO,
                lockers: vec![],
                lp_total_supply: 0,
            };
        }
        Err(e) => {
            tracing::warn!(lp_mint, error = %e, "LP mint account fetch failed");
            return LpLockState {
                lp_burned_pct: Decimal::ZERO,
                lockers: vec![],
                lp_total_supply: 0,
            };
        }
    };

    if total_supply == 0 {
        return LpLockState {
            lp_burned_pct: Decimal::ZERO,
            lockers: vec![],
            lp_total_supply: 0,
        };
    }

    // Fetch top 20 LP token accounts. We inspect each for burn/locker status.
    let accounts = match rpc
        .get_token_largest_accounts(lp_mint, "confirmed")
        .await
    {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(lp_mint, error = %e, "getTokenLargestAccounts failed for LP mint");
            return LpLockState {
                lp_burned_pct: Decimal::ZERO,
                lockers: vec![],
                lp_total_supply: total_supply,
            };
        }
    };

    let mut burned_raw: u128 = 0;
    let mut lockers: Vec<LockerInfo> = Vec::new();

    for account in &accounts {
        let amount: u128 = account.amount.parse().unwrap_or(0);
        if amount == 0 {
            continue;
        }

        // Check if this token account is the burn address.
        if is_burn_address(&account.address) {
            burned_raw = burned_raw.saturating_add(amount);
            debug!(lp_mint, amount, "LP tokens in burn address");
            continue;
        }

        // Fetch owner to check for locker programs.
        if let Ok(Some(owner)) = rpc.get_token_account_owner(&account.address).await
            && let Some(subkind) = classify_vesting_owner(&owner)
        {
            // Build the locker address as the owner program address.
            // NOTE: `unlock_at = None` for MVP — reading the on-chain
            // lock expiry requires deserialising the locker's account
            // data (program-specific layout). Phase 3 work.
            if let Ok(locker_addr) = Address::parse(chain, &owner) {
                lockers.push(LockerInfo {
                    locker_address: locker_addr,
                    locker_name: Some(program_subkind_to_display(subkind)),
                    locked_amount_raw: amount,
                    unlock_at: None, // Phase 3: read on-chain lock schedule
                });
            }
        }
    }

    // Compute burned percentage.
    let lp_burned_pct = if total_supply > 0 {
        // burned_raw / total_supply * 100, using Decimal for precision.
        let burned_d = Decimal::from_u128(burned_raw).unwrap_or(Decimal::ZERO);
        let total_d = Decimal::from_u128(total_supply).unwrap_or_else(|| Decimal::from(1));
        (burned_d / total_d) * Decimal::from(100)
    } else {
        Decimal::ZERO
    };

    LpLockState {
        lp_burned_pct,
        lockers,
        lp_total_supply: total_supply,
    }
}

/// Convert a program subkind string to a display name for `LockerInfo.locker_name`.
fn program_subkind_to_display(subkind: &str) -> String {
    match subkind {
        "streamflow" => "Streamflow Finance",
        "jupiter_lock" => "Jupiter Lock",
        "jupiter_dtf" => "Jupiter DTF",
        "tuktuk" => "Tuktuk",
        other => other,
    }
    .to_owned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::tests::MockSolanaRpc;
    use crate::rpc::{DecodedMint, TokenAccountBalance};
    use crate::programs::{STREAMFLOW_TIMELOCK, BURN_ADDRESS};

    fn make_mock_mint(supply: u64) -> DecodedMint {
        DecodedMint {
            supply: supply as u128,
            decimals: 9,
            mint_authority: None,
            freeze_authority: None,
            is_token2022: false,
            raw_account_data: vec![0u8; 82],
        }
    }

    fn make_account_balance(address: &str, amount: u64) -> TokenAccountBalance {
        TokenAccountBalance {
            address: address.to_owned(),
            amount: amount.to_string(),
            ui_amount_string: None,
            decimals: 9,
        }
    }

    // --- LP with 100% burned ---

    #[tokio::test]
    async fn lp_fully_burned() {
        let rpc = MockSolanaRpc {
            mint_account: Some(Ok(Some(make_mock_mint(1_000_000)))),
            largest_accounts: Some(Ok(vec![
                make_account_balance(BURN_ADDRESS, 1_000_000),
            ])),
            ..Default::default()
        };

        let state = analyse_lp_lock(&rpc, "LpMint111111111111111111111111111111111111", Chain::Solana).await;
        // 100% of 1_000_000 is in burn address.
        assert_eq!(state.lp_burned_pct, Decimal::from(100));
        assert!(state.lockers.is_empty());
    }

    // --- LP with 0% burned ---

    #[tokio::test]
    async fn lp_not_burned() {
        let rpc = MockSolanaRpc {
            mint_account: Some(Ok(Some(make_mock_mint(1_000_000)))),
            largest_accounts: Some(Ok(vec![
                make_account_balance("SomeLiquidProvider1111111111111111111111111", 1_000_000),
            ])),
            // Owner is some regular wallet, not a locker.
            token_account_owner: Some(Ok(Some(
                "SomeRegularOwner1111111111111111111111111111".to_owned(),
            ))),
            ..Default::default()
        };

        let state = analyse_lp_lock(&rpc, "LpMint111111111111111111111111111111111111", Chain::Solana).await;
        assert_eq!(state.lp_burned_pct, Decimal::ZERO);
        assert!(state.lockers.is_empty());
    }

    // --- LP with Streamflow lock ---

    #[tokio::test]
    async fn lp_locked_in_streamflow() {
        let rpc = MockSolanaRpc {
            mint_account: Some(Ok(Some(make_mock_mint(2_000_000)))),
            largest_accounts: Some(Ok(vec![
                make_account_balance("SomeStreamflowEscrow1111111111111111111111", 2_000_000),
            ])),
            token_account_owner: Some(Ok(Some(STREAMFLOW_TIMELOCK.to_owned()))),
            ..Default::default()
        };

        let state = analyse_lp_lock(&rpc, "LpMint111111111111111111111111111111111111", Chain::Solana).await;
        assert_eq!(state.lp_burned_pct, Decimal::ZERO, "not burned, just locked");
        assert_eq!(state.lockers.len(), 1);
        let locker = &state.lockers[0];
        assert_eq!(locker.locked_amount_raw, 2_000_000u128);
        assert!(
            locker.locker_name.as_deref() == Some("Streamflow Finance"),
            "expected display name 'Streamflow Finance'"
        );
        assert!(locker.unlock_at.is_none(), "MVP: unlock_at is always None");
    }

    // --- Mint not found returns empty state ---

    #[tokio::test]
    async fn mint_not_found_returns_empty() {
        let rpc = MockSolanaRpc {
            mint_account: Some(Ok(None)), // mint doesn't exist
            ..Default::default()
        };
        let state = analyse_lp_lock(&rpc, "NonExistentMint1111111111111111111111111111", Chain::Solana).await;
        assert_eq!(state.lp_burned_pct, Decimal::ZERO);
        assert!(state.lockers.is_empty());
        assert_eq!(state.lp_total_supply, 0);
    }

    // --- Zero total supply ---

    #[tokio::test]
    async fn zero_total_supply_returns_zero_burned_pct() {
        let rpc = MockSolanaRpc {
            mint_account: Some(Ok(Some(make_mock_mint(0)))),
            ..Default::default()
        };
        let state = analyse_lp_lock(&rpc, "LpMint111111111111111111111111111111111111", Chain::Solana).await;
        assert_eq!(state.lp_burned_pct, Decimal::ZERO);
    }

    // --- Partial burn (50%) ---

    #[tokio::test]
    async fn lp_half_burned() {
        let rpc = MockSolanaRpc {
            mint_account: Some(Ok(Some(make_mock_mint(1_000_000)))),
            largest_accounts: Some(Ok(vec![
                make_account_balance(BURN_ADDRESS, 500_000),
                make_account_balance("SomeLiquidProvider1111111111111111111111111", 500_000),
            ])),
            token_account_owner: Some(Ok(Some(
                "SomeRegularOwner1111111111111111111111111111".to_owned(),
            ))),
            ..Default::default()
        };

        let state = analyse_lp_lock(&rpc, "LpMint111111111111111111111111111111111111", Chain::Solana).await;
        // 500_000 / 1_000_000 * 100 = 50%
        assert_eq!(state.lp_burned_pct, Decimal::from(50));
    }
}
