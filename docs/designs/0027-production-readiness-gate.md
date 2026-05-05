# Design 0027 — Production Readiness Gate (Sprint 26)

**Date:** 2026-04-27
**Status:** SUPERSEDED 2026-04-27 (same session as drafting). User applied the kludge-test (memory `feedback_kludge_test.md`) to the ingestion architecture itself: the service's real workload is **on-demand deep-dive analysis on specific tokens** (≤15 RPC calls per query, seconds wall-time per token), demonstrated by the ZBT-on-BSC anomaly verdict produced this session. Validator-class hardware (512 GB Solana validator) and Yellowstone Geyser plugin firehose are over-engineering for this workload. Standard JSON-RPC + WebSocket subscriptions against lightweight self-hosted RPC nodes are sufficient. This document was specced under the old continuous-streaming assumption (firehose ingestion → Postgres → detectors react); the new model is pull-based query engine. Replacement: ADR 0007 (Pull-Based Query Engine Operational Model — pending architect draft) + design 0028 (Sprint 26 lightweight query-engine production deployment — pending architect draft). This document is preserved for historical record of the deprecated approach; do not implement against it. See memory `feedback_query_engine_model.md` for the binding rule.
**Author:** architect agent
**Sprint:** 26
**ADR refs:**
- ADR 0001 §D8 — three consumer delivery modes (Rust crate / REST / WebSocket)
- ADR 0003 — self-sovereign infrastructure; no SaaS in production hot path
- ADR 0006 (post-amendment) — code-level self-sovereignty; wire-protocol integration only

**Predecessor:** `docs/designs/0026-solana-stack-divestment.md` (Sprint 25, closed 2026-04-27)

---

## §1 Status / Date / Author / Sprint / ADR Refs

See header above. The status "Proposed — awaits user sign-off on §11 decisions" means
implementation begins only after the user has confirmed or redirected each of the eleven
sign-off items in §11. Specifically, items §11.3, §11.4, §11.6, and §11.11 carry
recommendations that the implementation agent will follow literally unless overridden.

---

## §2 Goals and Non-Goals

### §2.1 Goals

Sprint 26 closes the gap between "doctrinally clean and compile-green" and "deployable by
a consumer." The six deliverables that define "production-ready" for this project are:

1. **OTLP exporter wire-up.** The `TODO(sprint-20)` in
   `crates/server/src/init/tracing_init.rs` is resolved. When `OTEL_EXPORTER_OTLP_ENDPOINT`
   is set, `onchain-service` exports spans over gRPC (OTLP protocol) to the configured
   collector. When unset, behavior is unchanged (stdout-only tracing). No behavioral
   changes to detectors.

2. **Live testcontainers Postgres integration test.** The Docker mode in
   `crates/server/src/bin/onchain_validate.rs` already spins up a Postgres container and
   runs all 17 migrations (V00001–V00017). Sprint 26 promotes the existing
   `#[ignore] docker_dispatcher_all_13_detectors` test into a named production smoke test
   in `crates/server/tests/production_smoke_test.rs` that also exercises the REST endpoint
   `/v1/anomaly_events`. The new test is the sprint-close gate.

3. **Health and Prometheus metrics endpoints.** Both are already present and functional.
   Sprint 26 extends the health response to include chain-adapter liveness checks, adds
   the missing counters called out in §5.3, and splits `GatewayMetrics::registry` and
   `StreamingMetrics::registry` outputs behind a single `/metrics` handler that merges
   both families. Today the two registries are isolated and only `GatewayMetrics` is
   served at `/metrics`; `StreamingMetrics` is registered but never exposed.

4. **`infra/docker-compose.prod.yml`.** A single compose file orchestrating all sibling
   processes (`ethereum-node`, `solana-node`, `postgres`, `onchain-service`) with optional
   `otel-collector` service. Does not exist yet. Sprint 26 creates it.

5. **Backfill smoke test runbook.** No documented operator procedure for catch-up backfill
   exists beyond individual chain runbooks. Sprint 26 adds a dedicated section to
   `infra/PRODUCTION.md` and provides a CLI invocation for the `onchain-service --backfill`
   path against each chain.

6. **`infra/PRODUCTION.md` deployment doc.** Does not exist. Sprint 26 creates it as the
   authoritative first-deployment playbook: hardware BOM, cold-start sequence, expected
   timeline, readiness signals, rollback procedure, backup, log retention, secrets.

### §2.2 Non-Goals

The following are explicitly out of scope for Sprint 26 and must not appear in any
implementation brief dispatched under this design:

- New detectors (D14+), including Token-2022 extensions, Pump.fun graduation, and D13
  pool coverage extensions. All remain on the carry-forward list.
- Consumer-side integration code. The boundary is firm per memory
  `feedback_standalone_service_only.md`: we ship an API and SDK; consumers adopt on their
  own timeline. No writes to `bot-trader-2-0`, `mg-custody`, or any sibling repository.
- Stage 2 FDR calibration. Corpus-blocked; minimum 30 days of live data required.
- Additional EVM chains (Base, BSC, Arbitrum, Polygon). Phase 4 scope.
- Multi-tenant isolation between REST consumers (Rate-limit infrastructure exists via
  `crates/gateway/src/ratelimit.rs`; per-consumer quota enforcement is Sprint 27+).
- mTLS between `onchain-service` and its sibling node processes. Sprint 27+.
- ClickHouse integration. The current storage tier is Postgres-only; ClickHouse remains
  on the Phase 3 roadmap.

---

## §3 Architectural Overview

The production deployment topology after Sprint 26 is a single docker-compose stack with
four mandatory services and one optional observability sidecar:

```
┌─────────────────────────────────── host machine ───────────────────────────────────┐
│                                                                                     │
│  ┌──────────────────────┐    ┌──────────────────────┐                              │
│  │   solana-node         │    │   ethereum-node        │                            │
│  │  (Agave + Yellowstone)│    │  (Reth + Lighthouse)   │                            │
│  │  grpc :10000          │    │  ws   :8546            │                            │
│  │  rpc  :8899           │    │  rpc  :8545            │                            │
│  └───────────┬──────────┘    └───────────┬────────────┘                            │
│              │  gRPC / Yellowstone proto  │  JSON-RPC 2.0 / WebSocket               │
│              └───────────────┬───────────┘                                         │
│                              │ (internal network)                                  │
│  ┌───────────────────────────▼───────────────────────────────────────────────┐     │
│  │                       onchain-service                                     │     │
│  │  crates/server — single binary                                            │     │
│  │  REST  :8080  (GET /health, GET /metrics, /v1/*)                         │     │
│  │  WS    :8081  (GET /v1/ws/stream — AnomalyEvent topic)                   │     │
│  │                                                                           │     │
│  │  [tracing] ──OTLP gRPC──► otel-collector :4317 (optional sidecar)       │     │
│  └───────────────────────────┬───────────────────────────────────────────────┘     │
│                              │  pgwire / sqlx                                      │
│  ┌───────────────────────────▼───────────────────────┐                            │
│  │                   postgres-16                       │                           │
│  │  official postgres:16 image                         │                           │
│  │  port :5432 (internal network only)                 │                           │
│  └─────────────────────────────────────────────────────┘                           │
│                                                                                     │
│  ┌──────────────────────────────────────────────────────┐                          │
│  │  otel-collector  (optional sidecar — commented out)   │                         │
│  │  otel/opentelemetry-collector-contrib                  │                         │
│  │  grpc  :4317  ← OTLP from onchain-service             │                         │
│  │  routes to operator's Jaeger / Tempo / Honeycomb      │                         │
│  └──────────────────────────────────────────────────────┘                          │
│                                                                                     │
│  External access (firewall / reverse proxy):                                        │
│    :8080  REST  ── consumer HTTP clients (custody, exchange, MM)                    │
│    :8081  WS    ── market-maker streaming subscription                              │
│    All other ports (5432, 10000, 8899, 8545, 8546, 4317) are internal-only.       │
└─────────────────────────────────────────────────────────────────────────────────────┘
```

The Rust crate delivery mode (ADR 0001 §D8) is unchanged: `client-sdk` crate compiles
against the REST API surface. No in-process binary linkage with consumer projects.

Two departures from the Sprints 24+25 state worth noting explicitly. First, `onchain-service`
currently listens on a single port (configured in `gateway.bind_address`, default `:8080`);
the WS handler is served from that same port at `/v1/ws/stream`. The topology diagram
labels WS as `:8081` as a sign-off question (§11.6). If the user chooses to keep a single
port, the compose file simply does not split them. Second, the Solana node is a separate
machine in the ADR 0003 hardware BOM — the compose file supports `network_mode: host` for
the solana-node service as a deployment-time choice for operators who run it on the same
box.

---

## §4 Audit of Current State

This section documents exactly what exists today, what is functional, and what is missing
for each Sprint 26 deliverable.

### §4.1 OTLP Exporter

**File:** `/Users/dmytro.chystiakov/Projects/mg-onchain-analysis/crates/server/src/init/tracing_init.rs`

**State: STUB.** The function `init_tracing` builds a `tracing_subscriber` registry with
a single `fmt::layer()`. Lines 39–49 detect the `OTLP_ENDPOINT` / `otlp_endpoint` config
value and log a notice saying "OTLP exporter deferred to Sprint 20" — but do not attach
any OTLP layer. The feature was explicitly deferred in the `TODO(sprint-20)` comment; it
has not been revisited in Sprints 20–25.

Current workspace dependencies include `tracing-subscriber` but do NOT include
`opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp`, or `tracing-opentelemetry`.
Adding these requires `[workspace.dependencies]` entries and ADR 0006 Rule A
justifications (all four pass; see §6).

**What is missing:** The four OpenTelemetry crates, the OTLP layer construction in
`init_tracing`, and a guard that falls back to stdout-only when the env var is absent.

### §4.2 Testcontainers Integration Test

**File:** `/Users/dmytro.chystiakov/Projects/mg-onchain-analysis/crates/server/src/bin/onchain_validate.rs`

**State: PARTIALLY FUNCTIONAL.** The `test-containers` feature is declared in
`crates/server/Cargo.toml` (lines 17–24) and gates `testcontainers 0.23` plus
`testcontainers-modules 0.11`. Two `#[ignore]` tests exist in `onchain_validate.rs`:

- `docker_mode_smoke` (line ~981): starts a Postgres container, runs all 16 migrations
  (V00001–V00016 at time of writing; V00017 was added later and should now be 17), injects
  three synthetic baseline rows, verifies `COUNT(*) = 3`. **Functional for the migration
  path**, but does not exercise the REST layer.
- `docker_dispatcher_all_13_detectors` (line ~1073): constructs all 13 detectors with mock
  stores and dispatches them against a freshly-migrated empty DB, asserting `Ok(vec![])`.
  **Functional as a detector construction smoke test**, but the summary table falls back to
  config-only severity checks rather than asserting on actual emitted `AnomalyEvent`s, and
  the REST `/v1/anomaly_events` endpoint is not queried.

**What is missing:** A production smoke test in `crates/server/tests/production_smoke_test.rs`
that exercises the full pipeline end-to-end: migration, detector dispatch, persisted
`AnomalyEvent` rows queryable via the gateway's REST endpoint. That file does not exist.
The existing `#[ignore]` tests remain as they are; the new test is a separate artefact.

### §4.3 Health Endpoint

**File:** `/Users/dmytro.chystiakov/Projects/mg-onchain-analysis/crates/gateway/src/routes/health.rs`

**State: FUNCTIONAL (shallow).** The handler returns `HealthResponse` with `storage`
(real `SELECT 1` with a 500ms timeout), `registry` (always "ok" — Phase 3 placeholder per
line 82), `scoring: "ok"` and `detectors: "ok"` (both hardcoded strings). The route is
registered at `GET /health` in `build_router` (mod.rs line 27). Returns HTTP 503 when
storage is degraded.

**What is missing:** Chain-adapter liveness signal (is the Yellowstone gRPC or Ethereum WS
connected?) and a version/build field in the response body. The `registry_detail` check
at line 82 is explicitly a "Phase 3 TODO" stub. For Sprint 26, adding chain-adapter status
enrichment to the `HealthResponse` is the primary gap; the underlying storage check is
already correct.

### §4.4 Prometheus Metrics

**File:** `/Users/dmytro.chystiakov/Projects/mg-onchain-analysis/crates/gateway/src/metrics.rs`
**File:** `/Users/dmytro.chystiakov/Projects/mg-onchain-analysis/crates/server/src/streaming_metrics.rs`
**File:** `/Users/dmytro.chystiakov/Projects/mg-onchain-analysis/crates/gateway/src/routes/metrics_handler.rs`

**State: PARTIALLY FUNCTIONAL.** `GatewayMetrics` is fully registered and its
`encode_text()` method is exposed at `GET /metrics`. The handler returns the correct
`text/plain; version=0.0.4` content type. `GatewayMetrics` includes `detector_invocations_total`,
`http_requests_total`, and several WS counters.

`StreamingMetrics` is also fully registered with a rich set of streaming-scheduler counters
(`streaming_evaluations_total`, per-detector duration histograms, etc.), but its `registry`
is never merged into the `/metrics` response. The `metrics_handler` reads
`state.metrics.encode_text()` which only encodes `GatewayMetrics::registry`. A consumer
scraping `/metrics` today sees gateway-layer metrics only; streaming-scheduler metrics are
invisible.

**What is missing:** Merging `StreamingMetrics::registry` families into the `/metrics`
response. The design in §5.3 specifies adding three additional counters not present in
either registry: `anomalies_emitted_total{detector,chain,severity}`,
`chain_adapter_events_processed_total{chain,event_type}`, and
`db_query_duration_seconds_bucket`. The `prometheus = "0.13"` crate is already a direct
dependency in `crates/server/Cargo.toml` (line 64). No additional Cargo dep is needed for
metrics; the gap is plumbing, not dependencies.

### §4.5 Backfill Smoke Test Runbook

**State: MISSING.** The individual chain runbooks (`infra/ethereum-node/README.md` and
`infra/solana-validator/README.md`) document node setup and healthchecks but contain no
documented procedure for catch-up backfill: no CLI invocation, no expected wall-clock, no
OOM guidance, no verification steps to confirm convergence to chain head. The
`crates/chain-adapter` backfill API exists but is not surfaced as an operator-facing CLI
flag or runbook entry. Sprint 26 writes this runbook section.

### §4.6 `infra/docker-compose.prod.yml` and `infra/PRODUCTION.md`

**State: MISSING.** Neither file exists. The `infra/` directory contains
`ethereum-node/docker-compose.yml` (a per-service compose for the Reth+Lighthouse pair
only) and `solana-validator/` (systemd-based, not compose). There is no top-level compose
file that orchestrates all services together, and no operator playbook for a first
end-to-end deployment. Sprint 26 creates both.

---

## §5 Deliverable Specs

### §5.1 OTLP Exporter Wire-Up

The OTLP exporter is admitted under ADR 0006 Rule A without requiring a new ADR amendment.
OpenTelemetry is an open standard governed by the CNCF
(https://opentelemetry.io/docs/specs/otlp/); the four crates implement the published
protocol spec, not any vendor's proprietary SDK. The wire format (Protobuf over gRPC) is
the same public spec infrastructure (`tonic`, `prost`) already present in the workspace.

**New workspace deps (see §6 for full justification):**

```toml
opentelemetry            = "0.27"
opentelemetry_sdk        = "0.27"
opentelemetry-otlp       = { version = "0.27", features = ["grpc-tonic"] }
tracing-opentelemetry    = "0.28"
```

**Implementation in `crates/server/src/init/tracing_init.rs`:**

The existing `TODO(sprint-20)` block is replaced. When `OTEL_EXPORTER_OTLP_ENDPOINT` is
set (or `config.observability.otlp_endpoint` is populated), `init_tracing` constructs an
OTLP exporter via `opentelemetry-otlp` and attaches a `tracing_opentelemetry::layer()` to
the subscriber registry. When the env var is absent, only the existing `fmt::layer()` is
installed; the function signature and return type are unchanged.

A critical constraint from ADR 0006: `opentelemetry-otlp` must be configured with the
`grpc-tonic` feature flag, which uses `tonic` already in our workspace. The alternative
`hyper`/`http` feature would pull in a distinct hyper build. Tonic is the right choice for
consistency with the Yellowstone gRPC client.

**Span naming convention:** spans emitted via `#[instrument]` in detector code use the
default tracing span names (function name). The service-level attributes added to the OTLP
resource are: `service.name = "onchain-service"`, `service.version = env!("CARGO_PKG_VERSION")`.
Detector-specific attributes follow the `mg.detector.*` namespace (see §11.2).

**Integration test:** the existing `docker_dispatcher_all_13_detectors` test is left as a
unit-level smoke. Full OTLP emission verification is not a Sprint 26 gate — that requires
a mock OTLP collector container which adds non-trivial test infrastructure. Sprint 26 gate
is: the OTLP layer compiles, `init_tracing` does not panic when the env var is set to a
local address, and the workspace `cargo clippy --workspace --all-targets -- -D warnings`
remains clean.

### §5.2 Live Testcontainers Postgres Integration Test

**New file:** `crates/server/tests/production_smoke_test.rs`

This test is gated `#[cfg(feature = "test-containers")]` and marked `#[ignore]` per
Gotcha #13. It is run explicitly as part of the sprint-close gate:

```
cargo test --features test-containers \
  -p mg-onchain-server \
  production_smoke_test -- --ignored --nocapture
```

The test executes the following sequence, building on the infrastructure already proved
out in `docker_dispatcher_all_13_detectors`:

1. Start a Postgres container and run all 17 migrations (V00001–V00017).
2. Build a `PgStore` + `TokenRegistry` + mock graph stores + `MockTokenPriceProvider`.
3. Inject the three canonical synthetic baseline rows (established, rug, honeypot) via
   the same `inject_baseline` helper already in `onchain_validate.rs`.
4. Dispatch all 13 detectors via `Detector::evaluate` for the `synthetic_rug_baseline`
   token. The rug-pull token should cause D02 (`rug_pull_lp_drain`) to emit at least one
   `AnomalyEvent` when the pool row has `lp_burned_pct = 0.0` (zero liquidity locked).
   Insert that event directly into the `anomaly_events` table via `PgStore::insert_anomaly_events`.
5. Construct an `AppState` (bypassing JWT key loading by providing a test key pair via
   `JwtKeys::for_testing()` if it exists, or a generated ephemeral key).
6. Start the axum router via `axum_test::TestClient` (or `reqwest` against a bound port)
   and issue `GET /v1/anomaly_events?chain=solana&limit=10`.
7. Assert that the response contains at least one event with `detector_id` matching
   `"rug_pull_lp_drain"` and `confidence >= 0.6`.
8. Assert `GET /health` returns `{ "status": "ok" }` (200 OK) against the same AppState.
9. Assert `GET /metrics` returns a non-empty Prometheus text body containing
   `http_requests_total`.

The test is not a replacement for the existing `docker_dispatcher_all_13_detectors` test;
both tests remain. The new test exercises the REST layer on top of the already-validated
detector dispatch path.

**Disk note (Gotcha #112):** the production smoke test starts a Postgres container but not
Reth/Solana nodes. The disk footprint is `testcontainers` + `bollard` only (~2–3 GB in
`target/`). This is well under the threshold that caused disk-pressure incidents in
Sprint 25. The test does not build the full binary; it imports `mg-onchain-gateway` as a
library, which is already compiled for the workspace.

### §5.3 Health and Prometheus Metrics Endpoints

#### Health endpoint enrichment

`crates/gateway/src/routes/health.rs` adds a `chain_adapter_status` field to
`HealthResponse`. This field is a `Vec<ChainAdapterStatus>` where each entry reports:

```rust
pub struct ChainAdapterStatus {
    pub chain: String,
    pub connected: bool,
    pub last_slot_or_block: Option<u64>,
    pub detail: Option<String>,
}
```

The `AppState` must expose a method `chain_adapter_statuses() -> Vec<ChainAdapterStatus>`
that the health handler calls. Implementation: `MultiChainCoordinator` already tracks
per-chain adapter state; the coordinator exposes a `connection_statuses()` method (or
equivalent). The health handler calls this with a 1-second timeout; if the coordinator
does not respond within the timeout, the chain entry is reported `connected: false` with
`detail: "status check timed out"`.

The `HealthResponse` also gains `"version": env!("CARGO_PKG_VERSION")` and
`"build_commit": option_env!("GIT_SHA")` (set by the Docker build via `--build-arg`).

The depth toggle from §11.4 is implemented as a `shallow` query parameter:
`GET /health?shallow=true` skips the `SELECT 1` storage check and the chain-adapter
liveness check, returning `{ "status": "ok" }` immediately. Useful for load-balancer
TCP probes.

#### Merged `/metrics` handler

The `metrics_handler` in `crates/gateway/src/routes/metrics_handler.rs` is updated to
merge both Prometheus registries. `AppState` gains a field `streaming_metrics:
Arc<StreamingMetrics>`. The handler calls `encode_text()` on both registries and
concatenates the outputs. Since both use isolated registries with disjoint metric names
there is no risk of duplicate registration errors.

Three new counters are added, one in each crate as appropriate:

1. `anomalies_emitted_total{detector,chain,severity}` — incremented in the streaming
   scheduler when a non-empty `Vec<AnomalyEvent>` is persisted. Lives in
   `StreamingMetrics`.
2. `chain_adapter_events_processed_total{chain,event_type}` — incremented in
   `crates/chain-adapter` at the event boundary (one increment per `Event` variant
   dispatched). The `chain-adapter` crate currently does not link `prometheus`; this
   counter is surfaced via a callback closure injected at construction time (same pattern
   used by `AnomalyEventSink` trait) to avoid adding `prometheus` as a dep to
   `chain-adapter`.
3. `db_query_duration_seconds_bucket` — a `HistogramVec` labelled by `{operation}` (e.g.,
   `"insert_anomaly_events"`, `"fetch_top_holders"`) incremented in `crates/storage`.
   Same callback pattern: `storage` crate does not link `prometheus`; the histogram
   observer is injected via a `StorageMetrics` trait object at `PgStore` construction.

These additions are the only new counters. The existing rich set of counters in
`GatewayMetrics` and `StreamingMetrics` is preserved unchanged.

### §5.4 `infra/docker-compose.prod.yml`

The file is created at `infra/docker-compose.prod.yml`. It references the existing
per-service configs but adds `onchain-service` and the optional `otel-collector` on top.
Key design choices:

**Service definitions:**

- `ethereum-node`: inlined from `infra/ethereum-node/docker-compose.yml`; the Reth +
  Lighthouse pair becomes a nested `depends_on` group. The Ethereum node is optional:
  operators who only run Solana detectors may comment it out.
- `solana-node`: a single service entry pointing at an operator-managed Agave node with
  the Yellowstone plugin. Unlike Reth, Agave is not distributed as a stable Docker image;
  the compose entry uses a `build:` context pointing at `infra/solana-validator/` with
  a `Dockerfile` that builds Agave + the Yellowstone plugin from source. Build time is
  long (~30–60 minutes) but reproducible. Operators with an already-running validator
  may replace this service with `network_mode: host` and point `SOLANA_GRPC_URL` at the
  existing process.
- `postgres`: `image: postgres:16` (official image). Health-checked via
  `pg_isready -U ${POSTGRES_USER}`. Data volume at `${POSTGRES_DATA_DIR}`.
- `onchain-service`: built from the workspace root `Dockerfile`. Depends on `postgres`
  (health), and on `ethereum-node`/`solana-node` if enabled. Port mappings per §11.6.
- `otel-collector`: `image: otel/opentelemetry-collector-contrib:0.100.0` (pinned).
  Commented out by default per §11.11. Receives OTLP gRPC on port 4317 (internal); the
  operator provides `infra/otel-collector-config.yaml` to route spans to their preferred
  backend.

**Network topology:** all services share an internal Docker bridge network
(`mg-onchain-internal`). Only `onchain-service` ports are mapped to host interfaces:
`:8080` (REST) and `:8081` (WS) are the only externally reachable ports. Postgres (5432),
the OTLP collector (4317), and the node RPC ports (8545, 8546, 10000, 8899) are
internal-only. P2P ports for Reth (30303) and Agave (8001, 8004) are mapped
`0.0.0.0:PORT:PORT` as they require external reachability for peer discovery.

**Volume strategy:** each stateful service declares a named volume with a configurable
host path via `.env`. Operators bind-mount NVMe volumes by setting `POSTGRES_DATA_DIR`,
`RETH_DATA_DIR`, `SOLANA_LEDGER_DIR`, etc. in the `.env` file.

**Health checks:** every service has a Docker healthcheck with `start_period` tuned to
reality: `postgres` 30s (fast to start), `reth` 120s (snap sync may take hours but health
is checked at startup, not sync completion), `onchain-service` 60s (migration + startup),
`solana-node` 60s (validator boot; actual sync is asynchronous).

**Log rotation:** all services use `logging.driver: json-file` with `max-size: 100m` and
`max-file: 5` per §11.10 recommendation.

**Environment variable surface:** tunable vars documented in `infra/PRODUCTION.md §11`.
Key vars: `POSTGRES_URL`, `SOLANA_GRPC_URL`, `ETHEREUM_WS_URL`, `OTEL_EXPORTER_OTLP_ENDPOINT`,
`JWT_SIGNING_KEY_PATH`, `RUST_LOG`. All secrets are supplied via `.env` file (`.gitignore`'d);
a `.env.example` with safe placeholder values is provided.

### §5.5 Backfill Smoke Test Runbook

The runbook lives in `infra/PRODUCTION.md §7 — Backfill`. It covers two chains:

**Solana backfill.** Yellowstone gRPC does not support historical streaming; backfill uses
the Solana JSON-RPC `getBlock` + `getSignaturesForAddress` paths. The `chain-adapter`
backfill mode is invoked via:

```bash
onchain-service \
  --config config/service.toml \
  --backfill-chain solana \
  --backfill-from-slot <SLOT_7_DAYS_AGO> \
  --backfill-to-slot   latest \
  --backfill-batch-size 100
```

Approximate wall-clock at 100 blocks/batch: 7 days of Solana (≈10 M slots) takes 8–16
hours depending on RPC response time. OOM prevention: the backfill path processes one
batch in memory at a time; `batch_size_slots` controls peak RSS. Checkpoint resumption
is provided by the `adapter_checkpoints` Postgres table (V00001 migration); a restart
picks up from the last committed checkpoint automatically.

**Ethereum backfill.** Uses `eth_getLogs` with block-range pagination. The config key
`chains.ethereum.backfill_batch_size_blocks` (default 2000 per the existing
`EthereumAdapterConfig`) controls batch granularity. Invoked via:

```bash
onchain-service \
  --config config/service.toml \
  --backfill-chain ethereum \
  --backfill-from-block <BLOCK_7_DAYS_AGO> \
  --backfill-to-block   latest \
  --backfill-batch-size 2000
```

Approximate wall-clock: 7 days of Ethereum (≈50 000 blocks, ~30 GB of log data) takes
2–6 hours depending on Reth `eth_getLogs` throughput. Checkpoint resumption is identical
to Solana.

**Verification procedure:**

```bash
# After backfill completes, verify event counts in Postgres:
psql $POSTGRES_URL -c "
SELECT chain, COUNT(*) as events, MIN(block_time) as earliest, MAX(block_time) as latest
FROM anomaly_events
GROUP BY chain;
"
# Expected: Solana row with events > 0; Ethereum row with events > 0 if any
# detectors fired. A completely empty result after 7-day backfill indicates
# a detector config issue or a chain without any matching events (valid if
# the token set is small).

# Verify checkpoint state:
psql $POSTGRES_URL -c "SELECT * FROM adapter_checkpoints ORDER BY chain;"
# last_processed_slot / last_processed_block must match --backfill-to-slot/block values.
```

### §5.6 `infra/PRODUCTION.md` Deployment Doc

The file is created at `infra/PRODUCTION.md`. It is the authoritative operator playbook.
Section outline:

1. **Prerequisites.** Docker Engine 24+, Docker Compose v2, NVMe volumes, firewall rules
   for P2P ports (30303 TCP+UDP for Reth, 8001/8004 UDP for Agave).
2. **Hardware BOM.** Per-service minimum and recommended specs, table format. Single-box
   v1 target per §11.8 recommendation: a 32-core / 512 GB RAM / 6 TB NVMe server
   (e.g., Hetzner AX161 or OVHcloud Advance-3) is sufficient to run all four services
   simultaneously. The Solana node is the dominant consumer (256–512 GB RAM requirement
   per ADR 0003 §Hardware). Cost estimate: $400–800/mo bare-metal or dedicated cloud.
3. **Cold-start procedure.** Step-by-step from bare OS to first detector event.
4. **Expected first-run timeline.** Node sync is the bottleneck: Reth snap sync 4–8h;
   Agave snapshot 24–48h. `onchain-service` starts detecting events immediately after
   nodes reach the tip. Detectors fire on the first matching event after tip; expect
   first `AnomalyEvent` within minutes of reaching tip on a mainnet with active shitcoin
   activity.
5. **Readiness signals.** `GET /health` returns `chain_adapter_status[*].connected = true`
   for enabled chains. Prometheus metric `chain_adapter_events_processed_total` counter
   starts incrementing.
6. **Rollback procedure.** Stop `onchain-service`, revert to previous image tag, restart.
   Postgres schema is forwards-compatible: V00017 is the current head; no down-migrations
   are provided or needed for rollback (old code ignores new columns).
7. **Backfill.** See §5.5.
8. **Postgres backup.** Per §11.9 recommendation: `pg_dump` cron job, daily, compressed
   gzip to a separate volume or object storage. Sample crontab entry provided.
9. **Log retention.** Docker JSON driver with size-based rotation per §11.10.
10. **Secrets management.** All secrets in `.env` file; never committed. Operator
    generates `jwt.hex` (for Reth Engine API), JWT signing key PEM, and Postgres
    credentials before first run. Upgrade path to Docker secrets mount documented.
11. **Tunable environment variables.** Full table of all `.env` keys with defaults and
    descriptions.

---

## §6 Workspace Dependency Additions (Proposed)

Each entry must pass ADR 0006 Rule A: it must implement a public, versioned specification
maintained independently of any single vendor. The bridge escape hatch is closed; no entry
qualifies as an exception to Rule A — all four below are generic spec implementations.

| Crate | Version | Rule A justification |
|---|---|---|
| `opentelemetry` | `0.27` | OpenTelemetry public specification (CNCF); https://opentelemetry.io/docs/specs/otel/ — defines the SDK API surface (Tracer, Span, Resource). Not vendor-specific. |
| `opentelemetry_sdk` | `0.27` | Same spec — the SDK implementation. Provides `BatchSpanProcessor`, `SdkTracerProvider`. |
| `opentelemetry-otlp` | `0.27` | OTLP protocol spec (CNCF); https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/protocol/otlp.md. Uses `tonic` (already in workspace) under the `grpc-tonic` feature. No new gRPC runtime added. |
| `tracing-opentelemetry` | `0.28` | Bridge between `tracing` (already in workspace) and OpenTelemetry SDK. Implements the `tracing::Subscriber` trait extension. No domain specificity. |

`axum-prometheus` is rejected in favour of hand-rolling the merged `/metrics` handler.
The `prometheus = "0.13"` crate is already in `crates/server/Cargo.toml` as a direct dep.
Adding `axum-prometheus` would introduce a new crate for a function that can be accomplished
with three lines of existing code. Fewer deps is strictly better under ADR 0006's spirit.

No new deps are needed for the backfill runbook or the docker-compose file.

The `opentelemetry` crate family versions must be compatible: `opentelemetry 0.27`,
`opentelemetry_sdk 0.27`, `opentelemetry-otlp 0.27`, and `tracing-opentelemetry 0.28` are
the current compatible generation as of the design date. Before implementation, the dev
agent must verify the current stable release on crates.io and adjust version numbers if a
newer minor release is available. The Rule A justification holds for any version of these
crates because the justification is based on the spec they implement, not the version
number.

---

## §7 Build and Test Gates — Sprint 26 Must Satisfy

All six gates must pass before Sprint 26 is marked closed. Gates are listed in dependency
order; gates 1–3 can be verified iteratively during implementation, gates 4–6 are sprint-close
verification steps.

**Gate 1: Workspace compile.**
```bash
cargo build --workspace --all-targets
```
Must complete without errors. Note: `--all-targets` includes the `onchain-validate` binary
which requires `testcontainers` to be in `[dependencies]` (already gated behind the
`test-containers` feature in `Cargo.toml`). This gate does NOT require Docker.

**Gate 2: Clippy clean (workspace scope — non-negotiable).**
```bash
cargo clippy --workspace --all-targets -- -D warnings
```
Zero warnings. The sub-agent dispatch briefs for Sprint 26 MUST emphasise workspace scope
(`--workspace`) in capitals. This is a recurrent failure mode: Sprint 24 agent #5a and
Sprint 25 T25-5 first-attempt both ran clippy with `-p` scope and reported clean state
that the workspace check contradicted. Past briefs tightened the wording; Sprint 26 briefs
inherit the same tightening. See also §12 per-task sub-agent brief requirements.

**Gate 3: Existing test suite unchanged.**
```bash
cargo test --workspace
```
The existing 61+ test groups must pass. New tests added by Sprint 26 are in addition to
this baseline, not replacements.

**Gate 4: Live integration test.**
```bash
cargo test --features test-containers \
  -p mg-onchain-server \
  production_smoke_test -- --ignored --nocapture
```
Must pass end-to-end: Postgres container starts, migrations apply, detector dispatch runs,
REST assertion succeeds. Requires Docker daemon. This is the primary qualitative gate for
Sprint 26.

**Gate 5: Docker compose validation.**
```bash
docker compose -f infra/docker-compose.prod.yml config
```
Must complete without errors (compose file syntax valid). Does not require the images to
be built or nodes to be running.

**Gate 6: OTLP compile verification.**
```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
  cargo run --bin onchain-service -- --help
```
The binary must boot (showing help) without panicking when the OTLP endpoint env var is
set. This verifies the OTLP layer construction path does not crash on startup in the
absence of a live collector.

---

## §8 Consequences

### Positive

Any of the four consumer systems (bot-trader-2-0, mg-custody, market maker, exchange) can
adopt `onchain-service` without blockers from our side. The service is now:
- Deployable: `infra/docker-compose.prod.yml` is the single entry point.
- Observable: `/metrics` exposes both gateway and streaming-scheduler metrics; OTLP traces
  are available for any operator running a collector.
- Verifiable: the live integration test proves end-to-end correctness against a real
  Postgres schema, not just synthetic config validation.
- Operable: `infra/PRODUCTION.md` covers cold-start, readiness signals, backfill, backup,
  and rollback without requiring the operator to read seven separate documents.

### Negative

The workspace dependency count grows by four OpenTelemetry crates. These are all
generic-spec implementations (Rule A compliant) and introduce no vendor supply-chain
surface beyond what `tonic` and `prost` already represent. The marginal attack surface is
the OpenTelemetry span processing pipeline, which does not touch chain data or private key
material.

`infra/docker-compose.prod.yml` adds operational complexity versus a single-binary
deploy: operators now manage container lifecycle, volume mounts, and healthcheck states.
This complexity is unavoidable given the Solana + Ethereum node requirements (ADR 0003).

First-deploy timeline is dominated by chain sync rather than any software factor: Agave
snapshot sync takes 24–48h, Reth snap sync takes 4–8h. Operators must account for this
in deployment planning.

### Neutral

No detector logic changes. The 13 detectors, their thresholds, and their evidence schemas
are untouched. The 17 migrations are unchanged (next migration remains V00018). The Rust
test count grows by the tests in `production_smoke_test.rs` (estimated 5–8 new test
functions).

---

## §9 Migration Plan

Sprint 26 is greenfield production-readiness work. No existing functionality changes.
All additions are backwards-compatible:

- New workspace deps are additive; they do not modify the compile path for existing crates.
- The OTLP layer in `init_tracing` is conditional on env var presence; existing deployments
  without the var set are unaffected.
- The `HealthResponse` gains new optional fields; existing clients that deserialise a strict
  subset of the response are unaffected.
- The `/metrics` response gains new metric families; Prometheus scrapers ignore unknown
  metric names by default.
- `infra/docker-compose.prod.yml` is a new file; the existing per-service composes in
  `infra/ethereum-node/` and `infra/solana-validator/` are unchanged.

No database schema changes. The next migration (V00018) remains unwritten.

---

## §10 References

| # | Source | Used in |
|---|--------|---------|
| 1 | OpenTelemetry specification — https://opentelemetry.io/docs/specs/otel/ | §5.1, §6 |
| 2 | OTLP protocol specification — https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/protocol/otlp.md | §5.1, §6 |
| 3 | Prometheus exposition format v0.0.4 — https://prometheus.io/docs/instrumenting/exposition_formats/ | §4.4, §5.3 |
| 4 | testcontainers-rs — https://docs.rs/testcontainers/ | §4.2, §5.2 |
| 5 | testcontainers-modules (postgres) — https://docs.rs/testcontainers-modules/ | §4.2, §5.2 |
| 6 | `infra/ethereum-node/README.md` | §4.6, §5.5 |
| 7 | `infra/solana-validator/README.md` | §4.6, §5.5 |
| 8 | `docs/adr/0001-phase0-synthesis.md` §D8 | §3 (consumer delivery modes) |
| 9 | `docs/adr/0003-self-sovereign-infrastructure.md` | §3, §5.4, §5.6 |
| 10 | `docs/adr/0006-code-level-self-sovereignty.md` (post-amendment) | §6 |
| 11 | `memory/feedback_standalone_service_only.md` | §2.2 |
| 12 | `memory/feedback_kludge_test.md` | §6 (bridge escape hatch remains closed) |
| 13 | `memory/feedback_subagent_verification.md` | §7 (Gate 2), §12 |
| 14 | Reth Docker source build — `infra/ethereum-node/docker-compose.yml` | §5.4 |
| 15 | Yellowstone plugin from-source build — `infra/solana-validator/README.md` §11 | §5.4 |
| 16 | `crates/server/src/bin/onchain_validate.rs` (existing Docker tests) | §4.2, §5.2 |

---

## §11 Sign-Off Decisions

The following decisions require explicit user confirmation before implementation begins.
Each is stated as a question with a recommended answer and rationale.

**§11.1 OTLP transport: gRPC (port 4317) vs HTTP/protobuf (port 4318).**
Recommendation: gRPC default (`features = ["grpc-tonic"]`), env-overridable. Rationale:
`tonic` is already in the workspace for the Yellowstone client; the gRPC code path adds
zero new build machinery. The HTTP/protobuf option would require `reqwest` or `hyper` in
the OTLP crate's feature surface, which is redundant. Port 4317 is the OTLP gRPC standard;
4318 is HTTP/protobuf. Operators who front the collector with an HTTP reverse proxy can
configure the collector itself to accept HTTP on 4318 and forward over gRPC internally.

**§11.2 OTLP semantic conventions: standard OTel HTTP/RPC attributes + `mg.detector.*`
namespace vs a fully custom `mg.*` namespace for all attributes.**
Recommendation: follow OpenTelemetry HTTP semantic conventions (https://opentelemetry.io/docs/specs/semconv/http/)
for span names on REST endpoints, and use `mg.detector.id`, `mg.detector.chain`,
`mg.detector.confidence` for detector-specific attributes. Rationale: a consumer running
a commercial OTLP backend (Grafana Tempo, Honeycomb, Jaeger) gets automatic HTTP request
grouping out of the box with standard conventions. Custom `mg.*` attributes layer on top
without conflicting.

**§11.3 Prometheus metrics: hand-rolled merged `/metrics` handler vs `axum-prometheus`
crate.**
Recommendation: hand-roll. Rationale: `prometheus = "0.13"` is already a direct dep in
`crates/server/Cargo.toml`. The merged handler is ~15 lines of code (gather families from
both registries, concatenate the text output). Adding `axum-prometheus` would introduce
a new crate for a trivial function. Under ADR 0006's spirit, fewer deps is strictly better
when the alternative is trivial to implement. The hand-rolled handler is the right call.

**§11.4 Health endpoint depth: shallow vs deep; toggle mechanism.**
Recommendation: deep health check by default (checks Postgres pool + chain-adapter
connections), with `?shallow=true` query param for load-balancer TCP probes. Rationale:
a health endpoint that only proves the process responds is not useful for operator
diagnosis. The deep check (Postgres `SELECT 1` with 500ms timeout + chain-adapter
connection flag) surfaces the two most common failure modes. The shallow override avoids
health-check storms from high-frequency probes under load.

**§11.5 Docker-compose env-secret model: `.env` file with `${VAR}` interpolation vs Docker
secrets via mount.**
Recommendation: `.env` file for v1. Rationale: Docker secrets mounts require Swarm mode
or explicit `secrets:` declarations in compose v3 format; they add operational friction
for a single-node deploy. `.env` is simpler, well-understood, and easily audited
(operator can `cat .env` to verify). The upgrade path to Docker secrets is documented in
`infra/PRODUCTION.md §10` but not required for Sprint 26.

**§11.6 Consumer surface ports: single port `:8080` for both REST and WS vs separate
`:8080` (REST) and `:8081` (WS).**
Recommendation: single port `:8080` for v1. The WS handler is already served from the
same axum router at `/v1/ws/stream`. Splitting to `:8081` would require a second
`TcpListener` and a second axum router or a proxy layer. The rationale for splitting
would be to allow different firewall rules for streaming vs request/response traffic.
For v1 with four known consumers, a single port is simpler. The compose file exposes
`:8080` for both REST and WS; the `/metrics` endpoint is served on the same port and
considered internal-only by operator firewall configuration.

**§11.7 Backfill default `from-block` / `from-slot` strategy: 7 days vs 1 day vs
configurable.**
Recommendation: configurable, with 7-day default in `config/service.toml` documentation
and the runbook. Rationale: 7 days is enough history for all 13 current detectors
(longest window is D11 24h activity window; D04 pre-pump window is 60 min). Operators
with limited disk or time can override to 1 day. The `--backfill-from-slot` /
`--backfill-from-block` CLI flags already provide per-invocation override; the default
is documented rather than hardcoded.

**§11.8 First-deploy hardware target: single box vs split topology.**
Recommendation: single box for v1, with split topology documented in
`infra/PRODUCTION.md §2` as a scale-out option. Rationale: the dominant cost is the
Solana validator (256–512 GB RAM). A single 512 GB RAM / 32-core / 6 TB NVMe server
runs all four services simultaneously. Split topology (separate validator machine +
separate app machine) is the natural scale-out path when the user has more than one
consumer hitting the service at load, but it is not required for v1.

**§11.9 Postgres backup strategy: `pg_dump` cron vs WAL streaming vs none-yet.**
Recommendation: `pg_dump` cron (daily, gzip compressed) for v1. Rationale: the Postgres
data in Sprint 26 is detector state and anomaly events — important but reconstructible
via backfill if lost. WAL streaming provides point-in-time recovery and near-zero
RPO; it requires a standby or WAL archive target (S3 / object storage). For a v1
single-box deploy, the operational complexity of WAL streaming is not justified. Document
the upgrade path to WAL streaming in `infra/PRODUCTION.md §8`.

**§11.10 Log retention: Docker JSON driver with size-based rotation vs Loki/Vector
sidecar.**
Recommendation: Docker JSON driver (`logging.driver: json-file`, `max-size: 100m`,
`max-file: 5`) for v1. Rationale: the JSON driver requires no additional infrastructure.
500 MB of log retention per service is enough for post-incident diagnosis. Loki/Vector
adds two more containers to the compose stack and operator expertise overhead. Document
the upgrade path in `infra/PRODUCTION.md §9`.

**§11.11 OTLP collector in compose: ship one vs require operator to provide theirs.**
Recommendation: ship the `otel-collector` service in `infra/docker-compose.prod.yml` as
an optional, commented-out service. The operator uncomments the block and provides
`infra/otel-collector-config.yaml` to route spans to their preferred backend. Rationale:
an empty collector config (no exporters) is not useful; the operator must configure the
destination. Providing the collector container as a commented-out template lowers the
barrier for operators who want observability without prescribing a specific backend. The
`otel-collector-config.yaml.example` file ships with routes to Jaeger (self-hosted) and
OTLP/HTTP generic endpoint as examples.

---

## §12 Sub-Task Breakdown for Implementation

The following tasks are proposed for Sprint 26 implementation. They are listed in
dependency order; T26-1 must complete before T26-3 can begin (workspace dep changes
affect the tracing init). T26-2 through T26-5 are largely independent once T26-1's
workspace dep additions are in place.

**CRITICAL: Every dev-agent dispatch brief for Sprint 26 MUST include the following
anti-detour and verification requirements verbatim at the top:**

> ANTI-DETOUR: tools work, do NOT invoke skills, do NOT edit settings.json, do NOT try
> `fewer-permission-prompts`. Just do the work.
>
> SCOPE: `cargo clippy --workspace --all-targets -- -D warnings` — not `-p scope`,
> not `--lib` only. WORKSPACE SCOPE. This is the verification gate.
>
> DISK: prefer `cargo check --workspace --all-targets` for iterative verification.
> Reserve `cargo build --workspace --all-targets` for sprint-close. testcontainers +
> bollard add ~2–3 GB to target/.
>
> KLUDGE TEST: no bridges, no feature flags that gate vendor crates, no in-process
> linkage. OpenTelemetry crates are admitted under ADR 0006 Rule A (generic spec);
> document the rule reference in any Cargo.toml comments you add.
>
> OVER-REPORT HISTORY: S24 #5a and S25 T25-5 first-attempt both reported "clippy
> clean" on `-p` scope while workspace had warnings. Do not repeat this. Run
> `--workspace` explicitly.

---

**T26-1: Workspace dep additions + OTLP exporter wire-up**
- **Description:** Add `opentelemetry 0.27`, `opentelemetry_sdk 0.27`,
  `opentelemetry-otlp 0.27` (grpc-tonic feature), and `tracing-opentelemetry 0.28` to
  `[workspace.dependencies]` in the root `Cargo.toml`. Update
  `crates/server/src/init/tracing_init.rs` to attach the OTLP layer when
  `OTEL_EXPORTER_OTLP_ENDPOINT` is set; remove the `TODO(sprint-20)` stub comment. Add
  ADR 0006 Rule A attribution comments next to each new dep.
- **Estimated LOC delta:** +80 (Cargo.toml + tracing_init.rs changes; tests for the
  conditional layer path).
- **Dependencies:** none — this is the first task.
- **Agent type:** developer.

**T26-2: Merged `/metrics` endpoint + streaming metrics exposure**
- **Description:** Update `metrics_handler.rs` to concatenate `GatewayMetrics::registry`
  and `StreamingMetrics::registry` gather outputs. Add `streaming_metrics` field to
  `AppState`. Add `anomalies_emitted_total{detector,chain,severity}` counter to
  `StreamingMetrics`. Add `chain_adapter_events_processed_total{chain,event_type}` via
  callback closure injected into `MultiChainCoordinator` construction. Add
  `db_query_duration_seconds_bucket{operation}` via callback injected into `PgStore`.
- **Estimated LOC delta:** +120 (state.rs, metrics_handler.rs, streaming_metrics.rs,
  chain-adapter coordinator, storage/pg/store.rs, corresponding tests).
- **Dependencies:** T26-1 must be merged first (workspace compiles clean before this task
  begins).
- **Agent type:** developer.

**T26-3: Health endpoint enrichment**
- **Description:** Extend `HealthResponse` with `version`, `build_commit`, and
  `chain_adapter_status: Vec<ChainAdapterStatus>`. Add `?shallow=true` query param.
  Expose `MultiChainCoordinator::connection_statuses()` method. Wire into
  `health_handler` with a 1-second timeout per adapter.
- **Estimated LOC delta:** +90 (health.rs, state.rs, coordinator.rs changes, tests).
- **Dependencies:** T26-1 (workspace compiles clean).
- **Agent type:** developer.

**T26-4: Production smoke test**
- **Description:** Create `crates/server/tests/production_smoke_test.rs`. The test is
  gated `#[cfg(feature = "test-containers")]`, marked `#[ignore]`, and named
  `production_smoke_test`. It starts Postgres, runs migrations, injects the rug-pull
  baseline, dispatches D02, asserts a persisted `AnomalyEvent`, then queries
  `/v1/anomaly_events` via an axum test client and asserts the response contains the
  expected event. Also asserts `/health` returns 200 and `/metrics` returns a non-empty
  Prometheus text body. This task may build on the axum test utilities already used in
  existing gateway tests (check `crates/gateway/tests/` for patterns before writing new
  harness code).
- **Estimated LOC delta:** +180 (new test file; no production code changes required
  beyond T26-2 and T26-3 prerequisites).
- **Dependencies:** T26-2, T26-3 (AppState must include streaming_metrics for the
  /metrics assertion to cover both registries).
- **Agent type:** developer + systems-qa review.

**T26-5: `infra/docker-compose.prod.yml` + `.env.example`**
- **Description:** Create `infra/docker-compose.prod.yml` with the five service
  definitions described in §5.4. Create `infra/.env.example` documenting all tunable
  variables. Create `infra/otel-collector-config.yaml.example`. Verify
  `docker compose -f infra/docker-compose.prod.yml config` passes.
- **Estimated LOC delta:** +250 (compose file + env example + collector config example).
- **Dependencies:** none — compose file is independent of code changes.
- **Agent type:** developer (infrastructure).

**T26-6: `infra/PRODUCTION.md` + backfill runbook**
- **Description:** Create `infra/PRODUCTION.md` with the eleven sections described in
  §5.6 and §5.5. Reference the existing runbooks at `infra/ethereum-node/README.md` and
  `infra/solana-validator/README.md` rather than duplicating their content. Write the
  backfill CLI invocations and verification queries from §5.5.
- **Estimated LOC delta:** +450 (markdown only; no code).
- **Dependencies:** T26-5 must complete first so the `docker-compose.prod.yml` variable
  names referenced in PRODUCTION.md are final.
- **Agent type:** systems-qa (documentation).

**T26-7 (optional fast-follow): `infra/solana-validator/Dockerfile` for compose**
- **Description:** If the user chooses to keep the Solana node in the compose stack
  (rather than treating it as an operator-managed external process), write a `Dockerfile`
  in `infra/solana-validator/` that builds Agave + Yellowstone plugin from source. This
  mirrors the existing `infra/ethereum-node/docker-compose.yml` pattern. Estimated 6–8
  hours of initial build time; the image is pinned to the Agave + Yellowstone versions
  documented in `infra/solana-validator/README.md §4`.
- **Estimated LOC delta:** +80 (Dockerfile + build script).
- **Dependencies:** T26-5 (compose file provides the service definition context).
- **Agent type:** blockchain-engineer.

---

## §13 Open Questions and Out-of-Scope Items

The following are deliberately deferred from Sprint 26:

**Detector additions.** D14 (Token-2022 ConfidentialTransfer), D15 (NonTransferable), D16
(ScaledUiAmount), D17 (Pausable), Pump.fun graduation enrichment, D13 pool coverage
extension (Curve/Balancer/SushiSwap). All remain on the carry-forward list.

**Stage 2 FDR.** Smart-money calibration via the Barras 2010 FDR method remains
corpus-blocked. Minimum 30 days of live anomaly-event data at scale is required before
meaningful FDR correction is possible.

**Additional EVM chains.** Base, BSC, Arbitrum, Polygon. Phase 4 scope.

**ClickHouse.** The second storage tier (for high-volume time-series events) remains on
the Phase 3 roadmap. Sprint 26 remains Postgres-only.

**Multi-tenant quota enforcement.** The rate-limit infrastructure in
`crates/gateway/src/ratelimit.rs` exists but per-consumer quota configuration is not
documented or enforced. Sprint 27+.

**mTLS between `onchain-service` and node processes.** The current deployment uses
plaintext loopback sockets between services on the internal Docker bridge network. mTLS
is a Sprint 27+ hardening task.

**`eth_unsubscribe` on Receiver drop + mid-stream WS reconnect** (Sprint 17 TODOs in
`crates/chain-adapter/src/ethereum/`). Still on the carry-forward list; not addressed here.

**Cross-check test rename** (`*_topic0_matches_sol*` → drop `_sol`). Cosmetic; deferred.

**SPL Token layout decoders** in `crates/solana-types/` (deferred per design 0026 §11.6).
Still deferred.

**Decimals exact-fetch** (D11/D12/D13 SPEC-NOTEs from Sprint 21). Still deferred.

---

## §14 References (Full List)

| # | Source | URL / Path |
|---|--------|-----------|
| 1 | OpenTelemetry specification (CNCF) | https://opentelemetry.io/docs/specs/otel/ |
| 2 | OTLP protocol specification | https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/protocol/otlp.md |
| 3 | OpenTelemetry HTTP semantic conventions | https://opentelemetry.io/docs/specs/semconv/http/ |
| 4 | Prometheus exposition format v0.0.4 | https://prometheus.io/docs/instrumenting/exposition_formats/ |
| 5 | testcontainers-rs | https://docs.rs/testcontainers/0.23/ |
| 6 | testcontainers-modules (Postgres) | https://docs.rs/testcontainers-modules/0.11/ |
| 7 | `opentelemetry` crate | https://crates.io/crates/opentelemetry |
| 8 | `opentelemetry_sdk` crate | https://crates.io/crates/opentelemetry_sdk |
| 9 | `opentelemetry-otlp` crate | https://crates.io/crates/opentelemetry-otlp |
| 10 | `tracing-opentelemetry` crate | https://crates.io/crates/tracing-opentelemetry |
| 11 | Existing Reth + Lighthouse runbook | `infra/ethereum-node/README.md` |
| 12 | Existing Reth compose file | `infra/ethereum-node/docker-compose.yml` |
| 13 | Existing Solana validator runbook | `infra/solana-validator/README.md` |
| 14 | ADR 0001 §D8 — three delivery modes | `docs/adr/0001-phase0-synthesis.md` |
| 15 | ADR 0003 — self-sovereign infrastructure | `docs/adr/0003-self-sovereign-infrastructure.md` |
| 16 | ADR 0006 — code-level self-sovereignty (post-amendment) | `docs/adr/0006-code-level-self-sovereignty.md` |
| 17 | Design 0020 — server binary production entry | `docs/designs/0020-server-binary-production-entry.md` |
| 18 | Design 0026 — Solana stack divestment | `docs/designs/0026-solana-stack-divestment.md` |
| 19 | `memory/feedback_standalone_service_only.md` | `~/.claude/projects/…/memory/feedback_standalone_service_only.md` |
| 20 | `memory/feedback_kludge_test.md` | `~/.claude/projects/…/memory/feedback_kludge_test.md` |
| 21 | `memory/feedback_subagent_verification.md` | `~/.claude/projects/…/memory/feedback_subagent_verification.md` |
| 22 | `crates/server/src/init/tracing_init.rs` (current OTLP stub) | `crates/server/src/init/tracing_init.rs` |
| 23 | `crates/server/src/bin/onchain_validate.rs` (existing Docker tests) | `crates/server/src/bin/onchain_validate.rs` |
| 24 | `crates/gateway/src/routes/health.rs` | `crates/gateway/src/routes/health.rs` |
| 25 | `crates/gateway/src/metrics.rs` | `crates/gateway/src/metrics.rs` |
| 26 | `crates/server/src/streaming_metrics.rs` | `crates/server/src/streaming_metrics.rs` |
| 27 | Docker Compose v2 specification | https://docs.docker.com/compose/compose-file/ |
| 28 | otel/opentelemetry-collector-contrib image | https://hub.docker.com/r/otel/opentelemetry-collector-contrib |
