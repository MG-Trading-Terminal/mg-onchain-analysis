# Session Kickoff — Sprints 24+25 CLOSED (full ADR 0006 realization) / Sprint 26 OPEN (carry-forward backlog, no doctrinal blockers)

**Read this first after session restart.** Sprints 24+25 closed 2026-04-27 same day. **ADR 0006 fully realized end-to-end.** Vendor SDK Cargo crates removed across both EVM (Sprint 24: alloy + reth-*) and Solana (Sprint 25: solana-sdk + yellowstone-grpc-client + yellowstone-grpc-proto). New in-tree stack: `crates/evm-types/`, `crates/evm-types-macros/`, `crates/chain-adapter/src/jsonrpc/`, `crates/yellowstone-proto/`, `crates/solana-types/`. `bridge/` directory was proposed in flight (design 0025) and rescinded same session by user "kludge test" principle — ADR 0006 amended same session as original sign-off. **Architecture uniformly "wire protocols only" across both chains.** 61 test groups workspace-clean. **6 ADRs** (0006 amended). **26 design docs** (0024 + 0025 SUPERSEDED, 0026 added).

## TL;DR

Sprints 24+25 were a doctrinal+structural double-shipment with zero detector changes. User pushed back on the feature-flag-driven Reth ExEx plan ("если это наша разработка..."), conversation rapidly converged on extending ADR 0003 (no SaaS at runtime) into ADR 0006 (no vendor SDKs at compile time). All EVM-specific vendor crates ripped from main workspace and replaced by in-tree types + own JSON-RPC over WS client (Sprint 24). User then pushed further ("когда начинаются костыли..."), articulating the "kludge test" principle and rescinding the bridge concept itself (memory `feedback_kludge_test.md`); ADR 0006 amended same session, design 0025 (exex-bridge) SUPERSEDED before any code was written. Sprint 25 repurposed as Solana stack divestment — symmetric counterpart to Sprint 24's EVM divestment, applied to `solana-sdk` + `yellowstone-grpc-client` + `yellowstone-grpc-proto`. Architecture is now uniformly "wire protocols only" across both chains.

## First action on session start

**Sprints 24+25 CLOSED. Sprint 26 OPEN with no doctrinal blockers.**

User picks a theme from the carry-forward backlog. There is NO architect spec drafted for Sprint 26 — the doctrine work is done.

```
1. Read CLAUDE.md, ROADMAP.md.
2. Read this file + memory/research_state.md.
3. Read docs/adr/0006-code-level-self-sovereignty.md AMENDMENT block at top (binding doctrine).
4. User picks one of the carry-forward themes (see "Sprint 26 candidate themes" below).
5. Dispatch onchain-analyst or architect (depending on theme) to draft a §11-style design doc.
6. Sign off, then dispatch dev-agent.
```

## What Sprints 24+25 shipped (single working day)

### Doctrine
- **ADR 0006 accepted then AMENDED same session 2026-04-27**: extends ADR 0003 runtime self-sovereignty into compile-time / Cargo dependencies. Vendor-curated SDK crates banned everywhere in the repository (not just service crates — bridge escape hatch closed). Allowed = language-level Rust libs + generic implementations of public specifications. Reference reading of vendor source allowed (license-permitting).
- **Memory `feedback_kludge_test.md`** added: standard wire-protocol integration is OK; custom bridges/feature-flags/in-process linkage is a kludge that indicates a foundational architecture problem to be resolved by changing the architecture.
- **Memory `feedback_track_latest_rust.md`** added: workspace `rust-version = "1.95"` (current stable); track-latest policy, no MSRV-conservatism for internal monorepo.
- **Design 0025 (exex-bridge) SUPERSEDED** before any code was written. **Design 0026 (Solana divestment) accepted + implemented end-to-end.**

### Implementation
- **`crates/evm-types/` + `crates/evm-types-macros/`** (Sprint 24): in-tree EVM type stack (Address EIP-55, B256, U256/I256, ABI decoder, `event_signature!` proc-macro with compile-time keccak256). Replaces alloy.
- **`crates/chain-adapter/src/jsonrpc/`** (Sprint 24 #5b): in-tree JSON-RPC 2.0 over WebSocket via `tokio-tungstenite`. Replaces `alloy::rpc::client::RpcClient`.
- **`crates/yellowstone-proto/`** (Sprint 25 T25-1): vendored .proto from `rpcpool/yellowstone-grpc@v12.2.0+solana.3.1.13` + `tonic-prost-build = "0.14"` codegen. Replaces `yellowstone-grpc-client` + `yellowstone-grpc-proto`.
- **`crates/solana-types/`** (Sprint 25 T25-2 + T25-5 ext): in-tree Solana type stack (Pubkey/Signature/Hash/Slot/Epoch + Keypair/Instruction/AccountMeta/Transaction with hand-rolled compact-u16 short-vec wire format). Replaces solana-sdk.
- **All 5 service crates migrated**: chain-adapter / detectors / dex-adapter / token-registry / server. Tests preserved + augmented. Behaviour: identical.

### Inline fixups (caught by main session across both sprints)
1. T25-2 test bug (45-char base58 → 45 zero bytes per leading-zero convention, not 33).
2. T25-5 first-attempt agent rabbit-holed on `fewer-permission-prompts` skill; re-dispatched with anti-detour brief framing.
3. T25-6 cross-crate type coupling (RawAccount.owner consumed in pool_accounts struct literals; design 0026 §4 audit had labelled tasks as independent but they were coupled through public struct field types).
4. `mg_pubkey_to_sdk` test bridge cleanup (helper became unnecessary once both sides became `mg_solana_types::Pubkey`).
5. Sub-agent #5a clippy-scope narrowing (Sprint 24); brief framing tightened.

### Operational incident
- **Disk-pressure during T25-5 verification**: target/ filled system disk to 100% causing tool-output capture failures. User ran `cargo clean` twice across the sprint. Mid-flight switch from `cargo build --workspace --all-targets` to `cargo check --workspace --all-targets` (no linker = ~10× lighter on disk).

### Metrics (Sprints 24+25 combined)
- ≈61 test result groups, 0 failed across the workspace
- 13 detectors unchanged in count (D13 + D01 migrated off vendor SDKs; amplification from S23 preserved)
- 16 migrations unchanged (next V00017)
- **5 → 6 ADRs** (0006 added + amended)
- **23 → 26 design docs** (0024 + 0025 SUPERSEDED, 0023 + 0026 added)
- Workspace deps removed: 5 EVM (alloy + 4 reth-*) + 3 Solana (yellowstone-grpc-client + yellowstone-grpc-proto + solana-sdk) = **8 vendor SDK Cargo crates eliminated**
- Workspace deps added: 8 generic-spec implementations (tiny-keccak / primitive-types / proc-macro2 / syn / quote / tokio-tungstenite / ed25519-dalek / sha2)
- Net workspace dep delta: ~0 (vendor → generic-protocol/language-level swap)
- Rust MSRV: **1.88 → 1.95** (track-latest policy)
- Clippy `--workspace --all-targets -- -D warnings` clean
- ≈30 RA-stale rounds across both sprints (gotcha #3 ≈25× → ≈30×)
- Sub-agent over-reports: 2 (S24 #5a + S25 T25-5 first-attempt)
- Disk-full incidents: 1 (S25 T25-5 retry verification); resolved by user `cargo clean`
- Agent dispatches: 12 dev + 2 architect = 14 across both sprints

## Sprint 26 candidate themes (carry-forward backlog, no doctrinal blockers)

User picks one (or proposes a new one). All have outstanding spec or research tasks.

- **A. 3rd EVM detector** (bridge-drain / EVM wash trading port / Ethereum honeypot) — keeps EVM detector momentum
- **B. Token-2022 extensions** (D14-D17 sub-detectors: ConfidentialTransfer / NonTransferable / ScaledUiAmount / Pausable, ~400 LOC each) — high feature density
- **C. Pump.fun graduation enrichment** (~300 LOC ship-small)
- **D. Decimals exact-fetch** (closes 3 SPEC-NOTEs from S21: D11/D12/D13 fetch exact decimals from `tokens` table)
- **E. Observability hardening** (OTLP exporter wire-up + live integration test with testcontainers Postgres; both deferred from S19)
- **F. D13 mempool integration** (real-time pre-emption, S20 Decision 8)
- **G. D13 pool coverage extension** (Curve / Balancer / SushiSwap decoders, S20 Decision 2)
- **H. eth_unsubscribe on Receiver drop + mid-stream WS reconnect** (Sprint 17 TODOs in chain-adapter/ethereum/jsonrpc)
- **I. Cross-check test rename** (`*_topic0_matches_sol*` → drop "_sol", purely cosmetic; quick-win)
- **J. SPL layout decoders** in `mg-solana-types` (deferred per design 0026 §11.6 minimal-first; needed if more Solana detectors arrive)
- **K. Stage 2 FDR** (Barras 2010, corpus-blocked ≥30-day live data — only viable if corpus has matured since S22)

Recommended cadence when re-engaging: ask user which consumer (bot-trader, custody, MM, exchange) has the highest current ROI signal, then pick the theme that maps to that consumer's needs.

## Sub-agent briefing

```
Project: mg-onchain-analysis (Rust 2024 edition; rust-version = "1.95"; track-latest stable; Sprint 26 OPEN with no doctrinal blockers).

At session start, read:
  CLAUDE.md, ROADMAP.md, SESSION-KICKOFF.md,
  docs/adr/0001-0006 (esp. 0006 — code-level self-sovereignty doctrine, READ THE AMENDMENT BLOCK AT TOP),
  docs/designs/0001-0026 as relevant (0024 + 0025 SUPERSEDED — historical context only),
  research/sprint13-b-citations.md (if smart-money / FDR),
  CHANGELOG.md ## [Unreleased] (S9-S25).

Storage Postgres 16 only. 16 migrations shipped (V00001-V00016); next is V00017.
Self-sovereign infra (ADR 0003) — no Helius/Alchemy/Infura/Chainalysis/Scam-Sniffer/Flashbots/Nansen API in prod.
Self-sovereign code (ADR 0006, AMENDED) — no vendor SDK Cargo crates ANYWHERE in repository (bridge exception closed). Standard wire-protocol integration only.
STANDALONE SERVICE ONLY: NO writes to consumer repos.
13 detectors (D01-D13) + 1 background-task pipeline (smart-money). 11 Solana + 2 EVM. D04+D08+D05 amplified by smart-money labels (S23). D13 migrated off alloy in S24. D01 migrated off solana-sdk in S25.
Production binary `onchain-service` materialized S19. Boots clean: clap CLI + auto-migrate + signal handling + 30s drain. **No `onchain-reth` binary** (S24 wiped the feature-flag plan; S25 also rescinded the bridge plan).
Workspace deps: tokio, clap, toml, tokio-util, url, rust_decimal, statrs, async_trait, sqlx (with uuid feature), reqwest, tokio-tungstenite, primitive-types, tiny-keccak, syn 2, quote, proc-macro2, ed25519-dalek, sha2, prost-types, tonic, prost. **Zero vendor SDK Cargo crates.**
EVM stack in-tree: crates/evm-types/, crates/evm-types-macros/ (event_signature! proc-macro with compile-time keccak), crates/chain-adapter/src/jsonrpc/ (hand-rolled JSON-RPC 2.0 over tokio-tungstenite).
Solana stack in-tree: crates/yellowstone-proto/ (vendored .proto + tonic-build), crates/solana-types/ (Pubkey/Signature/Hash/Slot/Epoch + Keypair/Instruction/AccountMeta/Transaction with hand-rolled Solana wire format).
Detectors override `supported_chains()`. D12+D13 = `&[Chain::Ethereum]`; rest default `&[Chain::Solana]`.
TokenPriceProvider in `crates/storage/src/price_provider.rs` (S21). WalletPnlCorpusStore in `crates/storage/src/wallet_pnl_corpus.rs` (S22). SmartMoneyLabeller in `crates/graph/src/smart_money.rs` (S22). SmartMoneyLookup in `crates/graph/src/smart_money_lookup.rs` (S23). SwapFetcher trait abstracts swaps reads.
`cargo clippy --workspace --all-targets -- -D warnings` is the bar. **EMPHASIS for sub-agent briefs: workspace scope, NOT `-p` scope. Past gotcha #14 / S24-#5a + S25 T25-5 first-attempt recurrences.**
RA stale ~30× confirmed — `touch + cargo check` to verify after trait/module/feature changes.
Sub-agent rabbit-hole risk: do NOT invoke `fewer-permission-prompts` or any skill if a tool denies. Just retry with different command form. Brief framing must include explicit anti-detour wording at top.
Disk-pressure risk for full `cargo build --workspace --all-targets` (heavy dev-deps testcontainers + bollard ~10 GB). Prefer `cargo check` for iterative verification; reserve `cargo build/test` for sprint-close gates.
Inline fixup over second-agent dispatch when sub-agent gaps are small (≤7 file edits).
```

## Gotchas (high-signal subset)

1. **`crates/common` FROZEN.**
2. **Sub-agent clippy scope narrow** — S24 #5a + S25 T25-5 recurrence. Brief MUST emphasise `--workspace` scope in CAPS.
3. **Rust-analyzer lag — ~30× CONFIRMED.** S24+S25 saw many phantom rounds. Anytime Cargo.toml / dep-tree edits touch a build-graph node → expect ~30s RA lag. `touch + cargo check` clears.
9. **Detector evidence keys prefixed by detector_id.** Smart-money labels prefixed `smart_money/`.
13. **Docker-gated tests `#[ignore]`.**
14. **Sub-agent over-reports clean state** — see #2.
17. **Suppression policy** unchanged.
21. **STANDALONE SERVICE ONLY.**
22. **`Utc::now()` ban** — except documented batch-task exception (S22 #93).
27. **Detector + ChainAdapter + TokenPriceProvider + WalletPnlCorpusStore + SwapFetcher + GraphLabelStore + SmartMoneyLookup — all dyn-compatible.**
28. **`observed_at` from block_time** — except batch tasks.
31. **Migrations:** V00001-V00016. Next is **V00017**.
42. **Suppression by detector**: D08 NOT; D10 DOES; D11 NOT; D12 NOT; D13 hard-suppress on settlement allowlist.
58. **(S24 RESOLVED) `alloy` removed.** EVM types now from `mg-evm-types`.
59. **(S24 WIPED + S25 RESCINDED) Reth ExEx**: feature-flag plan WIPED in S24; bridge plan RESCINDED in S25 (kludge-test). Reth runs as standard sibling node consumed via JSON-RPC + WS exclusively.
60. **(NEW S25) `solana-sdk` removed.** Solana types now from `mg-solana-types`. Pubkey 32-byte base58, Signature 64-byte base58.
61. **(NEW S25) `yellowstone-grpc-client/proto` removed.** Yellowstone client now generated by us via `tonic-prost-build` from vendored `crates/yellowstone-proto/proto/geyser.proto` (pinned to upstream tag `v12.2.0+solana.3.1.13`; re-vendor on protocol bumps).
62. **(NEW S25) tonic 0.14 split**: codegen in separate `tonic-prost-build` crate; `ProstCodec` runtime in `tonic-prost`. Both pinned to 0.14.
63. **(NEW S25) Solana wire format** is `mg_solana_types::wire::{encode_compact_u16,decode_compact_u16}` + `Transaction::serialize()`. Compact-u16 short-vec is the protocol-level length-prefix encoding (1-3 bytes, MSB-continuation).
65. **`MultiChainCoordinator`** — multi-chain wrapper.
67. **`Detector::supported_chains()` override** — D12+D13 = Ethereum; rest = Solana.
77. **`crates/server/src/init/`** is production wiring entry.
80. **Auto-migrate is default; `--no-migrate` opt-out.**
81. **Graceful shutdown 30s drain** — smart-money JoinHandle joined to drain set.
82. **D13 SettlementAllowlist HARD suppression.**
86. **(S21 RESOLVED) Phase 5 USD enrichment for D11+D12+D13 closed via TokenPriceProvider.**
87. **(S21 OPEN) Decimals defaults**: D11=9 / D12=18 / D13=propagation. Sprint 26 carry-forward.
89. **(S22) Smart-money labelling MVP** = first non-Detector pipeline.
90. **(S22) `LabelType::SmartMoney`** already exists.
91. **(S22) V00016 `wallet_pnl_corpus`** materialized + NOT partitioned.
92. **(S22) Background-task spawn pattern.**
93. **(S22) Documented `Utc::now()` exception** for batch-task wall-clock window_end.
94. **(S22) Heuristic annotation** `smart_money/heuristic_not_fdr_controlled = true`.
95. **(S22 OPEN) Stage 2 FDR.**
96. **(S23) `SmartMoneyLookup` trait** in `crates/graph/src/smart_money_lookup.rs`.
97. **(S23) D04 P&D smart-money amplification**: Tier1 → +0.12; Tier2 ≥2 wallets → +0.07; Tier3 → 0.00. Cap 0.95. Pre-pump window 60-min (Fantazzini 2023).
98. **(S23) D08 Sybil smart-money amplification**: Tier1 → +0.10; Tier2 ≥2 → +0.05.
99. **(S23) D05 wash trading NEUTRAL metadata**: `delta=0.00` always.
100. **(S23) Builder pattern + `Option<Arc<dyn SmartMoneyLookup>>`** preserves backwards compat.
101. **(S23) Standardized 5-key evidence schema** for amplifying detectors.
102. **(S24) ADR 0006 binding** — vendor SDK crates banned in service crates; AMENDMENT 2026-04-27 closes the bridge escape hatch (no bridges anywhere without a new ADR).
103. **(S24) `crates/evm-types/`** is the EVM type/decoder home.
104. **(S24) `crates/evm-types-macros/`** owns the `event_signature!` proc-macro with compile-time keccak256.
105. **(S24) `crates/chain-adapter/src/jsonrpc/`** owns the in-tree JSON-RPC 2.0 over WebSocket client.
106. **(S24) Rust MSRV = 1.95** (current stable). Track-latest policy per memory `feedback_track_latest_rust.md`.
107. **(S24) Reference reading of vendor source allowed** — license-permitting, MIT/Apache OK to derive with `// reference: <crate>::<symbol> (<license>)` attribution. AGPL conceptual-only.
108. **(NEW S25) `crates/yellowstone-proto/`** owns the vendored Geyser proto + tonic-build-generated client. Pinned to tag `v12.2.0+solana.3.1.13`. Re-vendor on Yellowstone protocol bumps.
109. **(NEW S25) `crates/solana-types/`** owns Pubkey/Signature/Hash/Slot/Epoch + Keypair/Instruction/AccountMeta/Transaction. SPL layout decoders deferred to Sprint 26 per design 0026 §11.6.
110. **(NEW S25) Memory `feedback_kludge_test.md`** is binding doctrine: standard wire-protocol integration is OK; bridges/feature-flags/in-process linkage is a kludge to be resolved by changing architecture, not by sanctioning the kludge.
111. **(NEW S25) Sub-agent rabbit-hole pattern**: when dev-agent hits any tool-denial, they sometimes pivot to invoking `fewer-permission-prompts` skill or trying to edit `.claude/settings.json` instead of doing the work. Brief framing must include explicit anti-detour wording at the top: "tools work, do NOT invoke skills, do NOT edit settings.json, just do the migration." Reference Sprint 25 T25-5 first-attempt for the recurring failure mode.
112. **(NEW S25) Disk-pressure risk** for `cargo build --workspace --all-targets`: testcontainers + bollard add ~10 GB to `target/`, can fill the system disk. Prefer `cargo check` during iterative verification; reserve full build/test for sprint-close gates.

## Production posture as of Sprints 24+25 close

- Single binary `onchain-service` boots cleanly
- 13 detectors registered (12 streaming + D10 hook-only) + 1 background-task pipeline (smart-money 6h batch)
- 11 Solana + 2 EVM detectors
- D04 P&D + D08 Sybil + D05 wash trading consume smart-money labels (D04+D08 UP amplification, D05 NEUTRAL metadata)
- Ethereum path: in-tree `JsonRpcClient` over `tokio-tungstenite` + in-tree `event_signature!`-generated event decoders + `mg-evm-types` primitives
- Solana path: in-tree `tonic`-generated Yellowstone gRPC client over our `crates/yellowstone-proto/` + in-tree `mg-solana-types` primitives + hand-rolled Solana wire format
- 16 migrations auto-apply unless `--no-migrate`
- Default config: Solana on, Ethereum off, smart-money enabled
- SIGTERM/SIGINT triggers 30s drain → exit 0
- **Zero vendor SDK Cargo crates anywhere in the workspace** (verified `grep -rn "use alloy\|alloy::\|use solana_sdk\|solana_sdk::\|use yellowstone_grpc\|yellowstone_grpc_\|reth_" crates/ --include="*.rs"` returns only `///`/`//!` doc-comments and `// reference:` attribution)

## When Sprint 26 closes

Rewrite this file as Sprint 27 kickoff. Sprint 27 candidate themes will reflect what Sprint 26 decided to ship vs defer; the carry-forward backlog above is the starting menu.
