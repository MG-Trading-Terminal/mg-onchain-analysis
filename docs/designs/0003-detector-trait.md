# Design: `Detector` Trait + Config Loader

**Date:** 2026-04-21
**Status:** Draft
**Author:** architect agent
**Implements:**
- ADR 0001 §D4 — `AnomalyEvent { confidence, severity, evidence }`
- ADR 0001 §D5 — 6 MVP detectors (on-demand invocation, per-detector config)
- ADR 0001 §D8 — three delivery modes (in-process crate, REST, WS streaming)
- `CLAUDE.md` §Detector Rules — deterministic, cites source, thresholds from config, emits confidence

**Related designs:** `docs/designs/0001-crates-common-types.md` (frozen), `docs/designs/0002-storage-schemas-v1.md`

---

## Context

`crates/common` defines the output contract (`AnomalyEvent`, `Evidence`, `Confidence`, `Severity`) and is frozen for Phase 2. `crates/storage` exposes `PgStore` with typed query methods. `crates/token-registry` exposes `TokenRegistry::enrich()` which returns `TokenMeta` and also drives a `holder_classifications` sidecar table populated by `HolderClassifier`. The six MVP detectors listed in ADR 0001 §D5 are ready to be implemented once the shared `Detector` trait abstraction is in place.

This design closes two gaps:

1. **No shared invocation contract.** Without a `Detector` trait, each detector will invent its own async function signature, making the scheduler in `crates/server` impossible to write generically and unit testing without mocking difficult.
2. **No threshold discipline.** CLAUDE.md §Detector Rules mandates that every threshold is externalised to `config/detectors.toml` with a cited rationale. Without a typed config loader enforcing this, thresholds will drift into code as magic constants.

This design does NOT modify `crates/common`. Where gaps in the frozen types emerge, workarounds are called out explicitly in §Common Type Gaps at the bottom of this document.

ADR 0002 established Postgres-only storage. All query references below are PostgreSQL dialect. ClickHouse-style constructs (`FINAL`, `countIf`, `LowCardinality`) do not appear here.

---

## Module Layout

```
crates/detectors/src/
  lib.rs              -- Re-exports: Detector, DetectorContext, DetectorError, DetectorConfig
  trait.rs            -- The Detector trait definition
  context.rs          -- DetectorContext: what a detector reads
  error.rs            -- DetectorError (thiserror, non_exhaustive)
  config.rs           -- DetectorConfig, per-detector threshold structs, TOML loader
  mock.rs             -- MockPgRunner, MockRegistry for unit test injection (cfg(test))
  d01_honeypot.rs     -- (developer implements — not in this design)
  d02_rug_pull.rs     -- (developer implements — not in this design)
  d03_concentration.rs
  d04_pump_dump.rs
  d05_wash_trading.rs
  d06_mint_burn.rs
```

`lib.rs` re-exports all public items. Individual detector files are added as they ship (Sprint 2: d01; Sprint 3: d02, d03; Sprint 4: d04, d05, d06).

---

## Rust Sketch

### `trait.rs` — The Detector Trait

```rust
//! The `Detector` trait — the invocation contract for every on-chain anomaly detector.
//!
//! # Rust 2024 native async fn
//!
//! The trait uses native async fn syntax (Rust 2024 edition, stabilised in 1.75).
//! No `async_trait` proc macro is needed. Object-safety is intentionally NOT
//! required: detectors are generic over `DetectorContext` at call sites, not
//! dispatched through `dyn Detector`. This trades runtime polymorphism for
//! zero-overhead specialisation and avoids boxing futures.
//!
//! If a heterogeneous collection of detectors is needed (e.g. a scheduler
//! iterating `Vec<dyn Detector>`), use a concrete enum dispatcher or a
//! `Box<dyn for<'ctx> Fn(&'ctx DetectorContext<'ctx>) -> BoxFuture<...>>` wrapper
//! at the call site. That decision is left to `crates/server`.
//!
//! # Determinism
//!
//! Implementing types MUST satisfy:
//! - No wall-clock reads (`Utc::now()`, `std::time::Instant::now()`). Time comes
//!   from `DetectorContext::window` which carries block-time-sourced timestamps.
//! - No randomness unless an explicit `RngSeed` is injected (detectors never need RNG).
//! - No `HashMap` in any path that contributes to output. Use `BTreeMap` (already
//!   enforced in `Evidence::metrics` by the frozen `common` types).
//! - Ordered iteration over all collections derived from DB results. DB results
//!   MUST be ORDER BY'd in the query; do not rely on Postgres result order without
//!   an explicit ORDER BY clause.

use crate::context::DetectorContext;
use crate::error::DetectorError;
use mg_onchain_common::anomaly::AnomalyEvent;

/// The shared invocation contract for every anomaly detector.
///
/// # Generic parameter `S`
///
/// `S` is the query/storage runner injected via `DetectorContext`. In production,
/// `S` is `PgStore` from `crates/storage`. In tests, `S` is `MockPgRunner` from
/// `crates/detectors::mock`. This makes detectors testable without a live database.
///
/// # Returns
///
/// `Ok(Vec<AnomalyEvent>)` — may be empty (no anomaly detected), one element
/// (most detectors), or multiple (e.g. D05 wash trading may flag multiple actors).
///
/// `Err(DetectorError)` — see `DetectorError` variants for retry semantics.
pub trait Detector {
    /// A stable machine-readable identifier for this detector.
    ///
    /// Convention: `snake_case`. Must match the prefix used in `Evidence::metrics`
    /// keys and the subsection name in `config/detectors.toml`.
    ///
    /// Examples: `"honeypot_sim"`, `"rug_pull_lp_drain"`, `"holder_concentration"`,
    /// `"pump_dump"`, `"wash_trading_h1"`, `"mint_burn_anomaly"`.
    const ID: &'static str;

    /// Evaluate this detector for the token described in `ctx`.
    ///
    /// # Determinism contract
    ///
    /// Given identical `ctx.window`, `ctx.token`, `ctx.chain`, and identical rows
    /// in the backing store, this function MUST return identical output on every
    /// call. Violations are bugs, not acceptable non-determinism.
    ///
    /// # Async contract
    ///
    /// May issue async queries against `ctx.store`. Must not spawn background tasks
    /// or retain state between calls. All intermediate state is local to the
    /// invocation stack.
    async fn evaluate<'ctx>(
        &self,
        ctx: &'ctx DetectorContext<'ctx>,
    ) -> Result<Vec<AnomalyEvent>, DetectorError>;
}
```

---

### `context.rs` — DetectorContext

```rust
//! [`DetectorContext`] — the read-only view a detector gets of the world.
//!
//! A detector receives exactly what it needs and nothing more:
//! - The target token identity.
//! - The observation time window (block-time sourced — NOT wall-clock).
//! - A borrowed reference to the Postgres query runner.
//! - A borrowed reference to the token registry (enriched metadata + classifier).
//! - A borrowed reference to its own threshold config slice.
//!
//! `DetectorContext` is borrowed, not owned. Detectors cannot mutate shared state.
//! The lifetime `'ctx` ties the context references to the caller's scope.

use chrono::{DateTime, Utc};

use mg_onchain_common::chain::{Address, BlockRef, Chain};
use mg_onchain_storage::pg::PgStore;
use mg_onchain_token_registry::TokenRegistry;

use crate::config::DetectorConfig;

/// The time window over which a detector evaluates a token.
///
/// Both timestamps are derived from block time, not wall-clock time.
/// This is the primary mechanism for determinism: given the same `window`,
/// the same block events are in-scope.
#[derive(Debug, Clone, Copy)]
pub struct DetectorWindow {
    /// Inclusive start of the observation window (block time).
    pub start: DateTime<Utc>,
    /// Exclusive end of the observation window (block time).
    pub end: DateTime<Utc>,
    /// Block range corresponding to the window, for evidence population.
    pub block_start: BlockRef,
    pub block_end: BlockRef,
}

/// The read-only context injected into every detector invocation.
///
/// # Borrowing and lifetime
///
/// All fields are borrowed references tied to lifetime `'ctx`. The caller
/// (scheduler or on-demand API handler) owns the storage, registry, and config;
/// this struct borrows them for the duration of one `evaluate()` call.
///
/// Detectors MUST NOT retain a reference to `DetectorContext` beyond `evaluate()`.
pub struct DetectorContext<'ctx> {
    /// The token being evaluated.
    pub token: &'ctx Address,
    /// Which chain the token lives on.
    pub chain: Chain,
    /// The observation window (block-time sourced).
    pub window: DetectorWindow,
    /// Postgres query runner. In tests, a `MockPgRunner` satisfying the same
    /// interface can be substituted.
    // TODO(developer): if a `QueryRunner` trait abstraction is added to
    // `crates/storage`, change this field type to `&'ctx dyn QueryRunner`.
    // For Phase 2, `PgStore` is concrete — acceptable until tests require mocking
    // at the DB level (mock via canned SQL fixture files per detector).
    pub store: &'ctx PgStore,
    /// Token metadata + holder classifications from `crates/token-registry`.
    /// The registry caches enriched results; detectors call
    /// `ctx.registry.enrich(mint, chain)` and receive a full `TokenMeta`.
    /// The `holder_classifications` sidecar is accessed via
    /// `ctx.registry.classify_holder(address, chain)` — see §D3 note below.
    pub registry: &'ctx TokenRegistry,
    /// This detector's own threshold config. The loader guarantees the correct
    /// subsection is present before `evaluate()` is called; detectors may
    /// `.expect("threshold always present — guaranteed by loader")` on fields
    /// they declared as required.
    pub config: &'ctx DetectorConfig,
    /// The chain's null/zero address, for distinguishing mint/burn from transfers.
    /// Passed explicitly rather than hardcoded so EVM and Solana adapters differ
    /// without branching in detector logic.
    pub zero_address: &'ctx str,
}
```

**Note on D3 holder-classification join strategy:**

`DetectorContext` exposes `&TokenRegistry` rather than the `HolderClassifier` directly. The D3 concentration detector uses the sidecar via one of two access paths:

1. **SQL-level join (preferred for batch reads).** The Postgres `holder_classifications` table is populated by `HolderClassifier`. The D03 detector query can LEFT JOIN `holder_classifications` on `(chain, address)` to filter out `vesting_contract` and `dex_pool` holders before computing top-N percentages. This keeps classification logic in the query where it is visible, auditable, and avoids N+1 RPC calls.

2. **Registry call for individual addresses (fallback for enrichment).** For addresses not yet in `holder_classifications` (e.g. new holders since the last snapshot), the detector calls `ctx.registry.classify_holder(address, chain).await` which triggers the `HolderClassifier` ladder and writes back to the sidecar. This path has latency cost and should only be used for at-query-time enrichment of novel addresses.

The `TokenRegistry` type needs a `classify_holder` method exposed in its public API for path 2. The developer task for D03 must add this method to `crates/token-registry/src/lib.rs`. This is not a `crates/common` change — it is an additive method on `TokenRegistry`.

---

### `error.rs` — DetectorError

```rust
//! [`DetectorError`] — typed failures from detector evaluation.
//!
//! # Retry semantics
//!
//! Not all failures are equal. The caller (scheduler or on-demand handler)
//! uses the variant to decide whether to retry, disable, or quarantine:
//!
//! - [`DetectorError::TransientQuery`]: retry up to N times with backoff.
//!   Postgres connectivity blip, query timeout. Do NOT propagate to the consumer
//!   as an alert; log and retry.
//!
//! - [`DetectorError::PermanentQuery`]: disable the detector for this token for
//!   TTL seconds; log with ERROR. Likely a schema mismatch or migration gap.
//!
//! - [`DetectorError::MissingThresholdConfig`]: programming error — the loader
//!   should have caught this. Log with WARN; skip detector.
//!
//! - [`DetectorError::InsufficientBaseline`]: not an error — the detector has
//!   observed insufficient historical data to compute the primary signal. The
//!   detector SHOULD return `Ok(vec![])` for pure absence, but may return
//!   `Err(InsufficientBaseline)` when it also wants to signal "no data available
//!   yet" to the caller for logging purposes. Detectors that have a fallback
//!   signal (e.g. D04's `burst_concentration_ratio`) SHOULD use the fallback
//!   and return `Ok(...)` instead of this error.
//!
//! - [`DetectorError::MissingDependencyData`]: token not yet enriched in registry,
//!   or required pool state not yet present in Postgres. Retry after enrichment.
//!
//! - [`DetectorError::DeterminismViolation`]: should never happen in production.
//!   Triggered if a detector detects that its own output would be non-deterministic
//!   (e.g. an unordered result set was received and could not be sorted). Treated
//!   as a panic-adjacent condition — log at CRITICAL and disable the detector.

use thiserror::Error;

/// All failure modes from a detector invocation.
///
/// `#[non_exhaustive]` allows new variants to be added in minor releases without
/// breaking callers that match on `DetectorError`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DetectorError {
    /// Postgres query failed transiently. Retry-worthy.
    #[error("transient query failure in detector '{detector_id}': {source}")]
    TransientQuery {
        detector_id: &'static str,
        #[source]
        source: sqlx::Error,
    },

    /// Postgres query failed permanently (schema mismatch, data corruption).
    /// Detector is disabled until manually re-enabled.
    #[error("permanent query failure in detector '{detector_id}': {reason}")]
    PermanentQuery {
        detector_id: &'static str,
        reason: String,
    },

    /// A required threshold key was absent from the loaded config.
    /// Programming error — the loader should catch this before evaluate() is called.
    #[error("missing threshold config key '{key}' for detector '{detector_id}'")]
    MissingThresholdConfig {
        detector_id: &'static str,
        key: &'static str,
    },

    /// Insufficient historical baseline data to compute the primary statistic.
    ///
    /// Detectors with a fallback signal SHOULD NOT return this error — they should
    /// use the fallback and return `Ok(...)`. This variant is for detectors that
    /// have NO meaningful fallback and want to signal "skip this token for now".
    ///
    /// The `fallback_used` field documents whether a secondary signal was attempted.
    #[error("insufficient baseline for detector '{detector_id}' on token '{token}': {reason}")]
    InsufficientBaseline {
        detector_id: &'static str,
        token: String,
        reason: String,
        /// True if a fallback signal was attempted but also failed.
        fallback_used: bool,
    },

    /// Required dependency data (enriched token meta, pool state) not available yet.
    /// Retry after enrichment completes.
    #[error("missing dependency data for detector '{detector_id}' on token '{token}': {reason}")]
    MissingDependencyData {
        detector_id: &'static str,
        token: String,
        reason: String,
    },

    /// Non-determinism invariant violated. Should never happen.
    /// Treated as a fatal detector bug — disable on first occurrence.
    #[error("determinism violation in detector '{detector_id}': {reason}")]
    DeterminismViolation {
        detector_id: &'static str,
        reason: String,
    },
}

impl DetectorError {
    /// Returns true if the caller should retry this operation.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            DetectorError::TransientQuery { .. } | DetectorError::MissingDependencyData { .. }
        )
    }
}
```

---

### `config.rs` — Threshold Config + Loader

#### Config shape decision: Structured (chosen)

Two candidate shapes were considered:

**Flat shape:**
```toml
[honeypot_sim]
sell_tax_threshold = 0.50
sell_tax_threshold_rationale = "No legitimate fee-on-transfer token above 50%. Torres et al. 2019."
simulate_paths = 3
simulate_paths_rationale = "3 probe sizes to catch max-sell honeypots. GoPlus fork-state method."
```

**Structured shape (chosen):**
```toml
[honeypot_sim.sell_tax_threshold]
value = 0.50
rationale = "No legitimate fee-on-transfer token above 50%. Torres et al. 2019."
refs = ["D01/honeypot_sim"]

[honeypot_sim.simulate_paths]
value = 3
rationale = "3 probe sizes to catch max-sell honeypots. GoPlus fork-state method."
refs = ["D01/honeypot_sim"]
```

**Rationale for choosing structured:** The structured shape makes the `refs` field a typed list that the config loader can validate against `REFERENCES.md` entry IDs. This is a stronger discipline than a freeform string — a future validation pass can confirm every `refs` entry exists in the REFERENCES table. The flat shape embeds rationale as unvalidated prose. The extra nesting depth (2 levels instead of 1) is a worthwhile trade-off for machine-verifiable citations.

The TOML nesting maps cleanly to Rust structs via `serde`. See the Rust sketch below.

```rust
//! Threshold configuration types and TOML loader for all detectors.
//!
//! Every threshold is wrapped in `Threshold<T>` which carries the value,
//! a human-readable rationale string, and citation references to REFERENCES.md.
//!
//! # Loading
//!
//! At startup, `DetectorConfig::load(path)` parses `config/detectors.toml` and
//! validates that all required threshold keys are present. Missing keys produce
//! a hard error at startup — detectors never silently fall back to defaults.
//! This is intentional: a missing config key means the system was deployed
//! without the operator consciously setting thresholds.
//!
//! # Adding a new detector
//!
//! 1. Add a new struct `XxxConfig` with `Threshold<T>` fields.
//! 2. Add it to `AllDetectorConfigs`.
//! 3. Add the TOML subsection to `config/detectors.toml` with rationale + refs.
//! 4. Document the threshold in REFERENCES.md.
//! No other changes are needed — the loader picks up the new struct automatically.

use serde::Deserialize;
use std::path::Path;

// ---------------------------------------------------------------------------
// Threshold wrapper
// ---------------------------------------------------------------------------

/// A typed threshold value with its cited rationale.
///
/// Every threshold in `config/detectors.toml` uses this shape:
/// ```toml
/// [detector_id.threshold_name]
/// value    = 0.65
/// rationale = "Chainalysis 2025: deployer removes >= 65% of pool liquidity..."
/// refs     = ["D02/rug_pull_lp_drain"]
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct Threshold<T> {
    /// The threshold value.
    pub value: T,
    /// Human-readable rationale explaining the chosen value and its source.
    /// Must reference a REFERENCES.md entry ID in `refs`.
    pub rationale: String,
    /// REFERENCES.md entry IDs that justify this threshold.
    /// Format: `"D<NN>/<slug>"` matching the REFERENCES.md Detector column.
    pub refs: Vec<String>,
}

// ---------------------------------------------------------------------------
// Per-detector config structs
// ---------------------------------------------------------------------------

/// Thresholds for D01 Honeypot (simulation) detector.
///
/// Source: research/02-detection-methodology.md §2 + Torres et al. 2019.
#[derive(Debug, Clone, Deserialize)]
pub struct HoneypotConfig {
    /// Sell tax above this fraction triggers the detector.
    /// Range: (0.0, 1.0]. Default 0.50 (50%).
    pub sell_tax_threshold: Threshold<f64>,
    /// Number of distinct probe amounts to simulate for buy+sell.
    /// Catches max-sell-amount honeypots that allow small sells but block large ones.
    pub simulate_paths: Threshold<u32>,
    /// Buy/sell ratio sentinel above which the detector fires (zero-sell honeypot).
    /// 999.0 is the sentinel returned by d01_honeypot.sql when sell_count = 0.
    pub buy_sell_ratio_sentinel: Threshold<f64>,
}

/// Thresholds for D02 Rug Pull / LP Drain detector.
///
/// Source: research/02-detection-methodology.md §1 + Chainalysis 2025.
#[derive(Debug, Clone, Deserialize)]
pub struct RugPullConfig {
    /// Fraction of LP supply removed in a single tx (or cumulatively per actor
    /// in the window) to trigger the event-based drain signal.
    pub lp_removal_threshold: Threshold<f64>,
    /// Minimum pool liquidity in USD. Below this, false positives dominate.
    pub min_pool_usd: Threshold<f64>,
    /// Minimum lifetime transaction count for the pool.
    pub min_prior_txs: Threshold<i64>,
    /// State-based companion signal (RAVE gap): if lp_burned_pct < this value
    /// AND lp_locked_pct < lp_lock_safe_floor, fire a latent-risk alert.
    /// Source: RAVE probe §4 Gap 1 — 100% unlocked LP is a leading indicator.
    pub lp_burn_safe_floor: Threshold<f64>,
    /// Companion to lp_burn_safe_floor: minimum locked percentage that is
    /// considered "adequately protected".
    pub lp_lock_safe_floor: Threshold<f64>,
    /// If LP provider count is <= this value, elevate latent-risk confidence.
    /// Source: RAVE probe §5 — single LP provider = single point of failure.
    pub lp_providers_threshold: Threshold<i64>,
}

/// Thresholds for D03 Holder Concentration detector.
///
/// Source: research/02-detection-methodology.md §10 + Brown 2023 + TM-RugPull 2026.
#[derive(Debug, Clone, Deserialize)]
pub struct ConcentrationConfig {
    /// Minimum top-10 holder percentage (excluding vesting/CEX/DEX pool addresses)
    /// to trigger elevated confidence.
    pub top10_pct_elevated: Threshold<f64>,
    /// Top-10 percentage threshold for high-risk confidence.
    pub top10_pct_high_risk: Threshold<f64>,
    /// 24h Gini coefficient delta above this value triggers the shift signal.
    pub gini_delta_24h: Threshold<f64>,
    /// 24h top-10 holder percentage delta above this value triggers the signal.
    pub top10_pct_delta_24h: Threshold<f64>,
    /// Maximum deployer balance as a fraction of circulating supply before
    /// firing a deployer-concentration alert.
    pub deployer_balance_max_pct: Threshold<f64>,
}

/// Thresholds for D04 Pump & Dump detector.
///
/// Source: research/02-detection-methodology.md §3 + Karbalaii 2025 + Bolz 2024.
#[derive(Debug, Clone, Deserialize)]
pub struct PumpDumpConfig {
    /// 1-hour price spike as a fraction of window-open price.
    pub price_spike_pct: Threshold<f64>,
    /// Ratio of 1h volume to 7-day daily median volume.
    pub volume_multiplier: Threshold<f64>,
    /// Minimum days of baseline history required to use the volume_multiplier
    /// check. Below this, the detector falls back to burst_concentration_ratio.
    /// Source: RAVE probe §4 Gap 2 — zero-baseline case.
    pub min_baseline_days: Threshold<u32>,
    /// Fallback signal when baseline is unavailable (WET gap / RAVE gap):
    /// volume_1h / volume_24h above this threshold fires the detector at
    /// reduced confidence. This handles the "dormant-then-activated" pattern
    /// where the rolling median is zero or near-zero.
    /// Source: RAVE probe §4 Gap 2 + WET probe §D4.
    pub burst_concentration_ratio_threshold: Threshold<f64>,
    /// Fraction of insider (deployer cluster) holdings sold within 24h of spike
    /// to confirm the dump phase.
    pub insider_sell_pct: Threshold<f64>,
}

/// Thresholds for D05 Wash Trading (Heuristic 1) detector.
///
/// Source: research/02-detection-methodology.md §4 + Chainalysis 2025.
#[derive(Debug, Clone, Deserialize)]
pub struct WashTradingConfig {
    /// Solana slot window for buy+sell round-trip. 25 slots ≈ 10 seconds.
    /// Note: empirical recalibration against Solana data is pending (see
    /// research/02-detection-methodology.md Cross-cutting B gap #2).
    pub block_window: Threshold<i64>,
    /// Maximum fractional volume difference between buy and sell legs.
    pub volume_diff_pct: Threshold<f64>,
    /// Minimum number of qualifying round-trips to fire the signal.
    pub min_repetitions: Threshold<i64>,
    /// Heuristic 2 (Phase 3): minimum funded addresses from one controller.
    /// Not used in Phase 2 on-demand mode; included for config completeness.
    pub min_funded_addresses: Threshold<i64>,
    /// Heuristic 2: maximum buy/sell imbalance fraction.
    pub buy_sell_imbalance_max: Threshold<f64>,
}

/// Thresholds for D06 Mint/Burn Anomaly detector.
///
/// Source: research/02-detection-methodology.md §9 + Xia et al. 2021 + Sun et al. 2024.
#[derive(Debug, Clone, Deserialize)]
pub struct MintBurnConfig {
    /// Supply change (as fraction of circulating supply) in a single event
    /// or 1h window that triggers the signal.
    pub supply_change_pct: Threshold<f64>,
}

// ---------------------------------------------------------------------------
// Top-level config container
// ---------------------------------------------------------------------------

/// All detector threshold configs, loaded from one TOML file.
///
/// Subsection names MUST match detector ID constants (`Detector::ID`).
#[derive(Debug, Clone, Deserialize)]
pub struct AllDetectorConfigs {
    pub honeypot_sim: HoneypotConfig,
    pub rug_pull_lp_drain:RugPullConfig,
    pub holder_concentration: ConcentrationConfig,
    pub pump_dump: PumpDumpConfig,
    pub wash_trading_h1: WashTradingConfig,
    pub mint_burn_anomaly: MintBurnConfig,
}

/// Thin wrapper used in `DetectorContext` — carries the full config for
/// runtime access. Phase 2 passes the whole `AllDetectorConfigs` to context;
/// each detector accesses only its own subsection.
pub type DetectorConfig = AllDetectorConfigs;

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

/// Load and validate `config/detectors.toml`.
///
/// # Errors
///
/// Returns `anyhow::Error` if:
/// - The file does not exist or cannot be read.
/// - The TOML fails to parse.
/// - Any required subsection or threshold key is missing.
///   (Serde's `Deserialize` derive handles missing-key errors automatically.)
///
/// # Usage
///
/// ```rust,no_run
/// use mg_onchain_detectors::config::load_detector_config;
///
/// let config = load_detector_config("config/detectors.toml")?;
/// println!("sell_tax_threshold = {}", config.honeypot_sim.sell_tax_threshold.value);
/// ```
pub fn load_detector_config(path: impl AsRef<Path>) -> anyhow::Result<AllDetectorConfigs> {
    let content = std::fs::read_to_string(path.as_ref())
        .with_context(|| format!("failed to read {}", path.as_ref().display()))?;
    let config: AllDetectorConfigs = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.as_ref().display()))?;
    Ok(config)
}

// TODO(developer): Add a validate() method that cross-checks refs[] entries
// against REFERENCES.md programmatically. For Phase 2, refs are validated
// manually during code review. Automated validation is a Phase 3 improvement.
```

---

### `mock.rs` — Test Injection Pattern

```rust
//! Test-only mock implementations for injection into `DetectorContext`.
//!
//! No mocking framework. Concrete types with canned return values.
//! Detectors are tested by constructing a `DetectorContext` with:
//!   - A `MockPgRunner` holding pre-populated SQL result rows.
//!   - A `MockTokenRegistry` holding a fixed `TokenMeta`.
//!   - A fixed `DetectorWindow` with block-time-sourced timestamps.
//!
//! This pattern requires no network, no database, and no external process.
//! The mock types live in this module, gated behind `#[cfg(test)]`.

// TODO(developer): As the query surface grows, consider a lightweight
// "query fixture" pattern: MockPgRunner reads from a JSON file in
// tests/fixtures/solana/<token>/d01_result.json and returns typed rows.
// This decouples fixture maintenance from test code and supports the
// CLAUDE.md §"labelled test fixture" requirement.

#[cfg(test)]
pub mod tests {
    use std::collections::BTreeMap;

    /// A canned Postgres row result for testing. Each detector test constructs
    /// the specific row shape it needs and passes it to the mock.
    ///
    /// Because sqlx's Row type is not constructible outside sqlx internals,
    /// detectors must have their DB-reading logic separated from their
    /// computation logic. The pattern:
    ///
    /// 1. `async fn fetch_rows(store: &PgStore, ctx: &DetectorContext) -> Result<Vec<MyRow>>`
    ///    — executes the SQL, deserializes into a plain struct.
    /// 2. `fn compute(rows: &[MyRow], config: &MyDetectorConfig) -> Vec<AnomalyEvent>`
    ///    — pure function, no async, no DB. This is what unit tests exercise.
    ///
    /// This split means `compute()` can be called from unit tests with canned
    /// `Vec<MyRow>` inputs without any DB mock at all. The `fetch_rows()` function
    /// is tested separately with an integration test against a real Postgres container.
    ///
    /// See `docs/designs/0003-detector-trait.md` §Testability for rationale.
    pub struct CannedRow {
        pub fields: BTreeMap<String, String>,
    }
}
```

---

## Structural Decisions

### 1. Native async fn vs `async_trait`

Rust 2024 (MSRV 1.75) stabilises native async fn in traits. `async_trait` is not needed and would box every future unnecessarily. Object-safety is deferred: if a `dyn Detector` collection is needed in the scheduler, the developer adds an `ErasedDetector` wrapper at that call site rather than compromising the trait's zero-cost async semantics.

### 2. Streaming mode: deferred to Phase 3

On-demand invocation (`detector.evaluate(ctx).await` for a single token over a window) is fully specified here. Streaming evaluation — continuously evaluating as new events land — is explicitly deferred to Phase 3 for the following reasons:

- The indexer (`crates/indexer`) does not yet exist as a compiled crate (P2-2 is still in progress). Designing the streaming contract before the indexer's event emission shape is final risks re-derivation.
- The four consumers all have an on-demand access pattern for Phase 2: `bot-trader-2-0` calls before opening a position; custody, exchange, and MM call via REST for token screening. Streaming is a Phase 3/4 optimisation for the market-maker real-time feed.
- A safe composition exists for Phase 3: the indexer scheduler calls `detector.evaluate(token_addr, window_from_last_checkpoint)` for every token seen in a new batch of events. This is "streaming via polling" — not a push subscription model — and requires no changes to the `Detector` trait.

The streaming design note for Phase 3: the indexer should drive a `DetectorScheduler` that, on each new event batch, identifies affected tokens (by scanning `token_in` / `token_out` / `token` fields in the batch) and calls `evaluate()` for each. Detectors do not own subscriptions. The scheduler owns the event loop; detectors are stateless evaluators called from the scheduler.

### 3. Generic `S` vs concrete `PgStore`

`DetectorContext` uses a concrete `&PgStore` rather than a `&dyn QueryRunner` trait object. Rationale: no `QueryRunner` abstraction exists in `crates/storage` yet, and introducing one is a `crates/common`-adjacent decision that risks scope creep. The testability concern (how to mock the DB in unit tests) is resolved by the `fetch_rows` / `compute` split described in `mock.rs`: the computation function is pure and needs no mock.

If `crates/storage` adds a `QueryRunner` trait in Phase 3, `DetectorContext.store` can be changed to `&dyn QueryRunner` at that point. The trait field is the only change required; all detector code calls methods on `ctx.store` and those call signatures remain the same.

### 4. Evidence convention: detector-prefixed metric keys

Per the frozen `common` type contract (`crates/common/src/anomaly.rs`), `Evidence::metrics` is `BTreeMap<String, Decimal>`. The existing doc comment establishes the `<detector_id>/` prefix convention. This design enforces it by documentation and by example in the per-detector instance table below. No runtime enforcement in `common` (frozen). Enforcement is at the detector implementation level: code review + the per-detector evidence schema table in this document.

Values are `Decimal`, never `f64`. This follows `CLAUDE.md` §"NEVER f64 for prices, amounts" — though `Evidence::metrics` typically holds computed ratios, not monetary amounts, using `Decimal` avoids precision surprises downstream when evidence is persisted to Postgres `NUMERIC` columns or serialized to JSON.

### 5. TOML config shape: structured over flat

See decision rationale in `config.rs` above. Short summary: structured `{ value, rationale, refs }` enables machine-readable citation validation in a future tooling pass. Flat `_rationale` suffix is harder to parse programmatically and cannot carry a typed `refs` list.

### 6. InsufficientBaseline as error vs Ok(empty)

Two conventions were considered:
- Return `Ok(vec![])` when the primary signal is undefined. Simple but loses observability.
- Return `Err(InsufficientBaseline)` when primary AND fallback both fail. Caller can log "no baseline for D04 on token X" distinctly from "no anomaly detected".

Decision: detectors that have a fallback signal return `Ok(...)` using the fallback. Detectors with no fallback AND insufficient data return `Err(InsufficientBaseline)`. This preserves observability without polluting the happy path. The scheduler logs `InsufficientBaseline` at DEBUG level — it is expected on newly-indexed tokens.

---

## Per-Detector Instance Metadata

| # | Detector ID | Primary query | State companion (RAVE/WET gap) | Evidence keys (prefix/key) | Notes |
|---|-------------|---------------|-------------------------------|---------------------------|-------|
| D01 | `honeypot_sim` | `docs/queries/d01_honeypot.sql` — buy/sell ratio per pool per window | Structural state check: `TokenMeta.freeze_authority`, `TokenMeta.transfer_fee_bps` read directly from `ctx.registry.enrich()` (no SQL needed for static signals) | `honeypot_sim/buy_sell_ratio`, `honeypot_sim/sell_tax_est`, `honeypot_sim/freeze_authority_active`, `honeypot_sim/transfer_fee_bps`, `honeypot_sim/simulate_paths_tested` | Simulation via RPC (`simulateTransaction`) is the primary signal for Solana; the SQL query provides supporting on-chain evidence. RPC call must respect the async timeout budget. |
| D02 | `rug_pull_lp_drain` | `docs/queries/d02_rug_pull_lp_drain.sql` — LP burn events above threshold in window | State companion: read `PoolRow.lp_total_supply`, `PoolRow.deployer_lp_amount`, `PoolRow.lifetime_tx_count` + `MarketInfo.lp_burned_pct`, `MarketInfo.lp_locked_pct` from `ctx.registry.enrich()` to compute latent-risk signal without a drain event present | `rug_pull_lp_drain/lp_removed_pct`, `rug_pull_lp_drain/cumulative_removed_pct`, `rug_pull_lp_drain/pool_usd`, `rug_pull_lp_drain/lp_burned_pct`, `rug_pull_lp_drain/lp_locked_pct`, `rug_pull_lp_drain/lp_provider_count`, `rug_pull_lp_drain/latent_risk` | When no drain event fires but latent state signal fires: emit `AnomalyEvent` with lower confidence (0.50–0.75 range vs 0.85–1.0 for active drain). Set `latent_risk = "1"` in evidence to distinguish. |
| D03 | `holder_concentration` | `docs/queries/d03_holder_concentration_shift.sql` — Gini + top-10 delta vs 24h prior snapshot | SQL query LEFT JOINs `holder_classifications` on `(chain, address)` to filter out `vesting_contract` and `dex_pool` holders before computing top-N concentration. For addresses absent from sidecar, call `ctx.registry.classify_holder(address, chain)` to populate. | `holder_concentration/gini_delta_24h`, `holder_concentration/top10_pct_now`, `holder_concentration/top10_pct_24h_ago`, `holder_concentration/top10_pct_delta`, `holder_concentration/total_holders`, `holder_concentration/vesting_excluded_count` | `vesting_excluded_count` key documents how many addresses were excluded as vesting contracts — required for post-hoc calibration of false positives (WET probe §D3). |
| D04 | `pump_dump` | `docs/queries/d04_pump_and_dump.sql` — 1h volume spike vs 7-day baseline (Query 1) + insider sell-off (Query 2) | Fallback signal (RAVE gap + WET gap): when `median_volume_usd = 0` (WHERE guard in Query 1 returns empty), compute `burst_concentration_ratio = volume_1h / volume_24h` from a simpler aggregate query. If ratio > `burst_concentration_ratio_threshold`, fire at lower confidence. If `min_baseline_days` not met, use burst ratio unconditionally. | `pump_dump/volume_ratio`, `pump_dump/price_spike_pct`, `pump_dump/volume_z_score`, `pump_dump/insider_sell_pct`, `pump_dump/burst_concentration_ratio`, `pump_dump/baseline_days_available`, `pump_dump/fallback_used` | `fallback_used = "1"` when burst ratio triggered; `baseline_days_available` is the count of days with non-zero volume in the 7-day window, enabling downstream calibration of when to trust the z-score vs the fallback. |
| D05 | `wash_trading_h1` | `docs/queries/d05_wash_trading_h1.sql` — buy+sell round-trips per sender per pool within block window | None (H1 is purely event-based) | `wash_trading_h1/round_trip_count`, `wash_trading_h1/max_volume_diff_pct`, `wash_trading_h1/block_window_slots`, `wash_trading_h1/pool`, `wash_trading_h1/actor` | Solana slot window calibration is pending (research/02-detection-methodology.md gap #2). Phase 2 ships with default 25-slot window and `block_window_slots` in evidence for retrospective calibration. |
| D06 | `mint_burn_anomaly` | `docs/queries/d06_mint_burn_anomaly.sql` — transfers from/to zero address above supply-change threshold | State companion: read `TokenMeta.mint_authority` from `ctx.registry.enrich()` — if mint authority is active on a "fixed-supply" token, fire at high confidence regardless of whether a mint event has been observed yet. | `mint_burn_anomaly/supply_change_pct`, `mint_burn_anomaly/is_mint`, `mint_burn_anomaly/mint_authority_active`, `mint_burn_anomaly/freeze_authority_active`, `mint_burn_anomaly/is_lp_activity`, `mint_burn_anomaly/is_scheduled_emission` | `is_lp_activity = "1"` when the transfer target is a known LP contract — used to suppress false positives on Raydium/Orca LP minting. |

### Established-protocol suppression pattern (P4-0, 2026-04-21)

State-based latent signals (structural precondition signals that fire before an observed event)
can generate false positives on established protocols. These tokens carry structural-risk markers
(unlocked LP, single LP provider, active mint authority) for legitimate operational reasons that
differ fundamentally from scam patterns.

**Rule:** Detectors that have state-based latent signals MUST call
`crates::detectors::token_status::is_established_protocol(meta)` before emitting those signals.
If the predicate returns `true`, the latent signal MUST be suppressed and a low-confidence
(`confidence = 0.10`, `Severity::Info`) audit event MUST be emitted in its place.

**Asymmetric contract (critical):**
- State-based / latent / structural-precondition signals: **SUPPRESS** when `is_established_protocol` is true.
- Event-based signals (observed drain events, simulation failures, actual mint events): **DO NOT SUPPRESS**. An established protocol can still be attacked; suppressing event signals would mask real attacks.

**Predicate definition** (`token_status::is_established_protocol`):
- `jup_strict == true` (Jupiter strict list — curated, requires active human review), OR
- `jup_verified == true` AND `rugcheck_score < 40` (dual-signal filter, empirical boundary from P3-4 corpus)

**Applicability by detector:**

| Detector | Signal type | Apply suppression? |
|----------|------------|-------------------|
| D01 `honeypot_sim` | Static structural signals (freeze, fee, delegate, hook) | No — these are property-based (the property IS the risk); jup_verified attenuation (DG4) is a separate, weaker pattern |
| D02 `rug_pull_lp_drain` Signal A | Event-based drain | No |
| D02 `rug_pull_lp_drain` Signal B | State-based latent risk | **Yes** — implemented in P4-0 |
| D03 `holder_concentration` | Delta signals (event-like) | No — uses sidecar exclusion instead |
| D04 `pump_dump` | Volume spike (event-based) | No for event signals; Yes for any structural state companion |
| D05 `wash_trading_h1` | Round-trip events | No — purely event-based |
| D06 `mint_burn_anomaly` | Mint-authority structural check | **Yes** for the state companion (mint_authority active on "fixed-supply" token); No for observed mint events |

**Reference:** `docs/designs/0005-detector-02-rug-pull.md` §14 — D02 implementation and calibration detail.
**Empirical basis:** `research/fixtures/solana-corpus-phase1.md` §Calibration flag register (P3-4, 2026-04-21).

---

### Evidence schema detail: D01 Honeypot (first to ship)

For the honeypot detector, evidence MUST include the following keys (all `Decimal` values):

| Key | Value type | Example | Meaning |
|-----|-----------|---------|---------|
| `honeypot_sim/buy_sell_ratio` | Decimal | `"0.82"` | buy_count / sell_count from d01 SQL; 999 = zero sells |
| `honeypot_sim/sell_tax_est` | Decimal | `"0.00"` | Estimated sell tax from simulation or token metadata |
| `honeypot_sim/freeze_authority_active` | Decimal (0 or 1) | `"0"` | 1 = freeze authority set; 0 = renounced |
| `honeypot_sim/transfer_fee_bps` | Decimal | `"0"` | Token-2022 transfer fee in basis points; 0 = no fee |
| `honeypot_sim/simulate_paths_tested` | Decimal | `"3"` | How many probe amounts were tested in simulation |

`Evidence.addresses` should include: the pool address tested, the simulated buyer address.
`Evidence.tx_hashes` should include: the most recent sell transaction hash (if any) — confirms sells are working.
`Evidence.notes` should include a human-readable summary: "Sell ratio 0.82 against pool X; freeze authority renounced; no honeypot signals detected."

---

## Open Questions

1. **`TokenRegistry::classify_holder` public API.** The D03 concentration detector needs to call `classify_holder(address, chain)` on `TokenRegistry` for addresses not yet in the sidecar. `TokenRegistry` currently exposes only `enrich()`. Should `classify_holder` be a first-class public method on `TokenRegistry`, or should the D03 detector reach into the sidecar table directly via SQL? The SQL-join path (path 1) is preferred for batch reads but requires the sidecar to be reasonably complete. Decision needed before D03 is implemented.

2. **`DetectorContext.store` as `PgStore` vs trait object.** The current design uses concrete `&PgStore`. If integration tests need a fake DB without Postgres (testcontainers is heavy for unit-test CI), a `QueryRunner` trait in `crates/storage` would help. Defer to Phase 3 unless the P2-5 honeypot developer finds the `fetch_rows`/`compute` split insufficient.

3. **Simulation RPC for D01 (Solana honeypot).** `simulateTransaction` is not part of `PgStore` and is not part of `TokenRegistry`. The honeypot detector needs a `SolanaRpc` reference in `DetectorContext`. Either: (a) add an optional `rpc: Option<&'ctx dyn SolanaRpc>` field to `DetectorContext` (present for on-chain detectors, absent for storage-only detectors), or (b) D01 receives the RPC reference separately and is not a pure `Detector` implementor. Decision needed in P2-5.

4. **Pool address enumeration for multi-pool detectors.** D01 and D02 are per-pool detectors (the SQL takes a pool address parameter). A token can have multiple pools. Should the trait `evaluate()` be called once per token (and the implementation internally iterates pools), or called once per (token, pool) pair? The current design calls per-token and the implementation iterates. If a token has many pools, this may result in many SQL round-trips. An alternative: the context carries `pool_addresses: &[Address]` pre-fetched by the caller. Decide before D02 is implemented.

5. **Vesting-unlock calendar signal (WET probe gap).** The WET probe identified that the June 2026 unlock is a forward-looking precursor event not captured by any of the 6 MVP detectors. This is a Phase 3 signal (requires vesting schedule parsing from Jup Lock on-chain data). However, the `Detector` trait should be capable of hosting it without changes. Flag for Sprint 3 backlog.

---

## Non-Goals

This design explicitly does NOT cover:

- Implementation of any of the 6 detectors themselves (that is P2-5 and Sprint 3/4).
- The `scoring/` crate — combining multiple detector outputs into a single token risk score is Phase 5.
- The `gateway/` crate — REST and WS wire format for `AnomalyEvent` is Phase 4.
- The `client-sdk/` crate — Rust SDK for `bot-trader-2-0` is Phase 4.
- EVM chain adapters — Phase 4. `DetectorContext` carries a `Chain` field; detectors branch on it where chain-specific behavior is needed.
- Streaming/subscription invocation mode — deferred to Phase 3 (see §Structural Decision #2).
- The `scoring/` crate confidence combination formula (sigmoid weighting from `research/02-detection-methodology.md §Cross-cutting C`).

---

## Acceptance Checks for Developer

The following checklist constitutes the acceptance criterion for the developer implementing this design:

- [ ] `cargo check -p mg-onchain-detectors` passes with no errors.
- [ ] `cargo clippy -p mg-onchain-detectors --all-targets -- -D warnings` passes clean.
- [ ] `cargo test -p mg-onchain-detectors` passes (unit tests for config loader).
- [ ] `config/detectors.toml.example` parses without error via `load_detector_config()`.
- [ ] `AllDetectorConfigs` deserialization fails with a clear error message when a required threshold key is missing.
- [ ] `Threshold<f64>` deserialization fails when `rationale` is an empty string (add a custom validator or document that empty rationale is a code-review-time check).
- [ ] `DetectorContext` has no `pub` mutable fields.
- [ ] `Evidence::metrics` in any test output uses `BTreeMap`, never `HashMap`. (The type is already `BTreeMap` in `common`; this check confirms no intermediary converts to `HashMap`.)
- [ ] `DetectorError::is_retryable()` returns `true` for `TransientQuery` and `MissingDependencyData`; `false` for all others.
- [ ] A trivial "no-op" detector implementing the `Detector` trait compiles with native async fn syntax (no `async_trait`).
- [ ] At least one unit test exercises `load_detector_config` against `config/detectors.toml.example` and asserts `honeypot_sim.sell_tax_threshold.value == 0.50`.

---

## Common Type Gaps

The following gaps in frozen `crates/common` were surfaced during this design. They are NOT being fixed (frozen constraint). They are listed here for Sprint 3 evaluation.

1. **`TokenMeta` has no `lp_burned_pct` or `lp_locked_pct` field.** D02's state-based companion signal reads these from `MarketInfo` (which is a field on `TokenMeta` per `docs/designs/0001-crates-common-types.md`). The design assumes `MarketInfo` carries `lp_burned_pct: Option<Decimal>` and `lp_locked_pct: Option<Decimal>`. If these fields are absent, `token-registry` must add them to `MarketInfo` — which IS a `crates/common` change since `MarketInfo` is defined in `crates/common/src/token.rs`. Workaround for Phase 2: read these values from `PoolRow` in `PgStore` directly (bypassing `TokenMeta`). Flag for Sprint 3 common-type review.

2. **`Address` has no typed chain-specific byte representation.** The `common::chain::Address` type is `{chain: Chain, canonical: String}` — intentionally untyped bytes. Detectors that need to check "is this address the Solana zero address" must do a string comparison against a constant. This is fine for Phase 2 but becomes awkward with multiple chains. `DetectorContext.zero_address: &str` is the workaround.

3. **`AnomalyEvent` has no `pool` field.** Detectors that fire on a specific pool (D01 per-pool honeypot, D02 per-pool rug pull) must embed the pool address in `Evidence.addresses` with a `Evidence.notes` annotation, since `AnomalyEvent` only carries `token`. This is workable but loses structured access. Sprint 3 evaluation: add `pub pool: Option<Address>` to `AnomalyEvent`.

4. **`HolderSnapshot.balances` is `BTreeMap<Address, u128>` per design 0001.** Confirm this is a `BTreeMap` (deterministic iteration) and not a `HashMap`. If implementation used `HashMap`, this is a determinism bug to fix before D03 ships.

5. **No `AnomalyCategory` enum.** The RAVE probe §4 Gap 4 notes that "brand impersonation" is a distinct fraud category that should be in `crates/common`. Phase 2 workaround: use `detector_id` as the category discriminator. Sprint 3: propose `AnomalyCategory` enum addition to `crates/common`.
