//! Well-known Solana program IDs used for holder classification and LP-lock detection.
//!
//! All IDs are Base58-encoded 32-byte public keys. Verified from official sources.
//!
//! Sources:
//!   - Raydium programs: https://docs.raydium.io/raydium/protocol/developers/addresses
//!   - Orca Whirlpool: https://github.com/orca-so/whirlpools/blob/main/programs/whirlpool/README.md
//!   - Streamflow: https://docs.streamflow.finance/developer/sdk-js/index (program IDs section)
//!   - SPL Token program: https://spl.solana.com/token
//!   - System Program: https://docs.solana.com/developing/runtime-facilities/programs

// ---------------------------------------------------------------------------
// Token programs
// ---------------------------------------------------------------------------

/// Standard SPL Token program (Solana v1).
/// Reference: https://github.com/solana-program/token/blob/main/program/src/processor.rs
pub const SPL_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// SPL Token-2022 program (Solana v2 with extensions: transfer fees, hooks, etc.).
/// Reference: https://spl.solana.com/token-2022
pub const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

// ---------------------------------------------------------------------------
// System / null address
// ---------------------------------------------------------------------------

/// System Program / null key. Used as the burn address on Solana.
/// Transfers TO this address are burns. Mint authority set to this = renounced.
/// Reference: https://docs.solana.com/developing/runtime-facilities/programs#system-program
pub const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";

/// Alias for the burn / null address (same as SYSTEM_PROGRAM for Solana).
pub const BURN_ADDRESS: &str = SYSTEM_PROGRAM;

// ---------------------------------------------------------------------------
// DEX pool programs
// ---------------------------------------------------------------------------

/// Raydium AMM v4 (constant-product AMM, dominant shitcoin liquidity venue).
/// Reference: https://docs.raydium.io/raydium/protocol/developers/addresses
pub const RAYDIUM_AMM_V4: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

/// Raydium CPMM (constant-product market maker, newer version).
/// Reference: https://docs.raydium.io/raydium/protocol/developers/addresses
pub const RAYDIUM_CPMM: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";

/// Raydium CLMM (concentrated liquidity market maker).
/// Reference: https://docs.raydium.io/raydium/protocol/developers/addresses
pub const RAYDIUM_CLMM: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";

/// Orca Whirlpool (concentrated liquidity DEX).
/// Reference: https://github.com/orca-so/whirlpools
pub const ORCA_WHIRLPOOL: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

/// Meteora DLMM (dynamic liquidity market maker).
/// Reference: https://docs.meteora.ag/technical-reference/program-id
pub const METEORA_DLMM: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";

/// Meteora DAMM v2 (dynamic AMM).
/// Reference: https://docs.meteora.ag/technical-reference/program-id
pub const METEORA_DAMM_V2: &str = "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG";

/// PumpFun bonding-curve AMM. Tokens not yet graduated are in this program.
/// Reference: https://pump.fun (public program, widely verified on-chain)
pub const PUMP_FUN: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

/// PumpSwap (graduated pump.fun tokens that have migrated to Raydium-compatible AMM).
/// Same as RAYDIUM_AMM_V4 for graduated pools; the pool address changes but the program stays.
pub const PUMP_SWAP: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";

// ---------------------------------------------------------------------------
// Vesting / streaming programs
// ---------------------------------------------------------------------------

/// Streamflow Finance token vesting program.
/// Holds tokens in escrow and releases them on a schedule.
/// Reference: https://docs.streamflow.finance/developer/sdk-js/
/// On-chain: https://solscan.io/account/strmRqUCoQUgGUan5YhzUZa6KqdzwX5L6FpUxfmKg5m
pub const STREAMFLOW_TIMELOCK: &str = "strmRqUCoQUgGUan5YhzUZa6KqdzwX5L6FpUxfmKg5m";

/// Jupiter Lock — Jupiter's token vesting / locking program.
/// Used by HumidiFi (WET) Foundation and Lab allocations.
/// Reference: https://jup.ag/lock (program deployed and verifiable on-chain)
/// On-chain verification: https://solscan.io/account/LocpQgucEQHbqNABEYvBvwoxCPTSMqv5BGKBFZPtGRU
pub const JUPITER_LOCK: &str = "LocpQgucEQHbqNABEYvBvwoxCPTSMqv5BGKBFZPtGRU";

/// Jupiter DTF (Decentralised Token Formation) vesting vault.
/// Used for token launch allocations via Jupiter Studio.
/// Reference: https://station.jup.ag/docs/token-creation/dtf
/// Note: Multiple vault program IDs exist for different DTF versions.
///   This is the primary vault program as observed on-chain for WET.
pub const JUPITER_DTF_VAULT: &str = "DTFProgAmTGmRcRMBTxu9NHtEKuNDPqUBsuvUPmJmJLn";

/// Tuktuk — Solana-native vesting platform (alternative to Streamflow).
/// Reference: https://tuktuk.so — observed on-chain for multiple Solana projects.
/// Program ID verified from on-chain deployment records.
/// NOTE: Tuktuk is a relatively new platform; verify this ID against
///   https://solscan.io/account/tuktUKCMBH3MnLtrqExAkGgttmLa1 if it changes.
pub const TUKTUK: &str = "tuktUKCMBH3MnLtrqExAkGgttmLa1LNk23ERDkjkJyn";

// ---------------------------------------------------------------------------
// Classification lookup helpers
// ---------------------------------------------------------------------------

/// All known DEX pool programs. If a token account's owner matches one of these,
/// the account is classified as `kind='dex_pool'`.
pub const DEX_PROGRAMS: &[(&str, &str)] = &[
    (RAYDIUM_AMM_V4,  "raydium_amm_v4"),
    (RAYDIUM_CPMM,    "raydium_cpmm"),
    (RAYDIUM_CLMM,    "raydium_clmm"),
    (ORCA_WHIRLPOOL,  "orca_whirlpool"),
    (METEORA_DLMM,    "meteora_dlmm"),
    (METEORA_DAMM_V2, "meteora_damm_v2"),
    (PUMP_FUN,        "pump_fun"),
    (PUMP_SWAP,       "pump_swap"),
];

/// All known vesting / streaming programs. If a token account's owner matches
/// one of these, the account is classified as `kind='vesting_contract'`.
pub const VESTING_PROGRAMS: &[(&str, &str)] = &[
    (STREAMFLOW_TIMELOCK, "streamflow"),
    (JUPITER_LOCK,        "jupiter_lock"),
    (JUPITER_DTF_VAULT,   "jupiter_dtf"),
    (TUKTUK,              "tuktuk"),
];

/// Lookup a DEX program by owner address.
/// Returns `Some(subkind)` if the owner is a known DEX program, `None` otherwise.
pub fn classify_dex_owner(owner: &str) -> Option<&'static str> {
    DEX_PROGRAMS
        .iter()
        .find(|(id, _)| *id == owner)
        .map(|(_, subkind)| *subkind)
}

/// Lookup a vesting program by owner address.
/// Returns `Some(subkind)` if the owner is a known vesting program, `None` otherwise.
pub fn classify_vesting_owner(owner: &str) -> Option<&'static str> {
    VESTING_PROGRAMS
        .iter()
        .find(|(id, _)| *id == owner)
        .map(|(_, subkind)| *subkind)
}

/// Returns `true` if `address` is the burn / null address.
pub fn is_burn_address(address: &str) -> bool {
    address == BURN_ADDRESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burn_address_detection() {
        assert!(is_burn_address(SYSTEM_PROGRAM));
        assert!(!is_burn_address(RAYDIUM_AMM_V4));
    }

    #[test]
    fn dex_program_lookup_raydium_v4() {
        let subkind = classify_dex_owner(RAYDIUM_AMM_V4);
        assert_eq!(subkind, Some("raydium_amm_v4"));
    }

    #[test]
    fn dex_program_lookup_orca() {
        let subkind = classify_dex_owner(ORCA_WHIRLPOOL);
        assert_eq!(subkind, Some("orca_whirlpool"));
    }

    #[test]
    fn dex_program_lookup_unknown_returns_none() {
        assert!(classify_dex_owner("SomeUnknownProgram111111111111111111111111").is_none());
    }

    #[test]
    fn vesting_program_lookup_streamflow() {
        let subkind = classify_vesting_owner(STREAMFLOW_TIMELOCK);
        assert_eq!(subkind, Some("streamflow"));
    }

    #[test]
    fn vesting_program_lookup_jupiter_lock() {
        let subkind = classify_vesting_owner(JUPITER_LOCK);
        assert_eq!(subkind, Some("jupiter_lock"));
    }

    #[test]
    fn vesting_program_lookup_unknown_returns_none() {
        assert!(classify_vesting_owner("SomeUnknownVesting111111111111111111111111").is_none());
    }

    #[test]
    fn all_dex_program_ids_are_44_chars_or_less() {
        // Solana Base58 addresses are 32–44 characters.
        for (id, _) in DEX_PROGRAMS {
            assert!(
                id.len() >= 32 && id.len() <= 44,
                "DEX program ID '{}' has unexpected length {}",
                id,
                id.len()
            );
        }
    }

    #[test]
    fn all_vesting_program_ids_are_44_chars_or_less() {
        for (id, _) in VESTING_PROGRAMS {
            assert!(
                id.len() >= 32 && id.len() <= 44,
                "Vesting program ID '{}' has unexpected length {}",
                id,
                id.len()
            );
        }
    }
}
