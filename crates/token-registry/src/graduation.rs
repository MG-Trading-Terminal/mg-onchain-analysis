//! Launchpad graduation metadata types.
//!
//! # Design
//!
//! Bonding-curve launchpads (Pump.fun / four.meme / Clanker / Virtuals) emit a
//! "graduation" event when a token's TVL reaches the platform threshold, at which
//! point liquidity migrates to a permanent DEX pool. Tokens that just graduated
//! carry elevated pump-and-dump + rug-pull risk in the first hours:
//!
//! > "70% of pump events have accumulation phase" — Karbalaii 2025 (REFERENCES.md D04/pump_dump)
//!
//! These types live in `token-registry` (NOT `common` — see gotcha #1: `common` is FROZEN).
//!
//! `GraduationInfo` is stored in the `tokens.metadata_jsonb` Postgres column via the
//! existing `PgStore::upsert_token_metadata_jsonb` path — no new migration required.
//! See SPEC-NOTE below for storage strategy.
//!
//! # SPEC-NOTE: factory address verification
//!
//! Factory addresses for each launchpad were researched via WebFetch on 2026-04-24. Results:
//!
//! - Pump.fun: Program ID `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P` (already in
//!   `SubscribeFilter::solana_default()` — well-established, no further verification needed).
//! - four.meme (BSC): Token Manager proxy `0x5c952063c7fc8610ffdb798152d69f0b9550762b` via
//!   BscScan. Implementation contract unverified (EIP-1967 proxy). Graduation event signature
//!   not publicly documented; use indirect detection via decoder.
//! - Clanker (Base): Hook contracts v4.1 surfaced via gitbook docs (see launchpad_decoder.rs).
//!   Core factory/deployer contract address NOT confirmed via public docs.
//! - Virtuals (Base): VIRTUAL token `0x0b3e328455c4059eeb9e3f84b5543f74e24e7e1b` confirmed.
//!   Factory/bonding-curve contract address NOT confirmed via public docs.
//!   Virtuals decoder deferred to next sprint.
//!
//! TODO(next-sprint): Verify four.meme graduation event topic0 via decoded TX logs on BscScan.
//! TODO(next-sprint): Verify Clanker factory contract address via clanker-contracts repo.
//! TODO(next-sprint): Verify Virtuals Protocol bonding curve address via basescan + virtuals docs.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use mg_onchain_common::chain::TxHash;

// ---------------------------------------------------------------------------
// Launchpad enum
// ---------------------------------------------------------------------------

/// Bonding-curve launchpad from which a token graduated.
///
/// Serialized as a lowercase string (e.g. `"pump_fun"`, `"four_meme"`).
/// Use `#[serde(rename_all = "snake_case")]` for consistent storage in
/// `tokens.metadata_jsonb`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Launchpad {
    /// Pump.fun — Solana bonding-curve memecoin launchpad.
    ///
    /// Program: `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`
    /// Graduation: TVL reaches ~69 SOL threshold → pool migrated to Raydium AMM v4.
    /// Reference: Karbalaii 2025 arXiv:2504.15790
    PumpFun,

    /// four.meme — BSC bonding-curve memecoin launchpad.
    ///
    /// Token Manager proxy: `0x5c952063c7fc8610ffdb798152d69f0b9550762b` (BSC)
    /// SPEC-NOTE: graduation event topic0 not publicly documented.
    /// Indirect detection: pool init on PancakeSwap V2/V3 for tokens previously
    /// seen on four.meme Token Manager.
    FourMeme,

    /// Clanker.world — AI-agent token launchpad on Base.
    ///
    /// Hook contracts (v4.1): ClankerHookDynamicFeeV2 `0xd60D6B218116cFd801E28F78d011a203D2b068Cc`,
    ///                        ClankerHookStaticFeeV2 `0xb429d62f8f3bFFb98CdB9569533eA23bF0Ba28CC`
    /// SPEC-NOTE: core factory/deployer contract address not confirmed.
    /// Graduation: token liquidity provided to Uniswap V3 pool on Base at deploy time.
    ClankerWorld,

    /// Virtuals Protocol — AI-agent token launchpad on Base.
    ///
    /// VIRTUAL token: `0x0b3e328455c4059eeb9e3f84b5543f74e24e7e1b` (Base)
    /// SPEC-NOTE: factory/bonding curve contract address not confirmed via public docs.
    /// Graduation: bonding curve TVL threshold → permanent Uniswap V3 pool.
    VirtualsProtocol,
}

impl Launchpad {
    /// Return a human-readable name for logging and evidence strings.
    pub fn display_name(self) -> &'static str {
        match self {
            Launchpad::PumpFun => "pump.fun",
            Launchpad::FourMeme => "four.meme",
            Launchpad::ClankerWorld => "clanker.world",
            Launchpad::VirtualsProtocol => "virtuals.io",
        }
    }

    /// Return the canonical chain name for this launchpad.
    pub fn chain_name(self) -> &'static str {
        match self {
            Launchpad::PumpFun => "solana",
            Launchpad::FourMeme => "bsc",
            Launchpad::ClankerWorld | Launchpad::VirtualsProtocol => "base",
        }
    }
}

// ---------------------------------------------------------------------------
// GraduationInfo
// ---------------------------------------------------------------------------

/// Graduation metadata captured when a bonding-curve token migrates to a
/// permanent DEX pool.
///
/// # Storage
///
/// Stored in `tokens.metadata_jsonb` column (existing Postgres path — no new
/// migration required). The column is a JSONB map; this struct is serialized
/// under the key `"graduation"`.
///
/// # Time-source discipline (gotcha #22 / #28)
///
/// `graduation_time` MUST be populated from `block_time` (the block header
/// timestamp), NEVER from `Utc::now()`. Callers are responsible for extracting
/// `block_time` from the indexer event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GraduationInfo {
    /// Which launchpad emitted the graduation event.
    pub launchpad: Launchpad,

    /// Block timestamp of the graduation event (from block_time, NEVER Utc::now()).
    pub graduation_time: DateTime<Utc>,

    /// Block number (slot for Solana, block number for EVM) at graduation.
    pub graduation_block: u64,

    /// Transaction hash of the graduation transaction.
    pub graduation_tx: TxHash,

    /// Initial USD liquidity in the migrated pool at graduation time.
    /// `Decimal::ZERO` when USD value is not available at index time.
    pub initial_liquidity_usd_at_grad: Decimal,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal::Decimal;

    fn fixed_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 24, 12, 0, 0).unwrap()
    }

    fn sample_tx_evm() -> TxHash {
        TxHash::evm_from_hex(
            "0xabc123def456000000000000000000000000000000000000000000000000dead",
        )
        .expect("hardcoded hex must parse")
    }

    fn sample_tx_solana() -> TxHash {
        // 88-char base58-encoded 64-byte Solana signature.
        TxHash::solana_from_base58(
            "5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF52i4pnkjW8kqxG3dGbwMtm",
        )
        .expect("hardcoded base58 must parse")
    }

    /// Basic serde round-trip for GraduationInfo.
    #[test]
    fn graduation_info_serde_roundtrip() {
        let info = GraduationInfo {
            launchpad: Launchpad::PumpFun,
            graduation_time: fixed_time(),
            graduation_block: 300_000_000,
            graduation_tx: sample_tx_solana(),
            initial_liquidity_usd_at_grad: Decimal::new(69_000, 2), // $690.00
        };

        let json = serde_json::to_string(&info).expect("serialization must succeed");
        let back: GraduationInfo = serde_json::from_str(&json).expect("deserialization must succeed");
        assert_eq!(info, back, "GraduationInfo must survive serde round-trip");
    }

    /// Launchpad enum serde round-trip — all variants.
    #[test]
    fn launchpad_enum_serde_roundtrip() {
        let variants = [
            Launchpad::PumpFun,
            Launchpad::FourMeme,
            Launchpad::ClankerWorld,
            Launchpad::VirtualsProtocol,
        ];

        for variant in variants {
            let json = serde_json::to_string(&variant).expect("serialization must succeed");
            let back: Launchpad =
                serde_json::from_str(&json).expect("deserialization must succeed");
            assert_eq!(variant, back, "Launchpad::{variant:?} must survive serde round-trip");
        }
    }

    /// Launchpad serializes as snake_case string.
    #[test]
    fn launchpad_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&Launchpad::PumpFun).unwrap(),
            r#""pump_fun""#
        );
        assert_eq!(
            serde_json::to_string(&Launchpad::FourMeme).unwrap(),
            r#""four_meme""#
        );
        assert_eq!(
            serde_json::to_string(&Launchpad::ClankerWorld).unwrap(),
            r#""clanker_world""#
        );
        assert_eq!(
            serde_json::to_string(&Launchpad::VirtualsProtocol).unwrap(),
            r#""virtuals_protocol""#
        );
    }

    /// GraduationInfo stored under "graduation" key in a JSON map (metadata_jsonb pattern).
    #[test]
    fn graduation_info_stored_in_metadata_map() {
        let info = GraduationInfo {
            launchpad: Launchpad::FourMeme,
            graduation_time: fixed_time(),
            graduation_block: 40_000_000,
            graduation_tx: sample_tx_evm(),
            initial_liquidity_usd_at_grad: Decimal::ZERO,
        };

        let mut map = serde_json::Map::new();
        map.insert(
            "graduation".to_string(),
            serde_json::to_value(&info).expect("must serialize"),
        );
        let json = serde_json::Value::Object(map);
        let back: GraduationInfo = serde_json::from_value(
            json["graduation"].clone(),
        )
        .expect("must deserialize from nested map");
        assert_eq!(info, back);
    }

    /// chain_name returns correct chain for each launchpad.
    #[test]
    fn launchpad_chain_name() {
        assert_eq!(Launchpad::PumpFun.chain_name(), "solana");
        assert_eq!(Launchpad::FourMeme.chain_name(), "bsc");
        assert_eq!(Launchpad::ClankerWorld.chain_name(), "base");
        assert_eq!(Launchpad::VirtualsProtocol.chain_name(), "base");
    }
}
