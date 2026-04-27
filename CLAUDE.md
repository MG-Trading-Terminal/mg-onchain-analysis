# MG Onchain Analysis

## What
Shared on-chain analytics library and service for the MeatGrinder ecosystem. Detects anomalies in tokens (primarily shitcoins on Solana, EVM chains, and others): rug pulls, honeypots, whale movements, pump&dump patterns, wash trading, MEV/sandwich activity, abnormal liquidity events, mint/burn anomalies, holder concentration shifts.

Consumed by four sibling systems that currently have **zero** on-chain visibility:
- `~/Projects/bot-trader-2-0` — Rust trading bot (integrates as Rust crate)
- `~/Projects/mg-custody` — Rust custody service (REST + webhook)
- Market maker
- Exchange

**Design principle:** this is a *shared capability*, not a bot add-on. API shape, performance envelope, and data model must serve all four consumers equally from day one.

## Stack
- **Core:** Rust 2024, `anyhow::Result`, `tracing`, `rust_decimal` for money.
- **Transport:** REST (OpenAPI 3.1) for request/response, WebSocket for streaming alerts, JSON-RPC outbound to blockchain nodes.
- **Storage:** PostgreSQL (metadata, detectors state) + ClickHouse (time-series events, transfers, trades). Decide per-detector which tier fits.
- **Streaming:** start with in-process channels; move to Redpanda/Kafka when multi-instance is needed.
- **Deployment:** standalone binary + Rust crates (reusable anywhere); Docker image for service mode.

## Structure (target; build incrementally)
```
crates/
  common/              # Chain, Token, Address, Transfer, PoolEvent, AnomalyEvent, Severity
  chain-adapter/       # ChainAdapter trait + per-chain implementations (Solana, Ethereum, BSC, Base, ...)
  indexer/             # Block → events pipeline, reorg handling, backfill, mempool (where supported)
  token-registry/      # Token metadata: decimals, supply, LP pools, holders snapshot
  dex-adapter/         # DexAdapter trait + Uniswap v2/v3/v4, Raydium, Orca, PancakeSwap, Jupiter
  graph/               # Address graph: wallet clustering, whale tracking, smart money labels
  detectors/            # Detector trait + individual detectors (one file per signal)
  scoring/             # Combine detector outputs → token risk score / anomaly confidence
  gateway/             # axum REST + WebSocket API, OpenAPI spec
  client-sdk/          # Thin Rust client for consumers (bot, custody, MM, exchange)
  storage/             # sqlx (Postgres) + clickhouse-rs wrappers
  server/              # Binary entry point, background task orchestration
```

## Commands
```bash
cargo build --release
cargo test
cargo test -p onchain-detectors
cargo check
cargo run --release --bin onchain-service
```

## Code Style
- Rust 2024 edition, `anyhow::Result`, `tracing`
- **NEVER `f64` for prices, amounts, supplies, liquidity** — `rust_decimal` or `U256` / `u128` for raw token units
- Amounts in JSON: string-encoded Decimal (never float)
- Addresses: normalize to chain-canonical form (checksum for EVM, Base58 for Solana) at the boundary
- Every detector implements a common `Detector` trait with explicit inputs, explicit thresholds (config, not hardcoded), and a cited rationale
- No hardcoded magic numbers for thresholds — every number has a `REFERENCES.md` entry

## Detector Rules (CRITICAL)

### Every detector MUST:
1. **Cite its source.** Academic paper, public blog, Dune dashboard, prior incident — tracked in `REFERENCES.md`. No signal ships without a reference.
2. **Publish its thresholds as config.** No magic constants in code. Thresholds live in `config/detectors.toml` with a comment explaining the reasoning.
3. **Have a labelled test fixture.** At least one known-positive and one known-negative token, captured as on-chain state snapshots, checked into `tests/fixtures/`.
4. **Emit confidence, not booleans.** `AnomalyEvent { detector, confidence: 0.0..1.0, severity, evidence }`. Let `scoring/` crate combine.
5. **Be reproducible.** Given the same block range input, output MUST be deterministic.

### False positives are cheap. False negatives are expensive.
When in doubt, fire the event with low confidence. Consumers can filter by threshold. A missed rug pull costs the trading bot real money; a spurious alert costs a review click.

## Multi-Chain Rules

Every chain lives behind `ChainAdapter` trait. Per-chain quirks are documented in the adapter crate:

### Solana
- High TPS, very high event volume — use Geyser plugin / Helius / Triton for streaming, not plain RPC polling
- Account model, not UTXO; SPL tokens via Token Program 2022
- Commitment: use `confirmed` for hot path, `finalized` for immutable records
- Raydium, Orca, Meteora, Jupiter aggregator — each has distinct pool layouts
- Token-2022 extensions (transfer fees, transfer hooks) are legitimate but also vectors for scams

### Ethereum / EVM (ETH, BSC, Base, Arbitrum, Polygon, ...)
- ERC-20 `Transfer(address,address,uint256)` is the workhorse event
- Uniswap v2: `Swap`, `Mint`, `Burn` on pair contract
- Uniswap v3: tick-based liquidity, `Swap` event has different signature
- Uniswap v4: hooks — custom logic per pool, harder to analyze
- Reorg: wait 12 confirmations for finality; deeper for L2s depending on prover
- Mempool via `eth_subscribe("newPendingTransactions")` or dedicated providers (Flashbots, bloXroute, Blocknative)

### Common pitfalls
- ERC-20 decimals differ per token — never hardcode 18
- Token transfers can be proxied (ERC-4337, meta-transactions) — follow the money, not the `from` field
- Honeypot tokens let you buy but revert on sell — detect by simulating a sell, not by reading events

## Research & References (MANDATORY)

This service makes financial decisions for four consumer systems. A false negative lets a scam through; a false positive blocks legitimate trades. Every threshold, formula, and heuristic must be defensible.

### REFERENCES.md
- Every detector's logic cites its source: paper, blog, dataset, or prior incident
- Table: `Detector | Signal | Source | Used In | Verified Against`
- No detector merges without a REFERENCES.md entry

### ROADMAP.md
- Phase-based plan. MVP before everything else.
- Phase 0: market research + architecture decision (before any code)
- Phase 1: indexer + chain adapter for ONE chain (Solana — most shitcoin density)
- Phase 2: core detectors (whale, LP rug, honeypot, pump)
- Phase 3: graph / smart money / clustering
- Phase 4: additional chains
- Phase 5: scoring, SDK, consumer integrations

### SPRINTS.md
- Sprint log: goals, completed, metrics (crates, tests, LOC, detectors)
- One sprint ≈ one theme (e.g. "Sprint 3: honeypot detection + Solana Raydium adapter")

### CHANGELOG.md
- Every change logged with date, category (Added/Changed/Fixed/Removed)
- Reference links for any external source used

### research/
- Raw research outputs live here: competitor teardowns, paper summaries, threshold experiments
- Markdown preferred; notebooks OK for EDA

## Workflow

### Before any non-trivial work
1. Read ROADMAP.md + SPRINTS.md to understand current state
2. Check REFERENCES.md for prior art on the signal you're about to touch
3. Enter plan mode for 3+ step tasks

### Implement
- Match patterns from sibling projects (`mg-custody`, `bot-trader-2-0`) — don't reinvent
- Every detector: trait impl + config threshold + unit test + fixture + REFERENCES entry
- `cargo test` must pass before marking anything done

### End of session
- Update CHANGELOG.md (what changed)
- Update ROADMAP.md (check off completed tasks, add discovered ones)
- Update SPRINTS.md (metrics)

## Related Projects
- `~/Projects/bot-trader-2-0` — trading bot, primary consumer, Rust/Bybit
- `~/Projects/mg-custody` — custody service, reference for Rust project layout, per-chain trait pattern, documentation discipline

## Expert Agents (slash commands)
- `/pm` — decomposition, roadmap, progress tracking, research coordination
- `/architect` — system design, API contracts, scalability across 4 consumers
- `/developer` — Rust implementation and code review
- `/onchain-analyst` — domain expert: what signals matter, how to detect anomalies, statistical rigor
- `/blockchain-engineer` — chain adapters, RPC, indexing, reorg handling, mempool
- `/data-engineer` — storage tiers, streaming, throughput, backfill strategy
- `/security-researcher` — scam patterns, rug/honeypot heuristics, contract risk
- `/systems-qa` — reliability, failure modes, reorg chaos testing, RPC outages
