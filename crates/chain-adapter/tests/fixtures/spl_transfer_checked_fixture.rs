//! Fixture byte arrays for SPL Token instruction decode tests.
//!
//! Each fixture is a realistic byte sequence captured from the Solana SPL Token
//! program specification. No live network calls are made — these are static
//! test vectors derived from the SPL Token instruction encoding documented at:
//!
//! Reference: https://github.com/solana-program/token/blob/main/program/src/instruction.rs
//!
//! # Fixture: TransferChecked
//!
//! Layout: [discriminator=12][amount: u64 LE][decimals: u8]
//! Example: transfer 1.5 USDC (6 decimals) = 1_500_000 raw units
//!   discriminator = 12 (0x0C)
//!   amount = 1_500_000 = 0x16E360 → LE bytes: [0x60, 0xE3, 0x16, 0x00, 0x00, 0x00, 0x00, 0x00]
//!   decimals = 6

/// Raw bytes for `TransferChecked` transferring 1.5 USDC (1_500_000 raw, 6 decimals).
pub const TRANSFER_CHECKED_1_5_USDC: &[u8] = &[
    12,                                       // discriminator = TransferChecked
    0x60, 0xE3, 0x16, 0x00, 0x00, 0x00, 0x00, 0x00, // amount = 1_500_000 LE
    6,                                        // decimals = 6
];

/// Raw bytes for `MintToChecked` minting 1 WSOL (1_000_000_000 raw, 9 decimals).
pub const MINT_TO_CHECKED_1_WSOL: &[u8] = &[
    14,                                       // discriminator = MintToChecked
    0x00, 0xCA, 0x9A, 0x3B, 0x00, 0x00, 0x00, 0x00, // amount = 1_000_000_000 LE
    9,                                        // decimals = 9
];

/// Raw bytes for `BurnChecked` burning 500 USDC (500_000_000 raw, 6 decimals).
pub const BURN_CHECKED_500_USDC: &[u8] = &[
    15,                                       // discriminator = BurnChecked
    0x00, 0x65, 0xCD, 0x1D, 0x00, 0x00, 0x00, 0x00, // amount = 500_000_000 LE
    6,                                        // decimals = 6
];

/// Raw bytes for `Transfer` (plain, no decimals field) of 100 lamports.
pub const TRANSFER_PLAIN_100: &[u8] = &[
    3,                                        // discriminator = Transfer
    100, 0, 0, 0, 0, 0, 0, 0,                // amount = 100 LE
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_checked_fixture_amount_decode() {
        // Verify the fixture encodes the expected amount.
        let amount_bytes: [u8; 8] = TRANSFER_CHECKED_1_5_USDC[1..9].try_into().unwrap();
        let amount = u64::from_le_bytes(amount_bytes);
        assert_eq!(amount, 1_500_000, "fixture must encode 1.5 USDC = 1_500_000 raw");
        assert_eq!(TRANSFER_CHECKED_1_5_USDC[9], 6, "fixture must encode 6 decimals");
    }

    #[test]
    fn mint_to_checked_fixture_amount_decode() {
        let amount_bytes: [u8; 8] = MINT_TO_CHECKED_1_WSOL[1..9].try_into().unwrap();
        let amount = u64::from_le_bytes(amount_bytes);
        assert_eq!(amount, 1_000_000_000, "fixture must encode 1 SOL = 1_000_000_000 lamports");
        assert_eq!(MINT_TO_CHECKED_1_WSOL[9], 9, "fixture must encode 9 decimals");
    }

    #[test]
    fn burn_checked_fixture_amount_decode() {
        let amount_bytes: [u8; 8] = BURN_CHECKED_500_USDC[1..9].try_into().unwrap();
        let amount = u64::from_le_bytes(amount_bytes);
        assert_eq!(amount, 500_000_000, "fixture must encode 500 USDC");
    }

    #[test]
    fn transfer_plain_fixture_amount_decode() {
        let amount_bytes: [u8; 8] = TRANSFER_PLAIN_100[1..9].try_into().unwrap();
        let amount = u64::from_le_bytes(amount_bytes);
        assert_eq!(amount, 100);
    }
}
