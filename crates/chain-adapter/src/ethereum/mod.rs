//! Ethereum chain adapter — WebSocket JSON-RPC implementation.
//!
//! # Overview
//!
//! [`EthereumAdapter`] implements the [`ChainAdapter`] trait for Ethereum using a
//! self-hosted Reth node (ADR 0004) via WebSocket JSON-RPC (`eth_subscribe` + `eth_getLogs`).
//!
//! Per ADR 0003, no Alchemy/Infura/QuickNode in the production hot path. The adapter
//! connects to `ws://127.0.0.1:8546` (configurable) — the Reth WS endpoint defined in
//! `infra/ethereum-node/docker-compose.yml`.
//!
//! # Sprint 15 scope
//!
//! This sprint delivers the compile-green skeleton:
//! - Trait contract (`EthereumRpc`) and mock (`MockEthereumRpc`) — `rpc.rs`
//! - Adapter struct + `ChainAdapter` impl stubs — `adapter.rs`
//! - EVM type conversions — `types.rs`
//! - Reorg buffer (hash-tracking, depth 16) — `reorg.rs`
//! - Event signature constants + decoder stubs — `decoder.rs`
//!
//! Sprint 16 will wire the real `WsRpcClient` (alloy-rs WebSocket) and implement
//! full ABI decoding in `decoder.rs`.
//!
//! # Version pinning
//!
//! Sprint 15 adds no alloy-rs dependency (skeleton is dep-light by design).
//! Sprint 16 will add:
//! - `alloy = { version = "0.x", features = [...] }` to workspace Cargo.toml
//! - Wire `WsRpcClient` to use `alloy::providers::WsConnect`
//!
//! # Reth ExEx (Sprint 16+)
//!
//! Currently uses WebSocket JSON-RPC + `ReorgBuffer` hash-tracking for reorg detection.
//! Sprint 16+ will add an ExEx in-process variant (`exex.rs`) behind a feature flag
//! that receives `ChainCommitted`/`ChainReverted` directly from the Reth node process,
//! eliminating the hash-tracking state machine for the ExEx path.
//!
//! # Known gaps (Sprint 15)
//!
//! - `subscribe()` returns an empty stream (stub).
//! - `backfill()` returns an empty stream (stub).
//! - `health_check()` returns `Ok(())` without an RPC call.
//! - `tip()` returns `BlockRef { chain: Ethereum, height: 0 }` (stub).
//! - `WsRpcClient` methods are all `unimplemented!()`.
//! - Decoder functions return `Ok(None)` / `Ok(false)` (no ABI decode yet).

pub mod adapter;
pub mod decoder;
pub mod reorg;
pub mod rpc;
pub mod types;

// ExEx-mode client — compiled only with --features exex.
// Sprint 24: EthereumRpcExEx trait + ExExRpcClient skeleton.
// Sprint 25: onchain-reth binary entry wires ExExRpcClient into EthereumAdapter.
#[cfg(feature = "exex")]
pub mod exex;

pub use adapter::EthereumAdapter;
pub use rpc::{EthereumRpc, MockEthereumRpc, WsRpcClient};

#[cfg(feature = "exex")]
pub use exex::{EthereumRpcExEx, ExExRpcClient, ExExNotification};
