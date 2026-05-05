# Design 0026 — Solana Stack Divestment (Sprint 25)

**Date:** 2026-04-27
**Status:** Accepted (user sign-off 2026-04-27, blanket "ок" on all 7 §11 decisions)
**Author:** architect agent
**Sprint:** 25 (repurposed from the superseded ExEx bridge design 0025)

**ADR refs:**
- ADR 0001 §D2 — Yellowstone gRPC as the canonical out-of-process streaming pattern for
  Solana. Remains fully in force. This design is the completion of its promise.
- ADR 0003 — self-sovereign infrastructure. Zero third-party SaaS in the production hot path.
- ADR 0006 — code-level self-sovereignty (post-amendment, bridge escape hatch closed).
  `solana-sdk`, `yellowstone-grpc-client`, and `yellowstone-grpc-proto` are now listed as
  banned everywhere in the repository, not merely in service crates.

**Predecessor / superseded:**
- `docs/designs/0025-exex-bridge-out-of-process.md` — SUPERSEDED 2026-04-27. Preserved for
  historical record. Do not implement against it.

**Symmetry reference:**
- Sprint 24 — `crates/evm-types/`, `crates/evm-types-macros/`, `chain-adapter/src/jsonrpc/`
  removed `alloy-*` from the workspace. Sprint 25 applies the same operation to the Solana
  side.

---

## §1 Status / Date / Author / Sprint / ADR Refs

See header above. The status "Proposed — awaits user sign-off on §11 decisions" means
implementation begins only after the user has confirmed or redirected each of the seven
sign-off items in §11. The spec is otherwise complete for implementation.

---

## §2 Goals and Non-Goals

### §2.1 Goals

- Remove `solana-sdk = "4"` from every crate in the main workspace: `chain-adapter`,
  `detectors`, `dex-adapter`, `token-registry`, and `server` (dev-dep). After Sprint 25
  the workspace `[workspace.dependencies]` block contains no `solana-*` or `agave-*`
  Cargo entries.
- Remove `yellowstone-grpc-client = "13.1"` from the workspace. Replace the vendor-shipped
  gRPC client with a tonic-generated client built from the vendored `.proto` schema inside
  a new `crates/yellowstone-proto/` package.
- Remove `yellowstone-grpc-proto = "12.2"` as a Cargo dependency. The `.proto` files from
  that package are copied into our repository as a spec asset under
  `crates/yellowstone-proto/proto/` and checked in. They are compiled by our own `build.rs`
  via `tonic-build`. The Cargo crate is no longer referenced.
- Create `crates/solana-types/` providing `Pubkey`, `Signature`, `Slot`, `Hash`, and
  `Epoch` as our own types — the Solana analogue of `crates/evm-types/`.
- Achieve full architectural symmetry with the Sprint 24 EVM divestment. After this sprint
  both chain sides are consumed via wire protocols only (gRPC / JSON-RPC) from our own
  generated or hand-written code.
- Update `infra/solana-validator/README.md` to clarify the build-from-source requirement
  for the Yellowstone plugin, and document the proto re-vendoring procedure.

### §2.2 Non-Goals

- Writing our own Solana validator or consensus client. Agave (`anza-xyz/agave`) runs as a
  separate process on the operator's hardware, identical to how it does today.
- Replacing the Yellowstone Geyser plugin. The plugin runs inside the Agave validator
  binary, built from the `rpcpool/yellowstone-grpc` repo at a pinned tag. We compile it
  from source and load it as a `.so`. We do not own or modify its code; we own only our
  client.
- Token-2022 detector additions. These remain on the Sprint 24 deferral list and are
  Sprint 26+ scope.
- Observability hardening (Prometheus + Grafana dashboards). Deferred from Sprint 24,
  remains deferred.
- Stage 2 FDR smart-money calibration. Deferred from Sprint 23, remains deferred.
- EVM chain additions (Base, BSC, Arbitrum, Polygon). Phase 4 scope.
- Pump.fun detector (D14+). Deferred to a future sprint.

---

## §3 Architectural Overview

### §3.1 Three-process model

```
┌────────────────────────────────────────────────────────────────────────────────┐
│  Host machine (bare metal / dedicated server per ADR 0003)                     │
│                                                                                │
│  ┌───────────────────────────────────────────────────────────────────────┐    │
│  │  solana-validator process (Agave)                                     │    │
│  │                                                                       │    │
│  │  binary: agave-validator (built from source: anza-xyz/agave v3.1.13) │    │
│  │  plugin: yellowstone-grpc plugin .so                                  │    │
│  │         (built from source: rpcpool/yellowstone-grpc                  │    │
│  │          v12.2.0+solana.3.1.13)                                       │    │
│  │                                                                       │    │
│  │  ports:  8899 (JSON-RPC)       — backfill + getAccountInfo            │    │
│  │          10000 (gRPC)          — live streaming (geyser.proto)        │    │
│  │          1234 / 8999 (Prometheus) — metrics                           │    │
│  └────────────────────────────────────────────────┬──────────────────────┘    │
│                                                   │                           │
│                     gRPC, tonic-generated client  │  geyser.proto (vendored)  │
│                     our code in crates/yellowstone-proto/ + chain-adapter      │
│                                                   │                           │
│  ┌────────────────────────────────────────────────▼──────────────────────┐    │
│  │  onchain-service process  (main workspace, crates/server binary)      │    │
│  │                                                                       │    │
│  │  crates/chain-adapter/src/solana/subscribe.rs                        │    │
│  │    uses: crates/yellowstone-proto (our generated GeyserClient)       │    │
│  │    uses: crates/solana-types::{Pubkey, Signature, Slot}              │    │
│  │    ZERO yellowstone-grpc-client / solana-sdk imports                 │    │
│  │                                                                       │    │
│  │  port 8080 (REST API)     port 8090 (WS streaming)                   │    │
│  └───────────────────────────────────────────────────────────────────────┘    │
│                                                                                │
│  ┌────────────────────────────────────────────────────────────────────────┐   │
│  │  postgres-16 process                                                   │   │
│  │  port 5432 (pgwire)                                                    │   │
│  └────────────────────────────────────────────────────────────────────────┘   │
└────────────────────────────────────────────────────────────────────────────────┘
```

### §3.2 Narrative

Three processes run on the same host. The topology is unchanged from today's runtime
deployment; only the compile-time composition of the `onchain-service` binary changes.

The Agave validator is a separate process we build from source, not a binary we ship.
It loads the Yellowstone Geyser plugin as a shared library (`.so`) at startup. The plugin
is built from source at a pinned tag whose version suffix must match the Agave version.
This build + versioning discipline already exists in `infra/solana-validator/README.md`
and is confirmed here. Our code does not touch this process.

The Yellowstone Geyser plugin exposes a gRPC server on port 10000 using the `geyser.proto`
schema. That schema is the specification. We vendor it into `crates/yellowstone-proto/proto/`
as a checked-in text file and compile it at build time using `tonic-build`. The result is
a `GeyserClient` struct and associated message types that are entirely our generated code
— no Cargo dependency on the vendor's Rust crate.

The `onchain-service` process imports `crates/yellowstone-proto` (our generated client)
and `crates/solana-types` (our Pubkey/Signature/Slot types) instead of the current
`yellowstone-grpc-client`, `yellowstone-grpc-proto`, and `solana-sdk` crates. The gRPC
streaming session, reconnect loop, and slot-update handling in `chain-adapter/src/solana/`
continue to work identically at runtime; only the import paths and type names change.

The wire protocol between the validator and the service is unchanged — it is the same
Yellowstone gRPC protocol over the same socket. No message format migration is needed.
This is the correct abstraction: the protocol is the contract, not the vendor crate.

The EVM side (Sprint 24, now complete) follows the identical pattern: self-hosted Reth
exposes JSON-RPC over WebSocket, consumed by our in-tree `JsonRpcClient` with no alloy
imports. The resulting architecture is symmetric: both chains are reached via public wire
protocols from our own code, with zero vendor SDK crates in the main workspace binary.

The Postgres process is unchanged. Its connection path (`sqlx` over pgwire) was already
ADR 0006-compliant and is not affected by this sprint.

---

## §4 Audit of Current Vendor Footprint

The following table documents every `use solana_sdk::*` and `use yellowstone_grpc*::*`
import site discovered by auditing the source tree. File paths are relative to the
workspace root.

### §4.1 `yellowstone-grpc-client` and `yellowstone-grpc-proto`

All Yellowstone vendor imports are confined to `crates/chain-adapter/src/solana/`.

| File | Import | What it provides |
|---|---|---|
| `crates/chain-adapter/src/solana/subscribe.rs:51` | `yellowstone_grpc_client::GeyserGrpcClient` | vendor gRPC client; connect + subscribe_once |
| `crates/chain-adapter/src/solana/subscribe.rs:52–56` | `yellowstone_grpc_proto::geyser::{subscribe_update::UpdateOneof, SlotStatus, SubscribeRequest, SubscribeRequestFilter*}` | proto message types for subscribe request + update dispatch |
| `crates/chain-adapter/src/solana/subscribe.rs:540` | `yellowstone_grpc_proto::geyser::CommitmentLevel` | enum used in subscribe request builder |
| `crates/chain-adapter/src/solana/config.rs:126` | `yellowstone_grpc_proto::geyser::CommitmentLevel` | `CommitmentConfig::to_proto()` conversion |
| `crates/chain-adapter/src/solana/config.rs:276` | `yellowstone_grpc_proto::geyser::CommitmentLevel` | same conversion in test helper |
| `crates/chain-adapter/src/solana/mod.rs:172` | `yellowstone_grpc_client::GeyserGrpcClient` | health_check() method |
| `crates/chain-adapter/src/solana/mod.rs:197` | `yellowstone_grpc_client::GeyserGrpcClient` | tip() method: get_slot() call |

Total: 7 import sites across 3 files, all within `crates/chain-adapter/src/solana/`. No
other crate imports Yellowstone vendor types.

### §4.2 `solana-sdk`

Spread across five crates. Broken down by crate and type used.

**`crates/chain-adapter/src/solana/`** (4 sites, 2 files)

| File | Lines | Type used | Purpose |
|---|---|---|---|
| `subscribe.rs:356–378` | ~22 lines | `solana_sdk::pubkey::Pubkey` | Converts raw 32-byte slices from proto `account_keys` into `Pubkey` for `to_string()` display |
| `backfill.rs:311` | ~4 lines | `solana_sdk::pubkey::Pubkey` | Same pattern in backfill JSON-RPC response parser |
| `decode.rs:48` | 1 line | `solana_sdk::pubkey::Pubkey` | Type annotation on `TxDecodeInput::account_keys: &[Pubkey]` field |

**`crates/detectors/src/d01_honeypot.rs`** (6 sites)

| Lines | Type used | Purpose |
|---|---|---|
| 106 | `solana_sdk::hash::Hash` | Blockhash type for simulation transaction construction |
| 107 | `solana_sdk::pubkey::Pubkey` | Address type for pool/token identifiers |
| 108 | `solana_sdk::signer::Signer as _` | Trait import for `payer.pubkey()` call on `Keypair` |
| 1494 | `solana_sdk::transaction::Transaction` | `encode_tx()` helper serialises a transaction to base64 |
| 2909, 2961, 3012, 3087, 3176, 3253 | `solana_sdk::pubkey::Pubkey::new_from_array` | test fixture construction |

**`crates/dex-adapter/src/solana/`** (6 files, ~32 import lines)

| File | Types used |
|---|---|
| `simulation.rs:21–26` | `Instruction`, `AccountMeta`, `Pubkey`, `Keypair`, `SeedDerivable` |
| `pool_accounts.rs:29` | `Pubkey` (struct field in `PoolState`, account key comparisons) |
| `raydium_v4_state.rs:102` | `Pubkey` (packed C-struct field type via `bytemuck`) |
| `raydium_v4.rs:40–45` | `Hash`, `AccountMeta`, `Instruction`, `Pubkey`, `Keypair`, `Signer`, `Transaction` |
| `raydium_cpmm.rs:41–46` | `Hash`, `AccountMeta`, `Instruction`, `Pubkey`, `Keypair`, `Signer`, `Transaction` |
| `openbook_market.rs:82` | `Pubkey` (market vault signer derivation via `create_program_address`) |

**`crates/token-registry/src/rpc.rs`** (2 sites)

| Lines | Type used | Purpose |
|---|---|---|
| 197 | `solana_sdk::pubkey::Pubkey` | `RawAccount.owner` field type (parsed from `getAccountInfo` response) |
| 628 | `solana_sdk::pubkey::Pubkey::from_str` | Parses owner address string from JSON response |

**`crates/server/` (dev-dep only)**

| File | Lines | Type used | Purpose |
|---|---|---|---|
| `tests/d01_simulation_e2e_test.rs:279,589,592,782` | `solana_sdk::pubkey::Pubkey::new_from_array` | Test fixture key construction |
| `tests/sprint8_exit_test.rs:668` | `solana_sdk::pubkey::Pubkey::new_from_array` | Test fixture key construction |

The table above reveals a clean split: `Pubkey` (32-byte address) is used everywhere as
an address type; `Hash`, `Keypair`, `Instruction`, `Transaction`, `AccountMeta`, and
`Signer` are used only in the simulate-sell flow within `dex-adapter` and its callers in
`detectors`. This split directly shapes the `crates/solana-types/` API surface decision
in §5 and the dex-adapter migration strategy question in §11.

---

## §5 `crates/solana-types/` Design

`crates/solana-types/` is the Solana analogue of `crates/evm-types/`. Its role is to
provide the minimum type surface needed by all service crates after the migration, without
linking `solana-sdk`. The `solana-sdk` source is Apache-2.0; reading it for implementation
guidance and leaving attribution comments is explicitly permitted by ADR 0006
§Reference-Reading Policy.

### §5.1 Type inventory

**`Pubkey([u8; 32])`**

The most widely used type across all five affected crates. The implementation requires:
base58 `Display` (for converting pubkeys to strings, the dominant use pattern),
`FromStr` for parsing base58 strings (used in `token-registry`, `dex-adapter` tests, and
`backfill.rs`), and `from_str_const(s: &str) -> Self` for the compile-time constants in
`simulation.rs` (e.g., `COMPUTE_BUDGET_PROGRAM_ID`, `WSOL_MINT`, `SPL_TOKEN_PROGRAM_ID`).
`as_ref() -> &[u8]` is needed by PDA derivation (seeds are byte slices). `Eq + PartialEq
+ Hash + Copy + Clone` are required for use as map keys and struct fields.

The `find_program_address` and `create_program_address` methods are more complex: they
implement Solana's PDA derivation algorithm (hash-and-bump loop over SHA-256 of seeds,
checking off-curve via a try-deserialise on the Ed25519 curve). These methods are used
in `simulation.rs` (ATA derivation) and `openbook_market.rs` (vault signer). The
algorithm is documented in the Solana runtime source
(`solana-program/src/pubkey.rs::create_program_address`). It requires checking whether
a 32-byte value lies on the Ed25519 curve — the standard test is attempting to deserialise
it as a compressed Edwards point using `curve25519-dalek` or an equivalent. We can use
`curve25519-dalek = "4"` (Apache-2.0, public spec, no Solana specificity) for this single
check. Alternatively, `ed25519-dalek = "2"` (which Sprint 25 adds for the signing path)
already transitively provides `curve25519-dalek`. Either way, no `solana-sdk` dependency.

Base58 uses `bs58` already in the workspace.

Estimated implementation: ~150 LOC for `Pubkey` + PDA derivation, ~80 LOC tests.

**`Signature([u8; 64])`**

Used at the `chain-adapter` ingestion boundary when recording transaction signatures.
Requires base58 `Display` and `FromStr`. No cryptographic operations needed at this
layer — validation is the validator's job. Implementation: ~40 LOC + ~30 LOC tests.

**`Hash([u8; 32])`**

Used in `dex-adapter` as the `recent_blockhash` parameter when constructing simulation
transactions. The `Hash` itself is opaque from our perspective — we receive it from the
RPC response, pass it to the transaction builder, and the RPC replaces it via
`replace_recent_blockhash: true` anyway. We need `FromStr` (base58) and `Default`.
Implementation: ~25 LOC + ~15 LOC tests.

**`Slot(u64)`**

A newtype wrapping `u64`. Used in `chain-adapter` for checkpoint tracking. Implements
`From<u64>`, `Into<u64>`, `Display`, and `serde`.

**`Epoch(u64)`**

Analogous newtype. Used less frequently but part of the Yellowstone proto message
surface. Include for completeness; ~10 LOC.

### §5.2 Out of scope for Sprint 25

`Keypair`, `Instruction`, `AccountMeta`, and `Transaction` are needed only by the
`dex-adapter` simulate-sell path. The migration strategy for this path is a §11
sign-off decision: migrate now to `ed25519-dalek` raw signing (option a) or defer
`dex-adapter` to Sprint 26 with a bridging TODO (option b). See §11 decision 1 for
the full analysis.

If option (a) is taken, `Keypair` becomes a thin wrapper around `ed25519_dalek::SigningKey`,
`Instruction` and `AccountMeta` become simple data structs (no cryptographic content),
and `Transaction` follows the Solana serialization format (bincode-compatible layout
documented in the runtime source). If option (b) is taken, `dex-adapter` keeps `solana-sdk`
in Sprint 25 as a single transitional exception pending Sprint 26 cleanup.

### §5.3 Approximate scope

Target: ≤500 LOC in `src/`, ≤300 LOC in tests, excluding generated or derived code.
This is the same scale as `crates/evm-types/` (`address.rs` + `hash.rs` combined are
~300 LOC). The Solana types are smaller because there is no ABI decoder equivalent — the
SPL instruction data format is simpler and is already decoded in-tree in `decode.rs`
without any `solana-sdk` involvement.

### §5.4 Dependency footprint for `crates/solana-types/`

```
[dependencies]
bs58           = { workspace = true }
serde          = { workspace = true }
thiserror      = { workspace = true }
ed25519-dalek  = "2"              # for Keypair wrapping + is_on_curve check in PDA derivation
sha2           = "0.10"           # SHA-256 in PDA derivation (already in workspace via dex-adapter)
```

`ed25519-dalek` implements RFC 8032 (Edwards-Curve Digital Signature Algorithm), a public
IETF spec. It is admitted under ADR 0006 Rule A. It is analogous to `tiny-keccak` in
`crates/evm-types/`.

---

## §6 `crates/yellowstone-proto/` Design

### §6.1 Purpose

`crates/yellowstone-proto/` is a new crate whose sole job is to vendor the Yellowstone
`geyser.proto` (and its dependencies) and expose a tonic-generated `GeyserClient` plus
the associated message types as our public API. It replaces both `yellowstone-grpc-proto`
(compiled proto bindings) and `yellowstone-grpc-client` (hand-rolled tonic client) in one
shot.

### §6.2 Proto vendoring procedure

The `yellowstone-grpc-proto` Cargo crate at version 12.2 contains two `.proto` files:

- `proto/geyser.proto` — the main Geyser streaming API definition
- `proto/solana-storage.proto` — storage-layer types referenced by `geyser.proto`

Both files are copied verbatim from the `rpcpool/yellowstone-grpc` repository at tag
`v12.2.0+solana.3.1.13` into `crates/yellowstone-proto/proto/`. The files are checked
into our repository. They are text, not compiled artefacts, and their contents are the
spec we implement against.

A comment block at the top of each vendored `.proto` file records the provenance:
```protobuf
// Vendored from: https://github.com/rpcpool/yellowstone-grpc
// Tag: v12.2.0+solana.3.1.13
// Vendored on: 2026-04-27
// License: Apache-2.0
// DO NOT EDIT — update via the re-vendoring procedure in
// infra/solana-validator/README.md §proto-revendoring
```

### §6.3 `build.rs` and code generation

```rust
fn main() {
    tonic_build::configure()
        .build_server(false)     // client only; we never serve the plugin protocol
        .compile_protos(
            &["proto/geyser.proto"],
            &["proto/"],
        )
        .expect("tonic-build failed for geyser.proto");
}
```

The generated code is ephemeral — it lives in `$OUT_DIR` and is regenerated on every
build. It is never checked in. This is the standard tonic-build pattern used by
`crates/chain-adapter`'s dev-dep `tonic` today.

### §6.4 `lib.rs` re-exports

The crate exposes a minimal surface:

```rust
pub mod geyser {
    tonic::include_proto!("geyser");
}

pub use geyser::geyser_client::GeyserClient;

// Re-export message types used by chain-adapter subscribe.rs and config.rs.
pub use geyser::{
    CommitmentLevel,
    SubscribeRequest,
    SubscribeRequestFilterAccounts,
    SubscribeRequestFilterSlots,
    SubscribeRequestFilterTransactions,
    SubscribeUpdate,
    subscribe_update::UpdateOneof,
    SlotStatus,
};
```

This replaces `use yellowstone_grpc_client::GeyserGrpcClient` and all
`use yellowstone_grpc_proto::geyser::*` imports in `chain-adapter/src/solana/`.

### §6.5 Proto re-vendoring procedure

When the Yellowstone plugin version needs to be bumped (e.g., to track a new Agave
release):

1. Check out the new tag from `rpcpool/yellowstone-grpc`.
2. Copy `proto/geyser.proto` and `proto/solana-storage.proto` to
   `crates/yellowstone-proto/proto/`, overwriting the existing files.
3. Update the `// Vendored on:` and `// Tag:` comment headers in both files.
4. Run `cargo build -p mg-yellowstone-proto` to verify the new proto compiles.
5. Run `cargo clippy --workspace --all-targets -- -D warnings` to verify no
   regressions in the import surface.
6. Update `infra/solana-validator/README.md §4 Pinned Versions` with the new
   plugin version.
7. Commit both the proto files and the runbook update in the same commit.

The procedure is ~5 minutes of work. Proto versioning is semantic (breaking API changes
result in a major version bump in the tag, not the Cargo semver). Monitor the
`rpcpool/yellowstone-grpc` releases page for breaking proto changes alongside Agave
version bumps.

### §6.6 Approximate scope

Approximately 30 LOC (`build.rs` + `lib.rs` re-exports) of our own code. The generated
Rust code is ~8,000 LOC of protobuf-generated boilerplate, but none of it is authored
or maintained by us. The two vendored `.proto` files total ~400 lines.

---

## §7 Migration Plan Per Service Crate

Each subsection below identifies the specific files and import lines that change, the
replacement import, and the estimated LOC delta. All five crates must be migrated before
the workspace dependency entries can be removed.

### §7.1 `crates/chain-adapter` — largest single block, ~120 LOC touched

**`src/solana/subscribe.rs`**

Remove:
```rust
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, SlotStatus, SubscribeRequest,
    SubscribeRequestFilterAccounts, SubscribeRequestFilterSlots,
    SubscribeRequestFilterTransactions,
};
// (test) use yellowstone_grpc_proto::geyser::CommitmentLevel;
```

Add:
```rust
use mg_yellowstone_proto::{
    GeyserClient,
    UpdateOneof, SlotStatus, SubscribeRequest,
    SubscribeRequestFilterAccounts, SubscribeRequestFilterSlots,
    SubscribeRequestFilterTransactions, CommitmentLevel,
};
```

Replace all `GeyserGrpcClient::build_from_shared(...)` calls with the equivalent tonic
channel construction. The vendor client's `build_from_shared` / `x_token` / `connect`
builder pattern maps directly to tonic's `Channel::from_shared(...).connect().await`
plus metadata interceptors for the auth token. The health check and `get_slot` methods
(used in `mod.rs`) map to `HealthClient::check()` and `GeyserClient::get_latest_blockhash()`
or equivalent RPC calls on the generated client.

The `account_keys` collection (lines 356–378) currently uses `solana_sdk::pubkey::Pubkey`
as an intermediate step: raw bytes arrive as `Vec<u8>` from the proto, are converted to
`Pubkey` for the `to_string()` base58 representation, then the string is used downstream.
After migration this becomes `mg_solana_types::Pubkey::from([u8; 32])` + `.to_string()`.
The net effect is identical; only the type name changes.

Estimated delta: -8 LOC imports + ~15 LOC substitutions in connect/subscribe/health paths
= ~-5 net LOC.

**`src/solana/config.rs`**

Replace `yellowstone_grpc_proto::geyser::CommitmentLevel` with
`mg_yellowstone_proto::CommitmentLevel`. The `to_proto()` method on `CommitmentConfig`
currently returns the vendor enum; it returns our re-export of the generated enum instead.
Estimated delta: ~10 LOC changed.

**`src/solana/decode.rs`**

Line 48: `use solana_sdk::pubkey::Pubkey;` — the `Pubkey` type is used only in the
`TxDecodeInput::account_keys: &[Pubkey]` field. Replace with
`use mg_solana_types::Pubkey;`. The rest of `decode.rs` works on `String` addresses
(`parse_solana_addr` parses strings via `Address::parse`), so no further changes are
needed in this file. Estimated delta: 1 line changed.

**`src/solana/backfill.rs`** (line 311)

Same pattern as `decode.rs`: `Pubkey` is used to parse base58 strings from the
`getBlock` JSON response. Replace with `mg_solana_types::Pubkey`. The parse call
`s.parse::<Pubkey>().ok()` relies on `FromStr`, which our implementation provides.
Estimated delta: 1 line changed.

**`Cargo.toml`**

Remove: `solana-sdk.workspace = true`, `yellowstone-grpc-client.workspace = true`,
`yellowstone-grpc-proto.workspace = true`.

Add: `mg-solana-types = { path = "../solana-types" }`,
`mg-yellowstone-proto = { path = "../yellowstone-proto" }`.

### §7.2 `crates/detectors/src/d01_honeypot.rs` — ~30 LOC touched

This file imports `solana_sdk::hash::Hash`, `solana_sdk::pubkey::Pubkey`, and
`solana_sdk::signer::Signer as _`. The last import is used only to call
`payer.pubkey()` on a `Keypair` — which becomes `payer.verifying_key().to_bytes()` when
`Keypair` is our `ed25519_dalek::SigningKey`-backed type, or a method on our own
`Keypair` wrapper in `crates/solana-types/`.

Replace:
- `solana_sdk::hash::Hash` → `mg_solana_types::Hash`
- `solana_sdk::pubkey::Pubkey` → `mg_solana_types::Pubkey`
- `solana_sdk::signer::Signer as _` → not needed if `Keypair::pubkey()` is a direct
  method on our wrapper type (no trait needed)
- `solana_sdk::transaction::Transaction` in `encode_tx()` → `mg_solana_types::Transaction`
  (if option a is taken) or left as `solana_sdk` in Sprint 25 with a
  `// TODO Sprint 26: migrate to mg_solana_types::Transaction` comment (if option b)

Test fixtures (lines 2909–3253): `solana_sdk::pubkey::Pubkey::new_from_array([byte; 32])`
→ `mg_solana_types::Pubkey::from([byte; 32])`. Six sites in tests only.

**`Cargo.toml`**

Remove: `solana-sdk.workspace = true`, `bincode = "1"` (only if transaction
serialization is removed from this crate in option a; retained if option b).
Add: `mg-solana-types = { path = "../solana-types" }`.

Estimated delta: ~-5 net LOC (removal of trait import lines, simpler method calls).

### §7.3 `crates/dex-adapter/src/solana/` — heaviest migration, ~180–250 LOC depending on option

This crate has the deepest `solana-sdk` surface: `Pubkey`, `Hash`, `AccountMeta`,
`Instruction`, `Keypair`, `Signer`, `SeedDerivable`, `Transaction` across six files.

**Files using only `Pubkey`**: `pool_accounts.rs`, `raydium_v4_state.rs`,
`openbook_market.rs`.

For these files the migration is mechanical: replace `solana_sdk::pubkey::Pubkey` with
`mg_solana_types::Pubkey`. The `find_program_address` and `create_program_address` methods
are on our `Pubkey` type. The `bytemuck` zero-copy cast in `raydium_v4_state.rs` requires
that `mg_solana_types::Pubkey` is `#[repr(C)]` with size 32 — trivially satisfied by
`Pubkey([u8; 32])`.

**Files using `Keypair`, `Instruction`, `AccountMeta`, `Transaction`, `Hash`**:
`simulation.rs`, `raydium_v4.rs`, `raydium_cpmm.rs`.

If §11 decision 1 is option (a): the `Keypair` wrapper in `crates/solana-types/` holds an
`ed25519_dalek::SigningKey`. `from_seed(seed: &[u8; 32])` calls
`SigningKey::from_bytes(seed)`. `pubkey() -> Pubkey` returns
`Pubkey(signing_key.verifying_key().to_bytes())` — the public key bytes, which is exactly
what Solana uses as a wallet address. `sign(message: &[u8]) -> Signature` calls
`signing_key.sign(message)`. The `Instruction`, `AccountMeta`, and `Transaction` types
are data-only structs; we define them in `solana-types` matching the Solana serialization
layout (documented in the runtime source, Apache-2.0). The `Transaction::new_signed_with_payer`
constructor and bincode serialization match the `solana-sdk` implementation identically
because the on-wire format is a protocol spec, not an implementation detail.

Estimated delta under option (a): ~+100 LOC in `crates/solana-types/` + ~-30 LOC of
import lines removed in dex-adapter = net ~+70 LOC across both crates.

If §11 decision 1 is option (b): `dex-adapter` retains `solana-sdk` in Sprint 25. All
Yellowstone-specific imports still move to `crates/yellowstone-proto/` and all `Pubkey`
uses in the pure-decode path of `pool_accounts.rs` and `raydium_v4_state.rs` move to
`mg_solana_types`. The signing path (`simulation.rs`, `raydium_v4.rs`, `raydium_cpmm.rs`)
remains on `solana-sdk` until Sprint 26. A comment `// TODO T26-1: migrate to mg_solana_types`
marks each remaining site. Under this option `solana-sdk` stays in the workspace as a
single-crate transitional exception in `dex-adapter/Cargo.toml` until Sprint 26.

**`Cargo.toml`**

Option (a): Remove `solana-sdk.workspace = true`, `sha2 = "0.10"` (moved to
`crates/solana-types/`). Add `mg-solana-types = { path = "../solana-types" }`.

Option (b): Keep `solana-sdk.workspace = true` until Sprint 26. Remove only the
`yellowstone-grpc-*` workspace entries. Add `mg-solana-types`.

### §7.4 `crates/token-registry/src/rpc.rs` — 2 import sites, ~5 LOC touched

`RawAccount.owner: solana_sdk::pubkey::Pubkey` (line 197) becomes
`mg_solana_types::Pubkey`. The `from_str()` call at line 628 becomes
`Pubkey::from_str(...)` on our type. Both changes are mechanical.

**`Cargo.toml`**

Remove: `solana-sdk.workspace = true`.
Add: `mg-solana-types = { path = "../solana-types" }`.

Estimated delta: 2 LOC changed, 1 Cargo dep line changed.

### §7.5 `crates/server/` — dev-dep only, ~8 LOC touched

`solana-sdk` appears only in `[dev-dependencies]` (line 94 of `Cargo.toml`) and in two
test files. Both test files use only `solana_sdk::pubkey::Pubkey::new_from_array`.

Replace with `mg_solana_types::Pubkey::from([byte; 32])` in each test site. Remove the
`solana-sdk` dev-dep entry from `Cargo.toml`. Add
`mg-solana-types = { path = "../solana-types" }` as a dev-dep.

Estimated delta: ~-5 net LOC.

---

## §8 `infra/solana-validator/` Runbook Update

The existing runbook at `infra/solana-validator/README.md` already covers:

- Hardware requirements (§2), OS preparation (§5–§6), Rust toolchain (§8).
- Building and installing the Agave validator binary from source (§9).
- Generating the identity keypair (§10).
- Building the Yellowstone gRPC plugin from source (§11).
- Configuring the plugin (§12).
- Snapshot sync procedure (§13).
- Validator startup and systemd unit (§14–§15).
- Health checks and monitoring (§16–§17).

The pinned versions in the runbook's §4 table are: Agave `v3.1.13` and Yellowstone plugin
`v12.2.0+solana.3.1.13`. These are the same versions currently pinned in the workspace
`Cargo.toml` for `yellowstone-grpc-proto = "12.2"`.

Sprint 25 requires the following additions to the runbook:

**New subsection — `§4a Proto Re-Vendoring Procedure`** (mirroring §6.5 above). This
documents the five-step procedure for updating the proto files in
`crates/yellowstone-proto/proto/` when bumping to a new Yellowstone plugin version.

**Version-coupling rules (confirm or add)**: The existing §4 already states that the
Yellowstone plugin version suffix must match the Agave version. Sprint 25 adds the
corollary: the proto files in `crates/yellowstone-proto/proto/` must match the plugin
version in the runbook's §4 table. When bumping Agave + plugin, update the workspace
proto files in the same commit that updates the runbook §4 version pins.

**No Docker image change for Sprint 25**: The runbook currently describes building from
source on the host machine. The decision on whether to move to a pre-built Docker image
(§11 decision 4) is a sign-off item. If the user selects the pre-built Docker path, the
runbook gains a `§9a Docker Image Build` section describing the CI pipeline that clones,
builds, and pushes to the private registry on each release tag bump. The systemd unit is
updated to pull the image instead of running the local binary.

---

## §9 Cargo Workspace Cleanup

The following diff applies to `Cargo.toml` at the workspace root after all five crate
migrations are complete.

**Remove from `[workspace.dependencies]`:**

```toml
# REMOVE — replaced by crates/yellowstone-proto/ generated client
yellowstone-grpc-client = "13.1"
yellowstone-grpc-proto  = "12.2"

# REMOVE — replaced by crates/solana-types/
solana-sdk = "4"
```

**Keep unchanged (already present):**

```toml
tonic  = { version = "0.14", features = ["tls-native-roots"] }
prost  = "0.14"
```

Both remain because `crates/yellowstone-proto/` uses `tonic-build` in its `build.rs`
(which requires `prost` at runtime for the generated code) and `crates/chain-adapter`
continues to use `tonic` directly for the gRPC channel.

**Add to `[workspace.dependencies]`:**

```toml
# tonic-build: code generation for crates/yellowstone-proto/build.rs.
# Listed as a workspace dep so the version stays in lock-step with tonic.
tonic-build = "0.14"
```

**Add conditionally (§11 decision 1, option a only):**

```toml
# ed25519-dalek: RFC 8032 Ed25519 signing for crates/solana-types/ Keypair wrapper.
# Admitted under ADR 0006 Rule A: implements a public IETF specification.
# See docs/adr/0006 §Allowed and Banned Dependencies.
ed25519-dalek = "2"
```

**Add for sha2 workspace coordination:**

```toml
sha2 = "0.10"
```

`sha2` is already a direct dep in `crates/dex-adapter/` and is transitively present via
`ed25519-dalek`. Elevating it to a workspace dep ensures a single version across the
workspace and avoids duplication.

**Add workspace members:**

```toml
members = [
    # ... existing entries ...
    # Sprint 25 — ADR 0006 Solana divestment
    "crates/solana-types",
    "crates/yellowstone-proto",
]
```

After the workspace cleanup, `cargo build --release` produces a binary with no
`solana-*`, `agave-*`, or `yellowstone-grpc-*` entries in its Cargo dependency tree. The
only gRPC-adjacent dependencies are `tonic`, `prost`, and our own
`crates/yellowstone-proto`.

---

## §10 Consequences

### Positive

**Zero Solana vendor Cargo crates in the build closure.** The `onchain-service` binary
links no code from `solana-sdk`, `yellowstone-grpc-client`, or `yellowstone-grpc-proto`.
These three crates account for a substantial fraction of the current dependency tree:
`solana-sdk = "4"` alone transitively pulls in `solana-program`, `borsh`, `bs58` (an
older version), and dozens of crypto primitives from the Solana ecosystem. Removing them
reduces compilation time and supply-chain surface in proportion to that tree.

**Symmetric architecture across both chains.** After this sprint, the EVM and Solana
ingestion paths have identical structure: a separate process (Reth / Agave) exposes data
via a wire protocol (JSON-RPC / Yellowstone gRPC). The `onchain-service` binary consumes
each via our own generated or hand-written client code. `crates/evm-types/` and
`crates/solana-types/` are the respective type crates; neither imports vendor code. A
new engineer or auditor can understand the full decode path for either chain by reading
only in-tree code.

**We control the Yellowstone client code.** The generated `GeyserClient` is a direct
function of the vendored `.proto` file. When we re-vendor, we see exactly what changed —
a git diff of the `.proto` is readable; a cargo update of a compiled crate is not.
Protocol-breaking changes in Yellowstone are visible as compile errors in our generated
code, not as silent runtime failures.

**Full-text auditability.** Every type and function that processes Solana-native data is
now in-tree. An auditor reviewing how a Yellowstone `SubscribeUpdateTransactionInfo` is
decoded into a `Transfer` event reads only `crates/yellowstone-proto/`, `crates/solana-types/`,
and `crates/chain-adapter/src/solana/`. No external Cargo crate source required.

### Negative

**Implementation cost.** Writing `crates/solana-types/` from first principles, including
the PDA derivation algorithm, takes careful implementation and testing. Estimated sprint
allocation is approximately 1.5 sprints total (solana-types skeleton + chain-adapter
migration + detectors/token-registry migration) if option (a) is taken for dex-adapter,
or approximately 1.0 sprint if option (b) defers the dex-adapter signing path to Sprint 26.

**Proto re-vendoring is a manual step.** When the Yellowstone plugin version must be
bumped to track a new Agave release (which happens roughly every 4–8 weeks), a developer
must follow the re-vendoring procedure in §6.5. This is approximately five minutes of
work, but it is a manual step that `cargo update` does not handle. The runbook documents
the procedure explicitly. The cost is bounded and predictable.

**We own the PDA derivation bug surface.** `Pubkey::find_program_address` implements a
hash-and-bump loop that is security-critical: incorrect derivation would produce wrong
pool addresses in the simulate-sell flow, causing silent failures. Mitigation: the
algorithm is well-documented in the Solana runtime source (Apache-2.0); we use it as
a reference implementation. Test fixtures for `derive_associated_token_account` already
exist in `dex-adapter/src/solana/simulation.rs` and use real Solscan-verified addresses,
which serves as the ground-truth regression test.

**`dex-adapter` option (a) adds a Solana transaction serialization implementation.**
The `Transaction` serialization format (bincode with a specific header layout) must be
implemented correctly to produce transactions that the RPC will accept for simulation.
The format is stable and documented in the Solana runtime source. However, it does add
implementation surface that must be regression-tested. The existing integration test
`crates/server/tests/d01_simulation_e2e_test.rs` serves as the end-to-end regression
guard once the migration is complete.

### Neutral

The `tonic` and `prost` crates remain in the workspace. They were already there; Sprint 25
does not introduce new generic protocol dependencies, it only relocates Solana-specific code
from a vendor crate to an in-tree crate and a generated crate.

The `ed25519-dalek` addition (option a) is a net reduction in Solana-ecosystem supply-chain
surface: `solana-sdk` itself transitively depended on `ed25519-dalek` (via `solana-zk-token-sdk`
and related crates). We replace the vendor's re-export with a direct, minimal dependency on
the upstream spec implementation crate. The dep is admitted under ADR 0006 Rule A as an
implementation of RFC 8032.

The on-wire behaviour of the system is completely unchanged. The Yellowstone gRPC stream
carries the same bytes; the validator emits the same protocol. The migration is purely at
the compile-time dependency boundary.

The `crates/common/` `Address` enum's Solana variant currently wraps a `String` (the
base58 address). It does not wrap `solana_sdk::Pubkey` directly. This means the switch
from `solana_sdk::Pubkey` to `mg_solana_types::Pubkey` in chain-adapter does not
propagate into `crates/common/` — the boundary conversion at `parse_solana_addr()` in
`decode.rs` is the only touch point.

---

## §11 Sign-Off Decisions

The following seven decisions require explicit user confirmation before implementation
begins. Each item states the recommended path and the alternative.

**Decision 1 — `dex-adapter` signing strategy.**

Option (a): Implement `Keypair`, `Instruction`, `AccountMeta`, and `Transaction` in
`crates/solana-types/` now in Sprint 25, using `ed25519-dalek` for signing. This removes
`solana-sdk` from all five crates in a single sprint. The Transaction serialization
format is stable and documented. Total additional implementation: ~100 LOC in
`solana-types/` + ~180 LOC migration in dex-adapter.

Option (b): Defer `dex-adapter`'s signing path to Sprint 26. `solana-sdk` stays only in
`crates/dex-adapter/Cargo.toml` (not in the workspace `[workspace.dependencies]` as a
shared dep, but as a direct dep on that single crate). Workspace-level `solana-sdk`
removal is complete except for dex-adapter. Sprint 25 scope is reduced by ~100 LOC.

**Recommendation:** Option (a) if the 1.5-sprint estimate is acceptable. The PDA
derivation and Transaction serialization are the only non-trivial pieces; both have
ground-truth regression tests in the existing test suite. Option (b) is safer if the
sprint window is tight. The architect leans toward (a) for full divestment symmetry with
Sprint 24.

**Decision 2 — Proto vendoring location.**

Option (in-tree): `crates/yellowstone-proto/proto/geyser.proto` — proto files live
inside the crate package. Single crate, single source of truth. `build.rs` paths are
relative. Simpler.

Option (top-level): `proto/yellowstone/geyser.proto` — a top-level `proto/` directory
shared with any future chains that may need proto vendoring (Cosmos IBC, etc.).

**Recommendation:** In-tree. For now there is one proto package and one crate. Adding a
shared `proto/` directory introduces a cross-crate path dependency that complicates
`build.rs` and workspace path management. Re-evaluate when a second chain requires a
similar proto crate.

**Decision 3 — `Pubkey` display encoding.**

Base58 is the canonical Solana convention for displaying addresses, transaction
signatures, and blockhashes. It is what every existing call site in the codebase
produces and expects. The `bs58` crate is already a workspace dependency.

**Recommendation:** Base58 confirmed. No alternative is viable — changing to hex would
break all cross-references against external tools (Solscan, Helius, CLI tools).

**Decision 4 — Validator/plugin Docker image strategy.**

Option (pre-built): The Agave + Yellowstone plugin Docker image is built in CI on each
release tag bump and pushed to the private container registry. Service deployment pulls
the pre-built image. Faster service-deploy times; reproducible image hashes.

Option (build-on-machine): Operator builds from source on the target machine per the
existing runbook procedure. No CI pipeline required; no container registry required.
Higher deploy time (several hours for first build); subsequent builds use `sccache`.

**Recommendation:** Pre-built Docker image for production deployments. Building Agave
from source takes 30–90 minutes on typical CI hardware. Service deploys should not wait
for validator compilation. The image tag encodes the Agave + plugin version, satisfying
the version-coupling requirement. The runbook retains the from-source build instructions
as the fallback for operators without Docker infrastructure.

**Decision 5 — Solana version pin.**

As of 2026-04-27:
- `anza-xyz/agave` latest stable release: `v3.1.13` (from
  `https://github.com/anza-xyz/agave/releases`, last release 2026-04-10, no `-rc`/`-beta`
  suffix).
- `rpcpool/yellowstone-grpc` matching release: `v12.2.0+solana.3.1.13`.

These match the versions currently pinned in `infra/solana-validator/README.md §4` and
in the workspace `Cargo.toml` comment. No version change is required for Sprint 25.

The proto files to vendor are taken from tag `v12.2.0+solana.3.1.13`. The workspace
`tonic = "0.14"` and `prost = "0.14"` are already in the workspace; tonic-build `0.14`
is compatible with prost `0.14`.

**Recommendation:** Pin at `v3.1.13` / `v12.2.0+solana.3.1.13` for Sprint 25. Track the
Agave release feed for `v3.2.x` when it exits RC.

**Decision 6 — `crates/solana-types/` API surface: minimal-first vs full upfront.**

Minimal-first: ship only what the current codebase actually imports — `Pubkey`,
`Signature`, `Hash`, `Slot`, `Epoch`. Add `Keypair`, `Instruction`, `AccountMeta`,
`Transaction` only if decision 1 selects option (a). No SPL account layout decoders
in Sprint 25 (those are Sprint 26 scope per ADR 0006 §Crate Layout Policy).

Full upfront: also include SPL Token account layout decoders, Token-2022 extension
parsers, and other types that will eventually be needed.

**Recommendation:** Minimal-first. Adding types not yet used increases implementation
and review cost for zero Sprint 25 benefit. ADR 0006 §Crate Layout Policy already
reserves SPL layout decoders for `crates/solana-types/` as a Sprint 26 deliverable —
this decision is consistent with that plan.

**Decision 7 — Backwards compat for `Detector` / `ChainAdapter` trait surfaces after
the `Address` type transition.**

The `Address` type in `crates/common/` is an enum-style type across chains. Its Solana
variant is constructed via `Address::parse(Chain::Solana, base58_str)` and represented
internally as a base58 `String`. The Solana variant does not hold a `solana_sdk::Pubkey`
struct — it holds the string representation. This means the type-level migration from
`solana_sdk::Pubkey` to `mg_solana_types::Pubkey` is contained entirely within
`crates/chain-adapter/src/solana/` and the other four crates being migrated. The
`ChainAdapter` trait, the `Detector` trait, and `crates/common/` types are not affected.

**Confirmation required:** The user should confirm that this characterisation of the
`Address` type is accurate and that no `ChainAdapter` or `Detector` implementation
elsewhere in the workspace holds a `solana_sdk::Pubkey` by value in a public struct.
The audit in §4 found no such usage outside the five crates listed, but the user may
have context the audit cannot surface. If the characterisation is correct, this decision
is a non-issue — the migration is transparent to all trait consumers.

---

## §12 Sub-Task Breakdown

Each task is atomic: it compiles and passes `cargo clippy --workspace --all-targets
-- -D warnings` independently before the next task begins. The workspace-scope flag is
mandatory — do NOT use `-p <single-crate>` scope for final verification. Sprint 24 had
a sub-agent over-report because scope was narrowed to a single package; the workspace
flag catches cross-crate regressions. Every task description should include this
requirement in its implementation brief.

**T25-1 — Proto vendoring + `crates/yellowstone-proto/` skeleton**

Create `crates/yellowstone-proto/` with `Cargo.toml`, `build.rs`, and `src/lib.rs`.
Copy `geyser.proto` + `solana-storage.proto` from `rpcpool/yellowstone-grpc` at tag
`v12.2.0+solana.3.1.13`. Add `tonic-build = { workspace = true }` to build-deps.
Add the crate to `[workspace.members]`. Verify: `cargo build -p mg-yellowstone-proto`
succeeds; `cargo clippy --workspace --all-targets -- -D warnings` passes. Dependencies
on later tasks: none. Estimated delta: +35 LOC our code, +~400 lines vendored proto.

**T25-2 — `crates/solana-types/` skeleton (`Pubkey`, `Signature`, `Slot`, `Hash`, `Epoch`)**

Implement the five types. `Pubkey` must include `find_program_address` and
`create_program_address` (PDA derivation with `ed25519-dalek` on-curve check).
`Signature` must include base58 `Display` + `FromStr`. `Hash` must include
`FromStr` (base58) + `Default`. Include ~300 LOC of unit tests covering round-trip
serialization, deterministic ATA derivation against Solscan-verified addresses,
and PDA bump-seed search. Add `ed25519-dalek = "2"` and `sha2 = "0.10"` to
`[workspace.dependencies]`. Add the crate to `[workspace.members]`. Verify workspace
clippy clean. Dependencies: none (independent of T25-1). Estimated delta: ~500 LOC.

**T25-3 — `crates/chain-adapter` migration**

Replace `yellowstone-grpc-client`, `yellowstone-grpc-proto`, and `solana-sdk` imports
in `subscribe.rs`, `config.rs`, `decode.rs`, `backfill.rs`, and `mod.rs`. Use
`mg_yellowstone_proto::GeyserClient` and `mg_solana_types::Pubkey`. Update
`crates/chain-adapter/Cargo.toml`. Run `cargo clippy --workspace --all-targets
-- -D warnings`. Dependencies: T25-1, T25-2. Estimated delta: ~-30 net LOC.

**T25-4 — `crates/token-registry` migration**

Replace `solana_sdk::pubkey::Pubkey` in `rpc.rs` (two sites). Update `Cargo.toml`.
Run `cargo clippy --workspace --all-targets -- -D warnings`. Dependencies: T25-2.
Estimated delta: ~-3 net LOC.

**T25-5 — `crates/dex-adapter` migration (Pubkey-only sites)**

Migrate `pool_accounts.rs`, `raydium_v4_state.rs`, and `openbook_market.rs` to
`mg_solana_types::Pubkey`. This does not touch the signing path yet.

If decision 1 is option (a): also migrate `simulation.rs`, `raydium_v4.rs`, and
`raydium_cpmm.rs` to use `mg_solana_types::{Keypair, Hash, Instruction, AccountMeta,
Transaction}` (requires T25-2 to include these types). Remove `solana-sdk.workspace =
true` from `dex-adapter/Cargo.toml`. Estimated delta: ~-150 net LOC (with option a).

If decision 1 is option (b): add `// TODO T26-1: migrate signing path to mg_solana_types`
to each remaining `solana_sdk::` import. `solana-sdk` stays as a direct dep in
`dex-adapter/Cargo.toml` but is removed from `[workspace.dependencies]`. Estimated
delta: ~-30 net LOC (without option a, Pubkey-only migration).

Run `cargo clippy --workspace --all-targets -- -D warnings`. Dependencies: T25-2.

**T25-6 — `crates/detectors` migration (d01_honeypot.rs)**

Migrate `solana_sdk::hash::Hash`, `Pubkey`, `Signer`, and (if option a) `Transaction`
imports. Update test fixtures to use `mg_solana_types::Pubkey::from([byte; 32])`.
Update `Cargo.toml`. Run `cargo clippy --workspace --all-targets -- -D warnings`.
Dependencies: T25-2 (and T25-5 if option a is taken, since dex-adapter types must be
migrated first). Estimated delta: ~-10 net LOC.

**T25-7 — `crates/server` dev-dep migration + workspace dep cleanup**

Migrate server test files to `mg_solana_types::Pubkey`. Update `[dev-dependencies]`.
Then remove `solana-sdk`, `yellowstone-grpc-client`, and `yellowstone-grpc-proto` from
root `Cargo.toml` `[workspace.dependencies]`. Add `tonic-build`, `ed25519-dalek` (if
option a), and `sha2` workspace entries. Run `cargo clippy --workspace --all-targets
-- -D warnings` one final time to confirm zero remaining `solana_sdk::` or
`yellowstone_grpc` imports. Update `infra/solana-validator/README.md` with the proto
re-vendoring procedure section. Dependencies: T25-3 through T25-6 all complete.
Estimated delta: ~-5 net LOC.

---

## §13 Open Questions and Out of Scope

**SPL Token + Token-2022 account layout decoders.** ADR 0006 §Crate Layout Policy lists
these as a `crates/solana-types/` deliverable but defers them to Sprint 26. They are not
needed for the Sprint 25 scope (which only migrates existing code, not new functionality).

**Pump.fun detector (D14+).** Deferred from Sprint 24; remains deferred. The migration
in this sprint does not affect the detector pipeline other than updating import paths.

**Observability hardening** (Prometheus alerts, Grafana dashboards, SLO definitions).
Deferred from Sprint 24.

**Stage 2 FDR smart-money calibration.** Data-blocked; deferred from Sprint 23.

**Additional EVM detectors.** Sprint 24 deferral list item; Sprint 26+ scope.

**Firedancer compatibility.** The Yellowstone plugin has no Firedancer-native release as
of April 2026. Re-evaluate when `rpcpool/yellowstone-grpc` publishes Firedancer support.
Track `firedancer-io/firedancer` issue tracker.

**Multi-instance `onchain-service`.** CLAUDE.md notes Redpanda/Kafka as the future
streaming layer for multi-instance mode. This is deferred indefinitely; in-process
channels remain sufficient for the current consumer set.

**Holder snapshot backfill.** Large-scale `getProgramAccounts` calls for full holder
snapshots are operationally constrained by the validator's account index configuration.
Deferred to Phase 3 as noted in ADR 0001.

---

## §14 References

| # | Source | Claim grounded |
|---|---|---|
| 1 | `docs/adr/0001-phase0-synthesis.md` §D2 | Yellowstone gRPC as canonical out-of-process pattern; provider-agnostic design |
| 2 | `docs/adr/0003-self-sovereign-infrastructure.md` | Self-hosted validator as production default; zero 3rd-party SaaS in hot path |
| 3 | `docs/adr/0006-code-level-self-sovereignty.md` (post-amendment) | `solana-sdk`, `yellowstone-grpc-client`, `yellowstone-grpc-proto` banned everywhere; bridge escape hatch closed |
| 4 | `solana-sdk` source (Apache-2.0) | Reference implementation for `Pubkey::find_program_address`, `Transaction` serialization layout. Not linked — read for implementation guidance per ADR 0006 §Reference-Reading Policy. `https://github.com/solana-labs/solana/blob/master/sdk/program/src/pubkey.rs` |
| 5 | `rpcpool/yellowstone-grpc` proto files | Spec to vendor: `https://github.com/rpcpool/yellowstone-grpc/tree/v12.2.0+solana.3.1.13/yellowstone-grpc-proto/proto` |
| 6 | `anza-xyz/agave` releases | Version tracking: `https://github.com/anza-xyz/agave/releases` |
| 7 | RFC 8032 — Edwards-Curve Digital Signature Algorithm (Ed25519) | Specification governing `ed25519-dalek` and our `Keypair` wrapper. `https://www.rfc-editor.org/rfc/rfc8032` |
| 8 | Base58 encoding specification (Bitcoin Wiki) | Encoding algorithm used by `Pubkey::Display` + `FromStr`. `https://en.bitcoin.it/wiki/Base58Check_encoding` |
| 9 | `infra/solana-validator/README.md` | Existing runbook; §4 Pinned Versions, §11 Build Yellowstone plugin |
| 10 | `Cargo.toml` lines 41–58 | Current Solana vendor footprint (yellowstone-grpc-client 13.1, yellowstone-grpc-proto 12.2, solana-sdk 4) |
