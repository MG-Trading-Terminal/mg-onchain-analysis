# Session Kickoff — Sprint 23 CLOSED (Smart-money consumer integration) / Sprint 24 OPEN

**Read this first after session restart.** Sprint 23 closed 2026-04-25 single-session. **Smart-money consumer integration shipped** — S22 labels now flow into D04 P&D (UP +0.12/+0.07) + D08 Sybil (UP +0.10/+0.05) + D05 wash trading (NEUTRAL metadata-only). New `SmartMoneyLookup` trait + builder pattern Option backwards compat. Cross-detector enrichment loop closed. **1293 tests passing** (+34). **13 detectors** (D04+D05+D08 enhanced; count unchanged). **16 migrations**. **5 ADRs**. **23 design docs**. **Gotcha #3 (RA stale) now confirmed 21×**.

## TL;DR

Sprint 23 converted Sprint 22 labelling investment into measurable detector quality improvements. 3 detectors amplify based on smart-money labels: D04 catches mastermind buyers pre-pump (Perseus-anchored), D08 catches informed-coordinator Sybil clusters, D05 emits metadata for downstream consumer policy (genuine ambiguity, no confidence change). Builder pattern preserves backwards compat — existing `::new(thresholds)` callsites unchanged.

## First action on session start

**Sprint 23 CLOSED. Sprint 24 OPEN.** User directive not yet locked.

**Option A (3rd EVM detector):**
- Bridge-drain / EVM wash trading port (D05 → Ethereum) / Ethereum-honeypot variants
- Pattern S12/S14/S18/S20: analyst → user sign-off → developer
- ~1 sprint

**Option B (Reth ExEx feature flag — OLDEST deferral, 7 sprints carry S17→S23):**
- `cfg(feature = "exex")` + `ExExRpcClient` alternate impl
- ~1 sprint, infrastructure-only

**Option C (Token-2022 extensions):**
- 4 sub-detectors × ~400 LOC: ConfidentialTransfer / NonTransferable / ScaledUiAmount / Pausable → D14-D17

**Option D (Pump.fun graduation enrichment):**
- ~300 LOC ship-small

**Option E (Decimals exact-fetch — closes 3 S21 SPEC-NOTEs):**
- D11/D12/D13 fetch exact decimals from `tokens` table

**Option F (Observability hardening):**
- OTLP exporter + live integration test (S19 + S20 deferred)

**Option G (D13 pool coverage extension):**
- Curve / Balancer / SushiSwap decoders

**Option H (D13 mempool integration):**
- Real-time pre-emption

**Option I (Stage 2 FDR — Barras 2010):**
- Only viable when ≥30-day live corpus available; data-blocked

Recommended: **Option B (Reth ExEx)** is the OLDEST infrastructure deferral (7 sprints carry); each sprint without it adds drift. **Option A (3rd EVM detector)** keeps EVM detector momentum. **Option C (Token-2022)** ships 4 detectors at once for high feature density. Strategic balance shifts toward **infrastructure debt closure (B)** unless user wants more feature density.

```
1. Read CLAUDE.md, ROADMAP.md.
2. Read this file + memory/research_state.md + designs 0022 + 0023 (smart-money pipeline + consumer).
3. Read CHANGELOG.md ## [Unreleased] Sprint 12-23 entries.
4. Read SPRINTS.md Sprint 23 section.
5. Pick A/B/C/D/E/F/G/H/I or propose different.
```

## What Sprint 23 shipped (single session)

### S23-1 Spec (onchain-analyst)
- `docs/designs/0023-smart-money-consumer-integration.md`
- 8 user-approved decisions: SmartMoneyLookup trait location / D04 deltas / D04 60-min window / D08 UPWARD / D05 NEUTRAL / per-eval batch load / 5-key evidence schema / builder pattern Option backwards compat

### S23-2 SmartMoneyLookup trait (NEW `crates/graph/src/smart_money_lookup.rs`)
- async trait + `GraphSmartMoneyLookup` impl + `MockSmartMoneyLookup` (test-utils gated)
- Reads from `GraphLabelStore::addresses_with_label(LabelType::SmartMoney)` + parses tier from evidence JSON + filters expired
- 7 unit tests

### S23-2 Shared amplifier helper (NEW `crates/detectors/src/smart_money_amplifier.rs`)
- `TierCounts` + `intersect_tier_counts` — avoids duplication across D04/D08/D05
- 6 unit tests

### S23-2 D04 amplification
- Builder `with_smart_money()` + `fetch_pre_pump_buyers` helper
- Step 5: Tier1 → +0.12 (capped per-event), Tier2 ≥2 wallets → +0.07, Tier3 → 0.00. 0.95 cap respected
- 5-key evidence prefix `pump_dump_v1/`
- 9 unit tests

### S23-2 D08 amplification
- Builder + Step 7 cluster intersection (Tier1=+0.10 / Tier2 ≥2=+0.05)
- Coexists with existing GraphLabelStore (S11)
- 6 unit tests

### S23-2 D05 NEUTRAL
- Builder + metadata-only emission (`delta=0.00` explicitly)
- Confidence UNCHANGED regardless of smart-money presence
- 3 unit tests assert invariance

### S23-2 Production wiring
- `init::detectors.rs` constructs `GraphSmartMoneyLookup` via `min_label_confidence` config
- Injects via `with_smart_money(...)` builder for D04/D08/D05

### Inline developer fixes (3, no main-session intervention)
1. D04 `ctx.window.block_start` (BlockRef) → `ctx.window.start` (DateTime<Utc>)
2. D08 `ctx.cluster_store` → `self.cluster_store` field
3. Missing `min_label_confidence` in 4 places — added all

### Cross-detector enrichment trail closed
S22 smart-money labels → S23 SmartMoneyLookup trait → D04+D08 UP amplification + D05 NEUTRAL metadata. Labels actively improve signal-to-noise ratio.

### Metrics
- **1293 tests passing, 0 failed, 29 ignored** (+34 from 1259: 7 trait + 6 amplifier + 9 D04 + 6 D08 + 3 D05 + 3 fixture)
- 13 detectors unchanged in count; D04+D05+D08 enhanced
- 16 migrations unchanged
- **22 → 23 design docs** (0023 added)
- 5 ADRs unchanged
- Workspace deps unchanged
- Clippy `--workspace --all-targets -- -D warnings` clean
- 1 RA-stale round, 6 phantom errors → gotcha #3 counter **21×**
- Agent dispatches: 2 (1 spec + 1 impl; **NO timeout**)

## Sprint 24 candidate tracks

### A — 3rd EVM detector
1-3. Onchain-analyst spec → user sign-off → developer impl

### B — Reth ExEx feature flag (oldest deferral, 7 sprints)
4. `cfg(feature = "exex")` + `ExExRpcClient`

### C — Token-2022 extensions
5-8. ConfidentialTransfer / NonTransferable / ScaledUiAmount / Pausable

### D — Pump.fun graduation enrichment
9. T1-2 ship-small

### E — Decimals exact-fetch (3 SPEC-NOTEs from S21)
10. D11/D12/D13 fetch exact decimals from `tokens` table

### F — Observability hardening
11. OTLP exporter (env-gated)
12. Live integration test (testcontainers + mock adapter)

### G — D13 pool coverage extension
13. Curve / Balancer / SushiSwap decoders

### H — D13 mempool integration
14. Real-time pre-emption

### I — Stage 2 FDR (data-blocked)
15. Barras 2010 FDR — only viable when ≥30-day live corpus

## Sprint 24 exit criterion

`cargo clippy --workspace --all-targets -- -D warnings` clean. At least ONE of A/B/C/D/E/F/G/H landed.

## Sub-agent briefing

```
Project: mg-onchain-analysis (Rust 2024, Sprint 24 OPEN after Sprint 23 smart-money consumer integration shipped).
At session start, read:
  CLAUDE.md, ROADMAP.md, SESSION-KICKOFF.md,
  docs/adr/0001-0005,
  docs/designs/0001-0023 as relevant,
  research/sprint13-b-citations.md (if smart-money / FDR),
  CHANGELOG.md ## [Unreleased] (S9-S23).
Storage Postgres 16 only. 16 migrations shipped (V00001-V00016); next is V00017.
Self-sovereign infra (ADR 0003+0004) — no Helius/Alchemy/Infura/Chainalysis API/Scam-Sniffer API/Flashbots-Relay API/Nansen API in prod.
STANDALONE SERVICE ONLY: NO writes to consumer repos.
13 detectors (D01-D13) + 1 background-task pipeline (smart-money). 11 Solana + 2 EVM. D04+D08+D05 amplified by smart-money labels (S23).
Production binary `onchain-service` materialized S19. Boots clean: clap CLI + auto-migrate + signal handling + 30s drain.
Workspace deps: alloy = "1.0", clap, toml, tokio-util, url, rust_decimal, statrs, async_trait + sqlx uuid feature.
Detectors override `supported_chains()`. D12+D13 = `&[Chain::Ethereum]`; rest default `&[Chain::Solana]`.
TokenPriceProvider in `crates/storage/src/price_provider.rs` (S21). WalletPnlCorpusStore in `crates/storage/src/wallet_pnl_corpus.rs` (S22). SmartMoneyLabeller in `crates/graph/src/smart_money.rs` (S22). SmartMoneyLookup in `crates/graph/src/smart_money_lookup.rs` (S23). SwapFetcher trait abstracts swaps reads.
Reth ExEx is Sprint 24+ feature flag (7 sprints deferred — oldest carry).
First non-Detector pipeline pattern shipped S22; first cross-detector enrichment trail S23.
`cargo clippy --workspace --all-targets -- -D warnings` is the bar.
RA stale 21× confirmed — `touch + cargo check` to verify after trait/module/feature changes.
S18+S19+S20+S21+S22+S23 lesson: tighter agent briefs with explicit time-box + clear deferral list = no timeout. Builder pattern Option<...> backwards compat is reusable for cross-detector wirings.
```

## Gotchas (high-signal subset)

1. **`crates/common` FROZEN.**
2. **Sub-agent clippy scope narrow.**
3. **Rust-analyzer lag — 21× CONFIRMED.** S23 saw 6 phantom (BlockRef/TimeDelta sub mismatch on d04 — agent had ALREADY fixed via field accessor change). Pattern: type-mismatch fixes + new field additions = textbook trigger.
9. **Detector evidence keys prefixed by detector_id.** Smart-money labels prefixed `smart_money/`.
13. **Docker-gated tests `#[ignore]`.**
14. **Sub-agent over-reports clean state.**
17. **Suppression policy** unchanged.
21. **STANDALONE SERVICE ONLY.**
22. **`Utc::now()` ban** — except documented batch-task exception (S22 #93).
27. **Detector + ChainAdapter + TokenPriceProvider + WalletPnlCorpusStore + SwapFetcher + GraphLabelStore + SmartMoneyLookup — all dyn-compatible via `#[async_trait]` or ErasedX wrapper.**
28. **`observed_at` from block_time** — except batch tasks.
31. **Migrations:** V00001-V00016. Next is **V00017**.
42. **Suppression by detector**: D08 NOT; D10 DOES; D11 NOT; D12 NOT; D13 hard-suppress on settlement allowlist.
49. **(RESOLVED S19) Server-binary materialized.**
58. **`alloy = "1.0"` is the EVM workspace dep.**
59. **(S15→S23 deferred 7 sprints) Reth ExEx is Sprint 24+ feature flag.** Oldest accumulated deferral.
65. **`MultiChainCoordinator`** — multi-chain wrapper.
67. **`Detector::supported_chains()` override** — D12+D13 = Ethereum; rest = Solana.
77. **`crates/server/src/init/`** is production wiring entry.
80. **Auto-migrate is default; `--no-migrate` opt-out.**
81. **Graceful shutdown 30s drain** — smart-money JoinHandle joined to drain set.
82. **D13 SettlementAllowlist HARD suppression.**
86. **(S21 RESOLVED) Phase 5 USD enrichment for D11+D12+D13 closed via TokenPriceProvider.**
87. **(S21 OPEN) Decimals defaults**: D11=9 / D12=18 / D13=propagation. Sprint 24+ exact-fetch.
89. **(S22) Smart-money labelling MVP** = first non-Detector pipeline.
90. **(S22) `LabelType::SmartMoney`** already exists — reused without schema change.
91. **(S22) V00016 `wallet_pnl_corpus`** materialized + NOT partitioned.
92. **(S22) Background-task spawn pattern** — periodic interval ticker + cancellation token + JoinHandle.
93. **(S22) Documented `Utc::now()` exception** for batch-task wall-clock window_end.
94. **(S22) Heuristic annotation** `smart_money/heuristic_not_fdr_controlled = true` until Stage 2 unblocks.
95. **(S22 OPEN) Stage 2 FDR** — config flag exists, implementation when corpus matures.
96. **(NEW S23) `SmartMoneyLookup` trait** in `crates/graph/src/smart_money_lookup.rs`. `GraphSmartMoneyLookup` reads from `GraphLabelStore::addresses_with_label(LabelType::SmartMoney)`. Per-evaluation batch load (no TTL cache — label table small + changes only every 6h).
97. **(NEW S23) D04 P&D smart-money amplification**: Tier1 → +0.12 (capped per-event, not per-wallet); Tier2 ≥2 wallets → +0.07; Tier3 → 0.00. Cap 0.95. Pre-pump window 60-min (Fantazzini 2023, configurable via `d04_pre_pump_window_minutes`).
98. **(NEW S23) D08 Sybil smart-money amplification**: Tier1 → +0.10; Tier2 ≥2 → +0.05. Coexists with existing GraphLabelStore funding-source-label injection (S11).
99. **(NEW S23) D05 wash trading NEUTRAL metadata**: `delta=0.00` always. Confidence unchanged. Documents deliberate no-change for downstream consumer policy. Genuine ambiguity (legit MM vs skilled wash) unresolvable without secondary signal.
100. **(NEW S23) Builder pattern + `Option<Arc<dyn SmartMoneyLookup>>`** preserves backwards compat — existing `::new(thresholds)` callsites compile unchanged. `with_smart_money()` is opt-in injection. **Reusable template for future cross-detector wirings.**
101. **(NEW S23) Standardized 5-key evidence schema** for amplifying detectors: `{detector_id}/smart_money_present`, `tier1_count`, `tier2_count`, `tier3_count`, `amplification_delta`. Mirror this when adding more amplifiers.

## Production posture as of Sprint 23

- Single binary `onchain-service` boots cleanly
- 13 detectors registered (12 streaming + D10 hook-only) + 1 background-task pipeline (smart-money 6h batch)
- 11 Solana + 2 EVM detectors
- D04+D08 UP-amplified by smart-money labels; D05 emits NEUTRAL metadata
- 16 migrations auto-apply unless `--no-migrate`
- Default config: Solana on, Ethereum off, smart-money enabled (heuristic-tier labels emitted + consumed by D04/D08/D05)
- SIGTERM/SIGINT triggers 30s drain → exit 0

## When Sprint 24 closes

Rewrite this file as Sprint 25 kickoff.
