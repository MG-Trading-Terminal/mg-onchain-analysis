# Design 0024 — Reth ExEx Feature Flag (Sprint 24, S24-1)

**Date:** 2026-04-24
**Status:** Draft — awaiting user sign-off on §11 decisions before implementation
**Author:** architect agent
**Sprint:** 24 (S24-1: spec; S24-2: implementation; S24-3: closure)
**Carry-from:** Sprint 17 (7 sprints, oldest deferred infrastructure item)

**ADR refs:**
- ADR 0001 §D2 — push-based, reorg-aware ingestion as architectural pattern (Yellowstone analogy)
- ADR 0003 — self-sovereign infrastructure; ExEx is internal (our binary becomes Reth), does not violate
- ADR 0004 — Reth as EVM node choice; ExEx named as the "Yellowstone-gRPC analogue + future axis"
- ADR 0005 — MultiChainCoordinator; ExEx is transparent to coordinator and Indexer run loop

**Related designs:**
- `docs/designs/0020-server-binary-production-entry.md` — `onchain-service` binary entry (Sprint 19)
- `docs/designs/0021-detector-13-sandwich-mev.md` — EVM detector using chain-adapter (Sprint 20)

**External references (verified during Sprint 24 spec):**
- Reth ExEx notifications (v1.11.3 source): `github.com/paradigmxyz/reth/blob/v1.11.3/crates/exex/types/src/notification.rs`
- Reth NodeBuilder install_exex API: `github.com/paradigmxyz/reth/blob/v1.11.3/crates/node/builder/src/builder/mod.rs`
- Reth v1.11.3 workspace Cargo.toml (alloy version): `github.com/paradigmxyz/reth/blob/v1.11.3/Cargo.toml`
- Reth v2.1.0 release (April 2026 stable, for version tracking): `github.com/paradigmxyz/reth/releases`

---

## §1 Background

### S17 carry: why ExEx was deferred seven sprints

ADR 0004 (EVM node choice, Sprint 15) committed to Reth as the Ethereum execution client and
named ExEx as the streaming integration path — "the structural equivalent of Yellowstone gRPC."
ADR 0005 (Sprint 17) explicitly noted: "When ExEx lands, it affects only `EthereumAdapter::subscribe`.
The coordinator, the Indexer loop, and the shutdown protocol are entirely unchanged." Both ADRs
acknowledged ExEx as a Sprint 17+ feature flag.

The explicit defer reasons at Sprint 17, as recorded in SESSION-KICKOFF.md gotcha #59:

1. The `EthereumAdapter` `subscribe()` method was still a stub returning an empty stream.
   Full WS-RPC polling subscribe shipped Sprint 16. ExEx without a working baseline path
   had no testable fallback.
2. The server binary (`onchain-service`) was not yet materialised (shipped Sprint 19).
   The ExEx mode requires a different binary entry; a design against a placeholder server
   was premature.
3. Sprint 17 capacity was consumed by `MultiChainCoordinator`, which ExEx depends on.
4. Sprint 18-23 each had higher-priority items: Permit2 decoders, D11-D13 EVM detectors,
   smart-money pipeline, consumer integration.

As of Sprint 24: the server binary exists and boots cleanly (Sprint 19), the Ethereum WS-RPC
path is fully implemented and production-tested (Sprint 16 + 17), the `MultiChainCoordinator`
is wired and stable (Sprint 17), and smart-money consumer integration is closed (Sprint 23).
The dependency chain is clear. 7 sprints of carry ends here.

### ADR 0004 ExEx framing

ADR 0004 §1 (Streaming event channel) makes the analogy explicit:

```
Validator (agave) → Yellowstone gRPC plugin → chain-adapter subscribe() stream → indexer
```

The EVM equivalent with ExEx:

```
Reth node (EL) → ExEx in-process push → chain-adapter ExExRpcClient → indexer
```

The structural properties that make both attractive are identical:
- Push-based, no polling round-trip per block
- Reorg-aware: `ChainReverted`/`ChainReorged` deliver explicit rollback information
- No serialisation boundary: ExEx runs in-process with direct Rust type access

ADR 0004 also noted: "ExEx code lives in a separate file from the fallback path; the fallback
(`subscribe.rs`) works on any EVM node and is implemented first." That separation principle
is the foundation of the feature-flag design in this document.

### Current state entering Sprint 24

The WS-RPC path is the production EVM ingestion path:

```
crates/chain-adapter/src/ethereum/
  rpc.rs        — EthereumRpc trait + WsRpcClient (Sprint 16) + reconnect (Sprint 17)
  adapter.rs    — EthereumAdapter: ChainAdapter impl using Arc<dyn EthereumRpc>
  decoder.rs    — 8 ERC/UniV2/V3 decoders + 5 Permit2 decoders (Sprint 16 + 18)
```

The ExEx path needs one new file:

```
crates/chain-adapter/src/ethereum/
  exex.rs       — ExExRpcClient: implements EthereumRpc (or extension trait) behind
                  cfg(feature = "exex")
```

And the server binary needs a new entry point:

```
crates/server/src/bin/
  onchain_reth.rs  — Reth NodeBuilder + install_exex entry (Sprint 25)
```

The Sprint 24 scope is the API surface + feature gate compilation. The binary entry is Sprint 25.

---

## §2 Goals and Non-Goals

### Goals (Sprint 24)

1. Define the feature flag scope precisely: which crates, which Cargo feature name.
2. Define the trait extension surface: how `ExExRpcClient` relates to `EthereumRpc`.
3. Identify the workspace dependency additions: which Reth crates, at which version,
   with which alloy compatibility constraints.
4. Produce a skeleton `ExExRpcClient` that compiles under `--features exex` and
   satisfies the trait contract (stubs acceptable; full impl is Sprint 25).
5. Confirm the compile-time matrix: which crates compile under default features vs
   `--features exex`.
6. Document the two deployment modes for ops (WS mode vs ExEx mode).
7. Capture the 4-6 sign-off decisions in §11 for user approval before S24-2 begins.

### Non-Goals (deferred to Sprint 25 or later)

1. **`onchain-reth` binary entry point.** The Reth `cli::main` + `install_exex` wiring
   is Sprint 25. Sprint 24 ships the ExEx client API and feature gate; the binary is
   the follow-on task.
2. **Real Reth integration test.** An end-to-end test that boots Reth in-process and
   exercises the ExEx path requires the Reth runtime in CI (non-trivial). Sprint 25+.
3. **State-diff consumption.** ExEx delivers `ExecutionOutcome` alongside block events,
   which includes EVM state diffs (account balance changes, storage slot deltas). This
   is useful for richer detector signals (e.g., exact pool reserve at every block without
   `eth_call`). Deferred: no detector currently requires state diffs; add when needed.
4. **ExEx finality signals.** Reth's ExEx also delivers finality checkpoint events.
   Mapping these to `Event::SlotFinalized` for the EVM path is Sprint 25 scope.
5. **L2 chains via ExEx.** Base uses op-reth; Arbitrum uses Nitro. Per ADR 0004, L2
   chain ADRs are separate. ExEx on L2s is not this design's scope.

---

## §3 Architecture

### 3.1 Invariant: single deployable unit

ADR 0003 mandates a single deployable unit. ExEx mode satisfies this invariant differently
from WS mode, but does not violate it:

| Mode | Binary | How single |
|------|--------|-----------|
| WS mode (default) | `onchain-service` | Standalone binary; connects to Reth via WS 8546 |
| ExEx mode | `onchain-reth` | Reth-as-library + our ExEx plugin; single binary runs both |

In WS mode: two processes run (Reth + `onchain-service`), communicating over a local TCP socket.
In ExEx mode: one process runs (`onchain-reth`), with Reth's execution engine and our ExEx plugin
in the same process space. The ExEx mode is, if anything, *more* consistent with ADR 0003's
"single deployable unit" intent: it eliminates the inter-process network hop entirely.

The ExEx mode binary is a full Reth node that also runs our plugin. It replaces the separate
Reth Docker container. Ops replaces `docker-compose` running two containers (Reth + service)
with `onchain-reth` running as a single systemd unit.

### 3.2 Feature flag wiring (Decision 2)

The feature flag is named `exex`. It gates:

1. `crates/chain-adapter`: `ExExRpcClient` and the trait extension `EthereumRpcExEx`
2. `crates/server`: the `onchain-reth` binary entry point (Sprint 25)

The flag does NOT gate:
- `crates/indexer`, `crates/detectors`, `crates/storage`, `crates/common`, `crates/gateway` —
  these are entirely unaffected by which EVM transport is in use
- `crates/scoring`, `crates/graph`, `crates/token-registry`, `crates/dex-adapter`,
  `crates/client-sdk` — similarly unaffected

Recommended scope: Option C from the prompt — `chain-adapter` + `server`. The `chain-adapter`
crate owns the ExEx client implementation; `server` owns the binary entry that runs as a Reth
plugin. Both need the feature flag.

In `crates/chain-adapter/Cargo.toml`:

```toml
[features]
default = []
exex = [
    "reth-exex",
    "reth-primitives",
    "reth-node-builder",
    "reth-tracing",
]

[dependencies]
# ... existing deps unchanged ...

# ExEx-mode-only deps (compiled only when --features exex)
reth-exex        = { version = "=1.11.3", optional = true }
reth-primitives  = { version = "=1.11.3", optional = true, default-features = false }
reth-node-builder = { version = "=1.11.3", optional = true }
reth-tracing     = { version = "=1.11.3", optional = true, default-features = false }
```

In `crates/server/Cargo.toml` (existing `onchain-service` binary unchanged):

```toml
[features]
default = []
exex = [
    "mg-onchain-chain-adapter/exex",
    "reth-node-builder",
    "reth-cli",
]

[dependencies]
# ExEx-mode-only deps
reth-node-builder = { version = "=1.11.3", optional = true }
reth-cli          = { version = "=1.11.3", optional = true }
```

### 3.3 How ExEx notification types map to Event enum

`ExExNotification<N>` (Reth v1.11.3) has three variants:

| ExExNotification variant | Contains | Maps to Event |
|--------------------------|----------|---------------|
| `ChainCommitted { new: Arc<Chain<N>> }` | Newly committed blocks with txs + receipts | `Transfer`, `Swap`, `PoolEvent`, `TokenMeta` (decoded per block) |
| `ChainReverted { old: Arc<Chain<N>> }` | Reverted blocks | `Event::ReorgMarker { slot: block_number }` per reverted block |
| `ChainReorged { old: Arc<Chain<N>>, new: Arc<Chain<N>> }` | Both reverted and replacement blocks | `Event::ReorgMarker` for old blocks, then new-block events |

Note: ADR 0004 documented `ChainUpdated` as a variant name. The actual variant in Reth v1.11.3
source is `ChainReorged`. The semantic mapping is identical — the name difference is a Reth
internal API choice that was finalized after ADR 0004 was written.

The `Chain<N>` type (from `reth-primitives`) provides:
- `blocks()` — ordered iterator of `SealedBlockWithSenders`
- Each block has full transaction data and receipts
- Receipts contain the decoded logs (equivalent to `eth_getLogs` output)

This means the `ExExRpcClient` can produce the same decoded events as `WsRpcClient` by walking
`notification.committed_chain().blocks()` and running the existing `decoder.rs` functions over
the log data. No changes to `decoder.rs` are needed; it is a pure function over log byte data.

### 3.4 Layering diagram

```
crates/server (onchain-service binary — default features)
    └─ MultiChainCoordinator
           └─ Indexer<EthereumAdapter, PgStore, PgCheckpointStore>
                  └─ EthereumAdapter
                         └─ Arc<dyn EthereumRpc>
                                └─ WsRpcClient  (default path)
                                └─ ExExRpcClient  (--features exex, Sprint 25 binary)

crates/server (onchain-reth binary — --features exex, Sprint 25)
    └─ Reth NodeBuilder
           └─ install_exex("mg-onchain", |ctx| async { ... })
                  └─ ExExRpcClient (receives ExExNotification<N>)
                         └─ tokio::sync::mpsc → EthereumAdapter.subscribe() stream
```

Key insight from ADR 0005: the coordinator and `Indexer::run` are unchanged in both modes.
`EthereumAdapter` holds `Arc<dyn EthereumRpc>`. In WS mode that is `WsRpcClient`. In ExEx
mode that is `ExExRpcClient`. Everything above the adapter is identical.

---

## §4 Workspace Dependency Additions (Decision 1)

### 4.1 Version selection

The current Reth stable line has two branches as of April 2026:
- **v1.11.3** — the latest v1.x release (March 12, 2026). The last stable before v2.0.
- **v2.1.0** — the current stable release (April 20, 2026). Major version, Storage V2 as default.

**Recommendation: pin to Reth v1.11.3** for Sprint 24.

Rationale: Reth v2.0.0 was released April 8, 2026 — two weeks before this sprint opens.
A 2-week-old major version has not accumulated sufficient ecosystem validation for a
production analytics service. Reth v2.x introduces Storage V2 as the default DB format,
which implies migration risk and potentially unstable ExEx API surface during the v2.0.x
stabilization window. Reth v1.11.3 is stable, well-tested (18+ months of v1.x production
history), and the ExEx API was stabilized in the v1.x series. A Reth v2.x migration path
can be an ADR 0004 amendment in Sprint 28+ once the v2.x series stabilizes.

The version pin uses exact version (`=1.11.3`), not a range (`"1"`). Rationale: Reth crates
have a tight internal version coupling. Using `"1"` allows Cargo to resolve any `1.x.y`
version, which could drift to Reth v1.x+1.y where the ExEx API may have changed. The
`onchain-reth` binary must be compiled against the exact same Reth version it runs against
(the running node binary). Exact pinning enforces this constraint at compile time.

### 4.2 alloy version conflict analysis (Decision 7 from prompt)

This is a concrete risk. Reth v1.11.3 depends on `alloy = "1.6.3"` (verified against the
v1.11.3 workspace Cargo.toml). The workspace currently pins `alloy = "1.0"` (resolved to
1.0.22 per Sprint 16 notes).

**Gap: alloy 1.0.22 (current) vs alloy 1.6.3 (Reth v1.11.3 requirement).**

This is a dependency conflict. If both versions are present in the dependency tree, Cargo
will attempt to resolve. alloy follows semver: v1.x.y is backwards-compatible within the
1.x minor series theoretically, but in practice alloy types used by `reth-primitives` (e.g.,
`alloy_primitives::Address`, `alloy_primitives::B256`) must be the same version as those
used in our decoder. A type from `alloy 1.0` is a distinct Rust type from the same-named
type in `alloy 1.6` — they will not unify.

**Resolution path:**

Option A: Upgrade workspace alloy to `"1.6"` in Sprint 24. This bumps all existing EVM
code (`WsRpcClient`, `decoder.rs`) to alloy 1.6. Risk: breaking changes between alloy 1.0
and 1.6 that require code changes. Low probability (alloy maintains API stability within
major version), but requires audit + re-test of Sprint 16/18 EVM code.

Option B: Keep workspace at alloy 1.0. Accept that ExEx code uses alloy 1.6 types from
Reth crates, but our decoder continues to use alloy 1.0 types. These types will NOT be
directly substitutable. The ExEx path must convert `reth-primitives` types (alloy 1.6) to
raw byte/string representations before handing off to `decoder.rs` (alloy 1.0).

Option C (recommended): Upgrade workspace alloy to `"1.6"` and audit the delta before
Sprint 24 implementation begins. The `WsRpcClient` uses alloy at a low level (only
`alloy::transports::ws::WsConnect`, `alloy::rpc::client::RpcClient`, and
`alloy::primitives::B256` for subscription IDs). None of these are likely to have
breaking API changes between 1.0 and 1.6. The audit is small: read the alloy 1.1-1.6
changelogs for breaking changes in those three types.

**Recommendation: Option C. Bump workspace `alloy` from `"1.0"` to `"1.6"` in Sprint 24-2,
verify existing EVM tests still pass, then add Reth deps.** This is a prerequisite step
for S24-2 that must be done before the Reth crates are added.

### 4.3 Workspace dependency additions

Add to `Cargo.toml` `[workspace.dependencies]` (under the existing alloy entry):

```toml
# Reth ExEx integration — only compiled when --features exex is active.
# Pin to exact version to enforce node/plugin binary version parity.
# alloy must be upgraded to 1.6 first (see §4.2) to satisfy reth-primitives' dep.
reth-exex         = { version = "=1.11.3", optional = true }
reth-primitives   = { version = "=1.11.3", optional = true, default-features = false }
reth-node-builder = { version = "=1.11.3", optional = true }
reth-tracing      = { version = "=1.11.3", optional = true, default-features = false }
```

The `reth-cli` crate (needed for the binary entry's `cli::main`) is added to the `server`
crate's `Cargo.toml` directly, not the workspace, because it is a binary-only dependency
that only `crates/server` (for `onchain-reth`) uses.

**Crate rationale:**

| Crate | Why needed |
|-------|-----------|
| `reth-exex` | `ExExContext`, `ExExNotifications`, `install_exex` — the ExEx runtime |
| `reth-primitives` | `Chain<N>`, `SealedBlockWithSenders`, `Receipt` — block/receipt types for notification payload |
| `reth-node-builder` | `NodeBuilder`, `NodeConfig` — required by `onchain-reth` binary entry (Sprint 25) |
| `reth-tracing` | Reth's tracing setup — required when running as a Reth-embedded binary to unify log filtering |

**Crates NOT added:**

| Crate | Why excluded |
|-------|-------------|
| `reth-provider` | Provides historical state access (database queries). Not needed: ExEx receives state via notifications, not by querying the provider. Adding it would pull in the full Reth DB stack as a compile-time dep even under `--features exex`. |
| `reth-blockchain-tree` | Internal Reth consensus; exposed via `ExExContext` but not needed directly. |
| `reth-rpc` | Reth's JSON-RPC server. Not needed: we use alloy's separate WS client for the default path. |

### 4.4 Compile time impact

Adding `reth-exex` + `reth-primitives` + `reth-node-builder` behind an optional feature
incurs zero compile-time cost for the default build:

```bash
# Default (Solana-only or WS-EVM) — unchanged compile time:
cargo build --release

# ExEx mode — significantly longer first compile due to Reth dep tree:
cargo build --release --features exex
```

The Reth crate tree is large (50+ crates). The first `--features exex` build will take
substantially longer. Subsequent incremental builds are fast. CI is partitioned by feature
(see §10).

---

## §5 ExExRpcClient API Skeleton

### 5.1 Trait extension approach (Decision 3)

Three options were evaluated:

**Option A: Same `EthereumRpc` trait, `ExExRpcClient` implements it directly.**
Advantage: no new trait. Disadvantage: `EthereumRpc` is designed for request-response
(get block, get logs, subscribe). The ExEx path is push-based; implementing
`subscribe_new_heads` on `ExExRpcClient` requires bridging a push notification stream to
a pull-compatible stream interface. Achievable (via mpsc channel), but forces ExEx into
the request-response mold.

**Option B: Separate trait `EthereumExEx` for ExEx-specific notifications.**
Advantage: clean separation. Disadvantage: `EthereumAdapter` must now handle two distinct
trait objects — it currently holds `Arc<dyn EthereumRpc>`. Adding `Option<Arc<dyn EthereumExEx>>`
as a second field creates dual-mode logic scattered across `adapter.rs`.

**Option C (recommended): Trait extension `EthereumRpcExEx: EthereumRpc`.**
`ExExRpcClient` implements `EthereumRpcExEx`, which requires all of `EthereumRpc` plus adds
one ExEx-specific method: `notification_stream`. The `EthereumAdapter` stores
`Arc<dyn EthereumRpc>` regardless of mode — in WS mode a `WsRpcClient`, in ExEx mode an
`ExExRpcClient` downcast to `EthereumRpc`. The notification-stream method is called only
in ExEx mode boot, where the caller has the concrete `ExExRpcClient` type.

This design preserves the existing `EthereumAdapter` constructor signature unchanged.
`EthereumAdapter::new(rpc: impl EthereumRpc + 'static, ...)` accepts `ExExRpcClient` via
the `EthereumRpc` supertrait. No changes to `adapter.rs`.

### 5.2 `EthereumRpcExEx` trait skeleton

Location: `crates/chain-adapter/src/ethereum/exex.rs` (new file, compiled only under
`cfg(feature = "exex")`).

```rust
// crates/chain-adapter/src/ethereum/exex.rs
// Compiled only when: cfg(feature = "exex")

use std::pin::Pin;
use futures::Stream;
use reth_exex::ExExNotification;
use reth_primitives::EthPrimitives;
use crate::ethereum::rpc::EthereumRpc;
use crate::error::AdapterError;

/// Trait extension for ExEx-capable RPC implementations.
///
/// Implementors must also satisfy `EthereumRpc` — the base trait remains the
/// `EthereumAdapter`'s injection point. `EthereumRpcExEx` adds the ExEx-specific
/// notification stream for callers that have the concrete ExEx-mode type.
///
/// # Why a supertrait?
///
/// `EthereumAdapter::new(rpc: impl EthereumRpc + 'static, ...)` is unchanged.
/// `ExExRpcClient` satisfies `EthereumRpc` via the supertrait requirement.
/// The notification_stream method is only called at boot in the ExEx binary entry
/// (Sprint 25), where the caller holds the concrete `ExExRpcClient` type.
pub trait EthereumRpcExEx: EthereumRpc {
    /// Return a stream of `ExExNotification` messages from the Reth in-process channel.
    ///
    /// Notifications arrive in block order. The stream terminates only when the
    /// ExEx context is shut down (node shutdown). Callers MUST NOT call `subscribe_new_heads`
    /// when using this stream — the two paths are mutually exclusive.
    fn notification_stream(
        &self,
    ) -> Pin<Box<dyn Stream<Item = Result<ExExNotification<EthPrimitives>, AdapterError>> + Send + 'static>>;
}

/// ExEx-mode RPC client.
///
/// Receives `ExExNotification` messages from Reth's in-process channel and bridges
/// them to the `EthereumRpc` interface expected by `EthereumAdapter`.
///
/// # Design
///
/// The `ExExContext` delivers notifications via `ExExNotifications<N>`. This client
/// wraps that channel in a tokio mpsc pair to produce the stream types expected by
/// the `EthereumRpc` trait methods.
///
/// `get_latest_block_number`, `get_block_by_number`, and `get_logs` are implemented
/// by tracking the latest committed block from notifications. `subscribe_new_heads`
/// is implemented by mapping `ChainCommitted` notifications to `BlockHeader` values.
///
/// # Sprint 24 scope
///
/// This Sprint ships the type + trait impl with method stubs that compile and
/// satisfy the trait contract. Full notification-driven implementations are Sprint 25.
#[cfg(feature = "exex")]
pub struct ExExRpcClient {
    /// Latest committed block number, updated as notifications arrive.
    /// Used to satisfy `get_latest_block_number` without an RPC round-trip.
    latest_block: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Sender side of the internal notification bridge.
    /// The ExEx boot code feeds notifications into this channel.
    notification_tx: tokio::sync::mpsc::Sender<ExExNotification<EthPrimitives>>,
    /// Receiver side — wrapped into the notification_stream.
    /// Stored as Option<...> so it can be moved out exactly once into the stream.
    notification_rx: std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<ExExNotification<EthPrimitives>>>>,
}

#[cfg(feature = "exex")]
impl ExExRpcClient {
    /// Create a new `ExExRpcClient` with a bounded notification channel.
    ///
    /// The channel capacity should be sized to hold approximately 2-3 blocks'
    /// worth of notifications without backpressure, typically 8-16 is sufficient
    /// for Ethereum mainnet (~12s block time, ExEx receives at finality pace).
    pub fn new(channel_capacity: usize) -> (Self, tokio::sync::mpsc::Sender<ExExNotification<EthPrimitives>>) {
        let (tx, rx) = tokio::sync::mpsc::channel(channel_capacity);
        let latest_block = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let client = Self {
            latest_block,
            notification_tx: tx.clone(),
            notification_rx: std::sync::Mutex::new(Some(rx)),
        };
        (client, tx)
    }
}
```

### 5.3 EthereumRpc implementation on ExExRpcClient (Sprint 24 stubs)

The following method implementations are stubs that compile and return correct types:

- `get_latest_block_number`: reads from `latest_block` AtomicU64.
- `get_finalized_block_number`: returns 0 (stub; Sprint 25 maps finality notifications).
- `get_block_by_number`: returns `AdapterError::RpcError` with "ExEx mode: use notification_stream" (stubs; Sprint 25 derives block data from committed notifications).
- `subscribe_new_heads`: bridges `ChainCommitted` notifications to `BlockHeader` stream (Sprint 25).
- `get_logs`: bridges `ChainCommitted` notifications to log collection (Sprint 25).

The key point is that the Sprint 24 stubs satisfy the trait so that the feature-gated code
compiles and is testable at the API level. All stubs are marked with `TODO(sprint-25)` comments.

### 5.4 notification_stream implementation (Sprint 24 scope — non-stub)

This method IS implemented in Sprint 24 because it is the primary ExEx ingestion point:

```rust
#[cfg(feature = "exex")]
impl EthereumRpcExEx for ExExRpcClient {
    fn notification_stream(
        &self,
    ) -> Pin<Box<dyn Stream<Item = Result<ExExNotification<EthPrimitives>, AdapterError>> + Send + 'static>> {
        let rx = self.notification_rx
            .lock()
            .unwrap()
            .take()
            .expect("notification_stream called more than once");
        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok))
    }
}
```

This is the only ExEx-specific method. It moves the receiver out exactly once (enforced by
the `Option<...>` + `take()`). If called a second time, it panics with a clear message.
The stream yields `Ok(notification)` for each committed/reverted/reorged block; the caller
(ExEx boot code in Sprint 25) drives the notification loop.

---

## §6 Build Modes

### 6.1 Default mode (WS-only, current production)

```bash
# Build the existing standalone service binary (unchanged from Sprint 19):
cargo build --release --bin onchain-service

# Run:
./target/release/onchain-service --config config/service.toml
```

The binary connects to a separately-running Reth node via WS port 8546. Two processes
run: Reth (from Docker) and `onchain-service`. This is the current production mode.

**Compile scope:** all workspace crates; no Reth library crates compiled.

### 6.2 ExEx mode (Reth-as-library, Sprint 25 binary)

```bash
# Build the ExEx-mode binary (Sprint 25 — requires --features exex):
cargo build --release --features exex --bin onchain-reth

# Run as a single binary replacing both Reth and the service:
./target/release/onchain-reth \
  --config config/service.toml \
  --reth.datadir /data/reth \
  --reth.chain mainnet
```

The binary IS a full Reth node. It syncs Ethereum mainnet, runs all Reth execution stages,
AND runs our analytics plugin as an ExEx. No separate Reth process. No WS socket hop.

**Compile scope:** all workspace crates + `reth-exex`, `reth-primitives`, `reth-node-builder`,
`reth-tracing`. The Reth dep tree is large; first compile is slow (see §4.4).

### 6.3 Solana-only deployment (ExEx not active regardless of features)

When `[chains.ethereum] enabled = false` (the default per SESSION-KICKOFF.md gotcha #19),
the `MultiChainCoordinator` does not spawn an Ethereum indexer task. The ExEx feature flag
is irrelevant: even if compiled, `ExExRpcClient` is never instantiated.

This means operators running Solana-only deployments are completely unaffected by the ExEx
feature flag and do not pay the compile-time cost unless they opt in.

### 6.4 Feature flag inheritance

The `exex` feature in `chain-adapter` is activated by `server`'s `exex` feature:

```toml
# In crates/server/Cargo.toml:
[features]
exex = ["mg-onchain-chain-adapter/exex", ...]
```

A downstream crate activating `server/exex` transitively activates `chain-adapter/exex`.
The flag propagates correctly via Cargo's feature unification. No crate outside
`chain-adapter` and `server` needs to know about the `exex` feature.

---

## §7 Operator Deployment Guide

This section contains the content that will be added to `infra/ethereum-node/README.md`
as a new §9 ExEx Mode section in Sprint 25 (after the binary entry is shipped).

**Deferred edit:** `infra/ethereum-node/README.md` is NOT modified in Sprint 24.
Sprint 25 adds the section once `onchain-reth` exists and can be tested.

### 7.1 When to choose WS mode vs ExEx mode

| Consideration | WS Mode (default) | ExEx Mode |
|---------------|-------------------|-----------|
| Reth node | Separate process (Docker) | Embedded in `onchain-reth` binary |
| Processes | 2 (Reth + service) | 1 (`onchain-reth`) |
| Latency | +1 TCP round-trip per block | In-process; no socket hop |
| Binary build | `cargo build --release` | `cargo build --release --features exex` |
| Build time | ~5 min (baseline) | ~15-25 min first build (Reth dep tree) |
| Binary size | ~50 MB | ~200-400 MB (includes Reth EL) |
| Reth upgrade | `docker pull` + restart | Recompile + restart |
| Reorg handling | Hash-tracking ReorgBuffer (16 blocks) | `ChainReverted` notification (explicit) |
| Recommended for | Existing operators; simple ops | New deployments; lowest latency |

### 7.2 ExEx mode build prerequisites

ExEx mode requires:
1. The Rust toolchain pinned by Reth v1.11.3 (`rust-toolchain.toml` must match).
2. Sufficient disk for the binary: ~300-400 MB static binary.
3. The `--features exex` flag on all `cargo build` and `cargo test` invocations.
4. The Reth data directory pre-populated (snap sync is still the initial sync mechanism —
   see `infra/ethereum-node/README.md` §3 for the sync procedure).

### 7.3 Migration from WS mode to ExEx mode

Migration is a pure runtime/build swap with no data migration:

1. Stop `onchain-service` and the Reth Docker container.
2. Build `onchain-reth`: `cargo build --release --features exex --bin onchain-reth`.
3. Configure `onchain-reth` to use the existing Reth datadir (same `--reth.datadir` path).
4. Start `onchain-reth`. It reads the existing Reth DB and continues from the last checkpoint.

The Postgres data (adapter_checkpoints, events tables) is compatible between modes. The
`adapter_id = "ethereum"` checkpoint row continues to be used. The `last_signature`
field (which is EVM `block_number` in string form for EVM checkpoints) is unchanged.

---

## §8 Migration Path from WS-Only to ExEx

### 8.1 No data migration required

The storage layer (Postgres) is identical in both modes. Events are written with
`chain = 'ethereum'` and `block_number` as the key. The checkpoint record
(`adapter_checkpoints` table, `adapter_id = 'ethereum'`) uses `last_slot` as
`block_number`. Both `WsRpcClient` and `ExExRpcClient` write to the same tables via the
same `Indexer::run` loop via the same `PgStore` sink. No schema change, no data migration.

### 8.2 Checkpoint continuity

On switch from WS to ExEx mode, the `ExExRpcClient` starts from the checkpoint stored in
`adapter_checkpoints`. If the checkpoint is at block N, the ExEx mode starts consuming
notifications from block N+1. The Reth node will already have blocks N+1 onward in its
local DB; ExEx backfill (a Reth feature) can replay those blocks through the ExEx channel
before the live tip notifications begin.

The WS-mode `ReorgBuffer` (in-memory hash-tracking of last 16 blocks) is NOT needed in
ExEx mode. `ChainReverted` notifications provide explicit rollback information. The
`ReorgBuffer` is a WS-mode-only construct and is gated accordingly (it is inside
`adapter.rs`'s subscribe implementation which is not called in ExEx mode).

### 8.3 Rollback from ExEx to WS mode

Rollback is as simple as rebuilding without `--features exex` and restarting Reth + service.
The Reth DB is unchanged (ExEx mode does not write any additional Reth DB data). The
Postgres checkpoint is at a valid block number in both modes.

---

## §9 Compile-Time Matrix

Which crates compile under which feature combinations:

| Crate | default | --features exex | Notes |
|-------|---------|-----------------|-------|
| `common` | yes | yes | FROZEN; unaffected |
| `chain-adapter` | yes | yes | `exex.rs` gated by `cfg(feature = "exex")` |
| `indexer` | yes | yes | Unaffected; uses `ChainAdapter` trait only |
| `detectors` | yes | yes | Unaffected; `supported_chains()` dispatch only |
| `storage` | yes | yes | Unaffected; Postgres writes |
| `gateway` | yes | yes | Unaffected; REST/WS API |
| `scoring` | yes | yes | Unaffected |
| `graph` | yes | yes | Unaffected |
| `token-registry` | yes | yes | Unaffected |
| `dex-adapter` | yes | yes | Unaffected |
| `client-sdk` | yes | yes | Unaffected |
| `server` (onchain-service binary) | yes | yes (reth deps gated) | `onchain-service` entry unchanged |
| `server` (onchain-reth binary) | no | yes | Sprint 25; new binary target |
| Reth dep tree | no | yes | reth-exex, reth-primitives, reth-node-builder, reth-tracing |

**Critical invariant:** `cargo build --release` (no `--features exex`) must compile cleanly
with zero Reth crates in the dep tree. This preserves fast Solana-only deployment builds
and ensures that CI without Reth configured cannot regress.

**Clippy invariant:** `cargo clippy --workspace --all-targets -- -D warnings` must pass for
both default features AND `--features exex`. CI runs both.

---

## §10 Testing Approach

### 10.1 Default-features tests (existing, no change)

All 1293 existing tests continue to run under default features:

```bash
cargo test --workspace
```

These tests cover all existing functionality. ExEx code is gated by `cfg(feature = "exex")`
and does not compile under default features, so there is zero risk of ExEx stubs breaking
existing tests.

### 10.2 ExEx-features compilation test (Sprint 24 deliverable)

```bash
cargo build --features exex
cargo clippy --workspace --all-targets --features exex -- -D warnings
cargo test --workspace --features exex
```

The Sprint 24 deliverable is that all three commands succeed. The `cargo test --features exex`
run will execute the ExEx feature-gated unit tests (described below).

### 10.3 ExEx unit tests (Sprint 24)

Tests that can be written without a running Reth node:

1. **`ExExRpcClient::new` constructs correctly** — verify `latest_block = 0`, channel is
   connected.
2. **`notification_stream` moves receiver exactly once** — call it once, assert stream is
   returned; call it again and assert panic with `expect` message.
3. **`ExExRpcClient` satisfies `EthereumRpc` supertrait** — compile-time test: assign
   `Arc<ExExRpcClient>` to `Arc<dyn EthereumRpc>` and verify it compiles.
4. **`ExExRpcClient` satisfies `EthereumRpcExEx` supertrait** — same pattern.
5. **`get_latest_block_number` reads AtomicU64** — set the atomic, verify return value.
6. **`EthereumAdapter::new` accepts `ExExRpcClient`** — verify adapter construction works
   with the ExEx client.

These tests are in `crates/chain-adapter/src/ethereum/exex.rs` under `#[cfg(test)]` and
`#[cfg(feature = "exex")]`.

### 10.4 Deferred integration test (Sprint 25)

A full integration test that:
- Boots a minimal Reth node in-process (using `reth-node-builder` in test mode)
- Installs `ExExRpcClient` as an ExEx plugin
- Sends synthetic block notifications
- Asserts that `EthereumAdapter::subscribe` emits the expected events

This requires the Reth runtime in CI. Sprint 25 scope. Not required for Sprint 24 exit.

### 10.5 CI matrix (Sprint 24 addition)

Add a second CI job:

```yaml
# .github/workflows/ci.yml (existing job: default features)
- name: Build + test (default features)
  run: cargo test --workspace

# NEW job (Sprint 24):
- name: Build + clippy (--features exex)
  run: |
    cargo build --workspace --features exex
    cargo clippy --workspace --all-targets --features exex -- -D warnings
    cargo test --workspace --features exex
```

The ExEx CI job will be slower due to the Reth dep tree. Cache the `~/.cargo/registry`
and `target/` directories in CI to amortize across runs.

---

## §11 Decisions Requiring Sign-Off

Six decisions require explicit user confirmation before S24-2 (implementation) begins.

### Decision 1: Reth version pin — v1.11.3 vs v2.x

**Recommended: `=1.11.3`** (exact pin, v1.x line)

**Rationale:** v2.1.0 is the latest stable Reth release (April 20, 2026) but was released
2 weeks before this sprint. Storage V2 as default in v2.0 is a significant internal change
that may have ripple effects on ExEx API stability during the v2.0.x stabilization window.
v1.11.3 is the last stable v1.x release (March 12, 2026), production-proven, and the ExEx
API is stable in v1.x. A v2.x migration is a future ADR 0004 amendment sprint.

**Trade-off:** Pinning to v1.x means we track a maintenance branch, not the current stable.
Reth team may backport security fixes but will not add new features to v1.x. If a critical
ExEx API improvement lands in v2.x only, we must upgrade then. Exact version pin (`=1.11.3`)
rather than a range (`"1"`) avoids silent Reth-version drift between binary and library.

**User options:**
- **v1.11.3 exact (recommended):** `reth-exex = { version = "=1.11.3", optional = true }`
- **v2.1.0 exact:** `reth-exex = { version = "=2.1.0", optional = true }` — requires
  auditing ExEx API changes in v2.x changelog before S24-2.
- **v1.x range:** `reth-exex = { version = "1", optional = true }` — allows any v1.x
  minor upgrade; higher drift risk between node binary and library.

### Decision 2: Feature flag scope — chain-adapter only vs chain-adapter + server

**Recommended: Option C — `chain-adapter` + `server` both have `exex` features**

**Rationale:** `chain-adapter/exex` gates the `ExExRpcClient` struct and `EthereumRpcExEx`
trait. `server/exex` gates the `onchain-reth` binary entry (Sprint 25). They are logically
independent: `chain-adapter/exex` can compile without `server/exex` (useful for library
consumers). `server/exex` activates `chain-adapter/exex` as a dependency feature.

The alternative (Option A: chain-adapter only) would work for Sprint 24 since the binary
entry is deferred. However, establishing `server/exex` now avoids a Cargo.toml change in
Sprint 25 that would force a rebuild of everything.

**User options:**
- **Option C (recommended):** `chain-adapter/exex` (impl) + `server/exex` (binary, Sprint 25)
- **Option A (narrow):** `chain-adapter/exex` only for Sprint 24; add `server/exex` in Sprint 25

### Decision 3: Trait surface — same trait vs extension trait

**Recommended: Option C — `EthereumRpcExEx: EthereumRpc` extension trait**

**Rationale:** `EthereumAdapter::new(rpc: impl EthereumRpc + 'static, ...)` is unchanged.
`ExExRpcClient` satisfies `EthereumRpc` via the supertrait requirement, so `EthereumAdapter`
accepts it without modification. The `notification_stream` method is ExEx-specific and
called only at boot in the `onchain-reth` binary, where the concrete type is known.

Option A (same trait) forces `subscribe_new_heads` to bridge push notifications to a
pull interface, which is not wrong but is unnecessary complexity when the ExEx path's
natural entry point is `notification_stream`. Option B (separate trait) requires
`EthereumAdapter` to carry two optional fields — more invasive change to a stable struct.

**User options:**
- **Option C (recommended):** `EthereumRpcExEx: EthereumRpc` supertrait extension
- **Option A:** `ExExRpcClient` implements `EthereumRpc` directly; `notification_stream`
  added to `EthereumRpc` as an `Option` returning method (breaks existing impls)
- **Option B:** Separate `EthereumExEx` trait; `EthereumAdapter` gains a second optional field

### Decision 4: Binary naming and deployment mode

**Recommended: new binary `onchain-reth` for ExEx mode (Sprint 25); `onchain-service` unchanged**

**Rationale:** The ExEx mode binary requires Reth's `cli::main` as its entry point, which
takes over argument parsing and lifecycle management. This is fundamentally different from
`onchain-service`'s clap-based CLI (Sprint 19). Sharing a binary would require complex
conditional entry logic. Two named binaries with clear responsibilities is simpler.

```toml
# In crates/server/Cargo.toml (Sprint 25 addition):
[[bin]]
name = "onchain-service"
path = "src/bin/onchain_service.rs"

[[bin]]
name = "onchain-reth"
path = "src/bin/onchain_reth.rs"
required-features = ["exex"]
```

The `required-features = ["exex"]` annotation means `cargo build` without `--features exex`
will not build `onchain-reth`, avoiding accidental builds without the correct feature.

**User options:**
- **Two binaries (recommended):** `onchain-service` (default) + `onchain-reth` (exex)
- **One binary with runtime mode switch:** `--mode exex` flag on `onchain-service`; more
  complex conditional entry; not recommended (conflates two very different boot paths)

### Decision 5: alloy version bump — stay at 1.0 vs upgrade to 1.6

**Recommended: upgrade workspace `alloy` from `"1.0"` to `"1.6"`**

**Rationale:** Reth v1.11.3 depends on alloy 1.6.3. Our workspace currently pins alloy 1.0
(resolved 1.0.22). If both versions are present in the dep tree, alloy types will not unify
between Reth crates and our decoder — `alloy_primitives::Address` from 1.0 and from 1.6
are distinct Rust types, causing compile errors where conversion is needed. The cleanest
resolution is to upgrade the workspace alloy pin to `"1.6"`.

The alloy team maintains API stability within major versions (1.x). The API surface we use
is narrow: `alloy::transports::ws::WsConnect`, `alloy::rpc::client::RpcClient`,
`alloy::primitives::B256`. A changelog audit of alloy 1.1-1.6 must be done before S24-2
to confirm no breaking changes in those three items.

**Impact if breaking changes exist:** the `WsRpcClient` implementation in `rpc.rs` would
need patching. Given the narrow API surface, this is low probability and low scope if it
does occur.

**User options:**
- **Upgrade to alloy 1.6 (recommended):** clean dep tree; run existing EVM tests to verify
- **Stay at alloy 1.0:** ExEx client must convert Reth's alloy-1.6 types to raw bytes before
  our decoder; adds conversion boilerplate; increases ExEx client complexity

### Decision 6: Sprint 24 scope cap — skeleton only vs partial ExEx impl

**Recommended: Sprint 24 ships compilation + API surface + unit tests; full ExEx notifications impl is Sprint 25**

**Rationale:** The Sprint 24 S24-1 spec is architectural — this document. S24-2 implements
the feature gate and skeleton. Doing the full notification-to-Event mapping in Sprint 24
couples S24-2 to Sprint 25's binary entry, creating a risk of over-run. The skeleton gives
us:
1. Feature flag compiles cleanly under `--features exex`
2. `ExExRpcClient` + `EthereumRpcExEx` trait surface is testable at the API level
3. Existing tests all pass (default features unaffected)
4. The binary entry (Sprint 25) is unblocked — it just needs a compiling `ExExRpcClient`

The `TODO(sprint-25)` stubs make the deferral explicit and reviewable. The `notification_stream`
method (the primary ExEx entry point) IS implemented in Sprint 24 since it is simple
(move receiver out of Mutex) and required for the Sprint 25 binary entry to be non-trivial.

**User options:**
- **Skeleton (recommended):** Sprint 24 = compilation + API + notification_stream + unit tests
- **Full impl:** Sprint 24 also maps `ChainCommitted`/`ChainReverted` to full Event emission.
  Higher sprint capacity risk; requires the Sprint 25 binary to test end-to-end. Not blocked
  but not the conservative path.

---

## §12 Sprint 24 Deferral List

The following items are explicitly out of Sprint 24 scope and must not be attempted in S24-2
or S24-3 without a new session kickoff:

| Item | Why deferred | Target sprint |
|------|-------------|---------------|
| `onchain-reth` binary entry (`crates/server/src/bin/onchain_reth.rs`) | Requires Reth `cli::main` integration; distinct from API surface work; binary entry tests require real Reth runtime | Sprint 25 |
| Full `ChainCommitted` → `Transfer`/`Swap`/`PoolEvent` mapping in `ExExRpcClient` | Coupled to binary entry end-to-end test; skeleton + `notification_stream` is sufficient for Sprint 24 | Sprint 25 |
| `ExExRpcClient::get_logs` implementation | Only needed after Sprint 25 binary entry exists; stub acceptable | Sprint 25 |
| `ExExRpcClient` finality signal (`Event::SlotFinalized`) | Requires Reth finality notification wiring; not needed for stub | Sprint 25 |
| Real Reth integration test (subprocess or in-process Reth boot) | Requires CI infrastructure changes; significant scope | Sprint 25+ |
| `infra/ethereum-node/README.md` ExEx mode section | §7 above contains the content; edit deferred until binary exists | Sprint 25 |
| State-diff consumption (EVM state at block level) | No detector requires it; significant additional Reth dep surface (`reth-provider`) | Sprint 28+ |
| ExEx on L2 chains (Base via op-reth, Arbitrum via Nitro) | Separate ADR per L2; out of scope for L1-only ExEx | Phase 4 L2 ADRs |
| Reth v2.x migration | v2.x stabilization window not closed; ADR 0004 amendment | Sprint 28+ |

---

## §13 File Change Summary

Files created in Sprint 24 (S24-2):

| File | Action | Contents |
|------|--------|---------|
| `crates/chain-adapter/src/ethereum/exex.rs` | Create | `EthereumRpcExEx` trait + `ExExRpcClient` struct + stub impls + unit tests; gated by `cfg(feature = "exex")` |
| `crates/chain-adapter/Cargo.toml` | Modify | Add `[features] exex = [...]` + optional Reth deps |
| `crates/server/Cargo.toml` | Modify | Add `[features] exex = [...]` + optional Reth + reth-cli deps |
| `Cargo.toml` | Modify | Add `reth-exex`, `reth-primitives`, `reth-node-builder`, `reth-tracing` to `[workspace.dependencies]` (optional); bump `alloy` to `"1.6"` |
| `crates/chain-adapter/src/ethereum/mod.rs` | Modify | Add `#[cfg(feature = "exex")] pub mod exex; #[cfg(feature = "exex")] pub use exex::*;` |

Files NOT changed in Sprint 24:

- `crates/chain-adapter/src/ethereum/adapter.rs` — unchanged; `EthereumAdapter::new` accepts `ExExRpcClient` via existing `impl EthereumRpc + 'static` bound
- `crates/chain-adapter/src/ethereum/rpc.rs` — unchanged; `WsRpcClient` and `EthereumRpc` trait are not modified
- `crates/chain-adapter/src/ethereum/decoder.rs` — unchanged; decoder functions are pure over byte data
- `crates/chain-adapter/src/lib.rs` — unchanged; `ChainAdapter` trait and `Event` enum are not modified
- `crates/indexer/` — unchanged; coordinator and run loop are not modified
- `crates/common/` — FROZEN (gotcha #1)
- `crates/detectors/` — unchanged; no detector changes
- `infra/ethereum-node/README.md` — NOT modified in Sprint 24 (Sprint 25 adds §9 ExEx mode)
- CHANGELOG.md, SPRINTS.md, SESSION-KICKOFF.md — parent session handles
