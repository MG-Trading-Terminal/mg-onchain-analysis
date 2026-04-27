//! `PoolAccountProvider` trait — abstraction over Raydium pool-state fetching.
//!
//! # Design
//!
//! The honeypot detector's `simulate_sell()` orchestrator needs the full account
//! set required to build a swap instruction. Fetching that account set from the
//! chain requires on-chain pool-state reads, ATA derivation, and wSOL wrapping
//! logic — roughly 1500 LOC that deserve their own sprint.
//!
//! This trait hides that future work behind a clean boundary so the orchestration
//! logic can be fully implemented and tested today, using `MockPoolAccountProvider`
//! in tests and `NotWiredPoolAccountProvider` in production until the follow-up lands.
//!
//! # Production wiring (follow-up task)
//!
//! Replace `NotWiredPoolAccountProvider` in `crates/gateway/src/routes/analyze.rs`
//! with a concrete implementation that calls `getAccountInfo` on the pool state
//! account, deserialises the Raydium layout, and derives user ATAs via
//! `spl-associated-token-account`. Tracked as a separate sprint item.
//!
//! # References
//!
//! - `docs/designs/0004-detector-01-honeypot.md` §3.2 (simulation algorithm)
//! - `docs/designs/0004-detector-01-honeypot.md` §9.2 (skip semantics)

use std::sync::Arc;

use async_trait::async_trait;
use solana_sdk::pubkey::Pubkey;
use thiserror::Error;

use mg_onchain_token_registry::SolanaRpc;

use crate::solana::openbook_market::{
    OpenbookMarketState, decode_openbook_market_state, derive_market_vault_signer,
};
use crate::solana::raydium_cpmm::{
    CpmmPoolState, PoolStateDecodeError, RaydiumCpmmSwapAccounts, decode_cpmm_pool_state,
    RAYDIUM_CPMM_PROGRAM_ID,
};
use crate::solana::raydium_v4::RaydiumV4SwapAccounts;
use crate::solana::raydium_v4_state::{
    RAYDIUM_V4_PROGRAM_ID_PUBKEY, AmmV4PoolState, AmmV4DecodeError, decode_amm_v4_pool_state,
};
use crate::solana::simulation::{SPL_TOKEN_PROGRAM_ID, derive_associated_token_account};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Error variants for [`PoolAccountProvider`] implementations.
#[derive(Debug, Error)]
pub enum PoolAccountError {
    /// Implementation not yet wired in this build (e.g. production provider
    /// pending pool-state fetcher). Detectors should treat this as a SKIP with
    /// reason, not a signal.
    #[error("pool_account_provider not wired: {reason}")]
    NotWired { reason: String },

    /// The requested pool was not found on-chain for the given DEX.
    #[error("pool {pool} not found on chain (dex={dex})")]
    PoolNotFound { pool: String, dex: String },

    /// An RPC error occurred while fetching pool account state.
    #[error("rpc error fetching pool {pool}: {reason}")]
    Rpc { pool: String, reason: String },

    /// The pool account data was present but could not be parsed.
    #[error("malformed pool state: {0}")]
    Malformed(String),
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Supplies the full account set required to build a swap instruction against
/// Raydium v4 or Raydium CPMM.
///
/// Hides pool-state fetching, ATA derivation, and token-program selection from
/// the detector so that `simulate_sell()` can remain pure and testable via mocks.
///
/// # Production provider (follow-up task)
///
/// The `NotWiredPoolAccountProvider` in this module returns
/// `PoolAccountError::NotWired` unconditionally. The detector translates this
/// into `sim_skipped = true, reason = "pool_account_provider_not_wired"` per
/// §9.2 skip semantics. Static signals S1–S5 remain the live defense.
///
/// A real implementation must:
/// 1. Call `getAccountInfo` on the pool state address.
/// 2. Deserialise the Raydium v4 / CPMM pool state layout.
/// 3. Derive user ATAs via the ATA program seeds.
/// 4. Return the complete account set.
#[async_trait]
pub trait PoolAccountProvider: Send + Sync {
    /// Fetch all accounts for a Raydium AMM v4 `SwapBaseIn` instruction.
    ///
    /// `pool` is the AMM pool state address. `user_owner` is the simulation
    /// keypair's pubkey — used to derive the user's token ATAs.
    async fn v4_swap_accounts(
        &self,
        pool: &Pubkey,
        user_owner: &Pubkey,
    ) -> Result<RaydiumV4SwapAccounts, PoolAccountError>;

    /// Fetch all accounts for a Raydium CPMM `swap_base_input` instruction.
    ///
    /// `pool` is the pool state address. `user_owner` is the simulation
    /// keypair's pubkey — used to derive the user's token ATAs.
    async fn cpmm_swap_accounts(
        &self,
        pool: &Pubkey,
        user_owner: &Pubkey,
    ) -> Result<RaydiumCpmmSwapAccounts, PoolAccountError>;
}

// ---------------------------------------------------------------------------
// NotWiredPoolAccountProvider (production stand-in)
// ---------------------------------------------------------------------------

/// Production stand-in for [`PoolAccountProvider`].
///
/// Returns [`PoolAccountError::NotWired`] for every call. The detector's
/// `simulate_sell()` catches this and emits `sim_skipped = true` with reason
/// `"pool_account_provider_not_wired"`, keeping S6 out of the confidence formula
/// until the follow-up task ships a real implementation.
///
/// # When to replace
///
/// Replace with a concrete implementation when the Raydium pool-state fetcher
/// and ATA derivation logic are available (separate sprint). The gateway wiring
/// in `crates/gateway/src/routes/analyze.rs` is the only construction site.
pub struct NotWiredPoolAccountProvider;

#[async_trait]
impl PoolAccountProvider for NotWiredPoolAccountProvider {
    async fn v4_swap_accounts(
        &self,
        _pool: &Pubkey,
        _user_owner: &Pubkey,
    ) -> Result<RaydiumV4SwapAccounts, PoolAccountError> {
        Err(PoolAccountError::NotWired {
            reason: "raydium_v4_pool_state_fetcher_not_implemented".to_owned(),
        })
    }

    async fn cpmm_swap_accounts(
        &self,
        _pool: &Pubkey,
        _user_owner: &Pubkey,
    ) -> Result<RaydiumCpmmSwapAccounts, PoolAccountError> {
        Err(PoolAccountError::NotWired {
            reason: "raydium_cpmm_pool_state_fetcher_not_implemented".to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// HttpPoolAccountProvider — real on-chain CPMM path (Sprint 9, B1.5)
// ---------------------------------------------------------------------------

/// CPMM authority PDA seed.
///
/// The AMM authority is derived via:
/// `Pubkey::find_program_address(&[CPMM_AUTHORITY_SEED], &RAYDIUM_CPMM_PROGRAM_ID)`
///
/// Verified against `raydium-cp-swap/programs/cp-swap/src/states/pool.rs`:
/// `pub const AUTH_SEED: &[u8] = b"vault_and_lp_mint_auth_seed";`
/// <https://github.com/raydium-io/raydium-cp-swap/blob/master/programs/cp-swap/src/states/pool.rs>
const CPMM_AUTHORITY_SEED: &[u8] = b"vault_and_lp_mint_auth_seed";

/// Production [`PoolAccountProvider`] backed by a live Solana RPC connection.
///
/// # CPMM real path
///
/// [`cpmm_swap_accounts`] fetches the pool state via `getAccountInfo`, decodes
/// the Raydium CPMM `PoolState` layout, and derives user ATAs from the
/// per-pool token-program fields.
///
/// # v4 path
///
/// [`v4_swap_accounts`] returns `PoolAccountError::NotWired` in B1. The v4 path
/// requires additional OpenBook market state fetching and is deferred to B2.
///
/// # Account slot semantics
///
/// The CPMM `swap_base_input` instruction has 13 account slots. This provider
/// sets `input_*` = token_0 fields and `output_*` = token_1 fields from the pool
/// state. The caller (the `simulate_sell` orchestrator in B2) is responsible for
/// swapping input/output direction to match buy vs. sell simulation direction,
/// per §DG4 of `docs/designs/0004-detector-01-honeypot.md`.
pub struct HttpPoolAccountProvider {
    rpc: Arc<dyn SolanaRpc>,
}

impl HttpPoolAccountProvider {
    /// Construct with an RPC client.
    pub fn new(rpc: Arc<dyn SolanaRpc>) -> Self {
        Self { rpc }
    }
}

/// Derive the CPMM AMM authority PDA.
fn cpmm_authority_pda() -> Pubkey {
    let program_id: Pubkey = RAYDIUM_CPMM_PROGRAM_ID
        .parse()
        .expect("RAYDIUM_CPMM_PROGRAM_ID is a valid pubkey");
    let (pda, _bump) =
        Pubkey::find_program_address(&[CPMM_AUTHORITY_SEED], &program_id);
    pda
}

#[async_trait]
impl PoolAccountProvider for HttpPoolAccountProvider {
    /// Fetch and compose all accounts for a Raydium AMM v4 `SwapBaseIn` instruction.
    ///
    /// # Steps
    ///
    /// 1. Fetch the AMM pool state account via `get_account_raw`.
    /// 2. Verify account owner is the Raydium v4 program (no Anchor discriminator in v4).
    /// 3. Decode the [`AmmV4PoolState`] layout (752-byte C-packed struct).
    /// 4. Fetch the OpenBook market state account (address from pool state).
    /// 5. Verify market owner matches `pool_state.market_program`.
    /// 6. Decode the [`OpenbookMarketState`] layout.
    /// 7. Derive the market vault signer PDA.
    /// 8. Derive the AMM authority PDA (`["amm authority"]` under v4 program).
    /// 9. Derive user ATAs (SPL Token classic — v4 pre-dates Token-2022 support).
    /// 10. Compose and return [`RaydiumV4SwapAccounts`].
    ///
    /// # Direction convention
    ///
    /// `user_source_token` = coin (token 0) ATA, `user_dest_token` = PC (token 1) ATA.
    /// The `simulate_sell` orchestrator handles direction per §DG4.
    async fn v4_swap_accounts(
        &self,
        pool: &Pubkey,
        user_owner: &Pubkey,
    ) -> Result<RaydiumV4SwapAccounts, PoolAccountError> {
        use mg_onchain_token_registry::RegistryError;

        // 1. Fetch pool state.
        let pool_raw = self
            .rpc
            .get_account_raw(&pool.to_string())
            .await
            .map_err(|e: RegistryError| PoolAccountError::Rpc {
                pool: pool.to_string(),
                reason: e.to_string(),
            })?
            .ok_or_else(|| PoolAccountError::PoolNotFound {
                pool: pool.to_string(),
                dex: "raydium_v4".to_owned(),
            })?;

        // 2. Verify owner is the Raydium v4 program.
        if pool_raw.owner != RAYDIUM_V4_PROGRAM_ID_PUBKEY {
            return Err(PoolAccountError::Malformed(format!(
                "v4 pool {} owned by {} instead of raydium_v4 program {}",
                pool, pool_raw.owner, RAYDIUM_V4_PROGRAM_ID_PUBKEY
            )));
        }

        // 3. Decode pool state.
        let pool_state: AmmV4PoolState =
            decode_amm_v4_pool_state(&pool_raw.data).map_err(|e: AmmV4DecodeError| {
                PoolAccountError::Malformed(format!("v4 pool state decode failed: {e}"))
            })?;

        // 4. Fetch market state.
        let market_raw = self
            .rpc
            .get_account_raw(&pool_state.market.to_string())
            .await
            .map_err(|e: RegistryError| PoolAccountError::Rpc {
                pool: pool_state.market.to_string(),
                reason: e.to_string(),
            })?
            .ok_or_else(|| PoolAccountError::Malformed(format!(
                "market account {} missing for v4 pool {}",
                pool_state.market, pool
            )))?;

        // 5. Verify market owner matches pool_state.market_program.
        if market_raw.owner != pool_state.market_program {
            return Err(PoolAccountError::Malformed(format!(
                "market {} owned by {} but pool_state.market_program = {}",
                pool_state.market, market_raw.owner, pool_state.market_program
            )));
        }

        // 6. Decode market state.
        let market_state: OpenbookMarketState =
            decode_openbook_market_state(&market_raw.data).map_err(|e| {
                PoolAccountError::Malformed(format!("openbook market state decode failed: {e}"))
            })?;

        // 7. Derive market vault signer.
        let market_vault_signer = derive_market_vault_signer(
            &pool_state.market,
            market_state.vault_signer_nonce,
            &pool_state.market_program,
        )
        .map_err(|e| PoolAccountError::Malformed(format!("vault signer derivation failed: {e}")))?;

        // 8. Derive AMM authority PDA.
        // Seed: b"amm authority" — verified against raydium-amm program/src/processor.rs
        // constant `AUTHORITY_AMM = "amm authority"`.
        // Source: https://github.com/raydium-io/raydium-amm/blob/master/program/src/processor.rs
        let (amm_authority, _bump) = Pubkey::find_program_address(
            &[b"amm authority"],
            &RAYDIUM_V4_PROGRAM_ID_PUBKEY,
        );

        // 9. Derive user ATAs — SPL Token classic only for v4 (no Token-2022).
        let user_source_token = derive_associated_token_account(
            user_owner,
            &SPL_TOKEN_PROGRAM_ID,
            &pool_state.coin_vault_mint,
        );
        let user_dest_token = derive_associated_token_account(
            user_owner,
            &SPL_TOKEN_PROGRAM_ID,
            &pool_state.pc_vault_mint,
        );

        // 10. Compose result.
        Ok(RaydiumV4SwapAccounts {
            amm_pool: *pool,
            amm_authority,
            amm_open_orders: pool_state.open_orders,
            amm_target_orders: pool_state.target_orders,
            pool_coin_vault: pool_state.coin_vault,
            pool_pc_vault: pool_state.pc_vault,
            market_program: pool_state.market_program,
            market: pool_state.market,
            market_bids: market_state.bids,
            market_asks: market_state.asks,
            market_event_queue: market_state.event_queue,
            market_coin_vault: market_state.coin_vault,
            market_pc_vault: market_state.pc_vault,
            market_vault_signer,
            user_source_token,
            user_dest_token,
            user_owner: *user_owner,
        })
    }

    /// Fetch and compose all accounts for a Raydium CPMM `swap_base_input` instruction.
    ///
    /// # Steps
    ///
    /// 1. Fetch the pool state account via `get_account_raw`.
    /// 2. Verify account owner is the CPMM program.
    /// 3. Decode the [`CpmmPoolState`] layout.
    /// 4. Derive user input/output ATAs from the pool's per-mint token-program fields.
    /// 5. Derive the AMM authority PDA.
    /// 6. Compose and return [`RaydiumCpmmSwapAccounts`].
    ///
    /// # Direction convention
    ///
    /// `input_*` = token_0 side, `output_*` = token_1 side.
    /// The orchestrator swaps direction per §DG4.
    async fn cpmm_swap_accounts(
        &self,
        pool: &Pubkey,
        user_owner: &Pubkey,
    ) -> Result<RaydiumCpmmSwapAccounts, PoolAccountError> {
        use mg_onchain_token_registry::RegistryError;

        // 1. Fetch pool account.
        let raw = self
            .rpc
            .get_account_raw(&pool.to_string())
            .await
            .map_err(|e: RegistryError| PoolAccountError::Rpc {
                pool: pool.to_string(),
                reason: e.to_string(),
            })?
            .ok_or_else(|| PoolAccountError::PoolNotFound {
                pool: pool.to_string(),
                dex: "raydium_cpmm".to_owned(),
            })?;

        // 2. Verify owner.
        let expected_program: Pubkey = RAYDIUM_CPMM_PROGRAM_ID
            .parse()
            .expect("RAYDIUM_CPMM_PROGRAM_ID is valid");
        if raw.owner != expected_program {
            return Err(PoolAccountError::Malformed(format!(
                "pool {} owned by {} instead of CPMM program {}",
                pool, raw.owner, RAYDIUM_CPMM_PROGRAM_ID
            )));
        }

        // 3. Decode pool state.
        let state: CpmmPoolState =
            decode_cpmm_pool_state(&raw.data).map_err(|e: PoolStateDecodeError| {
                PoolAccountError::Malformed(format!("pool state decode failed: {e}"))
            })?;

        // 4. Derive user ATAs.
        let input_token_account =
            derive_associated_token_account(user_owner, &state.token_0_program, &state.token_0_mint);
        let output_token_account =
            derive_associated_token_account(user_owner, &state.token_1_program, &state.token_1_mint);

        // 5. Derive authority PDA.
        let authority = cpmm_authority_pda();

        // 6. Compose result.
        Ok(RaydiumCpmmSwapAccounts {
            payer: *user_owner,
            authority,
            amm_config: state.amm_config,
            pool_state: *pool,
            input_token_account,
            output_token_account,
            input_vault: state.token_0_vault,
            output_vault: state.token_1_vault,
            input_token_program: state.token_0_program,
            output_token_program: state.token_1_program,
            input_token_mint: state.token_0_mint,
            output_token_mint: state.token_1_mint,
            observation_state: state.observation_key,
        })
    }
}

// ---------------------------------------------------------------------------
// MockPoolAccountProvider (test utilities)
// ---------------------------------------------------------------------------

/// Configurable mock for [`PoolAccountProvider`] used in detector unit tests.
///
/// Call `with_v4_accounts`, `with_cpmm_accounts`, `with_v4_error`, or
/// `with_cpmm_error` to set the canned response. Both methods default to
/// returning `PoolAccountError::NotWired` when not configured, so tests that
/// do not need pool accounts can use `MockPoolAccountProvider::default()`.
#[cfg(any(test, feature = "test-utils"))]
#[derive(Debug, Default)]
pub struct MockPoolAccountProvider {
    v4_response: Option<Result<RaydiumV4SwapAccounts, PoolAccountError>>,
    cpmm_response: Option<Result<RaydiumCpmmSwapAccounts, PoolAccountError>>,
}

#[cfg(any(test, feature = "test-utils"))]
impl MockPoolAccountProvider {
    /// Configure a successful Raydium v4 swap account response.
    pub fn with_v4_accounts(mut self, accounts: RaydiumV4SwapAccounts) -> Self {
        self.v4_response = Some(Ok(accounts));
        self
    }

    /// Configure an error response for Raydium v4 swap accounts.
    pub fn with_v4_error(mut self, err: PoolAccountError) -> Self {
        self.v4_response = Some(Err(err));
        self
    }

    /// Configure a successful Raydium CPMM swap account response.
    pub fn with_cpmm_accounts(mut self, accounts: RaydiumCpmmSwapAccounts) -> Self {
        self.cpmm_response = Some(Ok(accounts));
        self
    }

    /// Configure an error response for Raydium CPMM swap accounts.
    pub fn with_cpmm_error(mut self, err: PoolAccountError) -> Self {
        self.cpmm_response = Some(Err(err));
        self
    }

    /// Clone a `RaydiumV4SwapAccounts` from the stored response (for test assertions).
    fn clone_v4_response(&self) -> Result<RaydiumV4SwapAccounts, PoolAccountError> {
        match &self.v4_response {
            Some(Ok(a)) => Ok(RaydiumV4SwapAccounts {
                amm_pool: a.amm_pool,
                amm_authority: a.amm_authority,
                amm_open_orders: a.amm_open_orders,
                amm_target_orders: a.amm_target_orders,
                pool_coin_vault: a.pool_coin_vault,
                pool_pc_vault: a.pool_pc_vault,
                market_program: a.market_program,
                market: a.market,
                market_bids: a.market_bids,
                market_asks: a.market_asks,
                market_event_queue: a.market_event_queue,
                market_coin_vault: a.market_coin_vault,
                market_pc_vault: a.market_pc_vault,
                market_vault_signer: a.market_vault_signer,
                user_source_token: a.user_source_token,
                user_dest_token: a.user_dest_token,
                user_owner: a.user_owner,
            }),
            Some(Err(_)) => Err(PoolAccountError::NotWired {
                reason: "mock configured with error — check with_v4_error()".to_owned(),
            }),
            None => Err(PoolAccountError::NotWired {
                reason: "v4_response not configured on MockPoolAccountProvider".to_owned(),
            }),
        }
    }

    /// Clone a `RaydiumCpmmSwapAccounts` from the stored response (for test assertions).
    fn clone_cpmm_response(&self) -> Result<RaydiumCpmmSwapAccounts, PoolAccountError> {
        match &self.cpmm_response {
            Some(Ok(a)) => Ok(RaydiumCpmmSwapAccounts {
                payer: a.payer,
                authority: a.authority,
                amm_config: a.amm_config,
                pool_state: a.pool_state,
                input_token_account: a.input_token_account,
                output_token_account: a.output_token_account,
                input_vault: a.input_vault,
                output_vault: a.output_vault,
                input_token_program: a.input_token_program,
                output_token_program: a.output_token_program,
                input_token_mint: a.input_token_mint,
                output_token_mint: a.output_token_mint,
                observation_state: a.observation_state,
            }),
            Some(Err(_)) => Err(PoolAccountError::NotWired {
                reason: "mock configured with error — check with_cpmm_error()".to_owned(),
            }),
            None => Err(PoolAccountError::NotWired {
                reason: "cpmm_response not configured on MockPoolAccountProvider".to_owned(),
            }),
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
#[async_trait]
impl PoolAccountProvider for MockPoolAccountProvider {
    async fn v4_swap_accounts(
        &self,
        _pool: &Pubkey,
        user_owner: &Pubkey,
    ) -> Result<RaydiumV4SwapAccounts, PoolAccountError> {
        // Set user_owner so the signing keypair matches the signer slot in the
        // instruction AccountMeta. Real providers derive the user's ATAs from
        // this pubkey; the mock just stamps it into the user_owner field.
        self.clone_v4_response().map(|mut a| {
            a.user_owner = *user_owner;
            a
        })
    }

    async fn cpmm_swap_accounts(
        &self,
        _pool: &Pubkey,
        user_owner: &Pubkey,
    ) -> Result<RaydiumCpmmSwapAccounts, PoolAccountError> {
        // Set payer so the signing keypair matches the signer slot (index 0)
        // in the AccountMeta list. Real providers derive user ATAs from this
        // pubkey; the mock just stamps it into the payer field.
        self.clone_cpmm_response().map(|mut a| {
            a.payer = *user_owner;
            a
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use solana_sdk::pubkey::Pubkey;

    fn dummy_pool() -> Pubkey {
        Pubkey::new_from_array([0xAA; 32])
    }

    fn dummy_user() -> Pubkey {
        Pubkey::new_from_array([0xBB; 32])
    }

    fn dummy_v4_accounts() -> RaydiumV4SwapAccounts {
        let k = Pubkey::new_from_array([0x01; 32]);
        RaydiumV4SwapAccounts {
            amm_pool: k,
            amm_authority: k,
            amm_open_orders: k,
            amm_target_orders: k,
            pool_coin_vault: k,
            pool_pc_vault: k,
            market_program: k,
            market: k,
            market_bids: k,
            market_asks: k,
            market_event_queue: k,
            market_coin_vault: k,
            market_pc_vault: k,
            market_vault_signer: k,
            user_source_token: k,
            user_dest_token: k,
            user_owner: k,
        }
    }

    fn dummy_cpmm_accounts() -> RaydiumCpmmSwapAccounts {
        let k = Pubkey::new_from_array([0x02; 32]);
        RaydiumCpmmSwapAccounts {
            payer: k,
            authority: k,
            amm_config: k,
            pool_state: k,
            input_token_account: k,
            output_token_account: k,
            input_vault: k,
            output_vault: k,
            input_token_program: k,
            output_token_program: k,
            input_token_mint: k,
            output_token_mint: k,
            observation_state: k,
        }
    }

    #[tokio::test]
    async fn not_wired_v4_returns_typed_error() {
        let provider = NotWiredPoolAccountProvider;
        let result = provider.v4_swap_accounts(&dummy_pool(), &dummy_user()).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, PoolAccountError::NotWired { .. }),
            "must be NotWired variant, got: {err}"
        );
        assert!(
            err.to_string().contains("raydium_v4_pool_state_fetcher_not_implemented"),
            "error message must name the missing component: {err}"
        );
    }

    #[tokio::test]
    async fn not_wired_cpmm_returns_typed_error() {
        let provider = NotWiredPoolAccountProvider;
        let result = provider.cpmm_swap_accounts(&dummy_pool(), &dummy_user()).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, PoolAccountError::NotWired { .. }),
            "must be NotWired variant, got: {err}"
        );
        assert!(
            err.to_string().contains("raydium_cpmm_pool_state_fetcher_not_implemented"),
            "error message must name the missing component: {err}"
        );
    }

    #[tokio::test]
    async fn mock_v4_returns_configured_accounts() {
        let provider = MockPoolAccountProvider::default().with_v4_accounts(dummy_v4_accounts());
        let result = provider.v4_swap_accounts(&dummy_pool(), &dummy_user()).await;
        assert!(result.is_ok(), "configured mock must succeed");
    }

    #[tokio::test]
    async fn mock_cpmm_returns_configured_accounts() {
        let provider =
            MockPoolAccountProvider::default().with_cpmm_accounts(dummy_cpmm_accounts());
        let result = provider.cpmm_swap_accounts(&dummy_pool(), &dummy_user()).await;
        assert!(result.is_ok(), "configured mock must succeed");
    }

    #[tokio::test]
    async fn mock_unconfigured_v4_returns_not_wired() {
        let provider = MockPoolAccountProvider::default();
        let result = provider.v4_swap_accounts(&dummy_pool(), &dummy_user()).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), PoolAccountError::NotWired { .. }));
    }

    #[tokio::test]
    async fn mock_unconfigured_cpmm_returns_not_wired() {
        let provider = MockPoolAccountProvider::default();
        let result = provider.cpmm_swap_accounts(&dummy_pool(), &dummy_user()).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), PoolAccountError::NotWired { .. }));
    }

    // -----------------------------------------------------------------------
    // HttpPoolAccountProvider tests (B1.5, Sprint 9)
    //
    // Uses MockSolanaRpc::with_account_raw populated with the checked-in CPMM
    // pool fixture bytes. Pool: 2AqnFKiRgCcf7iravD8nWcyS6MG2fu3c6rBSpiPWnw9A
    // (Solana mainnet, 2026-04-24).
    // -----------------------------------------------------------------------

    /// The fixture pool pubkey — used as the `pool` argument.
    const FIXTURE_POOL: &str = "2AqnFKiRgCcf7iravD8nWcyS6MG2fu3c6rBSpiPWnw9A";

    /// Fixture bytes for pool 2AqnFKiRgCcf7iravD8nWcyS6MG2fu3c6rBSpiPWnw9A.
    const FIXTURE_BYTES: &[u8] = include_bytes!(
        "../../../../tests/fixtures/raydium_cpmm/2AqnFKiRgCcf7iravD8nWcyS6MG2fu3c6rBSpiPWnw9A.bin"
    );

    fn cpmm_program_pubkey() -> Pubkey {
        crate::solana::raydium_cpmm::RAYDIUM_CPMM_PROGRAM_ID
            .parse()
            .unwrap()
    }

    fn make_raw_account(data: Vec<u8>) -> mg_onchain_token_registry::rpc::RawAccount {
        mg_onchain_token_registry::rpc::RawAccount {
            lamports: 5_616_720,
            owner: cpmm_program_pubkey(),
            data,
            executable: false,
            rent_epoch: 0,
        }
    }

    #[tokio::test]
    async fn http_provider_cpmm_returns_correct_vaults_and_mints() {
        use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;

        let pool_pubkey: Pubkey = FIXTURE_POOL.parse().unwrap();
        let user = Pubkey::new_from_array([0xAB; 32]);

        let mock_rpc = MockSolanaRpc::default()
            .with_account_raw(FIXTURE_POOL, make_raw_account(FIXTURE_BYTES.to_vec()));
        let provider = HttpPoolAccountProvider::new(Arc::new(mock_rpc));

        let result = provider.cpmm_swap_accounts(&pool_pubkey, &user).await;
        assert!(result.is_ok(), "HttpPoolAccountProvider must succeed on valid fixture: {result:?}");

        let accs = result.unwrap();

        // payer = user_owner
        assert_eq!(accs.payer, user, "payer must equal user_owner");

        // pool_state = the pool address
        assert_eq!(accs.pool_state, pool_pubkey, "pool_state must match input pool");

        // Vaults — non-zero and distinct
        assert_ne!(accs.input_vault, Pubkey::default(), "input_vault must not be zero");
        assert_ne!(accs.output_vault, Pubkey::default(), "output_vault must not be zero");
        assert_ne!(accs.input_vault, accs.output_vault, "vaults must be distinct");

        // Mints match fixture
        let expected_token_0_mint: Pubkey =
            "oHo3ssTsm9bxtegyRMpYsvASVGQAF2SYqeX1JjJWXRA".parse().unwrap();
        let expected_token_1_mint: Pubkey =
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
        assert_eq!(accs.input_token_mint, expected_token_0_mint, "input_token_mint mismatch");
        assert_eq!(accs.output_token_mint, expected_token_1_mint, "output_token_mint mismatch");

        // Token programs: token_0 = Token-2022, token_1 = SPL Token
        let token_2022: Pubkey = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".parse().unwrap();
        let spl_token: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse().unwrap();
        assert_eq!(accs.input_token_program, token_2022, "input_token_program must be Token-2022");
        assert_eq!(accs.output_token_program, spl_token, "output_token_program must be SPL Token");

        // User ATAs are deterministic (non-zero)
        assert_ne!(accs.input_token_account, Pubkey::default(), "input ATA must not be zero");
        assert_ne!(accs.output_token_account, Pubkey::default(), "output ATA must not be zero");
        assert_ne!(accs.input_token_account, accs.output_token_account, "ATAs must be distinct");

        // Authority must be the CPMM authority PDA (non-zero)
        assert_ne!(accs.authority, Pubkey::default(), "authority must not be zero");
        assert_ne!(accs.authority, pool_pubkey, "authority must differ from pool");

        // observation_state must be non-zero
        assert_ne!(accs.observation_state, Pubkey::default(), "observation_state must not be zero");
    }

    #[tokio::test]
    async fn http_provider_cpmm_atas_are_deterministic() {
        use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;

        let pool_pubkey: Pubkey = FIXTURE_POOL.parse().unwrap();
        let user = Pubkey::new_from_array([0xCD; 32]);

        let make_provider = || {
            let mock = MockSolanaRpc::default()
                .with_account_raw(FIXTURE_POOL, make_raw_account(FIXTURE_BYTES.to_vec()));
            HttpPoolAccountProvider::new(Arc::new(mock))
        };

        let accs1 = make_provider().cpmm_swap_accounts(&pool_pubkey, &user).await.unwrap();
        let accs2 = make_provider().cpmm_swap_accounts(&pool_pubkey, &user).await.unwrap();

        assert_eq!(
            accs1.input_token_account, accs2.input_token_account,
            "input ATA must be deterministic"
        );
        assert_eq!(
            accs1.output_token_account, accs2.output_token_account,
            "output ATA must be deterministic"
        );
        assert_eq!(accs1.authority, accs2.authority, "authority PDA must be deterministic");
    }

    #[tokio::test]
    async fn http_provider_cpmm_pool_not_found_returns_pool_not_found() {
        use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;

        // MockSolanaRpc returns Ok(None) for unconfigured addresses
        let pool_pubkey: Pubkey = FIXTURE_POOL.parse().unwrap();
        let user = Pubkey::new_from_array([0x11; 32]);
        let provider = HttpPoolAccountProvider::new(Arc::new(MockSolanaRpc::default()));

        let result = provider.cpmm_swap_accounts(&pool_pubkey, &user).await;
        assert!(matches!(result, Err(PoolAccountError::PoolNotFound { .. })));
    }

    #[tokio::test]
    async fn http_provider_cpmm_wrong_owner_returns_malformed() {
        use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;

        let pool_pubkey: Pubkey = FIXTURE_POOL.parse().unwrap();
        let user = Pubkey::new_from_array([0x11; 32]);

        // Construct a RawAccount with a wrong (non-CPMM) owner
        let wrong_owner_account = mg_onchain_token_registry::rpc::RawAccount {
            lamports: 100,
            owner: Pubkey::new_from_array([0xFF; 32]), // wrong owner
            data: FIXTURE_BYTES.to_vec(),
            executable: false,
            rent_epoch: 0,
        };
        let mock = MockSolanaRpc::default()
            .with_account_raw(FIXTURE_POOL, wrong_owner_account);
        let provider = HttpPoolAccountProvider::new(Arc::new(mock));

        let result = provider.cpmm_swap_accounts(&pool_pubkey, &user).await;
        assert!(matches!(result, Err(PoolAccountError::Malformed(_))), "wrong owner must be Malformed: {result:?}");
    }

    // -----------------------------------------------------------------------
    // HttpPoolAccountProvider v4 tests (B2.3, Sprint 9)
    //
    // Uses synthetic fixtures built from known on-chain values for the
    // well-known SOL/USDC Raydium v4 pool:
    //   Pool: 58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2
    //   Market: 9wFFyRfZBsuAha4YcuxcXLKwMxJR43S7fPfQLusDBzvT
    //   Market program: srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX (Serum)
    //   coin_vault_mint: So11111111111111111111111111111111111111112 (wSOL)
    //   pc_vault_mint: EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v (USDC)
    //
    // Captured from Solana mainnet 2026-04-24. We build synthetic byte buffers
    // rather than captured binary fixtures because the synthetic builder produces
    // deterministic known values for all fields we care about.
    // -----------------------------------------------------------------------

    use crate::solana::raydium_v4_state::{AMM_V4_POOL_STATE_SIZE, RAYDIUM_V4_PROGRAM_ID_PUBKEY};
    use crate::solana::openbook_market::SERUM_PROGRAM_ID;

    const V4_FIXTURE_POOL: &str = "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2";
    const V4_FIXTURE_MARKET: &str = "9wFFyRfZBsuAha4YcuxcXLKwMxJR43S7fPfQLusDBzvT";

    fn make_v4_pool_raw(pool_state_data: Vec<u8>) -> mg_onchain_token_registry::rpc::RawAccount {
        mg_onchain_token_registry::rpc::RawAccount {
            lamports: 3_591_360,
            owner: RAYDIUM_V4_PROGRAM_ID_PUBKEY,
            data: pool_state_data,
            executable: false,
            rent_epoch: 0,
        }
    }

    fn make_market_raw(market_data: Vec<u8>, market_program: Pubkey) -> mg_onchain_token_registry::rpc::RawAccount {
        mg_onchain_token_registry::rpc::RawAccount {
            lamports: 10_000_000,
            owner: market_program,
            data: market_data,
            executable: false,
            rent_epoch: 0,
        }
    }

    /// Build a synthetic v4 pool state byte buffer with known field values.
    #[allow(clippy::too_many_arguments)]
    fn build_v4_pool_bytes(
        coin_vault_mint: &Pubkey,
        pc_vault_mint: &Pubkey,
        market: &Pubkey,
        market_program: &Pubkey,
        coin_vault: &Pubkey,
        pc_vault: &Pubkey,
        open_orders: &Pubkey,
        target_orders: &Pubkey,
        nonce: u64,
    ) -> Vec<u8> {
        let mut data = vec![0u8; AMM_V4_POOL_STATE_SIZE];
        let write_u64 = |buf: &mut Vec<u8>, off: usize, val: u64| {
            buf[off..off + 8].copy_from_slice(&val.to_le_bytes());
        };
        let write_pk = |buf: &mut Vec<u8>, off: usize, pk: &Pubkey| {
            buf[off..off + 32].copy_from_slice(pk.as_ref());
        };
        write_u64(&mut data, 0, 6); // status
        write_u64(&mut data, 8, nonce); // nonce
        write_u64(&mut data, 32, 9);  // coin_decimals
        write_u64(&mut data, 40, 6);  // pc_decimals
        // Pubkeys at corrected offsets (336-based, verified 2026-04-24).
        // pool_total_deposit_pc/coin are u128, shifting all pubkeys +16 from
        // the originally documented 320 start.
        write_pk(&mut data, 336, coin_vault);
        write_pk(&mut data, 368, pc_vault);
        write_pk(&mut data, 400, coin_vault_mint);
        write_pk(&mut data, 432, pc_vault_mint);
        let dummy_lp = Pubkey::new_from_array([0x11; 32]);
        write_pk(&mut data, 464, &dummy_lp);
        write_pk(&mut data, 496, open_orders);
        write_pk(&mut data, 528, market);
        write_pk(&mut data, 560, market_program);
        write_pk(&mut data, 592, target_orders);
        let dummy_wq = Pubkey::new_from_array([0x22; 32]);
        write_pk(&mut data, 624, &dummy_wq);
        let dummy_lpvault = Pubkey::new_from_array([0x33; 32]);
        write_pk(&mut data, 656, &dummy_lpvault);
        let dummy_owner = Pubkey::new_from_array([0x44; 32]);
        write_pk(&mut data, 688, &dummy_owner);
        write_pk(&mut data, 720, &dummy_owner);
        data
    }

    /// Build a synthetic OpenBook market byte buffer with known field values.
    #[allow(clippy::too_many_arguments)]
    fn build_market_bytes(
        vault_signer_nonce: u64,
        coin_mint: &Pubkey,
        pc_mint: &Pubkey,
        coin_vault: &Pubkey,
        pc_vault: &Pubkey,
        bids: &Pubkey,
        asks: &Pubkey,
        event_queue: &Pubkey,
    ) -> Vec<u8> {
        use crate::solana::openbook_market;

        let mut data = vec![0u8; openbook_market::MARKET_STATE_MIN_SIZE + 64];
        data[0..5].copy_from_slice(b"serum");
        let write_u64 = |buf: &mut Vec<u8>, off: usize, val: u64| {
            buf[off..off + 8].copy_from_slice(&val.to_le_bytes());
        };
        let write_pk = |buf: &mut Vec<u8>, off: usize, pk: &Pubkey| {
            buf[off..off + 32].copy_from_slice(pk.as_ref());
        };
        // vault_signer_nonce @ 45
        write_u64(&mut data, 45, vault_signer_nonce);
        // coin_mint @ 53
        write_pk(&mut data, 53, coin_mint);
        // pc_mint @ 85
        write_pk(&mut data, 85, pc_mint);
        // coin_vault @ 117
        write_pk(&mut data, 117, coin_vault);
        // pc_vault @ 165
        write_pk(&mut data, 165, pc_vault);
        // bids @ 285
        write_pk(&mut data, 285, bids);
        // asks @ 317
        write_pk(&mut data, 317, asks);
        // event_queue @ 253
        write_pk(&mut data, 253, event_queue);
        data
    }

    /// Find a vault signer nonce that produces a valid PDA for (market, serum_program).
    fn find_valid_vault_signer_nonce(market: &Pubkey, market_program: &Pubkey) -> u64 {
        for nonce in 0u64..=255 {
            if Pubkey::create_program_address(
                &[market.as_ref(), &nonce.to_le_bytes()],
                market_program,
            ).is_ok() {
                return nonce;
            }
        }
        panic!("no valid vault signer nonce found for market {market} under program {market_program}");
    }

    #[tokio::test]
    async fn http_provider_v4_pool_not_found_returns_pool_not_found() {
        use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;

        let pool_pubkey: Pubkey = V4_FIXTURE_POOL.parse().unwrap();
        let user = Pubkey::new_from_array([0xAA; 32]);
        // MockSolanaRpc returns Ok(None) for unconfigured addresses
        let provider = HttpPoolAccountProvider::new(Arc::new(MockSolanaRpc::default()));

        let result = provider.v4_swap_accounts(&pool_pubkey, &user).await;
        assert!(
            matches!(result, Err(PoolAccountError::PoolNotFound { .. })),
            "missing pool must return PoolNotFound: {result:?}"
        );
    }

    #[tokio::test]
    async fn http_provider_v4_wrong_pool_owner_returns_malformed() {
        use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;

        let pool_pubkey: Pubkey = V4_FIXTURE_POOL.parse().unwrap();
        let user = Pubkey::new_from_array([0xBB; 32]);

        // Pool account with wrong owner
        let wrong_owner_account = mg_onchain_token_registry::rpc::RawAccount {
            lamports: 100,
            owner: Pubkey::new_from_array([0xFF; 32]),
            data: vec![0u8; AMM_V4_POOL_STATE_SIZE],
            executable: false,
            rent_epoch: 0,
        };
        let mock = MockSolanaRpc::default()
            .with_account_raw(V4_FIXTURE_POOL, wrong_owner_account);
        let provider = HttpPoolAccountProvider::new(Arc::new(mock));

        let result = provider.v4_swap_accounts(&pool_pubkey, &user).await;
        assert!(
            matches!(result, Err(PoolAccountError::Malformed(_))),
            "wrong pool owner must return Malformed: {result:?}"
        );
    }

    #[tokio::test]
    async fn http_provider_v4_missing_market_returns_malformed() {
        use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;

        let pool_pubkey: Pubkey = V4_FIXTURE_POOL.parse().unwrap();
        let market_pubkey: Pubkey = V4_FIXTURE_MARKET.parse().unwrap();
        let user = Pubkey::new_from_array([0xCC; 32]);

        let coin_vault_mint: Pubkey = "So11111111111111111111111111111111111111112".parse().unwrap();
        let pc_vault_mint: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
        let dummy = Pubkey::new_from_array([0x55; 32]);

        let v4_data = build_v4_pool_bytes(
            &coin_vault_mint, &pc_vault_mint,
            &market_pubkey, &SERUM_PROGRAM_ID,
            &dummy, &dummy, &dummy, &dummy, 254,
        );

        // Pool is present but market is NOT in the mock
        let mock = MockSolanaRpc::default()
            .with_account_raw(V4_FIXTURE_POOL, make_v4_pool_raw(v4_data));
        let provider = HttpPoolAccountProvider::new(Arc::new(mock));

        let result = provider.v4_swap_accounts(&pool_pubkey, &user).await;
        assert!(
            matches!(result, Err(PoolAccountError::Malformed(_))),
            "missing market must return Malformed: {result:?}"
        );
    }

    #[tokio::test]
    async fn http_provider_v4_success_returns_correct_accounts() {
        use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;

        let pool_pubkey: Pubkey = V4_FIXTURE_POOL.parse().unwrap();
        let market_pubkey: Pubkey = V4_FIXTURE_MARKET.parse().unwrap();
        let user = Pubkey::new_from_array([0xDD; 32]);

        let coin_vault_mint: Pubkey = "So11111111111111111111111111111111111111112".parse().unwrap();
        let pc_vault_mint: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
        let coin_vault   = Pubkey::new_from_array([0xA1; 32]);
        let pc_vault     = Pubkey::new_from_array([0xA2; 32]);
        let open_orders  = Pubkey::new_from_array([0xA3; 32]);
        let target_orders = Pubkey::new_from_array([0xA4; 32]);

        // Find a valid vault signer nonce for this (market, serum_program) pair.
        let nonce = find_valid_vault_signer_nonce(&market_pubkey, &SERUM_PROGRAM_ID);

        let v4_data = build_v4_pool_bytes(
            &coin_vault_mint, &pc_vault_mint,
            &market_pubkey, &SERUM_PROGRAM_ID,
            &coin_vault, &pc_vault, &open_orders, &target_orders, nonce,
        );

        let market_bids  = Pubkey::new_from_array([0xB1; 32]);
        let market_asks  = Pubkey::new_from_array([0xB2; 32]);
        let market_evq   = Pubkey::new_from_array([0xB3; 32]);
        let market_coin_vault = Pubkey::new_from_array([0xB4; 32]);
        let market_pc_vault   = Pubkey::new_from_array([0xB5; 32]);

        let market_data = build_market_bytes(
            nonce,
            &coin_vault_mint,
            &pc_vault_mint,
            &market_coin_vault,
            &market_pc_vault,
            &market_bids,
            &market_asks,
            &market_evq,
        );

        let mock = MockSolanaRpc::default()
            .with_account_raw(V4_FIXTURE_POOL, make_v4_pool_raw(v4_data))
            .with_account_raw(V4_FIXTURE_MARKET, make_market_raw(market_data, SERUM_PROGRAM_ID));
        let provider = HttpPoolAccountProvider::new(Arc::new(mock));

        let result = provider.v4_swap_accounts(&pool_pubkey, &user).await;
        assert!(result.is_ok(), "v4 success path must succeed: {result:?}");

        let accs = result.unwrap();

        // Pool addresses
        assert_eq!(accs.amm_pool, pool_pubkey, "amm_pool must match input pool");
        assert_eq!(accs.amm_open_orders, open_orders, "amm_open_orders mismatch");
        assert_eq!(accs.amm_target_orders, target_orders, "amm_target_orders mismatch");
        assert_eq!(accs.pool_coin_vault, coin_vault, "pool_coin_vault mismatch");
        assert_eq!(accs.pool_pc_vault, pc_vault, "pool_pc_vault mismatch");
        assert_eq!(accs.market, market_pubkey, "market mismatch");
        assert_eq!(accs.market_program, SERUM_PROGRAM_ID, "market_program mismatch");

        // Market accounts
        assert_eq!(accs.market_bids, market_bids, "market_bids mismatch");
        assert_eq!(accs.market_asks, market_asks, "market_asks mismatch");
        assert_eq!(accs.market_event_queue, market_evq, "market_event_queue mismatch");
        assert_eq!(accs.market_coin_vault, market_coin_vault, "market_coin_vault mismatch");
        assert_eq!(accs.market_pc_vault, market_pc_vault, "market_pc_vault mismatch");

        // Derived accounts
        assert_ne!(accs.amm_authority, Pubkey::default(), "amm_authority must not be zero");
        assert_ne!(accs.market_vault_signer, Pubkey::default(), "market_vault_signer must not be zero");
        assert_eq!(accs.user_owner, user, "user_owner must equal input user");

        // User ATAs — derived from SPL Token classic
        assert_ne!(accs.user_source_token, Pubkey::default(), "user_source_token must not be zero");
        assert_ne!(accs.user_dest_token, Pubkey::default(), "user_dest_token must not be zero");
        assert_ne!(accs.user_source_token, accs.user_dest_token, "source/dest ATAs must differ");
    }

    #[tokio::test]
    async fn http_provider_v4_atas_are_deterministic() {
        use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;

        let pool_pubkey: Pubkey = V4_FIXTURE_POOL.parse().unwrap();
        let market_pubkey: Pubkey = V4_FIXTURE_MARKET.parse().unwrap();
        let user = Pubkey::new_from_array([0xEE; 32]);

        let coin_vault_mint: Pubkey = "So11111111111111111111111111111111111111112".parse().unwrap();
        let pc_vault_mint: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
        let dummy = Pubkey::new_from_array([0x77; 32]);
        let nonce = find_valid_vault_signer_nonce(&market_pubkey, &SERUM_PROGRAM_ID);

        let v4_data = build_v4_pool_bytes(
            &coin_vault_mint, &pc_vault_mint,
            &market_pubkey, &SERUM_PROGRAM_ID,
            &dummy, &dummy, &dummy, &dummy, nonce,
        );
        let market_data = build_market_bytes(
            nonce, &coin_vault_mint, &pc_vault_mint,
            &dummy, &dummy, &dummy, &dummy, &dummy,
        );

        let make_provider = || {
            let mock = MockSolanaRpc::default()
                .with_account_raw(V4_FIXTURE_POOL, make_v4_pool_raw(v4_data.clone()))
                .with_account_raw(V4_FIXTURE_MARKET, make_market_raw(market_data.clone(), SERUM_PROGRAM_ID));
            HttpPoolAccountProvider::new(Arc::new(mock))
        };

        let accs1 = make_provider().v4_swap_accounts(&pool_pubkey, &user).await.unwrap();
        let accs2 = make_provider().v4_swap_accounts(&pool_pubkey, &user).await.unwrap();

        assert_eq!(accs1.user_source_token, accs2.user_source_token, "source ATA must be deterministic");
        assert_eq!(accs1.user_dest_token, accs2.user_dest_token, "dest ATA must be deterministic");
        assert_eq!(accs1.amm_authority, accs2.amm_authority, "amm_authority must be deterministic");
        assert_eq!(accs1.market_vault_signer, accs2.market_vault_signer, "vault_signer must be deterministic");
    }

    // -----------------------------------------------------------------------
    // HttpPoolAccountProvider v4 mainnet e2e tests (Sprint 10, S10-1.4)
    //
    // Uses BOTH real mainnet fixtures loaded via MockSolanaRpc::with_account_raw:
    //   Pool:   58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2 (SOL/USDC v4)
    //   Market: 8BnEgHoWFysVcuFFX7QztDmzuH8r5ZFvyP3sYwn1XTh6 (Serum SOL/USDC)
    //
    // Source: Solana mainnet `getAccountInfo`, commitment=confirmed, 2026-04-24
    // RPC: https://api.mainnet-beta.solana.com (ADR 0003 bootstrap-only)
    //
    // All expected addresses cross-checked against Solscan on 2026-04-24.
    // -----------------------------------------------------------------------

    const MAINNET_V4_POOL: &str = "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2";
    const MAINNET_V4_POOL_BYTES: &[u8] = include_bytes!(
        "../../../../tests/fixtures/raydium_v4/58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2.bin"
    );

    const MAINNET_MARKET: &str = "8BnEgHoWFysVcuFFX7QztDmzuH8r5ZFvyP3sYwn1XTh6";
    const MAINNET_MARKET_BYTES: &[u8] = include_bytes!(
        "../../../../tests/fixtures/openbook_market/8BnEgHoWFysVcuFFX7QztDmzuH8r5ZFvyP3sYwn1XTh6.bin"
    );

    fn make_mainnet_v4_pool_raw() -> mg_onchain_token_registry::rpc::RawAccount {
        mg_onchain_token_registry::rpc::RawAccount {
            lamports: 4_781_558_248,
            owner: RAYDIUM_V4_PROGRAM_ID_PUBKEY,
            data: MAINNET_V4_POOL_BYTES.to_vec(),
            executable: false,
            rent_epoch: 0,
        }
    }

    fn make_mainnet_market_raw() -> mg_onchain_token_registry::rpc::RawAccount {
        mg_onchain_token_registry::rpc::RawAccount {
            lamports: 5_458_084,
            owner: SERUM_PROGRAM_ID,
            data: MAINNET_MARKET_BYTES.to_vec(),
            executable: false,
            rent_epoch: 0,
        }
    }

    #[tokio::test]
    async fn http_provider_v4_mainnet_fixtures_returns_correct_accounts() {
        // End-to-end: both fixtures fed through HttpPoolAccountProvider.v4_swap_accounts.
        // All 17 RaydiumV4SwapAccounts slots verified against Solscan (2026-04-24).
        use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;

        let pool_pubkey: Pubkey = MAINNET_V4_POOL.parse().unwrap();
        let market_pubkey: Pubkey = MAINNET_MARKET.parse().unwrap();
        let user = Pubkey::new_from_array([0xAB; 32]);

        let mock_rpc = MockSolanaRpc::default()
            .with_account_raw(MAINNET_V4_POOL, make_mainnet_v4_pool_raw())
            .with_account_raw(MAINNET_MARKET, make_mainnet_market_raw());
        let provider = HttpPoolAccountProvider::new(Arc::new(mock_rpc));

        let result = provider.v4_swap_accounts(&pool_pubkey, &user).await;
        assert!(
            result.is_ok(),
            "HttpPoolAccountProvider must succeed on real mainnet fixtures: {result:?}"
        );
        let accs = result.unwrap();

        // --- Pool-level slots ---
        assert_eq!(accs.amm_pool, pool_pubkey, "amm_pool must match input pool");
        assert_eq!(accs.user_owner, user, "user_owner must equal input user");
        assert_eq!(accs.market, market_pubkey, "market must match decoded market pubkey");

        // market_program: Serum (from pool state)
        let expected_serum: Pubkey =
            "srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX".parse().unwrap();
        assert_eq!(accs.market_program, expected_serum, "market_program must be Serum");

        // pool_coin_vault: from pool state @ offset 336
        let expected_pool_coin_vault: Pubkey =
            "DQyrAcCrDXQ7NeoqGgDCZwBvWDcYmFCjSb9JtteuvPpz".parse().unwrap();
        assert_eq!(accs.pool_coin_vault, expected_pool_coin_vault, "pool_coin_vault mismatch");

        // pool_pc_vault: from pool state @ offset 368
        let expected_pool_pc_vault: Pubkey =
            "HLmqeL62xR1QoZ1HKKbXRrdN1p3phKpxRMb2VVopvBBz".parse().unwrap();
        assert_eq!(accs.pool_pc_vault, expected_pool_pc_vault, "pool_pc_vault mismatch");

        // --- Market-level slots (from OpenBook market state) ---

        // market_coin_vault: from market state @ offset 117
        let expected_market_coin_vault: Pubkey =
            "CKxTHwM9fPMRRvZmFnFoqKNd9pQR21c5Aq9bh5h9oghX".parse().unwrap();
        assert_eq!(accs.market_coin_vault, expected_market_coin_vault, "market_coin_vault mismatch");

        // market_pc_vault: from market state @ offset 165
        let expected_market_pc_vault: Pubkey =
            "6A5NHCj1yF6urc9wZNe6Bcjj4LVszQNj5DwAWG97yzMu".parse().unwrap();
        assert_eq!(accs.market_pc_vault, expected_market_pc_vault, "market_pc_vault mismatch");

        // market_vault_signer: derived via create_program_address([market, nonce=1.to_le_bytes()], serum)
        let expected_vault_signer: Pubkey =
            "CTz5UMLQm2SRWHzQnU62Pi4yJqbNGjgRBHqqp6oDHfF7".parse().unwrap();
        assert_eq!(accs.market_vault_signer, expected_vault_signer, "market_vault_signer mismatch");

        // market_bids / market_asks / market_event_queue
        let expected_bids: Pubkey =
            "5jWUncPNBMZJ3sTHKmMLszypVkoRK6bfEQMQUHweeQnh".parse().unwrap();
        let expected_asks: Pubkey =
            "EaXdHx7x3mdGA38j5RSmKYSXMzAFzzUXCLNBEDXDn1d5".parse().unwrap();
        let expected_event_queue: Pubkey =
            "8CvwxZ9Db6XbLD46NZwwmVDZZRDy7eydFcAGkXKh9axa".parse().unwrap();
        assert_eq!(accs.market_bids, expected_bids, "market_bids mismatch");
        assert_eq!(accs.market_asks, expected_asks, "market_asks mismatch");
        assert_eq!(accs.market_event_queue, expected_event_queue, "market_event_queue mismatch");

        // amm_authority: PDA derived from v4 program, must not be zero
        assert_ne!(accs.amm_authority, Pubkey::default(), "amm_authority must not be zero");
        assert_ne!(accs.amm_authority, pool_pubkey, "amm_authority must differ from pool");

        // User ATAs: deterministic from (user_owner, SPL_TOKEN, mint)
        assert_ne!(accs.user_source_token, Pubkey::default(), "user_source_token must not be zero");
        assert_ne!(accs.user_dest_token, Pubkey::default(), "user_dest_token must not be zero");
        assert_ne!(
            accs.user_source_token, accs.user_dest_token,
            "source and dest ATAs must be distinct"
        );

        // amm_open_orders: from pool state open_orders field
        let expected_open_orders: Pubkey =
            "HmiHHzq4Fym9e1D4qzLS6LDDM3tNsCTBPDWHTLZ763jY".parse().unwrap();
        assert_eq!(accs.amm_open_orders, expected_open_orders, "amm_open_orders mismatch");

        // amm_target_orders: from pool state target_orders field
        let expected_target_orders: Pubkey =
            "CZza3Ej4Mc58MnxWA385itCC9jCo3L1D7zc3LKy1bZMR".parse().unwrap();
        assert_eq!(accs.amm_target_orders, expected_target_orders, "amm_target_orders mismatch");
    }

    #[tokio::test]
    async fn http_provider_v4_mainnet_fixtures_atas_are_deterministic() {
        // ATAs must be stable across two identical calls with the same user pubkey.
        use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;

        let pool_pubkey: Pubkey = MAINNET_V4_POOL.parse().unwrap();
        let user = Pubkey::new_from_array([0xCD; 32]);

        let make_provider = || {
            let mock = MockSolanaRpc::default()
                .with_account_raw(MAINNET_V4_POOL, make_mainnet_v4_pool_raw())
                .with_account_raw(MAINNET_MARKET, make_mainnet_market_raw());
            HttpPoolAccountProvider::new(Arc::new(mock))
        };

        let accs1 = make_provider().v4_swap_accounts(&pool_pubkey, &user).await.unwrap();
        let accs2 = make_provider().v4_swap_accounts(&pool_pubkey, &user).await.unwrap();

        assert_eq!(accs1.user_source_token, accs2.user_source_token, "source ATA must be deterministic");
        assert_eq!(accs1.user_dest_token, accs2.user_dest_token, "dest ATA must be deterministic");
        assert_eq!(accs1.market_vault_signer, accs2.market_vault_signer, "vault_signer must be deterministic");
        assert_eq!(accs1.amm_authority, accs2.amm_authority, "amm_authority must be deterministic");
    }
}
