# MG Onchain Analysis

**Shared on-chain anomaly detection engine for the MG Trading Terminal ecosystem.**

Detects scams and abnormal token behaviour across Solana and EVM chains: rug pulls,
honeypots, pump & dump, wash trading, holder concentration, MEV/sandwich activity,
sybil clusters, drainer patterns, and abnormal mint/burn or liquidity events.

[![Rust](https://img.shields.io/badge/rust-2024-orange)]() [![Crates](https://img.shields.io/badge/crates-15-blue)]() [![Detectors](https://img.shields.io/badge/detectors-14-green)]() [![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

> **Status: active development.** Detector logic and the Solana ingestion path are
> the most mature areas; EVM ingestion and several detectors are still being wired
> end to end. See [ROADMAP.md](ROADMAP.md) and [SPRINTS.md](SPRINTS.md) for current state.

## Why This Exists

Four sibling systems make money decisions with **zero** on-chain visibility today —
a trading bot, a custody service, a market maker, and an exchange. This project is a
*shared capability*, not a bot add-on: one analysis engine, one data model, one API,
serving all four consumers equally.

**Design principle:** false positives are cheap, false negatives are expensive.
A missed rug pull costs the trading bot real money; a spurious alert costs a review
click. When in doubt, the detector fires with low confidence and lets the consumer
filter by threshold.

## Architecture

```
   Solana / EVM nodes
   (Yellowstone gRPC, JSON-RPC, WebSocket)
            │
   ┌────────▼─────────┐
   │  chain-adapter   │  ChainAdapter trait — per-chain ingestion, reorg handling
   └────────┬─────────┘
   ┌────────▼─────────┐
   │     indexer      │  block → events pipeline, backfill, checkpoints
   └────────┬─────────┘
   ┌────────▼─────────┐   ┌──────────────────┐
   │     storage      │◄──┤  token-registry  │  decimals, supply, LP, holders
   │  (PostgreSQL)    │   │  dex-adapter     │  Raydium / Orca / Uniswap v2-v4
   │                  │   │  graph           │  wallet clustering, smart money
   └────────┬─────────┘   └──────────────────┘
   ┌────────▼─────────┐
   │    detectors     │  D01–D14 — one signal per file, each with a cited source
   └────────┬─────────┘
   ┌────────▼─────────┐
   │     scoring      │  combine detector outputs → token risk score
   └────────┬─────────┘
   ┌────────▼─────────┐
   │     gateway      │  axum REST + WebSocket, OpenAPI 3.1
   └────────┬─────────┘
        consumers  (bot-trader, mg-custody, market maker, exchange)
                   via REST/WS or the client-sdk crate
```

## Detectors

Each detector implements a common `Detector` trait, emits a confidence score
(`0.0..1.0`) rather than a boolean, publishes its thresholds as config, and cites
its source in [REFERENCES.md](REFERENCES.md). No signal ships without a reference.

| ID  | Signal | ID  | Signal |
|-----|--------|-----|--------|
| D01 | Honeypot | D08 | Sybil clusters |
| D02 | Rug pull | D09 | Deployer change-point (BOCPD) |
| D03 | Holder concentration | D10 | Launch audit |
| D04 | Pump & dump | D11 | Synchronized activity |
| D05 | Wash trading | D12 | Permit2 drainer |
| D06 | Mint / burn anomaly | D13 | Sandwich MEV |
| D07 | Token-2022 withheld withdraw | D14 | Bridge drain |

## Crates (15)

| Crate | Purpose |
|-------|---------|
| `mg-onchain-common` | Core types: `Chain`, `Token`, `Transfer`, `Swap`, `PoolEvent`, `AnomalyEvent`, `Severity` |
| `mg-onchain-chain-adapter` | `ChainAdapter` trait + Solana / EVM implementations |
| `mg-onchain-indexer` | Block → events pipeline, reorg handling, backfill |
| `mg-onchain-token-registry` | Token metadata: decimals, supply, LP pools, holder snapshots |
| `mg-onchain-dex-adapter` | `DexAdapter` trait + Raydium, Orca, Uniswap v2/v3/v4 |
| `mg-onchain-graph` | Address graph: wallet clustering, whale tracking, smart-money labels |
| `mg-onchain-detectors` | `Detector` trait + D01–D14 |
| `mg-onchain-scoring` | Combine detector outputs → token risk score |
| `mg-onchain-storage` | PostgreSQL persistence (`sqlx`) |
| `mg-onchain-gateway` | axum REST + WebSocket API, OpenAPI spec |
| `mg-onchain-client-sdk` | Thin Rust client for consumers |
| `mg-onchain-server` | Binary entry point + background task orchestration |
| `mg-evm-types` / `mg-evm-types-macros` | In-tree EVM primitive types (ADR 0006) |
| `mg-solana-types` | In-tree Solana primitive types (ADR 0006) |

## Quick Start

Requires Rust (stable, edition 2024 — see `Cargo.toml` `rust-version`) and PostgreSQL.

```bash
git clone git@github.com:MG-Trading-Terminal/mg-onchain-analysis.git
cd mg-onchain-analysis

# Build everything
cargo build --release

# Run the test suite (unit tests, no external deps)
cargo test --workspace --lib

# Apply database migrations (sqlx)
psql "$DATABASE_URL" -f migrations/postgres/V00001__init.sql
# ...remaining migrations in migrations/postgres/ in order
```

### Score a real token from the CLI

`onchain-check-token` runs the detector math against live on-chain holder data
over standard JSON-RPC — no Postgres, no Docker:

```bash
cargo run --release --bin onchain-check-token -- --chain solana --token <MINT>
```

### Run the service

```bash
cargo run --release --bin onchain-service
```

## Binaries

| Binary | Purpose |
|--------|---------|
| `onchain-service` | Main service: ingestion + detectors + gateway |
| `onchain-cli` | Operator CLI |
| `onchain-check-token` | One-shot token score from live RPC data |
| `onchain-score-server` | Thin REST wrapper around `onchain-check-token` |
| `onchain-validate` | Validation harness (optional `test-containers` feature) |
| `onchain-calibrate` | Detector threshold calibration pass |

## Configuration

Service configuration lives in `config/` as TOML. Every file has a checked-in
`*.example` template. Secrets (RPC tokens, database credentials) are **never**
committed — supply them via environment variables, which override the TOML values
at startup. See `config/service.toml` for the documented defaults.

## Documentation

- [ROADMAP.md](ROADMAP.md) — phase-based plan and current status
- [REFERENCES.md](REFERENCES.md) — source for every detector threshold and heuristic
- [CHANGELOG.md](CHANGELOG.md) — dated change log
- [SPRINTS.md](SPRINTS.md) — sprint log with metrics
- [CONTRIBUTING.md](CONTRIBUTING.md) — crate layout, detector rules, dev workflow
- [SECURITY.md](SECURITY.md) — vulnerability reporting
- `docs/adr/` — architecture decision records
- `docs/designs/` — per-detector design documents

## License

[MIT](LICENSE).
