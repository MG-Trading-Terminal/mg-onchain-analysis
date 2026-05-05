# mg-onchain-analysis — production deployment runbook

This is the operator playbook for deploying mg-onchain-analysis end-to-end.
Designed for operators who want to bring up the service with zero blockers
from our side.

**Status:** Sprint 26 v1. Covers the three operator topologies (ETH-only /
Solana-only / multi-chain) on a single box per topology. Scale-out, mTLS, and
HA topology are Sprint 27+ follow-ups.

**ADR/design references:**
- ADR 0003 — self-sovereign infrastructure (no SaaS in production hot path)
- ADR 0004 — Reth as the EVM node
- ADR 0006 — code-level self-sovereignty (post-amendment, no bridges)
- ADR 0007 — pull-based query engine operational model
- Design 0028 — lightweight query-engine production deployment

## 1. Overview

mg-onchain-analysis is a **pull-based query engine** for on-chain anomaly
detection on shitcoin manipulation patterns. Consumers (trading bot, custody,
market maker, exchange) hit our service with a token of interest and get back
a calibrated verdict assembled from 13 detectors.

Per ADR 0007 the service does NOT firehose all chain activity. It runs RPC
queries on demand against self-hosted nodes and caches verdicts. This makes
deployment substantially cheaper than a continuous-streaming pipeline.

The stack:

| Component | Software | Why this and not something else |
|---|---|---|
| EVM node | **Reth** (built from source per ADR 0003) | Rust ecosystem, parallel sync, our type system aligns |
| EVM consensus client | **Lighthouse** | Required post-Merge to advance Reth |
| Solana node | **Agave** in **RPC-only mode** | ADR 0007 — no Yellowstone Geyser needed |
| Database | **Postgres 16** | One storage tier per ADR 0002 |
| Service | `onchain-service` (this repo, built from source) | 100% our code per ADR 0006 |
| Observability (optional) | **OpenTelemetry collector** | Public spec, operator's choice of backend |

## 2. Prerequisites

- Docker Engine 24+
- docker-compose v2+
- ≥1 Gbps network bandwidth (chain sync requires sustained bandwidth)
- Static public IPv4/IPv6 (P2P nodes need to be reachable from peers)
- Hardware as per topology (§3)
- Outbound TCP/UDP allowed for chain P2P ports (30303 ETH, 8001 Solana, 9000 Lighthouse)

## 3. Hardware BOM by topology

### Topology A — Ethereum-only operator

Single box. Reth + Lighthouse + Postgres + onchain-service co-resident.

| Resource | Minimum | Recommended |
|---|---|---|
| CPU | 8 cores | 16 cores |
| RAM | 32 GB | 64 GB |
| NVMe | 2 TB | 4 TB |
| Network | 100 Mbps sustained | 1 Gbps |

Cost: **~$80-150/mo bare-metal** (Hetzner AX41-class or equivalent).

Use case: bot/MM/exchange operating only on Ethereum + L2s.

### Topology B — Solana-only operator

Single box. Agave RPC + Postgres + onchain-service co-resident.

| Resource | Minimum | Recommended |
|---|---|---|
| CPU | 12 cores | 16 cores |
| RAM | 64 GB | 128 GB |
| NVMe | 2 TB | 4 TB |
| Network | 1 Gbps | 1 Gbps |

Cost: **~$150-250/mo bare-metal** (Hetzner AX102-class or equivalent).

**RAM caveat:** the 64-128 GB estimate for Agave in RPC-only mode is from
community experience, not first-hand benchmarks against the specific
`AGAVE_TAG` we pin (v3.1.13 default). Validate with your own pin before
provisioning. Solana under load can OOM if RAM is tight; preferring more is
always safer.

Use case: shitcoin trading on Solana memecoin markets.

### Topology C — Multi-chain operator (ETH + Solana)

Single box (recommended for v1). All services co-resident.

| Resource | Minimum | Recommended |
|---|---|---|
| CPU | 16 cores | 24 cores |
| RAM | 128 GB | 256 GB |
| NVMe | 4 TB | 6 TB |
| Network | 1 Gbps | 1 Gbps |

Cost: **~$300-500/mo bare-metal**.

Scale-out variant (separate boxes for ETH, Solana, app): documented in §14.

## 4. First deployment

### 4.1 Clone and prepare

```bash
git clone https://github.com/meatgrinder/mg-onchain-analysis.git
cd mg-onchain-analysis
git checkout v0.1.0  # pin to a tagged release for production
```

### 4.2 Generate JWT secret (required for Ethereum profile)

```bash
openssl rand -hex 32 > infra/jwt.hex
chmod 600 infra/jwt.hex
```

### 4.3 Configure secrets

```bash
cp infra/.env.example infra/.env
# Edit infra/.env — set POSTGRES_PASSWORD to a strong random value:
openssl rand -hex 32  # use this for POSTGRES_PASSWORD
```

### 4.4 Build images and bring up the stack

Pick your topology profile:

```bash
# Topology A (Ethereum only):
docker compose -f infra/docker-compose.prod.yml --profile ethereum build
docker compose -f infra/docker-compose.prod.yml --profile ethereum up -d

# Topology B (Solana only):
docker compose -f infra/docker-compose.prod.yml --profile solana build
docker compose -f infra/docker-compose.prod.yml --profile solana up -d

# Topology C (multi-chain):
docker compose -f infra/docker-compose.prod.yml --profile multi build
docker compose -f infra/docker-compose.prod.yml --profile multi up -d
```

### 4.5 Watch chain sync (the long part)

Chain sync dominates first-run wall-clock. Expected:

| Chain | Mode | Hardware | Sync time |
|---|---|---|---|
| Ethereum | snap-pruned | Topology A recommended | 4-12 hours |
| Solana | RPC-only, snapshot+ledger | Topology B recommended | 6-24 hours |

Monitor:

```bash
docker compose logs -f ethereum-node
docker compose logs -f lighthouse
docker compose logs -f solana-node
```

Look for "synced to head" / "caught up" messages.

### 4.6 Readiness check

Once chains are synced and `onchain-service` has applied migrations:

```bash
curl -s http://localhost:8080/health | jq
# Expected:
# {
#   "status": "ok",
#   "storage": "ok",
#   "scoring": "ok",
#   "detectors": "ok",
#   "registry": "ok",
#   "uptime_seconds": 123
# }
```

The first `GET /health` may return `503 Service Unavailable` while migrations
run. After ≤30 seconds it should flip to 200.

### 4.7 First query

```bash
# Ethereum example (USDT mainnet):
curl -s "http://localhost:8080/v1/score?chain=ethereum&token=0xdAC17F958D2ee523a2206206994597C13D831ec7" | jq

# Solana example (Wrapped SOL):
curl -s "http://localhost:8080/v1/score?chain=solana&token=So11111111111111111111111111111111111111112" | jq
```

## 5. Configuration

| Env var | Required | Default | Purpose |
|---|---|---|---|
| `POSTGRES_PASSWORD` | yes | — | Postgres password (set in `.env`) |
| `POSTGRES_USER` | no | `mg_onchain` | Postgres user |
| `POSTGRES_DB` | no | `mg_onchain` | Postgres database |
| `OTEL_ENDPOINT` | no | unset | OTLP collector endpoint (gRPC, port 4317). Unset = stdout-only tracing. |
| `RUST_LOG` | no | `info` | Service log filter |
| `RETH_TAG` | no | `v1.3.0` | Reth image tag pin |
| `AGAVE_TAG` | no | `v3.1.13` | Agave build tag pin (matches `infra/solana-validator/`) |
| `SERVICE_TAG` | no | `latest` | onchain-service image tag pin (used for rollback) |
| `RETH_DATA_DIR` | no | named volume | Override Reth data dir (point at NVMe mount) |
| `LIGHTHOUSE_DATA_DIR` | no | named volume | Override Lighthouse data dir |
| `AGAVE_LEDGER_DIR` | no | named volume | Override Agave ledger dir |
| `PG_DATA_DIR` | no | named volume | Override Postgres data dir |

Service-side configuration files (mounted from `config/` into the container):

- `config/service.toml` — chain enable flags, periodic-scan cadence, OTLP endpoint defaults
- `config/detectors.toml` — detector thresholds, verdict_cache TTL classes
- `config/adapters.toml` — chain-adapter parameters
- `config/known_bridges.toml` — D14 bridge-drain registry

To override a config file at runtime, mount it:

```yaml
# in docker-compose.prod.yml under onchain-service.volumes:
- ./service.toml:/app/config/service.toml:ro
```

## 6. Observability

### `/health` endpoint

```
GET http://localhost:8080/health
```

Deep check by default: queries Postgres `SELECT 1` + each chain-adapter
connection. Returns 200 if all healthy, 503 if any component is degraded.

`?shallow=true` query param returns 200 immediately if the process is alive
(for TCP load-balancer probes that hit too frequently for the deep check).

### `/metrics` Prometheus endpoint

```
GET http://localhost:8080/metrics
```

Returns Prometheus text format. Counters published:
- `detectors_evaluated_total{detector_id, chain, outcome}`
- `anomalies_emitted_total{detector_id, chain, severity}`
- `chain_adapter_events_processed_total{chain, event_type}`
- `db_query_duration_seconds_bucket{...}`

Sample Prometheus scrape config:

```yaml
scrape_configs:
  - job_name: mg-onchain-service
    static_configs:
      - targets: ['localhost:8080']
    scrape_interval: 30s
```

### OTLP traces (optional)

Uncomment the `otel-collector` service in `docker-compose.prod.yml`. Provide
`infra/otel-collector-config.yaml` pointing at your trace backend (Jaeger,
Grafana Tempo, Honeycomb, etc.). Example minimal config:

```yaml
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317

exporters:
  otlphttp/tempo:
    endpoint: https://tempo.example.com

service:
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [otlphttp/tempo]
```

Set `OTEL_ENDPOINT=http://otel-collector:4317` in `.env` to enable.

## 7. Backup

Postgres is the only stateful component the operator owns end-to-end. Reth
and Agave hold chain data which is publicly recoverable from peers (just
re-syncs cost wall-time, not data).

### Daily pg_dump cron (v1)

```bash
# /etc/cron.d/mg-onchain-backup
0 3 * * * root docker compose -f /path/to/infra/docker-compose.prod.yml exec -T postgres \
    pg_dump -U mg_onchain mg_onchain | gzip > /backup/pg-$(date +\%F).sql.gz
```

Keep 30 daily backups, then weekly. Off-site copy to S3-compatible storage
recommended.

### Restore

```bash
gunzip < /backup/pg-2026-04-28.sql.gz | \
  docker compose -f infra/docker-compose.prod.yml exec -T postgres \
    psql -U mg_onchain mg_onchain
```

### WAL streaming (Sprint 27 upgrade path)

For HA deploys, upgrade to WAL streaming with a standby Postgres replica.
Documented as Sprint 27+ work — not in this v1 runbook.

## 8. Log retention

Docker JSON driver with rotation is configured in compose:

- `max-size: 100m` per file
- `max-file: 5` per service

That is ~500 MB per service kept on disk. Sufficient for ~3-7 days of normal
operation.

For longer retention or central logging, add a Loki+Promtail or
Vector-on-host setup. Out of scope for v1 — Sprint 27 carry-forward.

## 9. Secrets management

v1: `.env` file with file-system permissions (`chmod 600`). Operators with
mature secret-management infrastructure (Vault, Doppler, AWS Secrets Manager)
should mount secrets via Docker secrets instead:

```yaml
# in docker-compose.prod.yml under postgres:
secrets:
  - postgres_password
environment:
  POSTGRES_PASSWORD_FILE: /run/secrets/postgres_password

secrets:
  postgres_password:
    file: ./secrets/postgres_password
```

This is the documented upgrade path; not the v1 default.

## 10. Rollback

### Service binary rollback

Pin `SERVICE_TAG` in `.env` to the previous tag, then:

```bash
docker compose -f infra/docker-compose.prod.yml --profile multi pull onchain-service
docker compose -f infra/docker-compose.prod.yml --profile multi up -d onchain-service
```

### Migration rollback

V00018 migration drops bulk event tables that were never populated under the
old continuous-streaming model. The migration includes row-count guards that
fail loudly if production data is present (design 0028 §11.4). If the guard
fired during your initial migration and you have populated data:

1. Coordinate with the team to write a V00019 migration that handles your
   data.
2. Revert by restoring from the pre-migration `pg_dump` if necessary.

`sqlx migrate revert` is NOT supported in production — forward-only
migrations only.

## 11. Common operator scenarios

### Full stack restart

```bash
docker compose -f infra/docker-compose.prod.yml --profile multi down
docker compose -f infra/docker-compose.prod.yml --profile multi up -d
```

Chain data persists in named volumes; sync resumes from the last checkpoint.

### Single-service restart

```bash
docker compose -f infra/docker-compose.prod.yml restart onchain-service
```

### Chain re-sync from scratch

```bash
docker compose -f infra/docker-compose.prod.yml --profile multi down -v
# CAUTION: -v deletes named volumes including chain data and Postgres
```

This forces a full re-sync — hours of wall-clock. Only do this if data
corruption is suspected.

### Tail logs

```bash
docker compose -f infra/docker-compose.prod.yml logs -f onchain-service
```

## 12. Troubleshooting

| Symptom | Likely cause | Remediation |
|---|---|---|
| `/health` returns 503 with `storage: error` | Postgres pool unreachable | `docker compose ps postgres`, check container is healthy |
| `/health` returns 503 with `registry: error` | Chain RPC unreachable | Check chain-node container logs, peer count |
| Chain node "no peers" warning | Inbound P2P port blocked | Open `30303` (ETH) / `8001` (Solana) on host firewall |
| Chain stuck on a slot/block | Disk full or RAM pressure | `docker stats`, free space on data volume |
| OTLP traces not appearing | Collector misconfigured | `docker compose logs otel-collector`, verify exporter config |
| Service OOM-killed | Postgres pool too large or detector eval too parallel | Reduce `[postgres] max_connections` and `[periodic_scan] max_concurrent` |

## 13. Hardening checklist (Sprint 27+ — out of scope for v1)

- TLS termination for `:8080` REST/WS (reverse-proxy with Caddy or nginx)
- mTLS between onchain-service and otel-collector
- Firewall rules (UFW or cloud security groups) — only expose `:8080` and
  P2P ports externally
- fail2ban or equivalent for `:8080` brute-force protection
- Postgres `pg_hba.conf` SCRAM-SHA-256 enforcement (already on by default
  in compose)
- Disk encryption at rest (LUKS or cloud-provider equivalent)

## 14. Scale-out topology (advanced)

For Topology C operators with HA requirements, split services across boxes:

| Box | Services | Hardware |
|---|---|---|
| Box 1 | ethereum-node + lighthouse | 64 GB / 4 TB / 16 cores |
| Box 2 | solana-node | 128 GB / 4 TB / 16 cores |
| Box 3 | postgres + onchain-service + otel-collector | 32 GB / 500 GB / 8 cores |

Modify `docker-compose.prod.yml` to expose chain-node ports beyond loopback,
deploy three compose files (one per box), and adjust service env vars
(`ETHEREUM_RPC_URL`, `SOLANA_HTTP_URL`, etc.) to point at Box 1 and Box 2's
public-but-firewalled IPs.

This topology is documented but not the v1 default. Sprint 27 carry-forward
includes a dedicated `infra/docker-compose.scale-out.yml` and updated
operator instructions.

## 15. References

- [`docs/adr/0003-self-sovereign-infrastructure.md`](../docs/adr/0003-self-sovereign-infrastructure.md)
- [`docs/adr/0004-evm-node-choice-geth-vs-reth.md`](../docs/adr/0004-evm-node-choice-geth-vs-reth.md)
- [`docs/adr/0006-code-level-self-sovereignty.md`](../docs/adr/0006-code-level-self-sovereignty.md)
- [`docs/adr/0007-pull-based-query-engine.md`](../docs/adr/0007-pull-based-query-engine.md)
- [`docs/designs/0028-lightweight-query-engine-deployment.md`](../docs/designs/0028-lightweight-query-engine-deployment.md)
- [`infra/ethereum-node/`](./ethereum-node/) — Reth-from-source build runbook
- [`infra/solana-validator/`](./solana-validator/) — Agave-from-source build runbook
