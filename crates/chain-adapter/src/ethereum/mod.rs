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
//! # Reth ExEx (out-of-process bridge — Sprint 25)
//!
//! Per ADR 0006 (code-level self-sovereignty), the ExEx integration runs as a separate
//! `bridge/exex-bridge/` process that links `reth-exex` only there and exposes
//! `ChainCommitted`/`ChainReverted` notifications over our own gRPC proto. The
//! `chain-adapter` crate does NOT link any `reth-*` dependency.
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
pub mod http;
pub mod reorg;
pub mod rpc;
pub mod types;

pub use adapter::EthereumAdapter;
pub use http::{
    AddressClass, ContractAge, DiscoveredToken, EvmTokenMeta, OwnershipEventProbe,
    RecentHolderFlows, SimulateSellOutcome, SwapVolumeProbe, TransferEdge, classify_address,
    discover_recent_pairs, discover_recent_v3_pools, eth_get_transaction_count,
    evm_token_metadata, fetch_recent_holder_flows, find_contract_age, probe_ownership_events,
    probe_swap_volume, simulate_sell_evm,
};
pub use rpc::{EthereumRpc, MockEthereumRpc, WsRpcClient};
