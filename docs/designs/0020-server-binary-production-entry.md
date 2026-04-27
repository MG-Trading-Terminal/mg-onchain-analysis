# Design 0020 — Server Binary Production Entry Point (Sprint 19 S19-1)

**Date:** 2026-04-25
**Status:** Draft — awaiting user sign-off on §11 decisions before S19-2 implementation
**Author:** architect agent
**Sprint:** 19 (Option F — gotcha #49 closure after 7-sprint debt)
**ADR refs:**
- ADR 0001 §D8 — Three delivery modes (crate / REST / WS) from same detector code
- ADR 0003 — Self-sovereign infrastructure; single deployable unit; zero 3rd-party SaaS in hot path
- ADR 0004 — Reth is the EVM node; ExEx is a Sprint 19+ feature flag
- ADR 0005 — MultiChainCoordinator (Pattern B) is the multi-chain spawn pattern
**Related designs:**
- `docs/designs/0014-streaming-detector.md` — streaming scheduler + worker architecture
- `docs/designs/0016-detector-09-bocpd-deployer-changepoint.md` — D09 indexer hook wiring
- `docs/designs/0019-detector-12-permit2-drainer.md` — D12 Ethereum-only detector

---

## §1 Background: Seven-Sprint Placeholder Debt

`crates/server/src/main.rs` has been a placeholder (`fn main() {}`) since Sprint 12. At the time, the server library (`lib.rs`) had just wired the streaming scheduler, five detectors (D01/D02/D04/D05/D06), and the risk-report store. The decision to leave `main.rs` as a stub was pragmatic: wiring a production binary entry would have been premature before the multi-chain foundation was in place.

Seven sprints later (S12 → S19), the wait-list has grown to the full production surface:

| Component | Landed sprint |
|-----------|---------------|
| D09 BOCPD deployer changepoint + D09IndexerHook | S12 |
| D10 LaunchAudit + D10IndexerHook | S12 |
| V00012 token_risk_reports migration + PgTokenRiskReportStore | S12 |
| V00013 BOCPD state + pools.initial_liquidity_usd | S12 |
| D11 SynchronizedActivity detector | S14 |
| EthereumAdapter (real WsRpcClient + 8 decoders + reconnect) | S16 |
| MultiChainCoordinator (Pattern B — ADR 0005) | S17 |
| ShutdownSignal (CancellationToken wrapper) | S17 |
| D12 Permit2Drainer detector (first EVM-only, `supported_chains = [Ethereum]`) | S18 |
| V00014 permit2_events migration + PgPermit2EventStore | S18 |
| D08 Sybil detector | S11 |
| D07 WithdrawWithheld detector | S10 |
| D03 HolderConcentration detector | S3 |

Every detector (D01-D12) passes `cargo test`. The gateway, storage, and client-sdk crates are fully functional. The only thing preventing a production deployment is the empty `main.rs`.

This design resolves that. It closes gotcha #49 by describing the precise construction sequence, configuration schema, graceful shutdown protocol, health check wiring, and test coverage required for Sprint 19 S19-2 to ship a production-deployable binary entry point.

---

## §2 Goals

1. **Close gotcha #49.** Replace `fn main() {}` with a production-quality tokio entry point.
2. **Wire all 12 detectors** into the streaming scheduler (D01-D12 inclusive), respecting the `supported_chains()` chain-filter already present in `ErasedDetector`.
3. **Wire MultiChainCoordinator** for Solana + Ethereum, honoring the per-chain `enabled` flag so ops can stage rollout.
4. **Wire indexer hooks** D09IndexerHook and D10IndexerHook into the Coordinator, connected to the PgAnomalyEventSink and BocpdStateStore (V00013).
5. **Wire three stores** from `crates/storage/src/pg.rs`: `PgTokenRiskReportStore` (V00012), `PgBocpdStateStore` (V00013), `PgPermit2EventStore` (V00014).
6. **Auto-migrate** to V00014 on startup (opt-out via `--no-migrate` flag).
7. **Expose `/health` and `/ready`** probes through the existing gateway health handler.
8. **Graceful shutdown** via `ShutdownSignal::from_os_signals()` (CancellationToken), 30-second drain window.
9. **Compile clean** under `cargo clippy --workspace --all-targets -- -D warnings` with default feature set (no ExEx feature flag).
10. **Integration-testable** without a live node via the existing fixture-replay infrastructure.

**Non-goals for this sprint:**
- Reth ExEx feature flag (gotcha #59 — Sprint 19+ separate track; binary compiles without it)
- Consumer integrations (gotcha #21 — STANDALONE SERVICE ONLY)
- Backfill CLI subcommand (Phase 5)
- Multi-region HA or supervisor process management

---

## §3 Module Structure

After Sprint 19 S19-2, `crates/server/src/` will have this layout:

```
crates/server/src/
  lib.rs                          existing — spawn_streaming_subsystem(), re-exports
  main.rs                         NEW — tokio entry point (replaces placeholder)
  erased_detector.rs              existing — ArcErasedDetector + ErasedDetector trait
  risk_report_store.rs            existing — TokenRiskReportStore trait + PgTokenRiskReportStore
  streaming_config.rs             existing — StreamingConfig
  streaming_metrics.rs            existing — StreamingMetrics
  config.rs                       NEW — ServiceConfig (top-level config struct)
  init/
    mod.rs                        NEW — pub use; top-level initializer type
    tracing.rs                    NEW — init_tracing() + optional OTLP exporter
    storage.rs                    NEW — connect_postgres() + run_migrations()
    adapters.rs                   NEW — build_solana_adapter(), build_ethereum_adapter()
    coordinator.rs                NEW — build_coordinator() assembles AdapterSlots
    hooks.rs                      NEW — build_pool_initialize_hook() wires D09+D10 hooks
    detectors.rs                  NEW — build_detector_set() constructs all 12 ErasedDetectors
    app_state.rs                  NEW — build_app_state() assembles AppState from parts
  streaming/
    mod.rs                        existing
    scheduler.rs                  existing
    worker.rs                     existing
    registry.rs                   existing
```

New files in `crates/server/src/init/` are all pure constructors with no side effects beyond `tracing`. They are the sole location where concrete types are assembled from config + Arc-wrapped dependencies. `main.rs` calls them in sequence and then enters the tokio event loop.

The `config.rs` module introduces `ServiceConfig`, a top-level serde struct covering all sub-configs in one file. This is a new addition; the streaming config previously lived only in `StreamingConfig`. `ServiceConfig` owns `StreamingConfig` as a nested field, preserving backward compatibility with `config/service.toml`.

---

## §4 Initialization Sequence

The startup sequence is strictly ordered. Each step must succeed before the next proceeds; any error is fatal (`anyhow::bail!` propagates through `main` as a non-zero exit code).

### Step 1 — Tracing

```
init::tracing::init_tracing(&config.observability)
```

Initialize `tracing_subscriber::registry()` with `EnvFilter` from `RUST_LOG`. If `OTLP_ENDPOINT` environment variable is set, attach an OTLP exporter (`opentelemetry_otlp`). This step has no dependencies and must be first so all subsequent steps emit structured logs.

**Why first:** every subsequent error is unobservable without a subscriber. Panics before this step produce untraced output to stderr only.

### Step 2 — Config load and validation

```
ServiceConfig::load(config_path: &Path) -> anyhow::Result<ServiceConfig>
```

Load `config/service.toml` via `toml::from_str`. Validate required fields (Postgres URL, at least one chain enabled). Emit a startup log of the effective config (redacted — never log Postgres credentials). Surface validation errors with the TOML key path.

`ServiceConfig` is a flat serde struct. Sub-configs for chains, storage, streaming, gateway, and observability are nested TOML tables. The `chains` section has per-chain `enabled` flags (Decision D-E).

**Why second:** all subsequent constructors take fields from `ServiceConfig`. Detecting invalid config before opening any network connection avoids partial initialization.

### Step 3 — Postgres connect + migrate

```
init::storage::connect_postgres(&config.storage) -> anyhow::Result<PgPool>
init::storage::run_migrations(&pool) -> anyhow::Result<()>
```

Connect to Postgres using `PgPoolOptions` with a bounded retry loop (configurable `db_connect_retries`, default 5, 2s backoff). Run `sqlx::migrate!()` to apply V00001–V00014 in order. If `--no-migrate` flag is set, skip `run_migrations`.

The migration runner is idempotent: applying V00001-V00014 on a database that already has them is a no-op.

**Why third:** storage is the single shared dependency for all downstream constructors. Connecting early surfaces misconfigured `DATABASE_URL` before any heavier initialization.

### Step 4 — Construct stores

```
let pg_store = PgStore::new(pool.clone());
let bocpd_state_store = Arc::new(PgBocpdStateStore::new(pool.clone()));
let permit2_event_store = Arc::new(PgPermit2EventStore::new(pool.clone()));
let anomaly_event_sink = Arc::new(PgAnomalyEventSink::new(pool.clone()));
let risk_report_store = if config.streaming.token_risk_reports_enabled {
    Some(Arc::new(PgTokenRiskReportStore::from_pg_store(&pg_store)))
} else {
    None
};
```

All five stores share the same `PgPool` (pool is `Arc`-backed inside sqlx; cloning is cheap). `PgAnomalyEventSink` implements the `AnomalyEventSink` trait used by D09/D10 hooks. The `risk_report_store` is `Option<Arc<dyn TokenRiskReportStore>>` — `None` when `token_risk_reports_enabled = false` (gotcha #47 preserved).

### Step 5 — Load configs

```
let detector_config = Arc::new(DetectorConfig::load("config/detectors.toml")?);
let scoring_config = ScoringConfig::load("config/scoring.toml")?;
let gateway_config = GatewayConfig::from_file("config/gateway.toml")?;
let known_drainers = KnownDrainerSet::load("config/known_drainers.toml")?;
```

Configs are loaded after the pool is established so that config validation errors are emitted through the tracing subscriber.

### Step 6 — Construct chain adapters

```
init::adapters::build_solana_adapter(&config.chains.solana) -> SolanaAdapter
init::adapters::build_ethereum_adapter(&config.chains.ethereum) -> EthereumAdapter
```

Each builder reads the chain-specific sub-config block. `SolanaAdapterConfig` is deserialized from `config/adapters.toml [solana]`. `EthereumAdapterConfig` is deserialized from `config/adapters.toml [ethereum]`. Both default to self-hosted endpoints per ADR 0003:

- Solana: `grpc://localhost:10000` (Yellowstone gRPC plugin default)
- Ethereum: `ws://localhost:8545` (Reth WS-RPC default)

Adapters that are disabled in config (`chains.solana.enabled = false`) are not constructed, and their `AdapterSlot` is not added to the coordinator.

### Step 7 — Construct indexer hooks

```
init::hooks::build_pool_initialize_hook(
    bocpd_state_store.clone(),
    anomaly_event_sink.clone(),
    detector_config.clone(),
) -> Arc<dyn PoolInitializeHook>
```

This constructs a composite hook that delegates to both `D09IndexerHook` and `D10IndexerHook`. The composite is a new thin struct (not a change to D09/D10 themselves) defined in `init/hooks.rs`. It calls each hook's `on_new_token_launch` and `on_reorg` in sequence, propagating the first error.

Rationale for a composite: `Indexer::new` takes `Option<Arc<dyn PoolInitializeHook>>` — a single optional hook. Rather than changing `Indexer::new` to accept `Vec<Arc<dyn PoolInitializeHook>>` (which would break gotcha #39's 9-param signature guarantee), the composite pattern wraps both hooks in a single `Arc`. `Indexer::new` is unchanged.

Chain-guard behavior is already implemented in D09 and D10 (gotcha #70): both hooks return `Ok(())` immediately when `chain != Chain::Solana`. The composite correctly passes the `chain` parameter through.

### Step 8 — Construct MultiChainCoordinator

```
init::coordinator::build_coordinator(
    enabled_adapters,
    shutdown.clone(),
) -> MultiChainCoordinator
```

Builds `Vec<AdapterSlot>` from enabled adapters and calls `MultiChainCoordinator::new`. The Indexer instances themselves are constructed inside `init::coordinator::build_coordinator`, calling `Indexer::new` with the 9-param signature (gotcha #39 preserved). Each Indexer gets its own `adapter_id` (`"solana"` or `"ethereum"`) for checkpoint isolation.

The `pool_initialize_hook` (Step 7) is passed to the Solana Indexer only. The Ethereum Indexer receives `None` for the hook — D09 and D10 are Solana-only hooks (their chain-guard fires on Ethereum calls, but since D09/D10 BOCPD state is Solana-trained, passing `None` is cleaner and avoids unnecessary invocations for the Ethereum path in this sprint).

**Note:** the hook `Arc` can later be passed to the Ethereum Indexer as well once EVM equivalents of D09/D10 exist. The `init::coordinator` constructor should accept this as an optional per-chain parameter to allow future flexibility without structural refactoring.

### Step 9 — Construct streaming detector set (all 12 detectors)

```
init::detectors::build_detector_set(
    detector_config.clone(),
    pg_store.clone(),
    bocpd_state_store.clone(),
    permit2_event_store.clone(),
    rpc_client.clone(),          // SolanaRpc for D01 simulation
    pool_account_provider.clone(),
    known_drainers.clone(),
) -> Vec<ArcErasedDetector>
```

Constructs all 12 detectors in alphabetical order by `detector_id` (the same convention used in `lib.rs` today). The `supported_chains()` filter in `SchedulerWorker::evaluate_token` ensures D01-D11 skip on Ethereum contexts and D12 skips on Solana contexts — no change to detector code needed.

Detector construction order (deterministic — matches scoring output key order):
1. D01 HoneypotDetector (Solana, cadenced)
2. D02 RugPullDetector (Solana)
3. D03 HolderConcentrationDetector (Solana)
4. D04 PumpDumpDetector (Solana)
5. D05 WashTradingDetector (Solana)
6. D06 MintBurnAnomalyDetector (Solana)
7. D07 WithdrawWithheldDetector (Solana)
8. D08 SybilDetector (Solana)
9. D09 BocpdDeployerChangepoint (Solana, hook-triggered — also wired into streaming for re-eval)
10. D10 LaunchAuditDetector (Solana, hook-triggered — also wired into streaming)
11. D11 SynchronizedActivityDetector (Solana)
12. D12 PermitDrainerDetector (Ethereum only — `supported_chains = [Ethereum]`)

D09 and D10 are dual-path: they fire at pool-init time via the hook (event-driven, immediate), and also participate in the periodic streaming re-evaluation cycle via their standard `Detector::evaluate` implementation. Both paths are correct and complementary.

### Step 10 — Construct AppState and gateway

```
let app_state = init::app_state::build_app_state(
    gateway_config,
    pg_store.clone(),
    token_registry,
    scoring_engine,
    detector_config.clone(),
    jwt_keys,
    gateway_metrics,
)?;
```

`AppState::new` is unchanged (it lives in `crates/gateway`). `build_app_state` assembles the `TokenRegistry`, `ScoringEngine`, `JwtKeys`, and `GatewayMetrics` from their respective configs and passes them to `AppState::new`.

### Step 11 — Start everything

```
// Spawn streaming subsystem (scheduler + worker pool)
spawn_streaming_subsystem(app_state.clone(), config.streaming.clone(), streaming_metrics).await;

// Start coordinator (spawns per-chain indexer tasks)
coordinator.start(indexer_event_tx).await?;

// Wire coordinator events into invalidation broadcast
tokio::spawn(coordinator_to_invalidation_bridge(
    indexer_event_rx,
    app_state.invalidation_tx.clone(),
));

// Run gateway (blocks until shutdown)
run_gateway(app_state.clone()).await?;
```

The gateway's `run_gateway()` blocks the main task. When it returns (on SIGTERM/SIGINT), the shutdown sequence begins.

### Step 12 — Graceful shutdown

See §6 for the full shutdown sequence.

---

## §5 Runtime Architecture

### Single tokio multi-thread runtime

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> { ... }
```

`tokio::main` expands to `tokio::runtime::Builder::new_multi_thread().enable_all().build()`. This is the correct choice: all I/O is async, all futures are `Send`, and the multi-thread scheduler maximizes throughput for concurrent Postgres queries and network I/O. There is no case for per-chain isolated runtimes at this scale (two chains, bounded concurrency per ADR 0005 Decision 3).

### Task topology

```
main task
├── gateway (axum serve — blocks main on I/O event loop)
├── scheduler task (DetectorScheduler::run — tokio::spawn)
├── N × worker tasks (SchedulerWorker::run — tokio::spawn, N = worker_count)
├── coordinator bridge task (event forwarding — tokio::spawn)
├── solana indexer task (Indexer::run — via MultiChainCoordinator::start)
└── ethereum indexer task (Indexer::run — via MultiChainCoordinator::start, if enabled)
```

All tasks share a single `ShutdownSignal`. The gateway installs OS signal handlers independently (its existing `shutdown_signal()` function); the `ShutdownSignal::from_os_signals()` is used by the coordinator and scheduler. Both signal paths result in the same CancellationToken being cancelled.

**Avoiding double-signal handling:** the gateway's `shutdown_signal()` (in `crates/gateway/src/lib.rs`) is an internal async fn that only gates `axum::serve.with_graceful_shutdown`. The `ShutdownSignal::from_os_signals()` is separate and gates the coordinator + scheduler. When SIGTERM arrives, both listeners fire concurrently: the gateway stops accepting new connections and drains in-flight requests, while the coordinator stops ingesting events and the scheduler drains its queue. This is the correct concurrent shutdown behavior.

### Coordinator-to-invalidation bridge

The `MultiChainCoordinator` produces `Event` values (via `event_stream()` or the `start(tx)` API). The streaming scheduler consumes `InvalidationEvent` values from the `broadcast::Sender` in `AppState`. A lightweight bridge task translates between them:

```
coordinator mpsc::Receiver<Result<Event, AdapterError>>
    → filter Token/Swap/PoolEvent types
    → construct InvalidationEvent { chain, mint, block_time, slot_hints }
    → app_state.invalidation_tx.send(...)
```

This bridge lives in `init/coordinator.rs` as a free async function `coordinator_to_invalidation_bridge(rx, tx)`. It does not belong in `MultiChainCoordinator` (which has no gateway dependency) or in the scheduler (which has no indexer dependency). The `init/` module is the correct seam.

The bridge observes the `ShutdownSignal` so it exits cleanly when the coordinator stops producing events.

---

## §6 Graceful Shutdown Sequence

Shutdown is triggered by SIGTERM or SIGINT. The sequence is:

```
1. SIGTERM/SIGINT received
   ├── ShutdownSignal::from_os_signals() cancels the CancellationToken
   └── gateway's shutdown_signal() future resolves (concurrent)

2. Coordinator shutdown (via CancellationToken)
   - Per-chain indexer tasks observe `shutdown.cancelled()` in their select! loop
   - Each indexer breaks its event loop, flushes pending batches, saves checkpoint
   - MultiChainCoordinator::join() awaits all per-chain task handles
   - Drain timeout: 30s (Decision D-D)

3. Streaming scheduler shutdown (via async_channel close)
   - When the coordinator bridge task exits, the mpsc::Sender is dropped
   - Bridge exit causes invalidation_tx to have no new senders (existing WS subscribers
     continue to drain until the gateway closes their connections)
   - Scheduler's invalidation_rx.recv() returns Closed → scheduler task exits
   - Queue is drained: workers finish in-flight evaluations, then queue is closed
   - Workers observe async_channel::Receiver::recv() returning None → exit

4. Workers finish current evaluations (bounded by per_detector_timeout_ms)
   - Each worker completes the current detector call under its timeout guard
   - No new jobs accepted (queue closed)
   - Workers exit their run() loops

5. Gateway drains in-flight HTTP requests
   - axum::serve with_graceful_shutdown gives in-flight requests shutdown_timeout_seconds
   - Default 5s (GatewayInner.shutdown_timeout_seconds); configurable in gateway.toml
   - After timeout, remaining connections are forcibly closed

6. main() returns Ok(())
   - tokio runtime drops all remaining tasks
   - process exits 0
```

**Timeout budget (Decision D-D):**
- Indexer drain window: 30s (configurable via `service.toml [shutdown] drain_timeout_seconds`)
- Gateway request drain: 5s (existing `gateway.toml [gateway] shutdown_timeout_seconds`)
- Worker eval completion: bounded by `per_detector_timeout_ms` (default 3s per eval; at most N workers × 3s in practice, bounded by queue close)
- Total worst-case: 30s + 5s + a few seconds for the runtime to drop. Well within a typical 60s container SIGKILL window.

---

## §7 Config Schema

The production binary reads from three config files (plus `config/detectors.toml` and `config/scoring.toml` which already exist and are unchanged):

### config/service.toml (extended)

```toml
# =============================================================================
# config/service.toml — mg-onchain-server top-level config
# =============================================================================

[shutdown]
# Seconds to wait for the indexer + scheduler to drain in-flight work.
# After this window, tokio drops remaining tasks and exits.
# Default 30s. Configurable via --no-migrate and this field.
drain_timeout_seconds = 30

[observability]
# RUST_LOG filter string used by tracing_subscriber.
# Overridden by the RUST_LOG environment variable.
log_filter = "info,mg_onchain=debug"
# Optional OTLP collector endpoint. Set in environment:
#   OTLP_ENDPOINT=http://otel-collector:4317
# When unset, OTLP exporter is NOT loaded (no hard dep on observability infra).
# otlp_endpoint = ""  # read from env OTLP_ENDPOINT at runtime

[chains.solana]
enabled = true     # Decision D-E: Solana default-on

[chains.ethereum]
enabled = false    # Decision D-E: Ethereum default-off for first rollout

# [streaming] section unchanged from existing service.toml
# All StreamingConfig fields remain under [streaming]
```

### config/adapters.toml (existing, unchanged)

Already has `[solana]` and the commented-out dev-bootstrap alternatives. Add `[ethereum]` section:

```toml
[ethereum]
# Self-hosted Reth node WebSocket endpoint (ADR 0003 + ADR 0004).
# Default: ws://localhost:8545 (Reth default WS port)
rpc_url = "ws://localhost:8545"
# Reorg confirmation depth. 12 blocks ≈ 2.4 min. ADR 0004 §Finality.
reorg_depth = 12
# Checkpoint path for block hash / block number resume.
checkpoint_path = "./checkpoints/ethereum.json"
```

### config/storage.toml (existing, unchanged)

```toml
postgres_url = "postgres://user:password@localhost:5432/mg_onchain"
migrations_auto_apply = true  # Decision D-A opt-out path
```

### CLI flags

```
onchain-service [OPTIONS]
  --config <PATH>      Path to service.toml  [default: config/service.toml]
  --no-migrate         Skip automatic migration at startup
  --help               Print help
```

The binary does not use a subcommand structure for the initial sprint. A `migrate` subcommand can be added in a later sprint if ops requests it.

---

## §8 Health Check and Observability

### `/health` (liveness)

Already implemented in `crates/gateway/src/routes/health.rs`. Returns 200 when Postgres is reachable (`SELECT 1` with 500ms timeout) and the registry is initialized.

**Extension needed for Sprint 19:** add chain adapter lag checks to the `HealthResponse`. The coordinator's `healthcheck()` method already calls `adapter.health_check()` on each chain. The `/health` handler should call `coordinator.healthcheck().await` and include per-chain status. The coordinator reference needs to be added to `AppState` or passed separately.

**Recommended approach:** store `Arc<MultiChainCoordinator>` in `AppState` alongside the existing fields. This requires a minor extension to `AppState::new` (one additional field). The change is backward-compatible (existing test constructors can pass `None` or a mock coordinator).

### `/ready` (readiness)

Currently not a separate endpoint. The `/health` endpoint already serves as a combined liveness + readiness check. For Sprint 19, promote the chain-lag check to the readiness signal:

```
readiness = Postgres reachable
          AND at least one chain adapter last saw a block within N minutes
```

Recommended N = 5 minutes (300 seconds). The `tip()` method on `ChainAdapter` returns the current head `BlockRef`. Storing the last-seen block time per chain in `MultiChainCoordinator` (or in `AppState`) and comparing against wall-clock in the health handler gives the freshness signal.

**Alternative:** use the existing `adapter_checkpoints` table. Query `MAX(updated_at)` per `adapter_id`. If the freshest checkpoint is older than 5 minutes, the chain is lagging. This requires no new in-memory state — just a DB query in the health handler. Recommended for Sprint 19 (simpler, no new AppState fields).

**Liveness vs. readiness split:** the gateway currently exposes only `/health`. For Kubernetes/Docker, two separate endpoints at `/health` (liveness — process up, never blocks unless OOM) and `/ready` (readiness — can serve requests, checks DB + chain lag) are conventional. The split can be deferred to a follow-up sprint without blocking production deployment.

### `/metrics` Prometheus scrape endpoint

Already implemented in `crates/gateway/src/routes/metrics_handler.rs`. The existing `StreamingMetrics` and `GatewayMetrics` are already registered. No new endpoints needed.

### OTLP (optional)

`opentelemetry_otlp` is gated by `OTLP_ENDPOINT` environment variable. When the variable is absent, no OTLP exporter is constructed and the binary has zero runtime dep on an observability collector. This preserves dev-machine buildability without an OTEL stack.

The OTLP integration uses `opentelemetry_sdk` + `opentelemetry_otlp` crates, added behind a `cfg(feature = "otlp")` feature flag or conditionally constructed at runtime based on `OTLP_ENDPOINT`. Runtime-conditional construction (no feature flag) is simpler and avoids a build-time flag matrix. Recommend runtime-conditional for Sprint 19.

---

## §9 Test Strategy

### Unit tests (no external deps)

All new `init/` constructors should have unit tests that verify:
- Config deserialization from a minimal TOML string (no file I/O)
- Chain enable/disable flag correctly determines which `AdapterSlot`s are added
- Composite `PoolInitializeHook` correctly delegates to both D09 + D10 hooks
- Shutdown sequence exits cleanly (mock coordinator, mock scheduler)

These tests require no Postgres, no live nodes, and no fixture files. They run in `cargo test`.

### Integration test: binary startup + clean shutdown

Add `crates/server/tests/binary_shutdown_test.rs`:

```
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn server_starts_and_shuts_down_cleanly() {
    // 1. Load minimal ServiceConfig with streaming.enabled = false
    //    and chains.solana.enabled = false, chains.ethereum.enabled = false
    //    (no live adapters needed — coordinator spawns zero tasks)
    // 2. Connect to test Postgres, run migrations
    // 3. Construct AppState with mock/stub registry and detector_config
    // 4. Spawn spawn_streaming_subsystem (with streaming.enabled = false → no-op)
    // 5. Create ShutdownSignal, cancel it immediately
    // 6. Await coordinator.join() → assert all results are Ok
    // 7. Assert process state is clean (no panics, no leaked tasks)
}
```

This test verifies the initialization sequence end-to-end without network I/O. It requires a Postgres instance (gated by `#[ignore]`, run via `DATABASE_URL`). This is the same pattern as the existing `sprint8_exit_test.rs` and `d01_simulation_e2e_test.rs`.

### Fixture-based determinism test

Add to `crates/server/tests/`:

```
streaming_all_detectors_test.rs
```

This test constructs all 12 detectors (using mock stores via `MockPgRunner`) and verifies that running the full detector set against a fixed fixture set produces deterministic output (same output for two identical runs). This closes a coverage gap — the existing `streaming_d04_integration_test.rs` covers only D04. The new test covers all 12 detectors including D12's `supported_chains` chain-guard.

---

## §10 Compile-Flag Matrix

Sprint 19 compiles with `default` features only. No feature flags are introduced in this sprint.

| Feature flag | Status | Notes |
|---|---|---|
| (default) | S19-2 target | No flags. WS-only EVM path via `WsRpcClient`. |
| `exex` | Sprint 19+ future | Reth ExEx alternate `EthereumAdapter::subscribe`. Gated via `cfg(feature = "exex")`. NOT in this sprint (gotcha #59). |
| `otlp` | Optional | Can be added as a feature flag OR as runtime-conditional from env var. Recommend env-var approach for S19. |
| `full-archive` | Future | Archive node storage for historical backfill. Out of scope S19. |

The `Cargo.toml` for `crates/server` should add:
- `tokio-util` (already a transitive dep via `crates/indexer` — make explicit)
- `clap` (for `--config` and `--no-migrate` CLI flags; lightweight `derive` feature only)
- `toml` (for `ServiceConfig::load`)

No new chain-adapter, alloy, or yellowstone-grpc deps are added.

---

## §11 Decisions Requiring Sign-off

### D-A — Migration policy

**Recommended: auto-run on startup, with `--no-migrate` opt-out flag.**

When `--no-migrate` is NOT passed (the default), `main.rs` calls `sqlx::migrate!()` immediately after the Postgres pool is established, before any other component is initialized. This is operator-friendly: a fresh deployment needs zero manual steps. It is idempotent: running migrations on a current database applies zero changes.

The `--no-migrate` flag is provided for the following ops scenarios:
- Read-only replicas (reporting DB — should never accept schema writes)
- CD pipelines that run migrations as a separate step with explicit rollback control
- Blue-green deployments where the new schema must be applied before traffic switches

**Trade-offs:**
- Auto-run: zero operator burden; risks a bad migration bringing down startup if the migration fails. Mitigation: migrations are append-only (no destructive DDL in V00001-V00014); test migrations in CI before merging.
- Separate CLI subcommand (`onchain-service migrate`): cleaner separation of migration concerns; requires an additional invocation in every deployment script; adds binary surface area.
- Both with flag default: the approach recommended here. Simple flag, no subcommand.

**User options:** A1 (auto-run default, `--no-migrate` opt-out) | A2 (separate `migrate` subcommand, binary boots without migrating) | A3 (always auto-run, no opt-out)

### D-B — Multi-binary vs single-binary

**Recommended: single binary `onchain-service` that runs indexer + scheduler + gateway.**

ADR 0001 §D8 committed to "standalone binary + Rust crates". ADR 0003 committed to "single deployable unit". The four consumers (bot, custody, MM, exchange) are REST/WS clients — they have no deployment coupling to this binary.

A split into `onchain-indexer` + `onchain-api` + `onchain-worker` would enable independent scaling of each tier. However, at the current operational scale (one Postgres, two chains, fixed consumer set), the complexity cost of a split binary is not justified. The single binary shares all `Arc<>`-wrapped state in-process, which eliminates cross-process IPC (Kafka/Redpanda) as a requirement at this scale. The CLAUDE.md stack note says "start with in-process channels; move to Redpanda/Kafka when multi-instance is needed" — that trigger has not been met.

**Trade-offs:**
- Single binary: simpler deployment (one process, one config file, one health check); all state in-process; bounded by single-machine resources; no cross-process backpressure complexity.
- Split binaries: independent horizontal scaling; process-level fault isolation (API pod can restart independently of indexer pod); required for multi-instance Kubernetes deployment. Cost: add Redpanda/Kafka as a hard dep for the indexer → scheduler event bus; adds significant ops complexity.

**User options:** B1 (single binary, recommended) | B2 (split `onchain-indexer` + `onchain-api`; implies Redpanda addition)

### D-C — `token_risk_reports_enabled` default

**Recommended: keep default `false` (opt-in). Flip to `true` when ops confirms V00012 is applied.**

Gotcha #47 established `token_risk_reports_enabled = false` in Sprint 12 as an explicit opt-in to protect against deploying the binary against a database that does not yet have the V00012 migration. Since Sprint 19 will auto-run all migrations including V00012, this protection is now provided by the migration step itself.

However, keeping the default `false` gives ops an explicit opt-in gate for the persistence path. The in-memory `RiskCache` continues to serve all `/v1/tokens/{chain}/{mint}/risk` requests without Postgres writes. The flag is easily flipped in `config/service.toml` after confirming the migration ran successfully.

**Trade-offs:**
- Keep `false` (recommended): explicit opt-in; no accidental V00012 write load; ops must consciously enable persistence.
- Flip to `true`: writes enabled by default post-migration; simpler for ops who expect persistence to "just work"; risk of confusion when V00012 does not exist (now mitigated by auto-migrate).

**User options:** C1 (keep default `false`, recommended) | C2 (flip to `true` as new default in production binary)

### D-D — Graceful shutdown drain timeout

**Recommended: 30 seconds default, configurable via `config/service.toml [shutdown] drain_timeout_seconds`.**

The drain window covers:
- Indexer flush: a full batch (up to 500 events) flushed to Postgres. At 10ms per batch, this is well under 1 second.
- Scheduler drain: all pending jobs dispatched to the async_channel before close. Near-instantaneous.
- Worker completions: each worker finishes its current eval under `per_detector_timeout_ms` (default 3s). With N workers all evaluating simultaneously, worst case is ~3s.
- Checkpoint saves: one per chain adapter. Two Postgres writes. Under 100ms each.

Total expected drain time is under 5 seconds for normal load. The 30-second window provides headroom for Postgres latency spikes (cold storage, WAL flush under high concurrent load). Kubernetes default SIGKILL grace is 30 seconds; the recommended drain window fits within this.

**Trade-offs:**
- 10s: fits within aggressive SIGKILL windows (some cloud providers use 10s). May truncate a slow Postgres flush under sustained write load.
- 30s (recommended): comfortable headroom for all flush + checkpoint operations. Standard for data pipeline services.
- Configurable (included): ops can tune downward in latency-sensitive environments or upward in high-throughput deployments.

**User options:** D1 (10s default, configurable) | D2 (30s default, configurable — recommended) | D3 (30s fixed, not configurable)

### D-E — Per-chain enable flags

**Recommended: Solana `enabled = true`, Ethereum `enabled = false` by default in `config/service.toml`.**

D12 (Permit2Drainer) is the first production EVM detector. The EthereumAdapter is functionally complete with real WsRpcClient + reconnect. However, the infra/ethereum-node runbook (Reth + Lighthouse) has not been tested end-to-end in production. Defaulting Ethereum to disabled allows ops to:
- Deploy and validate the Solana path in production first
- Stage the Ethereum adapter when the self-hosted Reth node is running
- Enable Ethereum by flipping one config line (`enabled = true`) without a binary redeployment

The `D12PermitDrainerDetector` will be loaded into the detector set regardless of the Ethereum chain flag — the `supported_chains()` filter in `SchedulerWorker` will simply never dispatch D12 jobs until Ethereum tokens appear in the invalidation channel (which they won't until the Ethereum adapter is enabled and the indexer is running).

**Trade-offs:**
- Solana-on / Ethereum-off (recommended): conservative; allows independent chain rollout; no Ethereum-side operational gaps block Solana production deployment.
- Both enabled by default: simpler config; requires ops to have a functioning Reth node before deploying (otherwise the Ethereum adapter will fail health checks and mark the service degraded).

**User options:** E1 (Solana on, Ethereum off — recommended) | E2 (both on by default, ops must provide Reth node) | E3 (both off by default, ops must opt-in to each chain)

---

## §12 Migration Plan

### Existing tests keep working

D01-D12 tests are all in `crates/detectors/tests/` and `crates/server/tests/`. None depend on a working `main.rs`. They use mock stores and fixture replay. Sprint 19 S19-2 does not touch any existing test.

`sprint8_exit_test.rs` tests the streaming subsystem via `spawn_streaming_subsystem()`. This function signature is unchanged (Step 9 adds detectors to the set, but the function takes `Vec<ArcErasedDetector>` which accepts any length). The sprint8 test will continue to pass with the subset of detectors it constructs.

### Detector additions to `spawn_streaming_subsystem`

The current `lib.rs` `spawn_streaming_subsystem` wires only D01/D02/D04/D05/D06 (5 of 12 detectors). Sprint 19 S19-2 will update `lib.rs` to call `init::detectors::build_detector_set()` and pass the full 12-detector `Vec<ArcErasedDetector>`. This is a backward-compatible change — the `SchedulerWorker` field is `Vec<ArcErasedDetector>` with no minimum length constraint. Workers iterate the full Vec on each job; detectors that don't support the job's chain return early via the `supported_chains()` guard.

The comment block in `lib.rs` that lists `"D01/D06/D04/D02/D05"` will be updated to reflect all 12 detectors.

### Config file additions

Two new config keys in `config/service.toml`:
- `[shutdown] drain_timeout_seconds = 30`
- `[chains.solana] enabled = true`
- `[chains.ethereum] enabled = false`

These are backward-compatible additions: the existing service.toml will continue to work without them (struct fields use `serde(default)`).

One new config section in `config/adapters.toml.example`:
- `[ethereum]` block (documented with self-hosted Reth defaults per ADR 0003)

### gotcha #49 closure

Sprint 19 S19-2 closes gotcha #49 when `cargo run --release --bin onchain-service` runs without panicking, processes at least one Solana event from fixture replay, and shuts down cleanly via SIGTERM. The integration test in `crates/server/tests/binary_shutdown_test.rs` is the automated verification.

### D09 + D10 server wiring (gotcha #49 sub-item)

D09 and D10 have been wired via `D09IndexerHook` + `D10IndexerHook` in `crates/detectors/` since Sprint 12. What has been missing is the server-side construction of these hooks and their injection into the `Indexer` via the coordinator. Sprint 19 S19-2's `init::hooks::build_pool_initialize_hook()` closes this gap.

---

## References

| # | Source | Decision grounded |
|---|--------|------------------|
| 1 | `crates/server/src/main.rs` | Gotcha #49: placeholder stub being replaced |
| 2 | `crates/server/src/lib.rs` | spawn_streaming_subsystem: 5-detector baseline; extended to 12 |
| 3 | `crates/indexer/src/coordinator.rs` | MultiChainCoordinator::new / start / join / healthcheck |
| 4 | `crates/indexer/src/shutdown.rs` | ShutdownSignal::from_os_signals pattern |
| 5 | `crates/indexer/src/hooks.rs` | PoolInitializeHook trait — composite pattern rationale |
| 6 | `crates/detectors/src/d09_deployer_changepoint.rs` | D09IndexerHook + PgAnomalyEventSink |
| 7 | `crates/detectors/src/d12_permit2_drainer.rs` | supported_chains = [Ethereum] — D-E rationale |
| 8 | `crates/storage/src/pg.rs` | PgStore + store impls for V00012/V00013/V00014 |
| 9 | `crates/gateway/src/lib.rs` | run_gateway + shutdown_signal |
| 10 | `crates/gateway/src/routes/health.rs` | /health handler — chain lag extension point |
| 11 | `crates/gateway/src/state.rs` | AppState::new — coordinator field addition |
| 12 | `config/service.toml` | StreamingConfig defaults — unchanged |
| 13 | `config/adapters.toml.example` | Self-sovereign defaults — ADR 0003 binding |
| 14 | `config/storage.toml.example` | migrations_auto_apply — D-A opt-out |
| 15 | SESSION-KICKOFF.md gotcha #1 | crates/common FROZEN |
| 16 | SESSION-KICKOFF.md gotcha #21 | STANDALONE SERVICE ONLY |
| 17 | SESSION-KICKOFF.md gotcha #29 | scheduler queue overflow is intentional |
| 18 | SESSION-KICKOFF.md gotcha #31 | next migration is V00015 |
| 19 | SESSION-KICKOFF.md gotcha #33 | crate dep direction (gateway ← server, not vice versa) |
| 20 | SESSION-KICKOFF.md gotcha #39 | Indexer::new 9-param signature preserved |
| 21 | SESSION-KICKOFF.md gotcha #47 | token_risk_reports_enabled default false — D-C |
| 22 | SESSION-KICKOFF.md gotcha #49 | server-binary stub — this design closes it |
| 23 | SESSION-KICKOFF.md gotcha #59 | Reth ExEx is Sprint 19+ feature flag — NOT in S19-2 |
| 24 | SESSION-KICKOFF.md gotcha #65 | MultiChainCoordinator is the spawn pattern |
| 25 | SESSION-KICKOFF.md gotcha #67 | supported_chains() default = Solana; D12 overrides |
| 26 | ADR 0001 §D8 | Three delivery modes — single binary satisfies all |
| 27 | ADR 0003 | Self-sovereign — default endpoints localhost; no 3rd-party in hot path |
| 28 | ADR 0004 | Reth WS-RPC for Ethereum; ExEx deferred |
| 29 | ADR 0005 Decision 3 | Unified streaming queue — no per-chain queues |
