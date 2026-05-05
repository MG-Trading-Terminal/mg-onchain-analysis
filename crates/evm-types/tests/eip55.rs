//! EIP-55 checksum address test vectors.
//!
//! Vectors sourced from https://eips.ethereum.org/EIPS/eip-55
//! reference: EIP-55 (public spec)

use mg_evm_types::Address;

/// All vectors from the EIP-55 specification.
const EIP55_VECTORS: &[&str] = &[
    "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
    "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359",
    "0xdbF03B407c01E7cD3CBea99509d93f8DDDC8C6FB",
    "0xD1220A0cf47c7B9Be7A2E6BA89F429762e7b9aDb",
];

#[test]
fn eip55_spec_vectors_round_trip() {
    for &checksummed in EIP55_VECTORS {
        let parsed: Address = checksummed.parse().expect("parse failed");
        let re_encoded = parsed.to_checksum_0x();
        assert_eq!(
            re_encoded, checksummed,
            "EIP-55 mismatch for {checksummed}: got {re_encoded}"
        );
    }
}

#[test]
fn eip55_lowercase_input_normalises_to_checksum() {
    let lower = "0x5aaeb6053f3e94c9b9a09f33669435e7ef1beaed";
    let expected = "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed";
    let parsed: Address = lower.parse().unwrap();
    assert_eq!(parsed.to_checksum_0x(), expected);
}

#[test]
fn eip55_uppercase_input_normalises_to_checksum() {
    let upper = "0x5AAEB6053F3E94C9B9A09F33669435E7EF1BEAED";
    let expected = "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed";
    let parsed: Address = upper.parse().unwrap();
    assert_eq!(parsed.to_checksum_0x(), expected);
}

#[test]
fn eip55_all_zeros() {
    let addr = Address::ZERO;
    // All-zero address is all lowercase (all nibbles hash to < 8 direction is irrelevant for 0)
    assert_eq!(addr.to_checksum_0x(), "0x0000000000000000000000000000000000000000");
}

#[test]
fn eip55_serde_preserves_checksum() {
    for &checksummed in EIP55_VECTORS {
        let addr: Address = checksummed.parse().unwrap();
        let json = serde_json::to_string(&addr).unwrap();
        // The serialised string should be the checksummed form (with quotes).
        let expected_json = format!("\"{}\"", checksummed);
        assert_eq!(json, expected_json, "serde output for {checksummed}");
    }
}
