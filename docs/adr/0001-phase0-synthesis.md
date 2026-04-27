# ADR 0001 — Phase 0 Synthesis: Chain Priority, Ingestion Strategy, MVP Detector Shortlist

**Status:** Accepted
**Date:** 2026-04-21
**Inputs:** `research/01-market-scan.md` (v2, verified), `research/02-detection-methodology.md`
**Supersedes:** none

---

## Context

Phase 0 of `mg-onchain-analysis` required two parallel research deliverables before any code is written: a competitive market scan (24 products) and a detection methodology survey (10 anomaly categories, 20+ primary sources). Both are now complete. This ADR crystallises the architectural decisions those deliverables unlocked.

Four consumers depend on the outcome: `bot-trader-2-0` (Rust trading bot — embeds as crate), `mg-custody` (Rust custody service — REST/webhooks), market maker, exchange. API shape, chain priority, and detector set must serve all four.

## Decisions

### D1. Solana-first

**Decision:** Phase 1 (foundation) and Phase 2 (MVP detectors) target Solana exclusively. EVM chains (Ethereum, BSC, Base) are Phase 4.

**Rationale:**
- **Market gap.** RugCheck.xyz dominates Solana risk-scanning but is Solana-only; GoPlus/TokenSniffer own EVM breadth but their Solana coverage lags. No product serves both with comparable signal quality under a single schema. That is a defensible product wedge (see `research/01-market-scan.md` §6, gaps 1 and 5).
- **Shitcoin density.** Chainalysis (2025) found 3.59% of 2,063,519 tokens launched in 2024 meet pump-and-dump criteria; the majority launched on Solana (pump.fun / Raydium v4). ROI per detector is highest where anomaly base rate is highest.
- **Token-2022 extensions** (transfer fees, transfer hooks) are a legitimate-but-abusable primitive unique to Solana — this is a detectable signal class with no EVM equivalent (`research/02-detection-methodology.md` §2, §9).
- **No mempool simplification.** Solana has no public mempool; sandwich/MEV detection therefore only needs post-slot analysis — smaller scope than EVM for Phase 2. EVM mempool work is deferred to Phase 4.

**Trade-off accepted:** `mg-custody` needs Tron USDT flow analysis (AML/compliance) that this ADR defers to Phase 4+. Custody's compliance needs are met in MVP by integrating external screening (Chainalysis Screening Oracle is free on-chain) rather than building our own.

### D2. Ingestion via Yellowstone gRPC (open protocol, provider-agnostic)

**Decision:** Solana ingestion uses the Yellowstone gRPC Geyser plugin protocol (`github.com/rpcpool/yellowstone-grpc`). One Rust adapter works against any provider: Helius LaserStream, Triton Dragon's Mouth, or a self-hosted validator running the plugin.

**Rationale:**
- The market scan confirms Helius (LaserStream) and Triton (Dragon's Mouth) both expose the **same open-source protocol** under different brand names. We can write one gRPC client and swap providers by config.
- Provider independence: no lock-in, cost arbitrage possible, self-hosting path preserved for consumers with data-residency requirements (custody, exchange).
- Yellowstone gives us account updates + tx stream + slot metadata — covers all data primitives required by the MVP detectors (`research/02-detection-methodology.md` Cross-cutting A).
- Plain JSON-RPC + WebSocket is fallback/backfill only — streaming is gRPC.

**Alternatives rejected:**
- **Helius-only (vendor lock-in):** would tie ingestion to one provider; unacceptable for custody consumer.
- **Self-hosted validator only:** operationally heavy for Phase 1; defer to later when cost math justifies it.
- **Shyft / Bitquery / Syndica:** not evaluated in depth — only consider if Yellowstone costs become prohibitive.

### D3. Storage tier: Postgres (hot metadata) + ClickHouse (time-series)

**Decision:** Postgres for tokens/pools/checkpoints/detector_state (transactional, low volume). ClickHouse for transfers/swaps/pool_events/anomaly_events (high volume, columnar, analytical).

**Rationale:**
- **Data-primitive volume asymmetry** — Solana produces ~100× the event rate of Ethereum. ClickHouse is the obvious fit for wide time-series with partitioning by day + token.
- **Goldsky Mirror confirmed** as a viable "buy" path for EVM ingestion because it supports a **ClickHouse sink** natively. Same schema shape on our side regardless of whether events come from our own Solana Yellowstone adapter or from Goldsky Mirror on EVM — this is a Phase 4 optimisation but worth preserving compatibility now.
- **HolderSnapshot** is periodic + differential — expensive full snapshots + ClickHouse delta tables are the canonical shape (`research/02-detection-methodology.md` §10).

### D4. Detector output shape: `AnomalyEvent { confidence, severity, evidence }`

**Decision:** Every detector returns `(confidence: f64 [0..1], severity, evidence_bundle)`. No booleans. Thresholds are consumer-side.

**Rationale:**
- Direct instruction in `CLAUDE.md` §"Detector Rules". Reinforced by methodology survey (`research/02-detection-methodology.md` Cross-cutting C: sigmoid confidence mapping is the canonical pattern).
- Market-scan gap 6: "Every scanner today exposes boolean flags or an opaque score. None publishes per-signal precision/recall." — our `confidence + evidence` + published REFERENCES is a trust moat.
- Evidence bundle must include tx hashes, wallet addresses, computed metrics for human review — enables post-hoc calibration.

### D5. MVP detector set (Phase 2)

**Decision:** Ship 6 detectors in Phase 2, in priority order:

| # | Detector | Difficulty | Primary source cluster |
|---|----------|-----------|------------------------|
| 1 | Honeypot (simulation) | S | Torres et al. 2019 + Honeypot.is fork-state method |
| 2 | Rug Pull / LP Drain | M | Chainalysis 2025 + SolRPDS 2025 + LROO 2026 |
| 3 | Holder Concentration Shift | S–M | TM-RugPull 2026 + Brown 2023 + RugCheck exposure |
| 4 | Pump & Dump (volume/price spike) | M | Karbalaii 2025 + Bolz 2024 + La Morgia 2021 + Chainalysis 2025 |
| 5 | Wash Trading — Heuristic 1 | M | Chainalysis 2025 + Victor & Weintraud 2021 |
| 6 | Mint / Burn Anomaly | S | Xia et al. 2021 + Sun et al. 2024 |

**Deferred:**
- Sandwich / MEV victim → Phase 4 (EVM activates mempool + mev-inspect-py reference)
- Sybil / bundled-launch → Phase 3 (requires wallet funding graph)
- Smart Money tracking → Phase 3 (requires historical P&L cohort compute)
- Whale Movement → included as derived signal within Phase 2 detectors (large transfer is evidence, not standalone alert in MVP)

**Rationale:** Each Phase 2 detector has ≥2 independent cited sources AND is implementable from Yellowstone gRPC stream + pool state — no prerequisites beyond Phase 1 indexer.

### D6. Reference schema: RugCheck response shape as starting superset

**Decision:** The `AnomalyEvent` / token-risk schema in `crates/common` starts as a superset of the RugCheck v1 API response (fields live-verified during market-scan re-run), plus fields needed by Honeypot.is / GoPlus for EVM parity in Phase 4.

**Rationale:**
- RugCheck's live API response exposes exactly the signals our Phase 2 detectors produce: `mintAuthority`, `freezeAuthority`, `topHolders`, `lockers`, `markets.lp_burned_pct`, `transferFee`, `insiderNetworks`, `rugged` (ground-truth label), `launchpad`/`deployPlatform`. Starting from a proven schema is cheaper than designing one from scratch and then discovering omissions.
- Honeypot.is `simulationResult.{buyTax, sellTax, transferTax}` + `flags[]` give us the EVM honeypot shape that will be needed when Phase 4 activates.
- GoPlus `token_security` field list (30+ items across three categories) is the breadth reference for EVM — consulted when designing `crates/common` types, not copied.

### D7. Labelled fixture bootstrapping

**Decision:** Start a `tests/fixtures/solana/` corpus with 100 positive + 100 negative tokens by end of Phase 2. Sources: RugCheck's `rugged`-flagged tokens (positives), RugCheck's `verification.jup_verified` + `jup_strict` tokens (negatives), cross-referenced with Rekt News post-mortems where applicable.

**Rationale:**
- Methodology survey key gap 1: **no open-access Solana rug-pull labelled dataset at scale**. SolRPDS has 62,895 heuristic-flagged suspicious pools; SolRugDetector uses only 117 confirmed examples. We must bootstrap our own to calibrate confidence thresholds defensibly.
- RugCheck's `rugged` boolean gives a low-friction ground-truth label for positives; its `verification.jup_*` flags filter known-good tokens for negatives.
- Per `CLAUDE.md` §"Detector Rules", every detector needs a labelled positive + negative fixture. This corpus is the shared fixture pool.

### D8. Consumer delivery shape (reaffirm)

**Decision:** Three delivery modes from the same detector code:
- **Rust crate** — in-process for `bot-trader-2-0` (sub-ms detector calls, zero network hop)
- **REST** — for `mg-custody`, exchange (request/response, audit trail)
- **WebSocket streaming** — for market maker (subscribe to `AnomalyEvent` topic, consumer backpressure handled by gateway)

**Rationale:** Stated in initial tech decisions (see `memory/tech_decisions.md`), reinforced by market-scan gap 7 ("No product serves all four consumers simultaneously — consumers stitch 3+ vendors together"). This is our structural differentiation against SaaS-only competitors.

## Consequences

### What this ADR commits us to

- Cargo workspace with `common/`, `chain-adapter/`, `indexer/`, `storage/`, `detectors/`, `gateway/`, `server/` crates (Phase 1 start).
- Yellowstone gRPC adapter (one crate, provider-agnostic) is the first non-trivial code artefact.
- Postgres + ClickHouse dual-tier from day one (no "start simple with just Postgres and migrate later").
- Six specific detectors in Phase 2, each with threshold in `config/detectors.toml`, `REFERENCES.md` entry, labelled positive + negative fixture, unit test.
- Fixture corpus bootstrapping is a standing Phase 2 task (not deferred).

### What this ADR explicitly leaves open

- **EVM indexing: build vs buy (Goldsky Mirror).** Decided in Phase 4. Both paths land data in ClickHouse in the same schema.
- **Nansen Smart Money API integration.** Deferred to Phase 3/5 as optional enrichment. Now cheap ($0.05/call via x402) so not blocked by cost.
- **Self-hosted validator.** Preserved as an option; not required for Phase 1.
- **Chainalysis Screening Oracle integration for custody** — trivial on-chain read; add as a small `crates/screening` adapter when custody consumer wires up.
- **Tron and other chains** for custody compliance. Phase 4+.

### Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| RugCheck API schema drifts → our `AnomalyEvent` superset needs refactor | M | Version `crates/common` types; keep schema mapping explicit in adapter, not detector logic |
| Yellowstone stream disconnects / provider rate-limit under load | H | Systems-qa review before Phase 1 sign-off; two-provider fallback (Helius + Triton) with circuit breaker |
| Solana-only fixture corpus misses EVM-specific evasion patterns | M | Accept for Phase 2; add EVM fixtures concurrent with Phase 4 chain work |
| Detector thresholds calibrated in one market regime fail in another | H | Use rolling baselines / cross-token rank where possible (Cross-cutting C); regression-test fixtures across ≥2 market regimes before Phase 5 SDK cut |
| EigenPhi-style vendor disappearance affecting our dependencies | L | None of the six MVP detectors depends on a proprietary vendor data source; all derive from Yellowstone stream + RugCheck-style state reads we compute ourselves |

## Implementation starter list (becomes Phase 1 ROADMAP items)

1. Cargo workspace skeleton with the seven crates.
2. `crates/common` types: `AnomalyEvent`, `Transfer`, `Swap`, `PoolEvent`, `TokenMeta`, `HolderSnapshot`, `Severity`, `Confidence` — modelled after RugCheck schema + methodology §Cross-cutting A.
3. Yellowstone gRPC adapter crate behind `ChainAdapter` trait; provider-selectable (Helius / Triton / self-hosted) via config.
4. ClickHouse schema v1 for `transfers`, `swaps`, `pool_events`, `anomaly_events` with partition-by-day + order-by (token, block_time).
5. Postgres schema v1 for tokens, pools, deployer_clusters, adapter_checkpoints, audit.
6. Indexer with checkpoint + resume; reorg handling on `confirmed` commitment.
7. First integration test: backfill 1 hour of Raydium pool events, assert event count matches RugCheck API on the same token sample.

## References

- `research/01-market-scan.md` (verified 2026-04-21) — product landscape, signal frequency, gap analysis.
- `research/02-detection-methodology.md` — 10 anomaly categories with cited thresholds; MVP shortlist methodology.
- `CLAUDE.md` — detector discipline rules (cite source, emit confidence, labelled fixtures).
- `memory/tech_decisions.md` — upstream tech decisions (Rust 2024, dual delivery, standard transports).
