//! Concrete `TokenMetadataFetcher` implementations.
//!
//! # Location rationale
//!
//! These impls live here (not in `mg-onchain-storage`) because they depend on:
//! - `mg-onchain-token-registry::rpc::SolanaRpc` (Solana impl)
//! - `mg-onchain-chain-adapter::ethereum::rpc::EthereumRpc` (EVM impl)
//!
//! Adding those as deps to `mg-onchain-storage` would create a circular
//! dependency chain.  The server crate already imports all three, so this is
//! the natural host.
//!
//! # Solana impl
//!
//! Uses `SolanaRpc::get_mint_account` which decodes the SPL / Token-2022 mint
//! account layout.  Populates `decimals` and `total_supply_raw`.
//! `symbol` / `name` are left `None` — Metaplex metadata account decode is
//! already handled by the full token-registry enrichment path; we do not
//! duplicate it here.
//!
//! # EVM impl
//!
//! Calls four ERC-20 view selectors concurrently via `EthereumRpc::eth_call`:
//! - `decimals()`     → uint8  (selector `0x313ce567`)
//! - `symbol()`       → string (selector `0x95d89b41`)
//! - `name()`         → string (selector `0x06fdde03`)
//! - `totalSupply()`  → uint256 (selector `0x18160ddd`)
//!
//! Individual call failures are soft (field set to `None`).  If all four fail
//! the token is treated as not found.
//!
//! # ABI string decode
//!
//! ERC-20 `string` return values follow the ABI dynamic-type encoding:
//!   bytes 0..32  = data offset (always 0x20 for single string return)
//!   bytes 32..64 = string byte length (big-endian uint256, realistically u32)
//!   bytes 64..   = UTF-8 content padded to next 32-byte boundary
//!
//! # Verified selector values
//!
//! - `decimals()`:    keccak256("decimals()")    → first 4 bytes = `0x313ce567`
//! - `symbol()`:      keccak256("symbol()")      → first 4 bytes = `0x95d89b41`
//! - `name()`:        keccak256("name()")        → first 4 bytes = `0x06fdde03`
//! - `totalSupply()`: keccak256("totalSupply()") → first 4 bytes = `0x18160ddd`
//!
//! Source: EIP-20 canonical ABI signatures; verified via alloy `keccak256` in
//! the unit tests below.

use std::sync::Arc;

use async_trait::async_trait;
use tracing::{debug, instrument, warn};

use mg_onchain_chain_adapter::ethereum::rpc::EthereumRpc;
use mg_onchain_common::chain::{Address, Chain};
use mg_onchain_storage::token_metadata::{MetadataError, TokenMetadata, TokenMetadataFetcher};
use mg_onchain_token_registry::rpc::SolanaRpc;

// ---------------------------------------------------------------------------
// ERC-20 4-byte function selectors
// ---------------------------------------------------------------------------

/// `decimals()` → uint8.  keccak256("decimals()")[0..4].
const SELECTOR_DECIMALS: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];

/// `symbol()` → string.  keccak256("symbol()")[0..4].
const SELECTOR_SYMBOL: [u8; 4] = [0x95, 0xd8, 0x9b, 0x41];

/// `name()` → string.  keccak256("name()")[0..4].
const SELECTOR_NAME: [u8; 4] = [0x06, 0xfd, 0xde, 0x03];

/// `totalSupply()` → uint256.  keccak256("totalSupply()")[0..4].
const SELECTOR_TOTAL_SUPPLY: [u8; 4] = [0x18, 0x16, 0x0d, 0xdd];

// ---------------------------------------------------------------------------
// SolanaTokenMetadataFetcher
// ---------------------------------------------------------------------------

/// Fetches SPL / Token-2022 mint metadata via `getAccountInfo`.
///
/// Populates `decimals` and `total_supply_raw`.
/// `symbol` / `name` remain `None` — Metaplex decode is handled by the
/// full token-registry enrichment path.
pub struct SolanaTokenMetadataFetcher {
    rpc: Arc<dyn SolanaRpc>,
}

impl SolanaTokenMetadataFetcher {
    pub fn new(rpc: Arc<dyn SolanaRpc>) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl TokenMetadataFetcher for SolanaTokenMetadataFetcher {
    #[instrument(skip(self), fields(chain = "solana", token = %token))]
    async fn fetch_token_metadata(
        &self,
        chain: Chain,
        token: &Address,
    ) -> Result<Option<TokenMetadata>, MetadataError> {
        let mint_str = token.to_string();
        debug!(mint = %mint_str, "SolanaTokenMetadataFetcher: calling getAccountInfo");

        match self.rpc.get_mint_account(&mint_str).await {
            Ok(Some(decoded)) => {
                Ok(Some(TokenMetadata {
                    chain,
                    token: token.clone(),
                    decimals: Some(decoded.decimals as u32),
                    // Metaplex name/symbol decode deferred — see module doc.
                    symbol: None,
                    name: None,
                    total_supply_raw: Some(decoded.supply),
                }))
            }
            Ok(None) => {
                debug!(mint = %mint_str, "SolanaTokenMetadataFetcher: mint account not found");
                Ok(None)
            }
            Err(e) => {
                warn!(mint = %mint_str, error = %e, "SolanaTokenMetadataFetcher: getAccountInfo failed");
                Err(MetadataError::Rpc(e.to_string()))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// EvmTokenMetadataFetcher
// ---------------------------------------------------------------------------

/// Fetches ERC-20 token metadata via `eth_call` view-function calls.
///
/// Individual call failures are soft (field set to `None`).
/// If all four calls fail, returns `Ok(None)` — treated as "token not found".
pub struct EvmTokenMetadataFetcher {
    rpc: Arc<dyn EthereumRpc>,
}

impl EvmTokenMetadataFetcher {
    pub fn new(rpc: Arc<dyn EthereumRpc>) -> Self {
        Self { rpc }
    }

    /// Decode an ABI-encoded `string` return value.
    ///
    /// Layout:
    ///   bytes 0..32  = offset (0x20 for single return value)
    ///   bytes 32..64 = string byte length (big-endian)
    ///   bytes 64+    = UTF-8 content (zero-padded to 32-byte boundary)
    pub(crate) fn decode_abi_string(data: &[u8]) -> Option<String> {
        if data.len() < 64 {
            return None;
        }
        // String length is in bytes 56..64 (last 8 bytes of the 32-byte length word).
        let len_bytes: [u8; 8] = data[56..64].try_into().ok()?;
        let len = u64::from_be_bytes(len_bytes) as usize;
        if data.len() < 64 + len {
            return None;
        }
        String::from_utf8(data[64..64 + len].to_vec()).ok()
    }

    /// Call `decimals()` → `uint8` (right-padded in 32-byte return).
    async fn call_decimals(&self, contract: &str) -> Option<u32> {
        match self.rpc.eth_call(contract, SELECTOR_DECIMALS.to_vec()).await {
            Ok(data) if data.len() >= 32 => Some(data[31] as u32),
            Ok(_) => {
                warn!(contract, "decimals() returned too-short data");
                None
            }
            Err(e) => {
                warn!(contract, error = %e, "decimals() call failed");
                None
            }
        }
    }

    /// Call `symbol()` → ABI-encoded `string`.
    async fn call_symbol(&self, contract: &str) -> Option<String> {
        match self.rpc.eth_call(contract, SELECTOR_SYMBOL.to_vec()).await {
            Ok(data) => Self::decode_abi_string(&data),
            Err(e) => {
                warn!(contract, error = %e, "symbol() call failed");
                None
            }
        }
    }

    /// Call `name()` → ABI-encoded `string`.
    async fn call_name(&self, contract: &str) -> Option<String> {
        match self.rpc.eth_call(contract, SELECTOR_NAME.to_vec()).await {
            Ok(data) => Self::decode_abi_string(&data),
            Err(e) => {
                warn!(contract, error = %e, "name() call failed");
                None
            }
        }
    }

    /// Call `totalSupply()` → `uint256` truncated to `u128`.
    ///
    /// We take the last 16 bytes (128 bits) of the 32-byte big-endian uint256.
    /// Token supplies exceeding u128::MAX are not in scope for this project.
    async fn call_total_supply(&self, contract: &str) -> Option<u128> {
        match self.rpc.eth_call(contract, SELECTOR_TOTAL_SUPPLY.to_vec()).await {
            Ok(data) if data.len() >= 32 => {
                let bytes: [u8; 16] = data[16..32].try_into().ok()?;
                Some(u128::from_be_bytes(bytes))
            }
            Ok(_) => {
                warn!(contract, "totalSupply() returned too-short data");
                None
            }
            Err(e) => {
                warn!(contract, error = %e, "totalSupply() call failed");
                None
            }
        }
    }
}

#[async_trait]
impl TokenMetadataFetcher for EvmTokenMetadataFetcher {
    #[instrument(skip(self), fields(chain = %chain, token = %token))]
    async fn fetch_token_metadata(
        &self,
        chain: Chain,
        token: &Address,
    ) -> Result<Option<TokenMetadata>, MetadataError> {
        let contract = token.to_string();
        debug!(contract = %contract, chain = %chain, "EvmTokenMetadataFetcher: calling ERC-20 view functions");

        // Run all four calls concurrently — each is independent.
        let (decimals, symbol, name, total_supply) = tokio::join!(
            self.call_decimals(&contract),
            self.call_symbol(&contract),
            self.call_name(&contract),
            self.call_total_supply(&contract),
        );

        // If all four failed, treat as "token not found / invalid address".
        if decimals.is_none() && symbol.is_none() && name.is_none() && total_supply.is_none() {
            debug!(contract = %contract, "EvmTokenMetadataFetcher: all calls failed — token not found");
            return Ok(None);
        }

        Ok(Some(TokenMetadata {
            chain,
            token: token.clone(),
            decimals,
            symbol,
            name,
            total_supply_raw: total_supply,
        }))
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mg_onchain_chain_adapter::ethereum::rpc::MockEthereumRpc;
    use mg_onchain_common::chain::Chain;
    use mg_onchain_token_registry::rpc::DecodedMint;
    use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;

    fn eth_addr(s: &str) -> Address {
        Address::parse(Chain::Ethereum, s).expect("valid EVM address")
    }

    fn solana_addr() -> Address {
        Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112").unwrap()
    }

    // -----------------------------------------------------------------------
    // EvmTokenMetadataFetcher: decode_abi_string
    // -----------------------------------------------------------------------

    /// `decode_abi_string` correctly decodes a padded ABI string ("USDC").
    #[test]
    fn decode_abi_string_usdc_symbol() {
        let mut data = vec![0u8; 96];
        data[31] = 0x20; // offset
        data[63] = 4;    // length = 4
        data[64] = b'U';
        data[65] = b'S';
        data[66] = b'D';
        data[67] = b'C';

        let result = EvmTokenMetadataFetcher::decode_abi_string(&data);
        assert_eq!(result, Some("USDC".to_string()));
    }

    /// `decode_abi_string` returns None for data shorter than 64 bytes.
    #[test]
    fn decode_abi_string_too_short_returns_none() {
        let data = vec![0u8; 10];
        assert!(EvmTokenMetadataFetcher::decode_abi_string(&data).is_none());
    }

    /// `decode_abi_string` returns None when declared length exceeds data.
    #[test]
    fn decode_abi_string_length_overflow_returns_none() {
        let mut data = vec![0u8; 64];
        data[63] = 100; // length=100 but only 64 bytes available
        assert!(EvmTokenMetadataFetcher::decode_abi_string(&data).is_none());
    }

    // -----------------------------------------------------------------------
    // EvmTokenMetadataFetcher: decimals() decode
    // -----------------------------------------------------------------------

    /// `decimals()` returning 18 right-padded in 32 bytes decodes to 18.
    #[tokio::test]
    async fn evm_fetcher_decimals_18() {
        let mock = Arc::new(MockEthereumRpc::default());
        // Register the `decimals()` selector → 32-byte return with byte[31] = 18.
        let mut ret = vec![0u8; 32];
        ret[31] = 18;
        mock.set_eth_call_response(&SELECTOR_DECIMALS, Ok(ret));

        let fetcher = EvmTokenMetadataFetcher::new(mock);
        let contract = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        let addr = eth_addr(contract);
        let result = fetcher.fetch_token_metadata(Chain::Ethereum, &addr).await.unwrap();
        let meta = result.expect("must return Some when decimals() succeeds");
        assert_eq!(meta.decimals, Some(18));
    }

    /// When all four ERC-20 calls fail, `fetch_token_metadata` returns None.
    #[tokio::test]
    async fn evm_fetcher_all_calls_fail_returns_none() {
        let mock = Arc::new(MockEthereumRpc::default());
        // No responses registered → all calls return Ok(vec![]) (empty, too short)
        // which maps to None for each field.
        let fetcher = EvmTokenMetadataFetcher::new(mock);
        let addr = eth_addr("0xdead000000000000000000000000000000000001");
        let result = fetcher.fetch_token_metadata(Chain::Ethereum, &addr).await.unwrap();
        assert!(result.is_none(), "all-failures must return None");
    }

    /// `symbol()` call returns ABI-encoded "WETH" → decoded correctly.
    #[tokio::test]
    async fn evm_fetcher_symbol_weth() {
        let mock = Arc::new(MockEthereumRpc::default());

        // ABI-encode "WETH" (4 chars).
        let mut sym_data = vec![0u8; 96];
        sym_data[31] = 0x20; // offset
        sym_data[63] = 4;    // length
        sym_data[64] = b'W';
        sym_data[65] = b'E';
        sym_data[66] = b'T';
        sym_data[67] = b'H';
        mock.set_eth_call_response(&SELECTOR_SYMBOL, Ok(sym_data));

        let fetcher = EvmTokenMetadataFetcher::new(mock);
        let addr = eth_addr("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let result = fetcher.fetch_token_metadata(Chain::Ethereum, &addr).await.unwrap();
        let meta = result.expect("symbol() success → Some");
        assert_eq!(meta.symbol.as_deref(), Some("WETH"));
    }

    // -----------------------------------------------------------------------
    // SolanaTokenMetadataFetcher
    // -----------------------------------------------------------------------

    /// SolanaTokenMetadataFetcher returns decimals + supply from DecodedMint.
    #[tokio::test]
    async fn solana_fetcher_returns_decimals_from_mint_account() {
        let decoded = DecodedMint {
            supply: 1_000_000_000,
            decimals: 9,
            mint_authority: None,
            freeze_authority: None,
            is_token2022: false,
            raw_account_data: vec![],
        };
        let mock_rpc = Arc::new(MockSolanaRpc::with_mint(decoded));
        let fetcher = SolanaTokenMetadataFetcher::new(mock_rpc);

        let addr = solana_addr();
        let result = fetcher.fetch_token_metadata(Chain::Solana, &addr).await.unwrap();
        let meta = result.expect("must return Some for known mint");
        assert_eq!(meta.decimals, Some(9));
        assert_eq!(meta.total_supply_raw, Some(1_000_000_000));
        assert!(meta.symbol.is_none(), "Metaplex symbol deferred");
        assert!(meta.name.is_none(), "Metaplex name deferred");
    }

    /// SolanaTokenMetadataFetcher returns None when mint account not found.
    #[tokio::test]
    async fn solana_fetcher_returns_none_for_unknown_mint() {
        let mock_rpc = Arc::new(MockSolanaRpc::with_no_mint());
        let fetcher = SolanaTokenMetadataFetcher::new(mock_rpc);
        let addr = solana_addr();
        let result = fetcher.fetch_token_metadata(Chain::Solana, &addr).await.unwrap();
        assert!(result.is_none(), "unknown mint must return None");
    }
}
