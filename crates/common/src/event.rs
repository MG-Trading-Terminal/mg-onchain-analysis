//! Core streaming event types: Transfer, Swap, PoolEvent.
//!
//! These are the raw primitives emitted by `crates/chain-adapter` and consumed
//! by `crates/detectors` and `crates/storage`. Each value represents a single
//! on-chain event extracted from a block/slot.
//!
//! # Determinism
//!
//! Fields use `BTreeMap` (not `HashMap`) wherever a key-value bag is needed.
//! `BTreeMap` iterates in sorted key order, ensuring that serialization of the
//! same event always produces the same bytes — a prerequisite for CLAUDE.md
//! Detector Rule #5 (reproducibility).
//!
//! # Amount encoding
//!
//! Raw on-chain units are `u128` with `serde(with = ...)` helpers from
//! [`crate::amount`]. Human-scaled USD fields are `Decimal`. Never `f64`.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::amount::{deserialize_u128_from_str, serialize_u128_as_str};
use crate::chain::{Address, BlockRef, Chain, TxHash};

// ---------------------------------------------------------------------------
// DexKind
// ---------------------------------------------------------------------------

/// Known DEX programs / protocols.
///
/// `#[non_exhaustive]` so Phase 4+ additions (Meteora v2, Jupiter v7, Uniswap v4
/// hooks) do not break existing match arms in consumer crates.
///
/// ## OQ4 resolution
///
/// Unrecognised DEX programs are represented as `Unknown(String)` where the
/// string is the Solana program ID (Base58) or EVM factory address (checksummed
/// hex). Detector policy for `Unknown` events is defined per detector in
/// `crates/detectors` — `crates/common` makes no assumption about how consumers
/// handle this variant.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DexKind {
    RaydiumV4,
    RaydiumClmm,
    /// Raydium Constant Product Market Maker (CPMM) — the simpler 2024 AMM
    /// that does not require OpenBook. Program ID `CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C`.
    RaydiumCpmm,
    OrcaWhirlpool,
    OrcaLegacy,
    Meteora,
    JupiterAggregator,
    PumpFun,
    UniswapV2,
    UniswapV3,
    UniswapV4,
    PancakeSwapV2,
    PancakeSwapV3,
    /// Aerodrome Finance DEX on Base (Velodrome fork).
    ///
    /// Pool factory: `0x420DD381b31aEf6683db6B902084cB0FFEce40DA` (Base mainnet).
    /// Pool ABI: IPool.sol (aerodrome-finance/contracts); `Swap` event verified
    /// via `aerodrome::Swap::SIGNATURE_HASH` in `crates/chain-adapter` tests.
    ///
    /// Authorization: explicitly approved 2026-04-24 sprint briefing as
    /// authorized exception to the `crates/common` freeze.
    Aerodrome,
    /// Catch-all for unrecognised DEX programs.
    ///
    /// The inner string is the Solana program ID (Base58) or EVM factory
    /// address (checksummed hex) for the unrecognised program.
    Unknown(String),
}

// ---------------------------------------------------------------------------
// Transfer
// ---------------------------------------------------------------------------

/// A token transfer — ERC-20 `Transfer(from, to, value)` or SPL token transfer.
///
/// Cross-chain: Solana SPL and EVM ERC-20 both map to this type. The chain
/// adapter normalizes chain-specific representations at the ingestion boundary.
///
/// For EVM ERC-20 transfers via ERC-4337 meta-transactions or proxy contracts,
/// `from` MUST be the economic sender (the wallet paying), not the relayer.
/// The adapter is responsible for tracing through proxy hops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    /// Which chain emitted this event.
    pub chain: Chain,

    /// Transaction that contained this transfer (serialized as string).
    pub tx_hash: TxHash,

    /// Block / slot this transfer was confirmed in.
    pub block: BlockRef,

    /// Wall-clock time of the block. Source of truth for ClickHouse partitioning.
    pub block_time: DateTime<Utc>,

    /// Token mint address (Solana) or contract address (EVM).
    pub token: Address,

    /// Economic sender — zero address indicates a mint event.
    pub from: Address,

    /// Recipient — zero address indicates a burn event.
    pub to: Address,

    /// Raw on-chain transfer amount in the token's native units.
    ///
    /// Serialized as a decimal string to avoid JSON number precision loss.
    #[serde(
        serialize_with = "serialize_u128_as_str",
        deserialize_with = "deserialize_u128_from_str"
    )]
    pub amount_raw: u128,

    /// Token decimal exponent at the time of transfer. Copied from `TokenMeta`
    /// so the event is self-contained for ClickHouse queries.
    pub decimals: u8,

    /// Index within the transaction (EVM log index; Solana instruction index).
    /// Required to uniquely identify a transfer when one tx has multiple transfers.
    pub log_index: u32,
}

impl Transfer {
    /// True if this is a mint event (from the chain's zero address).
    ///
    /// Convention: the adapter sets `from` to the chain's zero/null address
    /// when the token program creates new supply (mint instruction).
    pub fn is_mint(&self) -> bool {
        is_zero_address(&self.from)
    }

    /// True if this is a burn event (to the chain's zero address).
    ///
    /// Convention: the adapter sets `to` to the chain's zero/null address
    /// when the token program destroys supply (burn instruction).
    pub fn is_burn(&self) -> bool {
        is_zero_address(&self.to)
    }
}

/// Returns true if the address is the canonical zero/null address for its chain.
///
/// - Solana: `11111111111111111111111111111111` (System Program / null address)
/// - EVM: `0x0000000000000000000000000000000000000000`
fn is_zero_address(addr: &Address) -> bool {
    match addr.chain {
        Chain::Solana => addr.as_str() == "11111111111111111111111111111111",
        Chain::Ethereum | Chain::Bsc | Chain::Base | Chain::Arbitrum | Chain::Polygon => {
            addr.as_str() == "0x0000000000000000000000000000000000000000"
        }
        Chain::Tron => addr.as_str() == "T9yD14Nj9j7xAB4dbGeiX9h8unkKHxuWwb",
    }
}

// ---------------------------------------------------------------------------
// Swap
// ---------------------------------------------------------------------------

/// A DEX swap event.
///
/// Raydium, Orca, Uniswap v2/v3/v4, PancakeSwap all map into this shape.
/// The DEX adapter normalizes pool-specific event formats at the boundary.
///
/// For Uniswap v3/v4 and Whirlpool (Orca), tick/price range details are not
/// captured here — only the net amounts in/out. Tick data belongs in `PoolEvent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Swap {
    pub chain: Chain,
    pub tx_hash: TxHash,
    pub block: BlockRef,
    pub block_time: DateTime<Utc>,

    /// The LP pool / pair contract address.
    pub pool: Address,

    /// DEX program / router that executed the swap.
    pub dex: DexKind,

    /// Wallet that initiated the swap (economic sender, not aggregator router).
    pub sender: Address,

    /// Token being sold into the pool.
    pub token_in: Address,

    /// Token being received from the pool.
    pub token_out: Address,

    /// Raw amount of `token_in` consumed. Serialized as string.
    #[serde(
        serialize_with = "serialize_u128_as_str",
        deserialize_with = "deserialize_u128_from_str"
    )]
    pub amount_in_raw: u128,

    /// Decimal exponent for `token_in`.
    pub decimals_in: u8,

    /// Raw amount of `token_out` received. Serialized as string.
    #[serde(
        serialize_with = "serialize_u128_as_str",
        deserialize_with = "deserialize_u128_from_str"
    )]
    pub amount_out_raw: u128,

    /// Decimal exponent for `token_out`.
    pub decimals_out: u8,

    /// USD value of the swap at block time. Populated by the indexer from
    /// a price oracle; `None` if no price data is available.
    ///
    /// Uses `Decimal` (not `f64`). `rust_decimal` with `serde-with-str` feature
    /// serializes this as a string automatically.
    pub usd_value: Option<Decimal>,

    /// Index within the transaction.
    pub log_index: u32,
}

// ---------------------------------------------------------------------------
// Token2022Instruction — pre-authorised extension for D07 (P5-5)
// ---------------------------------------------------------------------------

/// Kind of Token-2022 instruction relevant to the withdraw-withheld drain detector.
///
/// Discriminator byte values per the SPL Token-2022 program source:
///   https://github.com/solana-labs/solana-program-library/blob/master/token/program-2022/src/instruction.rs
///
/// This enum is used in [`Token2022InstructionEvent`] and stored as a text field
/// in the `token2022_instructions` Postgres table (V00007 migration).
///
/// # Pre-authorised extension
///
/// Added in P5-5 as part of D07 implementation. `crates/common` is otherwise frozen
/// (see CLAUDE.md §crates/common FROZEN). This variant was pre-authorised in the
/// briefing: "pre-authorised extension of `common::Event` enum — add it."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Token2022InstructionKind {
    /// `WithdrawWithheldTokensFromMint` — discriminator byte 27.
    /// Authority-gated: only `withdraw_withheld_authority` can call.
    /// Moves the mint's accumulated withheld balance to a destination account.
    WithdrawWithheldFromMint,

    /// `WithdrawWithheldTokensFromAccounts` — discriminator byte 28.
    /// Authority-gated: only `withdraw_withheld_authority` can call.
    /// Directly moves withheld balances from listed token accounts to destination.
    WithdrawWithheldFromAccounts,

    /// `HarvestWithheldTokensToMint` — discriminator byte 29.
    /// Permissionless: anyone can call to consolidate withheld balances to the mint.
    /// Does NOT transfer value to the authority; it merely consolidates it at the mint.
    HarvestWithheldToMint,

    /// `SetAuthority { authority_type: WithdrawWithheldTokens }` — base discriminator 6
    /// with `authority_type` byte = 4 (Token-2022 enum value for WithdrawWithheldTokens).
    /// Changes which wallet can call `WithdrawWithheld*` instructions.
    SetAuthorityWithdrawWithheld,
}

impl Token2022InstructionKind {
    /// Return the string representation stored in the `instruction_kind` Postgres column.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            Self::WithdrawWithheldFromMint => "withdraw_withheld_from_mint",
            Self::WithdrawWithheldFromAccounts => "withdraw_withheld_from_accounts",
            Self::HarvestWithheldToMint => "harvest_withheld_to_mint",
            Self::SetAuthorityWithdrawWithheld => "set_authority_withdraw_withheld",
        }
    }
}

impl std::fmt::Display for Token2022InstructionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_db_str())
    }
}

/// A decoded Token-2022 instruction event relevant to D07 (withdraw-withheld drain).
///
/// Emitted by the chain-adapter when it decodes a Token-2022 program instruction
/// in either the top-level or inner (CPI) instructions of a transaction. Stored
/// in the `token2022_instructions` Postgres table (V00007 migration).
///
/// # Pre-authorised extension
///
/// Part of the D07 implementation (P5-5 briefing). `crates/common` is otherwise frozen.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Token2022InstructionEvent {
    /// Which chain emitted this event.
    pub chain: Chain,

    /// Token mint address (Base58 Solana pubkey).
    pub mint: Address,

    /// Transaction containing the instruction.
    pub tx_hash: TxHash,

    /// Block / slot height.
    pub block_height: u64,

    /// Wall-clock time of the block.
    pub block_time: DateTime<Utc>,

    /// Which instruction type was decoded.
    pub kind: Token2022InstructionKind,

    /// The signer of the instruction (the `withdraw_withheld_authority` for
    /// `WithdrawWithheld*` instructions; the current authority for `SetAuthority`).
    /// `None` for `HarvestWithheldToMint` (permissionless, no authority required).
    pub authority: Option<Address>,

    /// Destination token account for `WithdrawWithheld*` instructions.
    /// `None` for `HarvestWithheldToMint` and `SetAuthority*` instructions.
    pub destination: Option<Address>,

    /// Total raw token units extracted.
    /// Populated for `WithdrawWithheld*` instructions; `None` for others.
    ///
    /// Serialized as string per CLAUDE.md u128 convention.
    #[serde(
        serialize_with = "serialize_u128_opt_as_str",
        deserialize_with = "deserialize_u128_opt_from_str"
    )]
    pub amount_raw: Option<u128>,

    /// New authority pubkey for `SetAuthorityWithdrawWithheld`.
    /// `None` if authority is being revoked or for other instruction kinds.
    pub new_authority: Option<Address>,

    /// Previous authority pubkey for `SetAuthorityWithdrawWithheld`.
    /// `None` for other instruction kinds.
    pub prev_authority: Option<Address>,

    /// Instruction log index within the transaction.
    /// CPI instructions use `outer_idx * 1000 + inner_idx` to be unique.
    pub log_index: u32,
}

/// Serialize `Option<u128>` as an optional string.
fn serialize_u128_opt_as_str<S>(v: &Option<u128>, s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match v {
        Some(n) => s.serialize_str(&n.to_string()),
        None => s.serialize_none(),
    }
}

/// Deserialize `Option<u128>` from an optional string.
fn deserialize_u128_opt_from_str<'de, D>(d: D) -> Result<Option<u128>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = serde::Deserialize::deserialize(d)?;
    match opt {
        None => Ok(None),
        Some(s) => s
            .parse::<u128>()
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

// ---------------------------------------------------------------------------
// PoolEvent
// ---------------------------------------------------------------------------

/// An LP pool state event: mint (add liquidity), burn (remove), sync (reserve
/// update), or initialize (pool creation).
///
/// The rug-pull detector primarily consumes `PoolEventKind::Burn` to detect LP
/// drains. The holder-concentration detector uses `PoolEventKind::Sync` to track
/// reserve deltas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolEvent {
    pub chain: Chain,
    pub tx_hash: TxHash,
    pub block: BlockRef,
    pub block_time: DateTime<Utc>,

    /// Pool / pair address.
    pub pool: Address,

    pub dex: DexKind,

    /// The specific kind of LP state change, with payload.
    pub kind: PoolEventKind,

    /// Address of the wallet performing the liquidity operation.
    pub actor: Address,

    /// Index within the transaction for uniqueness.
    pub log_index: u32,
}

/// The payload varies by event kind.
///
/// All raw amounts use `u128`. USD values use `Decimal`.
///
/// `#[non_exhaustive]` because new pool event types (e.g. Uniswap v4 hook events)
/// may be added in Phase 4 without a SemVer major bump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PoolEventKind {
    /// LP token minted (liquidity added).
    Mint {
        #[serde(
            serialize_with = "serialize_u128_as_str",
            deserialize_with = "deserialize_u128_from_str"
        )]
        amount0_raw: u128,
        #[serde(
            serialize_with = "serialize_u128_as_str",
            deserialize_with = "deserialize_u128_from_str"
        )]
        amount1_raw: u128,
        #[serde(
            serialize_with = "serialize_u128_as_str",
            deserialize_with = "deserialize_u128_from_str"
        )]
        lp_tokens_minted: u128,
    },

    /// LP token burned (liquidity removed).
    Burn {
        #[serde(
            serialize_with = "serialize_u128_as_str",
            deserialize_with = "deserialize_u128_from_str"
        )]
        amount0_raw: u128,
        #[serde(
            serialize_with = "serialize_u128_as_str",
            deserialize_with = "deserialize_u128_from_str"
        )]
        amount1_raw: u128,
        #[serde(
            serialize_with = "serialize_u128_as_str",
            deserialize_with = "deserialize_u128_from_str"
        )]
        lp_tokens_burned: u128,
    },

    /// Reserve sync (Uniswap v2 / Raydium style) — full reserve snapshot.
    Sync {
        #[serde(
            serialize_with = "serialize_u128_as_str",
            deserialize_with = "deserialize_u128_from_str"
        )]
        reserve0_raw: u128,
        #[serde(
            serialize_with = "serialize_u128_as_str",
            deserialize_with = "deserialize_u128_from_str"
        )]
        reserve1_raw: u128,
    },

    /// Pool initialization (first liquidity add or pool creation).
    Initialize {
        token0: Address,
        token1: Address,
    },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::Chain;

    fn solana_addr(s: &str) -> Address {
        Address::parse(Chain::Solana, s).unwrap()
    }

    fn dummy_block() -> BlockRef {
        BlockRef::new(Chain::Solana, 300_000_000)
    }

    fn dummy_tx() -> TxHash {
        TxHash::solana_from_base58(&bs58::encode(&[1u8; 64]).into_string()).unwrap()
    }

    // --- Token2022InstructionKind ---

    #[test]
    fn token2022_kind_as_db_str() {
        assert_eq!(
            Token2022InstructionKind::WithdrawWithheldFromMint.as_db_str(),
            "withdraw_withheld_from_mint"
        );
        assert_eq!(
            Token2022InstructionKind::WithdrawWithheldFromAccounts.as_db_str(),
            "withdraw_withheld_from_accounts"
        );
        assert_eq!(
            Token2022InstructionKind::HarvestWithheldToMint.as_db_str(),
            "harvest_withheld_to_mint"
        );
        assert_eq!(
            Token2022InstructionKind::SetAuthorityWithdrawWithheld.as_db_str(),
            "set_authority_withdraw_withheld"
        );
    }

    #[test]
    fn token2022_kind_display() {
        assert_eq!(
            Token2022InstructionKind::WithdrawWithheldFromAccounts.to_string(),
            "withdraw_withheld_from_accounts"
        );
    }

    #[test]
    fn token2022_kind_serde_roundtrip() {
        let kind = Token2022InstructionKind::SetAuthorityWithdrawWithheld;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, r#""set_authority_withdraw_withheld""#);
        let back: Token2022InstructionKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kind);
    }

    #[test]
    fn token2022_instruction_event_serde_roundtrip() {
        let mint = solana_addr("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
        let authority = solana_addr("4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R");
        let tx = dummy_tx();
        let event = Token2022InstructionEvent {
            chain: Chain::Solana,
            mint: mint.clone(),
            tx_hash: tx.clone(),
            block_height: 310_000_000,
            block_time: chrono::DateTime::parse_from_rfc3339("2026-04-21T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            kind: Token2022InstructionKind::WithdrawWithheldFromAccounts,
            authority: Some(authority.clone()),
            destination: Some(authority.clone()),
            amount_raw: Some(1_000_000_000u128),
            new_authority: None,
            prev_authority: None,
            log_index: 0,
        };

        let json = serde_json::to_string(&event).unwrap();
        // u128 must serialize as string
        assert!(json.contains(r#""1000000000""#), "amount_raw must be a string");
        // Round-trip
        let back: Token2022InstructionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.amount_raw, Some(1_000_000_000u128));
        assert_eq!(back.block_height, 310_000_000);
        assert!(back.new_authority.is_none());
    }

    #[test]
    fn token2022_instruction_event_amount_none_serde() {
        // SetAuthority instructions have no amount_raw
        let event = Token2022InstructionEvent {
            chain: Chain::Solana,
            mint: solana_addr("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"),
            tx_hash: dummy_tx(),
            block_height: 100,
            block_time: chrono::Utc::now(),
            kind: Token2022InstructionKind::SetAuthorityWithdrawWithheld,
            authority: None,
            destination: None,
            amount_raw: None,
            new_authority: Some(solana_addr(
                "So11111111111111111111111111111111111111112",
            )),
            prev_authority: Some(solana_addr(
                "So11111111111111111111111111111111111111112",
            )),
            log_index: 1,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: Token2022InstructionEvent = serde_json::from_str(&json).unwrap();
        assert!(back.amount_raw.is_none());
        assert!(back.new_authority.is_some());
    }

    // --- DexKind ---

    #[test]
    fn dex_kind_unknown_serde() {
        let dex = DexKind::Unknown("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P".into());
        let json = serde_json::to_string(&dex).unwrap();
        // Should round-trip
        let back: DexKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, dex);
    }

    #[test]
    fn dex_kind_known_serde_roundtrip() {
        let dex = DexKind::RaydiumV4;
        let json = serde_json::to_string(&dex).unwrap();
        assert_eq!(json, r#""raydium_v4""#);
        let back: DexKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, dex);
    }

    /// DexKind::Aerodrome serializes as "aerodrome" and round-trips cleanly.
    ///
    /// Authorization: crates/common authorized exception 2026-04-24.
    #[test]
    fn dex_kind_aerodrome_serde_roundtrip() {
        let dex = DexKind::Aerodrome;
        let json = serde_json::to_string(&dex).unwrap();
        assert_eq!(json, r#""aerodrome""#, "DexKind::Aerodrome must serialize as \"aerodrome\"");
        let back: DexKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, dex, "DexKind::Aerodrome must round-trip through JSON");
    }

    /// DexKind::Aerodrome is distinct from Unknown("aerodrome-v1").
    ///
    /// This test guards against the placeholder being silently re-introduced:
    /// the chain-adapter mapper must use the proper variant.
    #[test]
    fn dex_kind_aerodrome_not_equal_to_unknown_placeholder() {
        let proper = DexKind::Aerodrome;
        let placeholder = DexKind::Unknown("aerodrome-v1".to_string());
        assert_ne!(
            proper, placeholder,
            "DexKind::Aerodrome must be distinct from Unknown(\"aerodrome-v1\") placeholder"
        );
        // Serialized forms are also distinct.
        let proper_json = serde_json::to_string(&proper).unwrap();
        let placeholder_json = serde_json::to_string(&placeholder).unwrap();
        assert_ne!(proper_json, placeholder_json);
    }

    #[test]
    fn dex_kind_raydium_cpmm_serde_roundtrip() {
        let dex = DexKind::RaydiumCpmm;
        let json = serde_json::to_string(&dex).unwrap();
        assert_eq!(json, r#""raydium_cpmm""#);
        let back: DexKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, dex);
    }

    #[test]
    fn pool_event_kind_mint_serde() {
        let kind = PoolEventKind::Mint {
            amount0_raw: 1_000_000,
            amount1_raw: 2_000_000,
            lp_tokens_minted: 500_000,
        };
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains(r#""kind":"mint""#));
        // u128 values must be strings
        assert!(json.contains(r#""1000000""#));
    }

    #[test]
    fn pool_event_kind_sync_serde() {
        let kind = PoolEventKind::Sync {
            reserve0_raw: u128::MAX,
            reserve1_raw: 0,
        };
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains(r#""kind":"sync""#));
        assert!(json.contains("340282366920938463463374607431768211455"));
    }

    #[test]
    fn transfer_is_mint() {
        let zero = solana_addr("11111111111111111111111111111111");
        let dest = solana_addr("So11111111111111111111111111111111111111112");
        let t = Transfer {
            chain: Chain::Solana,
            tx_hash: dummy_tx(),
            block: dummy_block(),
            block_time: chrono::Utc::now(),
            token: solana_addr("So11111111111111111111111111111111111111112"),
            from: zero,
            to: dest,
            amount_raw: 1_000_000_000,
            decimals: 9,
            log_index: 0,
        };
        assert!(t.is_mint());
        assert!(!t.is_burn());
    }

    #[test]
    fn swap_amount_raw_serialized_as_string() {
        let pool = solana_addr("So11111111111111111111111111111111111111112");
        let sender = solana_addr("So11111111111111111111111111111111111111112");
        let swap = Swap {
            chain: Chain::Solana,
            tx_hash: dummy_tx(),
            block: dummy_block(),
            block_time: chrono::Utc::now(),
            pool: pool.clone(),
            dex: DexKind::RaydiumV4,
            sender: sender.clone(),
            token_in: pool.clone(),
            token_out: sender,
            amount_in_raw: u128::MAX,
            decimals_in: 9,
            amount_out_raw: 1,
            decimals_out: 6,
            usd_value: None,
            log_index: 0,
        };
        let json = serde_json::to_string(&swap).unwrap();
        assert!(json.contains("340282366920938463463374607431768211455"));
    }
}
