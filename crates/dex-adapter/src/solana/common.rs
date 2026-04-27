//! Shared helpers for Solana DEX instruction decoders.
//!
//! Byte-level parsing helpers, account resolution, and address parsing that
//! all Solana DEX decoders share. No DEX-specific logic lives here.

use mg_onchain_common::chain::{Address, Chain};

use crate::error::DexAdapterError;

// ---------------------------------------------------------------------------
// Byte-level readers
// ---------------------------------------------------------------------------

/// Read 8 bytes at `offset` as a little-endian `u64`.
///
/// Used for Raydium AMM v4 (non-Borsh packed C-struct encoding).
#[inline]
pub fn read_u64_le(
    data: &[u8],
    offset: usize,
    context: &'static str,
) -> Result<u64, DexAdapterError> {
    let end = offset + 8;
    if data.len() < end {
        return Err(DexAdapterError::DataTooShort {
            context,
            offset,
            need: 8,
            got: data.len().saturating_sub(offset),
        });
    }
    let arr: [u8; 8] = data[offset..end].try_into().unwrap();
    Ok(u64::from_le_bytes(arr))
}

/// Read a single byte at `offset`.
#[inline]
pub fn read_u8(
    data: &[u8],
    offset: usize,
    context: &'static str,
) -> Result<u8, DexAdapterError> {
    data.get(offset).copied().ok_or(DexAdapterError::DataTooShort {
        context,
        offset,
        need: 1,
        got: 0,
    })
}

// ---------------------------------------------------------------------------
// Account resolution
// ---------------------------------------------------------------------------

/// Return the account address string at `index`, or `MissingAccount` error.
#[inline]
pub fn get_account<'a>(
    accounts: &'a [String],
    index: usize,
    program: &'static str,
    name: &'static str,
) -> Result<&'a str, DexAdapterError> {
    accounts
        .get(index)
        .map(|s| s.as_str())
        .ok_or(DexAdapterError::MissingAccount { program, index, name })
}

// ---------------------------------------------------------------------------
// Address parsing
// ---------------------------------------------------------------------------

/// Parse a Base58 Solana address string into a `common::Address`.
pub fn parse_solana_addr(
    s: &str,
    program: &'static str,
    index: usize,
) -> Result<Address, DexAdapterError> {
    Address::parse(Chain::Solana, s).map_err(|e| DexAdapterError::InvalidAddress {
        program,
        index,
        reason: e.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_u64_le_one() {
        let mut data = vec![0u8; 1]; // discriminator placeholder
        data.extend_from_slice(&1u64.to_le_bytes());
        assert_eq!(read_u64_le(&data, 1, "test").unwrap(), 1);
    }

    #[test]
    fn read_u64_le_max() {
        let data = u64::MAX.to_le_bytes();
        assert_eq!(read_u64_le(&data, 0, "test").unwrap(), u64::MAX);
    }

    #[test]
    fn read_u64_le_too_short() {
        let data = vec![0u8, 1u8, 2u8];
        assert!(read_u64_le(&data, 0, "test").is_err());
    }

    #[test]
    fn read_u8_ok() {
        let data = vec![42u8];
        assert_eq!(read_u8(&data, 0, "test").unwrap(), 42);
    }

    #[test]
    fn read_u8_oob() {
        let data: Vec<u8> = vec![];
        assert!(read_u8(&data, 0, "test").is_err());
    }

    #[test]
    fn get_account_ok() {
        let accounts = vec!["abc".to_string(), "def".to_string()];
        assert_eq!(get_account(&accounts, 1, "prog", "field").unwrap(), "def");
    }

    #[test]
    fn get_account_missing() {
        let accounts: Vec<String> = vec![];
        assert!(get_account(&accounts, 0, "prog", "field").is_err());
    }
}
