# ADR 0005 — Multi-Chain Indexer Spawn Pattern

**Status:** Proposed — awaits user sign-off (5 decisions, §Sign-off Decisions).
**Date:** 2026-04-24
**Supersedes:** nothing. Extends ADR 0001 §D1 (Solana-first was a phase ordering choice, not a
permanent architecture constraint) and ADR 0004 (Reth node choice, now feeds into spawn pattern).
**Inputs:** SESSION-KICKOFF.md gotchas #33, #39, #49, #57, #59;
`crates/indexer/src/lib.rs` (current `Indexer<A,S,C>` + `IndexerBuilder`);
`crates/indexer/src/hooks.rs` (`PoolInitializeHook` trait);
`crates/chain-adapter/src/lib.rs` (`ChainAdapter` trait + `Event` enum);
`crates/chain-adapter/src/ethereum/adapter.rs` (Sprint 16 complete adapter);
`crates/server/src/streaming/scheduler.rs` (`DetectorScheduler`, unified `BTreeMap` queue);
`crates/server/src/streaming/worker.rs` (`SchedulerWorker`, chain-labelled metrics);
`crates/server/src/streaming/registry.rs` (`StreamingRegistry`, `(Chain, Mint)` key);
`crates/detectors/src/lib.rs` (D01-D11 registration, all Solana-scoped);
`crates/indexer/src/config.rs` (`AdapterConfig` enum, currently `Solana`-only variant);
ADR 0003 (single deployable unit, binding); ADR 0004 (Reth, Sprint 17+ ExEx flag).

---

## Context

Sprint 16 closed with `EthereumAdapter` functionally complete: real `WsRpcClient` via alloy-rs
1.0, 8 event decoders (ERC-20 Transfer, Uniswap v2/v3 Swap/Mint/Burn/Collect, PairCreated,
PoolCreated), and 11 mainnet fixture-replay integration tests passing. The adapter correctly
implements the `ChainAdapter` trait and passes `cargo clippy --workspace --all-targets --
-D warnings` clean.

What is missing is the architectural wire from `EthereumAdapter` into the `Indexer` run loop and
`DetectorScheduler`. Currently:

- `crates/indexer/src/lib.rs` is generic over a single `A: ChainAdapter`. It instantiates one
  stream, runs one event loop, tracks one `chain: String`, maintains one checkpoint.
- `crates/indexer/src/config.rs` has `AdapterConfig::Solana(SolanaAdapterConfig)` — no EVM variant.
- `IndexerBuilder::build` hardcodes `let chain = "solana".to_string()`.
- `Indexer::run` calls `SubscribeFilter::solana_default()` unconditionally (line 191).
- `crates/server/src/streaming/scheduler.rs` uses a single unified `BTreeMap<(Chain, String),
  PendingJob>` and one `async_channel` queue, both already keyed by `Chain` — the scheduler is
  already multi-chain-aware at the data-model level.
- `crates/server/src/streaming/registry.rs` uses `BTreeMap<(Chain, Mint), StreamingState>` —
  already multi-chain-aware.
- `crates/server/src/streaming/worker.rs` labels all metrics with `chain` — already multi-chain
  aware.
- `crates/detectors/src/lib.rs` exposes D01-D11, all implemented using Solana-specific storage
  queries (SPL token tables, Solana addresses, `pools.dex` values like `raydium_v4`). No detector
  has a `supported_chains()` method or a `chain` guard.

The architectural decision this ADR resolves: **how does `crates/server` spawn chain indexers
for Solana and Ethereum simultaneously while satisfying ADR 0003 (one binary) and keeping the
existing Indexer core untouched during S17-2 plumbing?**

Three patterns were evaluated. The recommendation is Pattern B (`MultiChainCoordinator`).

---

## Decision (RECOMMENDED — user sign-off required on §Sign-off Decisions)

**Pattern B: introduce a `MultiChainCoordinator` struct in `crates/indexer`.**

The coordinator wraps N `Box<dyn ChainAdapter>` instances, each paired with its own Indexer
task. It owns the per-chain task lifecycle (spawn, monitor, shutdown) and exposes a unified
control surface to `crates/server`. `Indexer<A, S, C>` is unchanged; each per-chain Indexer
instance remains generic over its single adapter type.

This recommendation is driven by three concrete technical observations from reading the source:

1. `Indexer::run` contains approximately 400 lines of battle-tested loop logic (batching,
   flush triggers, reorg handling, graph-writer integration, hook invocation). Touching that
   loop to add `tokio::select!` over N adapters (Pattern A) risks introducing regressions in
   the existing Solana path. A coordinator that spawns independent tasks preserves the existing
   loop invariant completely.

2. `IndexerBuilder::build` already hardcodes `chain = "solana"`. Adding a `Vec<Box<dyn
   ChainAdapter>>` param to `Indexer::new` (Pattern A) would require the Indexer to route events
   from multiple streams through its single loop — conflating per-chain ordering, checkpointing,
   and reorg semantics into one place. These are already independent per-chain concerns.

3. Pattern C (per-chain Indexer instances sharing storage, no coordinator) is architecturally
   correct but pushes the orchestration responsibility up into `crates/server/src/main.rs` (still
   a placeholder stub per gotcha #49). Embedding the multi-chain spawn logic in `server/main.rs`
   before `server` is materialised creates a coupling that will need to be factored out again
   when `server` is formalised (Sprint 17 Option E). The coordinator struct gives that logic a
   stable home in `crates/indexer` from day one.

---

## Recommended Architecture

### `MultiChainCoordinator` struct

Location: `crates/indexer/src/coordinator.rs` (new file).

```
crates/indexer/src/
  lib.rs                 — existing Indexer<A,S,C>, unchanged
  hooks.rs               — PoolInitializeHook, unchanged
  coordinator.rs         — NEW: MultiChainCoordinator
  config.rs              — extend AdapterConfig with Ethereum variant
```

Conceptual shape (informational, not prescriptive — S17-2 owns implementation):

```rust
/// Owns N per-chain Indexer tasks. Spawned once in crates/server.
pub struct MultiChainCoordinator {
    handles: Vec<tokio::task::JoinHandle<Result<(), IndexerError>>>,
    shutdown: ShutdownSignal,
}

impl MultiChainCoordinator {
    /// Spawn one Indexer task per configured chain adapter.
    /// Each Indexer<A, S, C> runs independently in its own tokio task.
    /// Storage (PgStore) is shared via Arc; chain column disambiguates rows.
    pub async fn spawn(
        chains: Vec<ChainIndexerConfig>,
        storage: Arc<StorageHandle>,
        shutdown: ShutdownSignal,
        graph_writer: Option<GraphIndexerWriter>,
        pool_initialize_hook: Option<Arc<dyn PoolInitializeHook>>,
    ) -> Result<Self, IndexerError>;

    /// Wait for all chain tasks to complete (shutdown or error).
    pub async fn join(self) -> Vec<Result<(), IndexerError>>;

    /// Return per-chain health status (used by /health endpoint).
    pub fn health(&self) -> Vec<ChainHealth>;
}
```

`ChainIndexerConfig` is a new config variant that wraps `AdapterConfig` + `BatchConfig` +
`adapter_id`. It replaces the current single `AdapterConfig` in `IndexerConfig`.

### Updated `AdapterConfig` enum

The existing `AdapterConfig` in `crates/indexer/src/config.rs` is extended:

```rust
#[non_exhaustive]
pub enum AdapterConfig {
    Solana(SolanaAdapterConfig),
    Ethereum(EthereumAdapterConfig),  // NEW in S17-2
}
```

`EthereumAdapterConfig` mirrors `SolanaAdapterConfig`: `rpc_url`, `reorg_depth`, `checkpoint`
path. No hardcoded values; all thresholds live in `config/adapters.toml`.

### How Indexer::new is called (per-chain)

Each chain gets its own `Indexer::new(adapter, sink, checkpoint_store, adapter_id, chain,
batch_cfg, shutdown, graph_writer, pool_initialize_hook)` call, identically to today. The 9-arg
signature is unchanged. `adapter_id` is chain-scoped (`"solana"`, `"ethereum"`) so checkpoint
rows are distinct in `adapter_checkpoints`. No checkpoint key collision.

The coordinator spawns:

```rust
tokio::spawn(async move { solana_indexer.run().await })
tokio::spawn(async move { ethereum_indexer.run().await })
```

Each task runs the full existing `Indexer::run` loop independently. Solana reorgs never touch
Ethereum state; Ethereum reorgs never touch Solana state. Storage rows are chain-tagged.

---

## Consequences

### Positive

1. **Zero changes to `Indexer<A,S,C>`** during S17-2. The battle-tested run loop, reorg
   handling, graph-writer integration, and hook invocation are untouched. Regressions in the
   Solana path are structurally impossible from the spawn-pattern change.

2. **Minimal surface area.** `MultiChainCoordinator` is ~100-150 LOC. It does one thing:
   spawn tasks and collect handles. All per-chain logic stays inside `Indexer`.

3. **Independent failure boundaries.** A panicking Ethereum adapter task does not kill the
   Solana task. The coordinator can log the failure, increment a metric, and restart the failed
   task with backoff — or surface it via `/health` and let ops decide.

4. **Checkpoint isolation.** Each adapter has its own `adapter_id` key in `adapter_checkpoints`.
   An Ethereum checkpoint write cannot corrupt the Solana checkpoint and vice versa (enforced by
   the existing `AsyncCheckpointStore` + Postgres unique constraint on `adapter_id`).

5. **ExEx path is additive.** When Sprint 17+ Reth ExEx feature flag ships, it replaces only
   the `EthereumAdapter::subscribe` implementation. The coordinator, the Indexer loop, and the
   shutdown protocol are entirely unchanged. The feature flag is transparent to the coordinator.

6. **`crates/server` stays clean.** `server/main.rs` constructs a `MultiChainCoordinator` and
   calls `coordinator.spawn(...)`. One call site, one handle, one `join().await`. The
   orchestration logic does not leak into the server binary.

7. **`SubscribeFilter` issue is contained.** Currently `Indexer::run` calls
   `SubscribeFilter::solana_default()` unconditionally (line 191 of `crates/indexer/src/lib.rs`).
   The coordinator approach exposes this per-chain: each chain's `ChainAdapter` owns its default
   filter, and `Indexer::run` calls `adapter.default_filter()` (or takes a filter param). This
   is a localized 1-line change, not a structural refactor.

### Negative

1. **Net new struct + 1 new file.** ~150 LOC coordinator + updated config. Small but non-zero
   S17-2 effort.

2. **Hook and graph-writer sharing requires `Arc`.** The `PoolInitializeHook` and
   `GraphIndexerWriter` are already `Arc<dyn PoolInitializeHook>` in the current Indexer. The
   coordinator passes `Arc::clone()` to each chain's Indexer. This is already the correct
   pattern — no change needed.

3. **Cross-chain analytics remain per-chain.** If a future detector needs to correlate Solana
   and Ethereum events (e.g., bridge-front-run detection), it must read from shared Postgres
   storage rather than from a shared in-memory event stream. This is correct: detectors are
   stateless over a query window, not stateful stream processors.

### Neutral

- The shared `PgStore` (`Arc<PgStore>`) is already the storage abstraction. Both chain tasks
  write to the same Postgres instance with `chain` as a discriminating column. No schema changes
  required — all event tables already have `chain` columns (verified in V00001-V00013).

- `DetectorScheduler` and `SchedulerWorker` are unchanged. They already key on `(Chain, Mint)`.
  When EVM events arrive in Postgres and EVM tokens appear in invalidation events, the scheduler
  will dispatch them to workers naturally — no scheduler changes needed in S17-2.

---

## Alternatives Considered

### Pattern A: `Vec<Box<dyn ChainAdapter>>` param on `Indexer::new`

**Rejected for S17-2. Acceptable as a long-term simplification only.**

The core problem is that `Indexer::run` maintains per-run state that is chain-specific:
`last_slot: u64`, `last_signature: Option<String>`, `last_block_time`, and the reorg-handler
call `handle_reorg(slot, &self.chain, ...)`. With N adapters, these become `Vec<u64>`,
`Vec<Option<String>>`, indexed by adapter position — but the event stream interleaves across
chains non-deterministically. The loop complexity grows super-linearly with chain count.

Worse, `Indexer::run` calls `SubscribeFilter::solana_default()` on line 191 — a Solana-specific
filter. Extending Pattern A would require either (a) a `ChainAdapter::default_filter()` method
(new trait method, breaking change), or (b) accepting filters alongside adapters in the Vec —
which is now two vecs in lockstep, a worse API than a coordinator.

The `tokio::select!` over N streams approach also complicates shutdown: `ShutdownSignal` is
currently a single cancellation token. With N streams in one loop, a stream error on chain 2
stops chains 1 through N — no per-chain failure isolation.

**Acceptable path for Sprint 20+ if the coordinator proves over-engineered for 2-3 chains.**

### Pattern C: Per-chain `Indexer` instances sharing storage (no coordinator)

**Rejected because it pushes orchestration into `crates/server/src/main.rs`.**

Pattern C is architecturally the same as Pattern B minus the coordinator wrapper. The difference
is where the spawn logic lives. Without a coordinator, `server/main.rs` must:

- Call `IndexerBuilder::build(solana_adapter, solana_config, shutdown.clone()).await?`
- Call `IndexerBuilder::build(ethereum_adapter, ethereum_config, shutdown.clone()).await?`
- Spawn both with `tokio::spawn`
- Collect both `JoinHandle`s
- Implement the join/error-propagation logic inline

This is 30-50 lines of orchestration code that does not belong in a binary entry point. When
the server binary is materialised (Sprint 17 Option E, gotcha #49), this boilerplate will need
to be factored out. Pattern B does that factoring upfront and charges the cost once.

Pattern C becomes the correct choice only if `crates/server` is permanently a thin binary with
no reusable orchestration logic — which contradicts the stated `IndexerBuilder` abstraction
intent (the builder comment explicitly says "use in server crate when wiring production
dependencies").

---

## Implementation Notes for S17-2

These constraints bind the Sprint 17 S17-2 implementation task.

### Step 1: Update `AdapterConfig` enum

File: `crates/indexer/src/config.rs`

Add `Ethereum(EthereumAdapterConfig)` variant. Mark `#[non_exhaustive]` (already present).
`EthereumAdapterConfig` should mirror the fields available in `EthereumAdapter::new`:
- `rpc_url: String` — ws:// URL for the Reth node WebSocket endpoint
- `reorg_depth: u64` — default 12 (matches `EthereumAdapter::with_reorg_depth`)
- `checkpoint_path: Option<PathBuf>` — for `FileCheckpointStore`

### Step 2: Fix `Indexer::run` hardcoded Solana filter

File: `crates/indexer/src/lib.rs`, line 191.

Current: `let filter = SubscribeFilter::solana_default();`

Either:
- Add a `fn default_filter(&self) -> SubscribeFilter` method to `ChainAdapter` trait (returning
  the chain-appropriate defaults), and call `self.adapter.default_filter()` here; or
- Accept an optional `SubscribeFilter` in `Indexer::new` (10th param) defaulting to the adapter's
  own default.

Recommended: add `fn default_filter() -> SubscribeFilter` as a provided method on `ChainAdapter`
with a Solana default — `EthereumAdapter` overrides with EVM contract-address filter. This avoids
a 10th constructor param.

### Step 3: Add `MultiChainCoordinator`

File: `crates/indexer/src/coordinator.rs` (new).

Expose from `crates/indexer/src/lib.rs` via `pub mod coordinator; pub use coordinator::*`.

`MultiChainCoordinator::spawn` iterates `Vec<ChainIndexerConfig>`, pattern-matches on
`AdapterConfig` variant, constructs the appropriate adapter (Solana or Ethereum), calls
`Indexer::new`, spawns a task, collects the handle.

### Step 4: Update `IndexerBuilder` or create `MultiChainCoordinator::build`

Current `IndexerBuilder` is Solana-specific (hardcodes `chain = "solana"`). For S17-2:

- Either extend `IndexerBuilder` to accept `AdapterConfig` (preferred — consistent pattern), or
- Fold construction into `MultiChainCoordinator::spawn` directly.

The `hardcoded chain = "solana".to_string()` comment in `IndexerBuilder::build` (line 437)
explicitly notes this limitation: "Phase 2 is Solana-only; Phase 4 reads from AdapterConfig."
S17-2 closes this gap.

### Step 5: Integration test

Add a `MultiChainCoordinator` integration test in `crates/indexer/tests/`:
- Construct a `MockSolanaAdapter` + `MockEthereumAdapter` (both returning fixed event streams).
- Spawn coordinator.
- Assert events from both chains land in the MockSink.
- Assert shutdown drains both chains' pending events.
- Assert per-chain checkpoints are distinct.

This test must NOT require a live node. Use `InMemoryCheckpointStore` and `MockSink` (already
exists in `crates/indexer/src/lib.rs` tests module — extract to test utilities).

### Files that must NOT change in S17-2

- `crates/indexer/src/hooks.rs` — `PoolInitializeHook` trait is chain-agnostic already.
- `crates/indexer/src/reorg.rs` — per-chain calls already take `&str chain` param.
- `crates/chain-adapter/src/lib.rs` — `ChainAdapter` trait, `Event` enum, `SubscribeFilter`.
- `crates/common/` — FROZEN (gotcha #1).
- `crates/detectors/src/` — no detector changes in S17-2 (EVM detectors are Sprint 18+).
- `crates/server/src/streaming/` — scheduler/worker/registry unchanged.

### What S17-2 deliberately does NOT do

- Wire EVM events into the `DetectorScheduler`. The scheduler is already `(Chain, Mint)`-keyed;
  EVM events will flow through naturally once EVM tokens appear in `invalidation_tx`. No scheduler
  changes are needed until EVM detectors exist (Sprint 18+).
- Implement Reth ExEx (gotcha #59 — Sprint 17+ feature flag, separate task).
- Implement WsRpcClient reconnect-on-disconnect (Sprint 17 A4 — separate task from ADR).

---

## Cross-Cutting Questions (mandatory per ADR 0005 brief)

### Q1. Spawn pattern — answered above: Pattern B (`MultiChainCoordinator`).

### Q2. Detector chain-awareness

D01-D11 are Solana-specific at the query level, not at the trait level. `Detector::evaluate`
receives a `DetectorContext` which already carries `ctx.chain: Chain`. Every detector can inspect
this field.

**Recommended approach: add `fn supported_chains(&self) -> &[Chain]` as an optional provided
method on the `Detector` trait, defaulting to `&[Chain::Solana]`.**

This is a non-breaking addition to the `Detector` trait (provided method with default). Existing
D01-D11 implementations inherit the Solana-only default at zero cost. EVM detectors override to
`&[Chain::Ethereum]` or `&[Chain::Solana, Chain::Ethereum]`.

The `SchedulerWorker` in `evaluate_token` can filter out detectors whose `supported_chains()`
does not include `job.chain` before dispatching. This is a 3-line guard added to the detector
dispatch loop.

**Alternative rejected: per-chain detector registry struct.** This would duplicate the existing
`Vec<ArcErasedDetector>` in `SchedulerWorker` into a `HashMap<Chain, Vec<ArcErasedDetector>>`.
It solves the same problem but adds indirection. The `supported_chains()` filter on a flat Vec
is simpler and testable without a registry.

**Alternative rejected: implicit runtime check (`ctx.chain`).** Detectors would silently emit
no events or incorrect events when run on the wrong chain (e.g., D02 querying Solana-specific
`pools.dex = 'raydium_v4'` against an Ethereum token). Silent failure is worse than explicit
filter-before-dispatch.

### Q3. Streaming scheduler — unified queue with chain-tagged jobs

The existing `DetectorScheduler` already uses a unified `BTreeMap<(Chain, String), PendingJob>`
and a single `async_channel`. This is the correct design:

- **Single queue, chain-labelled jobs** — workers pick up any chain's job from the queue.
  Workers are stateless with respect to chain (they read `job.chain` to build `DetectorContext`).
- **No per-chain queues** — would require per-chain worker pools, per-chain backpressure tuning,
  and per-chain overflow metrics. Unnecessary complexity for 2-3 chains.
- **Metrics already labelled by chain** — `streaming_evaluations_total{chain}`,
  `streaming_evaluation_duration_seconds{chain}`, `streaming_detector_evaluation_duration_seconds
  {chain, detector_id}`. These label the existing unified queue's behaviour per chain already.
- **Backpressure is shared across chains.** When the queue is full, jobs from any chain are
  dropped. This is acceptable: detector re-evaluation ticks are idempotent and the debounce
  window provides natural rate-limiting. Solana's higher event rate may starve Ethereum ticks in
  a shared queue — this is acceptable in MVP; a weighted-fair MPMC queue is a Phase 5 concern.

Decision: unified queue, no changes to scheduler or worker in S17-2.

### Q4. Reorg semantics — explicit reaffirmation

Per-chain reorg buffers are isolated by design:

- Solana reorg (`Event::ReorgMarker { slot }`) is handled by `handle_reorg` in the Solana
  Indexer task. It touches only Solana rows in Postgres (chain = 'solana') and the Solana
  checkpoint.
- Ethereum reorg (same `Event::ReorgMarker { slot }` where `slot` is the EVM block number) is
  handled by `handle_reorg` in the Ethereum Indexer task. It touches only Ethereum rows
  (chain = 'ethereum') and the Ethereum checkpoint.
- The `handle_reorg` function already takes `chain: &str` and `adapter_id: &str` as parameters.
  Row deletion uses `WHERE chain = $1 AND block_slot >= $2`. There is zero cross-chain coupling
  in the reorg path.
- Cross-chain reorg coordination is explicitly NOT needed. Solana's slot-based reorg and
  Ethereum's hash-parent-tracking reorg are independent consensus mechanisms. A Solana reorg at
  slot N has no implication for Ethereum block state, and vice versa.

**There is no cross-chain reorg concern. This is a deliberate non-requirement.**

### Q5. `PoolInitializeHook` shape for EVM

`PoolInitializeHook::on_new_token_launch` currently receives `chain: Chain`, `deployer: &str`,
`token0: &str`, `token1: &str`, `observed_at: DateTime<Utc>`, `block_ref: BlockRef`. The
`chain: Chain` parameter already disambiguates Solana vs Ethereum calls.

The EVM equivalent of Raydium/Meteora pool init (`PoolEvent::Initialize`) is Uniswap v2
`PairCreated` / Uniswap v3 `PoolCreated`. Both are decoded by the Sprint 16 `EthereumAdapter`
decoder and emitted as `Event::PoolEvent(PoolEvent { kind: PoolEventKind::Initialize { ... } })`.

**Recommendation: same `PoolInitializeHook` trait, no EVM-specific variant.**

Rationale:
- The hook signature is already chain-agnostic (`chain: Chain` parameter).
- D09 BOCPD and D10 LaunchAudit currently implement the hook for Solana. When EVM detectors
  land (Sprint 18+), they will implement the same hook with Ethereum-specific logic gated on
  `if chain == Chain::Ethereum { ... }` inside the hook implementation. The hook dispatch
  happens inside `Indexer::run` which already has the correct `pe.chain` value.
- A parallel `EvmPoolInitializeHook` trait would require the Indexer to maintain two optional
  hook fields and two dispatch branches — doubling the hook plumbing for no structural benefit.
- D09 and D10 are invoked via `Arc<dyn PoolInitializeHook>`. EVM variants will be wrapped in
  the same `Arc<dyn PoolInitializeHook>` pattern. If an EVM-specific D09 is introduced, it can
  either share the existing D09 struct (adding EVM logic behind a chain guard) or be a new
  struct that also implements the same trait.

**For S17-2 specifically:** the hook is not touched. D09 and D10 implementations will receive
`chain = Chain::Ethereum` calls once the Ethereum Indexer starts and encounters
`PoolEvent::Initialize`. D09 will no-op on Ethereum (its BOCPD state is Solana-trained). D10's
`initial_liquidity_sol < 5.0` threshold is explicitly Solana-denominated — it should no-op on
Ethereum with a `if chain != Chain::Solana { return Ok(()); }` guard, added in Sprint 18 when
EVM detectors are formalised. For now, a spurious low-confidence D10 event for Ethereum pools
is acceptable (false positives are cheap; CLAUDE.md §Detector Rules).

---

## Sign-off Decisions

The following five decisions require explicit user confirmation before S17-2 begins.

### Decision 1: Spawn pattern — Pattern B (`MultiChainCoordinator`) vs alternatives

**Recommended: Pattern B.**

Key trade-off: Pattern B adds ~150 LOC coordinator in `crates/indexer`. Pattern A (Vec on
Indexer) avoids the new file but forces shared per-chain state into one loop — a complexity
trap that grows with chain count. Pattern C (no coordinator, spawn in server) is functionally
equivalent to B but defers the factoring to `server/main.rs` materialisation.

**User options:**
- **B (recommended):** `MultiChainCoordinator` in `crates/indexer/src/coordinator.rs`.
- **C (acceptable):** spawn two independent Indexers directly in `server/main.rs` with no
  coordinator wrapper. Simpler now; requires refactor when server is formalised.
- **A (not recommended):** Vec-of-adapters on Indexer. Requires non-trivial Indexer refactor.

### Decision 2: `Detector::supported_chains()` method

**Recommended: add provided method `fn supported_chains(&self) -> &[Chain]` to `Detector` trait,
defaulting to `&[Chain::Solana]`. Filter in `SchedulerWorker::evaluate_token` before dispatch.**

Key trade-off: this is a non-breaking trait extension — all existing D01-D11 inherit the
default at zero cost. The alternative (per-chain detector registry `HashMap`) is more explicit
but adds indirection. The alternative (implicit `ctx.chain` check) risks silent wrong-chain
evaluation.

**User options:**
- **`supported_chains()` method (recommended):** explicit, non-breaking, filterable.
- **Per-chain registry `HashMap<Chain, Vec<ArcErasedDetector>>`:** more explicit routing,
  slightly higher construction cost in `server/lib.rs`.
- **Implicit chain check:** leave to detector implementation; risk of silent failure.

Note: this change touches `crates/detectors/src/detector.rs` (one method addition) and
`crates/server/src/streaming/worker.rs` (one filter guard). Neither is in `crates/common`.

### Decision 3: Unified vs per-chain streaming queue

**Recommended: unified queue (status quo), no changes in S17-2.**

Key trade-off: the existing `DetectorScheduler` already handles multi-chain jobs correctly
(keyed by `(Chain, Mint)`). Solana's higher event rate may marginally starve Ethereum jobs in
a shared queue. This is acceptable for 2-chain MVP. Per-chain queues add worker-pool complexity.

**User options:**
- **Unified queue (recommended, status quo):** no changes. Works correctly today.
- **Per-chain queues:** separate `async_channel` per chain, separate worker pool per chain.
  Better isolation; recommended only if Solana consistently starves Ethereum in production load
  testing.

### Decision 4: `PoolInitializeHook` — shared trait vs EVM-specific variant

**Recommended: shared `PoolInitializeHook` trait for Solana and Ethereum. D09/D10 add an
`if chain != Chain::Solana { return Ok(()); }` guard in Sprint 18 when EVM detectors land.
For S17-2, spurious EVM calls to D09/D10 are acceptable (BOCPD no-ops on unknown chains;
D10 liquid threshold is Solana-denominated — low false positive risk on Ethereum tokens).**

Key trade-off: shared trait is simpler and avoids Indexer double-dispatch. Chain-specific
variants offer cleaner isolation but require a second hook field in `Indexer`. Given that both
D09 and D10 are event-driven (not high-frequency), the shared-trait approach is correct for MVP.

**User options:**
- **Shared trait (recommended):** no changes to `PoolInitializeHook`, Indexer, or D09/D10 in
  S17-2. Add chain guards in Sprint 18.
- **Parallel `EvmPoolInitializeHook` trait:** adds a second `Option<Arc<dyn EvmPoolInitializeHook>>`
  field to `Indexer` (10th param becomes 11th). Cleaner semantic boundary; costs more refactor.

### Decision 5: `SubscribeFilter` for Ethereum

**Recommended: add `fn default_filter() -> SubscribeFilter` as a provided method on `ChainAdapter`.**

Currently `Indexer::run` calls `SubscribeFilter::solana_default()` unconditionally (line 191 of
`crates/indexer/src/lib.rs`). This is a 1-line bug that becomes visible the moment
`EthereumAdapter` is plumbed in — it will pass Solana program IDs as the Ethereum log filter,
producing zero EVM events.

The EVM equivalent filter specifies `contract_addresses` (Uniswap factory addresses, known ERC-20
tokens). These live in `config/adapters.toml` under `[ethereum]`. `SubscribeFilter::ethereum_default()`
is not meaningful in isolation (the list of interesting contracts is deployment-specific); it is
better as an adapter-level concern.

**User options:**
- **`ChainAdapter::default_filter()` provided method (recommended):** Solana impl returns
  `SubscribeFilter::solana_default()`; Ethereum impl returns an empty-addresses filter that the
  adapter populates from its config. Indexer calls `self.adapter.default_filter()`.
- **Accept `SubscribeFilter` in `Indexer::new` (10th param):** explicit, no trait change, but
  pushes filter construction to the call site (coordinator or server). Slightly more verbose.
- **`SubscribeFilter::chain_default(chain: Chain)` static constructor:** centralises default
  construction but introduces chain-specific logic into the `SubscribeFilter` type, which lives
  in `crates/chain-adapter`. Acceptable but less cohesive than the adapter owning its defaults.

---

## Reth ExEx (Sprint 17+ Feature Flag) — Informational

ADR 0004 commits to Reth as the EVM node. The ExEx variant (`cfg(feature = "exex")`) is deferred
to Sprint 17+ per SESSION-KICKOFF.md gotcha #59. This ADR makes no decisions about ExEx.

When ExEx lands, it affects only `EthereumAdapter::subscribe` — replacing the
`eth_subscribe("newHeads") + eth_getLogs` polling with `ExExNotification` push events. The
`MultiChainCoordinator`, `Indexer::run`, reorg handling, `PoolInitializeHook`, and the streaming
scheduler are all unchanged. ExEx is transparent to everything above the adapter layer. This is
a positive consequence of Pattern B: the coordinator does not see adapter internals.

---

## References

| # | Source | Decision grounded |
|---|--------|------------------|
| 1 | `crates/indexer/src/lib.rs:191` | Hardcoded `SubscribeFilter::solana_default()` — Decision 5 |
| 2 | `crates/indexer/src/lib.rs:437` | `chain = "solana"` hardcode in `IndexerBuilder::build` — Step 4 |
| 3 | `crates/indexer/src/config.rs` | `AdapterConfig::Solana` only — Step 1 |
| 4 | `crates/server/src/streaming/scheduler.rs:85` | `BTreeMap<(Chain, String), PendingJob>` — Q3 |
| 5 | `crates/server/src/streaming/registry.rs:45` | `BTreeMap<(Chain, Mint), StreamingState>` — Q3 |
| 6 | `crates/server/src/streaming/worker.rs:134` | `let chain_str = job.chain.to_string()` — Q2/Q3 |
| 7 | `crates/indexer/src/hooks.rs:59` | `on_new_token_launch(chain: Chain, ...)` — Q5 |
| 8 | `crates/detectors/src/lib.rs` | D01-D11 all Solana-scoped — Q2 |
| 9 | `docs/adr/0003-self-sovereign-infrastructure.md` | Single deployable unit (binding) |
| 10 | `docs/adr/0004-evm-node-choice-geth-vs-reth.md` | Reth ExEx future path (Sprint 17+) |
| 11 | SESSION-KICKOFF.md gotcha #1 | `crates/common` FROZEN |
| 12 | SESSION-KICKOFF.md gotcha #21 | STANDALONE SERVICE ONLY |
| 13 | SESSION-KICKOFF.md gotcha #39 | `GraphIndexerWriter` + `PoolInitializeHook` both optional |
| 14 | SESSION-KICKOFF.md gotcha #49 | Server-binary stub — orchestration must not leak into it |
| 15 | SESSION-KICKOFF.md gotcha #57 | EthereumAdapter complete-but-unplumbed |
| 16 | SESSION-KICKOFF.md gotcha #59 | Reth ExEx is Sprint 17+ feature flag, not S17-2 scope |
| 17 | CLAUDE.md §Detector Rules | False positives cheap, false negatives expensive — Q5 hook guard |
| 18 | CLAUDE.md §Multi-Chain Rules | Per-chain quirks documented in adapter crate |
