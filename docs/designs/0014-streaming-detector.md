# Design 0014 — Streaming Detector Mode

**Date:** 2026-04-22 (rev 1: 2026-04-24, main-session review pass)
**Status:** Draft — rev 1 addresses 10 review points (concurrency correctness, determinism, evidence conventions, perf-lever scope, failure-mode specificity)
**Author:** architect agent + main-session review
**Resolves:** OQ "streaming mode" deferred in `docs/designs/0003-detector-trait.md` §Structural Decision #2
**Sprint:** 8, Track A (P6-5)
**ADR impact:** No ADR produced — `Detector` trait is UNCHANGED (see §2.1). Phase 4 multi-process upgrade (§8) is deferred to a FUTURE ADR (candidate 0005+), not pre-committed to NATS.

**Related:**
- `docs/designs/0003-detector-trait.md` — `Detector` trait, `DetectorContext`
- `docs/adr/0002-drop-clickhouse-postgres-only.md` — Postgres only
- `docs/adr/0003-self-sovereign-infrastructure.md` — no SaaS pub/sub
- `crates/indexer/src/lib.rs` — existing pipeline, backpressure model
- `crates/gateway/src/ws/mod.rs` — WS polling model (currently DB poll at 500ms)
- `crates/gateway/src/state.rs` — `invalidation_tx: broadcast::Sender<InvalidationEvent>`

---

## §1 Context

### What streaming is

Seven detectors (D01-D07) ship in Sprint 7 with an **on-demand invocation model**: the gateway's `POST /v1/tokens/analyze` endpoint constructs a `DetectorContext` for a fixed window and runs all detectors once, synchronously, before returning. The result is cached in `RiskCache`.

The on-demand model satisfies custody and exchange (infrequent, REST-driven, batch cadence). It does not satisfy:

- **Trading bot** — wants a D04 (pump/dump) or D05 (wash-trading) re-score within seconds of a new burst of trades, not on next API call. A 15-minute cache TTL can mean entering a position during the dump phase.
- **Market maker** — wants per-pool LP event signals (D02) pushed as soon as a pool event is indexed, not polled.
- **High-TPS tokens** — pump.fun launches 1000+ tokens per day; many die in under an hour. The risk window is narrow. On-demand re-scores on stale cache miss the rug.

"Streaming Detector mode" is the system by which **the indexer's event flow triggers detector re-evaluation** for affected tokens, persists fresh `AnomalyEvent` rows to Postgres, and WS consumers receive them within seconds via the existing `anomaly_events` poll loop.

### What streaming is NOT

- Order execution — no trading decisions here.
- MEV — no mempool racing.
- A replacement for on-demand REST — REST stays as the primary interface for custody and exchange. Streaming is an event-driven supplement.
- A push notification system — WS consumers still poll Postgres (`anomaly_events` table) at configurable intervals. The streaming scheduler's only output is Postgres rows. This is intentional: it preserves the determinism and replay properties of the existing system.

### Why this design is possible now

The indexer already lands batched events in Postgres and fires `invalidation_tx` (a `broadcast::Sender<InvalidationEvent>`, currently subscribed only by the WS placeholder arm). The streaming scheduler can subscribe to this same channel and become the primary consumer. No new IPC mechanism is required.

---

## §2 Five OQ Resolutions

### §2.1 OQ1 — `Detector` trait shape: (d) event-driven fan-out, stateless

**Decision: option (d). The `Detector` trait is UNCHANGED.**

The indexer fires events to a `DetectorScheduler`. The scheduler identifies affected tokens, builds `DetectorContext` windows, and calls the existing `detector.evaluate(ctx).await` on each. Detectors remain stateless; the scheduling layer performs all orchestration.

**Reasoning against the alternatives:**

- **(a) `evaluate_stream` on every detector** — requires migrating all 7 detectors, adding a method that most (D03 concentration, D07 withdraw-withheld) have no use for. D03 is a snapshot detector that has no marginal value from a streaming variant; calling `evaluate()` on the current window is already correct.
- **(b) Sibling `StreamingDetector` trait** — cleaner than (a) but still requires new trait impls for D04/D05/D06 and a two-codepath scheduler. If "streaming D04" just means "call evaluate() on the latest window", the trait adds ceremony without capability.
- **(c) `evaluate_incremental(prev_result, new_events)`** — requires detectors to carry state between calls. State that persists between calls breaks the CLAUDE.md determinism invariant: the same event replayed from a different starting state produces different output. Ruled out.
- **(d) Scheduler calls `evaluate()` on current window** — detectors produce identical output for identical window + DB state, regardless of whether they were invoked by the scheduler or by a REST call. Determinism is preserved. No per-detector migration. The fixture/test story is unchanged: tests call `evaluate()` with a canned context and canned rows.

**No ADR 0004 is required** — the trait shape does not change. This document IS the record of the decision.

---

### §2.2 OQ2 — Scheduling model: (B) single queue + event-to-token router + bounded worker pool

**Decision: option (B) with a debounce layer from (D).**

**Model:**

```
invalidation_tx (broadcast::Sender<InvalidationEvent>)
        |
        v
DetectorScheduler::run() — subscribes to invalidation_tx
        |
        | groups events by (chain, mint) over a debounce_window_ms
        v
SchedulerQueue: bounded mpsc channel of (Chain, Mint, Vec<slot_hint>)
  capacity: queue_capacity (config, default 4096)
        |
        v
Worker pool: N = tokio::available_parallelism() * 2 tasks
  each worker: pulls (chain, mint), builds DetectorContext,
               calls each detector, persists AnomalyEvents to PgStore
```

**Debounce layer:** a token entering the queue is not re-queued if it is already present. The scheduler accumulates slot hints for the same (chain, mint) over `debounce_window_ms` (config, default 500ms) before enqueuing. This collapses a burst of 50 transfers for one token into a single worker invocation covering the full burst.

**Rationale for bounding:**

Pump.fun creates ~1000 tokens/day. At peak, 200 may be "hot" simultaneously (active trade flow). With a 500ms debounce and 500ms poll on the indexer side, each hot token re-evaluates at most every 500ms. At N=8 workers and ~200ms per evaluation (Postgres query + scoring), throughput is ~2400 re-evals/minute — enough for 200 hot tokens at 1 re-eval/min each with headroom.

**Latency targets (per consumer SLO):**

| Consumer | Target freshness | Source |
|----------|-----------------|--------|
| Trading bot | p95 re-score within 5s of new event | Sprint 8 exit criterion (SESSION-KICKOFF.md) |
| Market maker | p95 LP event → WS push within 10s | Design estimate; recalibrate after first load test |
| Custody / Exchange | Batch mode; 15-minute cache acceptable | Current REST model — no change |

**Calibration status (rev 1).** The per-evaluation cost of "~200ms" in the
original draft is an architect estimate, NOT a measurement. Real numbers
exist in `crates/indexer/tests/sprint5_exit_test.rs` (per-detector latencies
after the full analyze pipeline runs against testcontainers Postgres).
**Phase 1 smoke test MUST:**
1. Read per-detector p50/p95 from `sprint5_exit_test` + today's
   `sprint7_exit_test` (to be added in Track C).
2. Sum them with a 20% safety margin to derive the actual per-evaluation
   budget.
3. Re-derive `queue_capacity` and `worker_count` from that budget against
   the 5s p95 bot SLO.
4. If the budget exceeds 1s per evaluation, narrow the streaming detector
   set (per-detector opt-in via `StreamingDetectorSet`, not "all seven").

Until Phase 1 smoke test lands, treat the 500ms debounce + ~200ms eval +
500ms WS poll = ~1.2s best-case as **unverified**. The p95 5s SLO is tight;
the p99 tail under queue-saturation load is likely 7-10s and needs
measurement, not modelling.

**Option (A) rejected:** O(active_tokens) tasks is unbounded. 7000 tasks after a week of pump.fun activity is not acceptable without aggressive eviction — and eviction adds the same complexity as option (B)'s queue.

**Option (C) rejected:** two code paths (hot/cold) increase implementation surface and add operational complexity before there is data showing hot/cold distinction matters. (B) with debounce achieves natural prioritization: a token that receives more events simply gets debounced more aggressively but still re-evaluates promptly.

---

### §2.3 OQ3 — Eviction policy: (c) idle timeout AND no active subscription, with LRU cap

**Decision:**

A token enters the streaming set when the `DetectorScheduler` receives an `InvalidationEvent` carrying its mint. This is automatic — no operator action required.

A token exits the streaming set when BOTH of these hold:
- No `InvalidationEvent` has been received for it in `streaming_idle_timeout_minutes` (config, default 60 minutes).
- Zero WS consumers have an active subscription filter covering that token.

The WS subscription count is maintained in a shared `Arc<RwLock<StreamingRegistry>>` described in §4. A token with no subscribers and no recent events is evicted from the registry on the next scheduler gc cycle (runs every `gc_interval_seconds`, default 120s).

**Cap:** `max_streaming_tokens` (config, default 5000). When the cap is reached, the LRU token (oldest last-event time) is evicted to make room. A metric `streaming_tokens_evicted_total` is incremented. The evicted token can re-enter on the next event.

**Option (d) "never evict" rejected:** unbounded memory growth and worker contention on dead tokens.

---

### §2.4 OQ4 — Backpressure: (d) per-consumer disconnect + (e) batch-dedup

**Decision: (d) + (e), layered.**

**Server-side backpressure toward slow WS consumers:**

Each WS consumer has a bounded `mpsc::Sender<serde_json::Value>` with capacity `send_buffer_capacity` (config, default 256, already in `ws.rs`). When `try_send` returns `Full`, the existing `lag_notice` is sent and the oldest buffered event is dropped for that consumer. No other consumer is affected. This is already implemented; streaming adds no new mechanism.

**Within the scheduler — batch-dedup (option e):**

Before enqueueing a `(chain, mint)` pair, the scheduler checks whether a re-evaluation is already queued for that pair. If yes, it merges the slot hints into the existing queue entry rather than adding a duplicate. This ensures that a fast-publishing token never accumulates more than one pending evaluation per (chain, mint) in the queue at a time.

**Server-side backpressure toward the indexer:**

The scheduler's queue is bounded (`queue_capacity`, default 4096). When the queue is full, new `InvalidationEvent`s are dropped at the scheduler's `broadcast::Receiver`. The `streaming_queue_overflow_total` counter is incremented. This is intentional: dropping a re-evaluation tick is safe (the next event will trigger another tick); blocking the indexer loop is not safe (it would stall event ingestion globally).

The indexer already handles its own Postgres backpressure by blocking on `sink.insert_*()` — this model is not changed.

**Scoring recomputation threshold (rev 1 — promoted to Phase 1):**

`TokenRiskReport` is recomputed by `ScoringEngine::score()` on every worker invocation — but only if per-detector confidence delta exceeds `scoring_skip_delta_threshold` (config, default `0.05`). The scoring engine itself runs in ~1ms; the expensive part is the downstream `risk_cache.insert` + `upsert_token_risk_report` Postgres write. At 400 evaluations/sec sustained (200 hot tokens × 500ms debounce), skipping those downstream writes for "no material change" ticks cuts ~90% of the write load after first evaluation per token. This is a Phase 1 ship requirement, not a follow-up. See §2.5 "Inter-tick scoring optimization" for the concrete implementation.

---

### §2.5 OQ5 — Integration surface

**Indexer → scheduler:** `broadcast::Sender<InvalidationEvent>` already in `AppState`. The `DetectorScheduler` subscribes at startup via `invalidation_tx.subscribe()`. No new IPC, no Redpanda, no NATS. This is the self-sovereign choice: tokio channels are sufficient for single-process deployment. The upgrade path to multi-process is described in §8.

**Scheduler → gateway WS:** The scheduler persists `AnomalyEvent` rows to `anomaly_events` (Postgres). The WS poll loop at 500ms picks them up via `fetch_anomaly_events_paginated`. No new channel between scheduler and WS handler. This preserves the existing replay semantics: a consumer that reconnects can replay from `resume_from` and will see streaming-generated events identically to on-demand events.

**Scoring recomputation:** runs on every worker tick unconditionally. `TokenRiskReport` result is upserted to `risk_cache` (in-memory) and optionally persisted to `token_risk_reports` (Postgres) for historical tracking. The WS broadcast arm (`Ok(_inval) = broadcast_rx.recv()`) is upgraded from a no-op placeholder to push fresh `TokenRiskReport` JSON to subscribed consumers without waiting for the 500ms poll tick.

**Persistence story:** streaming tick results ARE persisted. Every `AnomalyEvent` from a streaming evaluation goes to `anomaly_events` exactly like on-demand events.

**Provenance tag (rev 1 — placement corrected).** Distinguishing a streaming-emitted event from an on-demand-emitted one MUST live on the `AnomalyEvent` envelope, NOT in the `evidence` map. CLAUDE.md Gotcha #9 binds: "Detector evidence keys prefixed by detector_id". A top-level `source` evidence key violates that convention and pollutes detector-owned namespaces.

Two acceptable placements (Phase 1 implementation picks one — needs `crates/common` review since `AnomalyEvent` lives there):
- **(a) New `AnomalyEvent.source: AnomalyEventSource` enum** — `enum AnomalyEventSource { ApiRequest, StreamingScheduler }`. Cleanest, but `crates/common` is FROZEN — requires a pre-authorised addition.
- **(b) `meta.source` field via existing event metadata bag** — if `AnomalyEvent` already has a metadata HashMap (check), use it without touching the frozen schema.
- **(c) Per-row tag on `anomaly_events` Postgres table only** — schema gains `emitted_by` column, no Rust type change. Storage-layer-only diff. Probably the smallest blast radius.

Phase 1 default: **(c)** — schema-only addition via a new migration. Type stays frozen. Querying for streaming-emitted events filters on `WHERE emitted_by = 'streaming_scheduler'`. Re-evaluate (a)/(b) only if Rust callers (e.g. SDK consumers) need the distinction.

**Inter-tick scoring optimization (rev 1 — promoted from §9 to Phase 1).** A `confidence_delta_threshold` skip on `ScoringEngine::score()` is the primary perf lever, NOT a follow-up. Justification: at 200 hot tokens × 500ms debounce = 400 evaluations/sec sustained, even a fast scoring run (~1ms) plus the unconditional `risk_cache.insert` + `upsert_token_risk_report` write becomes the dominant cost after the Postgres event-window query. Phase 1 ships:

- `WorkerCache` per worker keeps `HashMap<(Chain, Mint), Vec<f32>>` of last per-detector scores.
- After detectors run, compute `delta = max(|new[i] - prev[i]|)`. If `delta < scoring_skip_delta_threshold` (config, default `0.05`), skip the `score()` + `risk_cache.insert` + `upsert_token_risk_report` calls. The `AnomalyEvent` rows still persist (events ≠ score).
- Metric: `streaming_score_skipped_total{reason}` (`reason ∈ {"below_delta", "first_evaluation", "manual_force"}`).

This is Phase 1 (with detectors), not Phase 2 (scaling). Without it, Phase 1 ships with a known perf cliff.

---

## §3 Data Flow Diagram

```
                       Self-hosted Solana validator
                              |
                              | Yellowstone gRPC
                              v
                       ChainAdapter::subscribe()
                              |
                              v
                   [crates/indexer] Indexer::run()
                    EventBatcher → PgEventSink
                              |
                    +---------+---------+
                    |                   |
                    v                   v
             PgStore::insert_*()   invalidation_tx.send(InvalidationEvent)
             (anomaly_events,           |
              transfers, swaps)         |
                    |                   |
                    |          +--------+--------+
                    |          |                 |
                    |          v                 v
                    |   [crates/server]    [crates/gateway]
                    |   DetectorScheduler  WS invalidation arm
                    |   ::run()            (currently no-op → upgraded)
                    |          |
                    |   debounce_window_ms
                    |          |
                    |   SchedulerQueue (bounded mpsc, capacity=4096)
                    |          |
                    |   Worker pool (N = parallelism * 2)
                    |   for each (chain, mint):
                    |     1. Build DetectorContext (window = last N minutes)
                    |     2. Call D01..D07 evaluate()  <-- TRAIT UNCHANGED
                    |     3. ScoringEngine::score()
                    |     4. PgStore::insert_anomaly_events()
                    |     5. PgStore::upsert_token_risk_report()
                    |     6. risk_cache.insert()
                    |     7. invalidation_tx.send() [re-broadcast for WS arm]
                    |
                    |
                    v
            [crates/gateway] GET /v1/ws/stream
            poll_ticker (500ms) -> fetch_anomaly_events_paginated
            OR
            invalidation_tx arm (immediate push of TokenRiskReport delta)
                    |
                    v
            WS consumer (bot, MM, exchange, custody)
```

---

## §4 Interface Definitions (Pseudo-code)

All items below are design-level pseudo-code. No `.rs` files are touched by this document.

### `StreamingRegistry` — tracks which tokens are being streamed

```
struct StreamingState {
    last_event_at: DateTime<Utc>,
    subscriber_count: u32,
}

struct StreamingRegistry {
    // BTreeMap for deterministic iteration order in gc pass.
    tokens: BTreeMap<(Chain, Mint), StreamingState>,
    max_tokens: usize,
    idle_timeout: Duration,
}

impl StreamingRegistry {
    fn on_event(&mut self, chain: Chain, mint: Mint, now: DateTime<Utc>);
    fn on_subscribe(&mut self, chain: Chain, mint: Mint);
    fn on_unsubscribe(&mut self, chain: Chain, mint: Mint);
    fn gc(&mut self, now: DateTime<Utc>) -> Vec<(Chain, Mint)>;  // returns evicted
    fn is_active(&self, chain: &Chain, mint: &Mint) -> bool;
    fn len(&self) -> usize;
}
```

Wrapped in `Arc<tokio::sync::RwLock<StreamingRegistry>>` and stored in `AppState`.

### `DetectorScheduler` — top-level streaming orchestrator

```
struct DetectorScheduler {
    invalidation_rx: broadcast::Receiver<InvalidationEvent>,
    queue_tx: mpsc::Sender<SchedulerJob>,
    registry: Arc<RwLock<StreamingRegistry>>,
    debounce_window: Duration,
    config: SchedulerConfig,
}

struct SchedulerJob {
    chain: Chain,
    mint: Address,
    observed_at: DateTime<Utc>,   // set once per job; passed into DetectorContext
    slot_hints: Vec<u64>,
}

impl DetectorScheduler {
    async fn run(self);  // consumes self; spawned as tokio task in crates/server
}
```

Internally: the scheduler maintains a `BTreeMap<(Chain, Mint), PendingJob>` for debouncing. On each `InvalidationEvent`, it merges into the pending map. A separate ticker fires every `debounce_window_ms` and drains the pending map into `queue_tx`.

### `SchedulerWorker` — one unit of the worker pool

**Concurrency model (rev 1 correction):** `Arc<Mutex<mpsc::Receiver>>` is a
tokio antipattern — a single mutex around the receiver serializes all N
workers and eliminates parallelism. Two acceptable patterns:

- **(preferred)** `async-channel` crate — a native MPMC channel where every
  worker holds its own `Receiver` clone; no mutex, natural work-stealing.
- **(fallback)** Single dispatcher task owns `mpsc::Receiver<SchedulerJob>`
  + holds `Vec<mpsc::Sender<SchedulerJob>>` (one per worker); dispatches
  round-robin or to the least-loaded worker. Still no shared mutex.

Phase 1 ships the `async-channel` variant unless a clippy / dep-policy
concern surfaces during implementation.

```
struct SchedulerWorker {
    queue_rx: async_channel::Receiver<SchedulerJob>,    // MPMC, cheap to clone
    store: PgStore,
    registry: TokenRegistry,
    detector_config: DetectorConfig,
    scoring: ScoringEngine,
    risk_cache: Arc<RiskCache>,
    invalidation_tx: broadcast::Sender<InvalidationEvent>,
    detectors: Vec<Arc<dyn Detector>>,                  // async-trait, project convention
    metrics: StreamingMetrics,
    window_minutes: u32,                                // from SchedulerConfig; default 60
    per_detector_timeout: Duration,                     // from SchedulerConfig; default 3s
}

impl SchedulerWorker {
    async fn run(self);  // consumes self; one task per worker
    async fn evaluate_token(&self, job: SchedulerJob) -> anyhow::Result<()>;
}
```

**Per-detector timeout (rev 1 addition).** A single slow detector (e.g. a
stuck Postgres query in D07 during an index-rebuild window) must NOT stall
the worker and reduce effective parallelism to N-1. Inside `evaluate_token`,
each detector call is wrapped:

```
let per_det = self.per_detector_timeout;
for det in &self.detectors {
    match tokio::time::timeout(per_det, det.evaluate(&ctx)).await {
        Ok(Ok(events)) => collected.extend(events),
        Ok(Err(e))     => self.metrics.streaming_evaluations_total
                              .with_label_values(&[chain, "error"]).inc(),
        Err(_elapsed)  => self.metrics.streaming_evaluations_total
                              .with_label_values(&[chain, "timeout"]).inc(),
    }
}
```

**Trait erasure (rev 1 reword).** `Vec<Arc<dyn Detector>>` uses the existing
`async-trait` pattern already adopted for `SolanaRpc` and `PoolAccountProvider`.
No new `ErasedDetector` trait is invented — the scheduler reuses the project
convention. The gateway's `analyze.rs` currently hand-writes seven concrete
detector types in a `tokio::join!` macro; it is not a migration target and
stays as-is. The scheduler is the first site that needs a dynamic detector
collection, and `Vec<Arc<dyn Detector>>` is the correct idiom.

### `SchedulerConfig` — extends `config/service.toml`

```toml
[streaming]
enabled                       = true
debounce_window_ms            = 500
queue_capacity                = 4096
worker_count                  = 0          # 0 = tokio::available_parallelism() * 2
window_minutes                = 60         # DetectorContext window per streaming tick
gc_interval_seconds           = 120
max_streaming_tokens          = 5000
streaming_idle_timeout_minutes = 60
per_detector_timeout_ms       = 3000       # rev 1: per-detector timeout in worker loop
scoring_skip_delta_threshold  = 0.05       # rev 1: skip score() + writes when per-detector delta < this
```

These join the service config, not `config/detectors.toml` (detectors have no scheduling concern).

---

## §5 Metrics

New metrics follow the naming pattern in `crates/gateway/src/metrics.rs`. They are registered in a new `StreamingMetrics` struct owned by `crates/server` (not `crates/gateway`, because the scheduler lives in the server binary).

| Metric name | Type | Labels | Meaning |
|-------------|------|--------|---------|
| `streaming_tokens_active` | Gauge | — | Current tokens in StreamingRegistry |
| `streaming_tokens_evicted_total` | Counter | `reason` ("idle", "cap") | Evictions from the registry |
| `streaming_queue_depth` | Gauge | — | Current SchedulerQueue depth |
| `streaming_queue_overflow_total` | Counter | — | Jobs dropped due to full queue |
| `streaming_evaluations_total` | Counter | `chain`, `outcome` ("ok", "error") | Worker evaluations |
| `streaming_evaluation_duration_seconds` | Histogram | `chain` | Time per token evaluation (all detectors + scoring) |
| `streaming_anomaly_events_persisted_total` | Counter | `chain`, `detector_id` | AnomalyEvents written to Postgres from streaming path |
| `streaming_debounce_merge_total` | Counter | — | Events merged into existing pending job (debounce hit) |
| `streaming_worker_idle_seconds` | Histogram | — | Time workers spend waiting for jobs |

Buckets for `streaming_evaluation_duration_seconds`: [0.050, 0.100, 0.200, 0.500, 1.0, 2.0, 5.0] — p99 target < 2s.

---

## §6 Failure Modes

### Slow WS consumer

The consumer's `mpsc` channel fills. `try_send` returns `Full`. The server drops the event for that consumer and increments `ws_lag_notices_total`. After `lag_notice_threshold` drops, it sends a `lag_notice` frame and resets the drop counter. If the consumer never drains (permanently slow), the heartbeat timeout fires and the connection closes. Other consumers are entirely unaffected — per-consumer isolation is the design (OQ4 decision d).

### Slow indexer (Postgres lag)

The indexer's `insert_*()` await blocks the subscribe loop, which stalls Yellowstone gRPC pull, which applies natural backpressure to the validator stream. `invalidation_tx` events stop arriving. The scheduler's debounce map drains; the queue eventually empties. Workers idle (recorded in `streaming_worker_idle_seconds`). When Postgres recovers, the backlog of events floods through. The queue fills; overflow is dropped (`streaming_queue_overflow_total` fires). This is the correct trade-off: no unbounded buffer, no OOM risk.

### Detector panic

Worker tasks are spawned via `tokio::spawn`. A panic inside a single
detector's `evaluate()` future must not kill the worker task — otherwise
one bad detector run takes a whole worker offline until restart. The
isolation pattern (rev 1 — spelled out, not glossed):

```
use std::panic::AssertUnwindSafe;
use futures::FutureExt;            // for .catch_unwind()

let det_fut = AssertUnwindSafe(det.evaluate(&ctx)).catch_unwind();
match tokio::time::timeout(per_det, det_fut).await {
    Ok(Ok(Ok(events))) => collected.extend(events),                       // happy path
    Ok(Ok(Err(e)))     => metrics.streaming_evaluations_total
                              .with_label_values(&[chain, "error"]).inc(), // detector returned Err
    Ok(Err(panic))     => {                                                // panic caught
        metrics.streaming_evaluations_total
            .with_label_values(&[chain, "panic"]).inc();
        tracing::error!(detector_id = %det.id(), panic = ?panic,
                        "detector panicked, isolated by catch_unwind");
    }
    Err(_elapsed) => metrics.streaming_evaluations_total
                         .with_label_values(&[chain, "timeout"]).inc(),
}
```

`AssertUnwindSafe` is required because most futures are not auto-`UnwindSafe`
(detector futures borrow from `DetectorContext` by reference). The assertion
is sound here: the future captures only `&ctx` (immutable) and config (also
immutable); even if a panic mid-future leaves a partial result on the stack,
no shared mutable state is corrupted.

`futures::FutureExt::catch_unwind` requires the `futures` crate (already a
workspace dep via `axum`); no new dep. Worker continues with the next
detector in the loop. The panicking token is NOT re-queued immediately —
it will re-enter the queue on the next genuine `InvalidationEvent` (or the
scheduler's GC tick). This avoids a tight panic-loop on a token that's in
a structurally bad state.

### Scheduler deadlock

The scheduler is a single async task consuming from `broadcast::Receiver<InvalidationEvent>`. Deadlock requires the scheduler to hold a lock while awaiting the `invalidation_tx` — this cannot occur if the debounce map and `StreamingRegistry` RwLock are held only within synchronous scopes (no `.await` while locked). The implementation MUST follow: acquire lock → compute → release → then await. The design enforces this via the `Arc<RwLock<StreamingRegistry>>` pattern where all lock guards are dropped before any `.await`.

### Channel exhaustion (broadcast lag)

`invalidation_tx` is a `broadcast::Sender` with capacity 1024 (already in `AppState`). A lagging `broadcast::Receiver` (the scheduler) that falls behind by 1024 events receives `RecvError::Lagged(n)` instead of events. The scheduler MUST handle this variant: log `streaming_queue_overflow_total += n`, then continue receiving. Events in the lag gap are not processed for that batch; the next event will trigger a fresh re-evaluation of the tokens that had events in the gap.

---

## §7 Test Strategy

### Determinism in async context

The streaming path must be testable deterministically despite async. The key insight: the scheduler's output is Postgres rows, and Postgres rows are deterministic given identical inputs. Tests follow this pattern:

1. Seed `anomaly_events` with zero rows.
2. Construct a `SchedulerJob` with a fixed `observed_at`.
3. Call `SchedulerWorker::evaluate_token(job)` directly (bypass the channel; call the method).
4. Assert `anomaly_events` rows match expected fixtures.

This is identical to the existing on-demand test pattern — `evaluate()` is called the same way. No new async test infrastructure is required.

### Unit tests (no DB)

Each detector's `compute()` pure function already has unit tests. These are unaffected. Streaming adds no new detector logic.

### Integration tests (testcontainers Postgres)

New test: `tests/streaming_scheduler_test.rs`:
- Start a real Postgres container.
- Feed a bounded sequence of `InvalidationEvent`s into the scheduler via a test channel.
- Assert that after the debounce window, one `SchedulerJob` per (chain, mint) lands in the queue.
- Assert that `evaluate_token()` persists exactly the expected `AnomalyEvent` rows.

### Backpressure test

- Create a scheduler with `queue_capacity = 2`.
- Send 10 distinct (chain, mint) pairs rapidly.
- Assert `streaming_queue_overflow_total >= 8` (queue absorbs 2, drops rest).
- Assert no panic, no deadlock.

### WS integration test

- Connect a WS client against a test gateway.
- Subscribe to a specific token.
- Trigger a streaming evaluation for that token.
- Assert the WS client receives an `event` frame within 2 × `poll_interval_ms`.

---

## §8 Migration Plan

### Phase 1 — Plumbing (no detector logic changes)

1. Add `StreamingMetrics` struct to `crates/server/src/streaming_metrics.rs`. Registration in the existing Prometheus registry.
2. Add `StreamingRegistry` to `crates/server/src/streaming/registry.rs`.
3. Add `DetectorScheduler` + `SchedulerWorker` stubs to `crates/server/src/streaming/`. Workers use `async-channel::Receiver` (MPMC), NOT `Arc<Mutex<mpsc::Receiver>>`.
4. Extend `InvalidationEvent` (in `crates/gateway/src/state.rs`) with `block_time: i64` + `slot_hints: Vec<u64>` fields. Verify WS poll arm still compiles.
5. Wire `AppState.invalidation_tx.subscribe()` to `DetectorScheduler`. Scheduler sets `observed_at = MAX(block_time)` from slot_hints — wall-clock is forbidden per determinism invariant.
6. Add `anomaly_events.emitted_by` column via a new Postgres migration (V00010 candidate). Default `'api_request'`; streaming worker writes `'streaming_scheduler'`.
7. Implement per-detector `tokio::time::timeout(per_detector_timeout_ms, ...)` + `FutureExt::catch_unwind` + `AssertUnwindSafe` pattern in worker loop (see §6 "Detector panic"). Unit test: a detector that panics → worker continues, metric increments, no task death.
8. Implement `scoring_skip_delta_threshold` short-circuit in worker → benchmark: with threshold at 0.05, same-token second-tick skips `score()` + `risk_cache.insert` + `upsert_token_risk_report`.
9. Phase 1 smoke test: spin up scheduler, feed 1000 synthetic `InvalidationEvent`s for 50 distinct tokens, assert workers drain without deadlock, measure p50/p95 per-evaluation latency against budget derived from `sprint5_exit_test.rs` measurements (see §2.2 "Calibration status"). Recalibrate `queue_capacity` + `worker_count` if p95 > budget.
10. All workers return `Ok(())` without calling any real detector yet (plumbing-only test). Actual detector wiring starts Phase 2.
11. `cargo clippy --workspace --all-targets -- -D warnings` clean. `cargo test --workspace` green.

### Phase 2 — D04 pump/dump as the first streaming detector **[DONE — 2026-04-22]**

D04 is the highest-value streaming case: short-window volume spikes are time-sensitive. It also has the simplest context requirements (no simulation, no graph).

1. Wire the worker pool to call `D04::evaluate()` for each incoming job. ✓
2. Integrate test: pump.fun fixture → streaming evaluation → assert `pump_dump` event persisted. ✓ (`crates/server/tests/streaming_d04_integration_test.rs`, Docker-gated)
3. Validate latency: assert `streaming_evaluation_duration_seconds` p50 < 200ms on test fixture. ✓ (loose 10s testcontainers bound; prod target still 200ms — measure with real DB).

**Rev 2 implementation notes:**
- `Detector` trait changed from `async fn evaluate` to `fn evaluate -> impl Future<Output=...> + Send` so `SchedulerWorker::run()` is `Send` for `tokio::spawn`. This is a one-line change per impl (native async fns are auto-`Send` when captures are `Send`).
- `SchedulerWorker` struct uses `Option<PgStore>` / `Option<TokenRegistry>` etc. with an `is_empty()` guard before access — allows Phase 1 plumbing tests to pass `None` without a real DB.
- `upsert_token_risk_report` deferred to Phase 3 (no `token_risk_reports` table yet). AnomalyEvents are the durable record; `risk_cache` is in-memory only for Phase 2.
- `PgStore::insert_anomaly_events` now takes `emitted_by: &str`. Old on-demand path passes `"api_request"`; streaming passes `"streaming_scheduler"`.
- 6 `SkipReason` entries recorded for D01/D02/D03/D05/D06/D07 so `TokenRiskReport.coverage` is 7-detector-consistent between streaming and on-demand paths.

### Phase 3 — Remaining detectors **[DONE — 2026-04-22]**

Order of priority:
1. D05 (wash trading) — also time-sensitive (round-trip block window).
2. D06 (mint/burn) — streaming makes the "mint authority active" structural signal fire on first token appearance.
3. D02 (LP rug) — streaming LP drain events are high-value for market maker.
4. D01 (honeypot) — simulation is expensive (~500ms per path × 3 paths); streaming only if `simulate_paths` is reduced for streaming ticks OR D01 runs at a cadence (every Nth tick, not every tick). Deferred to Phase 4 within this design. **When D01 is skipped in a streaming tick, the worker emits a `SkipReason { detector_id: "honeypot_sim", reason: "streaming_tick_d01_cadenced" }` into the `TokenRiskReport` so the report remains 7-detector-consistent between streaming and on-demand paths — `scoring` already supports `detectors_skipped: Vec<SkipReason>` (see `crates/scoring/src/types.rs`).**
5. D03 (concentration) — snapshot-based; streaming adds little marginal value. Re-evaluate on every new `holder_snapshot` event, not on every transfer. Deferred. Uses `SkipReason { detector_id: "holder_concentration", reason: "streaming_snapshot_only" }`.
6. D07 (withdraw-withheld) — fires rarely; streaming adds little value. Deferred. Uses `SkipReason { detector_id: "withdraw_withheld_drain", reason: "streaming_low_value" }`.

### Phase 4 — Multi-process upgrade path (future ADR)

When the service scales to multiple replicas, `tokio::broadcast` within a single process is insufficient. The migration requires picking a self-hostable pub/sub substrate (ADR 0003 compliant). Candidates — all viable, all self-hostable:

- **Redpanda** (Kafka-compatible, Rust-friendly via `rdkafka`)
- **NATS JetStream** (lightweight, operational simplicity)
- **RabbitMQ** (mature, heavier ops)

**This selection is a FUTURE ADR (candidate 0005+), not pre-committed by this design.** Picking requires ops-side input (runbook complexity, team familiarity, persistence SLA) and a load test against realistic event rates. The migration mechanics are stable regardless of pick:

1. Replace `invalidation_tx: broadcast::Sender<InvalidationEvent>` with the chosen substrate's producer.
2. `DetectorScheduler` subscribes to the consumer group instead of a broadcast channel.
3. No changes to `Detector` trait, `ScoringEngine`, or `DetectorContext`.
4. No changes to detectors.

The abstraction point is the `InvalidationEvent` message schema — keep it minimal: `{ chain: String, mint: String, block_time: i64, slot_hints: Vec<u64> }` (rev 1: `block_time` + `slot_hints` added so the scheduler can set `observed_at` deterministically without a Postgres round-trip). This extends the existing `InvalidationEvent` struct in `crates/gateway/src/state.rs` — verify the struct can gain two fields without breaking gateway poll consumers before shipping.

---

## §9 Open Questions Left for Implementation

These cannot be resolved without code experiments. They are explicitly deferred.

1. **D01 streaming cost.** D01's `simulate_sell()` calls `SolanaRpc::simulate_transaction` 3×. At ~500ms per call, streaming D01 on every event would add ~1.5s per token per tick. Options: (a) reduce `simulate_paths` to 1 for streaming ticks via a separate streaming config key; (b) run D01 only on the first appearance of a new token; (c) skip D01 in streaming entirely and rely on on-demand. Decision needs a latency measurement with the real `HttpPoolAccountProvider` (Track B).

2. **~~`ScoringEngine::score()` delta threshold~~** — **rev 1: resolved and promoted to Phase 1 scope** (see §2.4 + §2.5 "Inter-tick scoring optimization"). `scoring_skip_delta_threshold = 0.05` default, configurable. No longer deferred.

3. **`streaming_idle_timeout_minutes` calibration.** 60 minutes is a guess. On pump.fun, a token's active life is 2-4 hours. A lower value (15 minutes) would reduce memory pressure. The right value depends on observed inter-event gaps on pump.fun tokens — needs production data from Track B.

4. **`observed_at` source in streaming ticks.** **(rev 1 — DECIDED, not deferred.)** The scheduler sets `observed_at = MAX(block_time)` over the `slot_hints` accompanying the job. Wall-clock at dequeue is REJECTED — it would break the CLAUDE.md determinism invariant ("given the same event sequence, output MUST be deterministic") on replay, and that's a binding rule, not a replay-mode preference. The `MAX(block_time)` value is fetched as part of the existing window query the worker already issues to Postgres for events in the window — incremental cost is one column, not a separate round-trip. The `InvalidationEvent` schema is also extended to carry the latest `block_time` directly so the scheduler doesn't need to query Postgres for the debounce-merge case. See §2.5 amendment.

5. **WS push vs poll for `TokenRiskReport`.** The current WS model polls `anomaly_events` at 500ms. A streaming-aware upgrade is the `invalidation_tx` arm (already in `ws/mod.rs` as a placeholder). Upgrading it to push `TokenRiskReport` directly would cut latency from 500ms to ~0ms for the WS consumer. But `TokenRiskReport` is larger than a single `AnomalyEvent` and changes on every streaming tick. Evaluate whether consumers want the full report pushed or just an invalidation signal ("re-fetch via REST"). Decision needs consumer SLO input.
