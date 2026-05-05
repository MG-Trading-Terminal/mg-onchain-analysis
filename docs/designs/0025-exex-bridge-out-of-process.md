# Design 0025 — Reth ExEx Out-of-Process Bridge (Sprint 25, S25-1)

**Date:** 2026-04-27
**Status:** SUPERSEDED 2026-04-27 (same session as drafting). User rescinded the bridge concept itself: a custom bridge process whose only purpose is to legitimise vendor-crate linkage is a kludge, and a kludge indicates a foundational architecture problem to be resolved by changing the architecture (not by sanctioning the kludge). The standard JSON-RPC + WebSocket interface to Reth is sufficient for our use case (sub-second latency vs sub-millisecond is irrelevant for analytics; reorg detection via in-tree `ReorgBuffer` already works); ExEx's marginal benefits do not justify a `bridge/` workspace, vendor-crate linkage, or pinning the architecture to Reth specifically (vs Geth/Erigon/Nethermind interchangeability via standard JSON-RPC). ADR 0006 has been amended in the same session to close the "bridge escape hatch" (§Decision and §Bridge Process Pattern). This document is preserved for historical record of the deprecated proposal; do not implement against it. Sprint 25 is repurposed as Solana stack divestment (`solana-sdk` + `yellowstone-grpc-client` divestment, parallel to Sprint 24's alloy divestment) — see the new design doc when produced.
**Author:** architect agent
**Sprint:** 25 (S25-1: spec; S25-2–S25-6: implementation; S25-7: integration test + runbook)

**ADR refs:**
- ADR 0001 §D2 — Yellowstone gRPC as the canonical out-of-process bridge pattern; ExEx bridge
  completes the symmetry with Solana.
- ADR 0003 — self-sovereign infrastructure; zero 3rd-party SaaS in the production hot path.
- ADR 0004 §1 — streaming channel rationale: push-based, reorg-aware delivery is the architectural
  requirement; §6 and §8 are superseded by ADR 0006 and must not be read as current policy.
- ADR 0006 — code-level self-sovereignty: the binding doctrine. `bridge/exex-bridge/` is the
  ONLY workspace permitted to link `reth-exex` and `reth-primitives`. The main workspace
  (`crates/chain-adapter`, all other crates) has zero `reth-*` dependencies.

**Related designs:**
- `docs/designs/0024-reth-exex-feature-flag.md` — SUPERSEDED. The Cargo feature-flag approach
  (in-process `reth-exex` in the main workspace) violates ADR 0006. Retained for historical
  traceability only. Do not implement against it.
- `docs/designs/0020-server-binary-production-entry.md` — `onchain-service` binary (Sprint 19).

**External references:**
- Reth ExEx notifications (v1.11.3): `github.com/paradigmxyz/reth/blob/v1.11.3/crates/exex/types/src/notification.rs`
- Reth v2.1.0 release page: `github.com/paradigmxyz/reth/releases/tag/v2.1.0`
- Yellowstone gRPC protocol: `github.com/rpcpool/yellowstone-grpc`
- gRPC specification: `grpc.io/docs/`
- Protocol Buffers language guide: `protobuf.dev/programming-guides/proto3/`

---

## §1 Background

### §1.1 Why ExEx integration was deferred to Sprint 25

ADR 0004 (Sprint 15) identified Reth's Execution Extensions as "the structural equivalent of
Yellowstone gRPC" for EVM: push-based, reorg-aware, in-process streaming with explicit
`ChainCommitted` / `ChainReverted` / `ChainUpdated` notification types. ADR 0004 named ExEx
integration as the natural Sprint 17+ follow-on once the WS-RPC bootstrap path was stable.

Over Sprints 17–23 the carry-forward reasons were: WS-RPC path not yet complete (Sprint 16),
`onchain-service` binary not yet materialised (Sprint 19), `MultiChainCoordinator` not wired
(Sprint 17), and higher-priority detector work (Sprints 18–23). Design 0024 attempted to close
the item in Sprint 24 using a Cargo feature flag (`--features exex`), but that approach was
invalidated on 2026-04-27 when the user accepted ADR 0006 (code-level self-sovereignty). The
feature-flag approach links `reth-exex` and `reth-primitives` into the main workspace binary,
violating the vendor-SDK isolation rule.

ADR 0006 §Migration assigns Sprint 25 the task of building the out-of-process bridge that was
always the architecturally correct path. This document is that bridge's design specification.

### §1.2 The Yellowstone precedent is the template

ADR 0001 §D2 already solved this problem for Solana. The Yellowstone Geyser plugin exposes a
`.proto` schema (`geyser.proto`, `solana-storage.proto`). That schema is the specification.
The chain-adapter crate consumes it as a gRPC stream generated from the proto file using
`tonic-build` + `prost`, without linking the vendor-shipped `yellowstone-grpc-client` crate.
The adapter never sees `solana_sdk` types; it works exclusively with proto-generated types
converted to `crates/common` types at the decode boundary.

The EVM bridge follows exactly the same pattern. The difference is that for Solana the proto
schema is authored by the Yellowstone project team, while for EVM we author the proto schema
ourselves. This is, if anything, a stronger position: we own the contract end to end.

---

## §2 Goals

- Implement an out-of-process `exex-bridge` binary that runs as a sibling to a self-hosted
  Reth node, subscribes to `ExExNotification` messages via Reth's in-process ExEx channel,
  and re-exposes those notifications as a gRPC server-streaming endpoint over a local socket.
- Zero `reth-*` dependencies in the main `mg-onchain-analysis` workspace. The
  `crates/chain-adapter/src/ethereum/exex.rs` client uses only `tonic`, `prost`, and
  `crates/evm-types`.
- Mirror the Yellowstone-pattern symmetry: both chains now have an out-of-process streaming
  bridge (Yellowstone plugin for Solana, `exex-bridge` for EVM) and a thin gRPC client inside
  `chain-adapter`. Operational complexity is consistent across chains.
- Reorg-aware push-based stream: `ChainCommitted`, `ChainReverted`, and `ChainUpdated`
  ExEx notification types map cleanly to our existing `Event::ReorgMarker` and
  `Event::SlotFinalized` shapes without modifying the `Event` enum.
- Resume-from-checkpoint semantics: on `onchain-service` restart, the chain-adapter sends
  `SubscribeRequest { from_block }` and the bridge replays from its in-memory buffer. No
  re-sync from the Reth DB required for typical restarts.
- Bridge upgradable independently of the main service binary: the bridge workspace has its own
  `Cargo.lock` and its own `target/` directory. Reth version bumps are scoped to the bridge.

---

## §3 Non-Goals

- **Embedded Reth in the main workspace.** Banned by ADR 0006 §Rule B (supersedes ADR 0004
  §8). The main workspace binary (`onchain-service`) will never link `reth-node-builder`.
- **Cargo feature-flag paths inside the main workspace.** Design 0024 is superseded; the
  `--features exex` approach is dead. No `cfg(feature = "exex")` gates will appear in
  `crates/chain-adapter` or `crates/server`.
- **L2 chains (Base, Arbitrum, BSC).** Each L2 has a distinct execution client (op-reth,
  Nitro) and its own reorg semantics. L2 ExEx bridges require separate ADRs when those chains
  are prioritised in Phase 4 or Phase 5.
- **Mempool ingestion.** `eth_subscribe("newPendingTransactions")` on the WS path is the
  self-hosted mempool access mechanism documented in ADR 0004. Mempool bridging over ExEx is
  a Phase 4 stretch goal (sandwich/MEV detector), not Sprint 25.
- **Replacement of the WebSocket path.** The existing WS-RPC client in
  `crates/chain-adapter/src/ethereum/rpc.rs` (`WsRpcClient`) remains in place as the fallback
  and as the path for `eth_call`-based requests (honeypot simulation, token metadata reads).
  ExEx is the streaming path. The two paths are complementary, not competing.

---

## §4 Architectural Overview

### §4.1 Three-process model

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  Host machine (bare metal or Docker host)                                   │
│                                                                             │
│  ┌───────────────────────────┐    in-process ExEx channel                  │
│  │  Reth node process        │◄─────────────────────────────────┐          │
│  │  (bare Reth binary)       │                                  │          │
│  │  port 8545 (HTTP RPC)     │                                  │          │
│  │  port 8546 (WS RPC)       │                                  │          │
│  │  port 30303 (P2P)         │    ┌──────────────────────────────┐         │
│  └───────────────────────────┘    │  exex-bridge binary          │         │
│                │                  │  (bridge/exex-bridge/)       │         │
│                │ ExExNotification  │                              │         │
│                │ broadcast chan    │  links: reth-exex            │         │
│                └──────────────────►         reth-primitives       │         │
│                                   │         reth-node-builder     │         │
│                                   │         tonic + prost         │         │
│                                   │                              │         │
│                                   │  gRPC server                 │         │
│                                   │  unix:///run/exex-bridge.sock│         │
│                                   │  (or tcp://127.0.0.1:9650)   │         │
│                                   └───────────────┬──────────────┘         │
│                                                   │                        │
│                               gRPC (tonic+prost)  │  mg.exex.v1 proto      │
│                               SubscribeResponse   │  stream                │
│                                                   │                        │
│  ┌────────────────────────────────────────────────▼──────────────────────┐ │
│  │  onchain-service process  (main workspace, crates/server binary)      │ │
│  │                                                                       │ │
│  │  crates/chain-adapter/src/ethereum/exex.rs                           │ │
│  │    EthereumExExClient { tonic::Channel }                              │ │
│  │    subscribe_from(block) → impl Stream<Item = Event>                  │ │
│  │                                                                       │ │
│  │  links: tonic, prost, crates/evm-types — ZERO reth-* deps            │ │
│  │                                                                       │ │
│  │  port 8080 (REST API)     port 8090 (WS streaming)                   │ │
│  └───────────────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────────────────┘
```

### §4.2 Narrative

Three processes run on the same host. The Reth node is a standard Reth binary: it syncs
Ethereum mainnet, processes blocks, and maintains the execution database. It is configured to
load the `exex-bridge` binary as an ExEx (execution extension) at node startup via
`reth-node-builder`'s `install_exex` callback. This means the bridge process is co-launched
with Reth — they share a tokio runtime and Reth hands the bridge a
`tokio::sync::broadcast::Receiver<ExExNotification>` channel. This is the only point at which
Reth-internal types cross into bridge code.

The bridge's only job is translation and forwarding. It receives each `ExExNotification`,
converts its content into our own protobuf message types (from the `mg.exex.v1` proto schema),
and dispatches the encoded message to all connected gRPC clients over a server-streaming
response. The bridge maintains a short in-memory ring buffer of the last N committed blocks
(default: 256, configurable) to support resume-from-checkpoint without requiring a Reth DB
query. Beyond this buffer, the bridge is stateless: it does not write to any database and does
not maintain per-client state beyond the active gRPC stream.

The `onchain-service` process contains `crates/chain-adapter/src/ethereum/exex.rs`, a thin
gRPC client (implemented with `tonic`) that connects to the bridge over the local socket and
translates each incoming `SubscribeResponse` proto message into `Event` values fed into the
existing `Indexer` pipeline. This file has no `reth-*` imports — it sees only proto-generated
types and `crates/evm-types` primitives. The `ChainAdapter` trait surface is unchanged.

The WS-RPC path (`WsRpcClient` in `rpc.rs`) continues to serve two purposes: it is the
fallback path when the ExEx bridge is unavailable, and it handles all `eth_call` requests
(honeypot simulation, token metadata reads) that are point-in-time queries rather than
streaming subscriptions.

This design completes the symmetry with Solana. On the Solana side:
`self-hosted validator → Yellowstone Geyser plugin (in-process with validator) → gRPC stream
→ SolanaAdapter (tonic client in chain-adapter) → Event stream → Indexer`

On the Ethereum side after this sprint:
`self-hosted Reth node → exex-bridge (in-process with Reth, separate workspace) → gRPC stream
→ EthereumExExClient (tonic client in chain-adapter) → Event stream → Indexer`

The operational pattern is identical. The team already understands it from the Solana side.

---

## §5 Workspace Layout

### §5.1 Directory tree

```
mg-onchain-analysis/
├── Cargo.toml                         ← main workspace root (UNCHANGED; bridge/ excluded)
├── Cargo.lock                         ← main workspace lockfile (UNCHANGED)
│
├── bridge/                            ← new top-level directory (excluded from main workspace)
│   └── exex-bridge/
│       ├── Cargo.toml                 ← SEPARATE workspace root; pins reth-* deps
│       ├── Cargo.lock                 ← SEPARATE lockfile; never merged into main workspace
│       ├── build.rs                   ← tonic-build invocation against proto symlink
│       ├── rust-toolchain.toml        ← must match the pinned Reth version's toolchain
│       └── src/
│           └── main.rs                ← Reth NodeBuilder + install_exex + gRPC server
│
├── crates/
│   ├── chain-adapter/
│   │   └── src/
│   │       └── ethereum/
│   │           ├── mod.rs             ← adds pub mod exex (no cfg gate)
│   │           ├── exex.rs            ← NEW: EthereumExExClient (tonic-only, no reth-*)
│   │           ├── rpc.rs             ← unchanged: WsRpcClient (fallback + eth_call)
│   │           └── adapter.rs         ← minor update: ExEx path wired alongside WS
│   │
│   └── chain-adapter-proto/           ← NEW crate in main workspace
│       ├── Cargo.toml                 ← deps: tonic, prost, tonic-build (build dep)
│       ├── build.rs                   ← tonic-build::compile_protos(["proto/bridge.proto"])
│       └── proto/
│           └── bridge.proto           ← canonical proto schema (see §6)
│
└── infra/
    └── ethereum-node/
        └── README.md                  ← add §N "ExEx bridge deployment" section (S25-7)
```

### §5.2 Bridge workspace `Cargo.toml` sample

The bridge workspace is a minimal two-crate setup: the bridge binary and the generated proto
bindings compiled from the same `.proto` source as the main workspace.

```toml
# bridge/exex-bridge/Cargo.toml
[workspace]
resolver = "2"
members = [".", "proto-gen"]

[package]
name = "exex-bridge"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "exex-bridge"
path = "src/main.rs"

[dependencies]
# Reth — exact version pin; must match the running Reth node binary.
# See §11 Decision 5 for version selection.
reth-exex         = { version = "=2.1.0" }
reth-primitives   = { version = "=2.1.0", default-features = false }
reth-node-builder = { version = "=2.1.0" }
reth-tracing      = { version = "=2.1.0", default-features = false }
reth-node-ethereum = { version = "=2.1.0" }

# gRPC server
tonic             = { version = "0.12" }
prost             = { version = "0.13" }

# Generated proto types (compiled within the bridge workspace from the shared .proto file)
# The .proto source file is symlinked from the main workspace:
#   bridge/exex-bridge/proto/bridge.proto -> ../../crates/chain-adapter-proto/proto/bridge.proto
# The bridge compiles it independently with its own tonic-build invocation.
exex-bridge-proto-gen = { path = "proto-gen" }

# Async runtime + utilities
tokio             = { version = "1", features = ["full"] }
tokio-stream      = { version = "0.1" }
futures           = { version = "0.3" }
anyhow            = { version = "1" }
tracing           = { version = "0.1" }
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

[build-dependencies]
tonic-build = { version = "0.12" }
```

The bridge has a separate `proto-gen/` sub-crate (generated from the proto symlink) so that
the bridge binary's `build.rs` remains a thin wrapper. The main workspace's
`crates/chain-adapter-proto/` also compiles the same proto file independently — the two builds
share the `.proto` source but produce separate Rust modules. No bridge Rust type ever appears
in the main workspace binary.

---

## §6 Proto Schema Design

The proto schema lives at `crates/chain-adapter-proto/proto/bridge.proto` in the main
workspace. A symlink at `bridge/exex-bridge/proto/bridge.proto` points to this file, so both
builds compile from a single source of truth.

Schema versioning policy: all types are in `package mg.exex.v1`. Additive changes (new
optional fields, new oneof variants) are permitted within v1 without a version bump. Any
field removal, semantic change, or reordering of oneof variants requires bumping the package
to `mg.exex.v2`. The intent is that a v1 bridge speaks to a v1 client without negotiation, and
a v2 bridge deployment prompts a corresponding client update. The bridge's gRPC endpoint URL
path encodes the version: `/mg.exex.v1.ExExBridge/Subscribe`.

```protobuf
// crates/chain-adapter-proto/proto/bridge.proto
//
// mg.exex.v1 — EVM ExEx bridge streaming protocol.
//
// Schema versioning policy:
//   - Additive changes (new optional fields, new message types referenced only in
//     new oneof variants) are backwards-compatible and do not require a version bump.
//   - Field removals, semantic changes, and reordering of oneof cases require
//     bumping to mg.exex.v2 and updating the gRPC path accordingly.
//   - Clients MUST handle unknown oneof variants gracefully (ignore and continue).
//
// Wire format: Protocol Buffers 3 (proto3). Transport: gRPC over HTTP/2.
// Default socket: unix:///run/exex-bridge.sock (configurable; see SubscribeRequest).

syntax = "proto3";

package mg.exex.v1;

option java_multiple_files = true;
option java_package = "mg.exex.v1";

// ---------------------------------------------------------------------------
// Service definition
// ---------------------------------------------------------------------------

service ExExBridge {
  // Subscribe to the EVM execution stream starting from the given block.
  //
  // The server returns a long-lived stream of SubscribeResponse messages.
  // The stream includes periodic Heartbeat messages (default: every 5 seconds)
  // so the client can distinguish a quiet-chain period from a dead connection.
  //
  // If from_block is within the bridge's in-memory buffer (default: last 256 blocks),
  // the server replays committed blocks from from_block to current head, then
  // transitions to live delivery. If from_block is older than the buffer, the server
  // returns a ResumeTooOld error status (gRPC OUT_OF_RANGE code) and the client
  // must fall back to eth_getLogs backfill via the WS path.
  //
  // If resume_token is set, from_block is ignored and the server resumes from the
  // position encoded in the token. resume_token is an opaque bytes field; clients
  // treat it as a cursor and must not parse it.
  rpc Subscribe(SubscribeRequest) returns (stream SubscribeResponse);

  // Health check — unary, returns immediately.
  // Returns OK if the bridge is connected to the Reth ExEx channel and the
  // most recently processed block is within 5 minutes of wall clock time.
  rpc Health(HealthRequest) returns (HealthResponse);
}

// ---------------------------------------------------------------------------
// Subscribe RPC
// ---------------------------------------------------------------------------

message SubscribeRequest {
  // Start block for resume. The bridge replays committed blocks from this
  // block number up to the current head, then transitions to live delivery.
  // Use 0 to start from the current head (no replay).
  uint64 from_block = 1;

  // Opaque resume token returned by the bridge in a prior SubscribeResponse.
  // If set, from_block is ignored. Clients should use this when available as it
  // allows the bridge to resume from a sub-block position (e.g., after a partial
  // block was processed).
  bytes resume_token = 2;
}

message SubscribeResponse {
  oneof notification {
    // The canonical chain has been extended by one or more blocks.
    // Blocks are ordered from oldest to newest (ascending block number).
    ChainCommitted committed   = 1;

    // One or more blocks have been reverted (reorg, short side chain eviction).
    // Blocks are ordered from newest to oldest (descending block number, i.e.,
    // the most-recently-added block is described first).
    ChainReverted  reverted    = 2;

    // An atomic reorg-with-replacement: some blocks were reverted and a new
    // fork was committed in their place. Clients should process reverted first,
    // then committed.
    ChainUpdated   updated     = 3;

    // Periodic keepalive. Sent every N seconds (default: 5) to distinguish
    // a quiet chain from a dead connection. Clients MUST NOT interpret absence
    // of committed/reverted notifications as an error if heartbeats are arriving.
    Heartbeat      heartbeat   = 4;
  }

  // Opaque cursor that can be passed as resume_token in a new SubscribeRequest
  // to resume from the position of this response. Clients should persist the
  // most recently seen resume_token alongside their block checkpoint.
  bytes resume_token = 5;
}

// ---------------------------------------------------------------------------
// Notification message types
// ---------------------------------------------------------------------------

// The canonical chain has advanced: from_block..=to_block are now committed.
message ChainCommitted {
  // First block number in this notification (inclusive).
  uint64 from_block = 1;
  // Last block number in this notification (inclusive).
  uint64 to_block   = 2;
  // Committed blocks in ascending order. There is at least one block.
  // In the normal case (no batch) this is a single block.
  repeated Block blocks = 3;
}

// Some previously-committed blocks have been reverted (reorg).
message ChainReverted {
  // First block number that was reverted (inclusive). May equal to_block for
  // a single-block reorg.
  uint64 from_block = 1;
  // Last block number that was reverted (inclusive). This is the block that
  // was previously at the chain tip before the reorg.
  uint64 to_block   = 2;
  // Reverted blocks in DESCENDING order (tip first). Consumers use this to
  // walk back from the tip when emitting ReorgMarker events.
  // Block content is provided so consumers can reconstruct which events to evict.
  repeated Block blocks = 3;
}

// An atomic reorg-with-replacement. Some blocks were reverted and new blocks
// were committed in their place as part of a single ExEx notification.
// Clients MUST process reverted.blocks before committed.blocks.
message ChainUpdated {
  ChainReverted  reverted  = 1;
  ChainCommitted committed = 2;
}

// Periodic keepalive message.
message Heartbeat {
  // Wall clock timestamp at the bridge (Unix epoch seconds).
  uint64 timestamp_secs = 1;
  // Block number of the most recently processed committed block. Allows the
  // client to assess lag without waiting for the next committed notification.
  uint64 latest_block = 2;
}

// ---------------------------------------------------------------------------
// Block, transaction, and log types
// ---------------------------------------------------------------------------

// A single EVM block with the fields required by the chain-adapter's decoder.
//
// Design rationale: we include only the data that our existing decoder and
// detectors actually consume. Fields like uncle hashes, state root, receipts
// root, etc. are omitted from v1. They can be added as optional fields in a
// backwards-compatible v1 amendment when a detector requires them.
message Block {
  // Block number (height).
  uint64 number      = 1;
  // Block hash (32 bytes, big-endian).
  bytes  hash        = 2;
  // Parent block hash (32 bytes, big-endian).
  bytes  parent_hash = 3;
  // Block timestamp (Unix epoch seconds).
  uint64 timestamp   = 4;
  // The address that received the block reward (beneficiary / coinbase).
  // 20 bytes, big-endian (not EIP-55 checksummed at proto level; the
  // chain-adapter normalises to checksum form at the decode boundary).
  bytes  miner       = 5;
  // Gas limit for this block.
  uint64 gas_limit   = 6;
  // Gas used by all transactions in this block.
  uint64 gas_used    = 7;
  // Transactions in execution order (index 0 = first in block).
  repeated Tx   txs  = 8;
  // All logs emitted by all transactions in this block, in emission order.
  // (tx_index, log_index) uniquely identifies a log within a block.
  repeated Log  logs = 9;
}

// A transaction summary. We include only the fields our decoders use.
// Full calldata is omitted from v1; it can be added when a detector requires it
// (e.g., D01 EVM honeypot simulation needs eth_call, not calldata replay).
message Tx {
  // Transaction hash (32 bytes, big-endian).
  bytes  hash        = 1;
  // Sender address (20 bytes). For ERC-4337 / meta-transactions, chain-adapter
  // follows the inner logs' transfer path, not this field alone.
  bytes  from        = 2;
  // Recipient address (20 bytes). Zero bytes = contract creation.
  bytes  to          = 3;
  // Transaction execution status: 1 = success, 0 = reverted.
  uint32 status      = 4;
  // Index of this transaction within the block (0-based).
  uint32 tx_index    = 5;
}

// An EVM event log emitted by a contract during transaction execution.
// Corresponds to the JSON-RPC `eth_getLogs` response item shape.
message Log {
  // Address of the contract that emitted this log (20 bytes, big-endian).
  bytes           address  = 1;
  // Topics array. topics[0] is the event signature hash (keccak256).
  // Indexed event parameters follow in topics[1..]. Non-indexed parameters
  // are ABI-encoded in data.
  repeated bytes  topics   = 2;
  // ABI-encoded non-indexed parameters.
  bytes           data     = 3;
  // Index of the emitting transaction within the block (matches Tx.tx_index).
  uint32          tx_index = 4;
  // Index of this log among all logs emitted in the block.
  uint32          log_index = 5;
}

// ---------------------------------------------------------------------------
// Health RPC
// ---------------------------------------------------------------------------

message HealthRequest {}

message HealthResponse {
  // True if the bridge is receiving ExEx notifications and has processed
  // a block within the last 5 minutes.
  bool   ok           = 1;
  // Latest block number processed by the bridge.
  uint64 latest_block = 2;
  // Wall clock timestamp of the latest processed block (Unix epoch seconds).
  uint64 latest_block_time_secs = 3;
  // Human-readable status message (empty string if ok == true).
  string message      = 4;
}
```

---

## §7 gRPC Service Contract

### §7.1 Service interface

The full service block is specified in §6 above. The key operational properties:

**`Subscribe` — server-streaming.** The server keeps the stream open until the client
cancels, the bridge shuts down, or a fatal error occurs. The gRPC cancellation model applies:
the client drops the connection (e.g., `onchain-service` restart), the server detects the
broken pipe on the next write attempt and tears down the per-client dispatch channel cleanly.
The bridge does NOT maintain durable per-client state — a reconnecting client sends a fresh
`SubscribeRequest` and the bridge resumes from the buffer.

**`Health` — unary.** Returns immediately. Clients (the `onchain-service` health endpoint and
the reconnect loop's pre-connect check) must budget a 5-second timeout. The bridge considers
itself healthy if: (a) the ExEx notification channel from Reth is open, and (b) the most
recently processed block timestamp is within 300 seconds of `SystemTime::now()`. Condition (b)
detects a halted Reth node even if the channel is technically open.

**Backpressure and overflow policy.** The bridge maintains a per-client `tokio::sync::mpsc`
channel with a bounded capacity of 64 messages. If a client falls behind and the channel
fills, the bridge disconnects that client (sends a `gRPC RESOURCE_EXHAUSTED` status) rather
than dropping messages silently or blocking the broadcast loop. This is the correct
tradeoff: a slow `onchain-service` reconnects and replays from the checkpoint buffer; it does
not cause the bridge to stall delivery to other clients (there will typically be only one
client, but the design supports multiple), and it does not create an unbounded memory buffer.
Rationale for disconnect-vs-drop: dropped messages would create a gap in the event stream
that the chain-adapter cannot detect without hash-based gap checking; a disconnect forces an
explicit reconnect and replay, which is the correct recovery path.

**Heartbeat interval.** The bridge sends a `Heartbeat` message every 5 seconds on each active
stream if no `ChainCommitted` or `ChainReverted` was sent in that interval. On Ethereum mainnet
with a ~12-second block time, heartbeats alternate with committed notifications under normal
conditions. On chains with faster block times, committed notifications supersede heartbeats.
The 5-second interval is configurable via bridge config (`[grpc] heartbeat_secs = 5`).

**Auth.** Default deployment: loopback only (bind to `unix:///run/exex-bridge.sock` or
`tcp://127.0.0.1:9650`). No TLS or mTLS in Sprint 25. Document mTLS as a Sprint 26 hardening
item for deployments where `onchain-service` and the Reth node run on separate hosts.

---

## §8 Bridge Implementation Outline

This section describes the bridge implementation at the component level. It is not code — code
is Sprint 25 task T25-3 and T25-4.

### §8.1 Entry point: `bridge/exex-bridge/src/main.rs`

The bridge binary entry point uses `reth-node-builder`'s `NodeBuilder` to launch a full Reth
node and register the bridge as an ExEx via `install_exex`. This is the only place in the
entire codebase where Reth's node-builder API is called. The registration callback receives an
`ExExContext`, which provides the `ExExNotifications<N>` stream.

The high-level startup sequence:
1. Parse bridge config (TOML file: `--config bridge.toml`). Config includes: `[reth]` section
   forwarded to Reth's own config (data dir, chain spec, log level), `[grpc]` section (socket
   path or TCP address, heartbeat interval), `[buffer]` section (ring buffer size in blocks).
2. Create the broadcast ring buffer (a fixed-size `VecDeque<Arc<Block>>` guarded by a `Mutex`,
   capacity `buffer.max_blocks`).
3. Create a `tokio::sync::broadcast::Sender<Arc<BridgeNotification>>` for fan-out to all
   connected gRPC clients.
4. Call `NodeBuilder::new(node_config).install_exex("mg-exex-bridge", |ctx| { ... }).launch()`
   — this starts the Reth node and, once the ExEx channel is ready, calls our closure with the
   `ExExContext`.
5. Inside the ExEx closure, spawn a tokio task that reads `ExExNotification` values from
   `ctx.notifications`, converts each to our `BridgeNotification` (proto-ready intermediate
   type), appends committed blocks to the ring buffer, and broadcasts on the fan-out sender.
6. Start a `tonic::transport::Server` listening on the configured socket. Register the
   `ExExBridgeService` implementation (which holds a clone of the broadcast sender and a
   reference to the ring buffer) as the gRPC service handler.

### §8.2 ExEx notification translation

Reth's `ExExNotification` has three variants (as of v1.11.3; the variant naming may differ
slightly in v2.x — verify at bridge build time against the pinned version's source):

- `ChainCommitted { new: Arc<Chain<N>> }` → translate `new.blocks_iter()` to repeated `Block`
  proto messages; package as `SubscribeResponse { committed: ChainCommitted { ... } }`.
- `ChainReverted { old: Arc<Chain<N>> }` → translate `old.blocks_iter()` to repeated `Block`
  proto messages in descending order; package as `SubscribeResponse { reverted: ChainReverted
  { ... } }`. Also evict the reverted blocks from the ring buffer.
- `ChainUpdated { old, new }` (or `ChainReorged` in some Reth versions — verify variant name
  at bridge build time) → translate both chains; package as `SubscribeResponse { updated:
  ChainUpdated { reverted: ..., committed: ... } }`.

Each `SealedBlockWithSenders` in a `Chain<N>` provides:
- `block.number`, `block.hash()`, `block.parent_hash`, `block.timestamp` → proto `Block` fields.
- `block.body.transactions` + `block.senders` → proto `Tx` messages with `from` populated.
- `block.receipts()` → `Receipt.logs` for each receipt → proto `Log` messages.

The `tx_index` and `log_index` fields are computed as the bridge walks the receipts slice in
order — they are not stored in the Reth types directly.

### §8.3 Per-client gRPC dispatch

When a new `Subscribe` RPC call arrives, the `ExExBridgeService` handler:
1. Reads `from_block` from `SubscribeRequest`.
2. If `from_block` is within the ring buffer's range, generates the replay sequence by
   iterating the buffer from `from_block` to the current head. Each buffer entry is translated
   to a `SubscribeResponse::committed` message. Replay messages are sent before the live
   broadcast begins.
3. If `from_block` is older than the buffer, returns `gRPC OUT_OF_RANGE` status with a message
   indicating the oldest buffered block. The `EthereumExExClient` in `chain-adapter` handles
   this error by falling back to WS `eth_getLogs` backfill (the existing path).
4. Creates a per-client `tokio::sync::mpsc` channel with capacity 64.
5. Subscribes the mpsc sender to the broadcast sender (using `broadcast::Receiver`).
6. Spawns a dispatch task that reads from the broadcast receiver, forwards to the mpsc sender,
   and disconnects the client if the mpsc channel is full (RESOURCE_EXHAUSTED).
7. Returns the mpsc receiver wrapped as a `tonic::Streaming` response, interleaved with
   periodic `Heartbeat` messages from a `tokio::time::interval`.

### §8.4 Ring buffer

The ring buffer is a `VecDeque<Arc<Block>>` indexed by block number. It holds the last
`buffer.max_blocks` committed blocks (default: 256). On `ChainReverted`, evict the reverted
block entries from the buffer before broadcasting the reverted notification. On
`ChainCommitted`, push new blocks to the back and pop old entries from the front when the
buffer exceeds capacity.

Buffer operations are `O(1)` amortised (VecDeque front/back push-pop). Lookup for replay is
`O(n)` over the buffer to find `from_block` — at 256 entries this is negligible. If the buffer
grows in future configurations, a secondary `HashMap<u64, Arc<Block>>` index can be added.

### §8.5 Crate dependencies (bridge workspace only)

| Crate | Role |
|---|---|
| `reth-exex` | `ExExContext`, `ExExNotifications`, `install_exex` |
| `reth-primitives` | `Chain<N>`, `SealedBlockWithSenders`, `Receipt`, `Log` |
| `reth-node-builder` | `NodeBuilder`, `NodeConfig`, `launch` |
| `reth-node-ethereum` | `EthereumNode` type (required by `NodeBuilder::with_types`) |
| `reth-tracing` | Reth's tracing initialiser (required when running as a Reth-embedded process) |
| `tonic` | gRPC server |
| `prost` | Proto serialisation |
| `tokio` | Async runtime |
| `tokio-stream` | `ReceiverStream`, `StreamExt` |
| `futures` | Stream combinators |
| `anyhow` | Error propagation |
| `tracing`, `tracing-subscriber` | Structured logging |

No crate in this list appears in the main workspace's `Cargo.toml`. The `Cargo.lock` for the
bridge workspace is separate and must never be committed to the main workspace root.

### §8.6 Reth version pin

The bridge workspace pins Reth at a specific release tag (see §11 Decision 5). The running
Reth node binary must be the same version. This is the same constraint as the Yellowstone
plugin version suffix (`+solana.X.Y.Z`) — a solved operational pattern. The deployment
runbook (`infra/ethereum-node/README.md`) records the pinned Reth version alongside the bridge
binary version and must be updated together whenever either changes.

---

## §9 Chain-Adapter Client Outline

This section describes the new `crates/chain-adapter/src/ethereum/exex.rs` client. It is
not code — code is Sprint 25 task T25-5.

### §9.1 `EthereumExExClient` struct

```
EthereumExExClient {
    channel: tonic::transport::Channel,
    config: ExExClientConfig,
}
```

`ExExClientConfig` holds the bridge socket URI (default: `http://[::1]:9650` for TCP loopback,
or a Unix domain socket URI), connection timeout, retry policy, and the resume-too-old fallback
mode flag.

The struct implements a public method:

```
async fn subscribe_from(
    &self,
    from_block: u64,
) -> Result<impl Stream<Item = Result<Event, AdapterError>> + Send + 'static, AdapterError>
```

This is the primary consumption path. The `EthereumAdapter` calls `subscribe_from` when the
ExEx client is configured and the WS fallback is not active.

### §9.2 Proto message to Event mapping

Each `SubscribeResponse` oneof variant maps to zero or more `Event` values as follows:

**`ChainCommitted`:** for each `Block` in `blocks`, decode each `Log` using the existing
`crates/chain-adapter/src/ethereum/decoder.rs` functions. The decoder functions are pure over
`(address: [u8; 20], topics: &[[u8; 32]], data: &[u8])` — they require no `reth-*` types and
are unchanged by this sprint. Each decoded log may yield `Event::Transfer`, `Event::Swap`,
`Event::PoolEvent`, or be discarded (unknown topic0). Each `Tx` with known `to` address yields
`Event::TokenMeta` if the contract has not been seen before (same logic as the WS path). Emit
one `Event::SlotFinalized { slot: block.number }` per block once the finality threshold is
crossed (tracked by the `EthereumAdapter`'s existing confirmation-depth counter).

**`ChainReverted`:** for each `Block` in `reverted.blocks`, emit one `Event::ReorgMarker {
slot: block.number }`. Blocks are delivered in descending order, so `ReorgMarker` events are
emitted from tip backward — this matches the semantics the indexer's `ReorgBuffer` expects.

**`ChainUpdated`:** emit the `ChainReverted` events first (tip-backward), then the
`ChainCommitted` events (oldest-first). This produces the correct ordering for the indexer:
evict reverted slots, then ingest replacement blocks.

**`Heartbeat`:** discard. The `EthereumExExClient` uses heartbeat absence as a connection
liveness signal (if no message, including heartbeat, is received within `2 * heartbeat_secs`,
the client reconnects) but does not emit any `Event` for heartbeats.

### §9.3 Reconnect and fallback logic

The `EthereumExExClient` wraps the gRPC stream in a reconnect loop with exponential backoff
(500ms, 1s, 2s, 4s, cap 30s), matching the pattern in `crates/chain-adapter/src/solana/
reconnect.rs`. On reconnect, the client reads the latest `Checkpoint` from the `EthereumAdapter`
and passes `from_block = checkpoint.slot` in the new `SubscribeRequest`.

If the bridge returns `OUT_OF_RANGE` (from_block older than ring buffer), the client signals
the `EthereumAdapter` to activate the WS fallback path: `eth_getLogs` backfill from
`from_block` to `current_head - 1`, then reconnect the ExEx stream from `current_head`. This
is the same catch-up pattern the indexer already supports for the initial backfill phase.

### §9.4 No hash-tracking state machine

The WS-RPC path (`subscribe.rs`) requires an in-memory `ReorgBuffer` that tracks the last 16
block hashes to detect reorgs by parent-hash comparison, because `eth_subscribe("newHeads")`
does not deliver explicit reorg notifications. The ExEx path has no such requirement: `ChainReverted`
and `ChainUpdated` are explicit reorg notifications with full block content. The
`EthereumExExClient` does not maintain any block hash state. The `ReorgBuffer` in `reorg.rs`
remains for the WS path and is not used when ExEx is active.

### §9.5 Zero `reth-*` dependencies

The entire `exex.rs` file imports:
- `tonic` and the proto-generated types from `crates/chain-adapter-proto/`
- `crate::ethereum::decoder` (pure byte functions)
- `crate::ethereum::types` (local `BlockData`, `RawLog` shapes)
- `crates/evm-types` (`Address`, `B256`, `U256`)
- `crates/common` event types
- `AdapterError`

It does not import `reth-exex`, `reth-primitives`, `alloy`, or any vendor chain SDK. This is
verified at compile time: `cargo check -p mg-onchain-chain-adapter` from the main workspace
root must complete without linking any `reth-*` crate.

---

## §10 Resume and Checkpoint Semantics

### §10.1 Normal restart (within buffer window)

When `onchain-service` restarts, the `EthereumAdapter` loads the last `Checkpoint` from
Postgres (`adapter_checkpoints` table, `adapter_id = 'ethereum'`). The checkpoint records
`slot` as the last fully-processed block number. The `EthereumExExClient` sends
`SubscribeRequest { from_block: checkpoint.slot + 1 }` to the bridge.

If `checkpoint.slot + 1` is within the bridge's ring buffer (i.e., the service was offline for
less than `buffer.max_blocks * avg_block_time` seconds), the bridge replays committed blocks
from `checkpoint.slot + 1` to the current head. On mainnet with 12-second blocks and a 256-
block buffer, this covers approximately 51 minutes of downtime without triggering the WS
fallback path. On high-throughput networks with shorter block times, the buffer covers a
shorter wall-clock window — see §10.3.

### §10.2 Long restart (buffer miss, WS fallback)

If `onchain-service` was offline longer than the buffer window, the bridge returns
`OUT_OF_RANGE`. The `EthereumExExClient` logs a warning and activates the WS fallback:

1. Use `WsRpcClient::get_logs` in range batches of `batch_size_blocks` (default: 1000) from
   `checkpoint.slot + 1` to `bridge_current_head - 1`.
2. Feed batched logs through the existing `decoder.rs` functions, emitting `Event` values.
3. Save intermediate checkpoints every `batch_size_blocks` blocks so that a second failure
   during backfill does not restart from the original checkpoint.
4. Once the WS backfill reaches `bridge_current_head - safety_margin_blocks` (default: 10),
   reconnect the ExEx stream from that block and transition back to push delivery.

This fallback path is identical to the normal first-run backfill flow already in production.
No new code is required for the fallback itself — only the trigger logic (detecting
`OUT_OF_RANGE` and routing to the existing backfill path) is new.

### §10.3 Buffer size trade-off

The default buffer size is 256 blocks. Memory consumption:

- Ethereum mainnet: ~12-second block time → 256 blocks ≈ 51 minutes. Average block size on
  Ethereum mainnet is approximately 100–200 KB (varies with gas usage). At 256 blocks × 150 KB
  average = approximately 38 MB resident memory in the bridge process.
- Networks with denser blocks: a 1-second block time would yield 256 blocks in 256 seconds
  (~4 minutes) with similar memory consumption per block.

256 blocks is configurable via `[buffer] max_blocks = 256` in the bridge config. Operators
running networks with shorter block times should increase this value. The memory budget is
linear: 512 blocks ≈ 76 MB, 1024 blocks ≈ 150 MB — all within acceptable limits for a
process running alongside a Reth node (which itself uses gigabytes of RAM for its state cache).

The `Arc<Block>` wrapper on each buffered block allows the broadcast fan-out to share block
data between the ring buffer and all in-flight per-client dispatch channels without copying.

### §10.4 Resume token

Each `SubscribeResponse` carries an opaque `resume_token` bytes field. In v1, the token
encodes `(block_number: u64, log_index: u32)` as a 12-byte little-endian value. This allows
the client to resume from a position within a partially-processed block (e.g., if the service
processed the first 50 logs of a 200-log block before crashing). The chain-adapter persists
the most recently seen `resume_token` alongside the `Checkpoint` in the `last_signature` field
(reusing the existing column as a base64-encoded bytes field for EVM). Clients may pass either
`from_block` or `resume_token`; if both are set, `resume_token` takes precedence.

---

## §11 Sign-Off Decisions

The following decisions require explicit user confirmation before T25-2 (proto schema
finalisation) and T25-3 (bridge skeleton) begin. Each presents a recommendation and the
trade-off to be accepted.

### Decision 1: Bridge process model

**Option A (recommended):** The `exex-bridge` binary launches a full Reth node internally via
`reth-node-builder`'s `NodeBuilder`. Reth and the bridge run in the same process, sharing a
tokio runtime. The ExEx notification channel is a `tokio::sync::broadcast` Receiver — no IPC,
no socket hop between Reth and bridge. This is the standard Reth ExEx deployment model and is
directly documented in Reth's ExEx guide.

**Option B:** A separately-running Reth binary (launched independently) communicates with the
bridge via Reth's `ExExEndpoint` IPC mechanism (an experimental Reth feature as of v1.x/v2.x).
The bridge binary is a pure gRPC server with no Reth node lifecycle.

**Recommendation: Option A.** It is simpler operationally (one process to manage, one
systemd unit, same restart semantics as the Yellowstone plugin embedded in the validator
binary). The Reth ExEx IPC endpoint (Option B) is experimental and less well-documented. Note
that Option A embeds Reth at the bridge level, NOT at the `onchain-service` level — ADR 0006
§Rule B bans vendor SDK embedding in the main workspace; the bridge workspace (`bridge/
exex-bridge/`) is explicitly exempt from that rule per ADR 0006 §3 ("vendor crates may live in
isolated bridges with separate `Cargo.lock`"). The bridge is the correct place for Reth linkage.

**Decision needed:** Confirm Option A (bridge launches Reth internally) or override to
Option B (bridge attaches to separately-running Reth via ExEx IPC). If Option B: note that the
ExEx IPC API must be verified as stable in the chosen Reth version before design proceeds.

### Decision 2: Proto package versioning

**Recommendation:** Use `package mg.exex.v1;` from day one. This costs nothing now and
establishes the versioning discipline before the first wire-format is deployed. The gRPC
service path `/mg.exex.v1.ExExBridge/Subscribe` encodes the version, so a future v2 rollout
can run both servers simultaneously during migration.

**Decision needed:** Confirm `mg.exex.v1` from day one, or defer versioning to a future ADR.

### Decision 3: Reorg notification granularity

**Current `Event` enum:** `Event::ReorgMarker { slot: u64 }` is a per-slot (per-block for EVM)
marker. The chain-adapter emits one `ReorgMarker` per reverted block.

**Option A (recommended):** Keep per-block `ReorgMarker` semantics. The `ChainReverted`
notification lists all reverted blocks; the client emits one `ReorgMarker` per block. This
requires no change to the `Event` enum, the indexer, or any detector. A deep 10-block reorg
emits 10 `ReorgMarker` events — acceptable given that deep Ethereum reorgs are extremely rare
post-Merge (1-2 blocks is the observed maximum under normal conditions).

**Option B:** Add `Event::ReorgRange { from: u64, to: u64 }` to the `Event` enum. More
efficient for deep reorgs, but requires changes to the `Event` enum (breaking `#[non_exhaustive]`
match arms in the indexer and any detector that handles reorgs), the indexer eviction logic,
and all tests that pattern-match `Event`. The `crates/common` module is marked FROZEN in
`SESSION-KICKOFF.md` gotcha #1 — this is a high-friction change.

**Recommendation: Option A.** Per-block `ReorgMarker` requires no changes to the main
codebase. The efficiency argument for Option B only matters for deep reorgs, which are
theoretically impossible past the Ethereum finality horizon (64 blocks / ~12.8 minutes).

**Decision needed:** Confirm per-block `ReorgMarker` (Option A), or accept breaking change to
`Event` enum for `ReorgRange` (Option B).

### Decision 4: Bridge ring buffer size

**Recommendation:** 256 blocks as the default (`[buffer] max_blocks = 256`), configurable.

Trade-off: 256 blocks ≈ 51 minutes at Ethereum's 12-second block time, at approximately 38 MB
resident memory. This covers the overwhelming majority of planned maintenance restarts and
process crashes. Operators who need longer replay windows can increase the buffer at the cost
of proportionally more bridge memory.

**Decision needed:** Confirm 256 blocks as the configurable default, or specify a different
default value.

### Decision 5: Reth version pin

Design 0024 proposed pinning to `v1.11.3` (the last v1.x release, March 2026) because v2.x
was only 2 weeks old at Sprint 24 time. As of Sprint 25 (2026-04-27), Reth v2.1.0 was released
on April 20, 2026 — one week before this sprint. The v2.x stabilisation window is still early.

**Option A:** Pin to `v1.11.3` (exact). The v1.x ExEx API is stable and well-tested across 18
months of production. Reth team will continue to backport critical security fixes to v1.x for
some period. ExEx API changes between v1.x and v2.x are not yet fully documented; bridging the
gap later will require a single bridge workspace update.

**Option B (recommended):** Pin to `v2.1.0` (exact). V2.1.0 is the current stable release.
Storage V2 is the default in v2.0, but this does not affect the ExEx API surface (ExEx
notifications are generated by the execution pipeline, not the storage layer). Using the current
stable avoids building on a maintenance branch. The ExEx API in v2.x is expected to be
backwards-compatible with v1.x semantics (same `ChainCommitted`/`ChainReverted`/`ChainUpdated`
notification model; verify variant names in v2.x source before T25-3 begins).

**Recommendation: verify the ExEx notification enum variant names in `paradigmxyz/reth@v2.1.0`
`crates/exex/types/src/notification.rs` before confirming this decision.** Design 0024 §3.3
noted that ADR 0004 documented `ChainUpdated` but the actual v1.11.3 variant is `ChainReorged`.
The same discrepancy risk exists in v2.x. A 10-minute source read before committing prevents
a surprise at T25-3 implementation time.

**Decision needed:** Pin to `v1.11.3` (stable, proven) or `v2.1.0` (current stable, verify
variant names first). Either answer is acceptable; the bridge `Cargo.lock` enforces the choice.

### Decision 6: Heartbeat interval

**Recommendation:** 5 seconds. This is long enough to avoid chatty keepalive overhead on a
12-second block time chain (heartbeats appear roughly every other block interval under quiet
conditions) and short enough to detect a dead bridge within 10 seconds (two missed heartbeats
triggers reconnect in the client).

**Decision needed:** Confirm 5 seconds, or specify a different interval.

### Decision 7: Bridge authentication and network binding

**Recommendation:** Bind the bridge gRPC server to loopback only by default. Two binding
options:
- Unix domain socket: `unix:///run/exex-bridge.sock` — lower overhead, no network stack,
  suitable when bridge and `onchain-service` run on the same host.
- TCP loopback: `tcp://127.0.0.1:9650` — compatible with Docker networking where Unix sockets
  require volume mounts.

Default: TCP loopback (`127.0.0.1:9650`) for Docker-compose compatibility with the existing
`infra/ethereum-node/` deployment model. Unix socket documented as an alternative for bare-metal.

No TLS or mTLS in Sprint 25. mTLS over a configurable certificate path is a Sprint 26
hardening item for deployments where the bridge and `onchain-service` are on different hosts.
The loopback binding provides adequate isolation for co-located deployments.

**Decision needed:** Confirm TCP loopback default with Unix socket as a documented alternative,
or override to Unix socket as default. Confirm deferral of mTLS to Sprint 26.

---

## §12 Sub-Task Breakdown

Tasks are sequenced by dependency. T25-1 through T25-3 can begin immediately after sign-off on
§11. T25-4 depends on T25-3. T25-5 depends on T25-2. T25-6 depends on T25-4 and T25-5. T25-7
depends on T25-6.

### T25-1: Bridge workspace scaffold (~80 LOC)

Create `bridge/exex-bridge/` with `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`,
`build.rs`, and a `src/main.rs` stub that compiles with `reth-node-builder` and `tonic` present
but with placeholder logic. Verify: `cd bridge/exex-bridge && cargo check` succeeds. Verify:
`cargo check` from the main workspace root does NOT compile `bridge/` contents (the bridge
directory is not in the main workspace `members` list).

**Dependencies:** none. **Estimated LOC:** 80 (Cargo.toml 50, main.rs stub 30).

### T25-2: Proto schema definition and code generation (~150 LOC)

Create `crates/chain-adapter-proto/` in the main workspace with `Cargo.toml`, `build.rs`,
and `proto/bridge.proto` (the full proto file from §6). Add `tonic-build` to the build
dependency. Verify: `cargo build -p chain-adapter-proto` generates Rust types. Create the
proto symlink in the bridge workspace: `bridge/exex-bridge/proto/bridge.proto →
../../crates/chain-adapter-proto/proto/bridge.proto`. Verify: `cd bridge/exex-bridge && cargo
build` generates the same types in the bridge context. Write a unit test in `chain-adapter-proto`
that constructs each message type and serialises/deserialises via `prost`.

**Dependencies:** T25-1 (bridge workspace must exist for symlink). **Estimated LOC:** 150 (proto
file 120, Cargo.toml 15, build.rs 15).

### T25-3: Bridge binary skeleton with gRPC server (~300 LOC)

Expand `bridge/exex-bridge/src/main.rs` to include: config parsing (TOML, command-line);
`ExExBridgeService` struct implementing the generated `ex_ex_bridge_server::ExExBridge` tonic
trait with stub method bodies; `Health` RPC returning a hardcoded `HealthResponse { ok: true,
... }`; `Subscribe` RPC returning an empty stream. Wire `tonic::transport::Server` on the
configured socket. Start `NodeBuilder` with a placeholder ExEx callback that logs "ExEx
connected" and returns. Verify: `cd bridge/exex-bridge && cargo build --release` produces a
binary that starts, opens the gRPC port, and responds to the `Health` RPC.

**Dependencies:** T25-1, T25-2. **Estimated LOC:** 300 (main.rs skeleton 200, config.rs 60,
service.rs stub 40).

### T25-4: Bridge full implementation — ExEx translation and fan-out (~400 LOC)

Implement the full ExEx notification translation loop in `bridge/exex-bridge/src/exex.rs`:
`ExExNotification` → `BridgeNotification` → proto message serialisation. Implement the ring
buffer (`buffer.rs`) with insert-on-commit and evict-on-revert semantics. Implement the
per-client dispatch in `service.rs`: replay from buffer, broadcast fan-out, bounded mpsc
channel with RESOURCE_EXHAUSTED disconnect. Implement the heartbeat interval in the Subscribe
stream handler. Write unit tests for: buffer insert/evict/replay, ChainReverted ordering,
ChainUpdated sequencing (reverted before committed), buffer miss returns correct gRPC status.
The tests use synthetic `Block` proto values — no running Reth node required.

**Dependencies:** T25-3. **Estimated LOC:** 400 (exex.rs 180, buffer.rs 80, service.rs full
180).

### T25-5: `chain-adapter` ExEx gRPC client (~300 LOC)

Create `crates/chain-adapter/src/ethereum/exex.rs`. Implement `EthereumExExClient` with
`subscribe_from(block) → impl Stream<Item = Result<Event, AdapterError>>`. Implement the proto
→ `Event` translation (§9.2). Implement reconnect loop with exponential backoff and `OUT_OF_RANGE`
fallback signal. Wire `EthereumExExClient` into `EthereumAdapter` as an optional second
transport: if the ExEx client is configured (socket URI present in config), it is used for
`subscribe()`; if absent, the WS path remains active. Update `crates/chain-adapter/Cargo.toml`
to add `chain-adapter-proto` as a workspace-path dependency. Write unit tests: proto message
→ correct Event mapping for ChainCommitted, ChainReverted, ChainUpdated; ReorgMarker ordering;
heartbeat discarded; OUT_OF_RANGE → fallback flag set.

**Dependencies:** T25-2. **Estimated LOC:** 300 (exex.rs 240, adapter.rs wiring 60).

### T25-6: Integration test (~200 LOC)

Write an integration test in `bridge/exex-bridge/tests/integration.rs` that:
1. Starts the bridge binary in a subprocess with a synthetic ExEx source (a tokio task that
   sends crafted `Block` proto messages into the broadcast channel without a real Reth node).
2. Connects an `EthereumExExClient` from the main workspace to the bridge socket.
3. Asserts that a `ChainCommitted` notification containing a synthetic ERC-20 Transfer log
   round-trips through bridge → gRPC → client → `Event::Transfer` correctly.
4. Asserts that a `ChainReverted` notification produces `Event::ReorgMarker` in descending
   block order.
5. Asserts that reconnect after simulated bridge restart produces correct replay from the
   buffer.

This test uses the bridge binary but not a real Reth node, so it runs in CI without Reth
infrastructure. Real Reth end-to-end testing is a Sprint 26+ task.

**Dependencies:** T25-4, T25-5. **Estimated LOC:** 200.

### T25-7: Infra runbook update (~120 LOC)

Add a section to `infra/ethereum-node/README.md` describing the two-process deployment model:
Reth node (Docker or systemd) + `exex-bridge` binary. Include: build instructions for the
bridge binary, config file template (`bridge.toml`), systemd unit for the bridge process,
health check command (`grpcurl` invocation), monitoring metrics exported by the bridge
(Prometheus on port 9651 or equivalent). Reference the hardware sizing already in
`infra/ethereum-node/README.md` §Hardware — no change needed.

**Dependencies:** T25-6 (confirms bridge API is stable). **Estimated LOC:** 120 (markdown).

---

## §13 Open Questions and Out-of-Scope Items

The following items are deliberately excluded from Sprint 25. Each has a designated future home.

**mTLS / bridge authentication (Sprint 26).** For single-host deployments (bridge and
`onchain-service` on the same machine or Docker network), loopback binding is sufficient
isolation. When operators deploy bridge and service on separate hosts (e.g., Reth on a
dedicated bare-metal node, `onchain-service` on a separate application host), TLS with mutual
authentication is required. Sprint 26 adds a `[grpc.tls]` config section with `cert_path`,
`key_path`, and `ca_path` fields, and updates the `EthereumExExClient` to present a client
certificate.

**State-diff consumption (Sprint 28+).** The `ExExNotification` delivers `ExecutionOutcome`
alongside block events, which includes per-account balance deltas and storage slot changes at
every block. This is materially richer than log-based detection — for example, it enables
reconstructing exact Uniswap v3 pool reserves without `eth_call`, which would benefit D01
honeypot simulation and the planned D14 pool manipulation detector. No current detector
requires this. Adding state diff fields to the proto schema (new optional `state_changes`
field on `Block`) and the corresponding buffer memory increase are Sprint 28+ scope, dependent
on a concrete detector requirement.

**L2 chain ExEx bridges (separate ADRs).** Base uses `op-reth`, which is a Reth fork with
OP Stack modifications. An `exex-bridge-base/` variant in `bridge/` could reuse most of the
bridge code with different Reth crate variants. Arbitrum uses Nitro (not Reth-based); its
streaming path is different entirely. Both require per-chain ADRs that this document does not
cover.

**Multi-region bridge deployment (Phase 5 infra).** Running a bridge in a second region
(e.g., EU and US co-located with respective Reth nodes) for latency reasons is not Sprint 25
scope. The `EthereumExExClient` config already supports specifying a URI (rather than a
hardcoded loopback), so the client-side change is a config-only update when the time comes.

**Reth v2.x ExEx variant name verification.** Design 0024 §3.3 documented that ADR 0004 named
`ChainUpdated` but the v1.11.3 source uses `ChainReorged`. This discrepancy must be verified
against whichever Reth version is selected in §11 Decision 5 before T25-4 begins. The bridge
implementation outline in §8.2 uses `ChainUpdated` as the conceptual name — the concrete Rust
match arm must use the actual variant name from the pinned version's source.

**four.meme graduation event (Sprint 25 SPEC-NOTE, BSC).** The BSC-specific `TODO(next-sprint)`
in `crates/chain-adapter/src/lib.rs` `evm_default_for_chain` is not addressed by this bridge
design. The BSC ExEx bridge (if built) would benefit from confirmed topic0 for four.meme
graduation events, but that is a BSC ADR concern, not the Ethereum L1 bridge.

---

## §14 References

| # | Source | Claim grounded |
|---|---|---|
| 1 | `docs/adr/0001-phase0-synthesis.md` §D2 | Yellowstone gRPC as out-of-process bridge pattern; provider-agnostic design; canonical streaming model |
| 2 | `docs/adr/0003-self-sovereign-infrastructure.md` | Zero 3rd-party SaaS in hot path; self-hosted Reth node; runtime risk argument |
| 3 | `docs/adr/0004-evm-node-choice-geth-vs-reth.md` §1 | ExEx as "Yellowstone-gRPC analogue"; `ChainCommitted`/`ChainReverted`/`ChainUpdated` notification types; reorg semantics |
| 4 | `docs/adr/0006-code-level-self-sovereignty.md` | Binding doctrine: `bridge/exex-bridge/` is the only workspace allowed to link `reth-*`; main workspace has zero vendor SDK deps; bridge process pattern section |
| 5 | `docs/designs/0024-reth-exex-feature-flag.md` | SUPERSEDED: historical context for the feature-flag approach and why it was rejected; Reth v1.11.3 pin rationale; v2.1.0 as April 2026 stable |
| 6 | `crates/chain-adapter/src/lib.rs` | `Event` enum (ReorgMarker, SlotFinalized, ChainCommitted variants); `ChainAdapter` trait; `SubscribeFilter` shape |
| 7 | `crates/chain-adapter/src/solana/subscribe.rs` | Yellowstone gRPC stream lifecycle; reconnect loop pattern; slot status state machine (CONFIRMED → FINALIZED / DEAD) |
| 8 | `crates/chain-adapter/src/ethereum/rpc.rs` | `EthereumRpc` trait; `WsRpcClient` (fallback path, continues to serve `eth_call`); reconnect TODO |
| 9 | `infra/solana-validator/README.md` | Structural template: hardware BOM, pinned versions section, systemd unit, health checks, monitoring — mirrors required for `infra/ethereum-node/` ExEx section |
| 10 | `github.com/paradigmxyz/reth/blob/v1.11.3/crates/exex/types/src/notification.rs` | `ExExNotification` enum variants (note: v1.11.3 uses `ChainReorged` not `ChainUpdated` — verify in target version) |
| 11 | `github.com/paradigmxyz/reth/releases/tag/v2.1.0` | Reth v2.1.0 release (April 20, 2026); current stable at Sprint 25 time |
| 12 | `reth.rs/exex/exex.html` | Reth ExEx documentation; `install_exex` API; `ExExContext`; production-readiness statement |
| 13 | `github.com/paradigmxyz/reth/blob/v1.11.3/crates/node/builder/src/builder/mod.rs` | `NodeBuilder::install_exex` API shape |
| 14 | `github.com/rpcpool/yellowstone-grpc` | Yellowstone gRPC protocol — the Solana-side precedent this design mirrors |
| 15 | `grpc.io/docs/` | gRPC specification — server-streaming RPC, cancellation, status codes (OUT_OF_RANGE, RESOURCE_EXHAUSTED) |
| 16 | `protobuf.dev/programming-guides/proto3/` | Protocol Buffers 3 language guide — oneof semantics, backwards-compatibility rules for additive changes |
| 17 | `docs.soliditylang.org/en/latest/abi-spec.html` | Ethereum ABI specification — governs `data` and `topics` encoding in `Log` proto message |
| 18 | `ethereum.org/en/developers/docs/consensus-mechanisms/pos/` | Ethereum finality: LMD-GHOST + Casper FFG; 64-slot finality window; post-Merge reorg depth statistics |
