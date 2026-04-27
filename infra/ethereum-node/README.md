# Ethereum Node Runbook (Reth)

**Scope:** Stand up a Reth execution-layer node on Ethereum mainnet, paired with a
Lighthouse consensus-layer client, exposing:

- `http://127.0.0.1:8545` — HTTP JSON-RPC (backfill, one-off queries)
- `ws://127.0.0.1:8546` — WebSocket JSON-RPC (`eth_subscribe` live stream)
- `http://127.0.0.1:9001` — Reth Prometheus metrics

Both ports are bound to `127.0.0.1` by default. Do **not** expose them on a public
interface without a reverse proxy with authentication.

**No Alchemy / Infura / QuickNode in production** — ADR 0003 is binding. This runbook
covers a fully self-hosted setup.

**Time to first event:** 4–8 hours (Reth snap sync on NVMe + gigabit uplink). The machine
is usable for other tasks while sync proceeds.

---

## Table of Contents

1. [Overview](#1-overview)
2. [Hardware Requirements](#2-hardware-requirements)
3. [First-Run Quickstart](#3-first-run-quickstart)
4. [Configuration Reference](#4-configuration-reference)
5. [Healthcheck + Observability](#5-healthcheck--observability)
6. [Upgrade Procedure](#6-upgrade-procedure)
7. [Troubleshooting](#7-troubleshooting)
8. [Reset Procedure](#8-reset-procedure)

---

## 1. Overview

### What this delivers

A **pruned Reth execution-layer node** plus a **Lighthouse consensus-layer node** on
Ethereum mainnet that:

- Syncs via snap sync (state snapshot download + incremental block replay — no genesis
  replay required).
- Retains the **full transaction and log history** (`eth_getLogs` backfill works across
  all blocks). Ancient state (pre-snap-point trie nodes) is pruned to save disk.
- Exposes standard Ethereum JSON-RPC and WebSocket endpoints for the `chain-adapter` crate.
- Emits Prometheus metrics for Grafana alerting.

### Why Reth (ADR 0004 summary)

Reth is a Rust implementation of the Ethereum execution client by Paradigm. It provides:

1. **Execution Extensions (ExEx)** — a Rust-native, in-process, push-based streaming API
   with explicit reorg notifications (`ChainCommitted`, `ChainReverted`, `ChainUpdated`).
   This is the structural equivalent of the Yellowstone gRPC plugin on Solana. ExEx
   integration is tracked for Sprint 16; this sprint uses the `eth_subscribe` WebSocket
   path as the bootstrap adapter.

2. **Parallel execution engine** — approximately 2x faster snap sync than Geth on
   equivalent hardware (Paradigm benchmark, September 2024).

3. **Rust ecosystem alignment** — `alloy-primitives`, `reth-primitives`, and
   `alloy-rpc-types` are all Rust crates, usable directly in `crates/chain-adapter`
   without a language boundary.

4. **Clean reorg semantics** — ExEx `ChainReverted` maps directly to `Event::ReorgMarker`
   in the existing adapter contract.

### Architecture

```
Lighthouse (CL)  ←→  Reth (EL)      JWT auth on Engine API port 8551
                       │
                       ├── HTTP RPC  :8545  ← chain-adapter backfill
                       └── WS RPC   :8546  ← chain-adapter subscribe stream
```

Post-Merge Ethereum requires a consensus-layer client (Lighthouse here) to drive the
execution client. Without the CL, the EL will not advance past the Merge block.

---

## 2. Hardware Requirements

Requirements are taken from ADR 0003 §Hardware and ADR 0004 §Hardware sizing.

| Resource        | Minimum                        | Recommended                    |
|-----------------|--------------------------------|--------------------------------|
| CPU             | 4 cores                        | 8+ cores (Reth parallel exec)  |
| RAM             | 16 GB                          | 32 GB (state cache)            |
| Disk (Reth DB)  | 1.5 TB NVMe (PCIe Gen3+)       | 2 TB NVMe PCIe Gen4            |
| Disk (Lighthouse)| 100 GB NVMe                  | 200 GB NVMe                    |
| Network         | 100 Mbps symmetric             | 1 Gbps (snap sync bandwidth)   |

**Disk growth:** ~75 GB/month on a pruned node as of Q1-2026. Budget 2 TB NVMe for
18+ months of operation without resizing.

**Cost estimate:** $100–250/mo bare-metal or dedicated cloud (e.g. Hetzner AX41-NVMe at
~€60/mo, OVHcloud Advance-1 at ~$100/mo). An order of magnitude cheaper than the Solana
validator.

**RAM note:** 16 GB is workable but the state cache will be small, slowing block
processing. 32 GB gives Reth room to cache recent state in memory.

---

## 3. First-Run Quickstart

### Prerequisites

- Docker Engine 24+ and Docker Compose v2 installed on the host.
- NVMe volumes mounted at the paths listed in `.env` (see §4).
- A fresh JWT secret generated (see §4 JWT Secret).

### Step 1: Clone or copy infra files

```bash
# From the mg-onchain-analysis repo root:
cp infra/ethereum-node/.env.example infra/ethereum-node/.env
# Edit .env — set DATA_DIR and LIGHTHOUSE_DATA_DIR to your NVMe mount points.
```

### Step 2: Generate JWT secret

The Engine API (port 8551) between Reth and Lighthouse requires a shared JWT secret.

```bash
# Generate a random 32-byte (64 hex char) secret:
openssl rand -hex 32 > infra/ethereum-node/jwt.hex
# Verify it is 64 chars:
wc -c infra/ethereum-node/jwt.hex
# Expected: 65 (64 chars + newline)
```

The file `jwt-secret.example` contains a placeholder. Never commit the real `jwt.hex`.
Add `infra/ethereum-node/jwt.hex` to `.gitignore`.

### Step 3: Start the stack

```bash
cd infra/ethereum-node
docker compose up -d
```

This starts two services:
- `reth` — Reth execution client (snap sync begins immediately)
- `lighthouse` — Lighthouse consensus client (waits for Reth EL to be ready, then begins CL sync)

### Step 4: Monitor snap sync

Snap sync progresses in stages: downloading state snapshot → downloading block headers →
importing recent blocks. Watch progress:

```bash
# Follow Reth logs (look for "Syncing" progress lines):
docker compose logs -f reth

# Check block number via RPC (returns 0x0 until snap completes):
curl -s -X POST http://127.0.0.1:8545 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'
```

Reth snap sync takes **4–8 hours** on a fast NVMe + 1 Gbps uplink. On slower hardware or
network, up to 12–16 hours. The node begins accepting RPC requests immediately but
`eth_getLogs` queries over historical ranges will fail until sync reaches the relevant
blocks.

### Step 5: Verify sync complete

```bash
# Should return the current mainnet block number (hex):
curl -s -X POST http://127.0.0.1:8545 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_syncing","params":[],"id":1}'
# Returns false when fully synced.

# Chain tip:
curl -s -X POST http://127.0.0.1:8545 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'
```

When `eth_syncing` returns `false` and `eth_blockNumber` matches the current mainnet head
(verify at https://etherscan.io/), the node is ready for chain-adapter use.

---

## 4. Configuration Reference

### Pinned image

The `docker-compose.yml` uses a pinned Reth image:

```
ghcr.io/paradigmxyz/reth:v1.x.x@sha256:REPLACE_WITH_CURRENT_DIGEST
```

**How to verify and update the digest:**

```bash
# Pull the latest stable release tag (check https://github.com/paradigmxyz/reth/releases):
docker pull ghcr.io/paradigmxyz/reth:v1.3.0

# Get the digest:
docker inspect --format='{{index .RepoDigests 0}}' ghcr.io/paradigmxyz/reth:v1.3.0

# Update docker-compose.yml image line with the full digest string.
```

Always pin to a digest, not just a tag. Tags are mutable; digests are immutable.

For Lighthouse: use `sigp/lighthouse:v6.x.x@sha256:REPLACE_WITH_CURRENT_DIGEST`. Check
https://github.com/sigp/lighthouse/releases for the latest stable tag.

### Ports

| Port   | Bind address  | Protocol | Purpose                                    |
|--------|---------------|----------|--------------------------------------------|
| 8545   | 127.0.0.1     | HTTP     | JSON-RPC (backfill queries)                |
| 8546   | 127.0.0.1     | WS       | WebSocket JSON-RPC (live subscribe stream) |
| 8551   | 127.0.0.1     | HTTP     | Engine API (Reth ↔ Lighthouse JWT auth)    |
| 30303  | 0.0.0.0       | TCP/UDP  | Reth P2P (peer discovery — must be public) |
| 9001   | 127.0.0.1     | HTTP     | Reth Prometheus metrics                    |
| 5052   | 127.0.0.1     | HTTP     | Lighthouse beacon API                      |
| 9000   | 0.0.0.0       | TCP/UDP  | Lighthouse P2P                             |

Port 30303 and 9000 (P2P) must be reachable from the internet for the node to find peers.
All other ports are loopback-only. Use a firewall (ufw / iptables) to enforce this.

### Environment variables (`.env`)

See `.env.example` for the full list. Key variables:

| Variable             | Default                    | Description                               |
|----------------------|----------------------------|-------------------------------------------|
| `RETH_DATA_DIR`      | `/data/reth`               | Reth database directory (NVMe mount)      |
| `LIGHTHOUSE_DATA_DIR`| `/data/lighthouse`         | Lighthouse DB + keys (NVMe mount)         |
| `JWT_SECRET_PATH`    | `./jwt.hex`                | Path to 64-char hex JWT secret file       |
| `RETH_HTTP_PORT`     | `8545`                     | HTTP RPC port                             |
| `RETH_WS_PORT`       | `8546`                     | WebSocket RPC port                        |
| `RETH_METRICS_PORT`  | `9001`                     | Prometheus metrics port                   |
| `RETH_LOG_LEVEL`     | `info`                     | Log verbosity (trace/debug/info/warn)     |

### Finality + commitment policy (ADR 0004 §Finality)

The `chain-adapter` uses two block tags:

| Tier       | Tag / depth  | Latency    | Use                                   |
|------------|-------------|------------|---------------------------------------|
| Safe       | depth 12    | ~2.4 min   | Hot path: streaming event emission    |
| Finalized  | `finalized` | ~12.8 min  | Checkpoint saves, durable DB writes   |

The `finalized` block tag is natively supported by Reth and returns the last finalized
checkpoint block (updated every 64 slots by the CL).

---

## 5. Healthcheck + Observability

### HTTP readiness

```bash
# Check the node is alive and at the expected block:
curl -s -X POST http://127.0.0.1:8545 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' | jq .

# Check sync status (false = synced):
curl -s -X POST http://127.0.0.1:8545 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_syncing","params":[],"id":1}' | jq .
```

The `chain-adapter` `health_check()` method calls `eth_blockNumber` as its liveness probe.

### Docker healthcheck

The `docker-compose.yml` includes a healthcheck for the `reth` service that calls
`eth_blockNumber`. Run `docker compose ps` to see the health status.

### Prometheus metrics

Reth exposes metrics at `http://127.0.0.1:9001/metrics`. Key metrics to alert on:

| Metric                            | Alert condition                  |
|-----------------------------------|----------------------------------|
| `reth_sync_checkpoint_block_number` | Not increasing for > 5 min (sync stuck) |
| `reth_peers_total`                | < 5 peers (peer starvation)      |
| `reth_db_size_bytes`              | > 1.8 TB (disk pressure warning) |

Scrape interval: 30 s. Add to your Grafana/Prometheus config:

```yaml
- job_name: reth
  static_configs:
    - targets: ['localhost:9001']
```

### Sync status command

```bash
# One-liner sync summary:
docker compose exec reth reth node status 2>/dev/null || \
  curl -s -X POST http://127.0.0.1:8545 \
    -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"eth_syncing","params":[],"id":1}'
```

---

## 6. Upgrade Procedure

**Never upgrade a running node without backing up the database first.**

### Step 1: Check release notes

Review https://github.com/paradigmxyz/reth/releases for the new version. Check for:
- Database migration requirements (Reth may run automatic DB migrations on startup).
- Breaking changes in CLI flags or config format.
- ExEx API changes (relevant once Sprint 16 ExEx is wired).

### Step 2: Stop the stack

```bash
cd infra/ethereum-node
docker compose down
```

Wait for clean shutdown. Reth flushes its write-ahead log on SIGTERM.

### Step 3: Back up the database

```bash
# The DB is at $RETH_DATA_DIR (from .env). Snapshot with rsync or btrfs/ZFS snapshot:
rsync -av --progress /data/reth/ /backup/reth-$(date +%Y%m%d)/
# For large DBs (> 1 TB), prefer ZFS/btrfs snapshots (near-instant, copy-on-write).
```

Backup is required. Reth DB migrations are not always reversible. A failed upgrade
without a backup requires a full resync.

### Step 4: Update the image digest

```bash
# Pull the new image:
docker pull ghcr.io/paradigmxyz/reth:v<NEW_VERSION>

# Get the digest:
docker inspect --format='{{index .RepoDigests 0}}' ghcr.io/paradigmxyz/reth:v<NEW_VERSION>

# Update the image line in docker-compose.yml.
# Do the same for Lighthouse if upgrading it too.
```

### Step 5: Restart

```bash
docker compose up -d
# Monitor for DB migration completion:
docker compose logs -f reth
```

Reth logs `Running migrations` and `Migrations complete` if a DB migration runs.

### Step 6: Verify

Run the healthcheck from §5. Confirm `eth_blockNumber` is still near the chain tip and
incrementing.

---

## 7. Troubleshooting

### Disk full

**Symptom:** Reth exits with `No space left on device` or stops producing blocks.

**Cause:** The NVMe volume is exhausted. Pruned Reth grows ~75 GB/month.

**Fix:**
1. Check disk usage: `df -h /data/reth`
2. Short-term: remove old Reth logs: `docker compose exec reth find /root/.local/share/reth/logs -mtime +7 -delete`
3. Long-term: expand the NVMe volume or migrate to a larger disk. Reth's DB is not easily
   prunable after the fact — full resync on a larger disk is the reliable path.

### Peer count too low (< 5 peers)

**Symptom:** `reth_peers_total` metric is 0 or very low. Node is not syncing.

**Causes:**
- Port 30303 is firewalled. Check: `nmap -p 30303 <your-public-ip>` from an external host.
- Static peer list is stale. Reth uses its own peer discovery (discv4/discv5) — ensure UDP
  30303 is also open.
- ISP blocking P2P ports (rare but possible on residential connections).

**Fix:**
```bash
# Open port 30303 TCP+UDP (ufw example):
ufw allow 30303/tcp
ufw allow 30303/udp
# Check peer count:
curl -s -X POST http://127.0.0.1:8545 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"net_peerCount","params":[],"id":1}'
```

### Sync stuck

**Symptom:** `reth_sync_checkpoint_block_number` not advancing for > 5 minutes.

**Causes:**
- No peers (see above).
- Disk I/O bottleneck. Check with `iostat -x 1`.
- Memory pressure causing thrashing. Check with `free -h`.
- Lighthouse not syncing (EL waits for CL to provide new heads post-Merge).

**Fix:**
```bash
# Check Lighthouse sync:
curl -s http://127.0.0.1:5052/eth/v1/node/syncing | jq .
# If Lighthouse is syncing, Reth will follow once CL reaches the tip.

# Restart just Reth (Lighthouse can keep running):
docker compose restart reth
```

### RPC 429 / Too Many Requests despite self-hosted

**Symptom:** `eth_getLogs` or other calls return HTTP 429.

**Cause:** Reth does not rate-limit by default. A 429 means an upstream reverse proxy or
firewall is throttling requests — NOT the Reth node itself.

**Fix:** Check if nginx/caddy/haproxy is in front of port 8545 and remove or reconfigure
its rate-limit rules for internal chain-adapter traffic.

### Engine API authentication failure

**Symptom:** Lighthouse logs `JWT decoding error` or `Unauthorized` when connecting to Reth.

**Cause:** The JWT secret file in `./jwt.hex` does not match between Reth and Lighthouse,
or the file is malformed (not exactly 64 hex chars).

**Fix:**
```bash
# Verify file length (should be 65: 64 chars + newline):
wc -c infra/ethereum-node/jwt.hex

# Regenerate if wrong:
openssl rand -hex 32 > infra/ethereum-node/jwt.hex

# Restart both services (both must read the new secret):
docker compose down && docker compose up -d
```

### WebSocket disconnects / subscribe stream drops

**Symptom:** `chain-adapter` logs WebSocket reconnect attempts in a loop.

**Cause:** Reth's WebSocket server closed the connection (idle timeout, restart, or
resource pressure).

**Fix:** The `chain-adapter` `EthereumAdapter` includes auto-reconnect with exponential
backoff (Sprint 16 full implementation). Check Reth logs for the disconnect reason:

```bash
docker compose logs reth | grep -i "websocket\|ws\|disconnect" | tail -20
```

---

## 8. Reset Procedure (Destructive)

**This wipes the database and requires a full resync (4–8 hours).**

Use this if the database is corrupted, a migration failed, or you want to start fresh.

```bash
cd infra/ethereum-node

# 1. Stop everything:
docker compose down

# 2. Remove Reth database (DESTRUCTIVE):
rm -rf /data/reth/db /data/reth/static_files

# 3. Optionally remove Lighthouse data too (forces CL resync):
rm -rf /data/lighthouse/beacon

# 4. Keep the JWT secret (do NOT delete jwt.hex).

# 5. Restart:
docker compose up -d

# 6. Monitor snap sync (§3 Step 4).
```

The Lighthouse beacon chain data (~100 GB) can also be checkpoint-synced from a public
checkpoint sync provider to save time — see https://eth-clients.github.io/checkpoint-sync-endpoints/.

---

---

## Multi-Chain EVM Support

The `chain-adapter` crate supports multiple EVM chains simultaneously. Each chain requires
a separate self-hosted node connected via its own `ws_url` endpoint (configured in
`config/service.toml` under `[chains.<chain>]`).

### Recommended node software per chain

| Chain    | Recommended software | Default ws_url (service.toml) | Notes |
|----------|---------------------|-------------------------------|-------|
| Ethereum | Reth (`paradigmxyz/reth`) | `ws://127.0.0.1:8546` | See above runbook |
| BSC      | bnbchain/bsc (Go) or `node-real/op-bnb` | `ws://127.0.0.1:8547` | BNB Smart Chain full node |
| Base     | base-reth or op-reth (`paradigmxyz/reth` with OP Stack) | `ws://127.0.0.1:8548` | OP Stack L2 |
| Arbitrum | Arbitrum Nitro (`OffchainLabs/nitro`) | `ws://127.0.0.1:8549` | Nitro full node |
| Polygon  | Bor (`maticnetwork/bor`) | `ws://127.0.0.1:8550` | Polygon PoS execution client |

### Enabling additional chains

1. Stand up the node for the target chain (see its official docs).
2. In `config/service.toml`, set `[chains.<chain>] enabled = true` and update `ws_url`.
3. Restart `onchain-service`. The coordinator auto-spawns an `EthereumAdapter` per enabled
   chain via `build_evm_adapters()` in `crates/server/src/init/adapters.rs`.

### Reorg depth guidance

| Chain    | Default `reorg_depth` | Rationale | Source |
|----------|-----------------------|-----------|--------|
| Ethereum | 12 | LMD-GHOST PoS; ~2.4 min finality (ADR 0004) | ethereum.org |
| Base     | 12 | OP Stack L2; inherits Ethereum block cadence | base.org docs |
| Arbitrum | 12 | Nitro rollup; L2 batch safety at depth 12 | arbitrum.io docs |
| BSC      | 15 | Parlia PoA, 3 s blocks; slightly elevated short-fork risk | bnbchain.org/docs/bnbSmartChain/concepts/consensus/ |
| Polygon  | 64 | Bor PoS; Heimdall checkpoint every ~256 blocks (~8.5 min) | docs.polygon.technology/pos/architecture/heimdall/ |

These values are encoded in `EvmChainConfig::default_reorg_depth_for_chain(chain)` in
`crates/server/src/config.rs`. Operators may override per chain in `config/service.toml`.

Polygon PoS operators MUST NOT reduce below 64. The Heimdall checkpoint interval governs
finality; blocks below the checkpoint boundary can still be reorganised.

### Verified DEX contract addresses (D13 settlement allowlist)

Addresses in `crates/detectors/src/d13_sandwich_mev.rs::SETTLEMENT_ALLOWLIST` suppress
false-positive MEV alerts from legitimate batch routers. Verification status as of 2026-04-24:

| Chain    | Protocol                   | Address | Status |
|----------|---------------------------|---------|--------|
| Ethereum | CoW Protocol Settlement    | `0x9008D19f58AAbD9eD0D60971565AA8510560ab41` | VERIFIED (CoW Protocol GitHub + Etherscan) |
| Ethereum | Uniswap UniversalRouter V2 | `0x66a9893cc07d91d95644aedd05d03f95e1dba8af` | VERIFIED (Uniswap/universal-router mainnet.json) |
| Ethereum | Flashbots Builder           | `0xC92E8bdf79f0507f65a392b0ab4667716BFE0110` | SPEC-NOTE: no canonical source; Etherscan builder label |
| Ethereum | 1inch Fusion Settlement     | `0xa88800cd213da5ae406ce248380802bd53b47647` | SPEC-NOTE: 1inch docs returned 404; training-time knowledge |
| BSC      | PancakeSwap V2 Router       | `0x10ED43C718714eb63d5aA57B78B54704E256024E` | VERIFIED (PancakeSwap docs) |
| BSC      | PancakeSwap V3 SmartRouter  | `0x13f4EA83D0bd40E75C8222255bc855a974568Dd4` | SPEC-NOTE: PancakeSwap V3 GitHub 404; training-time |
| BSC      | Uniswap UniversalRouter V4  | `0x1906c1d672b88cd1b9ac7593301ca990f94eae07` | VERIFIED (Uniswap/universal-router BSC.json) |
| Base     | Aerodrome Router            | `0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43` | VERIFIED (aerodrome-finance/contracts README) |
| Base     | Uniswap UniversalRouter V1  | `0x2626664c2603336e57b271c5c0b26f421741e481` | VERIFIED (Uniswap/universal-router base.json) |
| Base     | Uniswap UniversalRouter V4  | `0x6ff5693b99212da76ad316178a184ab56d299b43` | VERIFIED (Uniswap/universal-router base.json) |
| Arbitrum | Camelot V2 Router           | `0xc873fecbd354f5a56e00e710b90ef4201db2448d` | VERIFIED (docs.camelot.exchange) |
| Arbitrum | Uniswap UniversalRouter V1  | `0x4c60051384bd2d3c01bfc845cf5f4b44bcbe9de5` | VERIFIED (Uniswap/universal-router arbitrum.json) |
| Arbitrum | Uniswap UniversalRouter V4  | `0xa51afafe0263b40edaef0df8781ea9aa03e381a3` | VERIFIED (Uniswap/universal-router arbitrum.json) |
| Polygon  | QuickSwap Router            | `0xa5E0829CaCEd8fFD4De3c43696c57F7D7A678ff` | VERIFIED (QuickSwap docs) |
| Polygon  | Uniswap UniversalRouter V1.2| `0x643770e279d5d0733f21d6dc03a8efbabf3255b4` | VERIFIED (Uniswap/universal-router polygon.json) |
| Polygon  | Uniswap UniversalRouter V4  | `0x1095692a6237d83c6a72f3f5efedb9a670c49223` | VERIFIED (Uniswap/universal-router polygon.json) |

### Permit2 universality

The Permit2 contract (`0x000000000022D473030F116dDEE9F6B43aC78BA3`) is deployed at the
same address on ALL EVM chains via deterministic CREATE2.

VERIFIED 2026-04-24: Uniswap/permit2 repository (`src/test/utils/DeployPermit2.sol`)
defines `address constant PERMIT2_ADDRESS = 0x000000000022D473030F116dDEE9F6B43aC78BA3`
as the canonical address. Block explorer source-read via V2 API requires an API key
(Etherscan/Bscscan/Basescan returned 403 without key). The address is universally
accepted in Uniswap SDK, OpenZeppelin, and Solady Permit2 integrations across all chains.

No chain-specific configuration is needed for Permit2.

### EVM compatibility notes (DEX event ABI)

- **PancakeSwap V2 (BSC)**: UniV2 fork — identical event signatures. Existing `univ2`
  decoders in `decoder.rs` work unchanged.

- **PancakeSwap V3 (BSC) — SPEC-NOTE**: Swap event differs from UniV3. PancakeSwap V3
  adds `protocolFeesToken0` and `protocolFeesToken1` extra parameters, producing a
  different `topic0` hash. The existing `univ3` decoder does NOT match PancakeSwap V3
  pools. Dedicated decoders deferred to next sprint. See `decoder.rs` univ3 module.

- **Aerodrome (Base) — SPEC-NOTE**: Solidly fork. Swap event has different parameter
  ordering from UniV2 (`to` is the 2nd indexed parameter, not the last). Produces a
  different `topic0`. The Aerodrome Router (allowlist entry) is VERIFIED; pool-level
  decoding requires a dedicated Aerodrome decoder (next sprint). See `decoder.rs` univ2.

- **Camelot (Arbitrum)**: V2 fork — compatible with `univ2` decoders.

- **QuickSwap (Polygon)**: V2 fork — compatible with `univ2` decoders.

---

## References

| # | Source | Claim |
|---|--------|-------|
| 1 | https://reth.rs/exex/exex.html | ExEx API, notification types |
| 2 | https://www.paradigm.xyz/2024/09/reth-v1 | Reth v1.0 release; sync benchmarks |
| 3 | https://github.com/paradigmxyz/reth/releases | Current stable release tags |
| 4 | https://github.com/sigp/lighthouse/releases | Lighthouse stable release tags |
| 5 | https://ethereum.github.io/execution-apis/api-documentation/ | Ethereum JSON-RPC spec |
| 6 | docs/adr/0003-self-sovereign-infrastructure.md | No Alchemy/Infura in prod |
| 7 | docs/adr/0004-evm-node-choice-geth-vs-reth.md | Reth decision + hardware sizing |
| 8 | infra/solana-validator/README.md | Structural template for this runbook |
| 9 | https://docs.pancakeswap.finance | PancakeSwap V2/V3 BSC deployment addresses |
| 10 | https://docs.quickswap.exchange | QuickSwap Polygon router address |
| 11 | https://github.com/Uniswap/universal-router/tree/main/deploy-addresses | UniversalRouter V1/V2 addresses per chain (Base, Arbitrum, Polygon) |
| 12 | https://github.com/aerodrome-finance/contracts | Aerodrome Router = 0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43 |
| 13 | https://github.com/Uniswap/permit2/blob/main/src/test/utils/DeployPermit2.sol | Permit2 canonical address 0x000000000022D473030F116dDEE9F6B43aC78BA3 |
| 14 | https://github.com/pancakeswap/pancake-v3-contracts/projects/v3-core/contracts/interfaces/pool/IPancakeV3PoolEvents.sol | PancakeSwap V3 Swap event adds 2 extra params vs UniV3 |
| 15 | https://github.com/aerodrome-finance/contracts/contracts/interfaces/IPool.sol | Aerodrome Swap event differs from UniV2 parameter ordering |
| 16 | bnbchain.org/docs/bnbSmartChain/concepts/consensus/ | BSC Parlia PoA finality model (reorg_depth=15) |
| 17 | docs.polygon.technology/pos/architecture/heimdall/ | Polygon Heimdall checkpoint interval (reorg_depth=64) |
