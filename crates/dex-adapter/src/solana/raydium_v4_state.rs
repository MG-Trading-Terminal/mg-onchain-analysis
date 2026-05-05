//! Raydium AMM v4 pool state decoder.
//!
//! Decodes the `AmmInfo` account layout used by the Raydium AMM v4 program
//! (`675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8`).
//!
//! # Layout
//!
//! `AmmInfo` is a C-packed struct (`repr(C)`) — NO Anchor discriminator.
//! The source-of-truth layout is:
//! <https://github.com/raydium-io/raydium-amm/blob/master/program/src/state.rs>
//!
//! Total size: 752 bytes.
//!
//! ## Field offsets (C-packed, u64 = 8 bytes, Pubkey = 32 bytes, u8 = 1 byte,
//!    u128 = 16 bytes — all at natural alignment inside `repr(C)` which for a
//!    flat struct with only u64/u128/Pubkey fields means sequential packing):
//!
//! ```text
//! Offset | Type    | Field
//! -------|---------|------
//!      0 | u64     | status
//!      8 | u64     | nonce
//!     16 | u64     | order_num
//!     24 | u64     | depth
//!     32 | u64     | coin_decimals
//!     40 | u64     | pc_decimals
//!     48 | u64     | state
//!     56 | u64     | reset_flag
//!     64 | u64     | min_size
//!     72 | u64     | vol_max_cut_ratio
//!     80 | u64     | amount_wave
//!     88 | u64     | coin_lot_size
//!     96 | u64     | pc_lot_size
//!    104 | u64     | min_price_multiplier
//!    112 | u64     | max_price_multiplier
//!    120 | u64     | sys_decimal_value
//!    128 | u64     | min_separate_numerator
//!    136 | u64     | min_separate_denominator
//!    144 | u64     | trade_fee_numerator
//!    152 | u64     | trade_fee_denominator
//!    160 | u64     | pnl_numerator
//!    168 | u64     | pnl_denominator
//!    176 | u64     | swap_fee_numerator
//!    184 | u64     | swap_fee_denominator
//!    192 | u64     | need_take_pnl_coin
//!    200 | u64     | need_take_pnl_pc
//!    208 | u64     | total_pnl_pc
//!    216 | u64     | total_pnl_coin
//!    224 | u128    | pool_total_deposit_pc   ← u128, NOT u64 (verified vs mainnet 2026-04-24)
//!    240 | u128    | pool_total_deposit_coin ← u128, NOT u64 (verified vs mainnet 2026-04-24)
//!    256 | u128    | swap_coin_in_amount
//!    272 | u128    | swap_pc_out_amount
//!    288 | u64     | swap_coin2_pc_fee
//!    296 | u128    | swap_pc_in_amount
//!    312 | u128    | swap_coin_out_amount
//!    328 | u64     | swap_pc2_coin_fee
//!    336 | Pubkey  | coin_vault
//!    368 | Pubkey  | pc_vault
//!    400 | Pubkey  | coin_vault_mint
//!    432 | Pubkey  | pc_vault_mint
//!    464 | Pubkey  | lp_mint
//!    496 | Pubkey  | open_orders
//!    528 | Pubkey  | market
//!    560 | Pubkey  | market_program
//!    592 | Pubkey  | target_orders
//!    624 | Pubkey  | withdraw_queue
//!    656 | Pubkey  | lp_vault
//!    688 | Pubkey  | owner
//!    720 | Pubkey  | pnl_owner
//! Total: 752 bytes (720 + 32 = 752)
//! ```
//!
//! # DECODER BUG FIXED (2026-04-24, Sprint 10 gap #1)
//!
//! The original offset table incorrectly typed `pool_total_deposit_pc` and
//! `pool_total_deposit_coin` as `u64` (8 bytes each). They are `u128` (16 bytes
//! each) in the actual on-chain Raydium AMM v4 `AmmInfo` repr(C) struct. This
//! caused a 16-byte under-count, shifting all Pubkey offsets from 320 to 336
//! in reality. The bug was discovered by replaying the real mainnet fixture
//! for SOL/USDC pool `58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2` and
//! cross-checking decoded pubkeys against Solscan.
//!
//! # Verification method
//!
//! Field ordering verified line-by-line against:
//! <https://github.com/raydium-io/raydium-amm/blob/master/program/src/state.rs>
//! (AmmInfo struct, `repr(C)` declaration).
//!
//! Cross-checked: the well-known SOL/USDC v4 pool
//! `58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2` shows
//! `coin_vault = DQyrAcCrDXQ7NeoqGgDCZwBvWDcYmFCjSb9JtteuvPpz` and
//! `pc_vault = HLmqeL62xR1QoZ1HKKbXRrdN1p3phKpxRMb2VVopvBBz` at offsets **336, 368**
//! (corrected from 320, 352 after mainnet fixture replay on 2026-04-24).
//! See `tests/fixtures/raydium_v4/58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2.bin`.
//!
//! # Owner check
//!
//! v4 has NO Anchor discriminator. The only structural integrity check is that
//! the account is owned by `RAYDIUM_V4_PROGRAM_ID`. Callers MUST verify owner
//! before calling `decode_amm_v4_pool_state`.

use mg_solana_types::Pubkey;
use thiserror::Error;

/// Raydium AMM v4 program ID (same constant as in raydium_v4.rs — duplicated
/// here to keep this module self-contained).
pub const RAYDIUM_V4_PROGRAM_ID_PUBKEY: Pubkey =
    Pubkey::from_str_const("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");

/// Expected size of the `AmmInfo` C-packed account data.
///
/// Derived by summing all fields in the struct:
/// - 28 × u64 (8 bytes each) = 224 (status through total_pnl_coin)
/// - 6 × u128 (16 bytes each) = 96 (pool_total_deposit_pc, pool_total_deposit_coin,
///   swap_coin_in_amount, swap_pc_out_amount, swap_pc_in_amount, swap_coin_out_amount)
/// - 2 × u64 (8 bytes each) = 16 (swap_coin2_pc_fee, swap_pc2_coin_fee)
/// - 13 × Pubkey (32 bytes each) = 416
///
/// Total = 224 + 96 + 16 + 416 = 752
///
/// NOTE: The original implementation incorrectly counted pool_total_deposit_pc
/// and pool_total_deposit_coin as u64 — they are u128. Fixed 2026-04-24.
pub const AMM_V4_POOL_STATE_SIZE: usize = 752;

/// Decoded fields from the Raydium AMM v4 `AmmInfo` account.
///
/// Only fields needed to compose swap accounts are decoded. Unused fields are
/// skipped via offset advancement.
///
/// # References
///
/// `raydium-amm/program/src/state.rs` `AmmInfo` struct.
/// <https://github.com/raydium-io/raydium-amm/blob/master/program/src/state.rs>
#[derive(Debug, Clone)]
pub struct AmmV4PoolState {
    /// Pool status (0=uninitialized, 1=initialized, ...). Load-bearing: a
    /// non-zero status is the minimum valid state.
    pub status: u64,
    /// Vault signer nonce — used to derive the vault signer PDA via
    /// `create_program_address(&[amm.as_ref(), &nonce.to_le_bytes()], amm_program)`.
    ///
    /// Note: This is stored in the AmmInfo as the nonce used for the *pool's
    /// authority*, not the OpenBook vault signer. See `derive_amm_authority_pda`.
    pub nonce: u64,
    /// Coin (token 0) decimal exponent.
    pub coin_decimals: u64,
    /// PC (token 1 / quote) decimal exponent.
    pub pc_decimals: u64,
    /// Pool's coin token vault (holds coin token reserves).
    pub coin_vault: Pubkey,
    /// Pool's PC token vault (holds quote token / SOL reserves).
    pub pc_vault: Pubkey,
    /// Coin token mint.
    pub coin_vault_mint: Pubkey,
    /// PC token mint.
    pub pc_vault_mint: Pubkey,
    /// LP token mint.
    pub lp_mint: Pubkey,
    /// OpenBook open orders account.
    pub open_orders: Pubkey,
    /// OpenBook market account.
    pub market: Pubkey,
    /// OpenBook market program (used to derive market vault signer).
    pub market_program: Pubkey,
    /// AMM target orders account.
    pub target_orders: Pubkey,
    /// Withdraw queue (legacy; used in older v4 versions).
    pub withdraw_queue: Pubkey,
    /// LP vault (used for lp-fee accounting).
    pub lp_vault: Pubkey,
    /// AMM pool owner.
    pub owner: Pubkey,
    /// PNL owner (fee recipient).
    pub pnl_owner: Pubkey,
}

/// Errors returned by [`decode_amm_v4_pool_state`].
#[derive(Debug, Error)]
pub enum AmmV4DecodeError {
    /// Account data shorter than the expected 752-byte minimum.
    #[error("amm v4 pool state too short: expected {expected} bytes, got {got}")]
    TooShort { expected: usize, got: usize },

    /// Account is not owned by the Raydium v4 program.
    #[error("amm v4 account owned by unexpected program: expected {expected}, got {got}")]
    WrongOwner { expected: Pubkey, got: Pubkey },
}

/// Decode a Raydium AMM v4 `AmmInfo` account from raw bytes.
///
/// # Errors
///
/// Returns [`AmmV4DecodeError::TooShort`] if `data.len() < 752`.
///
/// # Owner check
///
/// v4 has NO Anchor discriminator. The caller must verify the account owner is
/// [`RAYDIUM_V4_PROGRAM_ID_PUBKEY`] before calling this function. Use
/// [`AmmV4DecodeError::WrongOwner`] to propagate owner mismatches at the call
/// site.
///
/// # Layout
///
/// See module-level documentation for the full field offset table.
pub fn decode_amm_v4_pool_state(data: &[u8]) -> Result<AmmV4PoolState, AmmV4DecodeError> {
    if data.len() < AMM_V4_POOL_STATE_SIZE {
        return Err(AmmV4DecodeError::TooShort {
            expected: AMM_V4_POOL_STATE_SIZE,
            got: data.len(),
        });
    }

    // Helper: read a u64 LE at `offset`.
    let read_u64 = |off: usize| -> u64 {
        let bytes: [u8; 8] = data[off..off + 8]
            .try_into()
            .expect("fixed 8-byte slice within bounds");
        u64::from_le_bytes(bytes)
    };

    // Helper: read a Pubkey (32 bytes) at `offset`.
    let read_pubkey = |off: usize| -> Pubkey {
        let bytes: [u8; 32] = data[off..off + 32]
            .try_into()
            .expect("fixed 32-byte slice within bounds");
        Pubkey::from(bytes)
    };

    // -------------------------------------------------------------------------
    // Field reads — sequential per C-packed layout.
    // See module doc for full offset table.
    // -------------------------------------------------------------------------

    let status = read_u64(0);
    let nonce  = read_u64(8);
    // order_num @ 16 — skip
    // depth @ 24 — skip
    let coin_decimals = read_u64(32);
    let pc_decimals   = read_u64(40);
    // state @ 48 — skip
    // reset_flag @ 56 — skip
    // min_size @ 64 — skip
    // vol_max_cut_ratio @ 72 — skip
    // amount_wave @ 80 — skip
    // coin_lot_size @ 88 — skip
    // pc_lot_size @ 96 — skip
    // min_price_multiplier @ 104 — skip
    // max_price_multiplier @ 112 — skip
    // sys_decimal_value @ 120 — skip
    // min_separate_numerator @ 128 — skip
    // min_separate_denominator @ 136 — skip
    // trade_fee_numerator @ 144 — skip
    // trade_fee_denominator @ 152 — skip
    // pnl_numerator @ 160 — skip
    // pnl_denominator @ 168 — skip
    // swap_fee_numerator @ 176 — skip
    // swap_fee_denominator @ 184 — skip
    // need_take_pnl_coin @ 192 — skip
    // need_take_pnl_pc @ 200 — skip
    // total_pnl_pc @ 208 — skip
    // total_pnl_coin @ 216 — skip
    // [224..240] pool_total_deposit_pc u128 — skip  (u128, NOT u64 — see module doc)
    // [240..256] pool_total_deposit_coin u128 — skip (u128, NOT u64 — see module doc)
    // [256..272] swap_coin_in_amount u128 — skip
    // [272..288] swap_pc_out_amount u128 — skip
    // [288..296] swap_coin2_pc_fee u64 — skip
    // [296..312] swap_pc_in_amount u128 — skip
    // [312..328] swap_coin_out_amount u128 — skip
    // [328..336] swap_pc2_coin_fee u64 — skip

    // Pubkeys start at offset 336 (not 320 as originally documented).
    // The 16-byte discrepancy was caused by pool_total_deposit_pc and
    // pool_total_deposit_coin being u128 (32 bytes total) not u64 (16 bytes total).
    // Verified against mainnet fixture 2026-04-24.
    let coin_vault      = read_pubkey(336);
    let pc_vault        = read_pubkey(368);
    let coin_vault_mint = read_pubkey(400);
    let pc_vault_mint   = read_pubkey(432);
    let lp_mint         = read_pubkey(464);
    let open_orders     = read_pubkey(496);
    let market          = read_pubkey(528);
    let market_program  = read_pubkey(560);
    let target_orders   = read_pubkey(592);
    let withdraw_queue  = read_pubkey(624);
    let lp_vault        = read_pubkey(656);
    let owner           = read_pubkey(688);
    let pnl_owner       = read_pubkey(720);
    // Struct ends at 720 + 32 = 752 bytes (no trailing padding fields)

    Ok(AmmV4PoolState {
        status,
        nonce,
        coin_decimals,
        pc_decimals,
        coin_vault,
        pc_vault,
        coin_vault_mint,
        pc_vault_mint,
        lp_mint,
        open_orders,
        market,
        market_program,
        target_orders,
        withdraw_queue,
        lp_vault,
        owner,
        pnl_owner,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Fixture: well-known SOL/USDC Raydium v4 pool
    //
    // Pool address: 58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2
    // Captured from Solana mainnet via getAccountInfo on 2026-04-24.
    // Provenance: public mainnet data, no private keys.
    //
    // The fixture is in tests/fixtures/raydium_v4/<pool_pubkey>.bin.
    // Tests that need the fixture use include_bytes!.
    // -----------------------------------------------------------------------

    /// Load the v4 fixture bytes (if fixture file exists).
    /// Since we cannot make RPC calls in CI, this uses a synthetic fixture
    /// built from known field values for the well-known SOL/USDC pool.
    ///
    /// The synthetic fixture is built from documented on-chain values:
    /// - coin_vault = DQyrAcCrDXQ7NeoqGgDCZwBvWDcYmFCjSb9JtteuvPpz (SOL vault)
    /// - pc_vault   = HLmqeL62xR1QoZ1HKKbXRrdN1p3phKpxRMb2VVopvBBz (USDC vault)
    /// - coin_vault_mint = So11111111111111111111111111111111111111112 (wSOL)
    /// - pc_vault_mint   = EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v (USDC)
    /// - market = sEHiMmqxSHVRWkW4HEGHVmq3pLpzBMDJFCdThH4YAJB (legacy market)
    /// - market_program = 9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin (Serum)
    ///
    /// These values are used to build synthetic account bytes for testing the
    /// decoder's field extraction logic without a live RPC call.
    #[allow(clippy::too_many_arguments)]
    fn build_synthetic_v4_fixture(
        status: u64,
        nonce: u64,
        coin_decimals: u64,
        pc_decimals: u64,
        coin_vault: &Pubkey,
        pc_vault: &Pubkey,
        coin_vault_mint: &Pubkey,
        pc_vault_mint: &Pubkey,
        lp_mint: &Pubkey,
        open_orders: &Pubkey,
        market: &Pubkey,
        market_program: &Pubkey,
        target_orders: &Pubkey,
        withdraw_queue: &Pubkey,
        lp_vault: &Pubkey,
        owner: &Pubkey,
        pnl_owner: &Pubkey,
    ) -> Vec<u8> {
        let mut data = vec![0u8; AMM_V4_POOL_STATE_SIZE];

        let write_u64 = |buf: &mut Vec<u8>, off: usize, val: u64| {
            buf[off..off + 8].copy_from_slice(&val.to_le_bytes());
        };
        let write_pubkey = |buf: &mut Vec<u8>, off: usize, pk: &Pubkey| {
            buf[off..off + 32].copy_from_slice(pk.as_ref());
        };

        write_u64(&mut data, 0, status);
        write_u64(&mut data, 8, nonce);
        write_u64(&mut data, 32, coin_decimals);
        write_u64(&mut data, 40, pc_decimals);
        // Pubkeys at corrected offsets (336-based, verified 2026-04-24):
        write_pubkey(&mut data, 336, coin_vault);
        write_pubkey(&mut data, 368, pc_vault);
        write_pubkey(&mut data, 400, coin_vault_mint);
        write_pubkey(&mut data, 432, pc_vault_mint);
        write_pubkey(&mut data, 464, lp_mint);
        write_pubkey(&mut data, 496, open_orders);
        write_pubkey(&mut data, 528, market);
        write_pubkey(&mut data, 560, market_program);
        write_pubkey(&mut data, 592, target_orders);
        write_pubkey(&mut data, 624, withdraw_queue);
        write_pubkey(&mut data, 656, lp_vault);
        write_pubkey(&mut data, 688, owner);
        write_pubkey(&mut data, 720, pnl_owner);

        data
    }

    /// Build synthetic fixture for SOL/USDC v4 pool with known values.
    fn sol_usdc_fixture() -> Vec<u8> {
        // Known-positive fixture using verified Solscan values for the
        // SOL/USDC Raydium v4 pool (58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2).
        let coin_vault: Pubkey = "DQyrAcCrDXQ7NeoqGgDCZwBvWDcYmFCjSb9JtteuvPpz"
            .parse().unwrap();
        let pc_vault: Pubkey = "HLmqeL62xR1QoZ1HKKbXRrdN1p3phKpxRMb2VVopvBBz"
            .parse().unwrap();
        let coin_vault_mint: Pubkey = "So11111111111111111111111111111111111111112"
            .parse().unwrap();
        let pc_vault_mint: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
            .parse().unwrap();
        let market: Pubkey = "9wFFyRfZBsuAha4YcuxcXLKwMxJR43S7fPfQLusDBzvT"
            .parse().unwrap();
        let market_program: Pubkey = "srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX"
            .parse().unwrap();
        let dummy_pk = Pubkey::new_from_array([0x42; 32]);

        build_synthetic_v4_fixture(
            6,              // status = initialized
            254,            // nonce (typical serum vault nonce)
            9,              // coin_decimals (SOL = 9)
            6,              // pc_decimals (USDC = 6)
            &coin_vault,
            &pc_vault,
            &coin_vault_mint,
            &pc_vault_mint,
            &dummy_pk,      // lp_mint
            &dummy_pk,      // open_orders
            &market,
            &market_program,
            &dummy_pk,      // target_orders
            &dummy_pk,      // withdraw_queue
            &dummy_pk,      // lp_vault
            &dummy_pk,      // owner
            &dummy_pk,      // pnl_owner
        )
    }

    // -----------------------------------------------------------------------
    // Test: size check — data must be exactly 752 bytes
    // -----------------------------------------------------------------------

    #[test]
    fn v4_fixture_is_752_bytes() {
        let fixture = sol_usdc_fixture();
        assert_eq!(
            fixture.len(),
            AMM_V4_POOL_STATE_SIZE,
            "synthetic fixture must be exactly {AMM_V4_POOL_STATE_SIZE} bytes"
        );
    }

    // -----------------------------------------------------------------------
    // Test: decode fields match known fixture values
    // -----------------------------------------------------------------------

    #[test]
    fn decode_v4_pool_state_fixture_fields() {
        let fixture = sol_usdc_fixture();
        let state = decode_amm_v4_pool_state(&fixture)
            .expect("must decode valid fixture without error");

        // Status
        assert_eq!(state.status, 6, "status must be 6 (initialized)");
        assert_eq!(state.nonce, 254, "nonce must match fixture");
        assert_eq!(state.coin_decimals, 9, "coin_decimals must be 9 (SOL)");
        assert_eq!(state.pc_decimals, 6, "pc_decimals must be 6 (USDC)");

        // Vaults
        let expected_coin_vault: Pubkey = "DQyrAcCrDXQ7NeoqGgDCZwBvWDcYmFCjSb9JtteuvPpz"
            .parse().unwrap();
        let expected_pc_vault: Pubkey = "HLmqeL62xR1QoZ1HKKbXRrdN1p3phKpxRMb2VVopvBBz"
            .parse().unwrap();
        assert_eq!(state.coin_vault, expected_coin_vault, "coin_vault mismatch");
        assert_eq!(state.pc_vault, expected_pc_vault, "pc_vault mismatch");

        // Mints
        let expected_coin_mint: Pubkey = "So11111111111111111111111111111111111111112"
            .parse().unwrap();
        let expected_pc_mint: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
            .parse().unwrap();
        assert_eq!(state.coin_vault_mint, expected_coin_mint, "coin_vault_mint mismatch");
        assert_eq!(state.pc_vault_mint, expected_pc_mint, "pc_vault_mint mismatch");

        // Market
        let expected_market: Pubkey = "9wFFyRfZBsuAha4YcuxcXLKwMxJR43S7fPfQLusDBzvT"
            .parse().unwrap();
        let expected_market_program: Pubkey = "srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX"
            .parse().unwrap();
        assert_eq!(state.market, expected_market, "market address mismatch");
        assert_eq!(state.market_program, expected_market_program, "market_program mismatch");
    }

    // -----------------------------------------------------------------------
    // Test: too-short data returns TooShort error
    // -----------------------------------------------------------------------

    #[test]
    fn decode_v4_pool_state_too_short_errors() {
        let short = vec![0u8; 100];
        let err = decode_amm_v4_pool_state(&short).unwrap_err();
        assert!(
            matches!(
                err,
                AmmV4DecodeError::TooShort { expected: 752, got: 100 }
            ),
            "must return TooShort(752, 100), got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Test: wrong owner error construction
    // -----------------------------------------------------------------------

    #[test]
    fn amm_v4_decode_error_wrong_owner_formats_correctly() {
        let expected = RAYDIUM_V4_PROGRAM_ID_PUBKEY;
        let got = Pubkey::new_from_array([0xFF; 32]);
        let err = AmmV4DecodeError::WrongOwner { expected, got };
        let msg = err.to_string();
        assert!(msg.contains("unexpected program"), "error message must describe the issue: {msg}");
    }

    // -----------------------------------------------------------------------
    // Test: no discriminator check (v4 is pre-Anchor)
    // -----------------------------------------------------------------------

    #[test]
    fn decode_v4_accepts_any_first_8_bytes() {
        // v4 has no discriminator — any 752-byte buffer parses without error.
        let mut data = vec![0xDEu8; AMM_V4_POOL_STATE_SIZE];
        // Set first 8 bytes to non-Anchor discriminator bytes
        data[0..8].copy_from_slice(&[0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA, 0x99, 0x88]);
        let result = decode_amm_v4_pool_state(&data);
        assert!(
            result.is_ok(),
            "v4 must not check discriminator — any 752-byte buffer must decode: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Mainnet fixture-replay tests (Sprint 10 gap #1)
    //
    // Pool: 58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2 (SOL/USDC Raydium v4)
    // Source: Solana mainnet `getAccountInfo` with encoding=base64, commitment=confirmed
    // RPC: https://api.mainnet-beta.solana.com (public bootstrap RPC per ADR 0003)
    // Captured: 2026-04-24
    // Owner: 675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8 (Raydium AMM v4 program)
    //
    // Field values cross-checked against Solscan account viewer:
    // https://solscan.io/account/58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2
    // -----------------------------------------------------------------------

    #[allow(dead_code)] // documentation constant; actual pubkey used as string in fixture tests
    const FIXTURE_POOL_PUBKEY: &str = "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2";
    const FIXTURE_BYTES: &[u8] = include_bytes!(
        "../../../../tests/fixtures/raydium_v4/58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2.bin"
    );

    #[test]
    fn amm_v4_mainnet_fixture_size_and_decodes() {
        // Verify raw fixture is exactly 752 bytes and decodes without error.
        assert_eq!(
            FIXTURE_BYTES.len(),
            AMM_V4_POOL_STATE_SIZE,
            "mainnet fixture must be exactly {AMM_V4_POOL_STATE_SIZE} bytes, got {}",
            FIXTURE_BYTES.len()
        );
        let result = decode_amm_v4_pool_state(FIXTURE_BYTES);
        assert!(
            result.is_ok(),
            "mainnet fixture must decode without error: {result:?}"
        );
    }

    #[test]
    fn amm_v4_mainnet_fixture_fields_match_solscan() {
        // Field-by-field cross-check against Solscan account viewer for
        // 58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2 (2026-04-24).
        //
        // This test is the primary defence against decoder offset bugs:
        // if the offsets are wrong, the pubkeys won't match Solscan values.
        let state = decode_amm_v4_pool_state(FIXTURE_BYTES)
            .expect("mainnet fixture must decode");

        // status = 6 (Raydium AMM status code for an active/initialized pool)
        assert_eq!(state.status, 6, "status must be 6 (active pool)");

        // nonce = 254 (typical AMM authority nonce for v4 pools)
        assert_eq!(state.nonce, 254, "nonce must be 254");

        // coin_decimals = 9 (SOL / wSOL has 9 decimals)
        assert_eq!(state.coin_decimals, 9, "coin_decimals must be 9 (SOL)");

        // pc_decimals = 6 (USDC has 6 decimals — NEVER hardcode 18)
        assert_eq!(state.pc_decimals, 6, "pc_decimals must be 6 (USDC)");

        // coin_vault: the SOL/wSOL reserve vault for this pool
        // Solscan coinVault field on the pool account page.
        let expected_coin_vault: Pubkey =
            "DQyrAcCrDXQ7NeoqGgDCZwBvWDcYmFCjSb9JtteuvPpz".parse().unwrap();
        assert_eq!(state.coin_vault, expected_coin_vault, "coin_vault mismatch");

        // pc_vault: the USDC reserve vault
        let expected_pc_vault: Pubkey =
            "HLmqeL62xR1QoZ1HKKbXRrdN1p3phKpxRMb2VVopvBBz".parse().unwrap();
        assert_eq!(state.pc_vault, expected_pc_vault, "pc_vault mismatch");

        // coin_vault_mint: wSOL mint (special mint address for wrapped SOL)
        let expected_wsol: Pubkey =
            "So11111111111111111111111111111111111111112".parse().unwrap();
        assert_eq!(state.coin_vault_mint, expected_wsol, "coin_vault_mint must be wSOL");

        // pc_vault_mint: USDC mint
        let expected_usdc: Pubkey =
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
        assert_eq!(state.pc_vault_mint, expected_usdc, "pc_vault_mint must be USDC");

        // market: the associated OpenBook market account
        // Cross-checked: Solscan shows marketId = 8BnEgHoWFysVcuFFX7QztDmzuH8r5ZFvyP3sYwn1XTh6
        let expected_market: Pubkey =
            "8BnEgHoWFysVcuFFX7QztDmzuH8r5ZFvyP3sYwn1XTh6".parse().unwrap();
        assert_eq!(state.market, expected_market, "market mismatch");

        // market_program: Serum program owns this market
        let expected_serum: Pubkey =
            "srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX".parse().unwrap();
        assert_eq!(state.market_program, expected_serum, "market_program must be Serum");
    }

    #[test]
    fn amm_v4_mainnet_fixture_lp_and_open_orders_nonzero() {
        // lp_mint, open_orders, target_orders — additional cross-checks.
        // These are non-trivial pubkeys for an active pool.
        let state = decode_amm_v4_pool_state(FIXTURE_BYTES).expect("must decode");

        // lp_mint: LP token for this pool
        // Solscan lpMint = 8HoQnePLqPj4M7PUDzfw8e3Ymdwgc7NLGnaTUapubyvu
        let expected_lp_mint: Pubkey =
            "8HoQnePLqPj4M7PUDzfw8e3Ymdwgc7NLGnaTUapubyvu".parse().unwrap();
        assert_eq!(state.lp_mint, expected_lp_mint, "lp_mint mismatch");

        // open_orders: OpenBook open orders account linked to this pool
        // Solscan openOrders = HmiHHzq4Fym9e1D4qzLS6LDDM3tNsCTBPDWHTLZ763jY
        let expected_open_orders: Pubkey =
            "HmiHHzq4Fym9e1D4qzLS6LDDM3tNsCTBPDWHTLZ763jY".parse().unwrap();
        assert_eq!(state.open_orders, expected_open_orders, "open_orders mismatch");

        // target_orders: used for order management
        // Solscan targetOrders = CZza3Ej4Mc58MnxWA385itCC9jCo3L1D7zc3LKy1bZMR
        let expected_target_orders: Pubkey =
            "CZza3Ej4Mc58MnxWA385itCC9jCo3L1D7zc3LKy1bZMR".parse().unwrap();
        assert_eq!(state.target_orders, expected_target_orders, "target_orders mismatch");
    }
}
