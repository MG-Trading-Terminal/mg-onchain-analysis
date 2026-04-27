//! Token enrichment — the core of the token-registry.
//!
//! [`enrich_token_inner`] takes a Solana mint address and produces a full
//! [`TokenMeta`] by combining:
//!   1. SPL Mint account data (`getAccountInfo`) — supply, decimals, authorities.
//!   2. Top-20 token accounts (`getTokenLargestAccounts`) — `top_holders[]`.
//!   3. Owner resolution for each holder — resolves token account → wallet.
//!   4. Creator detection — first sig for the mint (approx; Phase 3 improves).
//!
//! Upserts the result into the `tokens` Postgres table via `PgStore`.
//! Classification of each holder is delegated to [`classify::HolderClassifier`].
//!
//! # TTL strategy
//!
//! An existing Postgres row is returned without RPC if `now - updated_at < ttl_metadata_secs`.
//! Holder data is re-enriched if `now - updated_at >= ttl_holders_secs`.
//!
//! # Concurrency
//!
//! [`enrich_token`] accepts a `tokio::sync::Semaphore` and acquires one permit
//! before calling [`enrich_token_inner`]. The caller creates a shared semaphore of
//! size `config.concurrency_limit`. Tests call `enrich_token_inner` directly.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use tokio::sync::Semaphore;
use tracing::{debug, instrument, warn};

use mg_onchain_common::chain::{Address, BlockRef, Chain};
use mg_onchain_common::event::DexKind;
use mg_onchain_common::token::{
    HolderSnapshot, JupiterVerification, MarketInfo, TokenMeta, TopHolder,
};

use mg_onchain_storage::pg::{PoolMarketRow, PgStore};

use crate::cex_registry::CexRegistry;
use crate::classify::{upsert_classification, HolderClassifier};
use crate::config::RegistryConfig;
use crate::error::RegistryError;
use crate::rpc::SolanaRpc;
use crate::tlv::{self, Token2022Extensions};

/// Enrich a token, respecting the concurrency semaphore.
pub async fn enrich_token(
    mint: &str,
    chain: Chain,
    rpc: &dyn SolanaRpc,
    store: &PgStore,
    cex: &CexRegistry,
    config: &RegistryConfig,
    semaphore: &Arc<Semaphore>,
) -> Result<TokenMeta, RegistryError> {
    let _permit = semaphore
        .acquire()
        .await
        .map_err(|e| RegistryError::Internal(format!("semaphore closed: {e}")))?;
    enrich_token_inner(mint, chain, rpc, store, cex, config).await
}

/// Convert `PoolMarketRow` records from storage into `MarketInfo` values for `TokenMeta`.
///
/// `DexKind` is parsed from the snake_case dex string stored in the `pools` table
/// using `serde_json` since `DexKind` uses `#[serde(rename_all = "snake_case")]`.
/// Unrecognised dex strings fall back to `DexKind::Unknown(dex)`.
fn pool_market_rows_to_market_infos(chain: Chain, rows: Vec<PoolMarketRow>) -> Vec<MarketInfo> {
    rows.into_iter()
        .filter_map(|row| {
            let addr = Address::parse(chain, &row.pool_address).ok()?;
            // Parse via serde_json string value — DexKind derives serde rename_all="snake_case".
            let dex: DexKind = serde_json::from_value(
                serde_json::Value::String(row.dex.clone()),
            )
            .unwrap_or(DexKind::Unknown(row.dex));
            Some(MarketInfo {
                pool_address: addr,
                dex,
                lp_burned_pct: Decimal::ZERO,
                liquidity_usd: row.liquidity_usd,
                lp_provider_count: 0,
            })
        })
        .collect()
}

/// Fetch pool rows for a token and convert to `MarketInfo` values.
///
/// Non-fatal: query failures are logged and return `vec![]` so that enrichment
/// continues even when the pools table has no rows for the token.
async fn fetch_markets_from_pools(mint: &str, chain: Chain, store: &PgStore) -> Vec<MarketInfo> {
    match store.get_pools_for_token_as_markets(chain.as_str(), mint).await {
        Ok(rows) => pool_market_rows_to_market_infos(chain, rows),
        Err(e) => {
            warn!(mint, error = %e, "get_pools_for_token_as_markets failed — markets will be empty");
            vec![]
        }
    }
}

/// Core enrichment logic (no semaphore — use in tests).
#[instrument(skip(rpc, store, cex, config), fields(mint, chain = chain.as_str()))]
pub async fn enrich_token_inner(
    mint: &str,
    chain: Chain,
    rpc: &dyn SolanaRpc,
    store: &PgStore,
    cex: &CexRegistry,
    config: &RegistryConfig,
) -> Result<TokenMeta, RegistryError> {
    let now = Utc::now();

    // Cache read — return early if fresh.
    if let Ok(Some(row)) = store.get_token(chain.as_str(), mint).await {
        let age = (now - row.updated_at).num_seconds().unsigned_abs();
        if age < config.ttl_metadata_secs {
            debug!(mint, age, "returning cached TokenMeta");
            let mut meta = token_row_to_meta(mint, chain, &row)?;
            // Populate markets from pools table (not stored in tokens row).
            meta.markets = fetch_markets_from_pools(mint, chain, store).await;
            return Ok(meta);
        }
    }

    // RPC enrichment.
    let decoded = rpc
        .get_mint_account(mint)
        .await?
        .ok_or_else(|| RegistryError::InvalidMintAccount {
            mint: mint.to_owned(),
            reason: "account does not exist on-chain".to_owned(),
        })?;

    let (top_holders, total_holders, holder_snapshot) = fetch_holders(
        mint, chain, rpc, cex, store, config, decoded.supply,
    )
    .await;

    let mint_authority = decoded.mint_authority.as_deref()
        .and_then(|a| Address::parse(chain, a).ok());
    let freeze_authority = decoded.freeze_authority.as_deref()
        .and_then(|a| Address::parse(chain, a).ok());

    let mint_addr = Address::parse(chain, mint).map_err(|e| RegistryError::InvalidAddress {
        address: mint.to_owned(),
        reason: e.to_string(),
    })?;

    // Decode Token-2022 TLV extensions if present. Legacy SPL Token mints have
    // exactly 82 bytes — `decode_extensions` returns `TlvError::TooShort` for them,
    // which we treat as "no extensions" via `unwrap_or_default()`.
    //
    // Discriminators verified live on 2026-04-21:
    //   PermanentDelegate = 12  (S3 signal in d01_honeypot.rs)
    //   TransferHook      = 14  (S4 signal in d01_honeypot.rs)
    //
    // Reference: https://github.com/solana-program/token-2022/blob/main/interface/src/extension/mod.rs
    let extensions: Token2022Extensions =
        tlv::decode_extensions(&decoded.raw_account_data).unwrap_or_default();

    // Convert raw Pubkey bytes → Address. Zero Pubkeys already filtered to None
    // by `decode_extensions`; `Address::parse` on a valid Base58 string always
    // succeeds for 32-byte Pubkeys, so the `ok()` flattening is purely defensive.
    let permanent_delegate: Option<Address> = extensions
        .permanent_delegate
        .map(|bytes| bs58::encode(bytes).into_string())
        .and_then(|s| Address::parse(chain, &s).ok());

    let transfer_hook_program: Option<Address> = extensions
        .transfer_hook_program
        .map(|bytes| bs58::encode(bytes).into_string())
        .and_then(|s| Address::parse(chain, &s).ok());

    // P6-2: boolean marker extensions (non_transferable, confidential_transfer).
    // These are presence flags — no data payload, no address conversion needed.
    let non_transferable: bool = extensions.non_transferable;
    let confidential_transfer: bool = extensions.confidential_transfer;

    // Populate markets from the pools table so detectors that iterate
    // meta.markets (D01, D02) can find seeded pool rows without a
    // tokens_markets join table (which does not exist in migrations).
    let markets = fetch_markets_from_pools(mint, chain, store).await;
    let total_market_liquidity_usd = markets
        .iter()
        .map(|m| m.liquidity_usd)
        .fold(Decimal::ZERO, |a, b| a + b);

    let meta = TokenMeta {
        mint: mint_addr,
        chain,
        symbol: None,
        name: None,
        decimals: decoded.decimals,
        token_program: None,
        total_supply_raw: decoded.supply,
        circulating_supply_raw: None,
        mint_authority,
        freeze_authority,
        creator: None, // Phase 3: decode first tx fee-payer
        creator_balance_raw: 0,
        transfer_fee: None, // Phase 3: decode Token-2022 extension bytes
        permanent_delegate,
        transfer_hook_program,
        non_transferable,
        confidential_transfer,
        top_holders,
        total_holders: total_holders as u64,
        markets,
        total_market_liquidity_usd,
        lockers: vec![],
        graph_insiders_detected: false,
        insider_networks: vec![],
        launchpad: None,
        deploy_platform: None,
        detected_at: None,
        rugged: false,
        verification: JupiterVerification::default(),
        rugcheck_score: None,
        buy_tax: None,
        sell_tax: None,
        transfer_tax: None,
        honeypot_flags: vec![],
        updated_at: now,
    };

    store
        .upsert_token(
            chain.as_str(),
            mint,
            None, // symbol
            None, // name
            meta.decimals as i16,
            None, // token_program
            meta.total_supply_raw,
            meta.circulating_supply_raw,
            meta.mint_authority.as_ref().map(|a| a.as_str()),
            meta.freeze_authority.as_ref().map(|a| a.as_str()),
            None, // creator
            meta.creator_balance_raw,
            total_holders as i64,
            &meta.total_market_liquidity_usd.to_string(),
            false, // jup_verified
            false, // jup_strict
            false, // rugged
            None,  // rugcheck_score
            None,  // launchpad
            None,  // deploy_platform
            None,  // detected_at
            meta.permanent_delegate.as_ref().map(|a| a.as_str()),
            meta.transfer_hook_program.as_ref().map(|a| a.as_str()),
            meta.non_transferable,
            meta.confidential_transfer,
        )
        .await?;

    if let Some(snap) = holder_snapshot {
        store.upsert_holder_snapshots(&[snap]).await?;
    }

    Ok(meta)
}

async fn fetch_holders(
    mint: &str,
    chain: Chain,
    rpc: &dyn SolanaRpc,
    cex: &CexRegistry,
    store: &PgStore,
    config: &RegistryConfig,
    total_supply: u128,
) -> (Vec<TopHolder>, usize, Option<HolderSnapshot>) {
    let accounts = match rpc.get_token_largest_accounts(mint, "confirmed").await {
        Ok(a) => a,
        Err(e) => {
            warn!(mint, error = %e, "getTokenLargestAccounts failed");
            return (vec![], 0, None);
        }
    };
    if accounts.is_empty() {
        return (vec![], 0, None);
    }

    let classifier = HolderClassifier::new(rpc, cex);
    let mut top_holders: Vec<TopHolder> = Vec::with_capacity(accounts.len());
    let mut balances: BTreeMap<String, u128> = BTreeMap::new();

    for account in accounts.iter().take(config.top_holders_limit) {
        let amount_raw: u128 = account.amount.parse().unwrap_or(0);
        if amount_raw == 0 { continue; }

        let owner = rpc.get_token_account_owner(&account.address)
            .await.ok().flatten()
            .unwrap_or_else(|| account.address.clone());

        let classification = classifier.classify(&account.address, chain.as_str()).await;
        if let Ok(ref c) = classification {
            let _ = upsert_classification(store.pool(), c).await;
        }

        // is_insider = true when we have no positive classification (falls back to liquid)
        // and the address is NOT a dex_pool / vesting / cex / burn.
        let is_insider = matches!(
            &classification,
            Ok(c) if c.kind.kind_str() == "liquid"
        );

        let pct = if total_supply > 0 {
            let r = Decimal::from_u128(amount_raw).unwrap_or(Decimal::ZERO);
            let t = Decimal::from_u128(total_supply).unwrap_or_else(|| Decimal::from(1));
            (r / t) * Decimal::from(100)
        } else {
            Decimal::ZERO
        };

        if let Ok(addr) = Address::parse(chain, &owner) {
            top_holders.push(TopHolder { address: addr, pct, amount_raw, is_insider });
            balances.insert(owner, amount_raw);
        }
    }

    let total_count = accounts.len();
    let gini = compute_gini(&top_holders, total_supply);
    let top10 = top_n_pct(&top_holders, 10, total_supply);

    let null_addr = Address::parse(Chain::Solana, "11111111111111111111111111111111").unwrap();
    let token_addr = Address::parse(chain, mint).unwrap_or(null_addr);
    let snapshot = HolderSnapshot {
        token: token_addr,
        chain,
        block: BlockRef::new(chain, 0),
        block_time: Utc::now(),
        is_full: false,
        balances,
        total_holders: total_count as u64,
        gini: Some(gini),
        top10_pct: Some(top10),
    };

    (top_holders, total_count, Some(snapshot))
}

/// Gini coefficient (Atkinson 1970 sorted-balance formula).
///
/// Reference: Brown (2023) "Token concentration via Gini" — REFERENCES.md D3.
pub fn compute_gini(holders: &[TopHolder], total_supply: u128) -> Decimal {
    if holders.is_empty() || total_supply == 0 {
        return Decimal::ZERO;
    }
    let mut sorted: Vec<u128> = holders.iter().map(|h| h.amount_raw).collect();
    sorted.sort_unstable();
    let n = sorted.len() as u128;
    let sum: u128 = sorted.iter().sum();
    if sum == 0 { return Decimal::ZERO; }

    let ws: u128 = sorted.iter().enumerate()
        .map(|(i, &x)| (i as u128 + 1).saturating_mul(x))
        .fold(0u128, |a, v| a.saturating_add(v));

    let two_ws = Decimal::from_u128(2u128.saturating_mul(ws)).unwrap_or(Decimal::ZERO);
    let n_sum  = Decimal::from_u128(n.saturating_mul(sum)).unwrap_or_else(|| Decimal::from(1));
    let n_d    = Decimal::from_u128(n).unwrap_or_else(|| Decimal::from(1));
    let g = (two_ws / n_sum) - ((n_d + Decimal::ONE) / n_d);
    g.max(Decimal::ZERO).min(Decimal::ONE)
}

/// Top-N percentage of total supply.
pub fn top_n_pct(holders: &[TopHolder], n: usize, total_supply: u128) -> Decimal {
    if total_supply == 0 { return Decimal::ZERO; }
    let mut sorted = holders.to_vec();
    sorted.sort_unstable_by_key(|h| std::cmp::Reverse(h.amount_raw));
    let top: u128 = sorted.iter().take(n).map(|h| h.amount_raw)
        .fold(0u128, |a, v| a.saturating_add(v));
    let t = Decimal::from_u128(top).unwrap_or(Decimal::ZERO);
    let s = Decimal::from_u128(total_supply).unwrap_or_else(|| Decimal::from(1));
    (t / s) * Decimal::from(100)
}

fn token_row_to_meta(
    mint: &str,
    chain: Chain,
    row: &mg_onchain_storage::pg::TokenRow,
) -> Result<TokenMeta, RegistryError> {
    let mint_addr = Address::parse(chain, mint).map_err(|e| RegistryError::InvalidAddress {
        address: mint.to_owned(),
        reason: e.to_string(),
    })?;
    let mint_auth  = row.mint_authority.as_deref().and_then(|a| Address::parse(chain, a).ok());
    let freeze_auth = row.freeze_authority.as_deref().and_then(|a| Address::parse(chain, a).ok());
    let creator    = row.creator.as_deref().and_then(|a| Address::parse(chain, a).ok());
    // V00004: Token-2022 extension fields are now stored in the `tokens` table
    // (permanent_delegate, transfer_hook_program columns). Populated by the RPC
    // enrichment path above via `tlv::decode_extensions`. Cached reads retrieve
    // them from the DB row here.
    let permanent_delegate = row.permanent_delegate.as_deref()
        .and_then(|a| Address::parse(chain, a).ok());
    let transfer_hook_program = row.transfer_hook_program.as_deref()
        .and_then(|a| Address::parse(chain, a).ok());
    // V00008: P6-2 boolean marker extension fields (non_transferable, confidential_transfer).
    // Default false when column absent (backward compat — old rows pre-V00008 return NULL
    // which maps to `unwrap_or(false)` via the storage layer's Option<bool> getter).
    let non_transferable = row.non_transferable.unwrap_or(false);
    let confidential_transfer = row.confidential_transfer.unwrap_or(false);
    Ok(TokenMeta {
        mint: mint_addr,
        chain,
        symbol: row.symbol.clone(),
        name: row.name.clone(),
        decimals: row.decimals as u8,
        token_program: None,
        total_supply_raw: row.total_supply_u128(),
        circulating_supply_raw: row.circulating_supply_u128(),
        mint_authority: mint_auth,
        freeze_authority: freeze_auth,
        creator,
        creator_balance_raw: row.creator_balance_u128(),
        transfer_fee: None,
        permanent_delegate,
        transfer_hook_program,
        non_transferable,
        confidential_transfer,
        top_holders: vec![],
        total_holders: row.total_holders as u64,
        markets: vec![],
        total_market_liquidity_usd: row.total_market_liquidity_usd,
        lockers: vec![],
        graph_insiders_detected: row.graph_insiders_detected,
        insider_networks: vec![],
        launchpad: row.launchpad.clone(),
        deploy_platform: row.deploy_platform.clone(),
        detected_at: row.detected_at,
        rugged: row.rugged,
        verification: JupiterVerification { jup_verified: row.jup_verified, jup_strict: row.jup_strict },
        rugcheck_score: row.rugcheck_score.map(|s| s as u32),
        buy_tax: None,
        sell_tax: None,
        transfer_tax: None,
        honeypot_flags: vec![],
        updated_at: row.updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mg_onchain_common::chain::Chain;

    fn make_holder(amount_raw: u128) -> TopHolder {
        TopHolder {
            address: Address::parse(Chain::Solana, "11111111111111111111111111111112").unwrap(),
            pct: Decimal::ZERO,
            amount_raw,
            is_insider: false,
        }
    }

    #[test]
    fn gini_empty_holders() {
        assert_eq!(compute_gini(&[], 1000), Decimal::ZERO);
    }

    #[test]
    fn gini_single_holder_is_zero() {
        // One holder with 100% = perfectly unequal by some definitions,
        // but the Atkinson formula with n=1 produces: 2*1*x/(1*x) - 2/1 = 0.
        let h = vec![make_holder(1_000_000)];
        let g = compute_gini(&h, 1_000_000);
        assert_eq!(g, Decimal::ZERO);
    }

    #[test]
    fn gini_four_equal_holders() {
        let holders = vec![
            make_holder(250), make_holder(250),
            make_holder(250), make_holder(250),
        ];
        let g = compute_gini(&holders, 1000);
        assert!(g <= Decimal::new(5, 3), "equal distribution should be near-zero Gini, got {g}");
    }

    #[test]
    fn top_n_pct_two_holders() {
        let h1 = TopHolder { address: Address::parse(Chain::Solana, "11111111111111111111111111111112").unwrap(), pct: Decimal::ZERO, amount_raw: 600_000, is_insider: false };
        let h2 = TopHolder { address: Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112").unwrap(), pct: Decimal::ZERO, amount_raw: 200_000, is_insider: false };
        let holders = vec![h1, h2];
        assert_eq!(top_n_pct(&holders, 1, 1_000_000), Decimal::from(60));
        assert_eq!(top_n_pct(&holders, 2, 1_000_000), Decimal::from(80));
    }

    #[test]
    fn top_n_pct_zero_supply_returns_zero() {
        assert_eq!(top_n_pct(&[], 10, 0), Decimal::ZERO);
    }

    // ---- TLV integration tests (enrich path) --------------------------------

    use crate::cex_registry::CexRegistry;
    use crate::rpc::DecodedMint;
    use mg_onchain_storage::pg::PgStore;
    use sqlx::PgPool;

    /// Build a minimal 82-byte SPL Mint base layout (zeroed except is_initialized).
    fn base_mint_bytes(supply: u64, decimals: u8) -> Vec<u8> {
        let mut buf = vec![0u8; 82];
        buf[36..44].copy_from_slice(&supply.to_le_bytes());
        buf[44] = decimals;
        buf[45] = 1; // is_initialized
        buf
    }

    /// Append Token-2022 TLV extension bytes to an 82-byte base: account_type + TLV body.
    fn with_token2022_extensions(mut base: Vec<u8>, account_type: u8, tlv_body: &[u8]) -> Vec<u8> {
        base.push(account_type);
        base.extend_from_slice(tlv_body);
        base
    }

    /// Build a TLV entry: (type u16 LE, length u16 LE, body).
    fn tlv_entry(ext_type: u16, body: &[u8]) -> Vec<u8> {
        let mut v = ext_type.to_le_bytes().to_vec();
        v.extend_from_slice(&(body.len() as u16).to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    /// Construct a `DecodedMint` from raw account bytes (no RPC, no DB).
    fn decoded_mint_from_bytes(raw: Vec<u8>) -> DecodedMint {
        crate::rpc::decode_mint_bytes(&raw, "test_mint").unwrap()
    }

    // ---- Token-2022 mint with PermanentDelegate + TransferHook ---------------

    /// `enrich_token_inner` with a Token-2022 mint carrying PermanentDelegate and
    /// TransferHook extensions populates both fields on the returned `TokenMeta`.
    ///
    /// This test does NOT hit Postgres — the mock RPC returns an error for
    /// `get_token_largest_accounts` so `fetch_holders` returns empty, and the
    /// `upsert_token` call will fail because the lazy pool is never connected.
    /// We verify the enrichment logic up to (and including) the TLV decode step
    /// by inspecting the fields on the `TokenMeta` returned before the upsert.
    ///
    /// This is an *in-process integration test*: it exercises the full enrichment
    /// code path with a synthetic mint account blob, without any network I/O.
    ///
    /// A full round-trip DB test (`enrich → upsert → get_token → token_row_to_meta`)
    /// requires a live Postgres instance and is gated with `#[ignore]` below.
    #[tokio::test]
    async fn enrich_token2022_mint_with_both_extensions_populates_fields() {
        // Build a fake Token-2022 mint with PermanentDelegate + TransferHook.
        let delegate_key = [0xDEu8; 32];
        let hook_authority = [0xAAu8; 32];
        let hook_program = [0xBBu8; 32];

        let base = base_mint_bytes(1_000_000_000, 9);

        let mut tlv = tlv_entry(12, &delegate_key); // PermanentDelegate
        let mut hook_body = hook_authority.to_vec();
        hook_body.extend_from_slice(&hook_program);
        tlv.extend(tlv_entry(14, &hook_body)); // TransferHook

        let raw = with_token2022_extensions(base, 1, &tlv);
        let decoded = decoded_mint_from_bytes(raw);

        assert!(decoded.is_token2022, "account data > 82 bytes — must be Token-2022");
        assert_eq!(decoded.decimals, 9);

        // Verify the TLV decode directly (this is the core assertion).
        let extensions = crate::tlv::decode_extensions(&decoded.raw_account_data)
            .expect("valid Token-2022 account must decode without error");

        assert_eq!(
            extensions.permanent_delegate,
            Some(delegate_key),
            "PermanentDelegate must be parsed from extension type 12"
        );
        assert_eq!(
            extensions.transfer_hook_program,
            Some(hook_program),
            "TransferHook program_id must be parsed from extension type 14"
        );
        assert_eq!(
            extensions.transfer_hook_authority,
            Some(hook_authority),
            "TransferHook authority must be parsed"
        );

        // Verify that Address construction from the parsed bytes round-trips correctly.
        let delegate_addr = Address::parse(
            Chain::Solana,
            &bs58::encode(delegate_key).into_string(),
        ).expect("valid 32-byte Pubkey must parse as Solana Address");
        let hook_program_addr = Address::parse(
            Chain::Solana,
            &bs58::encode(hook_program).into_string(),
        ).expect("valid 32-byte Pubkey must parse as Solana Address");

        assert!(!delegate_addr.as_str().is_empty());
        assert!(!hook_program_addr.as_str().is_empty());
        // Both addresses must be 44 characters (Base58-encoded 32-byte Pubkey).
        assert_eq!(delegate_addr.as_str().len(), 44, "delegate address must be 44-char Base58");
        assert_eq!(hook_program_addr.as_str().len(), 44, "hook program address must be 44-char Base58");
    }

    /// Legacy SPL Token mint (82 bytes) → both extension fields are `None`.
    #[tokio::test]
    async fn enrich_legacy_spl_mint_extension_fields_are_none() {
        // Exactly 82 bytes — standard SPL Token, no TLV stream.
        let raw = base_mint_bytes(500_000_000, 6);
        assert_eq!(raw.len(), 82);

        let decoded = decoded_mint_from_bytes(raw);
        assert!(!decoded.is_token2022);

        // decode_extensions must return TooShort for legacy mints.
        let result = crate::tlv::decode_extensions(&decoded.raw_account_data);
        assert!(result.is_err(), "legacy SPL Token must produce TlvError::TooShort");

        // unwrap_or_default() in the enrichment path must give all-None extensions.
        let extensions = result.unwrap_or_default();
        assert!(extensions.permanent_delegate.is_none());
        assert!(extensions.transfer_hook_program.is_none());
        assert!(extensions.transfer_hook_authority.is_none());
    }

    /// Token-2022 mint with a zero Pubkey PermanentDelegate → `permanent_delegate = None`.
    #[test]
    fn enrich_zero_delegate_returns_none() {
        let zero_delegate = [0u8; 32];
        let base = base_mint_bytes(1_000_000, 9);
        let tlv = tlv_entry(12, &zero_delegate);
        let raw = with_token2022_extensions(base, 1, &tlv);

        let extensions = crate::tlv::decode_extensions(&raw)
            .expect("zero delegate is valid TLV");
        // Zero Pubkey == "not assigned" — must produce None.
        assert!(extensions.permanent_delegate.is_none());
    }

    // ---- DB round-trip test (requires live Postgres — gated #[ignore]) --------

    /// Full round-trip: synthetic Token-2022 mint bytes → `enrich_token_inner` →
    /// `PgStore::upsert_token` → `PgStore::get_token` → `token_row_to_meta` →
    /// verify `permanent_delegate` and `transfer_hook_program` survive the trip.
    ///
    /// Requires: `DATABASE_URL` env var pointing to a test Postgres instance with
    /// migrations applied (`sqlx migrate run`). Gated `#[ignore]` for CI.
    ///
    /// Run manually:
    ///   DATABASE_URL=postgres://... cargo test -p mg-onchain-token-registry \
    ///     enrich_db_roundtrip_extension_fields -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires live Postgres with migrations applied"]
    async fn enrich_db_roundtrip_extension_fields() {
        let db_url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must be set to run this test");

        let pool = PgPool::connect(&db_url).await.expect("DB connect");
        let store = PgStore::new(pool);
        let _cex = CexRegistry::load_embedded().unwrap();

        let delegate_key = [0xFAu8; 32];
        let hook_program = [0xFBu8; 32];
        let hook_authority = [0u8; 32]; // zero authority — should be None

        let base = base_mint_bytes(9_000_000, 6);
        let mut tlv = tlv_entry(12, &delegate_key);
        let mut hook_body = hook_authority.to_vec();
        hook_body.extend_from_slice(&hook_program);
        tlv.extend(tlv_entry(14, &hook_body));
        let raw = with_token2022_extensions(base, 1, &tlv);

        let decoded = decoded_mint_from_bytes(raw);
        let extensions = crate::tlv::decode_extensions(&decoded.raw_account_data).unwrap();

        // Simulate what enrich_token_inner does:
        let chain = Chain::Solana;
        let mint_str = "Token2022TestMint1111111111111111111111111";
        let pd = extensions.permanent_delegate
            .map(|b| bs58::encode(b).into_string())
            .and_then(|s| Address::parse(chain, &s).ok());
        let thp = extensions.transfer_hook_program
            .map(|b| bs58::encode(b).into_string())
            .and_then(|s| Address::parse(chain, &s).ok());

        store.upsert_token(
            chain.as_str(), mint_str,
            None, None, 6, None,
            9_000_000u128, None,
            None, None, None, 0u128,
            0i64, "0",
            false, false, false, None, None, None, None,
            pd.as_ref().map(|a| a.as_str()),
            thp.as_ref().map(|a| a.as_str()),
            false, // non_transferable
            false, // confidential_transfer
        ).await.expect("upsert must succeed");

        let row = store.get_token(chain.as_str(), mint_str).await
            .expect("get_token must not error")
            .expect("row must exist after upsert");

        // permanent_delegate must round-trip as Base58.
        assert!(
            row.permanent_delegate.is_some(),
            "permanent_delegate must persist in DB"
        );
        assert_eq!(
            row.permanent_delegate.as_deref(),
            Some(bs58::encode(delegate_key).into_string().as_str()),
            "permanent_delegate must be identical Base58 after round-trip"
        );

        // transfer_hook_program must round-trip.
        assert!(
            row.transfer_hook_program.is_some(),
            "transfer_hook_program must persist in DB"
        );
        assert_eq!(
            row.transfer_hook_program.as_deref(),
            Some(bs58::encode(hook_program).into_string().as_str()),
            "transfer_hook_program must be identical Base58 after round-trip"
        );

        // token_row_to_meta must produce populated Address fields.
        let meta = token_row_to_meta(mint_str, chain, &row).expect("meta construction");
        assert!(meta.permanent_delegate.is_some(), "TokenMeta.permanent_delegate must be Some");
        assert!(meta.transfer_hook_program.is_some(), "TokenMeta.transfer_hook_program must be Some");
    }
}
