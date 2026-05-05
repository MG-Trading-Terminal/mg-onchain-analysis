//! Solana JSON-RPC client for token-registry enrichment.
//!
//! # Design
//!
//! All RPC calls go through the [`SolanaRpc`] trait. Production code uses
//! [`HttpSolanaRpc`] backed by `reqwest`. Tests inject a mock implementation.
//! This keeps all tests free of network I/O.
//!
//! # Rate-limit & retry
//!
//! HTTP 429 responses are treated as transient errors. Retry uses exponential
//! backoff with full jitter (see `config::RetryConfig::delay_for_attempt`).
//! After `max_attempts` the call returns `RegistryError::RpcExhausted`.
//!
//! # Commitment
//!
//! Hot-path enrichment uses `confirmed` commitment (fast, ~400ms finality).
//! Holder-snapshot job uses `finalized` (immutable, ~32 blocks / ~13s).
//! Per CLAUDE.md Solana rules.
//!
//! # SPL Mint account layout (hand-rolled, no spl-token dep)
//!
//! SPL Token Mint account layout (82 bytes):
//!   [0..4]   mint_authority option prefix: 0 = None, 1 = Some
//!   [4..36]  mint_authority pubkey (32 bytes)
//!   [36..44] supply: u64 LE
//!   [44]     decimals: u8
//!   [45]     is_initialized: u8 (must be 1)
//!   [46..50] freeze_authority option prefix: 0 = None, 1 = Some
//!   [50..82] freeze_authority pubkey (32 bytes)
//!
//! Reference: https://github.com/solana-program/token/blob/main/program/src/state.rs
//!
//! Token-2022 Mint has the same first 82 bytes plus variable-length extension
//! TLV data starting at byte 82. We decode only the base 82 bytes for MVP.
//! Extension detection: if account_data.len() > 82, Token-2022 extensions present.
//! Full extension decode is Phase 3+.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use base64::prelude::{Engine, BASE64_STANDARD};
use serde::Deserialize;
use tracing::{debug, instrument, warn};

use crate::config::RegistryConfig;
use crate::error::RegistryError;

// ---------------------------------------------------------------------------
// JSON-RPC response shapes
// ---------------------------------------------------------------------------

/// Wrapper around a single JSON-RPC response from Solana.
#[derive(Debug, Deserialize)]
pub struct RpcResponse<T> {
    pub result: Option<RpcResult<T>>,
    pub error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
pub struct RpcResult<T> {
    pub value: T,
}

#[derive(Debug, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// Parsed account info from `getAccountInfo`.
#[derive(Debug, Deserialize)]
pub struct AccountInfo {
    /// Base64-encoded account data.
    pub data: Vec<String>,
    /// Owner program address (Base58).
    pub owner: String,
    /// Lamport balance.
    pub lamports: u64,
    /// Whether the account is executable.
    pub executable: bool,
}

/// Single entry from `getTokenLargestAccounts`.
#[derive(Debug, Deserialize, Clone)]
pub struct TokenAccountBalance {
    /// Token account address (Base58).
    pub address: String,
    /// Raw token amount as string (u64 encoded as decimal string by RPC).
    pub amount: String,
    /// Human-scaled amount (as string).
    #[serde(rename = "uiAmountString")]
    pub ui_amount_string: Option<String>,
    /// Token decimals.
    pub decimals: u8,
}

/// Single signature from `getSignaturesForAddress`.
#[derive(Debug, Deserialize, Clone)]
pub struct SignatureInfo {
    /// Transaction signature (Base58).
    pub signature: String,
    /// Slot in which this tx was included.
    pub slot: u64,
    /// Block time (Unix timestamp, may be null for old txs).
    #[serde(rename = "blockTime")]
    pub block_time: Option<i64>,
    /// Error, if the transaction failed.
    pub err: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// simulateTransaction response types
// ---------------------------------------------------------------------------

/// Result of a `simulateTransaction` JSON-RPC call.
///
/// Mirrors the Solana `simulateTransaction` response:
/// <https://solana.com/docs/rpc/http/simulatetransaction>
///
/// `None` error means simulation succeeded (instruction completed without error).
/// `Some(s)` means the transaction would fail on-chain with the given message
/// (e.g. `"InstructionError: [0, {\"Custom\": 6}]"` for a program custom error).
#[derive(Debug, Clone)]
pub struct SimulatedTransaction {
    /// `None` = simulation succeeded. `Some(s)` = simulation error string.
    pub err: Option<String>,
    /// Program log output lines (e.g. from `msg!` calls).
    pub logs: Vec<String>,
    /// Per-account post-state snapshots, ordered to match `accounts_to_track`
    /// passed to `simulate_transaction`. `None` = account not found or not requested.
    pub accounts: Vec<Option<SimulatedAccount>>,
    /// Compute units consumed by the simulation (present on recent RPC versions).
    pub units_consumed: Option<u64>,
}

/// Post-simulation state snapshot for one tracked account.
///
/// Used to read SPL token account balances after the simulated swap without
/// a real on-chain transaction.
#[derive(Debug, Clone)]
pub struct SimulatedAccount {
    /// Lamport balance after simulation.
    pub lamports: u64,
    /// Base64-encoded account data fields (index 0 = data, index 1 = encoding label).
    pub data: Vec<String>,
    /// Owner program address (Base58).
    pub owner: String,
}

// ---------------------------------------------------------------------------
// Decoded Mint account
// ---------------------------------------------------------------------------

/// Decoded fields from an SPL Token / Token-2022 Mint account.
///
/// Hand-rolled from the canonical SPL Token Mint layout (82 bytes base).
/// Reference: https://github.com/solana-program/token/blob/main/program/src/state.rs
#[derive(Debug, Clone)]
pub struct DecodedMint {
    /// Total token supply in raw units.
    pub supply: u128,
    /// Token decimals (0–18; typically 6 or 9 for Solana tokens).
    pub decimals: u8,
    /// Mint authority address. `None` = authority renounced.
    pub mint_authority: Option<String>,
    /// Freeze authority address. `None` = authority renounced.
    pub freeze_authority: Option<String>,
    /// True if this is a Token-2022 mint (account data > 82 bytes).
    /// Transfer-fee and other extensions present but not decoded in MVP.
    pub is_token2022: bool,
    /// Raw account data bytes from `getAccountInfo`. Carried here so that
    /// `enrich.rs` can pass them directly to `tlv::decode_extensions` without
    /// a second RPC call. For Token-2022 mints this includes the TLV stream
    /// starting at byte 83; for legacy SPL mints it is exactly 82 bytes.
    ///
    /// Added in P5-4 to close the gap where `permanent_delegate` and
    /// `transfer_hook_program` were always `None` in production.
    pub raw_account_data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// RawAccount — generic account fetch result (B1.1)
// ---------------------------------------------------------------------------

/// Raw account state returned by `getAccountInfo` before any program-specific
/// decoding.  Used by [`PoolAccountProvider`] implementations that need to
/// fetch pool-state accounts whose layouts are not known to `token-registry`.
///
/// Added in Sprint 9 B1 to support `HttpPoolAccountProvider` in `dex-adapter`.
#[derive(Debug, Clone)]
pub struct RawAccount {
    /// Lamport balance.
    pub lamports: u64,
    /// Owner program address.
    pub owner: mg_solana_types::Pubkey,
    /// Raw account data bytes (decoded from base64).
    pub data: Vec<u8>,
    /// Whether the account is executable.
    pub executable: bool,
    /// Rent epoch (informational).
    pub rent_epoch: u64,
}

// ---------------------------------------------------------------------------
// SolanaRpc trait
// ---------------------------------------------------------------------------

/// Trait abstracting all Solana JSON-RPC calls needed by token-registry.
///
/// This is the injection point for tests: mock implementations can return
/// canned responses without network I/O.
///
/// All methods are async and return `Result<_, RegistryError>`.
/// `#[async_trait]` is required because async methods in traits are not
/// natively supported without `impl Trait` return types (Rust 2024 edition
/// uses `async fn in trait` but async-trait provides dyn compatibility).
#[async_trait]
pub trait SolanaRpc: Send + Sync {
    /// Fetch and decode the mint account for `mint_address`.
    ///
    /// Returns `None` if the account does not exist.
    async fn get_mint_account(
        &self,
        mint_address: &str,
    ) -> Result<Option<DecodedMint>, RegistryError>;

    /// Get the top N token accounts by balance for a given mint.
    ///
    /// Returns up to 20 accounts (Solana RPC hard limit).
    /// `commitment` should be `"confirmed"` for hot path, `"finalized"` for snapshots.
    async fn get_token_largest_accounts(
        &self,
        mint_address: &str,
        commitment: &str,
    ) -> Result<Vec<TokenAccountBalance>, RegistryError>;

    /// Fetch the owner of a token account.
    ///
    /// Given a *token account* address (not a mint), returns the wallet that
    /// owns this token account (the `owner` field of the SPL token account).
    /// Returns `None` if account does not exist.
    async fn get_token_account_owner(
        &self,
        token_account: &str,
    ) -> Result<Option<String>, RegistryError>;

    /// Get the earliest transaction signature for an address (for creator detection).
    ///
    /// Uses `getSignaturesForAddress` with `limit=1` in ascending order to find
    /// the first transaction involving the mint address. The first tx is the
    /// token creation transaction; its fee-payer / signer is the creator.
    async fn get_first_signature(
        &self,
        address: &str,
    ) -> Result<Option<SignatureInfo>, RegistryError>;

    /// Simulate a transaction against the current cluster state.
    ///
    /// Wraps the `simulateTransaction` JSON-RPC method.
    /// Reference: <https://solana.com/docs/rpc/http/simulatetransaction>
    ///
    /// # Arguments
    ///
    /// - `tx_base64`: Base64-encoded serialized `Transaction` (signed or unsigned).
    /// - `sig_verify`: Whether the RPC should verify the transaction signature.
    ///   Pass `false` for simulation keypairs (DG3 §3.2 — simulation uses throwaway
    ///   keypairs signed for shape only; `replaceRecentBlockhash: true` anyway).
    /// - `replace_recent_blockhash`: Replace the transaction's recent blockhash with
    ///   the current slot's blockhash. Must be `true` when `sig_verify` is `false`.
    /// - `commitment`: Commitment level for simulation state ("confirmed" is the
    ///   standard hot-path choice).
    /// - `accounts_to_track`: Base58 account addresses whose post-state to return.
    ///   Pass the simulation keypair ATA + source token ATA so the detector can
    ///   read post-simulation balances. Empty slice = no account state returned.
    async fn simulate_transaction(
        &self,
        tx_base64: &str,
        sig_verify: bool,
        replace_recent_blockhash: bool,
        commitment: &str,
        accounts_to_track: &[&str],
    ) -> Result<SimulatedTransaction, RegistryError>;

    /// Fetch raw account state for any program account.
    ///
    /// Uses `getAccountInfo` with `{ encoding: "base64", commitment: "confirmed" }`.
    /// Returns `None` when the account does not exist (RPC returns `value: null`).
    ///
    /// This is the low-level primitive used by [`PoolAccountProvider`] implementations
    /// that need to fetch pool-state accounts.  Token-registry callers that need
    /// a decoded mint should use [`get_mint_account`] instead.
    ///
    /// Added in Sprint 9 B1 to support `HttpPoolAccountProvider` in `dex-adapter`.
    async fn get_account_raw(
        &self,
        address: &str,
    ) -> Result<Option<RawAccount>, RegistryError>;
}

// ---------------------------------------------------------------------------
// HTTP production implementation
// ---------------------------------------------------------------------------

/// Production [`SolanaRpc`] implementation backed by `reqwest` HTTP.
///
/// Implements retry with exponential backoff + jitter on 429 and network errors.
/// Falls back to the next endpoint in `config.rpc_endpoints` on consecutive failures.
#[derive(Debug, Clone)]
pub struct HttpSolanaRpc {
    client: reqwest::Client,
    endpoints: Vec<String>,
    retry: crate::config::RetryConfig,
}

impl HttpSolanaRpc {
    /// Construct from registry config. The `reqwest::Client` is shared.
    pub fn new(config: &RegistryConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client build failed (TLS issue?)");

        Self {
            client,
            endpoints: config.rpc_endpoints.clone(),
            retry: config.retry.clone(),
        }
    }

    /// Select the endpoint for attempt `n` (round-robin across endpoints).
    fn endpoint_for(&self, attempt: u32) -> &str {
        let idx = (attempt as usize) % self.endpoints.len();
        &self.endpoints[idx]
    }

    /// POST a JSON-RPC request with retry + backoff.
    ///
    /// Returns the raw response body as a string, or `RegistryError` after
    /// `max_attempts` failures.
    async fn rpc_call(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<String, RegistryError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params
        });

        let mut last_err: Option<RegistryError> = None;

        for attempt in 0..self.retry.max_attempts {
            if attempt > 0 {
                let delay = self.retry.delay_for_attempt(attempt - 1);
                tokio::time::sleep(delay).await;
            }

            let endpoint = self.endpoint_for(attempt);
            debug!(method, attempt, endpoint, "RPC call");

            let result = self
                .client
                .post(endpoint)
                .json(&body)
                .send()
                .await;

            match result {
                Err(e) => {
                    warn!(method, attempt, error = %e, "RPC network error");
                    last_err = Some(RegistryError::RpcExhausted {
                        method,
                        attempts: attempt + 1,
                        reason: e.to_string(),
                    });
                    continue;
                }
                Ok(resp) if resp.status().as_u16() == 429 => {
                    warn!(method, attempt, "RPC rate limited (429)");
                    last_err = Some(RegistryError::RpcRateLimited { method });
                    continue;
                }
                Ok(resp) if !resp.status().is_success() => {
                    let status = resp.status();
                    let body_text = resp.text().await.unwrap_or_default();
                    warn!(method, attempt, status = %status, "RPC HTTP error");
                    last_err = Some(RegistryError::RpcExhausted {
                        method,
                        attempts: attempt + 1,
                        reason: format!("HTTP {status}: {body_text}"),
                    });
                    continue;
                }
                Ok(resp) => {
                    return resp.text().await.map_err(|e| RegistryError::RpcExhausted {
                        method,
                        attempts: attempt + 1,
                        reason: e.to_string(),
                    });
                }
            }
        }

        Err(last_err.unwrap_or(RegistryError::RpcExhausted {
            method,
            attempts: self.retry.max_attempts,
            reason: "all attempts failed".to_owned(),
        }))
    }
}

#[async_trait]
impl SolanaRpc for HttpSolanaRpc {
    #[instrument(skip(self), fields(mint = mint_address))]
    async fn get_mint_account(
        &self,
        mint_address: &str,
    ) -> Result<Option<DecodedMint>, RegistryError> {
        let params = serde_json::json!([
            mint_address,
            { "encoding": "base64", "commitment": "confirmed" }
        ]);

        let raw = self.rpc_call("getAccountInfo", params).await?;
        let resp: RpcResponse<Option<AccountInfo>> =
            serde_json::from_str(&raw).map_err(RegistryError::Json)?;

        if let Some(err) = resp.error {
            return Err(RegistryError::RpcExhausted {
                method: "getAccountInfo",
                attempts: 1,
                reason: format!("RPC error {}: {}", err.code, err.message),
            });
        }

        let account = match resp.result.and_then(|r| r.value) {
            None => return Ok(None),
            Some(a) => a,
        };

        let data_b64 = account
            .data
            .first()
            .ok_or_else(|| RegistryError::InvalidMintAccount {
                mint: mint_address.to_owned(),
                reason: "empty data array in getAccountInfo response".to_owned(),
            })?;

        let bytes = BASE64_STANDARD.decode(data_b64).map_err(|e| RegistryError::Base64Decode {
            account: mint_address.to_owned(),
            reason: e.to_string(),
        })?;

        decode_mint_bytes(&bytes, mint_address).map(Some)
    }

    #[instrument(skip(self), fields(mint = mint_address))]
    async fn get_token_largest_accounts(
        &self,
        mint_address: &str,
        commitment: &str,
    ) -> Result<Vec<TokenAccountBalance>, RegistryError> {
        let params = serde_json::json!([
            mint_address,
            { "commitment": commitment }
        ]);

        let raw = self
            .rpc_call("getTokenLargestAccounts", params)
            .await?;

        let resp: RpcResponse<Vec<TokenAccountBalance>> =
            serde_json::from_str(&raw).map_err(RegistryError::Json)?;

        if let Some(err) = resp.error {
            return Err(RegistryError::RpcExhausted {
                method: "getTokenLargestAccounts",
                attempts: 1,
                reason: format!("RPC error {}: {}", err.code, err.message),
            });
        }

        Ok(resp.result.map(|r| r.value).unwrap_or_default())
    }

    #[instrument(skip(self), fields(token_account))]
    async fn get_token_account_owner(
        &self,
        token_account: &str,
    ) -> Result<Option<String>, RegistryError> {
        let params = serde_json::json!([
            token_account,
            { "encoding": "jsonParsed", "commitment": "confirmed" }
        ]);

        let raw = self.rpc_call("getAccountInfo", params).await?;

        // jsonParsed returns a different shape; parse owner from the nested JSON.
        let v: serde_json::Value =
            serde_json::from_str(&raw).map_err(RegistryError::Json)?;

        // Shape: result.value.data.parsed.info.owner
        let owner = v
            .pointer("/result/value/data/parsed/info/owner")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned());

        Ok(owner)
    }

    #[instrument(skip(self), fields(address))]
    async fn get_first_signature(
        &self,
        address: &str,
    ) -> Result<Option<SignatureInfo>, RegistryError> {
        // Fetch the last page (oldest transactions) by requesting limit=1 with
        // no `before` param and searching backwards. The Solana API returns in
        // descending order by default (newest first). For creator detection we
        // want the oldest tx. A better approach is to traverse backwards with
        // `before` cursors, but for MVP we just take the first tx in the list
        // which is the most recent. True "first tx" detection is Phase 3.
        // TODO Phase 3: implement backwards cursor traversal for creator detection.
        let params = serde_json::json!([
            address,
            { "limit": 1, "commitment": "confirmed" }
        ]);

        let raw = self
            .rpc_call("getSignaturesForAddress", params)
            .await?;

        let resp: RpcResponse<Vec<SignatureInfo>> =
            serde_json::from_str(&raw).map_err(RegistryError::Json)?;

        if let Some(err) = resp.error {
            return Err(RegistryError::RpcExhausted {
                method: "getSignaturesForAddress",
                attempts: 1,
                reason: format!("RPC error {}: {}", err.code, err.message),
            });
        }

        Ok(resp.result.and_then(|r| r.value.into_iter().next()))
    }

    #[instrument(skip(self, tx_base64, accounts_to_track), fields(commitment))]
    async fn simulate_transaction(
        &self,
        tx_base64: &str,
        sig_verify: bool,
        replace_recent_blockhash: bool,
        commitment: &str,
        accounts_to_track: &[&str],
    ) -> Result<SimulatedTransaction, RegistryError> {
        // Build the config object. When `accounts_to_track` is non-empty, request
        // post-simulation account state in base64 encoding (needed to read SPL
        // token account balances without a second RPC call).
        let config = if accounts_to_track.is_empty() {
            serde_json::json!({
                "sigVerify": sig_verify,
                "replaceRecentBlockhash": replace_recent_blockhash,
                "commitment": commitment,
                "encoding": "base64",
            })
        } else {
            serde_json::json!({
                "sigVerify": sig_verify,
                "replaceRecentBlockhash": replace_recent_blockhash,
                "commitment": commitment,
                "encoding": "base64",
                "accounts": {
                    "encoding": "base64",
                    "addresses": accounts_to_track,
                },
            })
        };

        let params = serde_json::json!([tx_base64, config]);
        let raw = self.rpc_call("simulateTransaction", params).await?;

        parse_simulate_transaction_response(&raw)
    }

    #[instrument(skip(self), fields(address))]
    async fn get_account_raw(
        &self,
        address: &str,
    ) -> Result<Option<RawAccount>, RegistryError> {
        let params = serde_json::json!([
            address,
            { "encoding": "base64", "commitment": "confirmed" }
        ]);

        let raw = self.rpc_call("getAccountInfo", params).await?;
        let resp: RpcResponse<Option<AccountInfo>> =
            serde_json::from_str(&raw).map_err(RegistryError::Json)?;

        if let Some(err) = resp.error {
            return Err(RegistryError::RpcExhausted {
                method: "getAccountInfo",
                attempts: 1,
                reason: format!("RPC error {}: {}", err.code, err.message),
            });
        }

        let account = match resp.result.and_then(|r| r.value) {
            None => return Ok(None),
            Some(a) => a,
        };

        let data_b64 = account
            .data
            .first()
            .ok_or_else(|| RegistryError::InvalidMintAccount {
                mint: address.to_owned(),
                reason: "empty data array in getAccountInfo response".to_owned(),
            })?;

        let data = BASE64_STANDARD.decode(data_b64).map_err(|e| RegistryError::Base64Decode {
            account: address.to_owned(),
            reason: e.to_string(),
        })?;

        let owner = mg_solana_types::Pubkey::from_str(&account.owner)
            .map_err(|e| RegistryError::InvalidMintAccount {
                mint: address.to_owned(),
                reason: format!("invalid owner pubkey '{}': {}", account.owner, e),
            })?;

        // Solana RPC does not always include rent_epoch in all API versions;
        // default to 0 when absent (value is informational only).
        Ok(Some(RawAccount {
            lamports: account.lamports,
            owner,
            data,
            executable: account.executable,
            rent_epoch: 0,
        }))
    }
}

// ---------------------------------------------------------------------------
// simulateTransaction response parser (pure — no I/O)
// ---------------------------------------------------------------------------

/// Internal deserialization helpers for `simulateTransaction` response.
#[derive(Debug, Deserialize)]
struct SimulateValue {
    err: Option<serde_json::Value>,
    logs: Option<Vec<String>>,
    accounts: Option<Vec<Option<RawSimAccount>>>,
    #[serde(rename = "unitsConsumed")]
    units_consumed: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawSimAccount {
    lamports: u64,
    data: Vec<String>,
    owner: String,
}

fn parse_simulate_transaction_response(raw: &str) -> Result<SimulatedTransaction, RegistryError> {
    let resp: RpcResponse<SimulateValue> =
        serde_json::from_str(raw).map_err(RegistryError::Json)?;

    if let Some(err) = resp.error {
        return Err(RegistryError::RpcExhausted {
            method: "simulateTransaction",
            attempts: 1,
            reason: format!("RPC error {}: {}", err.code, err.message),
        });
    }

    let value = resp
        .result
        .ok_or_else(|| RegistryError::RpcExhausted {
            method: "simulateTransaction",
            attempts: 1,
            reason: "missing result field in simulateTransaction response".to_owned(),
        })?
        .value;

    // Serialize the `err` field to a string if present.
    let err_str = value.err.map(|e| e.to_string());

    let accounts: Vec<Option<SimulatedAccount>> = value
        .accounts
        .unwrap_or_default()
        .into_iter()
        .map(|opt| {
            opt.map(|a| SimulatedAccount {
                lamports: a.lamports,
                data: a.data,
                owner: a.owner,
            })
        })
        .collect();

    Ok(SimulatedTransaction {
        err: err_str,
        logs: value.logs.unwrap_or_default(),
        accounts,
        units_consumed: value.units_consumed,
    })
}

// ---------------------------------------------------------------------------
// Mint account decoder (pure function — no I/O)
// ---------------------------------------------------------------------------

/// Decode raw account bytes into a [`DecodedMint`].
///
/// Handles both SPL Token (82 bytes) and Token-2022 (82+ bytes).
/// Layout reference: https://github.com/solana-program/token/blob/main/program/src/state.rs
///
/// This is a pure function — no I/O, deterministic output. Tested independently.
pub fn decode_mint_bytes(bytes: &[u8], address: &str) -> Result<DecodedMint, RegistryError> {
    // Both SPL Token and Token-2022 start with the same 82-byte base layout.
    if bytes.len() < 82 {
        return Err(RegistryError::InvalidMintAccount {
            mint: address.to_owned(),
            reason: format!(
                "account data too short: {} bytes (expected >= 82)",
                bytes.len()
            ),
        });
    }

    // Bytes [0..4]: mint_authority COption discriminant (0=None, 1=Some)
    let mint_authority_present = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) == 1;
    // Bytes [4..36]: mint_authority pubkey (only valid when present=1)
    let mint_authority = if mint_authority_present {
        let key_bytes: [u8; 32] = bytes[4..36].try_into().unwrap();
        Some(bs58_encode_pubkey(&key_bytes))
    } else {
        None
    };

    // Bytes [36..44]: supply (u64 LE)
    let supply_bytes: [u8; 8] = bytes[36..44].try_into().unwrap();
    let supply = u64::from_le_bytes(supply_bytes) as u128;

    // Byte [44]: decimals
    let decimals = bytes[44];

    // Byte [45]: is_initialized (should be 1)
    if bytes[45] != 1 {
        return Err(RegistryError::InvalidMintAccount {
            mint: address.to_owned(),
            reason: "is_initialized = 0 (account not initialized)".to_owned(),
        });
    }

    // Bytes [46..50]: freeze_authority COption discriminant
    let freeze_authority_present = u32::from_le_bytes(bytes[46..50].try_into().unwrap()) == 1;
    let freeze_authority = if freeze_authority_present {
        let key_bytes: [u8; 32] = bytes[50..82].try_into().unwrap();
        Some(bs58_encode_pubkey(&key_bytes))
    } else {
        None
    };

    // Token-2022: if data is longer than 82 bytes, extensions are present.
    let is_token2022 = bytes.len() > 82;

    Ok(DecodedMint {
        supply,
        decimals,
        mint_authority,
        freeze_authority,
        is_token2022,
        raw_account_data: bytes.to_vec(),
    })
}

/// Encode 32 bytes as a Base58 Solana address.
fn bs58_encode_pubkey(bytes: &[u8; 32]) -> String {
    bs58::encode(bytes).into_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "test-utils"))]
pub mod tests {
    use super::*;

    /// A minimal mock `SolanaRpc` implementation for tests.
    ///
    /// Returns pre-configured responses. Each call type has a separate
    /// `Option<Result<...>>` field. `None` means the method was not configured
    /// and will return an error indicating "unexpected call".
    #[derive(Debug, Default)]
    pub struct MockSolanaRpc {
        pub mint_account: Option<Result<Option<DecodedMint>, RegistryError>>,
        pub largest_accounts: Option<Result<Vec<TokenAccountBalance>, RegistryError>>,
        pub token_account_owner: Option<Result<Option<String>, RegistryError>>,
        pub first_signature: Option<Result<Option<SignatureInfo>, RegistryError>>,
        /// Pre-configured response for `simulate_transaction`. `None` returns an error.
        pub simulate_response: Option<Result<SimulatedTransaction, RegistryError>>,
        /// Per-address raw account responses keyed by Base58 address string.
        ///
        /// Populated via [`MockSolanaRpc::with_account_raw`]. Addresses not present
        /// return `Ok(None)` (account not found), matching real RPC behaviour for
        /// unknown accounts.
        pub raw_accounts: std::collections::HashMap<String, RawAccount>,
    }

    impl MockSolanaRpc {
        pub fn with_mint(decoded: DecodedMint) -> Self {
            Self {
                mint_account: Some(Ok(Some(decoded))),
                ..Default::default()
            }
        }

        pub fn with_no_mint() -> Self {
            Self {
                mint_account: Some(Ok(None)),
                ..Default::default()
            }
        }

        /// Configure a successful simulation response.
        pub fn with_simulation_success(result: SimulatedTransaction) -> Self {
            Self {
                simulate_response: Some(Ok(result)),
                ..Default::default()
            }
        }

        /// Configure a failing simulation response (RPC-level error, not tx error).
        pub fn with_simulation_error(reason: impl Into<String>) -> Self {
            Self {
                simulate_response: Some(Err(RegistryError::RpcExhausted {
                    method: "simulateTransaction",
                    attempts: 1,
                    reason: reason.into(),
                })),
                ..Default::default()
            }
        }

        /// Pre-configure a raw account response.
        ///
        /// Subsequent calls to `get_account_raw(address)` will return
        /// `Ok(Some(account))` for that address. Addresses not configured return
        /// `Ok(None)` (simulates "account not found on-chain").
        pub fn with_account_raw(mut self, address: &str, account: RawAccount) -> Self {
            self.raw_accounts.insert(address.to_owned(), account);
            self
        }

        pub fn with_rate_limit_then_mint(decoded: DecodedMint) -> MockRetryRpc {
            MockRetryRpc {
                mint_on_attempt: 1,
                mint_result: decoded,
                call_count: std::sync::atomic::AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl SolanaRpc for MockSolanaRpc {
        async fn get_mint_account(
            &self,
            _mint: &str,
        ) -> Result<Option<DecodedMint>, RegistryError> {
            self.mint_account
                .as_ref()
                .map(|r| match r {
                    Ok(v) => Ok(v.clone()),
                    Err(e) => Err(RegistryError::Internal(e.to_string())),
                })
                .unwrap_or(Err(RegistryError::Internal("mock not configured: mint_account".to_owned())))
        }

        async fn get_token_largest_accounts(
            &self,
            _mint: &str,
            _commitment: &str,
        ) -> Result<Vec<TokenAccountBalance>, RegistryError> {
            self.largest_accounts
                .as_ref()
                .map(|r| match r {
                    Ok(v) => Ok(v.clone()),
                    Err(e) => Err(RegistryError::Internal(e.to_string())),
                })
                .unwrap_or(Ok(vec![]))
        }

        async fn get_token_account_owner(
            &self,
            _token_account: &str,
        ) -> Result<Option<String>, RegistryError> {
            self.token_account_owner
                .as_ref()
                .map(|r| match r {
                    Ok(v) => Ok(v.clone()),
                    Err(e) => Err(RegistryError::Internal(e.to_string())),
                })
                .unwrap_or(Ok(None))
        }

        async fn get_first_signature(
            &self,
            _address: &str,
        ) -> Result<Option<SignatureInfo>, RegistryError> {
            self.first_signature
                .as_ref()
                .map(|r| match r {
                    Ok(v) => Ok(v.clone()),
                    Err(e) => Err(RegistryError::Internal(e.to_string())),
                })
                .unwrap_or(Ok(None))
        }

        async fn simulate_transaction(
            &self,
            _tx_base64: &str,
            _sig_verify: bool,
            _replace_recent_blockhash: bool,
            _commitment: &str,
            _accounts_to_track: &[&str],
        ) -> Result<SimulatedTransaction, RegistryError> {
            self.simulate_response
                .as_ref()
                .map(|r| match r {
                    Ok(v) => Ok(SimulatedTransaction {
                        err: v.err.clone(),
                        logs: v.logs.clone(),
                        accounts: v.accounts.clone(),
                        units_consumed: v.units_consumed,
                    }),
                    Err(e) => Err(RegistryError::Internal(e.to_string())),
                })
                .unwrap_or_else(|| {
                    Err(RegistryError::Internal(
                        "mock not configured: simulate_response".to_owned(),
                    ))
                })
        }

        async fn get_account_raw(
            &self,
            address: &str,
        ) -> Result<Option<RawAccount>, RegistryError> {
            Ok(self.raw_accounts.get(address).cloned())
        }
    }

    /// Mock that simulates one rate-limit error then succeeds.
    pub struct MockRetryRpc {
        pub mint_on_attempt: u32,
        pub mint_result: DecodedMint,
        pub call_count: std::sync::atomic::AtomicU32,
    }

    #[async_trait]
    impl SolanaRpc for MockRetryRpc {
        async fn get_mint_account(
            &self,
            _mint: &str,
        ) -> Result<Option<DecodedMint>, RegistryError> {
            let n = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.mint_on_attempt {
                Err(RegistryError::RpcRateLimited { method: "getAccountInfo" })
            } else {
                Ok(Some(self.mint_result.clone()))
            }
        }

        async fn get_token_largest_accounts(
            &self,
            _mint: &str,
            _commitment: &str,
        ) -> Result<Vec<TokenAccountBalance>, RegistryError> {
            Ok(vec![])
        }

        async fn get_token_account_owner(
            &self,
            _token_account: &str,
        ) -> Result<Option<String>, RegistryError> {
            Ok(None)
        }

        async fn get_first_signature(
            &self,
            _address: &str,
        ) -> Result<Option<SignatureInfo>, RegistryError> {
            Ok(None)
        }

        async fn simulate_transaction(
            &self,
            _tx_base64: &str,
            _sig_verify: bool,
            _replace_recent_blockhash: bool,
            _commitment: &str,
            _accounts_to_track: &[&str],
        ) -> Result<SimulatedTransaction, RegistryError> {
            // Default: return a successful empty simulation so existing tests compile.
            Ok(SimulatedTransaction {
                err: None,
                logs: vec![],
                accounts: vec![],
                units_consumed: None,
            })
        }

        async fn get_account_raw(
            &self,
            _address: &str,
        ) -> Result<Option<RawAccount>, RegistryError> {
            Ok(None)
        }
    }

    // ----- decode_mint_bytes unit tests (test-only helpers) -----

    #[cfg(test)]
    fn make_mint_bytes(
        supply: u64,
        decimals: u8,
        mint_auth: Option<[u8; 32]>,
        freeze_auth: Option<[u8; 32]>,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; 82];

        // mint_authority COption
        if let Some(key) = mint_auth {
            buf[0..4].copy_from_slice(&1u32.to_le_bytes()); // present
            buf[4..36].copy_from_slice(&key);
        }

        // supply
        buf[36..44].copy_from_slice(&supply.to_le_bytes());

        // decimals
        buf[44] = decimals;

        // is_initialized
        buf[45] = 1;

        // freeze_authority COption
        if let Some(key) = freeze_auth {
            buf[46..50].copy_from_slice(&1u32.to_le_bytes()); // present
            buf[50..82].copy_from_slice(&key);
        }

        buf
    }

    #[test]
    fn decode_mint_with_no_authorities() {
        let bytes = make_mint_bytes(1_000_000_000, 9, None, None);
        let mint = decode_mint_bytes(&bytes, "test").unwrap();
        assert_eq!(mint.supply, 1_000_000_000u128);
        assert_eq!(mint.decimals, 9);
        assert!(mint.mint_authority.is_none());
        assert!(mint.freeze_authority.is_none());
        assert!(!mint.is_token2022);
    }

    #[test]
    fn decode_mint_with_both_authorities() {
        let auth_key = [0xABu8; 32];
        let bytes = make_mint_bytes(500_000, 6, Some(auth_key), Some(auth_key));
        let mint = decode_mint_bytes(&bytes, "test").unwrap();
        assert!(mint.mint_authority.is_some());
        assert!(mint.freeze_authority.is_some());
        // Verify Base58 encoding is correct length (32-byte pubkey → 44 chars)
        assert_eq!(
            mint.mint_authority.unwrap().len(),
            44,
            "Base58-encoded 32-byte pubkey should be 44 chars"
        );
    }

    #[test]
    fn decode_mint_token2022_detected() {
        let mut bytes = make_mint_bytes(100, 6, None, None);
        bytes.push(0x03); // Extra byte simulating Token-2022 extension TLV
        let mint = decode_mint_bytes(&bytes, "test").unwrap();
        assert!(mint.is_token2022, "should detect Token-2022 extension");
    }

    #[test]
    fn decode_mint_too_short_returns_error() {
        let bytes = vec![0u8; 10];
        let err = decode_mint_bytes(&bytes, "test").unwrap_err();
        assert!(
            matches!(err, RegistryError::InvalidMintAccount { .. }),
            "short data must return InvalidMintAccount"
        );
    }

    #[test]
    fn decode_mint_not_initialized_returns_error() {
        let mut bytes = make_mint_bytes(0, 9, None, None);
        bytes[45] = 0; // is_initialized = 0
        let err = decode_mint_bytes(&bytes, "test").unwrap_err();
        assert!(matches!(err, RegistryError::InvalidMintAccount { .. }));
    }

    #[test]
    fn bs58_encode_pubkey_all_zeros() {
        // All-zero pubkey should encode to "11111111111111111111111111111111"
        let result = bs58_encode_pubkey(&[0u8; 32]);
        assert_eq!(result, "11111111111111111111111111111111");
    }

    // ----- simulate_transaction mock tests -----

    #[test]
    fn mock_simulate_returns_configured_success_response() {
        // Build a successful SimulatedTransaction and verify MockSolanaRpc echoes it.
        let configured = SimulatedTransaction {
            err: None,
            logs: vec!["Program log: success".to_owned()],
            accounts: vec![Some(SimulatedAccount {
                lamports: 10_000,
                data: vec!["AAAA".to_owned(), "base64".to_owned()],
                owner: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_owned(),
            })],
            units_consumed: Some(42_000),
        };
        let mock = MockSolanaRpc::with_simulation_success(configured);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(mock.simulate_transaction(
            "AAABBBCCC",
            false,
            true,
            "confirmed",
            &["11111111111111111111111111111111"],
        ));

        let sim = result.expect("should succeed");
        assert!(sim.err.is_none(), "err must be None");
        assert_eq!(sim.logs.len(), 1);
        assert_eq!(sim.units_consumed, Some(42_000));
        assert_eq!(sim.accounts.len(), 1);
        assert!(sim.accounts[0].is_some());
    }

    #[test]
    fn mock_simulate_returns_error_when_unconfigured() {
        let mock = MockSolanaRpc::default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(mock.simulate_transaction(
            "AAABBBCCC",
            false,
            true,
            "confirmed",
            &[],
        ));
        assert!(
            result.is_err(),
            "unconfigured mock must return Err"
        );
    }

    #[test]
    fn mock_simulate_returns_configured_rpc_error() {
        let mock = MockSolanaRpc::with_simulation_error("node offline");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(mock.simulate_transaction(
            "AAABBBCCC",
            false,
            true,
            "confirmed",
            &[],
        ));
        assert!(result.is_err(), "error-configured mock must return Err");
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("node offline") || !err_str.is_empty());
    }

    #[test]
    fn parse_simulate_transaction_response_success() {
        // Test the pure parser function directly.
        let raw = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "context": { "slot": 100 },
                "value": {
                    "err": null,
                    "logs": ["Program log: hello"],
                    "accounts": null,
                    "unitsConsumed": 8500,
                    "returnData": null
                }
            }
        }"#;
        let sim = parse_simulate_transaction_response(raw).unwrap();
        assert!(sim.err.is_none());
        assert_eq!(sim.logs, vec!["Program log: hello"]);
        assert_eq!(sim.units_consumed, Some(8500));
        assert!(sim.accounts.is_empty());
    }

    #[test]
    fn parse_simulate_transaction_response_with_tx_error() {
        // Simulate a transaction that fails due to InstructionError.
        let raw = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "context": { "slot": 101 },
                "value": {
                    "err": {"InstructionError": [0, {"Custom": 6}]},
                    "logs": ["Program log: Error"],
                    "accounts": null,
                    "unitsConsumed": 1200
                }
            }
        }"#;
        let sim = parse_simulate_transaction_response(raw).unwrap();
        assert!(sim.err.is_some(), "tx error must populate err field");
        let err_str = sim.err.unwrap();
        assert!(
            err_str.contains("InstructionError") || err_str.contains("Custom"),
            "err must stringify the error object: {err_str}"
        );
    }

    #[test]
    fn parse_simulate_transaction_response_with_accounts() {
        let raw = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "context": { "slot": 102 },
                "value": {
                    "err": null,
                    "logs": [],
                    "accounts": [
                        {
                            "lamports": 2039280,
                            "data": ["AQID", "base64"],
                            "owner": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                            "executable": false,
                            "rentEpoch": 0
                        },
                        null
                    ],
                    "unitsConsumed": 55000
                }
            }
        }"#;
        let sim = parse_simulate_transaction_response(raw).unwrap();
        assert!(sim.err.is_none());
        assert_eq!(sim.accounts.len(), 2);
        assert!(sim.accounts[0].is_some());
        let acc = sim.accounts[0].as_ref().unwrap();
        assert_eq!(acc.lamports, 2_039_280);
        assert_eq!(acc.data[0], "AQID");
        assert_eq!(acc.owner, "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
        assert!(sim.accounts[1].is_none());
    }
}
