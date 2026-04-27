//! RPC abstractions for detectors that need on-chain simulation.
//!
//! # Design (DG1 resolution)
//!
//! `simulateTransaction` is not part of `DetectorContext` (which holds only
//! `PgStore` and `TokenRegistry`). Detectors needing RPC access receive an
//! `Arc<dyn SolanaRpc>` injected at construction time (option b from
//! docs/designs/0004-detector-01-honeypot.md §DG1).
//!
//! This module re-exports [`SolanaRpc`] and [`DecodedMint`] from
//! `mg_onchain_token_registry::rpc` so that `crates/detectors` can use them
//! without a direct (circular) dep on `token-registry`'s internal module path.
//!
//! # Usage
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use mg_onchain_detectors::rpc::SolanaRpc;
//! use mg_onchain_detectors::d01_honeypot::HoneypotDetector;
//! use mg_onchain_detectors::config::HoneypotConfig;
//!
//! // Inject RPC at construction time.
//! // In production: Arc::new(HttpSolanaRpc::new(&config))
//! // In tests:      Arc::new(MockSolanaRpc::default())
//! // fn make_detector(rpc: Arc<dyn SolanaRpc>, config: HoneypotConfig) -> HoneypotDetector {
//! //     HoneypotDetector::new(config, rpc)
//! // }
//! ```

// Re-export from token-registry's rpc module.
// `crates/detectors` already depends on `mg-onchain-token-registry`.
pub use mg_onchain_token_registry::SolanaRpc;
