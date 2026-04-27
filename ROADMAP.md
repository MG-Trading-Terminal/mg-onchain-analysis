# MG Onchain Analysis — Roadmap

> Phase 0 (research + architecture decisions) blocks every other phase. No code until Phase 0 is closed.

## Phase 0 — Research & Architecture Decisions
**Status:** complete (2026-04-21)

- [x] Create `.claude/agents/` with 8 expert agents (pm, architect, developer, onchain-analyst, blockchain-engineer, data-engineer, security-researcher, systems-qa)
- [x] Write `CLAUDE.md` with project intent, stack, and discipline rules
- [x] **Research: existing on-chain analytics products** → `research/01-market-scan.md` (verified against live vendor docs/APIs)
- [x] **Research: anomaly detection methodologies** → `research/02-detection-methodology.md` (10 categories, 20+ cited sources)
- [x] Synthesize → MVP detector shortlist (6 detectors for Phase 2 — see ADR 0001 §D5)
- [x] Architecture decision record: single-chain-first vs multi-chain-first → **Solana-first** (ADR 0001 §D1)
- [x] Architecture decision record: ingestion (own node vs provider SaaS vs hybrid) per chain → **Yellowstone gRPC, provider-agnostic** for Solana; EVM buy-vs-build deferred to Phase 4 (ADR 0001 §D2)
- [x] First `ADR` in `docs/adr/` — `docs/adr/0001-phase0-synthesis.md`
- [ ] Data model sketch: `AnomalyEvent`, `Transfer`, `Swap`, `PoolEvent`, `TokenMeta`, `HolderSnapshot` — **moved to Phase 1** (implementation in `crates/common`)
- [ ] API contract draft: OpenAPI 3.1 for REST, event schema for WS — **moved to Phase 1** (follows data model)

**Exit criteria:** market scan + methodology doc complete; MVP detector list agreed; architecture ADR signed off. **MET.**

## Phase 1 — Foundation (single chain)
**Target chain:** Solana (ADR 0001 §D1)

- [x] Cargo workspace skeleton with 11 crate stubs (`common`, `chain-adapter`, `indexer`, `token-registry`, `dex-adapter`, `graph`, `detectors`, `storage`, `gateway`, `client-sdk`, `server`) — `scoring/` deferred to Phase 5
- [x] `crates/common` types: `AnomalyEvent`, `Transfer`, `Swap`, `PoolEvent`, `TokenMeta`, `HolderSnapshot`, `Severity`, `Confidence` — RugCheck-schema-derived superset (ADR 0001 §D6). 7 modules, 63 tests.
- [x] Postgres schema v1: tokens, pools, deployer_clusters, adapter_checkpoints, audit — `migrations/postgres/V00001__init.sql` via `sqlx migrate`.
- [x] Postgres event tables v1: transfers, swaps, pool_events, anomaly_events, holder_snapshots (+ `holder_snapshots_history`). Monthly partitioning on `block_time`, BRIN on time columns, B-tree on `(chain, token, block_time DESC)`. `holder_snapshots` uses two-table UPSERT pattern per ADR 0002. — `migrations/postgres/V00002__event_tables.sql`. ClickHouse dropped per **ADR 0002 (supersedes ADR 0001 §D3)** — single Postgres tier; TimescaleDB escape hatch documented for future scale.
- [x] Yellowstone gRPC adapter crate behind `ChainAdapter` trait; provider-selectable (Helius LaserStream / Triton Dragon's Mouth / self-hosted) via config (ADR 0001 §D2). 73 tests. Reorg markers on `SLOT_DEAD`; finalization on `SLOT_FINALIZED`.
- [ ] Indexer orchestration in `crates/indexer/` — currently empty stub. Wires chain-adapter `subscribe()` → ClickHouse batch inserter → Postgres `AsyncCheckpointStore`. Internal adapter checkpoint + reorg handling is done; crate-level glue is pending.
- [ ] Integration test: backfill 1 hour of Raydium pool events, assert event count matches RugCheck API on the same token sample. **BLOCKED — see notes below.**
- [ ] API contract draft: OpenAPI 3.1 for REST, event schema for WS (carried over from Phase 0)

**Remaining blockers for Phase 1 exit:**

1. **Raydium DEX decoding.** Task 3 deferred DEX-specific pool-layout parsing to `crates/dex-adapter` (Phase 2). Without it, the chain-adapter emits raw Swap events tagged by program ID but without structured Raydium pool reserves/amounts. Pre-req for the integration test to assert anything meaningful.
2. **Indexer orchestration crate.** Needs writing — not a single task, but the glue that turns subscribe + storage into a runnable pipeline.
3. **RugCheck ground-truth semantics.** `api.rugcheck.xyz/v1/tokens/{mint}/report` exposes risk-scoring state (top holders, LP status, mint authority), not historical event counts. The Task 5 assertion "event count matches RugCheck within ±5%" is under-specified — needs redefinition. Alternatives: compare against Helius enhanced-transactions API for ground-truth swap counts, or weaken the test to "events round-trip storage without loss."
4. **Infrastructure.** Requires a live Yellowstone endpoint (Helius/Triton auth token or self-hosted node), Postgres, ClickHouse. Not yet provisioned.
5. **API contract draft.** Independent of the integration test; pushed from Phase 0.

**Exit criteria:** Solana events flowing end-to-end into storage, reproducibly.

## Phase 2 — MVP Detectors (Solana)
**Finalized from ADR 0001 §D5. Six detectors, prioritised by cited prior-art strength + Rust implementation path.**

- [x] **#1 Honeypot (simulation)** — Torres et al. 2019 + Honeypot.is fork-state method. **Shipped static-only in Sprint 2** (P2-5). Simulation path deferred to Phase 3 (DG3 — requires dex-adapter instruction builders). Three compensating controls active: stricter thresholds, `reevaluation_interval_minutes=15`, S5 behavioural backup.
- [x] **#2 Rug Pull / LP Drain** — Chainalysis 2025 + SolRPDS 2025 + LROO 2026. **Shipped Sprint 3 P3-2** (dual-signal: Signal A event-based + Signal B state-based leading indicator via `MarketInfo.lockers[]` from P2-3). Post-review additions: 24h trickle-drain companion window, expiry-proximity bonus, NaN-safe comparator, C1 determinism via `DetectorContext.observed_at`. Known P4 follow-ups: Sprint 4 threshold calibration for established-protocol FPs (RAY/PYTH/TRUMP/MPLX per P3-4 corpus).
- [x] **#3 Holder Concentration Shift** — TM-RugPull 2026 + Brown 2023. **Shipped Sprint 3 P3-3** (3 signals: Gini delta, Top-10% delta, Absolute ceiling — all computed over Liquid-filtered holders via sidecar JOIN; closes WET-probe vesting-contract FP).
- [x] **#4 Pump & Dump (volume/price spike)** — Karbalaii 2025 + Bolz 2024 + La Morgia 2021 + Chainalysis 2025. **Shipped Sprint 4 P4-1** (3 signals: A spike-with-baseline + B burst_concentration_ratio fallback + C insider-sell amplifier with 3-tier graceful degradation; `is_established_protocol` total suppression on Signal C; market-cap filter $60M per Bolz 2024). Post-review: threshold tightened 0.90→0.70 on burst_concentration_threshold (E-D04-9 2h slow-pump coverage); C1 determinism fix `ctx.observed_at` carried.
- [x] **#5 Wash Trading — Heuristic 1** — Chainalysis 2025 + Victor & Weintraud 2021. **Shipped Sprint 4 P4-2** (3 signals: A same-address round trips 25-slots Solana-recalibrated + B N-wallet cluster flow balance proxy O(N²) capped at top-50 senders + C volume-inflation ratio severity amplifier).
- [x] **#6 Mint / Burn Anomaly** — Xia et al. 2021 + Sun et al. 2024. **Shipped Sprint 4 P4-3** (3 signals: A active mint authority + grace period, dampened on established protocols; B supply-change >=5% event, established-suppressed; C hidden mint pattern >=20% cumulative/30d, established-suppressed, 14-day genesis allowance). Partial coverage of E-D02-11 Token-2022 withdraw_withheld — **D07 candidate declared for Phase 3**.
- [x] **Fixture corpus:** 100 positive + 100 negative Solana tokens at `tests/fixtures/solana/`. Phase 1 (50+50, Sprint 3 P3-4) + Phase 2 (50+50, Sprint 4 P4-4). All fixtures carry expected verdicts for D01-D06. Phase 2 FP rates on 100-negative corpus: D01 3.0%, D02 4.0%, D03 1.0%, D04 1.0%, D05 0.0%, D06 0.0%. 38 calibration flags → 10 Sprint 5 action items.
- [ ] **`config/detectors.toml`** — all thresholds externalised, each with a REFERENCES.md-linked rationale comment.

Each detector must ship with: `Detector` trait impl, config threshold (not hardcoded), positive + negative fixture, `REFERENCES.md` entry, unit test, determinism test over a fixed block range.

**Deferred (not Phase 2):**
- Sandwich / MEV victim → Phase 4 (EVM only; Solana has no public mempool)
- Sybil / bundled-launch → Phase 3 (requires funding-graph module)
- Smart Money → Phase 3 (requires historical P&L cohort compute)
- Whale Movement → rolled in as evidence within Phase 2 detectors; standalone alert deferred to Phase 3

## Phase 3 — Graph & Smart Money + Self-hosted infra
- [x] **Infra track (ADR 0003):** `infra/solana-validator/` runbook — **shipped Sprint 3 P3-5** (8 files, 2,399 lines). Hardware BOM + 3 purchase paths, Agave v3.1.13 + yellowstone-grpc v12.2.0+solana.3.1.13 pinned, systemd unit, Prometheus+Grafana monitoring, disaster recovery. Flagged ADR 0003 amendment: Anza current docs recommend 512GB RAM (not 256GB) for full account indexes — runbook documents both options with trade-off. User runs actual node on their schedule.
- [x] **Wallet graph storage** — shipped Sprint 11 (V00011 migration + `crates/graph` extension with `GraphLabelStore` + `TypedEdgeStore` traits + `PgGraphLabelStore` + `PgTypedEdgeStore` + mocks). Indexer writer populates `DeployerOf` + `AuthorityOf` edges + `DeployerEOA` labels (S11-4).
- [x] **Clustering: common-funder** — shipped Sprint 11 (`ClusterDetector::run_common_funder` extended to write `FundingSource` labels via `GraphLabelStore`). [ ] synchronized-activity, bytecode-similarity — Sprint 12+.
- [ ] Smart-money labelling (historical P&L criterion) — Sprint 12+
- [x] **Sybil detection (D08)** — shipped Sprint 11 (`crates/detectors/src/d08_sybil.rs`). Signal: top-100 holders clustered by common funder ≥30% → bundled-launch AnomalyEvent. Cites Liu et al. 2025 (arxiv:2505.09313) + Chainalysis 2025. Positive + negative fixtures labelled.
- [x] **D01 Simulation infrastructure (Track B, B1+B2)** — `HttpPoolAccountProvider` CPMM + v4 real paths shipped Sprint 9. CPMM: B1 (Sprint 9) — `SolanaRpc::get_account_raw`, `CpmmPoolState` decoder (mainnet fixture-verified), ATA derivation, instruction builders. v4: B2 (Sprint 9) — `AmmV4PoolState` + OpenBook market state decoders, gateway + streaming worker wired with D01 cadence (default N=10). Sprint 10 gap #1: mainnet fixture capture caught +16 byte Pubkey offset bug in v4 decoder (u128 vs u64), would have silently broken D01 S6 on all v4 pools. Sprint 10 gap #2+#3: D01 e2e Docker test full implementation + sprint8_exit_test C5 hardened.
- [x] **Tarjan SCC + Johnson cycle detection for D05 Signal B** — shipped Sprint 12 T2-2. `crates/graph/src/cycles.rs` hand-rolled (no petgraph dep). Option D projection: direct `transfers` table query, no indexer changes. D05 Signal B fully replaced (old `compute_cluster_flows` proxy deleted). Design `docs/designs/0017-d05-signal-b-graph-cycles.md`. Follow-up: cycle_volume bottleneck-min vs avg (task #12).
- [x] **Bayesian changepoint detection on deployer behavior (D09 BOCPD)** — shipped Sprint 12 T2-1. Univariate Normal-Gamma composite score over 5 features, constant hazard 1/300. `crates/detectors/src/d09_deployer_changepoint.rs`. Migration V00013. Event-driven via `PoolInitializeHook`. Design `docs/designs/0016-detector-09-bocpd-deployer-changepoint.md`. Follow-up: server-binary wiring pending main.rs materialisation.
- [x] **D10 launch audit detector** — shipped Sprint 12. Tier-1 quick win from research. `initial_liquidity_sol < 5.0` AND `lp_locked_pct == 0.0` at first pool Initialize. `crates/detectors/src/d10_launch_audit.rs`. Uses `pools.initial_liquidity_usd` (V00013 column). Event-driven via `D10IndexerHook`.
- [x] **`token_risk_reports` Postgres migration V00012 + `upsert_token_risk_report`** — shipped Sprint 12. `crates/server/src/risk_report_store.rs` (trait + PgStore impl). Worker wiring at `worker.rs:432` with best-effort error semantics. Config `token_risk_reports_enabled: bool` default false (opt-in). Delta-short-circuit verified structurally precedes upsert.
- [x] **D01 streaming observability** — shipped Sprint 12. Per-detector latency histogram `streaming_detector_evaluation_duration_seconds{chain, detector_id}` in `StreamingMetrics`. Idle-timeout calibration method documented in `streaming_config.rs`; actual tuning blocked on captured 24h pump.fun dataset.
- [ ] **Synchronized-activity clustering** — Sprint 13+ (blocked on primary citation research — WTF Academy insufficient per `research/03-feature-gap-2026-04-24.md`)
- [ ] **Smart-money labelling** — Sprint 13+ (blocked on primary citation beyond Nansen marketing)
- [ ] **Pump.fun bonding-curve graduation stream** — Sprint 13+ (T1-2 from research; ~300 LOC; enrichment for all detectors)
- [ ] Vesting-unlock calendar signal (WET probe) — Sprint 13+
- [ ] Token-2022 ConfidentialTransfer / NonTransferable / ScaledUiAmount / Pausable extended support — Sprint 13+ (4 sub-detectors × ~400 LOC; large scope)

## Phase 4 — Additional Chains (all self-hosted per ADR 0003)
Priority TBD. Each chain needs its own self-hosted node — no Alchemy / Infura / QuickNode / Moralis. Candidates:
- [ ] `infra/ethereum-node/` (Geth or Reth, archive or pruned per detector needs) + Ethereum adapter
- [ ] BSC (Geth-compatible)
- [ ] Base (Reth / op-geth)
- [ ] Arbitrum (nitro)
- [ ] Polygon (Bor)
- [ ] Tron (USDT flow for custody; distinct node stack)

## Phase 5 — Scoring, SDK, Consumer Integration
- [x] `scoring/` crate: **Sprint 5 P5-1** — 9 modules, 2,750 LOC, 65 tests. Weighted-sum Option 2. RAVE 0.827/0.83 + WET 0.308/0.31 calibration (< 0.6% delta).
- [x] `client-sdk/` crate: **Sprint 5 P5-3** — 3,054 LOC, 34 tests. Zero server-crate deps. Ed25519 JWT + auto-reconnect WS.
- [x] WS streaming API with consumer backpressure handling: **Sprint 5 P5-2** — axum WS `/v1/ws/stream`, bounded send buffer with `LagNotice` frames.
- [x] OpenAPI 3.1 spec finalized: **Sprint 5 P5-2** — `docs/api/openapi.yaml` (1,753 lines, validates against meta-schema). Rust client shipped via `client-sdk`; TS client not planned (user decides per-consumer).
- [x] `client-sdk/` surface ready for any consumer adoption — **shipped Sprint 5**. Per user boundary 2026-04-21 broadened ("мы никуда не интегрируем! пока это будет работать как сервис"): consumers (bot-trader, custody, MM, exchange) adopt on their own timeline. Service runs standalone via `crates/server/` exposing the gateway API. NO writes to any consumer repo.

## Out of scope (explicit)
- Front-end / dashboard (add later if value clear)
- NFT analytics (different domain)
- Chain forensics / AML (covered by Chainalysis-class tools)
- Price oracles (use external — Pyth, Chainlink, CEX feeds)
