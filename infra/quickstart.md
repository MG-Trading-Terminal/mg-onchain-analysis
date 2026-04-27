# Quickstart: validate mg-onchain-analysis in 10 minutes

**No Yellowstone validator. No Reth node. No external Postgres required** for Steps 1–2.

---

## 1. Self-contained harness (no infrastructure needed)

Validates that detector config loads correctly and all 4 fixture tokens map to
their expected severity levels. Runs entirely in-process — no Docker, no network.

```bash
cargo run --release --bin onchain-validate -- --no-docker
```

Expected output:

```
Token (name)                 Chain      Expected   Actual     Match?
--------------------------------------------------------------------
BONK (established)           solana     Low        Low        YES
USDC (established)           ethereum   Low        Low        YES
Synthetic rug pull           ethereum   Critical   Critical   YES
Synthetic honeypot           solana     Critical   Critical   YES

All 4 fixture tokens matched expected severity.
```

Verbose mode (print evidence + confidence ranges per token):

```bash
cargo run --release --bin onchain-validate -- --no-docker --verbose
```

Alternate fixture file:

```bash
cargo run --release --bin onchain-validate -- \
  --no-docker \
  --fixtures /path/to/my_tokens.json
```

Fixture JSON schema:

```json
[
  {
    "chain": "solana",
    "token": "<base58-mint>",
    "name": "Human label",
    "expected_severity": "Low | Medium | High | Critical",
    "synthetic_setup": "established_token_baseline | synthetic_rug_baseline | synthetic_honeypot_baseline"
  }
]
```

---

## 2. CLI smoke test (against a running service)

### 2a. Print supported chains + detector list (no service needed)

```bash
cargo run --release --bin onchain-cli -- info
```

### 2b. Start the service (requires Postgres)

In terminal 1 — start Postgres (Docker):

```bash
docker run --rm -d \
  -e POSTGRES_USER=onchain \
  -e POSTGRES_PASSWORD=onchain \
  -e POSTGRES_DB=onchain \
  -p 5432:5432 \
  postgres:16
```

In terminal 1 — start the service (Solana disabled to avoid needing a Yellowstone node):

```bash
cargo run --release --bin onchain-service -- \
  --config config/service.toml \
  --no-migrate
```

Or with auto-migrate:

```bash
DATABASE_URL=postgres://onchain:onchain@localhost/onchain \
cargo run --release --bin onchain-service -- --config config/service.toml
```

### 2b-2. Search by name (no service needed)

Resolve a token name or symbol to on-chain addresses using the Dexscreener public API.
No running service is required — this hits Dexscreener directly from the CLI.

```bash
# List candidates matching the ticker "OPG" with default $1000 min liquidity
cargo run --release --bin onchain-cli -- search OPG

# Increase limit, raise liquidity floor, machine-readable output
cargo run --release --bin onchain-cli -- search PEPE --limit 5 --min-liquidity-usd 100000 --format json

# Resolve name → top-TVL match → analyze automatically (requires running service)
cargo run --release --bin onchain-cli -- \
  --token-auth <your-jwt-token> \
  analyze-by-name OPG --auto-top

# Resolve name → fail with exit 5 if ambiguous (prompts to use --auto-top)
cargo run --release --bin onchain-cli -- \
  --token-auth <your-jwt-token> \
  analyze-by-name OPG
```

**Note:** Token name resolution uses the Dexscreener public API as a one-off
metadata-enrichment lookup. This is NOT in the detection hot path — ADR 0003
self-sovereign carve-out applies (same category as fixture capture via public RPC).
Dexscreener is never called from `crates/detectors/` or any production service code.

**Exit codes for search / analyze-by-name:**

| Code | Meaning |
|------|---------|
| 0 | Success |
| 4 | No token found matching query + min-liquidity filter |
| 5 | Multiple candidates — use --auto-top or specify --chain manually |

### 2c. Health check

In terminal 2:

```bash
cargo run --release --bin onchain-cli -- health
```

Expected:

```
Status:   ok
Storage:  ok
Scoring:  ok
Detectors:ok
Registry: ok
Uptime:   3s
```

### 2d. Analyze a token

```bash
cargo run --release --bin onchain-cli -- \
  --token-auth <your-jwt-token> \
  analyze \
  --chain solana \
  --token DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263
```

Optional flags:

```bash
  --format json        # raw JSON response
  --format summary     # one-liner: chain token score=X severity=Y
  --window-hours 48    # analysis window (1–168 hours, default 24)
```

---

## 3. Full production deployment

### 3a. Solana ingestion

Provision a self-hosted Yellowstone gRPC validator:

```
See: infra/solana-validator/README.md
```

In `config/service.toml`:

```toml
[chains.solana]
enabled = true
rpc_url = "http://<your-validator-ip>:10000"
```

### 3b. EVM ingestion

Per chain, provision a self-hosted Reth + Lighthouse pair:

```
See: infra/ethereum-node/README.md
```

In `config/service.toml` (example for Ethereum mainnet):

```toml
[chains.ethereum]
enabled = true
ws_url  = "ws://<your-reth-ip>:8546"
```

The service refuses to start if any enabled EVM chain has a placeholder `ws://127.0.0.1:854[6-9]` URL.

### 3c. Start the service

```bash
cargo run --release --bin onchain-service
```

Or the Docker image (when built):

```bash
docker run --rm \
  -e DATABASE_URL=postgres://onchain:onchain@db:5432/onchain \
  -p 8080:8080 \
  mg-onchain-service:latest
```

---

## 4. Endpoints

All endpoints are on the configured `gateway.bind_addr` (default `127.0.0.1:8080`).

```
POST   /v1/analyze            Analyze a token (requires analyze:write JWT scope)
GET    /v1/tokens/analyze     Legacy analyze endpoint (same auth)
GET    /v1/detectors          List loaded detectors + thresholds (requires read:events scope)
GET    /health                Liveness + component status (no auth)
GET    /ready                 Readiness probe (no auth)
GET    /metrics               Prometheus metrics (no auth)
WS     /v1/stream/events      WebSocket anomaly event stream (requires read:events scope)
```

### Analyze request body

```json
{
  "chain": "ethereum",
  "token": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
  "window_hours": 24
}
```

### Analyze response (abbreviated)

```json
{
  "chain": "ethereum",
  "token": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
  "evaluated_at": "2026-04-24T10:00:00Z",
  "aggregate_severity": "Low",
  "aggregate_confidence": 0.02,
  "analysis_duration_ms": 45,
  "detectors": [
    { "detector_id": "honeypot_sim",    "confidence": 0.0, "severity": "None",   "skipped": false },
    { "detector_id": "rug_pull_lp_drain","confidence": 0.0, "severity": "None",  "skipped": false },
    ...
  ]
}
```

---

## 5. Troubleshooting

**"ws_url placeholder" / service refuses to start**

Set a real self-hosted RPC endpoint per chain in `config/service.toml`. Or disable
the chain (`enabled = false`) until the node is ready.

**"failed to reach http://127.0.0.1:8080/health"**

The service is not running. Start it with `cargo run --bin onchain-service`.

**"failed to load detector config from config/detectors.toml"**

Run the CLI from the workspace root directory, or pass `--config` with an explicit
path.

**D10 signal_a_skipped in detector output**

D10 (`launch_audit`) Signal A requires `sol_price_usd` from the token registry. The
NoopD10Registry shim (Sprint 19) returns zero price, so Signal A is intentionally
skipped. Full enrichment lands in a future sprint (TODO sprint-20+ follow-up).

**High Rust-analyzer noise in IDE after trait/module changes**

Trust `cargo check`, not the IDE. Run `touch <changed-file> && cargo check` to flush
stale diagnostics. Confirmed RA-stale pattern 21+ times (gotcha #3).

**"smart_money config parse failed — labeller NOT started"**

Check `config/detectors.toml` for a `[smart_money_v1]` section. Missing section
is non-fatal; the smart-money labeller simply does not start.

---

## 6. What is NOT in production yet

| Item | Status | Sprint |
|------|--------|--------|
| D10 EVM Signal A (under-collat) | Skipped when `sol_price_usd = None` (NoopD10Registry) | Sprint 20+ |
| Decimals exact-fetch for D11/D12/D13 | Defaults: D11=9 / D12=18 / D13=propagation | Sprint 24+ |
| four.meme / clanker / Virtuals factory addresses | Partial on BSC/Base | Sprint 26+ |
| D01/D02 EVM threshold calibration | Heuristic (Solana-anchored) | Sprint 24+ |
| Reth ExEx `cfg(feature = "exex")` | Stub binary only | Sprint 25 |
| Stage 2 FDR (Barras 2010) | Data-blocked — needs 30d live corpus | Sprint 26+ |
| onchain-validate Docker testcontainers mode | Config-only mode now | Next sprint |
