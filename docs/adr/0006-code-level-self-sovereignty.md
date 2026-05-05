# ADR 0006 — Code-Level Self-Sovereignty (No Vendor-Curated Cargo Dependencies in Service Crates)

**Status:** Accepted (user sign-off 2026-04-27); **AMENDED 2026-04-27** — bridge escape hatch closed (see Amendment block below).
**Date:** 2026-04-27 (original) / 2026-04-27 (amendment, same session).
**Supersedes:** ADR 0004 §6 ("Language and ecosystem alignment" — the rationale for `alloy-*` in service crates) and ADR 0004 §8 ("Embedded library mode" — the rationale for `reth-node-builder` in-process use). Both sections are rendered void by this ADR; ADR 0004's node-choice recommendation (Reth as the EVM node) remains unaffected.
**Inputs:** ADR 0001 §D2 (Yellowstone gRPC as the out-of-process bridge pattern for Solana), ADR 0003 (self-sovereign infrastructure — the parent doctrine), ADR 0004 §6 and §8 (the sections being superseded), `memory/feedback_no_vendor_cargo_deps.md` (user directive 2026-04-27), `Cargo.toml` lines 41–138 (current vendor footprint), `crates/chain-adapter/Cargo.toml` (`exex` Cargo feature), `crates/server/Cargo.toml` (`onchain-reth` binary).

---

## AMENDMENT (2026-04-27, same session as original sign-off)

**The "bridge escape hatch" in this ADR is RESCINDED.** Specifically:

- §Decision and §Bridge Process Pattern: the rule "vendor crates may live in isolated `bridge/<name>/` workspaces" is **closed**.
- §Allowed and Banned Dependencies: the "Allowed (bridge only)" column is **void**. Banned dependencies are banned everywhere in this repository, not just in the main workspace.
- The originally-proposed `bridge/exex-bridge/` (and its design doc `docs/designs/0025-exex-bridge-out-of-process.md`) are SUPERSEDED. No `bridge/` directory will be created. ADR 0001 §D2 still references the Yellowstone bridge pattern for Solana, but that bridge is the **Yellowstone Geyser plugin running inside the validator binary** — not a workspace under our control linking vendor crates. Our consumption of Yellowstone is via the public `.proto` schema with our own client code generated through `tonic-build` (Sprint 25 deliverable); we do not link the vendor's `yellowstone-grpc-client` Rust crate.

**Rationale for the amendment.** The user articulated the principle in plain terms: an external dependency that connects via a clean public wire protocol (Linux kernel via syscalls, Postgres via pgwire, Reth via JSON-RPC, Yellowstone via gRPC over published proto) is acceptable infrastructure. A custom bridge process whose only purpose is to legitimise vendor-crate linkage is a kludge — and the existence of a kludge points to a foundational architecture problem that should be resolved by changing the architecture, not by sanctioning the kludge. Wherever the standard wire protocol is sufficient for the integration, no bridge is needed. Where the standard wire protocol is genuinely insufficient, the integration requires a new ADR justifying the exception on its specific merits — the blanket bridge allowance is no longer that justification.

**What the amendment does NOT change.**

- The §Decision rule (a): vendor-curated SDK crates remain banned in service crates of the main workspace. This rule is absolute and unchanged.
- The §Reference-Reading Policy: reading vendor source is still allowed (license-permitting).
- The §Macros / Ergonomics / Crate Layout policies: unchanged.
- ADR 0003 self-sovereign infrastructure rules: unchanged. Self-hosted Reth/Geth/Erigon/Agave still required for production; SaaS APIs (Helius/Alchemy/Infura/etc) remain banned.
- ADR 0004 node-choice: Reth remains the recommended EVM node, run as a sibling process consumed via standard JSON-RPC.

**The text of §Decision, §Allowed and Banned Dependencies, and §Bridge Process Pattern below is preserved unchanged for historical record.** Read this Amendment block first; the original text describes what was sanctioned at the time of original sign-off, not current policy.

---

---

## Context

ADR 0003 established that self-hosted infrastructure is the production default for all
on-chain data flows. It banned Helius, Triton, Alchemy, Infura, QuickNode, and equivalent
managed providers from the production hot path. The doctrine was framed as a runtime risk
argument: rate limits silence detectors at peak-rug volume, provider outages cascade to
all four consumers simultaneously, and provider logs expose our monitoring strategy.

That reasoning applies equally to the compile-time dimension. A Cargo dependency is not
fundamentally different from a SaaS call: we are importing a crate authored by a third
party, maintained on a schedule we do not control, compiled into the same binary as our
financial decision logic, and subject to every supply-chain attack vector that the Rust
ecosystem shares with npm (typosquatting, maintainer-account compromise, malicious patch
release, dependency confusion). The difference is that a SaaS provider's bug might return
a wrong value at runtime; a compromised Cargo dependency can execute arbitrary code at
compile time or at runtime with our process's full privileges.

Until this ADR, the project accumulated three categories of vendor-curated dependencies
in service crates:

1. **alloy 1.6** (`alloy-rpc-client-ws`, `alloy-pubsub`, `alloy-sol-types`, `alloy-json-rpc`)
   — brought in Sprint 16 and pinned at Sprint 24. The rationale recorded in ADR 0004 §6
   was "language and ecosystem alignment": alloy is Rust, so there is no JSON boundary,
   and alloy types (`Address`, `U256`, `B256`) compose cleanly with our workspace.
   Workspace Cargo.toml lines 99–126.

2. **reth-exex, reth-primitives, reth-node-builder, reth-tracing** (all git-pinned at
   `v1.11.3`) — gated behind the `exex` Cargo feature in `crates/chain-adapter` and the
   matching `exex` feature in `crates/server`. The rationale in ADR 0004 §8 was the
   possibility of running Reth in-process (`reth-node-builder`) as a future optimisation.
   Workspace Cargo.toml lines 128–138.

3. **solana-sdk 4** and **yellowstone-grpc-client 13.1 / yellowstone-grpc-proto 12.2**
   — present since Phase 1. `solana-sdk` provides `Pubkey` and `Signature` at the Solana
   ingestion boundary. `yellowstone-grpc-client` wraps the Yellowstone gRPC protocol behind
   a Rust SDK that the vendor ships as a companion to the plugin. Workspace Cargo.toml
   lines 41–46, 49–51.

On 2026-04-27 the user stated the principle directly (originally in Russian, translated
here): "I want to depend only on protocols and specifications of what we work with —
everything else, like macros, we can write ourselves for convenience; in any case I do
not want to depend on vendors except for specs and protocols. They can be buggy, they can
be hacked, and I don't want those problems."

This ADR closes the gap that ADR 0003 left open. ADR 0003 banned vendor infrastructure
at runtime. This ADR bans vendor-curated SDK crates at compile time, for the same
underlying reasons applied one layer lower in the stack.

The prohibition is specifically about vendor-curated, domain-specific SDKs, not about
generic implementations of public specifications. A crate that implements gRPC over
HTTP/2 (`tonic` + `prost`) is an implementation of a public spec (Protocol Buffers,
HTTP/2) — it is allowed. A crate that wraps a specific chain's node-internal data model
(`reth-primitives`, which encodes Reth's internal `Block` and `Receipt` representations)
is vendor-curated domain code — it is not allowed in our service crates, regardless of
how well-regarded the vendor is.

### The Yellowstone pattern is the precedent

ADR 0001 §D2 already solved this problem architecturally for Solana. The Yellowstone gRPC
Geyser plugin exposes a `.proto` schema (`geyser.proto`, `solana-storage.proto`). That
schema is the specification. We generate our gRPC client from the `.proto` file using
`tonic-build` and `prost` — both of which implement generic public specifications (gRPC
and Protocol Buffers respectively). The result is a client that speaks the Yellowstone
wire protocol without depending on the vendor-shipped `yellowstone-grpc-client` crate.

This is the template for every other chain boundary. EVM exposes JSON-RPC over WebSocket
(`eth_subscribe`, `eth_getLogs`) — a public specification. We speak it with `reqwest` or
`tokio-tungstenite` and decode the responses with `serde_json`. We do not need `alloy`'s
`WsConnect` or `RpcClient` to do this. EVM ABI encoding is specified in the Ethereum ABI
specification — we can write a decoder that consults only that specification, not alloy's
implementation of it. The Reth ExEx notification stream is a Reth-internal API, not a
public specification; the right analogue for EVM is therefore the same out-of-process
bridge that Yellowstone provides for Solana.

---

## Decision

Two binding rules govern Cargo dependencies in all crates within the main
`mg-onchain-analysis` workspace (every crate under `crates/`, the `server` binary, and
the `client-sdk`):

**Rule A — Allowed categories.** A dependency is allowed if it falls into one of two
categories: (i) language-level Rust infrastructure (async runtime, serialization, error
handling, logging, CLI, process management) that has no domain specificity to any
blockchain or vendor; or (ii) a generic implementation of a public, versioned
specification or protocol standard (HTTP, WebSocket, gRPC/protobuf, Postgres wire, SQL
migrations, TLS), where "public" means the spec is maintained by a standards body or an
open governance process independent of any single vendor.

**Rule B — Vendor SDK isolation.** Vendor-curated crates that implement, wrap, or expose
the internals of a specific ecosystem node, chain SDK, or provider platform are forbidden
in the main workspace. They may be linked exclusively in isolated bridge workspaces under
`bridge/` (separate `Cargo.toml` workspace roots, separate `Cargo.lock`, separate
binaries). A bridge's only permitted output is wire-format messages defined by a proto
schema that lives inside the main workspace under `crates/chain-adapter-proto/` or
equivalent. The bridge binary is a separate process; its types never cross the
compile-time boundary into the main workspace.

These rules apply to both direct dependencies and intentionally chosen transitive
dependencies. They do not govern accidental transitive dependencies pulled in by
allowed crates — those are a `cargo audit` / `cargo deny` concern, not this ADR's
scope.

---

## Allowed and Banned Dependencies

The table below enumerates the current and expected near-term dependency surface under
this policy. "Allowed (main workspace)" means the crate may appear in any `crates/*/Cargo.toml`.
"Allowed (bridge only)" means the crate may appear only in a `bridge/*/Cargo.toml` workspace.
"Banned" means it must not appear in any `Cargo.toml` reachable from the main workspace root.

| Crate | Status | Rationale |
|---|---|---|
| `serde`, `serde_json` | Allowed (main workspace) | Generic serialization framework; no domain specificity |
| `tokio` | Allowed (main workspace) | Async runtime; language-level infrastructure |
| `anyhow`, `thiserror` | Allowed (main workspace) | Error handling; language-level infrastructure |
| `tracing`, `tracing-subscriber` | Allowed (main workspace) | Structured logging; no domain specificity |
| `clap` | Allowed (main workspace) | CLI argument parsing; language-level infrastructure |
| `toml` | Allowed (main workspace) | Config file format; public TOML spec |
| `tokio-util`, `tokio-stream` | Allowed (main workspace) | Async utilities; language-level infrastructure |
| `url` | Allowed (main workspace) | RFC 3986 URL parsing; public spec |
| `async-trait`, `async-channel` | Allowed (main workspace) | Async trait ergonomics; language-level |
| `futures` | Allowed (main workspace) | Async combinators; language-level |
| `tonic`, `prost`, `tonic-build` | Allowed (main workspace) | gRPC over HTTP/2 + protobuf; public spec implementations |
| `tokio-tungstenite` | Allowed (main workspace) | WebSocket RFC 6455; public spec implementation |
| `reqwest`, `hyper` | Allowed (main workspace) | HTTP/1.1 and HTTP/2; public spec implementations |
| `sqlx` | Allowed (main workspace) | Postgres wire protocol + SQL; public spec |
| `chrono` | Allowed (main workspace) | ISO 8601 / RFC 3339 time; public spec |
| `uuid` | Allowed (main workspace) | RFC 4122 UUID; public spec |
| `base58`, `bs58` | Allowed (main workspace) | Base58 encoding algorithm; public algorithm spec |
| `hex` | Allowed (main workspace) | Hexadecimal encoding; public algorithm spec |
| `rust_decimal` | Allowed (main workspace) | Decimal arithmetic (IEEE 754-2008 Decimal); public spec |
| `primitive-types` (U256 only) | Allowed (main workspace) | 256-bit integer arithmetic; mathematical primitive with no chain-specific semantics — see note below |
| `tiny-keccak`, `sha2`, `sha3` | Allowed (main workspace) | Keccak-256 / SHA-2 / SHA-3; NIST / public spec implementations |
| `rand`, `rand_core` | Allowed (main workspace) | PRNG; public algorithm specs |
| `statrs` | Allowed (main workspace) | Statistical distributions; mathematical algorithms |
| `syn`, `quote`, `proc-macro2` | Allowed (main workspace) | Rust proc-macro language tooling; language-level |
| `tokio-retry` | Allowed (main workspace) | Retry logic; language-level infrastructure |
| `prometheus` | Allowed (main workspace) | Prometheus exposition format; public spec |
| `wiremock` | Allowed (main workspace, dev only) | HTTP mock server; no domain specificity |
| `testcontainers`, `testcontainers-modules` | Allowed (main workspace, dev only) | Container lifecycle utilities |
| `alloy-*` (any subcrate) | **Banned** | Vendor-curated EVM SDK; supersedes ADR 0004 §6 |
| `reth-exex` | **Banned (main workspace)** | Vendor-internal Reth streaming API | Allowed (bridge only) |
| `reth-primitives` | **Banned (main workspace)** | Vendor-internal Reth data types | Allowed (bridge only) |
| `reth-node-builder` | **Banned** | Vendor-internal Reth node embedding; supersedes ADR 0004 §8 |
| `reth-tracing` | **Banned (main workspace)** | Reth-specific tracing initialiser | Allowed (bridge only) |
| `solana-sdk`, `solana-*`, `agave-*` | **Banned** | Vendor-curated Solana SDK |
| `yellowstone-grpc-client` | **Banned** | Vendor-shipped gRPC client; the `.proto` is the spec — generate from it |
| `yellowstone-grpc-proto` | **Banned** | Vendor-compiled proto bindings; replace with `tonic-build` from the `.proto` file directly |

Note on `primitive-types::U256`: this crate implements 256-bit integer arithmetic using
a `[u64; 4]` word array with no chain-specific semantics. It contains no ABI logic, no
address types, no chain identifiers, and no EVM-specific operations. It is admitted under
Rule A as a mathematical primitive. If a future audit determines that `primitive-types`
has acquired EVM-specific semantics, it must be replaced with an in-tree U256
implementation at that point.

---

## Reference-Reading Policy

Vendor source code is a legitimate and encouraged resource during implementation. Reading
the source of `alloy-sol-types` to understand how dynamic ABI offsets are handled, or
reading `solana-sdk`'s `Pubkey` implementation to understand Base58Check encoding, is not
a violation of this ADR. The distinction that matters is compile-time linkage, not
intellectual reference.

License categories determine what we may derive from in our own code:

- **MIT or Apache-2.0 licensed vendor code** (`alloy-*`, `reth-*`, `solana-sdk`,
  `yellowstone-grpc-client`) — we may study the source AND derive from it with attribution.
  When an implementation in our codebase is informed by a vendor source, leave a comment
  of the form `// reference: alloy-sol-types::decode (MIT/Apache-2.0)` or
  `// reference: solana-sdk::pubkey::Pubkey (Apache-2.0)`. This comment serves as an
  audit trail, helps future maintainers, and establishes the attribution required by the
  Apache-2.0 terms.

- **AGPL-licensed vendor code** — study at a conceptual level only. No verbatim copy,
  no close derivative. AGPL propagates to consuming works; any code we write that is a
  derivative of AGPL source would require us to publish our own source under AGPL. Before
  reading any Solana-ecosystem or ancillary crate at the source level, verify its license.
  `solana-sdk` and `yellowstone-grpc-client` are Apache-2.0; however some adjacent
  tooling (e.g., certain validator utilities) may be AGPL. Treat AGPL as study-only.

The goal of reference-reading is to avoid reinventing edge-case handling that vendors
solved correctly: dynamic ABI type offsets, U256 little-endian word layout, Yellowstone
slot status enum semantics. We own every line we ship. The vendor's release cadence,
supply-chain surface, and transitive dependencies do not enter our build.

---

## Macros and Ergonomics Policy

Vendor ergonomics crates exist because the underlying protocols are verbose to work with
directly. The `sol!` macro in `alloy-sol-types` generates typed event and function
decoders from inline Solidity syntax. The `solana-sdk`'s `#[program]` and related macros
generate account deserialization boilerplate. We want equivalent ergonomics without the
vendor dependency.

The policy is: write the macro ourselves, backed exclusively by `syn`, `quote`, and
`proc-macro2` (universal Rust language tooling, permitted under Rule A).

The canonical example for this codebase is an EVM event decode macro. Its interface
would look like:

```rust
event_signature! {
    event Transfer(address indexed from, address indexed to, uint256 value);
}
```

and it would generate, at compile time:

- a `TransferLog` struct with typed fields
- a `TRANSFER_TOPIC0: [u8; 32]` constant (Keccak-256 of the canonical event signature)
- an `impl TryFrom<RawLog> for TransferLog` that performs ABI decoding according to the
  Ethereum ABI specification

This macro lives in `crates/evm-types-macros/`, depends only on `syn`/`quote`/`proc-macro2`,
and is tested independently of any EVM node. Reading the `alloy-sol-macro` source for
inspiration on Solidity parser construction is explicitly encouraged.

The same principle applies to any future Solana-side convenience macros (account layout
decoders, borsh deserialization helpers). The implementation goes in `crates/solana-types/`
or an adjacent `-macros` crate. No `solana-sdk` proc-macro dependency.

---

## Crate Layout Policy

Each chain supported by this service owns a dedicated types and protocol decoder crate
inside the main workspace. These crates contain everything the chain-adapter and detectors
need to parse and represent chain-native data — without linking any vendor SDK.

**`crates/evm-types/`** (Sprint 24 deliverable, per the migration plan below):

- `Address`: 20-byte array with EIP-55 checksum encoding and decoding
- `U256`: re-export of `primitive-types::U256` with a thin wrapper that adds
  `rust_decimal` conversion for display and arithmetic
- `B256`: 32-byte hash type
- ABI decoder: statically typed decoding of Ethereum ABI-encoded data (fixed-size and
  dynamic types, tuple types, indexed vs non-indexed log topics)
- Event decode macro invocation: the `event_signature!` macro from
  `crates/evm-types-macros/` used to generate typed decoders for ERC-20 Transfer,
  Uniswap v2 Swap/Mint/Burn, Uniswap v3 Swap, and any future event signatures
- No `alloy` import anywhere in this crate

**`crates/solana-types/`** (Sprint 26 deliverable):

- `Pubkey`: 32-byte array with Base58 encoding and decoding (replacing `solana_sdk::Pubkey`)
- `Signature`: 64-byte array with Base58 encoding (replacing `solana_sdk::Signature`)
- SPL Token layout decoders: account data parsing for Token Program (v1) and Token-2022,
  written against the SPL on-chain layout spec
- Yellowstone proto bindings: generated by `tonic-build` from the `.proto` files at build
  time, replacing `yellowstone-grpc-proto`
- No `solana-sdk`, `agave-*`, or `yellowstone-grpc-proto` import anywhere in this crate

The `crates/chain-adapter/` crate imports only `crates/evm-types/` and
`crates/solana-types/`, not any vendor SDK. The conversion to `common/` types (`Token`,
`Transfer`, `PoolEvent`, `AnomalyEvent`) happens at the module boundary inside
`chain-adapter/src/{chain}/decode.rs` as it does today, but the source types are now
ours.

---

## Bridge Process Pattern

The Reth ExEx API is a genuine technical asset: reorg-aware, push-based, in-process
streaming with explicit `ChainCommitted` / `ChainReverted` / `ChainUpdated` notification
types. This ADR does not discard that asset; it relocates it to a process boundary.

The bridge pattern for EVM mirrors exactly what Yellowstone does for Solana:

```
Reth node process
  └─ exex-bridge binary (bridge/exex-bridge/ workspace)
       links: reth-exex, reth-primitives (vendor, isolated)
       translates ExExNotification → proto message
       streams over local gRPC socket
           │
           │  gRPC (tonic + prost; spec-compliant)
           │  proto schema: crates/chain-adapter-proto/ (main workspace)
           │
onchain-service process
  └─ crates/chain-adapter/src/ethereum/exex.rs
       links: tonic + prost only (main workspace, spec-compliant)
       translates proto message → Event (common/)
```

The proto schema (`crates/chain-adapter-proto/` or equivalent) lives in the main
workspace and is the shared contract. It is a `.proto` file, which is a textual
specification. The bridge compiles it with `tonic-build` under its own `build.rs`. The
main workspace also compiles it with `tonic-build` under its own `build.rs`. The two
generated Rust files are independent; no vendor type ever crosses the process boundary.

The operational model is identical to the existing Solana setup: a self-hosted Reth node
runs the `exex-bridge` process (analogous to the Yellowstone Geyser plugin), and the
`onchain-service` connects to it via a local gRPC socket. Version coupling (bridge binary
must match Reth node version) is the same constraint that already exists with the
Yellowstone plugin version suffix, and is managed identically: the bridge workspace's
`Cargo.lock` pins the reth-* tags; the Reth node version is pinned in the deployment
runbook.

**`bridge/` directory layout:**

```
bridge/
  exex-bridge/
    Cargo.toml        — separate workspace root; pins reth-exex=v1.11.3 (or later)
    Cargo.lock        — isolated lockfile; never merged into the main workspace
    build.rs          — tonic-build invocation against proto files symlinked from main ws
    src/
      main.rs         — Reth NodeBuilder + install_exex + gRPC server
```

The `bridge/` directory is excluded from the main workspace's `Cargo.toml`
`[workspace] members` list. `cargo build` from the main workspace root never touches
`bridge/`.

---

## Consequences

### Positive

**Zero vendor SDK supply-chain surface in the main binary.** The `onchain-service` binary
links no crates whose authors are capable of introducing malicious code via a routine
version bump. The only external build-time code that executes is language-level
infrastructure (`tokio`, `serde`, etc.) and generic protocol implementations (`tonic`,
`reqwest`). These have a vastly smaller attack surface than a chain-specific SDK.

**Zero coupling to vendor release cadence.** Alloy's semver-breaking changes, Reth's
`rust-toolchain.toml` pin, and Solana SDK's `solana_program` ABI evolution no longer
dictate when we must update our workspace. We own every line that compiles into our
binary.

**Full-text auditability.** Every type and function that processes on-chain data is
in-tree. A security auditor reviewing the service does not need to read alloy, reth, or
solana-sdk source to understand how an EVM address is normalised or how a Yellowstone
account update is decoded. The entire decode path is readable in `crates/evm-types/`
and `crates/solana-types/`.

**Architectural symmetry.** The Reth ExEx bridge process mirrors the Yellowstone Geyser
plugin exactly: both are out-of-process translators that speak a gRPC protocol we define,
running alongside the node binary. Operational complexity is consistent across chains and
already understood by the team.

### Negative

**Implementation cost is significant.** Writing correct ABI decoders, address normalisation,
and Solana layout parsers from first principles takes calendar time. Estimated sprint
allocation:

| Deliverable | Estimated cost |
|---|---|
| `crates/evm-types/` + `crates/evm-types-macros/` + D12/D13 migration | ~1.5 sprints |
| `bridge/exex-bridge/` + proto schema + `exex.rs` gRPC client | ~1 sprint |
| `crates/solana-types/` + yellowstone proto regeneration | ~1.5 sprints |
| Migration of 5+ crates currently importing `solana-sdk` | ~0.5 sprint (parallel) |

Total estimated: approximately 4.5 sprints of implementation work, distributed across
Sprints 24 through 26 per the migration plan below. This cost is accepted in exchange
for the supply-chain guarantees above.

**We own the ABI decoder bug surface.** Alloy's ABI decoder has been exercised against
thousands of mainnet transactions and has accumulated substantial real-world test coverage.
Our decoder will start with fewer battle tests. Mitigation: use alloy's decoder as a
reference implementation when writing fixtures; run our decoder against the same
mainnet transaction corpus; leave `// reference: alloy-primitives` attribution comments
where we consult it for edge cases (dynamic type offsets, nested tuples, packed encoding).

**Bridge adds a process boundary.** The `exex-bridge` binary must be co-located with
the Reth node and reachable from `onchain-service` over a local socket. This adds one
process to the deployment manifests for EVM chains. As noted above, this is identical
to the existing Solana deployment (node + Yellowstone plugin), so it does not introduce
a new operational pattern — it extends one already in place.

### Neutral

The `.proto` files for the Yellowstone protocol and for the new ExEx bridge schema live
in the main workspace. Both are compiled with `tonic-build` at build time. This means
`tonic` and `prost` remain in the workspace and the overall gRPC machinery stays the same.
No net new build-time tool is introduced.

The `crates/common/` types (`Transfer`, `PoolEvent`, `AnomalyEvent`, `Token`, `Address`)
are unaffected by this ADR. They already contain no vendor types. The migration work is
entirely inside `chain-adapter`, `evm-types`, and `solana-types`.

---

## Migration Plan

This is a high-level sequence. Detailed task decomposition is owned by individual sprint
runbooks. The ordering respects compilation dependencies: lower-level crates (`evm-types`)
must exist before the crates that import them (`chain-adapter`) are migrated.

### Sprint 24 — Remove in-flight vendor EVM deps; create `crates/evm-types/`

1. Remove the `exex` Cargo feature from `crates/chain-adapter/Cargo.toml` and
   `crates/server/Cargo.toml`. This deletes `reth-exex`, `reth-primitives`, and
   `reth-tracing` from the main workspace build graph immediately.
2. Remove the `onchain-reth` binary entry (`src/main_reth.rs`) from
   `crates/server/Cargo.toml`. The stub binary was Sprint 24 scope only.
3. Remove `reth-exex`, `reth-primitives`, `reth-node-builder`, `reth-tracing` from the
   workspace `Cargo.toml` `[workspace.dependencies]` section.
4. Create `crates/evm-types/` with `Address` (EIP-55), `B256`, U256 re-export, and the
   ABI decoder skeleton.
5. Create `crates/evm-types-macros/` with the `event_signature!` proc-macro.
6. Migrate `crates/chain-adapter/src/ethereum/` to use `evm-types` types. Remove the
   `alloy` workspace dependency from `crates/chain-adapter/Cargo.toml`.
7. Migrate `crates/detectors/` D12 (Permit2) and D13 (sandwich MEV) if they import
   `alloy` directly. Remove `alloy` from their `Cargo.toml` entries.
8. Once all consumers of `alloy` are migrated, remove `alloy` from workspace
   `Cargo.toml` `[workspace.dependencies]`.

### Sprint 25 — Build `bridge/exex-bridge/`; wire gRPC client in `chain-adapter`

1. Define the EVM streaming proto schema in `crates/chain-adapter-proto/` (or a file
   within `crates/chain-adapter/proto/`). The schema covers the structural equivalent
   of `ExExNotification`: committed blocks with transactions and logs, reverted block
   numbers, finality signals.
2. Create `bridge/exex-bridge/` as a separate Cargo workspace. It links `reth-exex`,
   `reth-primitives`, `reth-node-builder` (pinned to the Reth node version in the
   deployment runbook). Its `main.rs` installs the ExEx via `NodeBuilder::launch_with_runner`
   and serves the proto stream over a local Unix socket or TCP loopback.
3. Add `crates/chain-adapter/src/ethereum/exex.rs` — a `tonic`-based gRPC client that
   connects to the bridge socket and translates proto messages to `Event` values.
4. Update `infra/ethereum-node/README.md` with the two-process deployment model.

### Sprint 26 — Create `crates/solana-types/`; regenerate Yellowstone client from proto

1. Create `crates/solana-types/` with `Pubkey`, `Signature`, SPL Token layout decoders,
   and `tonic-build`-generated Yellowstone gRPC bindings.
2. Add a `build.rs` to `crates/chain-adapter` that runs `tonic-build` against the
   Yellowstone `.proto` files (copied once from the upstream repo into
   `crates/chain-adapter/proto/yellowstone/` as a vendored spec asset, not a vendored crate).
3. Migrate `crates/chain-adapter/src/solana/` to use `solana-types::Pubkey` and
   `solana-types::Signature` instead of `solana_sdk::Pubkey` and `solana_sdk::Signature`.
4. Remove `solana-sdk` from `crates/chain-adapter/Cargo.toml` and from the workspace
   `[workspace.dependencies]`. Confirm no remaining import with `cargo check --all-targets`.
5. Remove `yellowstone-grpc-client` and `yellowstone-grpc-proto` from
   `crates/chain-adapter/Cargo.toml` and the workspace `[workspace.dependencies]`.
6. Audit remaining crates (`dex-adapter`, `token-registry`, `detectors`, `server`) for
   any direct `solana_sdk` import. Each found import is a migration task for this sprint.

---

## Exception Process

Any proposed addition of a new Cargo dependency to the main workspace must be evaluated
against the two-category test of Rule A before it is added.

The proposer answers the following three questions in the pull request description or
ADR amendment:

1. Is this crate implementing a public, versioned specification or protocol maintained
   independently of any single vendor? If yes, cite the specification (RFC, EIP, BEP, or
   equivalent). If no, proceed to question 2.

2. Is this a vendor-curated SDK for a specific ecosystem, chain, or node implementation?
   If yes: can the required functionality be placed in an isolated bridge workspace under
   `bridge/` instead? If a bridge is viable, that is the required path. If a bridge is
   not viable, a written justification and amendment to this ADR is required before the
   dependency is added.

3. If neither category applies unambiguously: written justification explaining the
   classification, plus an ADR amendment, are required before the dependency is added.

This process applies to both direct dependencies and to intentional elevation of
transitive dependencies to direct dependencies.

---

## Relationship to Existing ADRs

**ADR 0001 §D2** established the Yellowstone out-of-process bridge as the Solana ingestion
pattern. This ADR generalises that pattern: every chain's vendor-specific streaming API
is reached via an out-of-process bridge that speaks a gRPC protocol defined in our
workspace. ADR 0001 §D2 is not superseded; it is confirmed and extended.

**ADR 0003** banned 3rd-party SaaS in the production hot path. This ADR extends the same
doctrine to the compile-time dimension. The root motivation is identical: we are an
analytics service feeding four financial consumers; any code path we did not author is a
risk vector. ADR 0003 is not superseded; this ADR adds a new dimension to the same
principle.

**ADR 0004** recommended Reth as the EVM node (§Decision, §Trade-off Analysis), noted
that the chain-adapter can depend on `alloy-primitives` and `reth-exex` in service crates
(§6 and §8), and described an in-process embedded library mode (§8). The node-choice
recommendation stands: we run Reth. The two sections are superseded:
- §6 ("Language and ecosystem alignment"): the rationale that alloy's Rust types should
  be imported directly into service crates is superseded. We write our own EVM types in
  `crates/evm-types/`.
- §8 ("Embedded library mode"): the option of embedding `reth-node-builder` in-process
  is superseded. The out-of-process bridge in `bridge/exex-bridge/` provides the same
  ExEx streaming capability without linking reth-* into `onchain-service`.

---

## References

| # | Source | Claim grounded |
|---|---|---|
| 1 | `docs/adr/0001-phase0-synthesis.md` §D2 | Yellowstone gRPC as out-of-process bridge pattern; provider-agnostic design; self-hosted validator default |
| 2 | `docs/adr/0003-self-sovereign-infrastructure.md` | Parent doctrine: no 3rd-party SaaS in hot path; runtime risk argument |
| 3 | `docs/adr/0004-evm-node-choice-geth-vs-reth.md` §6 | Superseded: "language and ecosystem alignment" rationale for alloy in service crates |
| 4 | `docs/adr/0004-evm-node-choice-geth-vs-reth.md` §8 | Superseded: "embedded library mode" rationale for reth-node-builder in-process use |
| 5 | `memory/feedback_no_vendor_cargo_deps.md` | User directive 2026-04-27: depend on specs and protocols, not vendor crates |
| 6 | `Cargo.toml` lines 41–46 | yellowstone-grpc-client 13.1, yellowstone-grpc-proto 12.2, tonic 0.14, prost 0.14 — current Yellowstone vendor footprint |
| 7 | `Cargo.toml` lines 49–51 | solana-sdk 4 — current Solana vendor footprint |
| 8 | `Cargo.toml` lines 99–138 | alloy 1.6 and reth-* git-pinned at v1.11.3 — current EVM vendor footprint |
| 9 | `crates/chain-adapter/Cargo.toml` lines 1–6 | `exex` Cargo feature gating reth-exex + reth-primitives — being removed by this ADR |
| 10 | `crates/server/Cargo.toml` lines 29–37 | `onchain-reth` binary entry requiring `exex` feature — being removed by this ADR |
| 11 | Ethereum ABI specification | https://docs.soliditylang.org/en/latest/abi-spec.html — the public spec governing our in-tree ABI decoder |
| 12 | Yellowstone gRPC protocol `.proto` files | https://github.com/rpcpool/yellowstone-grpc/tree/master/yellowstone-grpc-proto/proto — the spec we generate from, not the vendor crate we link |
| 13 | EIP-55 (mixed-case checksum address encoding) | https://eips.ethereum.org/EIPS/eip-55 — the spec governing `crates/evm-types/` Address type |
| 14 | Rust crates.io supply-chain risk precedent | https://blog.rust-lang.org/2022/09/14/cargo-cves.html — documented CVEs from malicious registry crates |
