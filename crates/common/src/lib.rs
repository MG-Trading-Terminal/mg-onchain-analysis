//! `mg-onchain-common` — shared domain types for the `mg-onchain-analysis` workspace.
//!
//! This crate is the dependency floor of the workspace. Every crate above it —
//! `chain-adapter`, `indexer`, `detectors`, `scoring`, `gateway`, `client-sdk`,
//! `storage` — imports from it. It has no runtime dependencies (no tokio, no tracing,
//! no database drivers).
//!
//! # Amount encoding
//!
//! This crate follows the `CLAUDE.md` §Code Style invariant strictly:
//!
//! - **`u128`** for raw on-chain token amounts. All `u128` fields serialize as JSON
//!   **strings** to avoid IEEE-754 double-precision truncation. A token with 18
//!   decimals and `10^9` total supply has a raw unit count of `10^27`, which loses
//!   precision when encoded as a JSON number.
//! - **`rust_decimal::Decimal`** for human-scaled quantities: USD values,
//!   percentages, tax rates, Gini coefficients. Serialized as JSON strings via the
//!   `serde-with-str` feature.
//! - **`f64`** is used ONLY in [`anomaly::Confidence`], which represents a
//!   probability estimate in `[0.0, 1.0]` guarded by the `Confidence::new`
//!   constructor. It serializes as a JSON number (precision is fine for a
//!   probability). Never introduce a bare `f64` field in any struct in this crate.
//!
//! # Serde conventions
//!
//! Wire format uses `camelCase` field names (`rename_all = "camelCase"`) for
//! compatibility with the RugCheck v1 API response shape (ADR 0001 §D6) and
//! JS consumer expectations. Internal Rust code uses `snake_case` as usual.
//!
//! Enum variants on the wire use `snake_case` or `lowercase` as documented per
//! type. See [`chain::Chain`] (`lowercase`), [`anomaly::Severity`] (`lowercase`),
//! [`event::DexKind`] (`snake_case`).
//!
//! # Address and TxHash deserialization
//!
//! [`chain::Address`] and [`chain::TxHash`] do not implement `serde::Deserialize`
//! because their wire form is a bare string that requires chain context to parse.
//! Callers that receive JSON must:
//! 1. Deserialize the surrounding struct's `chain` field first.
//! 2. Call `Address::parse(chain, &string)` or `TxHash::parse(chain, &string)`.
//!
//! This matches the OQ1 resolution: `Address` is `{ chain, canonical: String }`
//! with a `parse` constructor, not a `{ chain, value }` wrapped serde type.

pub mod amount;
pub mod anomaly;
pub mod chain;
pub mod error;
pub mod event;
pub mod token;

// Flat re-exports of the most commonly used types.
pub use anomaly::{AnomalyEvent, Confidence, Evidence, Severity};
pub use chain::{Address, BlockRef, Chain, TxHash};
pub use error::CommonError;
pub use event::{DexKind, PoolEvent, PoolEventKind, Swap, Transfer};
pub use token::{
    HolderSnapshot, InsiderNetwork, JupiterVerification, LockerInfo, MarketInfo, TokenMeta,
    TopHolder, TransferFeeConfig,
};
