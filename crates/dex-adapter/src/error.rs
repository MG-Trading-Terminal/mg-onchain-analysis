//! Error types for `crates/dex-adapter`.

use thiserror::Error;

/// All errors produced by a `DexAdapter` decoder.
///
/// `#[non_exhaustive]` so future DEX-specific error conditions can be added
/// in minor releases without breaking consumer match arms.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DexAdapterError {
    /// The instruction data buffer was shorter than required.
    #[error("instruction data too short: need {need} bytes at offset {offset}, got {got} (context: {context})")]
    DataTooShort {
        context: &'static str,
        offset: usize,
        need: usize,
        got: usize,
    },

    /// The instruction discriminator did not match any known instruction for this program.
    #[error("unknown discriminator {discriminator:#04x?} for program {program}")]
    UnknownDiscriminator {
        program: &'static str,
        discriminator: Vec<u8>,
    },

    /// A required account was missing from the accounts slice.
    #[error("missing account at index {index}: {name} (program: {program})")]
    MissingAccount {
        program: &'static str,
        index: usize,
        name: &'static str,
    },

    /// An account address string was invalid (not valid Base58 or wrong length).
    #[error("invalid account address at index {index} (program: {program}): {reason}")]
    InvalidAddress {
        program: &'static str,
        index: usize,
        reason: String,
    },

    /// The program ID passed to the decoder was not the expected one.
    ///
    /// Callers should use `SolanaDexDecoder::decode` which dispatches by program
    /// ID — this variant is only returned when calling a program-specific decoder
    /// directly with a mismatched ID.
    #[error("wrong program: expected {expected}, got {got}")]
    WrongProgram { expected: &'static str, got: String },
}
