# Design 0028 — Lightweight Query Engine Deployment (Sprint 26)

**Date:** 2026-04-27
**Status:** Proposed — awaits user sign-off on §11 decisions.
**Author:** architect agent
**Sprint:** 26 (replaces SUPERSEDED `docs/designs/0027-production-readiness-gate.md`)
**ADR refs:**
- ADR 0001 §D2 — wire-protocol portion only (out-of-process processes talking standard wire
  format). The streaming-pipeline operational model bundled with §D2 is superseded by ADR 0007.
- ADR 0003 — self-sovereign infrastructure; no 3rd-party SaaS in hot path.
- ADR 0006 (post-amendment) — code-level self-sovereignty; kludge test; wire-protocol-only
  integration; vendor SDK ban.
- ADR 0007 — Pull-Based Query Engine operational model. This design is the Sprint 26
  implementation plan for ADR 0007.

**Supersedes:** `docs/designs/0027-production-readiness-gate.md` — the production-readiness
plan was written under the continuous-streaming pipeline assumption. That assumption is
superseded by ADR 0007. Some deliverables from 0027 are preserved here (OTLP wire-up,
Prometheus, health endpoints, testcontainers integration test, docker-compose, PRODUCTION.md
runbook) because they are independent of the ingestion model and remain valid Sprint 26 goals.
The parts that assumed a Yellowstone-based deployment topology, a full-validator Solana node,
and a bulk-event storage schema are replaced.

---

## §1 Status / Date / Author / Sprint / ADR Refs

See header above. Implementation begins after the user has confirmed or redirected each of
the ten sign-off items in §11. Items §11.1 and §11.4 carry structural implications for task
T26-1 and T26-5 respectively; those tasks must not begin before sign-off is received.

---

## §2 Goals and Non-Goals

### §2.1 Goals

Sprint 26 has two intertwined themes: structural migration (pull the ingestion layer into
alignment with the query engine model) and production readiness (deliver a deployable,
observable, operator-runbooked service). Both must close in the same sprint because the
deployment topology depends on the ingestion model.

The seven concrete goals:

1. **Rewrite Solana ingestion to standard JSON-RPC + WebSocket.** Replace the
   Yellowstone gRPC path in `crates/chain-adapter/src/solana/` with standard Solana
   JSON-RPC methods and WebSocket subscriptions, mirroring `crates/chain-adapter/src/ethereum/`.
   Delete `crates/yellowstone-proto/`.

2. **Mode-shift `crates/indexer/` to on-demand query engine.** The continuous-stream
   consumer becomes a worker pool that evaluates specific tokens in response to explicit
   triggers (REST requests, watchlist updates, periodic scan ticks).

3. **Storage schema additions.** Add `verdict_cache` table and adjust retention policies.
   Emit migration V00017.

4. **Gateway additions.** Add `/score?token=X` REST endpoint (synchronous single-token
   evaluation) and watchlist WS subscription push (verdicts pushed as they complete).

5. **OTLP + Prometheus + health endpoints.** Preserved from design 0027 §5.1–§5.3.
   The OTLP `TODO(sprint-20)` stub in `crates/server/src/init/tracing_init.rs` is resolved.
   `StreamingMetrics` are merged into the `/metrics` response. Health endpoint gains
   chain-adapter liveness fields.

6. **Testcontainers Postgres integration test.** A `production_smoke_test.rs` that starts
   a Postgres container, runs migrations, triggers a detector evaluation, persists an
   `AnomalyEvent`, and asserts both the `verdict_cache` row and the REST `/score` response.

7. **`infra/docker-compose.prod.yml` + `infra/PRODUCTION.md` runbook.** The compose file
   and operator playbook reflect the lightweight query engine topology, not the Yellowstone
   validator topology from design 0027.

### §2.2 Non-Goals

The following are explicitly out of scope for Sprint 26 and must not appear in any
implementation brief dispatched under this design:

- New detectors (D14+): Token-2022 extensions (ConfidentialTransfer, NonTransferable,
  ScaledUiAmount, Pausable), Pump.fun graduation enrichment, D13 pool coverage extension
  (Curve/Balancer/SushiSwap). All remain on the carry-forward list.
- Consumer-side integration code. The boundary is firm per `memory/feedback_standalone_service_only.md`:
  we ship an API and SDK; consumers adopt on their own timeline. No writes to `bot-trader-2-0`,
  `mg-custody`, or any sibling repository.
- Stage 2 FDR calibration. Corpus-blocked; minimum 30 days of live data required.
- Additional EVM chains (Base, BSC, Arbitrum, Polygon). Phase 4 scope.
- ClickHouse integration. The current storage tier is Postgres-only; ClickHouse remains on
  the Phase 3 roadmap.
- SPL Token account layout decoders in `crates/solana-types/`. Deferred per design 0026
  §11.6; not needed for Sprint 26 scope.
- Decimals exact-fetch (D11/D12/D13 SPEC-NOTEs from Sprint 21). Sprint 26 carry-forward.
- D13 mempool integration (pre-execution MEV detection). Sprint 27+.
- Multi-tenant quota enforcement. Rate-limit infrastructure exists; per-consumer enforcement
  is Sprint 27+.
- `eth_unsubscribe` on Receiver drop + mid-stream WS reconnect (Sprint 17 TODOs). Still
  deferred.

---

## §3 Architectural Overview

The production deployment after Sprint 26 is a lightweight docker-compose stack with three
mandatory services (chain node(s), Postgres, onchain-service) and one optional observability
sidecar. The dominant cost reduction relative to design 0027 is the Solana node hardware:
a standard RPC node rather than a full Yellowstone-enabled validator.

```
┌─────────────────────────────────────── host machine ────────────────────────────────────────┐
│                                                                                              │
│  ┌────────────────────────┐    ┌────────────────────────────┐                               │
│  │  solana-rpc-node        │    │  ethereum-node              │                              │
│  │  (Agave, RPC-only mode) │    │  (Reth + Lighthouse)        │                              │
│  │  ~64-128 GB RAM         │    │  ~32 GB RAM / 2 TB NVMe     │                              │
│  │  json-rpc  :8899        │    │  ws      :8546              │                              │
│  │  ws        :8900        │    │  rpc     :8545              │                              │
│  └──────────────┬──────────┘    └──────────────┬─────────────┘                              │
│                 │                              │                                             │
│                 │  Solana JSON-RPC 2.0 / WS    │  Ethereum JSON-RPC 2.0 / WS                │
│                 │  (standard protocol)          │  (standard protocol)                       │
│                 └──────────────┬───────────────┘                                             │
│                                │  (internal Docker network)                                 │
│  ┌─────────────────────────────▼──────────────────────────────────────────────────────┐     │
│  │                         onchain-service                                            │     │
│  │  crates/server — single binary                                                     │     │
│  │                                                                                    │     │
│  │  REST  :8080  (GET /health, GET /metrics, /v1/score, /v1/anomaly_events, ...)      │     │
│  │  WS    :8080  (GET /v1/ws/stream — AnomalyEvent push, watchlist verdicts)          │     │
│  │                                                                                    │     │
│  │  [tracing] ──OTLP gRPC──► otel-collector :4317  (optional sidecar)               │     │
│  └─────────────────────────────┬──────────────────────────────────────────────────────┘     │
│                                │  pgwire / sqlx                                             │
│  ┌─────────────────────────────▼────────────────────────────────────────────────────┐       │
│  │                       postgres-16                                                 │       │
│  │  official postgres:16 image                                                       │       │
│  │  port :5432 (internal network only)                                               │       │
│  └──────────────────────────────────────────────────────────────────────────────────┘       │
│                                                                                              │
│  ┌─────────────────────────────────────────────────────────────────────────────────┐        │
│  │  otel-collector  (optional sidecar — commented out by default)                  │        │
│  │  otel/opentelemetry-collector-contrib (pinned)                                  │        │
│  │  grpc :4317 ← OTLP from onchain-service                                         │        │
│  │  routes to operator's Jaeger / Grafana Tempo / Honeycomb                        │        │
│  └─────────────────────────────────────────────────────────────────────────────────┘        │
│                                                                                              │
│  External access (firewall / reverse proxy):                                                 │
│    :8080  REST + WS ── consumer HTTP clients + WS subscribers                               │
│    All other ports (5432, 8899, 8900, 8545, 8546, 4317) internal-only.                     │
└──────────────────────────────────────────────────────────────────────────────────────────────┘
```

The key difference from design 0027's topology: the Solana node box is `agave-validator`
in RPC-only mode (no Yellowstone plugin, no Geyser account-update stream) and requires
approximately 64–128 GB RAM rather than 256–512 GB. The process boundary is identical
(two sibling processes), but the node class is fundamentally lighter. Detailed hardware
sizing for three operator topologies is in §5.

---

## §4 Audit of Current Code Under the New Model

This section calls out what changes and what is preserved for each major crate.

### §4.1 `crates/chain-adapter/src/solana/` — REWRITE

The Yellowstone gRPC client calls throughout `subscribe.rs` and `reconnect.rs` are replaced
with standard Solana JSON-RPC + WebSocket. The six files in the module (`subscribe.rs`,
`reconnect.rs`, `config.rs`, `backfill.rs`, `decode.rs`, `mod.rs`) all touch either the
`mg_yellowstone_proto::GeyserClient` interface or Yellowstone-specific filter/update types.
After the rewrite:

- `subscribe.rs` uses `programSubscribe`, `accountSubscribe`, `logsSubscribe`, and
  `signatureSubscribe` WebSocket subscriptions via an extension of
  `crates/chain-adapter/src/jsonrpc/`.
- `reconnect.rs` is re-scoped to manage WS reconnect for standard JSON-RPC subscriptions
  rather than the Yellowstone session-reconnect loop.
- `config.rs` loses the Yellowstone endpoint and filter configuration; gains standard RPC
  + WS endpoint configuration (mirroring `EthereumAdapterConfig`).
- `backfill.rs` and `decode.rs` have minimal changes: `mg_yellowstone_proto` import sites
  become `mg_solana_types` sites (already partially migrated in Sprint 25); the RPC call
  shape is preserved (`getSignaturesForAddress`, `getTransaction`, `getBlock`).
- `mod.rs` removes `GeyserClient` health-check and tip-slot calls; gains standard RPC
  equivalents (`getHealth`, `getSlot`).

`token2022.rs` is purely a decoder module with no ingestion dependency. Unaffected.

Estimated LOC delta: approximately −600 LOC removed (Yellowstone session management,
filter encoding, slot-update dispatch) + approximately +400 LOC added (WS subscribe
management, standard RPC call wrappers) = net approximately −200 LOC.

### §4.2 `crates/yellowstone-proto/` — DELETE

The entire crate directory is removed. The `.proto` files, `build.rs`, and `src/lib.rs`
are deleted. The workspace `Cargo.toml` `[workspace.members]` entry is removed. The
`tonic-prost-build` build-dep and `tonic-prost` runtime dependency are removed from the
workspace `[workspace.dependencies]` block once no other crate references them (verify
with `cargo tree` before removing).

`tonic` and `prost` remain because they are used by the OpenTelemetry OTLP exporter.
The split `tonic-prost` crate (the tonic 0.14 prost-codec runtime) is specific to the
Yellowstone use case and is removed.

### §4.3 `crates/indexer/` — MODE SHIFT, minimal rewrite

The indexer's current architecture is a continuous-stream router (`coordinator.rs` spawns
per-chain workers via `router.rs`; `sink.rs` delivers events to detector pipelines;
`hooks.rs` manages detector lifecycle). The mode shift under the pull-based model:

- `coordinator.rs`: gains a `trigger_evaluate(token: TokenAddress, chain: Chain)` async
  method. The MultiChainCoordinator exposes this via its public API. The background
  streaming subscription task is replaced by the periodic scan workers spawned in
  `crates/server/src/init/`.
- `router.rs`: the event fan-out router retains its internal structure but its input source
  changes from a streaming channel to an explicit `evaluate(token)` dispatch.
- `sink.rs`: the `AnomalyEventSink` trait and its Postgres implementation are unchanged.
  Anomaly events continue to flow from detectors through the sink to the database.
- `hooks.rs`: detector lifecycle management is unchanged.

The net code change is approximately −200 LOC (removing streaming subscription startup,
checkpoint-update-on-every-slot logic) + approximately +100 LOC (trigger method,
verdict-cache write-path). Minimal structural surgery; maximum reuse of existing plumbing.

### §4.4 `crates/storage/` — minor schema additions + retention policies

Two additions:

1. **`verdict_cache` table** (new): stores the most recent scored verdict per
   (token_address, chain). Columns: `token_address`, `chain`, `score` (NUMERIC),
   `severity` (text enum), `detector_results` (JSONB), `evaluated_at` (timestamptz),
   `expires_at` (timestamptz). Primary key is (token_address, chain). `expires_at` is
   set to `evaluated_at + TTL` per the detector class rules in ADR 0007 §9.5.
   Migration: V00017.

2. **Retention policy adjustments**: bulk event tables that were intended to accumulate
   all transfers and swaps for all tokens are either dropped (if never populated in
   production) or archived (if populated). The decision on which tables qualify is in
   §11.4.

Estimated LOC delta: approximately +120 LOC (migration file + `VerdictCache` struct in
`crates/storage/src/pg.rs` + cache read/write methods) + approximately −50 LOC if any
bulk-event tables are removed from the schema helper.

### §4.5 `crates/gateway/` — two additions

1. **`GET /v1/score?token=<addr>&chain=<chain>` REST endpoint** — synchronous single-token
   evaluation. The handler calls `MultiChainCoordinator::trigger_evaluate(token, chain)`,
   waits for the result (with a configurable timeout, default 30 seconds), and returns the
   verdict as JSON. If a valid cached verdict exists (not expired), it is returned
   immediately without triggering a fresh evaluation. The endpoint is rate-limited via the
   existing `crates/gateway/src/ratelimit.rs` machinery.

2. **Watchlist WS subscription** — an extension of the existing `/v1/ws/stream` endpoint.
   Consumers subscribe to a token or set of tokens; the service pushes verdict updates to
   the subscriber as evaluations complete. The subscription message shape is an `AnomalyEvent`
   extended with a `verdict_summary` field. Backpressure: slow consumers are disconnected
   after a configurable buffer threshold (default 100 unread messages), identical to the
   existing WS slow-consumer policy.

Estimated LOC delta: approximately +300 LOC (route handler, timeout wrapper, cache
read-first logic, WS subscription plumbing).

### §4.6 `crates/server/src/init/` — periodic scan workers

Two new in-process tokio tasks are spawned in the server initialization sequence
(mirroring the smart-money labelling task spawned in Sprint 22):

1. **Watchlist rescore worker** — polls the watchlist table on a configurable cadence,
   dispatches `trigger_evaluate` for each token. Respects verdict cache TTLs: tokens whose
   cached verdicts are fresh are skipped. Uses a cancellation token for graceful shutdown
   (identical to the smart-money labeller pattern).

2. **New-launch discovery worker** — queries factory programs for newly created pools on
   each tick, discovers newly launched tokens, adds them to the watchlist, and queues
   an initial evaluation. Factory addresses are in `config/adapters.toml` (already exists
   for DEX factory program IDs).

Estimated LOC delta: approximately +200 LOC across `init/` plus the new periodic worker
files.

### §4.7 `crates/detectors/` and `crates/scoring/` — unchanged in logic

Detector trait implementations, threshold configurations, evidence schemas, and scoring
aggregation logic are untouched. The `Detector::evaluate` signature does not change. The
only possible adjustment is a thin adapter method `DetectorSet::evaluate_token(token,
context)` that packages the trigger into the existing trait call signature — approximately
+20 LOC if needed, otherwise zero change.

---

## §5 Per-Chain Deployment Topologies

Three operator-facing deployment scenarios replace the single-box monolith from design
0027 §11.8. Each is a fully valid production configuration; operators self-select based
on their chain coverage requirements.

### Topology A — Ethereum-Only Operator

Single box. Ethereum detectors (D12, D13) plus the four chain-agnostic EVM detectors
(D02, D03, D04, D06, D10, D11) scoped to Ethereum-listed tokens.

| Resource | Minimum | Recommended |
|----------|---------|-------------|
| CPU | 8 cores | 16 cores |
| RAM | 32 GB | 64 GB |
| NVMe | 2.5 TB | 4 TB |
| Network | 100 Mbps | 1 Gbps |

Estimated cost: $80–150/mo (e.g., Hetzner AX41-NVMe at ~€60/mo, OVHcloud Advance-1 at
~$100/mo). Services co-resident: Reth + Lighthouse, Postgres, onchain-service.

### Topology B — Solana-Only Operator

Single box. All 11 Solana detectors (D01–D11) active. Solana node in RPC-only mode:
no Geyser plugin, no Yellowstone stream, no account-update firehose.

| Resource | Minimum | Recommended |
|----------|---------|-------------|
| CPU | 12 cores | 16 cores |
| RAM | 64 GB | 128 GB |
| NVMe | 2.5 TB | 4 TB |
| Network | 100 Mbps | 1 Gbps |

Estimated cost: $150–250/mo (e.g., Hetzner AX102 at ~€130/mo, Equinix Metal m3.small).
Services co-resident: Agave RPC-only node, Postgres, onchain-service.

The RAM requirement (64–128 GB) is approximately 4× lower than the full validator
requirement (256–512 GB) documented in ADR 0003 §Hardware. The reduction comes from
running the Agave node in RPC-only mode, which does not maintain the full live accounts
database in memory and does not run the Yellowstone gRPC plugin. Standard RPC queries
(`getAccountInfo`, `getProgramAccounts` with filters, `getBlock`) are served by a much
lighter in-memory footprint.

### Topology C — Multi-Chain Operator (Ethereum + Solana)

Single box or split two-box. All 13 detectors active across both chains.

**Single-box:**

| Resource | Minimum | Recommended |
|----------|---------|-------------|
| CPU | 24 cores | 32 cores |
| RAM | 128 GB | 256 GB |
| NVMe | 5.5 TB | 8 TB |
| Network | 1 Gbps | 1 Gbps |

Estimated cost: $300–500/mo (e.g., Hetzner AX162-S at ~€250/mo, OVHcloud Advance-3 at
~$350/mo). Services: Reth + Lighthouse, Agave RPC-only, Postgres, onchain-service.

**Split two-box (higher availability):** Reth + Lighthouse on machine A (~32 GB RAM,
2 TB NVMe); Agave RPC-only + Postgres + onchain-service on machine B (~128 GB RAM, 4 TB
NVMe). Eliminates the scenario where a Solana node restart (triggered by an Agave upgrade)
disrupts Ethereum detection and vice versa. Recommended when the service feeds a consumer
with SLA requirements. Cost: approximately the sum of Topologies A + B.

In all topologies, the Reth node requires approximately 2 TB NVMe growing at ~75 GB/month
(pruned mode, full transaction history). The Solana RPC node requires approximately 2 TB
NVMe for the ledger and account snapshots. Postgres for the query engine model is modest:
approximately 50 GB initial, growing at ~10 GB/month (verdict cache + anomaly events only,
no bulk event accumulation).

---

## §6 Solana Ingestion Rewrite Specification

This section provides enough detail for the developer agent dispatched for task T26-2 to
begin implementation without architectural ambiguity.

### §6.1 JSON-RPC method inventory

The rewritten `crates/chain-adapter/src/solana/` uses the following standard Solana
JSON-RPC methods. All are part of the public Solana JSON-RPC specification
(https://solana.com/docs/rpc).

**Subscription (WebSocket):**
- `programSubscribe(<PROGRAM_ID>, {commitment: "confirmed", encoding: "base64"})` — account
  updates for all accounts owned by a program (used for pool monitoring on Raydium, Orca, etc.)
- `accountSubscribe(<ACCOUNT_PUBKEY>, {commitment: "confirmed", encoding: "base64"})` — single
  account updates (used for specific pool accounts on watchlist)
- `logsSubscribe({mentions: [<TOKEN_MINT>]}, {commitment: "confirmed"})` — all transactions
  mentioning a specific token mint (used for transfer log monitoring)
- `signatureSubscribe(<SIGNATURE>, {commitment: "finalized"})` — transaction confirmation
  (used when we submit simulation transactions via `simulateTransaction`)

**Block and transaction fetch:**
- `getBlock(<SLOT>, {transactionDetails: "full", rewards: false, commitment: "confirmed"})` —
  fetch a complete block for periodic-scan tick processing
- `getSignaturesForAddress(<PUBKEY>, {limit: 1000, before: <SIG>, commitment: "confirmed"})` —
  paginated signature list for backfill and initial token evaluation
- `getTransaction(<SIGNATURE>, {encoding: "jsonParsed", maxSupportedTransactionVersion: 0})` —
  fetch individual transaction detail during initial evaluation

**Account state:**
- `getAccountInfo(<PUBKEY>, {encoding: "base64", commitment: "confirmed"})` — raw account data
  for pool state decode and token-2022 extension inspection
- `getMultipleAccounts([<PUBKEY...>], {encoding: "base64"})` — batch fetch for holder snapshot
  (top-N holders from D03 holder concentration)
- `getProgramAccounts(<PROGRAM_ID>, {filters: [{memcmp: ...}]})` — enumerate pool accounts for
  new-launch discovery; filtered to avoid unbounded responses

**Chain status:**
- `getHealth` — liveness check; replaces Yellowstone `healthCheck()` gRPC call
- `getSlot({commitment: "confirmed"})` — current tip slot; replaces Yellowstone `getTip()`

### §6.2 Extending `crates/chain-adapter/src/jsonrpc/`

The current `jsonrpc/` module (`mod.rs`, `rpc.rs`, `types.rs`, `decoder.rs`, `adapter.rs`)
implements JSON-RPC 2.0 over WebSocket for Ethereum-specific method names and response shapes.
The underlying transport (tokio-tungstenite, JSON-RPC 2.0 framing, subscription management) is
chain-agnostic.

The recommended approach (§11.1 sign-off item) is to generalise the module so both chains
share the transport layer while each chain has its own method-name constants and response-type
deserializers. Concretely:

- The `JsonRpcClient` struct and its `call<Params, Result>` and `subscribe<Params, Notification>`
  generic methods become fully chain-agnostic; they speak JSON-RPC 2.0 over WS without knowing
  what chain they're talking to.
- Ethereum-specific method names and `eth_*` response types move to `crates/chain-adapter/src/ethereum/rpc.rs` as typed wrappers.
- New Solana-specific method names and response types live in `crates/chain-adapter/src/solana/rpc.rs` (new file), following the same pattern.
- The `JsonRpcClient` itself is exposed for direct use by both `ethereum/` and `solana/` modules.

This avoids duplicating the WebSocket connection management, reconnect logic, and
request-id tracking. The LOC overhead is approximately +100 LOC to refactor the module
boundary + approximately +300 LOC of Solana-specific method wrappers in `solana/rpc.rs`.

### §6.3 Commitment levels

The Solana commitment system maps to the existing `CommitmentConfig` enum in
`crates/chain-adapter/src/solana/config.rs`. Under the new model:

- `processed` — never used in production. Exists for local development only.
- `confirmed` — hot path for subscriptions and block fetches. Analogous to Ethereum depth-12
  (safe block). Used for all real-time evaluation data.
- `finalized` — used for checkpoint saves (`adapter_checkpoints` table) and immutable audit
  trail writes. Analogous to Ethereum `finalized` block tag.

### §6.4 Reorg handling

Solana reorgs (forks resolved differently from the optimistic view) are handled by tracking
the `confirmed` commitment as the canonical view. The `slot` field in emitted events reflects
the committed slot, not the most recent `processed` slot. When a slot that was previously
`confirmed` is dropped due to a fork resolution, the chain-adapter detects the discontinuity
by comparing the `parent` field in successive `getBlock` responses. A `ReorgMarker { slot }`
event is emitted to the indexer for affected slots, which marks any `anomaly_events` persisted
for those slots with `reorg = true` (the existing column in `anomaly_events`; see V00001 migration).

This logic is already partially implemented in `crates/chain-adapter/src/solana/backfill.rs`.
The rewrite preserves it and extends it to the subscription path.

### §6.5 Backfill paging

Backfill for a specific token uses `getSignaturesForAddress` with cursor-based paging via the
`before` parameter. The implementation mirrors `crates/chain-adapter/src/solana/backfill.rs`
(already uses this pattern). The cursor is checkpointed to `adapter_checkpoints` after each
successful batch, enabling restart-from-checkpoint on failure.

Backfill and live subscription MUST NOT race on the same token. The existing invariant from the
`ChainAdapter` trait documentation ("backfill MUST complete before subscribe starts for the same
address range") is preserved.

---

## §7 Indexer Mode Shift Specification

### §7.1 From streaming fan-out to trigger-driven evaluation

The continuous-stream model in `crates/indexer/coordinator.rs` spawns per-chain workers that
maintain persistent WebSocket connections and route incoming events to detector pipelines. Under
the pull-based model, the coordinator is restructured as a trigger-driven worker pool.

The external interface that the rest of the service sees — `MultiChainCoordinator` — gains
one new public method:

```rust
pub async fn trigger_evaluate(
    &self,
    token: TokenAddress,
    chain: Chain,
    reason: EvaluationReason,
) -> anyhow::Result<VerdictSummary>
```

`EvaluationReason` is an enum: `RestRequest`, `WatchlistScan`, `PeriodicRescore`,
`NewLaunchDiscovery`. It is recorded in `verdict_cache` for audit purposes.

The internal worker pool processes `trigger_evaluate` calls with bounded concurrency
(configurable `max_concurrent_evaluations`, default 8). Each evaluation:

1. Checks `verdict_cache` for a fresh non-expired entry; returns it immediately if found.
2. If no fresh entry, acquires a semaphore permit, fetches chain state via the chain-adapter,
   runs the detector set, aggregates via scoring, writes to `verdict_cache` and
   `anomaly_events`, releases the permit, and returns the verdict.

### §7.2 Periodic workers

The `watchlist_rescore_worker` and `new_launch_discovery_worker` are async functions that run
in an infinite loop with a `tokio::time::interval` tick and a cancellation token check. They
are spawned from `crates/server/src/init/` alongside the existing smart-money labelling task,
using the same `JoinHandle` + drain pattern. Graceful shutdown waits for any in-flight
evaluation to complete (up to the 30-second drain window).

### §7.3 Preserved streaming for HFT detectors

D12 (Permit2 drainer) and D13 (sandwich MEV) benefit from per-block coverage. For tokens
on the watchlist that are flagged as EVM tokens, the new-launch discovery worker triggers
a re-evaluation on every new Ethereum block (approximately every 12 seconds, driven by the
`eth_subscribe("newHeads")` subscription in `crates/chain-adapter/src/ethereum/subscribe.rs`).
This is not a continuous stream of all events — it is a per-block trigger that fires a
standard evaluation of the watchlisted tokens. The subscription infrastructure in the
Ethereum chain-adapter that already handles `newHeads` subscriptions is reused for this
purpose.

---

## §8 Storage Retention Policies

### §8.1 `verdict_cache` (new table, V00017)

Schema:

```sql
CREATE TABLE verdict_cache (
    token_address   TEXT        NOT NULL,
    chain           TEXT        NOT NULL,
    score           NUMERIC(5, 4) NOT NULL,
    severity        TEXT        NOT NULL,
    detector_results JSONB      NOT NULL,
    reason          TEXT        NOT NULL,
    evaluated_at    TIMESTAMPTZ NOT NULL,
    expires_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (token_address, chain)
);

CREATE INDEX verdict_cache_expires_at_idx ON verdict_cache (expires_at);
```

Rows are upserted on each evaluation. The `expires_at` index enables a periodic cleanup
job (tokio task, runs hourly) that deletes rows where `expires_at < NOW()`. There is no
partitioning; the table is small (one row per watchlisted token, capped by watchlist size).

### §8.2 `anomaly_events` (existing table — retention adjustment)

Currently grows without bound. Under the pull-based model, anomaly events are rare (only
tokens that exceed a detector threshold), so the growth rate is much lower than in the
streaming model. Retention policy: keep all rows indefinitely (the audit trail is
operationally valuable) with an operator-configurable retention period in `config/service.toml`
(default: `retain_anomaly_events_days = 0` meaning no automatic deletion). The retention
policy is enforced by the same hourly cleanup task that manages `verdict_cache`.

### §8.3 Bulk event tables

If any tables were created in previous migrations to hold bulk transfers, swaps, or pool
events for all tokens (as opposed to anomaly events or per-detector state), these are dropped
in V00017 if they have never been populated in the operator's production database. The
decision criterion is row count: a table with zero rows in production is safe to drop. Tables
with rows require a separate migration decision (§11.4). The dev agent executing T26-5 must
check row counts before generating the V00017 migration.

---

## §9 Deliverables Preserved from Design 0027

The following design-0027 deliverables are independent of the ingestion model and remain
Sprint 26 goals. The implementation specifications in design 0027 are accurate and are
inherited here by reference.

### §9.1 OTLP Exporter Wire-Up

The `TODO(sprint-20)` stub in `crates/server/src/init/tracing_init.rs` is resolved. When
`OTEL_EXPORTER_OTLP_ENDPOINT` is set, the service exports spans via OTLP gRPC using
`opentelemetry-otlp` with the `grpc-tonic` feature (consistent with the tonic already in
the workspace). When unset, stdout-only tracing is unchanged.

New workspace deps: `opentelemetry 0.27`, `opentelemetry_sdk 0.27`,
`opentelemetry-otlp 0.27` (grpc-tonic feature), `tracing-opentelemetry 0.28`. All four
pass ADR 0006 Rule A: OpenTelemetry is an open standard governed by the CNCF
(https://opentelemetry.io/docs/specs/otlp/). The wire format is gRPC over protobuf, using
the same `tonic` and `prost` already in the workspace.

Full spec: design 0027 §5.1.

### §9.2 Testcontainers Postgres Integration Test

New file `crates/server/tests/production_smoke_test.rs`. Gated
`#[cfg(feature = "test-containers")]`, marked `#[ignore]`, run explicitly as the
sprint-close gate.

The test shape adjusts to the pull-based model: instead of dispatching all 13 detectors
against streaming events, it calls `MultiChainCoordinator::trigger_evaluate(token, chain)`
for the synthetic rug-pull baseline token, asserts that the returned `VerdictSummary`
includes at least one anomaly from D02 (rug pull LP drain), and then asserts:

1. A row exists in `verdict_cache` for the evaluated token.
2. A row exists in `anomaly_events` with `detector_id = "rug_pull_lp_drain"` and
   `confidence >= 0.6`.
3. `GET /v1/score?token=<addr>&chain=solana` returns the cached verdict (200 OK, correct
   `detector_id` in evidence).
4. `GET /health` returns `{ "status": "ok" }` (200 OK).
5. `GET /metrics` returns a non-empty Prometheus text body containing
   `http_requests_total`.

Full spec: design 0027 §5.2 (shape adjusted as above).

### §9.3 Health and Prometheus Metrics Endpoints

Health endpoint enrichment: `chain_adapter_status` field per chain, `version` and
`build_commit` fields, `?shallow=true` toggle. Prometheus: `StreamingMetrics` merged into
the `/metrics` response; three new counters
(`anomalies_emitted_total{detector,chain,severity}`,
`chain_adapter_events_processed_total{chain,event_type}`,
`db_query_duration_seconds_bucket{operation}`).

Full spec: design 0027 §5.3.

### §9.4 `infra/docker-compose.prod.yml`

Compose file reflecting the lightweight topology from §3. Key changes relative to design
0027's compose spec:

- `solana-node` service: `agave-validator` (or `agave-rpc`) in RPC-only mode. No Yellowstone
  plugin build step. The `build:` context in the compose file targets a simpler Dockerfile
  in `infra/solana-node/` that builds the Agave RPC binary without the Geyser plugin.
  Operators who already run a full validator separately may point `SOLANA_RPC_URL` at their
  existing node's JSON-RPC endpoint and comment out the `solana-node` service entirely.
- Hardware labels in service comments document the per-topology requirements from §5.
- OTLP collector remains commented-out optional sidecar per §11.9.

### §9.5 `infra/PRODUCTION.md` Operator Runbook

Three-topology deployment guide (§5 topologies A, B, C), cold-start procedure, readiness
signals, backfill runbook for initial population of the watchlist from N days of history,
Postgres backup procedure, rollback, secrets management, and tunable environment variables.

The backfill runbook adjusts to the query engine model: "populate watchlist from factory
program `PairCreated` events over the last N days, then run an initial evaluation for each
discovered token." This replaces the design 0027 §5.5 approach of backfilling all raw
transfers and swaps.

---

## §10 Workspace Dependency Additions and Removals

### §10.1 Removals

The following workspace dependencies are removed when `crates/yellowstone-proto/` is
deleted (task T26-3). Verify with `cargo tree -p mg-yellowstone-proto` that no other crate
transitively depends on these before removing:

- `tonic-prost-build` (build-dep) — used only in `crates/yellowstone-proto/build.rs`.
- `tonic-prost` (runtime) — the tonic 0.14 Prost codec crate split; used only by the
  generated Yellowstone gRPC client. The main `tonic` and `prost` crates remain.

No other removals. `ed25519-dalek`, `sha2`, and `tonic-build` added in Sprint 25 remain
(used by `crates/solana-types/`, which is unchanged).

### §10.2 Additions

The OpenTelemetry stack preserved from design 0027 §6:

```toml
# ADR 0006 Rule A: OpenTelemetry is a CNCF-governed open standard.
# Spec: https://opentelemetry.io/docs/specs/otlp/
opentelemetry         = "0.27"
opentelemetry_sdk     = "0.27"
opentelemetry-otlp    = { version = "0.27", features = ["grpc-tonic"] }
tracing-opentelemetry = "0.28"
```

Before implementation, the dev agent must verify the current stable release on crates.io
and adjust version numbers if a newer compatible minor release is available. The Rule A
justification holds for any version of these crates.

---

## §11 Sign-Off Decisions

Ten decisions require explicit user confirmation before implementation begins. Items 1 and
4 carry structural implications for task planning and must be resolved first.

**§11.1 Solana JSON-RPC transport: generalise `crates/chain-adapter/src/jsonrpc/` to be
chain-agnostic vs introduce `crates/chain-adapter/src/solana/jsonrpc.rs` as a parallel
Solana-specific transport.**

Recommendation: **chain-agnostic generalisation of the existing `jsonrpc/` module.** The
underlying transport — JSON-RPC 2.0 over tokio-tungstenite with subscription management
and request-id tracking — is identical for both chains. The only differences are method
names and response-type shapes, which can be expressed as typed wrappers in the chain-specific
modules. Duplicating the transport layer would violate DRY and create two maintenance
surfaces for the same WebSocket reconnect logic. The generalisation adds approximately
+100 LOC to refactor the module boundary; the reduction in long-term maintenance cost
justifies it.

**§11.2 Yellowstone-proto deletion timing: at the start of Sprint 26 (clean break), at
the end (after Solana rewrite is verified), or feature-gated coexistence during the sprint.**

Recommendation: **end-of-sprint deletion (task T26-3 last).** Keeping the crate compilable
until T26-2 (Solana rewrite) is verified green ensures that a developer working on T26-2
can run `cargo clippy --workspace --all-targets` and get a clean result against the full
workspace before the deletion is attempted. Deleting at the start risks a period where both
the old yellowstone path and the new path are simultaneously broken. Feature-gated coexistence
is explicitly rejected: it is the kind of "both paths simultaneously" conditional compilation
that the kludge test prohibits.

**§11.3 Indexer rewrite vs in-place mode shift: rewrite `crates/indexer/` from scratch
vs minimal-diff mode shift of the existing coordinator/router/sink architecture.**

Recommendation: **minimal-diff mode shift.** The existing coordinator, router, sink, and
hooks plumbing is well-tested and represents several sprints of work. The mode shift (adding
a `trigger_evaluate` method, removing the streaming subscription startup loop) touches a
small fraction of the codebase. A full rewrite risks breaking existing behaviour and bloats
the sprint scope. The minimal-diff approach also preserves the existing unit tests in
`crates/indexer/tests/` without modification.

**§11.4 Storage migration policy for bulk event tables: drop in V00017, keep-but-orphan,
or migrate-and-archive.**

Recommendation: **drop in V00017 for tables that have never been populated in production
(verified by row count check at migration execution time).** A table that was designed for
the streaming model but was never populated with real data carries no operational risk if
dropped. Tables that ARE populated in production (even partially) require a separate
migration decision with explicit user sign-off before deletion. The V00017 migration
includes a guard: it fails loudly if it encounters a non-empty table in the drop list, so
the operator must decide explicitly. Orphaned-but-unreachable tables (no code references
them after the mode shift) are safe to leave for one sprint and drop in V00018 after
confirming zero production data.

**§11.5 Verdict cache TTL configurability: hard-coded constants, config-toml entries, or
per-detector overrides.**

Recommendation: **config-toml entries in `config/detectors.toml`**, one entry per detector,
with the defaults from ADR 0007 §9.5 (5-minute TTL for fast-moving signals, 1-hour TTL for
slow-moving signals, 15-minute TTL for D01 honeypot). Per-detector overrides allow operators
to tune cadence for their specific watchlist and hardware without code changes. Hard-coded
constants are rejected for the same reason all thresholds are in config: every number
should be defensible and adjustable.

**§11.6 Periodic scan worker placement: in-process tokio task vs separate binary vs
system cron.**

Recommendation: **in-process tokio task**, spawned from `crates/server/src/init/` with a
cancellation token, exactly mirroring the smart-money labelling background task added in
Sprint 22. The smart-money pattern is already tested, already drains gracefully on SIGTERM,
and already logs via the standard `tracing` spans. Duplicating that pattern for the two new
workers is ~30 LOC each plus the worker body. A separate binary would require IPC to share
the Postgres pool and chain-adapter clients; a system cron job cannot participate in the
service's 30-second graceful drain window.

**§11.7 ZBT-on-BSC labelled-positive fixture: add as Sprint 26 task T26-10 (optional) vs
defer to Sprint 27.**

Recommendation: **add as T26-10, time-permitting.** The ZBT demo on 2026-04-27 is the
concrete proof that motivated this entire ADR. Capturing the ZBT token's on-chain state
as a labelled-positive fixture for D03 (holder concentration), D05 (wash trading), and D06
(mint authority) turns the demo into a permanent regression test and provides a
demo-ready artifact for consumer conversations. The risk of including it as T26-10 is
scope creep if the sprint runs long; marking it optional means it does not gate sprint
closure. If T26-9 finishes with sprint time remaining, T26-10 is the first pick-up task.

**§11.8 Hardware target documentation: all three topologies (A, B, C) in PRODUCTION.md
vs single default with scaling notes.**

Recommendation: **all three topologies** as separate named sections in `infra/PRODUCTION.md`.
Operators arrive at the runbook with a specific deployment context (Ethereum-only startup,
Solana-only validator operator, full multi-chain deployment). Burying topology choices in
footnotes forces them to read everything before finding the relevant BOM. Three named
sections (§A, §B, §C) let the operator navigate directly to their configuration.

**§11.9 OTLP collector in compose: ship as commented-out optional vs require
operator-provided.**

Recommendation: **commented-out optional** — same as design 0027 §11.11. The
`otel/opentelemetry-collector-contrib` service is included as a commented-out block in
`infra/docker-compose.prod.yml` with an `infra/otel-collector-config.yaml.example` file
that routes spans to Jaeger (self-hosted) and a generic OTLP/HTTP endpoint as examples.
An operator who wants observability uncomments the block and provides the config; an
operator who does not wants the service is unaffected.

**§11.10 Backwards compatibility for the current consumer surface: preserve the existing
`/v1/anomaly_events` endpoint shape vs introduce a `v2` path alongside `v1`.**

Recommendation: **preserve `v1` unchanged.** No consumer has integrated against the current
API surface yet; all four sibling systems have zero on-chain visibility (per CLAUDE.md).
However, ADR 0001 §D8 documented the API contract as a commitment. The new `/v1/score`
endpoint is additive. The `v1` anomaly events endpoint is unchanged. Any breaking change
to the response schema goes in `v2` with `v1` retained for at least one sprint. Sprint 26
does not introduce any breaking changes.

---

## §12 Sub-Task Breakdown for Implementation

Nine tasks, one optional. Ordered by dependency. Each task includes an estimated LOC delta,
the pre-requisites, the verification command, and the agent type best suited for dispatch.

**CRITICAL: every dev-agent dispatch brief for Sprint 26 MUST include the following
verbatim at the top.**

> ANTI-DETOUR: tools work. Do NOT invoke skills. Do NOT edit settings.json. Do NOT try
> `fewer-permission-prompts`. Do the work directly.
>
> SCOPE: verification is `cargo clippy --workspace --all-targets -- -D warnings`. Not
> `-p scope`. Not `--lib` only. WORKSPACE SCOPE. This is the non-negotiable gate.
> Sub-agents in S24 #5a and S25 T25-5 first-attempt reported clean state on narrow scope
> while the workspace had warnings. Do not repeat this.
>
> DISK: prefer `cargo check --workspace --all-targets` for iterative verification during
> implementation. Reserve `cargo build --workspace --all-targets` for milestone verification.
> testcontainers + bollard add ~2–3 GB to target/ when the test-containers feature is enabled.
> If disk fills: `cargo clean` and switch to `cargo check`.
>
> KLUDGE TEST: no bridges, no feature flags that gate vendor crates, no in-process vendor
> SDK linkage. OpenTelemetry crates are admitted under ADR 0006 Rule A (generic CNCF spec);
> add an ADR 0006 attribution comment next to each new dep in Cargo.toml.
>
> OVER-REPORT HISTORY: S24 #5a and S25 T25-5 first-attempt both reported "clippy clean"
> on narrow scope while the workspace had warnings. S25 T25-5 first-attempt also pivoted
> to invoking the `fewer-permission-prompts` skill when a tool denied; do not do this.

---

**T26-1: Generalise `crates/chain-adapter/src/jsonrpc/` to chain-agnostic transport**

Description: Refactor the `jsonrpc/` module so the core JSON-RPC 2.0 over WebSocket
transport (`JsonRpcClient`, subscription management, request-id tracking, reconnect loop)
contains no Ethereum-specific method names or response types. Move
Ethereum-specific method wrappers (`eth_subscribe`, `eth_getLogs`, `eth_call`, etc.) to
`crates/chain-adapter/src/ethereum/rpc.rs` typed wrappers that call the generic
`JsonRpcClient::call` and `JsonRpcClient::subscribe` methods. Verify the Ethereum adapter
still compiles and its existing tests pass. No Solana code yet.

Estimated LOC delta: +100 LOC (refactor boundary) −30 LOC (consolidation). Net: ~+70 LOC.
Dependencies: none — first task.
Verification: `cargo clippy --workspace --all-targets -- -D warnings` + existing
Ethereum adapter tests green.
Agent type: developer.

---

**T26-2: Rewrite `crates/chain-adapter/src/solana/` from Yellowstone gRPC to standard
JSON-RPC + WS**

Description: Rewrite `subscribe.rs` and `reconnect.rs` using the Solana WebSocket
subscriptions listed in §6.1. Add `crates/chain-adapter/src/solana/rpc.rs` with typed
wrappers for all JSON-RPC methods listed in §6.1, using the now-generic `JsonRpcClient`
from T26-1. Update `config.rs` to mirror `EthereumAdapterConfig` (RPC URL + WS URL,
commitment config, backfill batch size). Update `mod.rs` to replace Yellowstone
`getHealth` / `getSlot` calls with standard equivalents. Preserve `decode.rs`,
`backfill.rs`, `checkpoint.rs`, and `token2022.rs` — these are already mostly
migrated from `solana-sdk` (Sprint 25) and do not need structural changes.

This task depends on T26-1 completing cleanly. The agent must NOT delete
`crates/yellowstone-proto/` yet; that is T26-3.

Estimated LOC delta: −600 LOC (Yellowstone path) +400 LOC (standard JSON-RPC + WS path).
Net: ~−200 LOC.
Dependencies: T26-1 merged and workspace clippy clean.
Verification: `cargo clippy --workspace --all-targets -- -D warnings` + Solana adapter
unit tests green (mock-RPC responses are sufficient; no live node required).
Agent type: blockchain-engineer.

---

**T26-3: Delete `crates/yellowstone-proto/` and workspace dep cleanup**

Description: Delete the `crates/yellowstone-proto/` directory. Remove the workspace
member entry from `Cargo.toml`. Remove `mg-yellowstone-proto` from `crates/chain-adapter/Cargo.toml`.
Verify with `grep -rn "mg_yellowstone_proto\|yellowstone_proto" crates/ --include="*.rs"`
returns no matches. Then remove `tonic-prost-build` from `[workspace.dependencies]`
(verify no other crate uses it) and `tonic-prost` (same check). Run
`cargo clippy --workspace --all-targets -- -D warnings`.

Estimated LOC delta: −430 LOC (deleted proto files + build.rs + lib.rs) + −15 LOC
(Cargo.toml dep removals).
Dependencies: T26-2 merged and workspace clippy clean (solana adapter no longer imports
yellowstone-proto).
Verification: `cargo clippy --workspace --all-targets -- -D warnings` clean. Zero
`yellowstone` references in `*.rs` files (grep confirmation).
Agent type: developer.

---

**T26-4: Indexer mode shift — add `trigger_evaluate` path to `crates/indexer/`**

Description: Add `trigger_evaluate(token, chain, reason)` to `MultiChainCoordinator`.
Add `VerdictSummary` return type. Add the bounded semaphore for max-concurrent-evaluations.
Add the `verdict_cache` read-first logic (check for fresh entry before dispatching). Wire
the evaluation result into `verdict_cache` write path and `anomaly_events` sink (existing
path). Remove the streaming subscription startup loop from `coordinator.rs` (or gate it
behind the periodic workers). Add the two periodic worker functions
(`watchlist_rescore_worker`, `new_launch_discovery_worker`) in `crates/server/src/init/`
following the smart-money labeller pattern.

This task does NOT change any detector implementations. It changes only the dispatch
mechanism.

Estimated LOC delta: +350 LOC (trigger method, worker tasks, verdict cache integration)
−200 LOC (streaming startup loop removal). Net: ~+150 LOC.
Dependencies: T26-3 (workspace clean without yellowstone). T26-5 must complete first to
provide the `verdict_cache` table.
Verification: `cargo clippy --workspace --all-targets -- -D warnings` + existing indexer
tests green + a new unit test for `trigger_evaluate` with a mock chain-adapter.
Agent type: developer.

---

**T26-5: Storage schema additions — `verdict_cache` + V00017 migration**

Description: Write migration V00017 (next after V00016). The migration creates the
`verdict_cache` table (schema in §8.1). It checks row counts for any bulk-event tables
listed in the migration's comment block; if any non-empty table is in the drop list,
the migration fails with a diagnostic error message directing the operator to sign off
before proceeding. Add `VerdictCacheEntry` struct and `PgStore::upsert_verdict_cache` /
`PgStore::get_verdict_cache` methods in `crates/storage/src/pg.rs`. Add the hourly
cleanup task for expired `verdict_cache` rows.

Estimated LOC delta: +180 LOC (migration file + pg.rs additions + cleanup task).
Dependencies: none — migration work is independent of ingestion rewrite. Can run in
parallel with T26-1 through T26-3.
Verification: `cargo clippy --workspace --all-targets -- -D warnings` + migration test
(existing sqlx migration test pattern in the storage crate).
Agent type: data-engineer.

---

**T26-6: Gateway additions — `/v1/score` REST endpoint + watchlist WS subscription**

Description: Add `GET /v1/score?token=<addr>&chain=<chain>` route to the axum router.
Handler calls `MultiChainCoordinator::trigger_evaluate`, waits up to 30 seconds (timeout
configurable), returns `VerdictSummary` as JSON. Rate-limited via the existing
`crates/gateway/src/ratelimit.rs` machinery. Add watchlist WS subscription extension to
the existing `/v1/ws/stream` handler: consumers send a `{"type": "subscribe_watchlist",
"tokens": ["<addr>", ...]}` message; the service pushes `VerdictSummary` updates for
those tokens as evaluations complete.

Estimated LOC delta: +320 LOC (route handler, timeout wrapper, WS subscription plumbing,
tests).
Dependencies: T26-4 (trigger_evaluate method must exist), T26-5 (verdict_cache schema).
Verification: `cargo clippy --workspace --all-targets -- -D warnings` + gateway route
integration tests (mock coordinator, assert response shape).
Agent type: developer.

---

**T26-7: OTLP wire-up + health endpoint enrichment + merged `/metrics`**

Description: Resolve the `TODO(sprint-20)` in `crates/server/src/init/tracing_init.rs`.
Add four OpenTelemetry crates to `[workspace.dependencies]`. Attach the OTLP layer when
`OTEL_EXPORTER_OTLP_ENDPOINT` is set. Extend `HealthResponse` with `chain_adapter_status`,
`version`, `build_commit`, and `?shallow=true` toggle. Merge `StreamingMetrics::registry`
into the `/metrics` response. Add the three new counters (anomalies_emitted_total,
chain_adapter_events_processed_total, db_query_duration_seconds_bucket).

This task is independent of the ingestion rewrite and can run in parallel with T26-1.

Estimated LOC delta: +250 LOC (tracing_init.rs + health.rs + metrics_handler.rs +
Cargo.toml additions).
Dependencies: none for the OTLP + health + metrics work. T26-6 must complete first for
the production smoke test (T26-8) to exercise all three endpoints.
Verification: `cargo clippy --workspace --all-targets -- -D warnings`. OTLP compile
check: `OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 cargo run --bin onchain-service -- --help`
must not panic.
Agent type: developer.

---

**T26-8: Testcontainers Postgres integration test (`production_smoke_test.rs`)**

Description: Create `crates/server/tests/production_smoke_test.rs` with the five
assertions listed in §9.2. The test starts a Postgres container, runs all V00001–V00017
migrations (including the new `verdict_cache` table), injects the rug-pull baseline token
via the existing `inject_baseline` helper, calls `trigger_evaluate`, and then asserts via
both direct Postgres queries and REST calls (`/v1/score`, `/health`, `/metrics`). The test
is gated `#[cfg(feature = "test-containers")]` and marked `#[ignore]`.

Before writing any new harness code, read `crates/gateway/tests/` for existing axum test
patterns and `crates/server/src/bin/onchain_validate.rs` for the existing Postgres +
testcontainers setup. Reuse as much existing infrastructure as possible.

Estimated LOC delta: +200 LOC (new test file; no production code changes).
Dependencies: T26-4, T26-5, T26-6, T26-7 all merged (AppState must include all new
fields for `/health` and `/metrics` assertions to cover the full scope).
Verification: `cargo test --features test-containers -p mg-onchain-server production_smoke_test -- --ignored --nocapture`
Agent type: developer + systems-qa review.

---

**T26-9: `infra/docker-compose.prod.yml` + `infra/.env.example` + `infra/PRODUCTION.md`**

Description: Create `infra/docker-compose.prod.yml` for the lightweight topology (§3).
The Solana node service uses `agave-validator` in RPC-only mode (no Yellowstone plugin
build step). Create `infra/.env.example`. Create `infra/otel-collector-config.yaml.example`.
Create `infra/PRODUCTION.md` with all three topology sections (A, B, C from §5), cold-start
procedure, readiness signals, watchlist backfill procedure for initial population from N
days of factory events, Postgres backup, rollback, secrets, and tunable variables.

Verify: `docker compose -f infra/docker-compose.prod.yml config` must complete without
errors.

Estimated LOC delta: +600 LOC (compose YAML + .env.example + otel config example +
runbook Markdown).
Dependencies: none — infrastructure files are independent of code changes. Can run in
parallel with any task. However, reading the final port and endpoint decisions from T26-6
and T26-7 before writing the compose file avoids inconsistencies.
Verification: `docker compose -f infra/docker-compose.prod.yml config` clean.
Agent type: systems-qa (infrastructure documentation).

---

**T26-10 (optional, time-permitting): ZBT-on-BSC labelled-positive fixture**

Description: Capture the ZBT token (BSC) on-chain state snapshot as a labelled-positive
fixture for D03 (holder concentration), D05 (wash trading), and D06 (mint authority).
Store in `tests/fixtures/bsc/zbt/`. Add an integration test in
`crates/detectors/tests/bsc/zbt_regression.rs` that replays the fixture through all three
detectors and asserts that each fires above its threshold. This is the regression test for
the ZBT demo that motivated ADR 0007.

Estimated LOC delta: +150 LOC (fixture JSON + test assertions).
Dependencies: T26-5 (schema includes verdict_cache so the test can assert cached verdict).
Not a sprint-close gate. Time-permitting only.
Agent type: onchain-analyst.

---

## §13 Open Questions and Out-of-Scope Items

The following are deliberately deferred from Sprint 26:

**Token-2022 detectors (D14–D17).** ConfidentialTransfer, NonTransferable, ScaledUiAmount,
Pausable — all on the carry-forward list. The Sprint 26 Solana adapter rewrite preserves
`token2022.rs` without extending it; these detectors require additional account layout
decoders in `crates/solana-types/`.

**Pump.fun graduation enrichment.** Deferred from Sprint 25 carry-forward list.

**Stage 2 FDR calibration.** Smart-money corpus-blocked; requires 30+ days of live
anomaly-event data at scale.

**Additional EVM chains.** Base, BSC, Arbitrum, Polygon remain Phase 4 scope. The BSC
ZBT fixture (T26-10 optional) tests the existing Ethereum-class adapter logic against BSC
data; it does not constitute a full BSC chain adapter.

**ClickHouse.** The second storage tier for high-volume time-series events remains on the
Phase 3 roadmap. Sprint 26 stays Postgres-only.

**Mempool integration for D13.** Pre-execution MEV detection via
`eth_subscribe("newPendingTransactions")` requires a distinct architectural component.
Deferred.

**`eth_unsubscribe` on Receiver drop + mid-stream WS reconnect.** Sprint 17 TODOs in
`crates/chain-adapter/src/ethereum/`. Still deferred.

**Decimals exact-fetch** (D11/D12/D13 SPEC-NOTEs from Sprint 21). Still deferred.

**SPL Token layout decoders** in `crates/solana-types/`. Deferred per design 0026 §11.6.
Needed when Solana-native detectors need to inspect SPL account data beyond what
`getAccountInfo` raw bytes provide.

**Multi-instance `onchain-service`.** Redpanda/Kafka for horizontal scaling remains on the
Phase 5 roadmap.

**Cross-check test rename** (`*_topic0_matches_sol*` → drop `_sol`). Cosmetic; deferred.

---

## §14 References

| # | Source | URL / Path |
|---|--------|-----------|
| 1 | ADR 0007 — Pull-Based Query Engine | `docs/adr/0007-pull-based-query-engine.md` |
| 2 | ADR 0006 — Code-Level Self-Sovereignty (post-amendment) | `docs/adr/0006-code-level-self-sovereignty.md` |
| 3 | ADR 0003 — Self-Sovereign Infrastructure | `docs/adr/0003-self-sovereign-infrastructure.md` |
| 4 | ADR 0001 §D2 — Wire-Protocol Portion | `docs/adr/0001-phase0-synthesis.md` |
| 5 | Design 0027 — Production Readiness Gate (SUPERSEDED) | `docs/designs/0027-production-readiness-gate.md` |
| 6 | Design 0026 — Solana Stack Divestment | `docs/designs/0026-solana-stack-divestment.md` |
| 7 | `memory/feedback_query_engine_model.md` | Binding user directive; ZBT demo data; hardware BOM revision |
| 8 | `memory/feedback_kludge_test.md` | Kludge test applied to ingestion model |
| 9 | `memory/feedback_subagent_verification.md` | Workspace-scope clippy requirement; over-report history |
| 10 | Solana JSON-RPC reference | https://solana.com/docs/rpc |
| 11 | Solana WebSocket subscriptions | https://solana.com/docs/rpc/websocket |
| 12 | Ethereum JSON-RPC specification | https://ethereum.github.io/execution-apis/api-documentation/ |
| 13 | OpenTelemetry specification (CNCF) | https://opentelemetry.io/docs/specs/otel/ |
| 14 | OTLP protocol specification | https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/protocol/otlp.md |
| 15 | Prometheus exposition format v0.0.4 | https://prometheus.io/docs/instrumenting/exposition_formats/ |
| 16 | testcontainers-rs | https://docs.rs/testcontainers/0.23/ |
| 17 | `crates/chain-adapter/src/ethereum/` | Structural exemplar for Solana adapter rewrite |
| 18 | `crates/chain-adapter/src/jsonrpc/` | Generic JSON-RPC 2.0 over WS transport to be generalised in T26-1 |
| 19 | `crates/yellowstone-proto/` | Crate scheduled for deletion in T26-3 |
| 20 | `crates/indexer/` | Mode shift target in T26-4 |
| 21 | `crates/storage/` | Schema additions in T26-5; V00017 migration |
| 22 | `crates/gateway/` | New endpoints in T26-6 |
| 23 | Agave validator documentation (RPC-only mode) | https://docs.anza.xyz/operations/requirements |
| 24 | Reth Docker image | https://ghcr.io/paradigmxyz/reth |
| 25 | SESSION-KICKOFF.md (Sprint 26 context) | `SESSION-KICKOFF.md` |
