//! Fixture-replay integration test for Ethereum event decoders.
//!
//! # Fixture source (ADR 0003 one-time carve-out)
//!
//! Logs were captured 2026-04-24 via `https://ethereum.publicnode.com`
//! (ADR 0003 one-time carve-out — public endpoint used ONLY for fixture capture,
//! never in production hot path). The fixture is static JSON; no network calls
//! occur at test time.
//!
//! ## Blocks covered
//!
//! - **Block 21,000,000** (`0x1406f40`, mainnet): Transfer, Approval, UniV2Swap,
//!   UniV2Mint, UniV2Burn, UniV3Swap — 6 of 8 event types
//! - **Block 21,486,016** (`0x147D9C0`, mainnet): UniV3Mint, UniV3Burn — 2 supplemental
//!   event types not present in block 21,000,000
//!
//! All 8 event types are covered across the two blocks.
//!
//! ## Known assertions
//!
//! The fixture includes a `known_assertions` block with pre-verified values:
//! - USDC Transfer: from/to/value_raw cross-checked against Etherscan
//!   tx `0x24e20c506fd16546178a03c955bca381376f97b9ff5aefb726abf84dea6c8913`
//! - V3Mint tick_lower/tick_upper/amount from block 21,486,016
//! - V3Burn tick_lower/tick_upper/amount from block 21,486,016
//!
//! ## Decimal policy check
//!
//! USDC has 6 decimals. `value_raw = 7,534,659,460`. The test verifies this raw
//! value without applying decimals — decimals come from the token-registry, NOT
//! from the decoder. No hardcoded 18 in this test.

use std::path::PathBuf;

use alloy::primitives::U256;
use mg_onchain_chain_adapter::ethereum::decoder::{
    try_decode_approval, try_decode_transfer, try_decode_v2_burn, try_decode_v2_mint,
    try_decode_v2_swap, try_decode_v3_burn, try_decode_v3_mint, try_decode_v3_swap,
    APPROVAL_TOPIC0, TRANSFER_TOPIC0, UNISWAP_V2_BURN_TOPIC0, UNISWAP_V2_MINT_TOPIC0,
    UNISWAP_V2_SWAP_TOPIC0, UNISWAP_V3_BURN_TOPIC0, UNISWAP_V3_MINT_TOPIC0,
    UNISWAP_V3_SWAP_TOPIC0,
};
use mg_onchain_chain_adapter::ethereum::types::RawLog;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Fixture loading
// ---------------------------------------------------------------------------

fn fixture_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/ethereum/mainnet_block_21000000.json");
    p
}

fn load_fixture() -> Value {
    let path = fixture_path();
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("fixture not found at {}: {}", path.display(), e));
    serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("fixture JSON parse failed: {e}"))
}

fn log_from_value(v: &Value) -> RawLog {
    let address = v["address"].as_str().unwrap_or("0x0").to_string();
    let topics: Vec<String> = v["topics"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|t| t.as_str().map(|s| s.to_string()))
        .collect();
    let data_hex = v["data"].as_str().unwrap_or("0x");
    let data = hex::decode(data_hex.strip_prefix("0x").unwrap_or(data_hex))
        .unwrap_or_default();
    let block_number = v["blockNumber"]
        .as_str()
        .and_then(|s| u64::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).ok())
        .unwrap_or(0);
    let tx_hash = v["transactionHash"].as_str().unwrap_or("0x0").to_string();
    let log_index = v["logIndex"]
        .as_str()
        .and_then(|s| u64::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).ok())
        .unwrap_or(0) as u32;

    RawLog { address, topics, data, block_number, tx_hash, log_index }
}

// ---------------------------------------------------------------------------
// Helper: decode all logs through all decoders, return event type counts
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
struct DecodeCounts {
    transfer: u32,
    approval: u32,
    v2_swap: u32,
    v2_mint: u32,
    v2_burn: u32,
    v3_swap: u32,
    v3_mint: u32,
    v3_burn: u32,
    no_match: u32,
    errors: u32,
}

fn decode_all(logs: &[Value]) -> DecodeCounts {
    let mut counts = DecodeCounts::default();

    for log_val in logs {
        let log = log_from_value(log_val);

        // Try each decoder in sequence; at most one should match (topic0 dispatch).
        let mut matched = false;

        macro_rules! try_decoder {
            ($fn:expr, $field:ident) => {
                match $fn(&log) {
                    Ok(Some(_)) => { counts.$field += 1; matched = true; }
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("decoder error on log {}: {e}", log.tx_hash);
                        counts.errors += 1;
                        matched = true;
                    }
                }
            };
        }

        try_decoder!(try_decode_transfer, transfer);
        try_decoder!(try_decode_approval, approval);
        try_decoder!(try_decode_v2_swap, v2_swap);
        try_decoder!(try_decode_v2_mint, v2_mint);
        try_decoder!(try_decode_v2_burn, v2_burn);
        try_decoder!(try_decode_v3_swap, v3_swap);
        try_decoder!(try_decode_v3_mint, v3_mint);
        try_decoder!(try_decode_v3_burn, v3_burn);

        if !matched {
            counts.no_match += 1;
        }
    }

    counts
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn fixture_file_exists_and_has_logs() {
    let fixture = load_fixture();
    let logs = fixture["logs"].as_array().expect("fixture must have 'logs' array");
    assert!(!logs.is_empty(), "fixture must contain at least one log");
    // All 8 event types must be covered per fixture comment
    let covered = fixture["event_types_covered"]
        .as_array()
        .expect("fixture must have event_types_covered");
    assert_eq!(covered.len(), 8, "fixture must cover all 8 event types");
}

#[test]
fn fixture_decodes_without_errors_or_panics() {
    let fixture = load_fixture();
    let logs = fixture["logs"].as_array().unwrap();
    let counts = decode_all(logs);

    assert_eq!(counts.errors, 0, "zero decode errors expected; got {counts:?}");
}

#[test]
fn fixture_covers_all_8_event_types() {
    let fixture = load_fixture();
    let logs = fixture["logs"].as_array().unwrap();
    let counts = decode_all(logs);

    assert!(counts.transfer >= 1, "at least 1 Transfer; got {counts:?}");
    assert!(counts.approval >= 1, "at least 1 Approval; got {counts:?}");
    assert!(counts.v2_swap >= 1, "at least 1 V2Swap; got {counts:?}");
    assert!(counts.v2_mint >= 1, "at least 1 V2Mint; got {counts:?}");
    assert!(counts.v2_burn >= 1, "at least 1 V2Burn; got {counts:?}");
    assert!(counts.v3_swap >= 1, "at least 1 V3Swap; got {counts:?}");
    assert!(counts.v3_mint >= 1, "at least 1 V3Mint; got {counts:?}");
    assert!(counts.v3_burn >= 1, "at least 1 V3Burn; got {counts:?}");
}

#[test]
fn usdc_transfer_decoded_correctly() {
    // Verifies known USDC transfer from the fixture.
    // tx: 0x24e20c506fd16546178a03c955bca381376f97b9ff5aefb726abf84dea6c8913
    // USDC contract: 0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48
    // from: 0xd91efec7e42f80156d1d9f660a69847188950747
    // to:   0x3974549dc16bf72af6fc3668d5f6c092c9e91c2b
    // value_raw: 7,534,659,460 (USDC has 6 decimals — do NOT divide here, that's token-registry work)
    let fixture = load_fixture();
    let logs = fixture["logs"].as_array().unwrap();

    let usdc = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    let usdc_log = logs.iter()
        .find(|l| {
            let t0 = l["topics"][0].as_str().unwrap_or("");
            let addr = l["address"].as_str().unwrap_or("");
            t0 == TRANSFER_TOPIC0 && addr.eq_ignore_ascii_case(usdc)
        })
        .expect("USDC Transfer log must be present in fixture");

    let log = log_from_value(usdc_log);
    let decoded = try_decode_transfer(&log)
        .expect("decode must not error")
        .expect("USDC Transfer must decode to Some");

    // from address — lowercase hex comparison (EIP-55 checksum is cosmetic)
    assert_eq!(
        decoded.from.to_string().to_lowercase(),
        "0xd91efec7e42f80156d1d9f660a69847188950747",
        "USDC Transfer from address mismatch"
    );
    assert_eq!(
        decoded.to.to_string().to_lowercase(),
        "0x3974549dc16bf72af6fc3668d5f6c092c9e91c2b",
        "USDC Transfer to address mismatch"
    );
    // 7,534,659,460 raw units. USDC has 6 decimals → 7534.65946 USDC.
    // We verify raw units here — decimal conversion is token-registry's responsibility.
    assert_eq!(decoded.value, U256::from(7_534_659_460u64),
        "USDC Transfer value_raw must be 7534659460 (raw units, not divided by 10^6)");
    assert_eq!(decoded.contract.to_lowercase(), usdc);
}

#[test]
fn v3_swap_decoded_correctly() {
    // Verifies that at least one V3Swap decodes with non-zero sender/recipient.
    let fixture = load_fixture();
    let logs = fixture["logs"].as_array().unwrap();

    let v3_log = logs.iter()
        .find(|l| l["topics"][0].as_str().unwrap_or("") == UNISWAP_V3_SWAP_TOPIC0)
        .expect("V3Swap log must be in fixture");

    let log = log_from_value(v3_log);
    let decoded = try_decode_v3_swap(&log)
        .expect("decode must not error")
        .expect("V3Swap must decode");

    // sender and recipient should be non-zero addresses
    assert_ne!(decoded.sender.to_string(), "0x0000000000000000000000000000000000000000");
    assert!(decoded.block_number > 0);
    // amounts must be non-zero (this is an actual swap)
    assert!(decoded.amount0 != alloy::primitives::I256::ZERO || decoded.amount1 != alloy::primitives::I256::ZERO,
        "at least one of amount0/amount1 must be non-zero in a real swap");
    println!("V3Swap decoded: pool={}, tick={}, sender={}", decoded.pool, decoded.tick, decoded.sender);
}

#[test]
fn v3_mint_decoded_correctly() {
    // Block 21,486,016: pool 0x6dcba3657ee750a51a13a235b4ed081317da3066
    // Known: tick_lower=-887270, tick_upper=887270 (full-range position, NonfungiblePositionManager)
    let fixture = load_fixture();
    let logs = fixture["logs"].as_array().unwrap();

    let mint_log = logs.iter()
        .find(|l| l["topics"][0].as_str().unwrap_or("") == UNISWAP_V3_MINT_TOPIC0)
        .expect("V3Mint log must be in fixture");

    let log = log_from_value(mint_log);
    let decoded = try_decode_v3_mint(&log)
        .expect("decode must not error")
        .expect("V3Mint must decode");

    assert_eq!(decoded.tick_lower, -887270, "V3Mint tickLower");
    assert_eq!(decoded.tick_upper, 887270, "V3Mint tickUpper");
    assert!(decoded.amount > 0, "V3Mint amount must be positive");
    assert!(decoded.amount0 > U256::ZERO || decoded.amount1 > U256::ZERO,
        "V3Mint must have non-zero amount0 or amount1");
    println!("V3Mint decoded: pool={}, owner={}, amount={}", decoded.pool, decoded.owner, decoded.amount);
}

#[test]
fn v3_burn_decoded_correctly() {
    // Block 21,486,016: pool 0xf9f7ee120e4ce2b4500611952df8c7470af09816
    // Known: tick_lower=1080, tick_upper=1200
    let fixture = load_fixture();
    let logs = fixture["logs"].as_array().unwrap();

    let burn_log = logs.iter()
        .find(|l| l["topics"][0].as_str().unwrap_or("") == UNISWAP_V3_BURN_TOPIC0)
        .expect("V3Burn log must be in fixture");

    let log = log_from_value(burn_log);
    let decoded = try_decode_v3_burn(&log)
        .expect("decode must not error")
        .expect("V3Burn must decode");

    assert_eq!(decoded.tick_lower, 1080, "V3Burn tickLower");
    assert_eq!(decoded.tick_upper, 1200, "V3Burn tickUpper");
    assert!(decoded.amount > 0, "V3Burn amount must be positive");
    println!("V3Burn decoded: pool={}, owner={}, amount={}", decoded.pool, decoded.owner, decoded.amount);
}

#[test]
fn unknown_topic0_logs_return_none_cleanly() {
    // Every decoder must return Ok(None) — no panics, no errors — for unknown topic0.
    let synthetic_unknown = RawLog {
        address: "0x1234567890123456789012345678901234567890".to_string(),
        topics: vec!["0xdeadbeef00000000000000000000000000000000000000000000000000000000".to_string()],
        data: vec![0u8; 64],
        block_number: 21_000_000,
        tx_hash: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        log_index: 0,
    };

    assert!(try_decode_transfer(&synthetic_unknown).unwrap().is_none());
    assert!(try_decode_approval(&synthetic_unknown).unwrap().is_none());
    assert!(try_decode_v2_swap(&synthetic_unknown).unwrap().is_none());
    assert!(try_decode_v2_mint(&synthetic_unknown).unwrap().is_none());
    assert!(try_decode_v2_burn(&synthetic_unknown).unwrap().is_none());
    assert!(try_decode_v3_swap(&synthetic_unknown).unwrap().is_none());
    assert!(try_decode_v3_mint(&synthetic_unknown).unwrap().is_none());
    assert!(try_decode_v3_burn(&synthetic_unknown).unwrap().is_none());
}

#[test]
fn v2_swap_decoded_from_fixture() {
    let fixture = load_fixture();
    let logs = fixture["logs"].as_array().unwrap();

    let swap_log = logs.iter()
        .find(|l| l["topics"][0].as_str().unwrap_or("") == UNISWAP_V2_SWAP_TOPIC0)
        .expect("V2Swap log must be in fixture");

    let log = log_from_value(swap_log);
    let decoded = try_decode_v2_swap(&log)
        .expect("decode must not error")
        .expect("V2Swap must decode");

    // Exactly one direction is non-zero in a real swap
    let has_in = decoded.amount0_in > U256::ZERO || decoded.amount1_in > U256::ZERO;
    let has_out = decoded.amount0_out > U256::ZERO || decoded.amount1_out > U256::ZERO;
    assert!(has_in && has_out, "V2Swap must have both in and out amounts non-zero");
    println!("V2Swap decoded: pair={}, sender={}, a0in={}, a1out={}",
        decoded.pair, decoded.sender, decoded.amount0_in, decoded.amount1_out);
}

#[test]
fn approval_decoded_from_fixture() {
    let fixture = load_fixture();
    let logs = fixture["logs"].as_array().unwrap();

    let appr_log = logs.iter()
        .find(|l| l["topics"][0].as_str().unwrap_or("") == APPROVAL_TOPIC0)
        .expect("Approval log must be in fixture");

    let log = log_from_value(appr_log);
    let decoded = try_decode_approval(&log)
        .expect("decode must not error")
        .expect("Approval must decode");

    assert_ne!(decoded.owner.to_string(), "0x0000000000000000000000000000000000000000");
    assert_ne!(decoded.spender.to_string(), "0x0000000000000000000000000000000000000000");
    println!("Approval decoded: owner={}, spender={}, value={}", decoded.owner, decoded.spender, decoded.value);
}

#[test]
fn v2_mint_and_burn_decoded_from_fixture() {
    let fixture = load_fixture();
    let logs = fixture["logs"].as_array().unwrap();

    let mint_log = logs.iter()
        .find(|l| l["topics"][0].as_str().unwrap_or("") == UNISWAP_V2_MINT_TOPIC0)
        .expect("V2Mint log must be in fixture");
    let mint = try_decode_v2_mint(&log_from_value(mint_log))
        .expect("no error").expect("V2Mint decoded");
    assert!(mint.amount0 > U256::ZERO || mint.amount1 > U256::ZERO);

    let burn_log = logs.iter()
        .find(|l| l["topics"][0].as_str().unwrap_or("") == UNISWAP_V2_BURN_TOPIC0)
        .expect("V2Burn log must be in fixture");
    let burn = try_decode_v2_burn(&log_from_value(burn_log))
        .expect("no error").expect("V2Burn decoded");
    assert!(burn.amount0 > U256::ZERO || burn.amount1 > U256::ZERO);

    println!("V2Mint: a0={}, a1={}", mint.amount0, mint.amount1);
    println!("V2Burn: a0={}, a1={}", burn.amount0, burn.amount1);
}
