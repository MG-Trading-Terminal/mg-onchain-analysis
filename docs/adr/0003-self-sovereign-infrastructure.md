# ADR 0003 — Self-Sovereign Infrastructure (No 3rd-Party Dependencies in Production)

**Status:** Accepted
**Date:** 2026-04-21
**Supersedes:** partially ADR 0001 §D2 (ingestion strategy — see §Relationship below)
**Inputs:** user-stated architectural principle (2026-04-21)

---

## Context

ADR 0001 §D2 chose Yellowstone gRPC as a **provider-agnostic protocol** and listed three viable endpoints with equal weight: Helius LaserStream, Triton Dragon's Mouth, self-hosted validator. In practice, early documentation + config examples treated managed providers as the expected default and self-hosted as an option "preserved for consumers with data-residency requirements."

During Sprint 2 design (2026-04-21) the user stated the principle directly: *"я хочу полностью свое решение без всех тих товарищей"* — I want a fully self-contained solution without those third parties.

Blockchain data is public and permissionless by design. Any on-chain event can be read by anyone running a node. Therefore **paying a 3rd-party provider is paying for infrastructure, not data**. And that infrastructure dependency is, for an analytics service feeding four production consumers, a concrete risk vector:

| Risk | Materialisation |
|------|-----------------|
| Rate-limit throttling | Detectors go silent precisely when rugs/pumps happen (peak event volume = peak risk = peak throttle) |
| Provider outage | Cascading failure: bot-trader, custody, market maker, exchange all lose on-chain visibility simultaneously |
| Provider terms change | Unilateral pricing/quota shifts, API deprecations |
| Query logs | Provider sees every token we monitor; market-making competitor could reconstruct our strategy |
| Subpoena exposure | Regulator orders provider logs → our activity pattern revealed |
| Acquisition / shutdown | Precedent: EigenPhi defunct (logged in `research/01-market-scan.md` §8); Hexagate → Chainalysis acquisition Dec 2024 |
| Censorship | Provider can refuse service to specific addresses / regions |

None of these risks attach to a self-hosted node. Self-hosted infrastructure is additional ops cost, but a bounded and predictable one.

## Decision

**Self-hosted infrastructure is the production default.** Zero 3rd-party dependencies in any hot-path data flow.

### Per-chain implementation

**Solana:**
- **Production**: self-hosted non-voting RPC validator with Yellowstone gRPC Geyser plugin. Runs on own hardware or dedicated cloud. The `ChainAdapter` trait already abstracts the endpoint via config — no code change needed, only config default + documentation shift.
- **Dev bootstrap (before the user's validator is online)**: Anza-operated public RPC (`api.mainnet-beta.solana.com`) is tolerated for read-only low-rate discovery calls. Yellowstone gRPC streaming is NOT available there; dev runs against captured fixtures instead (see below).
- **Test workflows**: always captured fixtures — zero external RPC in CI. Matches CLAUDE.md §Detector Rules determinism invariant.

**EVM (Phase 4):**
- Self-hosted Geth / Erigon / Reth per chain. Archive nodes where detectors need historical depth; pruned nodes otherwise.
- Same philosophy: no Alchemy / Infura / QuickNode / Moralis in production.
- Bootstrap path: captured fixture replay. Public endpoints (`cloudflare-eth.com`, etc.) tolerated only for initial discovery.

### What is NOT a 3rd-party dependency under this ADR

- **Price oracles** (Pyth, Chainlink) — already explicitly out of scope per ROADMAP §Out of scope. We do not embed price feeds.
- **Fixture data sources** during research / one-off probe work (RugCheck API, DEXScreener) — used to build labelled fixture corpora + verification, not for hot-path detection. The output (fixtures) lives in our repo; the provider is not a runtime dependency.
- **CEX wallet lists, vesting program IDs, DEX program IDs** — static data compiled into `crates/token-registry/data/*.json` from public sources. No runtime call.

### What IS deprecated

- Helius LaserStream / Enhanced API / RPC.
- Triton One Dragon's Mouth.
- Jupiter token API as a runtime dependency (snapshot the verified list into `crates/token-registry/data/` instead).
- Solscan API (already known-broken via bot protection per `research/token-probes/` reports).
- Birdeye API (API-key-gated).
- Nansen Smart Money API (flagged deferred in ADR 0001 §Consequences; reconfirmed dropped here).

Config examples + token-registry fallback paths that name these providers MUST be edited to reflect self-host as default. Provider endpoints may remain as commented-out alternative examples with a `# WARNING: third-party — dev bootstrap only, remove before production` header.

## Consequences

### Work to do in this sprint

1. `config/adapters.toml.example` — default endpoint becomes `grpc://localhost:10000` (standard Yellowstone plugin port). Provider blocks moved to commented-out "alternative, dev-bootstrap-only" section with warning.
2. `config/token-registry.toml.example` (if exists) — same default shift.
3. `crates/token-registry/src/rpc.rs` Helius fallback path logic — unchanged code, but documentation updated: Helius is a bootstrap-only helper, self-hosted is default.
4. `SESSION-KICKOFF.md` gotcha #11 sibling: self-sovereign stack is policy; sub-agents MUST NOT introduce 3rd-party SaaS deps in hot path.
5. `ROADMAP.md` — new infra track:
   - Phase 2 or early Phase 3: `infra/solana-validator/` — hardware spec, snapshot sync procedure, Yellowstone plugin build + config, systemd unit, monitoring checklist
   - Phase 4: `infra/ethereum-node/`, `infra/base-node/`, etc.
6. `REFERENCES.md` — no change (research sources are legitimate).
7. Sprint 2 exit integration test — **fixture replay is the primary path.** The "live run against your own node" option is a Phase 3 follow-up after the user stands up the validator.

### Hardware reality — Solana validator sizing

For a non-voting RPC node (no consensus participation):

| Resource | Minimum | Recommended |
|----------|---------|-------------|
| CPU | 12 cores | 16+ cores |
| RAM | 128 GB (restricted features) | 256 GB |
| Disk (accounts-db) | 1 TB NVMe | 2 TB NVMe |
| Disk (ledger) | 500 GB NVMe (with trim) | 2 TB NVMe |
| Network | 1 Gbps symmetric | 10 Gbps |
| Snapshot sync | 24-48h initial | depends on peers |

Self-hosted cost ranges $200-400/mo bare-metal colo → $600-1000/mo dedicated cloud. Ops: snapshot-restart discipline, client version tracking (Agave/Firedancer), alerting on slot-lag.

This is a real commitment. Until the user's node is operational, dev runs against fixtures; no live Solana work in-session.

### What this ADR does NOT commit us to

- Running a 24/7 production node during MVP development. Fixture-replay tests work without it.
- Implementing validator ops automation ourselves. Setup documentation + systemd unit is sufficient.
- Multi-region HA. Single node is acceptable for MVP; HA is a Phase 5 infra task.
- Archive node on day one. A pruned ledger (recent state only) is enough for Phase 2 detectors.

### Risks accepted

| Risk | Mitigation |
|------|------------|
| User's validator goes down → detectors dark | Document failover to captured fixtures + manual bootstrap RPC; alert on slot-lag |
| Snapshot divergence from mainnet | Validator reports slot + hash; detector queries include slot bound for audit |
| Higher initial effort to reach first live detection | Explicit; user accepted this as a principled trade-off |
| EVM Phase 4 doubles the infra footprint | Explicitly documented as Phase 4 scope; this ADR applies to Solana and extends identically |

## Relationship to ADR 0001 §D2

ADR 0001 §D2 chose the Yellowstone gRPC protocol and listed self-hosted as one of three equal endpoints. This ADR does not reject the protocol choice — Yellowstone gRPC remains the ingestion protocol. This ADR changes the **default endpoint**: self-hosted becomes the production path, not a "preserved for data-residency" option. Provider endpoints remain config-compatible (same gRPC wire format) but are demoted to bootstrap-only status.

Code in `crates/chain-adapter/` needs NO changes — provider-agnostic design already accommodates this shift. Only defaults, documentation, and examples move.

## References

- ADR 0001 §D2 (superseded in default-endpoint choice)
- `research/01-market-scan.md` §8 (EigenPhi defunct, Hexagate acquisition — precedent for 3rd-party failure modes)
- CLAUDE.md §Detector Rules (determinism — reinforces fixture-first testing)
- User principle stated 2026-04-21: "я хочу полностью свое решение без всех тих товарищей"
- Yellowstone gRPC Geyser plugin — `https://github.com/rpcpool/yellowstone-grpc`
- Solana validator hardware requirements — `https://docs.anza.xyz/operations/requirements`
