//! Per-launchpad graduation event decoders.
//!
//! Each decoder converts a raw on-chain log/instruction into a [`GraduationInfo`].
//!
//! # Design
//!
//! Decoders are pure functions: `(raw_input) -> Result<GraduationInfo, DecoderError>`.
//! No I/O. Testable with synthetic fixture data.
//!
//! # SPEC-NOTEs
//!
//! See module-level constants for factory address SPEC-NOTEs arising from incomplete
//! WebFetch research on 2026-04-24.
//!
//! # Time-source discipline (gotcha #22 / #28)
//!
//! All `graduation_time` fields MUST be passed in from the caller's `block_time`.
//! Decoders accept `block_time: DateTime<Utc>` as a parameter and pass it through
//! to `GraduationInfo` unchanged. Decoders NEVER call `Utc::now()`.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tracing::warn;

use mg_onchain_common::chain::TxHash;

use crate::graduation::{GraduationInfo, Launchpad};

// ---------------------------------------------------------------------------
// Pump.fun (Solana)
// ---------------------------------------------------------------------------

/// Pump.fun program ID on Solana.
///
/// Already present in `SubscribeFilter::solana_default()` for general indexing.
/// Used here as the anchor for graduation-event filtering.
///
/// Source: well-established public constant; present in this codebase's
/// `SubscribeFilter::solana_default()` since Sprint N (chain-adapter crate).
pub const PUMP_FUN_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

/// Pump.fun graduation instruction log prefix.
///
/// When a Pump.fun bonding curve graduates (~69 SOL TVL threshold), the program
/// emits a program-log line that starts with this prefix. The instruction is
/// `migrate_to_raydium` or similar.
///
/// SPEC-NOTE: The exact instruction name was not confirmed via public IDL as of
/// 2026-04-24 (pump-fun/pump-fun-program-idl 404). The canonical log prefix
/// `"Program log: Instruction: MigrateToRaydium"` is widely cited in Solana
/// developer community resources and matches observed on-chain behaviour.
/// Must be verified against a live graduation transaction before production deployment.
/// TODO(next-sprint): Verify against confirmed graduation TX in research/launchpad-probes/.
pub const PUMP_FUN_GRADUATION_LOG_PREFIX: &str = "Program log: Instruction: MigrateToRaydium";

/// Alternative graduation log prefix for newer Pump.fun AMM (post-2025).
///
/// SPEC-NOTE: Pump.fun launched its own AMM in 2025 alongside Raydium migration.
/// Some graduations now use `MigrateToAMM`. Both must be detected.
/// TODO(next-sprint): Verify via live transaction inspection.
pub const PUMP_FUN_GRADUATION_LOG_PREFIX_AMM: &str = "Program log: Instruction: MigrateToAMM";

// ---------------------------------------------------------------------------
// four.meme (BSC)
// ---------------------------------------------------------------------------

/// four.meme Token Manager proxy address on BSC.
///
/// Verification attempts (2026-04-24):
/// 1. BscScan https://bscscan.com/address/0x5c952063c7fc8610ffdb798152d69f0b9550762b:
///    Contract confirmed as "Four.meme: Token Manager". ~27.5M transactions on BSC.
///    EIP-1967 transparent proxy. Implementation: 0xecd0807e3bb87963d54ea0f5752c2889db441103 (NOT verified).
/// 2. GitHub four-meme/contracts (https://github.com/four-meme/contracts): 404 — repo not found.
/// 3. GitHub four-meme/launch-protocol: 404 — repo not found.
///
/// SPEC-NOTE: The proxy address is confirmed on BscScan. The implementation contract source is NOT
/// verified, so the graduation event topic0 cannot be derived from public ABI as of 2026-04-24.
/// TODO(next-sprint): Decode a confirmed four.meme graduation TX (BscScan event tab) to extract
/// the graduation event topic0 directly from the encoded log data.
pub const FOUR_MEME_TOKEN_MANAGER: &str = "0x5c952063c7fc8610ffdb798152d69f0b9550762b";

/// four.meme graduation event topic0.
///
/// SPEC-NOTE: Not confirmed via public ABI as of 2026-04-24. Derived from the
/// common pattern for bonding-curve graduation events:
/// `keccak256("TokenGraduated(address,address,uint256)")` → placeholder.
/// Must be replaced with verified value before production deployment.
/// TODO(next-sprint): Decode a confirmed four.meme graduation TX to extract topic0.
pub const FOUR_MEME_GRADUATION_TOPIC0: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000000";

/// four.meme migration/graduation alternate topic: `Launch(address,address,uint256)`.
///
/// SPEC-NOTE: Some BSC bonding-curve platforms emit a `Launch` event on graduation.
/// This is a CANDIDATE topic; not yet verified for four.meme specifically.
/// keccak256("TokenLaunched(address,address,uint256)") = SPEC-NOTE.
pub const FOUR_MEME_LAUNCH_TOPIC0: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000001";

// ---------------------------------------------------------------------------
// Clanker (Base)
// ---------------------------------------------------------------------------

/// Clanker hook contract address (ClankerHookDynamicFeeV2, v4.1) on Base.
///
/// Source: clanker.gitbook.io/clanker-documentation/references/deployed-contracts
/// Verified: 2026-04-24 via WebFetch.
pub const CLANKER_HOOK_DYNAMIC_FEE_V2: &str = "0xd60D6B218116cFd801E28F78d011a203D2b068Cc";

/// Clanker hook contract address (ClankerHookStaticFeeV2, v4.1) on Base.
///
/// Source: clanker.gitbook.io/clanker-documentation/references/deployed-contracts
/// Verified: 2026-04-24 via WebFetch.
pub const CLANKER_HOOK_STATIC_FEE_V2: &str = "0xb429d62f8f3bFFb98CdB9569533eA23bF0Ba28CC";

/// Clanker core factory contract address (v4.0) on Base.
///
/// Source: `clanker.gitbook.io/clanker-documentation/references/deployed-contracts`
/// Verified: 2026-04-24 via WebFetch — gitbook lists `Clanker` contract at this address
/// under Base (chainid 8453) v4.0.0 deployment.
///
/// In v4.1.0 Clanker no longer ships a separate factory; token deployment is exclusively
/// via hook contracts (ClankerHookDynamicFeeV2 / ClankerHookStaticFeeV2 — already
/// confirmed constants above). The v4.0 factory remains the canonical deployment entry
/// point for tokens launched before the v4.1 hook-only migration.
pub const CLANKER_FACTORY_V4: &str = "0xE85A59c628F7d27878ACeB4bf3b35733630083a9";

/// Clanker `TokenCreated` event topic0 on Base.
///
/// **Computed** (Sprint 24 Track 2, 2026-04-24) from the canonical ABI-encoded event
/// signature using `alloy::primitives::keccak256`:
///
/// ```text
/// keccak256("TokenCreated(address,address,address,string,string,string,string,string,int24,address,bytes32,address,address,address,uint256,address[])")
/// = 0x9299d1d1a88d8e1abdc591ae7a167a6bc63a8f17d695804e9091ee33aa89fb67
/// ```
///
/// ## Source
///
/// Event signature extracted from `clanker-sdk/src/abi/v4/Clanker.ts` (GitHub,
/// clanker-devco/clanker-sdk, 2026-04-24). The ABI file lists `TokenCreated` with
/// 16 parameters in this order: `(address,address,address,string,string,string,string,
/// string,int24,address,bytes32,address,address,address,uint256,address[])`.
///
/// Computation verified via `clanker_token_created_topic0_computation` test in
/// `crates/chain-adapter/src/ethereum/decoder.rs` (Sprint 24).
///
/// ## Verification status
///
/// **ABI-DERIVED** (not live-TX-verified). The topic0 is computed from the SDK ABI
/// and has NOT been cross-checked against an actual Clanker deployment TX on Basescan.
/// Use `CLANKER_TOKEN_DEPLOYED_TOPIC0_SPEC_NOTE` for the old SPEC-NOTE sentinel (retired below).
///
/// SPEC-NOTE D10-CLANKER-TOPIC0 (Sprint 24): Live-TX verification is deferred to
/// Sprint 25+. Use `decode_clanker_graduation` only after verifying this topic0
/// matches `topics[0]` of a real Clanker deployment TX from factory
/// `0xE85A59c628F7d27878ACeB4bf3b35733630083a9` on Basescan.
///
/// TODO(next-sprint): Fetch a confirmed Clanker deployment TX hash from Basescan
/// event tab of `CLANKER_FACTORY_V4`, decode topic0, compare to this const.
pub const CLANKER_TOKEN_CREATED_TOPIC0: &str =
    "0x9299d1d1a88d8e1abdc591ae7a167a6bc63a8f17d695804e9091ee33aa89fb67";

/// Retired SPEC-NOTE sentinel — kept for backwards compat in `decode_clanker_graduation`.
///
/// The decoder's SPEC-NOTE guard (`starts_with("SPEC-NOTE")`) still activates to
/// prevent the decoder from matching live events until live-TX verification is done.
/// Remove this constant and update the guard when `CLANKER_TOKEN_CREATED_TOPIC0`
/// is confirmed against a real TX (Sprint 25+).
pub const CLANKER_TOKEN_DEPLOYED_TOPIC0_SPEC_NOTE: &str = "SPEC-NOTE:unverified";

// ---------------------------------------------------------------------------
// Virtuals Protocol (Base)
// ---------------------------------------------------------------------------

/// VIRTUAL token contract address on Base.
///
/// Source: BaseScan search, 2026-04-24. This is the governance/protocol token,
/// NOT the factory contract.
pub const VIRTUALS_TOKEN_BASE: &str = "0x0b3e328455c4059eeb9e3f84b5543f74e24e7e1b";

/// Virtuals Protocol factory/bonding-curve contract address on Base.
///
/// Verification attempts (2026-04-24):
/// 1. docs.virtuals.io/developer-resources/contract-addresses: ECONNREFUSED.
/// 2. GitHub Virtual-Protocol/protocol-contracts:
///    Repo found. Contracts include AgentFactory, AgentNft, AgentToken, AgentDAO, etc.
///    Deployment addresses NOT listed in the repo README or visible JSON files.
///    Base chain deployment JSON (deployments/base.json, scripts/deploy.json): 404.
/// 3. VIRTUAL token: 0x0b3e328455c4059eeb9e3f84b5543f74e24e7e1b (Base) — confirmed governance token.
///    AgentFactory contract address on Base: NOT publicly documented as of 2026-04-24.
///
/// SPEC-NOTE: Virtuals Protocol uses an AgentFactory pattern. The exact factory address on Base and
/// its graduation event signature are NOT confirmed from public sources as of 2026-04-24.
/// TODO(next-sprint): Query Basescan for TXs to VIRTUAL token → find AgentFactory deployer.
pub const VIRTUALS_FACTORY_SPEC_NOTE: &str = "SPEC-NOTE:unverified";

/// Virtuals Protocol graduation event topic0 on Base.
///
/// SPEC-NOTE: NOT confirmed as of 2026-04-24.
/// TODO(next-sprint): Decode confirmed Virtuals graduation TX.
pub const VIRTUALS_GRADUATION_TOPIC0_SPEC_NOTE: &str = "SPEC-NOTE:unverified";

// ---------------------------------------------------------------------------
// Decoder error
// ---------------------------------------------------------------------------

/// Errors that can occur during graduation event decoding.
#[derive(Debug, thiserror::Error)]
pub enum DecoderError {
    #[error("graduation log not found in program logs")]
    LogNotFound,

    #[error("insufficient log data: {0}")]
    InsufficientData(String),

    #[error("graduation event topic0 not matched (spec-note: {0})")]
    SpecNoteUnverified(&'static str),
}

// ---------------------------------------------------------------------------
// Pump.fun decoder (Solana)
// ---------------------------------------------------------------------------

/// Synthetic representation of a Pump.fun graduation program log entry.
///
/// In production, the indexer provides this from the `TransactionMeta.log_messages`
/// field of a confirmed Solana transaction.
#[derive(Debug)]
pub struct PumpFunGraduationLog<'a> {
    /// All program log lines from the graduation transaction.
    pub log_messages: &'a [String],
    /// Transaction hash (base58 Solana signature).
    pub tx_hash: TxHash,
    /// Block slot number.
    pub slot: u64,
    /// Block timestamp from the indexer (NEVER Utc::now()).
    pub block_time: DateTime<Utc>,
    /// Liquidity USD at graduation time, if known from pool init event.
    /// Zero if not yet resolved.
    pub initial_liquidity_usd: Decimal,
}

/// Decode a Pump.fun graduation from program log messages.
///
/// Returns `Ok(GraduationInfo)` when the graduation log prefix is found in
/// `log_messages`. Returns `DecoderError::LogNotFound` otherwise.
///
/// # Note on graduation detection
///
/// Pump.fun graduation can be detected in two ways:
/// 1. **Direct**: The bonding curve program emits a log containing
///    `PUMP_FUN_GRADUATION_LOG_PREFIX` in the same tx.
/// 2. **Indirect**: A Raydium pool is initialized for a token whose mint was
///    previously seen transacting with the Pump.fun program. This fallback
///    is handled by the `PoolInitializeHook` path (existing hook).
///
/// This decoder handles method (1). Method (2) is a complementary path.
pub fn decode_pump_fun_graduation(
    log: &PumpFunGraduationLog<'_>,
) -> Result<GraduationInfo, DecoderError> {
    let has_graduation = log.log_messages.iter().any(|line| {
        line.contains(PUMP_FUN_GRADUATION_LOG_PREFIX)
            || line.contains(PUMP_FUN_GRADUATION_LOG_PREFIX_AMM)
    });

    if !has_graduation {
        return Err(DecoderError::LogNotFound);
    }

    Ok(GraduationInfo {
        launchpad: Launchpad::PumpFun,
        graduation_time: log.block_time,
        graduation_block: log.slot,
        graduation_tx: log.tx_hash.clone(),
        initial_liquidity_usd_at_grad: log.initial_liquidity_usd,
    })
}

// ---------------------------------------------------------------------------
// four.meme decoder (BSC)
// ---------------------------------------------------------------------------

/// Synthetic representation of a four.meme graduation EVM log entry.
#[derive(Debug)]
pub struct FourMemeGraduationLog<'a> {
    /// Emitting contract address (checksummed hex).
    pub contract: &'a str,
    /// First log topic (topic0 = event signature).
    pub topic0: &'a str,
    /// Token address (from log topics or data).
    pub token_address: &'a str,
    /// Pool address that received the graduated liquidity.
    pub pool_address: &'a str,
    /// Transaction hash (EVM hex).
    pub tx_hash: TxHash,
    /// Block number.
    pub block_number: u64,
    /// Block timestamp (from block header, NEVER Utc::now()).
    pub block_time: DateTime<Utc>,
    /// Initial liquidity USD (from pool quote if available).
    pub initial_liquidity_usd: Decimal,
}

/// Decode a four.meme graduation from an EVM log entry.
///
/// # SPEC-NOTE
///
/// The four.meme graduation event topic0 (`FOUR_MEME_GRADUATION_TOPIC0`) is NOT
/// verified as of 2026-04-24. This decoder matches against the known Token Manager
/// contract address and uses a placeholder topic0 sentinel check.
///
/// In production, until the verified topic0 is available, this decoder will
/// return `DecoderError::SpecNoteUnverified`. The fallback is indirect detection:
/// any PancakeSwap pool init involving a token previously seen on the four.meme
/// Token Manager triggers the graduation path via `PoolInitializeHook`.
pub fn decode_four_meme_graduation(
    log: &FourMemeGraduationLog<'_>,
) -> Result<GraduationInfo, DecoderError> {
    // Guard: must come from the known Token Manager contract.
    let contract_lower = log.contract.to_lowercase();
    if contract_lower != FOUR_MEME_TOKEN_MANAGER.to_lowercase() {
        return Err(DecoderError::LogNotFound);
    }

    // SPEC-NOTE: topic0 not verified — placeholder sentinel guards the hot path.
    if FOUR_MEME_GRADUATION_TOPIC0.starts_with("0x000000000000000000000000000000000000000000") {
        warn!(
            contract = log.contract,
            "four.meme graduation topic0 not verified (SPEC-NOTE) — decoder inactive until verified"
        );
        return Err(DecoderError::SpecNoteUnverified(
            "four.meme graduation topic0 must be verified before production use",
        ));
    }

    if log.topic0.to_lowercase() != FOUR_MEME_GRADUATION_TOPIC0.to_lowercase()
        && log.topic0.to_lowercase() != FOUR_MEME_LAUNCH_TOPIC0.to_lowercase()
    {
        return Err(DecoderError::LogNotFound);
    }

    Ok(GraduationInfo {
        launchpad: Launchpad::FourMeme,
        graduation_time: log.block_time,
        graduation_block: log.block_number,
        graduation_tx: log.tx_hash.clone(),
        initial_liquidity_usd_at_grad: log.initial_liquidity_usd,
    })
}

// ---------------------------------------------------------------------------
// Clanker decoder (Base)
// ---------------------------------------------------------------------------

/// Synthetic representation of a Clanker token deployment / graduation EVM log.
#[derive(Debug)]
pub struct ClankerGraduationLog<'a> {
    /// Emitting contract address.
    pub contract: &'a str,
    /// topic0 of the deployment event.
    pub topic0: &'a str,
    /// Deployed token address.
    pub token_address: &'a str,
    /// Uniswap V3 pool address created at graduation.
    pub pool_address: &'a str,
    /// Transaction hash (EVM hex).
    pub tx_hash: TxHash,
    /// Block number.
    pub block_number: u64,
    /// Block timestamp (from block header, NEVER Utc::now()).
    pub block_time: DateTime<Utc>,
    /// Initial pool liquidity USD at deployment.
    pub initial_liquidity_usd: Decimal,
}

/// Decode a Clanker token graduation from an EVM log entry.
///
/// # SPEC-NOTE
///
/// The Clanker core factory address and `TokenDeployed` event topic0 are NOT
/// verified as of 2026-04-24 (see `CLANKER_FACTORY_SPEC_NOTE`).
///
/// This decoder currently returns `DecoderError::SpecNoteUnverified` for all
/// inputs. Once the factory address and topic0 are verified, replace the
/// sentinel constants and this guard.
pub fn decode_clanker_graduation(
    log: &ClankerGraduationLog<'_>,
) -> Result<GraduationInfo, DecoderError> {
    // Guard: must originate from a known Clanker contract.
    // CLANKER_FACTORY_V4 is verified (gitbook 2026-04-24).
    // Hook contracts (ClankerHookDynamicFeeV2 / ClankerHookStaticFeeV2) are also
    // valid origins for v4.1 deployments.
    let contract_lower = log.contract.to_lowercase();
    let known_contracts = [
        CLANKER_FACTORY_V4,
        CLANKER_HOOK_DYNAMIC_FEE_V2,
        CLANKER_HOOK_STATIC_FEE_V2,
    ];
    if !known_contracts.iter().any(|c| c.to_lowercase() == contract_lower) {
        return Err(DecoderError::LogNotFound);
    }

    // SPEC-NOTE: TokenDeployed topic0 is still unverified — the factory address is
    // now confirmed but the event ABI has not been decoded from a live TX.
    // This guard keeps the decoder inactive on the event-matching path until topic0 is
    // confirmed. Contract-address filtering above is safe for allow-listing.
    if CLANKER_TOKEN_DEPLOYED_TOPIC0_SPEC_NOTE.starts_with("SPEC-NOTE") {
        warn!(
            contract = log.contract,
            "Clanker TokenDeployed topic0 not verified (SPEC-NOTE) — decoder inactive until topic0 confirmed"
        );
        return Err(DecoderError::SpecNoteUnverified(
            "Clanker TokenDeployed topic0 must be verified before production use",
        ));
    }

    Ok(GraduationInfo {
        launchpad: Launchpad::ClankerWorld,
        graduation_time: log.block_time,
        graduation_block: log.block_number,
        graduation_tx: log.tx_hash.clone(),
        initial_liquidity_usd_at_grad: log.initial_liquidity_usd,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixed_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 24, 12, 0, 0).unwrap()
    }

    fn solana_tx() -> TxHash {
        TxHash::solana_from_base58(
            "5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF52i4pnkjW8kqxG3dGbwMtm",
        )
        .expect("hardcoded base58 must parse")
    }

    fn evm_tx() -> TxHash {
        TxHash::evm_from_hex(
            "0xabc123def456000000000000000000000000000000000000000000000000dead",
        )
        .expect("hardcoded hex must parse")
    }

    // ------------------------------------------------------------------
    // Pump.fun decoder
    // ------------------------------------------------------------------

    /// Happy path: MigrateToRaydium log present → GraduationInfo returned.
    #[test]
    fn pump_fun_decoder_happy_path_raydium() {
        let logs = vec![
            "Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P invoke [1]".to_string(),
            PUMP_FUN_GRADUATION_LOG_PREFIX.to_string(),
            "Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P success".to_string(),
        ];
        let log = PumpFunGraduationLog {
            log_messages: &logs,
            tx_hash: solana_tx(),
            slot: 300_000_000,
            block_time: fixed_time(),
            initial_liquidity_usd: Decimal::new(6900, 2), // $69.00
        };

        let result = decode_pump_fun_graduation(&log).expect("must decode");
        assert_eq!(result.launchpad, Launchpad::PumpFun);
        assert_eq!(result.graduation_block, 300_000_000);
        assert_eq!(result.graduation_time, fixed_time());
        assert_eq!(result.initial_liquidity_usd_at_grad, Decimal::new(6900, 2));
    }

    /// Happy path: MigrateToAMM log present → GraduationInfo returned (newer Pump.fun AMM).
    #[test]
    fn pump_fun_decoder_happy_path_amm() {
        let logs = vec![
            "Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P invoke [1]".to_string(),
            PUMP_FUN_GRADUATION_LOG_PREFIX_AMM.to_string(),
        ];
        let log = PumpFunGraduationLog {
            log_messages: &logs,
            tx_hash: solana_tx(),
            slot: 300_000_001,
            block_time: fixed_time(),
            initial_liquidity_usd: Decimal::ZERO,
        };

        let result = decode_pump_fun_graduation(&log).expect("AMM path must decode");
        assert_eq!(result.launchpad, Launchpad::PumpFun);
    }

    /// Negative path: no graduation log → LogNotFound.
    #[test]
    fn pump_fun_decoder_no_graduation_log() {
        let logs = vec![
            "Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P invoke [1]".to_string(),
            "Program log: Instruction: Buy".to_string(),
        ];
        let log = PumpFunGraduationLog {
            log_messages: &logs,
            tx_hash: solana_tx(),
            slot: 300_000_002,
            block_time: fixed_time(),
            initial_liquidity_usd: Decimal::ZERO,
        };

        assert!(
            matches!(decode_pump_fun_graduation(&log), Err(DecoderError::LogNotFound)),
            "non-graduation tx must return LogNotFound"
        );
    }

    /// Empty log list → LogNotFound.
    #[test]
    fn pump_fun_decoder_empty_logs() {
        let log = PumpFunGraduationLog {
            log_messages: &[],
            tx_hash: solana_tx(),
            slot: 1,
            block_time: fixed_time(),
            initial_liquidity_usd: Decimal::ZERO,
        };
        assert!(matches!(decode_pump_fun_graduation(&log), Err(DecoderError::LogNotFound)));
    }

    // ------------------------------------------------------------------
    // four.meme decoder
    // ------------------------------------------------------------------

    /// four.meme decoder returns SpecNoteUnverified because topic0 is placeholder.
    #[test]
    fn four_meme_decoder_returns_spec_note_until_topic0_verified() {
        let log = FourMemeGraduationLog {
            contract: FOUR_MEME_TOKEN_MANAGER,
            topic0: "0xdeadbeef00000000000000000000000000000000000000000000000000000001",
            token_address: "0x1234000000000000000000000000000000000001",
            pool_address: "0x1234000000000000000000000000000000000002",
            tx_hash: evm_tx(),
            block_number: 40_000_000,
            block_time: fixed_time(),
            initial_liquidity_usd: Decimal::ZERO,
        };

        // Expected: SpecNoteUnverified (topic0 placeholder detected)
        assert!(
            matches!(
                decode_four_meme_graduation(&log),
                Err(DecoderError::SpecNoteUnverified(_))
            ),
            "four.meme decoder must return SpecNoteUnverified until topic0 is verified"
        );
    }

    /// four.meme decoder returns LogNotFound for wrong contract address.
    #[test]
    fn four_meme_decoder_wrong_contract_returns_not_found() {
        let log = FourMemeGraduationLog {
            contract: "0xdeadbeef00000000000000000000000000000001",
            topic0: FOUR_MEME_GRADUATION_TOPIC0,
            token_address: "0x1234000000000000000000000000000000000001",
            pool_address: "0x1234000000000000000000000000000000000002",
            tx_hash: evm_tx(),
            block_number: 40_000_001,
            block_time: fixed_time(),
            initial_liquidity_usd: Decimal::ZERO,
        };
        assert!(matches!(
            decode_four_meme_graduation(&log),
            Err(DecoderError::LogNotFound)
        ));
    }

    // ------------------------------------------------------------------
    // Clanker decoder
    // ------------------------------------------------------------------

    /// Clanker decoder: known factory address (v4.0) → SpecNoteUnverified (topic0 still pending).
    ///
    /// Factory `CLANKER_FACTORY_V4` is now verified (gitbook 2026-04-24).
    /// The decoder passes the contract-address gate but still returns SpecNoteUnverified
    /// because the TokenDeployed topic0 has not yet been confirmed from a live TX.
    #[test]
    fn clanker_decoder_known_factory_returns_spec_note_until_topic0_verified() {
        let log = ClankerGraduationLog {
            // CLANKER_FACTORY_V4 — verified via gitbook 2026-04-24.
            contract: CLANKER_FACTORY_V4,
            topic0: "0xdeadbeef00000000000000000000000000000000000000000000000000000002",
            token_address: "0x1234000000000000000000000000000000000003",
            pool_address: "0x1234000000000000000000000000000000000004",
            tx_hash: evm_tx(),
            block_number: 25_000_000,
            block_time: fixed_time(),
            initial_liquidity_usd: Decimal::new(100_000, 2),
        };

        assert!(
            matches!(
                decode_clanker_graduation(&log),
                Err(DecoderError::SpecNoteUnverified(_))
            ),
            "Clanker decoder must return SpecNoteUnverified until TokenDeployed topic0 is verified"
        );
    }

    /// Clanker decoder: unknown contract → LogNotFound.
    #[test]
    fn clanker_decoder_unknown_contract_returns_log_not_found() {
        let log = ClankerGraduationLog {
            contract: "0x0000000000000000000000000000000000000001",
            topic0: "0xdeadbeef00000000000000000000000000000000000000000000000000000002",
            token_address: "0x1234000000000000000000000000000000000003",
            pool_address: "0x1234000000000000000000000000000000000004",
            tx_hash: evm_tx(),
            block_number: 25_000_001,
            block_time: fixed_time(),
            initial_liquidity_usd: Decimal::ZERO,
        };

        assert!(
            matches!(decode_clanker_graduation(&log), Err(DecoderError::LogNotFound)),
            "Clanker decoder must return LogNotFound for unknown contract address"
        );
    }

    /// Clanker decoder: hook contract (DynamicFeeV2) also accepted as known origin.
    #[test]
    fn clanker_decoder_hook_contract_returns_spec_note_until_topic0_verified() {
        let log = ClankerGraduationLog {
            // Hook contract — verified via gitbook 2026-04-24.
            contract: CLANKER_HOOK_DYNAMIC_FEE_V2,
            topic0: "0xdeadbeef00000000000000000000000000000000000000000000000000000003",
            token_address: "0x1234000000000000000000000000000000000005",
            pool_address: "0x1234000000000000000000000000000000000006",
            tx_hash: evm_tx(),
            block_number: 25_000_002,
            block_time: fixed_time(),
            initial_liquidity_usd: Decimal::new(5_000, 0),
        };

        assert!(
            matches!(
                decode_clanker_graduation(&log),
                Err(DecoderError::SpecNoteUnverified(_))
            ),
            "Clanker hook contract must also pass contract gate but still return SpecNoteUnverified"
        );
    }

    // ------------------------------------------------------------------
    // Track 2 (Sprint 24): Clanker TokenCreated topic0 verification
    // ------------------------------------------------------------------

    /// Sprint 24 Track 2: verify `CLANKER_TOKEN_CREATED_TOPIC0` format.
    ///
    /// The value was computed via `alloy::primitives::keccak256` in
    /// `crates/chain-adapter` from the clanker-sdk v4 ABI (2026-04-24).
    /// This test verifies the string form is valid hex + correct length.
    ///
    /// Live-TX verification deferred to Sprint 25 (SPEC-NOTE D10-CLANKER-TOPIC0).
    #[test]
    fn clanker_token_created_topic0_format_valid() {
        let topic0 = CLANKER_TOKEN_CREATED_TOPIC0;
        assert!(
            topic0.starts_with("0x"),
            "CLANKER_TOKEN_CREATED_TOPIC0 must start with '0x', got: {topic0}"
        );
        assert_eq!(
            topic0.len(),
            66,
            "CLANKER_TOKEN_CREATED_TOPIC0 must be 66 chars (0x + 64 hex), got len={}", topic0.len()
        );
        // Verify all chars after 0x are valid lowercase hex.
        let hex_part = &topic0[2..];
        assert!(
            hex_part.chars().all(|c| c.is_ascii_hexdigit()),
            "CLANKER_TOKEN_CREATED_TOPIC0 hex portion must be valid hex, got: {hex_part}"
        );
        // Record the value for cross-checking (printed in test output).
        println!("CLANKER_TOKEN_CREATED_TOPIC0 (ABI-derived): {topic0}");
        println!("SPEC-NOTE D10-CLANKER-TOPIC0: live-TX verification deferred to Sprint 25");
    }

    /// Sprint 24 Track 2: SPEC-NOTE sentinel constant has expected prefix.
    ///
    /// Guards that the `decode_clanker_graduation` SpecNoteUnverified guard
    /// (`starts_with("SPEC-NOTE")`) correctly activates for the retired sentinel.
    #[test]
    fn clanker_spec_note_sentinel_has_spec_note_prefix() {
        assert!(
            CLANKER_TOKEN_DEPLOYED_TOPIC0_SPEC_NOTE.starts_with("SPEC-NOTE"),
            "retired sentinel must start with 'SPEC-NOTE' to activate the decoder guard"
        );
    }
}
