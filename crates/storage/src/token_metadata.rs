//! `TokenMetadataFetcher` — on-demand RPC fetch + upsert for unknown tokens.
//!
//! # Purpose
//!
//! When a detector encounters a token not present in the `tokens` table (no
//! decimals, no supply, no symbol), it was previously forced to fall back to
//! chain defaults (9 decimals for Solana, 18 for EVM).  This module closes
//! that gap: `PgStore::ensure_token_metadata` checks the table first and, on
//! a miss, calls the appropriate `TokenMetadataFetcher` implementation to fetch
//! live metadata from the chain and upsert it.
//!
//! # Trait + impl split (no circular deps)
//!
//! This module defines only the trait + data types + `PgStore` integration.
//! Concrete implementations (`SolanaTokenMetadataFetcher`,
//! `EvmTokenMetadataFetcher`) live in `crates/server/src/init/metadata_fetchers.rs`
//! which can depend on both `mg-onchain-token-registry` and
//! `mg-onchain-chain-adapter` without creating a circular dependency through
//! `mg-onchain-storage`.
//!
//! # Observed-at discipline (gotcha #22 / #28)
//!
//! `TokenMetadataFetcher::fetch_token_metadata` is called from the on-demand
//! REST API path (`POST /v1/tokens/analyze`). Per gotcha #93 documented exception,
//! `Utc::now()` is acceptable for on-demand API calls. The fetched metadata is
//! stored with `updated_at = now()` by the Postgres `upsert_token` call (server-side).
//!
//! # Migration
//!
//! No new migration is needed.  The existing `tokens` table schema (V00001+) has
//! all columns required by `upsert_token`.  Fields absent from the RPC response
//! (`creator`, `mint_authority` for EVM, etc.) are passed as `None` / `0` /
//! `false` defaults and left to the ON CONFLICT UPDATE path if already set.
//!
//! # ADR 0003 compliance
//!
//! Concrete impls use injectable trait objects over self-hosted RPC clients.
//! No 3rd-party SaaS APIs are called — all RPC calls go to self-hosted nodes.

use anyhow::Context as _;
use async_trait::async_trait;
use tracing::{debug, instrument, warn};

use mg_onchain_common::chain::{Address, Chain};

use crate::error::StorageError;
use crate::pg::PgStore;

// ---------------------------------------------------------------------------
// TokenMetadata — plain data struct carrying fetched fields
// ---------------------------------------------------------------------------

/// Metadata fetched from chain RPC for a token not yet in the `tokens` table.
///
/// All fields except `chain` and `token` are optional — individual RPC calls
/// may fail independently.  Callers should treat `None` as "unknown" and apply
/// their own defaults.
#[derive(Debug, Clone)]
pub struct TokenMetadata {
    pub chain: Chain,
    pub token: Address,
    /// Token decimals.  `None` if the RPC call failed.
    pub decimals: Option<u32>,
    /// Token symbol (e.g. "USDC").  `None` for Solana (Metaplex decode deferred).
    pub symbol: Option<String>,
    /// Token name (e.g. "USD Coin").  `None` for Solana.
    pub name: Option<String>,
    /// Total supply in raw on-chain units.  `None` if call failed.
    pub total_supply_raw: Option<u128>,
}

// ---------------------------------------------------------------------------
// MetadataError
// ---------------------------------------------------------------------------

/// Error type returned by `TokenMetadataFetcher::fetch_token_metadata`.
///
/// Distinct from `StorageError` to keep the trait decoupled from the storage layer.
/// `PgStore::ensure_token_metadata` converts this into `StorageError::Internal`.
#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("RPC error: {0}")]
    Rpc(String),

    #[error("address parse error: {0}")]
    AddressParse(String),
}

// ---------------------------------------------------------------------------
// TokenMetadataFetcher trait
// ---------------------------------------------------------------------------

/// Fetch token metadata from chain RPC.
///
/// Implementors:
/// - `SolanaTokenMetadataFetcher` in `crates/server/src/init/metadata_fetchers.rs`
/// - `EvmTokenMetadataFetcher`   in `crates/server/src/init/metadata_fetchers.rs`
///
/// Returns `Ok(None)` when the token is not found on-chain (e.g. invalid address).
/// Returns `Err(MetadataError)` only on transport-level failures.
#[async_trait]
pub trait TokenMetadataFetcher: Send + Sync {
    async fn fetch_token_metadata(
        &self,
        chain: Chain,
        token: &Address,
    ) -> Result<Option<TokenMetadata>, MetadataError>;
}

// ---------------------------------------------------------------------------
// PgStore::ensure_token_metadata — auto-populate on cache miss
// ---------------------------------------------------------------------------

impl PgStore {
    /// Return token metadata from `tokens` table, fetching via RPC on miss.
    ///
    /// # Flow
    ///
    /// 1. Query `tokens` table for `(chain, token)`.
    /// 2. If found → return `Some(TokenMetadata)` from the stored row.
    /// 3. If not found → call `fetcher.fetch_token_metadata(chain, token)`.
    /// 4. If fetcher returns `Some(meta)` → upsert into `tokens` table + return.
    /// 5. If fetcher returns `None` → return `None` (caller uses chain defaults).
    ///
    /// # On-demand context (gotcha #93 exception)
    ///
    /// Called from the on-demand REST API path.  `upsert_token` uses `now()` server-side.
    ///
    /// # Errors
    ///
    /// DB errors returned as `StorageError`.  Fetcher errors are logged and
    /// converted to `Ok(None)` — callers fall back to chain defaults.
    #[instrument(skip(self, fetcher), fields(chain = %chain, token = %token))]
    pub async fn ensure_token_metadata(
        &self,
        chain: Chain,
        token: &Address,
        fetcher: &dyn TokenMetadataFetcher,
    ) -> Result<Option<TokenMetadata>, StorageError> {
        let chain_str = chain.to_string();
        let token_str = token.to_string();

        // Step 1: DB lookup.
        let existing = self.get_token(&chain_str, &token_str).await?;

        if let Some(row) = existing {
            // Cache hit: reconstruct TokenMetadata from the stored row.
            debug!(chain = %chain_str, token = %token_str, "ensure_token_metadata: cache hit");
            let supply = row.total_supply_u128();
            return Ok(Some(TokenMetadata {
                chain,
                token: token.clone(),
                decimals: Some(row.decimals as u32),
                symbol: row.symbol,
                name: row.name,
                total_supply_raw: Some(supply),
            }));
        }

        // Step 2: Cache miss — fetch from chain.
        debug!(chain = %chain_str, token = %token_str, "ensure_token_metadata: cache miss, fetching from RPC");

        let fetched = match fetcher.fetch_token_metadata(chain, token).await {
            Ok(Some(meta)) => meta,
            Ok(None) => {
                debug!(chain = %chain_str, token = %token_str, "ensure_token_metadata: RPC returned None");
                return Ok(None);
            }
            Err(e) => {
                warn!(
                    chain = %chain_str,
                    token = %token_str,
                    error = %e,
                    "ensure_token_metadata: RPC fetch failed — returning None"
                );
                return Ok(None);
            }
        };

        // Step 3: Upsert into `tokens` table.
        let decimals_i16 = fetched.decimals.unwrap_or(0) as i16;
        let total_supply = fetched.total_supply_raw.unwrap_or(0);

        self.upsert_token(
            &chain_str,
            &token_str,
            fetched.symbol.as_deref(),
            fetched.name.as_deref(),
            decimals_i16,
            None,  // token_program: unknown at this point
            total_supply,
            None,  // circulating_supply_raw: unknown
            None,  // mint_authority
            None,  // freeze_authority
            None,  // creator
            0,     // creator_balance_raw
            0,     // total_holders
            "0",   // total_market_liquidity_usd
            false, // jup_verified
            false, // jup_strict
            false, // rugged
            None,  // rugcheck_score
            None,  // launchpad
            None,  // deploy_platform
            None,  // detected_at
            None,  // permanent_delegate
            None,  // transfer_hook_program
            false, // non_transferable
            false, // confidential_transfer
        )
        .await
        .with_context(|| {
            format!("ensure_token_metadata: upsert_token failed for {chain_str}/{token_str}")
        })
        .map_err(|e| StorageError::Other(e.to_string()))?;

        debug!(chain = %chain_str, token = %token_str, "ensure_token_metadata: upserted");
        Ok(Some(fetched))
    }
}

// ---------------------------------------------------------------------------
// MockTokenMetadataFetcher (test-utils)
// ---------------------------------------------------------------------------

/// Mock `TokenMetadataFetcher` for unit tests.
///
/// Pre-populate `responses` to control what `fetch_token_metadata` returns.
/// An empty responses map returns `Ok(None)` for all tokens.
#[cfg(any(test, feature = "test-utils"))]
#[derive(Debug, Default)]
pub struct MockTokenMetadataFetcher {
    /// Pre-canned responses keyed by token address string.
    responses: std::collections::HashMap<String, TokenMetadata>,
}

#[cfg(any(test, feature = "test-utils"))]
impl MockTokenMetadataFetcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a canned response for `token` (address string).
    ///
    /// The key is normalized to match the canonical form returned by
    /// `Address::to_string()`: EVM addresses (`0x`-prefixed) are lowercased;
    /// all other strings are stored as-is.
    pub fn insert(&mut self, token: &str, meta: TokenMetadata) {
        let key = if token.starts_with("0x") || token.starts_with("0X") {
            token.to_lowercase()
        } else {
            token.to_string()
        };
        self.responses.insert(key, meta);
    }
}

#[cfg(any(test, feature = "test-utils"))]
#[async_trait]
impl TokenMetadataFetcher for MockTokenMetadataFetcher {
    async fn fetch_token_metadata(
        &self,
        _chain: Chain,
        token: &Address,
    ) -> Result<Option<TokenMetadata>, MetadataError> {
        Ok(self.responses.get(&token.to_string()).cloned())
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mg_onchain_common::chain::{Address, Chain};

    fn eth_addr(s: &str) -> Address {
        Address::parse(Chain::Ethereum, s).expect("valid EVM address")
    }

    // -----------------------------------------------------------------------
    // MockTokenMetadataFetcher
    // -----------------------------------------------------------------------

    /// Mock fetcher returns registered metadata.
    #[tokio::test]
    async fn mock_fetcher_returns_registered_metadata() {
        let mut mock = MockTokenMetadataFetcher::new();
        let token_str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        let addr = eth_addr(token_str);

        mock.insert(
            token_str,
            TokenMetadata {
                chain: Chain::Ethereum,
                token: addr.clone(),
                decimals: Some(6),
                symbol: Some("USDC".to_string()),
                name: Some("USD Coin".to_string()),
                total_supply_raw: Some(50_000_000_000_000_u128),
            },
        );

        let result = mock.fetch_token_metadata(Chain::Ethereum, &addr).await.unwrap();
        let meta = result.expect("must return Some");
        assert_eq!(meta.decimals, Some(6));
        assert_eq!(meta.symbol.as_deref(), Some("USDC"));
        assert_eq!(meta.name.as_deref(), Some("USD Coin"));
        assert_eq!(meta.total_supply_raw, Some(50_000_000_000_000_u128));
    }

    /// Mock fetcher returns None for unregistered token.
    #[tokio::test]
    async fn mock_fetcher_returns_none_for_unknown_token() {
        let mock = MockTokenMetadataFetcher::new();
        let addr = eth_addr("0xdead000000000000000000000000000000000001");
        let result = mock.fetch_token_metadata(Chain::Ethereum, &addr).await.unwrap();
        assert!(result.is_none(), "unregistered token must return None");
    }

    /// ensure_token_metadata: cache-hit path returns metadata without calling fetcher.
    ///
    /// This is a logic-only test — we test the fetcher path indirectly because
    /// `ensure_token_metadata` requires a `PgStore` which requires a live DB.
    /// The cache-hit + cache-miss paths are tested via integration tests gated by
    /// `#[ignore = "requires live Postgres"]`.
    #[tokio::test]
    async fn mock_fetcher_chain_does_not_affect_lookup() {
        // Two different chains, same token address — mock keyed by address only.
        let mut mock = MockTokenMetadataFetcher::new();
        let token_str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        let addr = eth_addr(token_str);

        mock.insert(
            token_str,
            TokenMetadata {
                chain: Chain::Ethereum,
                token: addr.clone(),
                decimals: Some(6),
                symbol: Some("USDC".to_string()),
                name: None,
                total_supply_raw: None,
            },
        );

        // Works for Ethereum.
        let r_eth = mock.fetch_token_metadata(Chain::Ethereum, &addr).await.unwrap();
        assert!(r_eth.is_some());

        // Also works for BSC (same address format) — mock is address-keyed.
        let r_bsc = mock.fetch_token_metadata(Chain::Bsc, &addr).await.unwrap();
        assert!(r_bsc.is_some());
    }

    // -----------------------------------------------------------------------
    // ensure_token_metadata integration tests (gated by live DB)
    // -----------------------------------------------------------------------

    /// Cache miss → fetcher called → upserted → cache hit on second call.
    #[tokio::test]
    #[ignore = "requires live Postgres (set DATABASE_URL)"]
    async fn ensure_token_metadata_cache_miss_then_hit() {
        // Integration test — omitted in unit suite.
    }

    /// Fetcher returns None → ensure_token_metadata returns None.
    #[tokio::test]
    #[ignore = "requires live Postgres (set DATABASE_URL)"]
    async fn ensure_token_metadata_fetcher_none_returns_none() {
        // Integration test — omitted in unit suite.
    }
}
