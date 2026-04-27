# ADR 0004 — EVM Node Choice: Geth vs Reth

**Status:** Proposed — awaits user sign-off.
**Date:** 2026-04-24
**Supersedes:** nothing (first EVM ADR).
**Inputs:** ADR 0003 (binding: no Alchemy/Infura/QuickNode/Pocket in prod), ADR 0001 §D2
(ingestion pattern via self-hosted streaming), `infra/solana-validator/README.md`
(structural template), CLAUDE.md §Ethereum/EVM, ROADMAP.md §Phase 4.

---

## Context

Sprint 15 opens Phase 4 ("additional chains") following user directive "Graph потом EVM."
The graph foundation is complete (D01-D11, 13 migrations, Sprint 14 closed). EVM expansion
is the natural next strategic move.

Phase 4 requires a self-hosted Ethereum node per ADR 0003. The fundamental constraint is:

> Zero 3rd-party SaaS in the production hot path. No Alchemy, Infura, QuickNode, Pocket,
> Moralis, or equivalent. Blockchain data is public and permissionless; paying a provider
> is paying for infrastructure, not data. Self-hosted is the only acceptable default.

The analogue to Solana's Yellowstone gRPC Geyser plugin — a structured streaming channel
with reorg-aware semantics — is the central differentiator between the two candidate node
implementations. Getting this choice right now avoids an expensive migration mid-Phase 4.

### What this ADR must deliver

A binding node software choice (Geth or Reth) plus enough implementation constraints to
let the Sprint 15 S15-2 runbook (`infra/ethereum-node/`) and S15-3 ChainAdapter skeleton
be written deterministically. The ADR does NOT draft the runbook or adapter code.

### Scope boundaries

- **In scope:** Ethereum mainnet L1 only.
- **Out of scope:** BSC, Base, Arbitrum, Optimism, Polygon, Tron. Each of those gets its
  own per-chain ADR when they land in Phase 4 or Phase 5. Base/Arbitrum reorg models
  (ZK proof latency, sequencer trust) require dedicated treatment.

---

## Decision (RECOMMENDED — user sign-off required)

**Run Reth as the production Ethereum node.**

Supplementary decision: **snapshot sync, archive mode disabled initially** (pruned state
with full transaction history). See §Sign-off Decisions for the two sub-options the user
must confirm.

Rationale for each dimension follows in §Trade-off Analysis. The short version:

1. Reth's Execution Extensions (ExEx) API is the structural equivalent of Yellowstone gRPC:
   a Rust-native, reorg-aware, type-safe streaming channel. Our entire ingestion architecture
   was designed around this pattern. Geth has no first-class equivalent.

2. Reth is a Rust project. The chain-adapter crate can embed `reth-primitives` (or just
   the JSON-RPC types) without a language boundary. Geth requires a separate JSON-RPC
   or IPC hop for every event.

3. Reth's parallel execution engine has materially faster snap sync and lower steady-state
   CPU than Geth on equivalent hardware — a real ops benefit for a self-hosted single node.

4. Both nodes expose identical standard JSON-RPC and `eth_subscribe` WebSocket surfaces,
   so fallback from ExEx to standard subscription is available on either node. The ExEx
   advantage is additive, not a risk.

---

## Trade-off Analysis

### 1. Streaming event channel (ExEx vs alternatives)

The single most important dimension. Our ingestion model on Solana:

```
Validator (agave) → Yellowstone gRPC plugin → chain-adapter subscribe() stream → indexer
```

The EVM equivalent must provide:
- Per-block ordered stream of: transactions, receipts/logs, state diffs
- Explicit reorg notification with the canonical new block reference
- No polling: push-based delivery from the node to the adapter
- Rust-native or JSON-RPC accessible

#### Reth ExEx (Execution Extensions)

Reth ExEx is an in-process Rust extension point. An ExEx runs in the same process as the
node, receives a `tokio::sync::broadcast` channel of `ExExNotification` messages, and can
react to every committed execution outcome including reorgs.

The `ExExNotification` enum (as of Reth v1.x / mid-2025 stable):
- `ChainCommitted { new: Arc<Chain> }` — canonical chain advanced; contains the new blocks
  with full transaction/receipt data.
- `ChainReverted { old: Arc<Chain> }` — blocks reverted (reorg); contains the reverted
  blocks that must be un-emitted.
- `ChainUpdated { old: Arc<Chain>, new: Arc<Chain> }` — reorg with replacement; contains
  both the reverted and the new canonical chain tip simultaneously.

This maps directly onto our existing `Event` enum:
- `ChainCommitted` blocks yield `Transfer`, `Swap`, `PoolEvent`, `TokenMeta` events.
- `ChainReverted` blocks yield `ReorgMarker { block_number }` — exact analogue to
  Solana's `ReorgMarker { slot }`.
- `SlotFinalized` maps to Ethereum's finality checkpoint signals, which Reth also exposes.

The ExEx runs in-process. There is no serialisation boundary, no socket hop, no IPC pipe.
The data types are Reth's own `reth-primitives` (alloy-based types as of Reth 1.x).

**References:**
- Reth ExEx documentation: https://reth.rs/exex/exex.html
- ExEx API design: https://github.com/paradigmxyz/reth/tree/main/crates/exex
- ExEx notification types: `reth_exex::ExExNotification` in
  https://github.com/paradigmxyz/reth/blob/main/crates/exex/exex/src/notifications.rs

**Production readiness (as of Q1-2026):** Reth ExEx was stabilised in Reth v0.2.x (late
2024) and is used in production by several MEV infrastructure providers. The API surface
has remained stable through v1.x. The Reth team explicitly supports ExEx as a first-class
integration path. No known regressions.

#### Geth alternatives

Geth offers no ExEx equivalent. The practical options for streaming from a Geth node:

1. **`eth_subscribe("newHeads")` + `eth_getLogs` polling per block.** Works but is
   pull-based: every new head triggers a `eth_getLogs` call with address/topic filters.
   Introduces one RPC round-trip per block (~12s on mainnet). Sufficient for log-based
   detection but misses internal/trace calls and state diffs entirely.

2. **`debug_traceBlock` / `trace_block` (Geth with `--gcmode=archive` or `--tracer`).**
   Gives full trace including internal calls, but requires an archive or trace-enabled
   node (substantially higher disk footprint — see §3 below), and is pull-based not
   push-based. Each call is expensive; cannot be used on a pruned node.

3. **Webhooks / filter polling via `eth_newFilter`.** Deprecated pattern. Does not
   provide push semantics; reconnection semantics are fragile.

4. **Erigon Otterscan / erigon streaming.** Erigon (the third major EVM client) has its
   own streaming interface but is a third client to evaluate, not a Geth variant. Out of
   scope for this ADR.

**Conclusion on streaming:** Reth ExEx is categorically better than any Geth streaming
option for our use case. It provides the same push-based, reorg-aware, in-process model
that Yellowstone gRPC provides for Solana.

### 2. Sync speed

**Snap sync (both clients):** Both Geth and Reth support snap sync — downloading a recent
state snapshot and replaying transactions from there, not from block 1.

**Reth parallel execution advantage:** Reth's execution pipeline runs transaction execution
and state trie updates in parallel using its staged sync + parallel state root computation.
On equivalent hardware (32-core), Reth snap syncs mainnet in approximately **4-8 hours**
to a pruned node as of early 2026. Geth snap sync on the same hardware typically takes
**8-16 hours**.

These are community-reported ranges; actual times depend on network bandwidth and NVMe
performance. The 2x factor is consistent across multiple community benchmarks (see
Paradigm's blog on Reth performance and community GitHub discussions).

**Archive sync difference:** Archive sync (--full or --state.scheme=path in Geth) is
much slower for both clients — days, not hours. Reth again has the advantage due to
parallel execution, but the gap is less relevant to our initial deployment choice (we
recommend pruned-first per §Sign-off Decision 3).

**References:**
- Paradigm Reth v1.0 announcement benchmarks: https://www.paradigm.xyz/2024/09/reth-v1
- Community sync benchmarks (EthStaker): https://ethstaker.cc/

### 3. Disk footprint (as of Q1-2026)

Ethereum mainnet disk consumption grows ~60-80 GB/month on a pruned node (transactions
retained, ancient state pruned). State-only snapshot sync starts at ~800 GB - 1 TB.

| Mode | Approximate disk (2026) | Notes |
|---|---|---|
| Geth pruned (snap sync, default) | ~900 GB - 1.2 TB | `--syncmode snap`, ancient state pruned |
| Reth full (snap sync, state pruned) | ~900 GB - 1.2 TB | `--full` flag off; comparable to Geth default |
| Geth archive (`--gcmode=archive`) | ~16 - 18 TB | All historical state; required for `debug_traceBlock` |
| Reth archive (full state history) | ~13 - 15 TB | Reth's columnar state storage is somewhat more compact |
| Geth trace-enabled (archive + traces) | ~22+ TB | Requires separate trace storage |

**Our use case:** Live streaming detection does NOT require full historical state. Pruned
mode retains the complete transaction and log history (required for `eth_getLogs` backfill)
while discarding ancient state snapshots. This is sufficient for all detectors in Phase 2
(D01-D11 all read event/log data, not raw state diffs at arbitrary historical blocks).

If a future detector requires historical state (e.g., tracing an arbitrary historical
transaction to reconstruct pool reserves at block N), archive mode can be added on a
second node without touching the streaming node.

**Recommendation: pruned node, ~1-1.5 TB NVMe budget, growing ~75 GB/month.**

### 4. JSON-RPC and WebSocket compatibility

Both Geth and Reth expose a fully standards-compliant Ethereum JSON-RPC surface:
- `eth_getBlockByNumber`, `eth_getLogs`, `eth_getTransactionReceipt` (backfill primitives)
- `eth_subscribe("newHeads")`, `eth_subscribe("logs")` (WebSocket baseline)
- `eth_call` (honeypot simulation — D01 EVM equivalent)
- `debug_*` namespace on Reth requires enabling separately; same as Geth

There are no known RPC surface gaps between Reth v1.x and Geth v1.14.x that affect our
detector use cases. Either client can serve our fallback `eth_subscribe` + `eth_getLogs`
path.

**References:**
- Reth RPC compatibility matrix: https://reth.rs/jsonrpc/intro.html
- JSON-RPC spec: https://ethereum.github.io/execution-apis/api-documentation/

### 5. Reorg handling

**Ethereum finality context:** Ethereum post-Merge uses LMD-GHOST + Casper FFG consensus.
A checkpoint becomes "justified" after the first epoch vote and "finalized" after two
consecutive justified checkpoints. Finality is achieved approximately every 64 slots
(12.8 minutes at 12s/slot). Reorgs deeper than 1-2 blocks are extremely rare on Ethereum
mainnet post-Merge; reorgs deeper than the finality horizon (64 slots) are theoretically
impossible unless >1/3 of stake is malicious.

**Practical reorg policy:**
- Hot path: process at the `latest` head (depth 0). Emit events immediately. Accept that
  1-2 block reorgs may require retraction.
- Confirmation threshold: depth 12 blocks (~2.4 minutes) before treating events as
  durable. Analogous to Solana's `confirmed` commitment.
- Immutable records: wait for finality signal (64+ slots, or explicit finalized block tag).
  Analogous to Solana's `finalized` commitment.

**Geth reorg handling:** Geth emits `eth_subscribe("newHeads")` on every head change,
including after a reorg. The consumer must detect reorgs by tracking block hashes: if a
new head has a parent hash that does not match the previous head, a reorg occurred.
Geth does NOT emit an explicit "reorg notification" — the consumer must reconstruct it
by comparing hashes. This is manageable but requires explicit state tracking in the
chain-adapter.

**Reth ExEx reorg handling:** `ChainReverted` and `ChainUpdated` are explicit notification
types. The reorg is delivered with full information about which blocks are being removed
and which are replacing them. No hash-tracking state machine needed in the adapter. The
chain-adapter can emit `ReorgMarker { block_number }` directly from the notification.

**Advantage: Reth.** The ExEx reorg signal directly maps to our existing `Event::ReorgMarker`
and `Event::SlotFinalized` contract. The Geth path requires custom hash-tracking state.

### 6. Language and ecosystem alignment

Geth is written in Go. It is a separate process communicating over JSON-RPC or IPC.
The adapter must serialise/deserialise every event through JSON. There is no type-safe
interface available at the Rust level without going through the wire format.

Reth is written in Rust. The Reth crate ecosystem publishes:
- `reth-primitives`: core types (`Block`, `Transaction`, `Receipt`, `Log`)
- `reth-exex`: ExEx runtime and notification types
- `alloy-primitives`: address, hash, U256 types (shared with the broader EVM Rust ecosystem)
- `alloy-rpc-types`: JSON-RPC response types (also used for the fallback eth_subscribe path)

The chain-adapter can depend on `alloy-primitives` for address normalisation (EIP-55
checksum), `U256` arithmetic (token amounts), and log decoding — all without running a
node. This is the exact same value that `solana-sdk` types provide in
`crates/chain-adapter/src/solana/decode.rs`: chain-native types at the Rust level,
converted to `common/` types at the module boundary.

**Advantage: Reth.** Language and type system alignment with the existing Rust workspace.

### 7. Maintenance, community, release cadence

**Geth:**
- Client since 2015. Most battle-tested EVM client.
- Maintained by the Ethereum Foundation's Go team.
- 10+ years of production history. The canonical reference implementation.
- Release cadence: roughly monthly for patches, every few months for minor versions.
- Dominant production deployment share among EVM clients.

**Reth:**
- First production release: v0.1.0 in late 2023; v1.0.0 September 2024.
- Maintained by Paradigm (well-funded, EVM-focused research firm).
- Growing production adoption: used by Flashbots, several MEV searchers, and infrastructure
  providers as of 2025-2026.
- Release cadence: active development, roughly bi-weekly releases.
- Community growing rapidly; GitHub activity (paradigmxyz/reth) is high.

**Risk: Reth has fewer production-years.** Geth has a longer track record. Reth hit its
v1.0 production milestone in September 2024, meaning it had approximately 18 months of
production history as of this ADR (April 2026). For a non-critical analytics service (not
a production validator, not a financial settlement node), this risk is acceptable.

The downside of choosing Geth here is architectural: we permanently lose the ExEx
integration path and commit to the polling/hash-tracking approach described in §1.

**Mitigating factors for Reth immaturity:**
- We are an analytics consumer, not a consensus participant. A Reth bug that causes a
  short gap in event streaming is recoverable via backfill. A Reth crash is recoverable
  via restart. Neither outcome causes financial loss.
- The fallback path (`eth_subscribe` + `eth_getLogs`) works on either node and is our
  bootstrap path regardless. If Reth ExEx has a regression, we fall back to the polling
  path automatically — no data loss, only higher latency.
- Reth's codebase is well-tested; the Paradigm team runs it in production for their own
  trading infrastructure, giving strong incentive to maintain quality.

**References:**
- Reth v1.0 release: https://www.paradigm.xyz/2024/09/reth-v1
- Reth GitHub: https://github.com/paradigmxyz/reth
- Geth GitHub: https://github.com/ethereum/go-ethereum

### 8. Embedded library mode (future option)

Reth supports an embedded library mode (`reth-node-builder`) for running a full node
in-process within a Rust binary. This is ExEx's natural habitat: the node and the
extension run in a single process.

Concretely: the `crates/server` binary could optionally embed a Reth node directly,
eliminating the out-of-process RPC hop entirely. This is not recommended for Phase 4
MVP (operational complexity), but it is an architectural option that does not exist with
Geth.

---

## Alternatives Considered

### Alternative A: Geth (go-ethereum)

**Rejected.** The absence of a first-class streaming event channel equivalent to Yellowstone
gRPC or ExEx is the disqualifying factor. Every integration path for Geth relies on the
consumer polling for new events or implementing custom hash-tracking state machines.

Geth's 10+ year track record is a genuine advantage, but it buys reliability for a
consensus participant, not for an analytics consumer. The architectural debt of building
the indexer around polling semantics — and the absence of clean reorg notifications —
outweighs Geth's stability advantage for our use case.

If the Reth ExEx API becomes unstable or is deprecated (unlikely given Paradigm's
investment), the fallback path to `eth_subscribe` polling works identically on Geth,
meaning a node swap would require no adapter changes beyond the ExEx code path.

### Alternative B: Erigon / Otterscan

**Deferred.** Erigon is a third major EVM client with its own streaming interface and
excellent archive mode support. It offers `eth_getLogs` streaming via its own RPC
extensions and has a strong track record in archive use cases.

Erigon is not rejected, but it is deferred: evaluating a third client in addition to
Geth and Reth would expand the ADR scope unnecessarily. If Reth's ExEx proves problematic,
Erigon is the natural next candidate — particularly if archive mode becomes required for
future detectors.

### Alternative C: Both nodes (Reth for streaming + Geth/archive for backfill)

**Not recommended for MVP.** Running two separate nodes doubles the infra footprint.
The use case that would justify this — needing archived state traces for retrospective
detection while maintaining a separate streaming node — is not a Phase 4 requirement.

The escape hatch: Reth can be switched to archive mode on the same instance by
re-syncing with `--full` enabled (preserves all historical state). A second node can
be added later if historical trace queries become necessary.

### Alternative D: eth_subscribe polling only (no ExEx)

**This is a valid fallback, not an architectural choice.** `eth_subscribe("newHeads")` +
`eth_getLogs` works on any EVM node and is our bootstrap path in Phase 4 Sprint 15 S15-3
before ExEx integration is complete. The chain-adapter skeleton will implement this path
first, then add ExEx as the optimised streaming path.

Choosing this as the permanent architecture — even on top of Reth — would mean discarding
the ExEx integration and accepting polling latency and the hash-tracking reorg state
machine. Not recommended given the ExEx investment is modest (~1 sprint once the skeleton
is in place).

---

## Implementation Notes

These constraints bind Sprint 15 S15-2 (runbook) and S15-3 (adapter skeleton).

### S15-2: `infra/ethereum-node/` runbook

The runbook mirrors the structure of `infra/solana-validator/README.md`:
- Hardware BOM section (see §Hardware below)
- Pinned versions: Reth release tag + Docker image digest if using Docker
- OS preparation + sysctl tuning (fewer Ethereum-specific tunables than Solana)
- Snapshot sync procedure (Reth snap sync; document `reth init` + `reth node` flags)
- systemd unit (or docker-compose — sign-off decision #5 below)
- Health checks: `eth_blockNumber` round-trip + ExEx subscription test
- Monitoring: Reth's built-in Prometheus metrics on port 9001

**Pinned versions to verify before writing the runbook:**
- Current Reth stable: check https://github.com/paradigmxyz/reth/releases — use the
  latest non-rc tag in the v1.x series.
- Alloy crates version: must align with the Reth version (alloy is Reth's dependency,
  not independently versioned for our use).
- Rust toolchain: Reth pins a `rust-toolchain.toml`; the ExEx crate must use the same
  version.

### S15-3: `crates/chain-adapter/src/ethereum/` skeleton

**Phase 1 of the adapter (bootstrap path — no ExEx):**

```
crates/chain-adapter/src/ethereum/
  mod.rs        — EthereumAdapter struct; ChainAdapter impl
  config.rs     — EthereumAdapterConfig (analogous to SolanaAdapterConfig)
  subscribe.rs  — eth_subscribe("newHeads") + eth_getLogs polling stream
  backfill.rs   — eth_getLogs range queries (analogous to solana/backfill.rs)
  decode.rs     — ERC-20 Transfer log decode, Uniswap v2/v3 Swap/Mint/Burn decode
  checkpoint.rs — reuse solana/checkpoint.rs pattern (file + in-memory stores)
```

**Phase 2 of the adapter (ExEx path — follow-up sprint):**

```
crates/chain-adapter/src/ethereum/
  exex.rs       — Reth ExEx notification handler; maps ChainCommitted/Reverted to Event stream
```

The `ChainAdapter` trait requires no changes. The `Event` enum already has `ReorgMarker`
and `SlotFinalized`; renaming to `BlockNumber`-keyed variants was noted as a Phase 4
concern — the `slot` field in `ReorgMarker { slot: u64 }` doubles as `block_number` for
EVM without a breaking change.

**Key constraints from CLAUDE.md:**
- `Address`: normalise to EIP-55 checksum at decode boundary. Never lowercase hex.
- `Transfer` amounts: use `U256` or `u128` for raw token units. Never `f64`.
- ERC-20 decimals: read `decimals()` via `eth_call` at token-registry time; never
  hardcode 18.
- `Transfer` from field: for ERC-4337 / meta-transactions, walk inner logs, not just
  the top-level `from` field.
- Uniswap v3 `Swap` event signature differs from v2 — topic0 is different; both must be
  handled in `decode.rs`.

**`SubscribeFilter` for EVM** (extends the existing struct in `lib.rs`):
- `program_ids` → `contract_addresses: Vec<Address>` (the EVM analogue)
- Token-list bootstrap: read from `crates/token-registry/data/ethereum_tokens.json`
  (to be created in Phase 4; populated from Uniswap token list snapshot)
- Uniswap factory addresses are static config, not hardcoded — live in
  `config/adapters.toml` under `[ethereum]`

### Hardware sizing for `infra/ethereum-node/`

Ethereum mainnet node (Reth, pruned, streaming):

| Resource | Minimum | Recommended |
|---|---|---|
| CPU | 4 cores | 8+ cores (Reth parallel execution benefits from more) |
| RAM | 16 GB | 32 GB (state cache; more = faster block processing) |
| Disk | 1.5 TB NVMe | 2 TB NVMe (growth: ~75 GB/month on pruned) |
| Network | 100 Mbps | 1 Gbps (snap sync requires sustained bandwidth) |

This is dramatically smaller than the Solana validator footprint (no 512 GB RAM
requirement, no accounts-db write torrent). A mid-range dedicated server or a cloud
instance (8 vCPU, 32 GB RAM, 2 TB NVMe) is sufficient.

**Cost estimate:** $100-250/mo bare-metal or dedicated cloud (e.g. Hetzner AX41-NVMe
at ~€60/mo, OVHcloud Advance-1 at ~$100/mo). An order of magnitude cheaper than the
Solana validator.

### Finality and commitment policy

Per CLAUDE.md §Ethereum/EVM: "wait 12 confirmations for finality; deeper for L2s."

Formalised for the EVM adapter:

| Tier | Block depth | Analogue | Use |
|---|---|---|---|
| Unconfirmed | 0 (latest head) | Solana `processed` | Never used in detectors |
| Safe | 12 blocks (~2.4 min) | Solana `confirmed` | Hot path event streaming |
| Finalized | 64+ blocks (~12.8 min) or `finalized` tag | Solana `finalized` | Checkpoint saves, durable storage writes |

The Ethereum execution client exposes a `finalized` block tag (available since The Merge)
that returns the last finalized checkpoint block. The adapter should use `safe` for the
hot path and `finalized` for checkpoint and immutable storage writes.

This maps directly to the existing `CommitmentConfig` enum pattern in
`crates/chain-adapter/src/solana/config.rs` — an `EvmCommitmentConfig` enum with
`Latest`, `Safe`, and `Finalized` variants.

### Backfill concurrency note

Per the existing `ChainAdapter` trait contract:

> Backfill and subscribe MUST NOT race on the same slot. The indexer coordinates this
> by running backfill first, then starting subscribe from the first slot after backfill ends.

For EVM: backfill MUST NOT overlap with the live subscribe stream on the same block range.
This invariant is already encoded in the trait doc; the EVM adapter inherits it.

`eth_getLogs` has a per-request block range limit on many nodes (typically 2000-10000
blocks per call). The backfill implementation must page through ranges using configurable
`batch_size_blocks` (default: 1000 blocks) in `EthereumAdapterConfig`.

### Mempool (Phase 4 stretch goal)

CLAUDE.md: "Mempool via `eth_subscribe("newPendingTransactions")` or dedicated providers
(Flashbots, bloXroute, Blocknative)."

ADR 0003 constraint: no dedicated providers in production hot path. Self-hosted mempool
access means `eth_subscribe("newPendingTransactions")` on our own Reth node.

Reth's txpool is accessible via the standard `txpool_content` / `eth_subscribe` interface.
Flashbots and bloXroute are excluded by ADR 0003.

Mempool access is NOT a Phase 4 MVP requirement. The sandwich/MEV detector (mentioned in
CLAUDE.md and ROADMAP.md Phase 2 deferred list) is a Phase 4 stretch goal. It is noted
here to confirm that self-hosted mempool access is available on Reth without additional
infrastructure.

---

## Consequences

### Positive

1. **ExEx streaming model.** Reorg-aware, push-based, in-process streaming — the exact
   model that proved robust on Solana via Yellowstone. The adapter code structure and
   the `Event` type mapping are direct analogues.

2. **Rust ecosystem alignment.** `alloy-primitives` for address/hash/U256, `reth-exex`
   for streaming, `alloy-rpc-types` for fallback RPC — all first-class Rust crates with
   no FFI or JSON boundary at the type level.

3. **Lower hardware cost.** Pruned Reth node requires ~1.5-2 TB NVMe vs 4-6 TB for
   archive. The EVM node is an order of magnitude cheaper to operate than the Solana
   validator.

4. **Faster sync.** Reth snap sync is approximately 2x faster than Geth snap sync on
   equivalent hardware, reducing time-to-first-event in Phase 4.

5. **Clean reorg semantics.** `ChainReverted` ExEx notification maps directly to
   `Event::ReorgMarker`. No hash-tracking state machine required in the adapter.

### Negative

1. **Reth immaturity relative to Geth.** ~18 months of production history vs 10+ years
   for Geth. Mitigated by: analytics consumer (not consensus participant), automatic
   fallback to `eth_subscribe` polling on ExEx failure, and Paradigm's strong production
   usage incentive.

2. **ExEx integration adds adapter complexity.** The Phase 2 ExEx path (`exex.rs`)
   requires embedding a Reth crate dependency into `crates/chain-adapter`. This adds to
   Cargo.toml workspace complexity and pins the adapter to Reth's release cadence.
   Mitigated by: ExEx code lives in a separate file from the fallback path; the fallback
   (`subscribe.rs`) works on any EVM node and is implemented first.

3. **Version pinning coupling.** The ExEx crate (`reth-exex`) must be pinned to a
   specific Reth release, and the running node must match. This is the same version-
   coupling constraint as Yellowstone gRPC's `+solana.X.Y.Z` suffix — a solved pattern
   in our project.

4. **No archive state by default.** Detectors requiring historical state at arbitrary
   block heights (e.g., "what was the pool reserve at block 18,000,000?") cannot be
   satisfied by the pruned node. A second archive node would be required. Phase 4
   detectors (EVM honeypot simulation via `eth_call`, EVM LP drain, EVM pump-and-dump)
   all operate on event logs, not raw state — this limitation does not block Phase 4 MVP.

### Neutral

- The fallback path (`eth_subscribe` + `eth_getLogs`) is implemented first and works on
  any EVM-compatible node. This means the Phase 4 chain-adapter skeleton compiles and
  produces events regardless of whether ExEx is wired up. ExEx is an optimisation on top
  of an already-working baseline.

- L2 chains (Base, Arbitrum) use op-geth and Nitro respectively — neither is Reth.
  When L2 ADRs are written, they will either adopt the `eth_subscribe` fallback path
  (which works on any EVM node) or specify their own streaming mechanism. The Reth
  choice for L1 Ethereum does not constrain L2 adapter choices.

---

## Sign-off Decisions

The following five decisions require explicit user confirmation before S15-2 and S15-3
begin. Each presents a recommended default and the key trade-off.

### Decision 1: Node software — Reth vs Geth

**Recommended: Reth**

Trade-off: Reth's ExEx streaming API is architecturally aligned with our Yellowstone
pattern and provides clean reorg semantics. Geth is more battle-tested but requires
polling-based integration with a custom reorg state machine. For an analytics consumer
(not a consensus participant), Reth's 18-month production record is acceptable.

**User must decide:** Reth or Geth. If Geth, the ExEx path in `exex.rs` is dropped and
the fallback `eth_subscribe` polling path becomes permanent.

### Decision 2: Sync strategy — snapshot sync vs full sync from genesis

**Recommended: Snapshot sync (snap sync)**

Trade-off: Snap sync reaches the current chain tip in 4-8 hours on Reth (vs 8-16 hours
for Geth snap, vs days for full sync from genesis). Full sync from genesis gives cryptographic
verification of every block but is operationally impractical for a first deployment.
Full sync can always be performed later on a second node if trust in the snapshot is a
concern.

**User must decide:** Snap sync (default, fast, normal for analytics) or full sync
(slow, maximum verifiability, not required for analytics).

### Decision 3: Archive vs pruned node

**Recommended: Pruned (full transaction + log history, ancient state pruned)**

Trade-off: Pruned node costs ~1.5-2 TB NVMe now, growing ~75 GB/month. All Phase 4 MVP
detectors operate on event logs (`eth_getLogs`) and do not require historical state at
arbitrary block heights. Archive mode costs ~13-15 TB now and requires a larger machine.

If a future detector needs historical state traces (e.g., reconstructing Uniswap pool
reserves at a specific historical block), a second archive node can be added without
touching the streaming node.

**User must decide:** Pruned (recommended) or archive. Note: choosing archive now increases
hardware cost significantly and delays sync time to days.

### Decision 4: Finality depth for the hot path

**Recommended: depth 12 for "safe" (hot path), `finalized` block tag for durable writes**

Trade-off: Depth-12 (~2.4 minutes, 12 blocks) is the CLAUDE.md-specified confirmation
threshold for EVM. Using the Ethereum `safe` block tag (updated by the CL after 2 epochs,
~384 seconds) is more conservative but adds ~6 minutes of latency. The `finalized` tag
(~12.8 minutes) is used for checkpoint saves only.

A depth-12 reorg on post-Merge Ethereum mainnet is effectively impossible (LMD-GHOST
settles within 1-2 slots under normal conditions). The depth-12 policy is already
encoded in CLAUDE.md and does not need changing.

**User must decide:** Accept depth-12 for hot path as specified in CLAUDE.md, or increase
to `safe` block tag (~384 seconds). Recommendation is depth-12; `safe` is unnecessarily
conservative for a shitcoin analytics service.

### Decision 5: Deployment mode — Docker vs systemd

**Recommended: Docker (docker-compose) for Phase 4 MVP**

Trade-off: The Solana validator runbook uses systemd because the Agave validator binary
is not distributed as a Docker image and has complex startup flag requirements. Reth
provides official Docker images (`ghcr.io/paradigmxyz/reth`) with pinned digest tags.
Docker simplifies the Phase 4 bootstrap significantly (no Rust toolchain required on
the host, no manual binary install). The trade-off is Docker overhead (minimal for a
single-process node) and a dependency on the Reth Docker image registry.

A systemd path remains valid if the user prefers bare-metal consistency with the Solana
runbook. Docker is recommended for Phase 4 MVP to reduce runbook complexity.

**User must decide:** Docker (docker-compose, official Reth image) or systemd (binary
install, mirrors Solana runbook structure).

---

## References

All technical claims in this ADR are grounded against the following sources.

| # | Source | Claim grounded |
|---|---|---|
| 1 | https://reth.rs/exex/exex.html | ExEx API design, notification types, production-readiness |
| 2 | https://github.com/paradigmxyz/reth/blob/main/crates/exex/exex/src/notifications.rs | `ExExNotification` enum variants: `ChainCommitted`, `ChainReverted`, `ChainUpdated` |
| 3 | https://www.paradigm.xyz/2024/09/reth-v1 | Reth v1.0 production release (September 2024); sync benchmarks; Paradigm production usage |
| 4 | https://github.com/paradigmxyz/reth | Reth GitHub — release cadence, community size, ExEx crate status |
| 5 | https://github.com/ethereum/go-ethereum | Geth GitHub — stable reference implementation |
| 6 | https://reth.rs/jsonrpc/intro.html | Reth JSON-RPC compatibility matrix — no gaps vs Geth for our use cases |
| 7 | https://ethereum.github.io/execution-apis/api-documentation/ | Ethereum JSON-RPC spec — `eth_subscribe`, `eth_getLogs`, `eth_call` |
| 8 | docs/adr/0003-self-sovereign-infrastructure.md | Binding: no Alchemy/Infura/QuickNode in prod hot path |
| 9 | docs/adr/0001-phase0-synthesis.md §D2 | Yellowstone gRPC pattern: push-based, reorg-aware, provider-agnostic |
| 10 | CLAUDE.md §Ethereum/EVM | 12-confirmation policy; ERC-20 decimal handling; proxy follow-the-money; Uniswap v2/v3 event shapes |
| 11 | ROADMAP.md §Phase 4 | Phase 4 chain candidates and self-sovereign constraint |
| 12 | infra/solana-validator/README.md | Structural template for `infra/ethereum-node/` runbook |
| 13 | https://github.com/alloy-rs/alloy | alloy-primitives: Address (EIP-55), U256, B256 — Rust EVM types |
| 14 | https://ethstaker.cc/ | Community Reth vs Geth sync benchmark ranges |
| 15 | https://ethereum.org/en/developers/docs/consensus-mechanisms/pos/ | Ethereum finality: LMD-GHOST + Casper FFG; 64-slot finality window |
