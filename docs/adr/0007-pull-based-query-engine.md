# ADR 0007 — Pull-Based Query Engine Operational Model

**Status:** Proposed — awaits user sign-off on §9 decisions.
**Date:** 2026-04-27
**Author:** architect agent
**Supersedes:** the implicit operational-model assumption (continuous streaming pipeline)
bundled with ADR 0001 §D2. The wire-protocol portion of §D2 (out-of-process processes
talking standard wire format) is preserved and unaffected.
**Inputs:**
- ADR 0001 §D2 — Yellowstone gRPC as canonical out-of-process ingestion pattern; the
  streaming-pipeline operational model implicitly bundled with §D2 is what this ADR supersedes.
- ADR 0003 — self-sovereign infrastructure; no 3rd-party SaaS in production hot path.
- ADR 0006 (post-amendment) — code-level self-sovereignty; wire-protocol integration only;
  kludge test binding.
- `memory/feedback_query_engine_model.md` — binding user directive 2026-04-27.
- `memory/feedback_kludge_test.md` — same logic that killed exex-bridge applied
  symmetrically to the ingestion pattern.
- The ZBT-on-BSC anomaly demo (2026-04-27) — concrete proof that 5–15 RPC calls per token
  is the actual workload, not a continuous firehose.

---

## §1 Status, Date, Author, Supersedes, Inputs

See header above. The status "Proposed — awaits user sign-off on §9 decisions" means
implementation of the Sprint 26 plan (`docs/designs/0028-lightweight-query-engine-deployment.md`)
begins only after the user has confirmed or redirected each of the five sign-off items in §9.

---

## §2 Context

### §2.1 What ADR 0001 §D2 assumed

ADR 0001 §D2 selected the Yellowstone gRPC Geyser plugin protocol as the Solana ingestion
mechanism and characterised the resulting architecture as: validator produces events → gRPC
stream → `crates/chain-adapter/src/solana/subscribe.rs` receives them → `crates/indexer/`
routes them → detectors react to incoming events continuously.

The document was explicit about the protocol choice and the provider-agnostic pattern, and
those remain correct. What was implicit — never written down as a decision — was the
operational model layered on top: the service continuously ingests all chain activity, stores
a running record of transfers, swaps, and pool events for all tokens of interest in Postgres,
and detectors query that running record to produce verdicts. Under that model, Postgres grows
at 100+ GB/month and the Solana node must be validator-class (512 GB RAM, 6 TB NVMe) to
sustain the full account-update + transaction stream.

ADR 0003 reinforced the hardware commitment by documenting the Solana validator BOM at
256–512 GB RAM. ADR 0004 chose Reth as the Ethereum node but noted the much lighter hardware
requirement (~32 GB RAM) without drawing the contrast explicitly.

### §2.2 The ZBT-on-BSC demo falsified the assumption

On 2026-04-27, a complete anomaly verdict for the ZBT token on BSC was produced against a
live Ethereum-class node. The full analysis required:

- `eth_getLogs Transfer(address,address,uint256)` filtered to the ZBT contract over a 24-hour
  window — D05 wash trading
- `eth_call balanceOf(address)` repeated for top-N holders — D03 holder concentration
- `eth_getCode(contract)` and bytecode selector scan for `mint(address,uint256)` — D06 mint
  authority
- `eth_call owner()` view function against the token contract — D02 ownable
- `eth_getLogs Swap` on the PancakeSwap pool address — D04 pump-and-dump, D11 synchronized
  activity, D13 sandwich MEV
- `eth_getLogs PairCreated` from the factory address — D10 pool age

Total: between 5 and 15 RPC calls, completing in seconds. The user's reaction was direct:
"Понял о чем я? типа для этого не надо было все то что жрет столько ГБ и прочего?" (Get my
point? For this we didn't need all that stuff that eats so much GB.) Followed by "Ну да )))"
confirming the pull-based query engine as the correct operational model.

### §2.3 The divergence

The detectors as implemented are query-shaped, not stream-shaped. Every detector in the
current codebase fetches fresh chain state for a specific token when invoked: `eth_getLogs`
filtered to one contract address, `eth_call` against one account, `getSignaturesForAddress`
for one wallet. None of them scan all activity globally and react to incoming events in real
time. The detector logic always was a pull-based query engine. The ingestion architecture
was not aligned with the detector reality.

This ADR corrects the mismatch. It declares the pull-based query engine as the binding
operational model, eliminates the over-engineered ingestion path, and establishes the
correct hardware budget.

---

## §3 Decision

Two binding rules govern the operational model of `mg-onchain-analysis`:

**Rule A — Pull-Based Query Engine.** The service is a query engine triggered by external
signals. It does not continuously ingest all chain activity into storage. When a trigger
arrives, the service fetches the chain state relevant to the specific token under analysis,
runs the relevant detectors, caches the verdict with a TTL, and returns results. The four
trigger types are:

1. REST request — synchronous, single token, returns a scored verdict.
2. WS subscription on a watchlist — detector evaluations are pushed to connected subscribers
   as each token on the watchlist is rescored on a periodic or event-driven basis.
3. Periodic scan worker — an in-process background task that rescores all watchlist tokens
   on a configurable cadence and scans factory programs for newly launched pools at a
   configurable cadence.
4. Batch CLI invocation — offline rescoring of a set of tokens for research, backfill, or
   historical regression.

**Rule B — Standard JSON-RPC + WebSocket, symmetric across chains.** Per-chain ingestion
uses standard JSON-RPC and WebSocket subscriptions against lightweight self-hosted RPC
nodes. The pattern is symmetric: Ethereum already uses `crates/chain-adapter/src/jsonrpc/`
+ `crates/chain-adapter/src/ethereum/` consuming a Reth node via `eth_subscribe` and
`eth_getLogs`; Solana will mirror this pattern using Solana JSON-RPC methods
(`getBlock`, `getSignaturesForAddress`, `getTransaction`, `getAccountInfo`,
`getProgramAccounts`) and WebSocket subscriptions (`programSubscribe`, `accountSubscribe`,
`logsSubscribe`, `signatureSubscribe`) against a standard Agave RPC node. No Yellowstone
gRPC stream. No firehose.

---

## §4 What This Rule Kills

The following are eliminated by this ADR. Sprint 26 executes the removal as described in
`docs/designs/0028-lightweight-query-engine-deployment.md`.

**`crates/yellowstone-proto/`** — the vendored Yellowstone `.proto` files and the
`tonic-build`-generated `GeyserClient` shipped as a Sprint 25 deliverable. Sprint 25's
work was well-executed and demonstrated that we can generate gRPC clients from proto files
without vendor crates. However, the protocol it implemented — the Yellowstone firehose —
is the wrong operational model. The crate becomes dead code the moment the Solana
chain-adapter is rewritten to standard JSON-RPC. It is deleted in Sprint 26 task T26-3.

**Continuous-streaming indexer mode** — `crates/indexer/` currently operates as a
continuous-stream router: events arrive from the chain-adapter subscription stream and are
fanned out to detectors in real time. This mode is replaced by an on-demand worker pool
in Sprint 26. The indexer plumbing (coordinator, worker spawn, event routing) is largely
reused; the trigger source changes from a streaming subscription to explicit invocations.

**The Yellowstone-gRPC ingestion path in `crates/chain-adapter/src/solana/`** — the files
`subscribe.rs`, `reconnect.rs`, and associated gRPC session management are rewritten from
Yellowstone gRPC client calls to standard Solana JSON-RPC + WebSocket. The file structure
is preserved; the internals change to mirror `crates/chain-adapter/src/ethereum/`.

**Validator-class Solana hardware budget** — the ADR 0003 BOM of 256–512 GB RAM / 6 TB
NVMe was calculated for a non-voting RPC node that must hold the full account database in
memory to sustain the Yellowstone stream. An RPC-only node in standard query mode
(no Geyser plugin, no account-update stream) requires approximately 64–128 GB RAM / 2 TB
NVMe. The revised hardware budget is documented in
`docs/designs/0028-lightweight-query-engine-deployment.md` §5.

**The implicit assumption in ADR 0001 §D2** that the service operates as a streaming
pipeline. The wire-protocol portion of §D2 — out-of-process processes communicating over
a standard protocol that we generate clients for from a published schema — is not affected.
The operational model layered on top of it is what this ADR supersedes.

---

## §5 What This Rule Preserves

The following are unaffected by this ADR:

**All 13 detector implementations (D01–D13).** Detector logic was already query-shaped.
D01 honeypot simulation, D02 ownable, D03 holder concentration, D04 pump-and-dump, D05
wash trading, D06 mint authority, D07 withdraw-withheld, D08 Sybil, D09 BOCPD deployer
changepoint, D10 pool age, D11 synchronized activity, D12 Permit2 drainer, D13 sandwich
MEV — all fetch token-specific state on invocation. Their evidence schemas, threshold
configurations, and test fixtures are unchanged.

**`crates/evm-types/`, `crates/evm-types-macros/`, `crates/solana-types/`** — the in-tree
type primitives built in Sprints 24 and 25. These encode the EVM and Solana type systems
against public specifications. They are independent of the operational model and remain
the foundation for both chain adapters.

**`crates/chain-adapter/src/ethereum/`** — already implements the standard JSON-RPC + WS
pattern that Solana will adopt. This crate is the structural template for the Sprint 26
Solana rewrite and is otherwise untouched.

**`crates/chain-adapter/src/jsonrpc/`** — the in-house JSON-RPC 2.0 over WebSocket client
built in Sprint 24. Under the new model it serves both chains. The Solana JSON-RPC wire
format is JSON-RPC 2.0 over WebSocket, identical to Ethereum's; only method names and
parameter shapes differ.

**`crates/detectors/`, `crates/scoring/`, `crates/gateway/`, `crates/storage/`** — business
logic unchanged. The gateway REST and WS surfaces remain on `:8080`. No consumer-facing API
changes result from this ADR. The storage schema requires additions (`verdict_cache` table,
retention policy adjustments) but no destructive changes to existing tables.

**ADR 0001 §D2 wire-protocol portion** — the principle that each chain's data is accessed
via a standard out-of-process wire protocol, with our own client generated from a published
schema, remains in force and is extended: Solana JSON-RPC is as much a public standard wire
protocol as Yellowstone gRPC, and it turns out to be sufficient for our workload.

**ADR 0003 (self-sovereign infrastructure)** — the requirement that all production nodes are
self-hosted remains fully binding. The change is not "use Helius instead"; it is "use a
lighter class of self-hosted node." No 3rd-party SaaS provider enters the hot path.

**ADR 0006 (code-level self-sovereignty, post-amendment)** — the kludge test and the vendor
SDK ban remain binding. The Solana JSON-RPC + WS pattern passes the kludge test trivially:
we speak a public standard protocol over a standard socket to a standard node binary that
we build from source and control. No bridge, no feature flag, no vendor crate linkage.

**Smart-money labelling pipeline** (S22) — the 6-hour batch ticker that scores wallet P&L
and writes `LabelType::SmartMoney` rows is a population-scale background workload that is
already correctly shaped as a periodic batch job. It is the exception that proves the rule:
it processes a large population of wallets, not a single token on demand. It continues
unchanged.

---

## §6 Operational Model Details

### §6.1 Trigger types and data flow

When a trigger arrives, the following sequence executes:

1. The trigger (REST request, WS watchlist update, periodic scan tick, or CLI invocation)
   identifies a specific token address and chain.
2. The indexer worker pool spawns a `DetectorSet::evaluate(token, chain, context)` task.
3. Each active detector performs its RPC calls against the relevant self-hosted node using
   `crates/chain-adapter/src/jsonrpc/` as the transport, fetching only what it needs for
   the specific token.
4. Detector results are aggregated by `crates/scoring/` into a verdict with overall
   confidence and severity.
5. The verdict is written to `verdict_cache` in Postgres with a TTL appropriate to the
   detector class.
6. Any `AnomalyEvent` instances (detectors that fired above their threshold) are written to
   `anomaly_events` for the audit trail.
7. The REST response or WS push delivers the verdict to the requesting consumer.

### §6.2 Per-token deep-dive cost

The ZBT demo established the concrete cost baseline. A full multi-detector evaluation of
one token uses 5–15 RPC calls and completes in seconds of wall time on a co-located
self-hosted node. This means the service can comfortably evaluate dozens of tokens per
minute on a single node without approaching RPC capacity limits. The cost scales linearly
with the number of concurrent token evaluations, which is bounded by the watchlist size and
the periodic scan cadence.

### §6.3 Storage shape

The storage tier holds three categories of data:

- `verdict_cache` — the most recent scored verdict per (token, chain), with TTL. Stale
  verdicts are evicted on TTL expiry or overwritten on re-evaluation. This table does NOT
  accumulate all historical verdicts; it holds the current best estimate.
- `anomaly_events` — every `AnomalyEvent` ever emitted, retained for the audit trail.
  This table grows slowly because anomaly events are rare (only tokens that trigger a
  detector above its threshold) rather than continuous (every transfer from every token).
- `tokens`, `pools`, `address_labels`, `adapter_checkpoints` — metadata tables that already
  exist. They remain unchanged in structure; the indexer populates them as it discovers new
  tokens and pools during periodic scans.

What this storage model deliberately does NOT contain: a bulk pre-store of all transfers
and swaps for all tokens. Bulk event accumulation was a design artefact of the streaming
pipeline model; the query engine model fetches fresh event data from the chain node on each
evaluation and discards it after the detector run. Postgres growth under the new model is
approximately 10 GB/month, not 100+ GB/month.

### §6.4 Periodic scan workers

Two periodic scan workers run as in-process tokio tasks spawned by `crates/server/src/init/`:

**Watchlist rescore worker** — on a configurable cadence (default: every 5 minutes),
rescores every token currently in the watchlist by dispatching a full `DetectorSet::evaluate`.
Results update `verdict_cache`. Anomaly events are persisted if detectors fire.

**New-launch discovery worker** — on a configurable cadence (default: every 5 minutes),
queries factory programs for recently created pools (Raydium v4 factory on Solana, Uniswap
v2/v3 factory on Ethereum, PancakeSwap factory on BSC). Each newly discovered pool whose
token is not yet in the watchlist is added to the watchlist and queued for an initial
evaluation. This is the primary population mechanism for newly launched tokens.

### §6.5 HFT-class detectors

D12 (Permit2 drainer) and D13 (sandwich MEV) are the two detectors most plausibly
associated with time-sensitive patterns. Under the query engine model, they accept
poll-block-by-block latency: 12 seconds on Ethereum mainnet (one block), approximately
400 milliseconds on Solana (one confirmed slot). This is the chain's natural cadence.
Sub-second push semantics are not required because:

- The service is an analytics and scoring layer, not a trade-execution layer. Our consumers'
  bots track execution in real time themselves; our service provides the labelling context.
- D12 detects the structural pattern of a Permit2 drain (spender allowance, transfer-from
  execution) which plays out over multiple transactions spanning minutes to hours, not over
  a single 400ms slot.
- D13 detects MEV sandwich patterns in the context of a token evaluation; identifying that
  a token's users are regularly sandwiched is a scoring input, not a real-time trade alert.

For any future detector where genuine sub-block latency matters, the question must be
revisited with a concrete workload justification and a new ADR sign-off. Accepting 12s /
400ms as the cadence for the current detector set is not a permanent blanket policy; it is
a principled default for the current use cases.

---

## §7 Consequences

### Positive

**Dramatically lower hardware cost.** The Solana ingestion shift from full Yellowstone
validator to standard RPC-only node reduces the Solana node RAM requirement from 512 GB to
64–128 GB. Combined with the already-lightweight Reth node (~32 GB / 2 TB), the full
multi-chain deployment fits on hardware costing $200–400/mo rather than $500–1000+/mo.
The single-box dual-chain topology is now feasible on commodity dedicated servers.

**Symmetric architecture across chains.** Both Ethereum and Solana are accessed via
standard JSON-RPC + WebSocket against self-hosted nodes. The chain-adapter code structure,
connection lifecycle, backfill logic, and retry semantics are symmetric. A developer who
understands one side understands the other. The asymmetry introduced by Yellowstone gRPC
(a separate protocol stack, proto files, tonic client generation) is eliminated.

**Clean kludge-test pass on ingestion.** The Solana JSON-RPC + WS pattern passes the kludge
test unconditionally: it is a standard wire protocol exposed by a standard node binary
(`agave-validator` or `agave-rpc`) that we build from source and run on our own hardware.
No custom bridge, no feature flag, no vendor crate linkage. The Yellowstone streaming
pipeline, in retrospect, was over-engineered relative to what the detector workload
actually needed — the kludge-test logic that eliminated the exex-bridge plan applies here
symmetrically.

**Storage growth bounded.** Abandoning the bulk event pre-store eliminates the primary
driver of Postgres growth. The `verdict_cache` + `anomaly_events` model keeps the data
tier lean: verdicts are short-lived (TTL eviction), anomaly events are rare by design.

**Simpler reasoning about detector correctness.** When a detector evaluation always reads
fresh state from the chain at query time, there is no stale-data question about whether
the pre-stored events reflect the current chain state. The query is the source of truth.
Reproducibility is preserved: given the same block range, the same RPC calls return the
same data (modulo reorgs handled at the chain-adapter layer).

### Negative

**Sprint 25's `crates/yellowstone-proto/` becomes dead code.** The Sprint 25 investment in
proto vendoring and tonic-build-generated client code is discarded. The technical lessons
from that work — how `tonic-prost-build` generates client stubs from `.proto` files, how to
structure a vendored-proto crate — are recorded in the sprint history and remain useful for
any future gRPC integration. The crate itself, once deleted, reduces the workspace
dependency count by two crates (`tonic-prost-build` build-dep and the `tonic-prost` runtime
split). Sunk cost; lessons retained.

**Rewrite of `crates/chain-adapter/src/solana/`** — approximately one sprint of
implementation work. The Yellowstone gRPC subscription management (session establishment,
filter encoding, reconnect loop, slot-update handling) is replaced by Solana JSON-RPC
method calls and WebSocket subscription management. The Ethereum adapter is the template;
the LOC delta is bounded and the problem is well-understood.

**No sub-block event delivery for the periodic scan cadence.** The new-launch discovery
worker fires every N minutes. A token that launches on Raydium between two scan ticks is
not detected until the next tick. For the current use case (shitcoin anomaly scoring, not
HFT execution) this is acceptable. The ZBT demo confirmed that anomaly patterns manifest
over minutes to days, not sub-second intervals.

### Neutral

All 13 detector implementations are unchanged. Consumer-facing API surfaces (REST, WS,
client-sdk) are unchanged. The migration is entirely internal to the ingestion layer. A
consumer that integrated against the current API surface will continue to work without
modification after Sprint 26.

The tonic and prost crates remain in the workspace. They are used for the OpenTelemetry
OTLP exporter (admitted under ADR 0006 Rule A; see `docs/designs/0028-...` §10), not for
Yellowstone. The net workspace dependency delta after Sprint 26 is approximately zero: two
yellowstone-specific crates removed, OpenTelemetry stack added.

---

## §8 Migration Plan Summary

Sprint 26 executes the transition as a single-sprint atomic changeset. The detailed task
decomposition lives in `docs/designs/0028-lightweight-query-engine-deployment.md` §12.
At a high level:

1. Extend `crates/chain-adapter/src/jsonrpc/` to support Solana JSON-RPC method names and
   parameter shapes alongside Ethereum (or introduce a parallel Solana JSON-RPC module).
2. Rewrite `crates/chain-adapter/src/solana/{subscribe,reconnect,config,mod}.rs` from
   Yellowstone gRPC to standard Solana JSON-RPC + WebSocket.
3. Delete `crates/yellowstone-proto/` and remove `tonic-prost-build` + `tonic-prost` from
   the workspace.
4. Mode-shift `crates/indexer/` from continuous-stream consumer to on-demand worker pool.
5. Add `verdict_cache` table and storage retention policies (migration V00017).
6. Add `/score?token=X` REST endpoint and watchlist WS subscription to `crates/gateway/`.
7. Wire OTLP, `/health`, and `/metrics` (preserved from design 0027, still needed).
8. Write testcontainers integration test and `infra/docker-compose.prod.yml`.
9. Operator runbook in `infra/PRODUCTION.md` reflecting lightweight topology.

---

## §9 Sign-Off Decisions

The following five decisions require explicit user confirmation. Each carries a
recommendation with rationale.

**§9.1 Operational model name:** "Pull-Based Query Engine" vs "On-Demand Analysis Engine"
vs "Triggered Detector Service."

Recommendation: **Pull-Based Query Engine.** This names the architectural pattern (a system
that executes queries in response to pull triggers, rather than pushing events to
subscribers continuously), not the implementation detail. "On-Demand Analysis Engine" is
accurate but vaguer. "Triggered Detector Service" overweights the detector framing and
underweights the query architecture. "Pull-Based Query Engine" is the term that most
precisely describes what distinguishes this model from a streaming pipeline, and it will
be unambiguous to any future engineer reading this ADR alongside design 0028.

**§9.2 ADR form: ADR 0007 vs amendment to ADR 0001 §D2.**

Recommendation: **Separate ADR 0007.** ADR 0001 §D2 decided the wire-protocol choice
(Yellowstone gRPC as the provider-agnostic Solana protocol). That decision remains
relevant and correct for its scope. Adding an operational-model supersession to ADR 0001
would conflate two distinct decisions and make ADR 0001 harder to read in future. A
dedicated ADR 0007 that clearly states "supersedes the implicit operational-model
assumption of §D2, preserves the wire-protocol principle" is cleaner. The relationship
is documented in both documents.

**§9.3 HFT-class detector latency policy:** poll-every-block vs poll-every-15-seconds vs
accept-eventual-consistency for D12 and D13.

Recommendation: **poll-every-block** — 12 seconds on Ethereum mainnet, approximately
400 ms on Solana. The rationale is in §6.5: D12 and D13 detect structural patterns over
multiple blocks, not individual transactions that require sub-block reaction. Polling at
the natural chain cadence (one poll per new block) is as fast as the data becomes
canonically available on a non-archive node. A 15-second poll interval for Ethereum would
occasionally miss a block under normal load; polling every block is the correct granularity.
Solana's ~400ms slot time means "every block" is already very fast; no special case needed.

**§9.4 Periodic scan cadence default:** the "scan factory programs for new pool launches"
and "rescore watchlist" workers need a default cadence.

Recommendation: **N = 5 minutes** for both workers as the configurable default in
`config/service.toml`. This provides a maximum 5-minute latency from launch to first
detection, which is acceptable for anomaly scoring of newly launched shitcoins (the first
few minutes of a token's life are dominated by LP creation, not yet anomalous trading
patterns). Operators with a narrow watchlist can reduce to 1 minute; operators monitoring
a large watchlist on lightweight hardware can increase to 15 minutes. The value is in
`config/`, not hardcoded.

**§9.5 Verdict cache TTL default classes:** how stale can a cached verdict be before it
is treated as expired and re-evaluated on the next trigger?

Recommendation: **two TTL classes based on detector signal velocity:**

- Fast-moving signals (D04 pump-and-dump, D11 synchronized activity, D05 wash trading,
  D13 sandwich MEV): **5-minute TTL.** These signals can change materially within a single
  candle. A 5-minute-stale verdict is still actionable; a 1-hour-stale verdict for a
  pump-in-progress is not.
- Slow-moving signals (D02 ownable, D03 holder concentration, D06 mint authority, D07
  withdraw-withheld, D08 Sybil, D09 BOCPD deployer changepoint, D10 pool age, D12 Permit2
  drainer): **1-hour TTL.** Ownership transfers, concentration shifts, and token contract
  properties evolve over hours to days. Re-evaluating every 5 minutes would waste RPC quota
  with no signal improvement.
- D01 honeypot simulation: **15-minute TTL.** Honeypot contracts can be modified to
  selectively revert sells at the bytecode level; 15 minutes reflects a reasonable check
  cadence between the fast and slow classes.

TTL values are defined in `config/detectors.toml` per detector, with the defaults above.
Operators can override any individual TTL without code changes.

---

## §10 References

| # | Source | Claim grounded |
|---|--------|----------------|
| 1 | `docs/adr/0001-phase0-synthesis.md` §D2 | Yellowstone gRPC as out-of-process bridge pattern; wire-protocol principle preserved; operational-model assumption superseded |
| 2 | `docs/adr/0003-self-sovereign-infrastructure.md` | Self-sovereign infrastructure requirement; hardware BOM that this ADR revises downward |
| 3 | `docs/adr/0006-code-level-self-sovereignty.md` (post-amendment) | Kludge test; wire-protocol-only integration requirement; no vendor SDK in main workspace |
| 4 | `memory/feedback_query_engine_model.md` | Binding user directive 2026-04-27; ZBT demo data (5–15 RPC calls per token); hardware BOM revision (64–128 GB Solana RPC-only); two TTL classes |
| 5 | `memory/feedback_kludge_test.md` | Kludge test applied to Yellowstone firehose pattern: standard wire-protocol integration is OK; custom streaming pipeline for workload that only needs queries is a kludge |
| 6 | ZBT-on-BSC anomaly demo, 2026-04-27 | Concrete proof of workload shape: 6 RPC method types, 5–15 calls total, seconds wall-time, one token at a time |
| 7 | `crates/chain-adapter/src/ethereum/` | Exemplar of standard JSON-RPC + WS pattern; structural template for Solana rewrite |
| 8 | `crates/chain-adapter/src/jsonrpc/` | In-tree JSON-RPC 2.0 over WebSocket client; used by both chains under the new model |
| 9 | `docs/designs/0028-lightweight-query-engine-deployment.md` | Sprint 26 implementation plan for this ADR; hardware topologies; task decomposition |
| 10 | Solana JSON-RPC reference — https://solana.com/docs/rpc | Public standard wire protocol replacing Yellowstone for standard RPC queries |
| 11 | Solana WebSocket subscriptions — https://solana.com/docs/rpc/websocket | `programSubscribe`, `accountSubscribe`, `logsSubscribe`, `signatureSubscribe` |
| 12 | Ethereum JSON-RPC specification — https://ethereum.github.io/execution-apis/api-documentation/ | `eth_subscribe`, `eth_getLogs`, `eth_call` — the public standard already used by the Ethereum adapter |
