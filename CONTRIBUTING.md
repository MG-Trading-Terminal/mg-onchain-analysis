# Contributing to MG Onchain Analysis

## How the Code is Organized

```
crates/
  common/          ← Core types: Chain, Token, Transfer, Swap, AnomalyEvent (start here)
  chain-adapter/   ← ChainAdapter trait + per-chain ingestion (Solana, EVM)
  indexer/         ← Block → events pipeline, reorg handling, backfill
  token-registry/  ← Token metadata: decimals, supply, LP pools, holders
  dex-adapter/     ← DexAdapter trait + Raydium, Orca, Uniswap v2/v3/v4
  graph/           ← Address graph: wallet clustering, smart-money labels
  detectors/       ← Detector trait + D01–D14 (one signal per file)
  scoring/         ← Combine detector outputs → token risk score
  storage/         ← PostgreSQL persistence (sqlx)
  gateway/         ← axum REST + WebSocket API
  client-sdk/      ← Thin Rust client for consumers
  server/          ← Binary + background task orchestration
  evm-types/       ← In-tree EVM primitive types (ADR 0006)
  solana-types/    ← In-tree Solana primitive types (ADR 0006)
```

## Running Tests

```bash
# Unit tests (no external dependencies)
cargo test --workspace --lib

# Full suite — some integration tests use Docker testcontainers for PostgreSQL
cargo test --workspace

# A single crate
cargo test -p mg-onchain-detectors
```

## Code Style

- Rust 2024 edition, `anyhow::Result`, `tracing`.
- **NEVER use `f64` for prices, amounts, supplies, or liquidity** — use
  `rust_decimal::Decimal`, or `U256` / `u128` for raw token units.
- Amounts in JSON: string-encoded `Decimal`, never a float.
- Normalize addresses to chain-canonical form at the boundary (EVM checksum,
  Solana Base58).
- No hardcoded magic numbers for thresholds — every number is config-driven and
  has a [REFERENCES.md](REFERENCES.md) entry.
- `cargo fmt` and `cargo clippy` must be clean before a PR.

## Detector Rules (CRITICAL)

Every detector MUST:

1. **Cite its source.** Academic paper, public blog, dataset, or prior incident —
   tracked in [REFERENCES.md](REFERENCES.md). No signal ships without a reference.
2. **Publish its thresholds as config.** No magic constants in code. Thresholds
   live in `config/detectors.toml` with a comment explaining the reasoning.
3. **Have a labelled test fixture.** At least one known-positive and one
   known-negative token, captured as on-chain state snapshots in `tests/fixtures/`.
4. **Emit confidence, not booleans.** `AnomalyEvent { detector, confidence: 0.0..1.0,
   severity, evidence }`. The `scoring` crate combines them.
5. **Be reproducible.** Given the same block-range input, output MUST be deterministic.

## Adding a New Detector

1. Add `crates/detectors/src/dNN_<name>.rs` implementing the `Detector` trait.
2. Add its thresholds to `config/detectors.toml` with rationale comments.
3. Add a known-positive and known-negative fixture under `tests/fixtures/`.
4. Add a `REFERENCES.md` entry citing the source for the signal.
5. Write a design note in `docs/designs/` for non-trivial detectors.

## Workflow

- Read [ROADMAP.md](ROADMAP.md) and [SPRINTS.md](SPRINTS.md) before non-trivial work.
- Check [REFERENCES.md](REFERENCES.md) for prior art on the signal you are touching.
- `cargo test` must pass before marking anything done.
- Update [CHANGELOG.md](CHANGELOG.md) with every change (dated, categorized).

## Security

Do **not** open a public GitHub issue for security vulnerabilities — see
[SECURITY.md](SECURITY.md) for private reporting instructions.
