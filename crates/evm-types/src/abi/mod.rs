//! Ethereum ABI encoder/decoder.
//!
//! Public surface:
//! - `decode::*` — individual type decoders
//! - `types::Token` — discriminated union of decoded values
//! - `error::DecodeError` — error variants

pub mod decode;
pub mod error;
pub mod types;

pub use decode::{
    decode_address, decode_bool, decode_bytes_dynamic, decode_bytes_fixed, decode_int,
    decode_string, decode_uint, resolve_field_offsets,
};
pub use error::DecodeError;
pub use types::Token;
