//! [`DetectorContext`] — the read-only view a detector gets of the world.
//!
//! A detector receives exactly what it needs and nothing more:
//! - The target token identity.
//! - The observation time window (block-time sourced — NOT wall-clock).
//! - A borrowed reference to the Postgres query runner.
//! - A borrowed reference to the token registry (enriched metadata + classifier).
//! - A borrowed reference to the full detector config (all thresholds).
//!
//! `DetectorContext` is borrowed, not owned. Detectors cannot mutate shared state.
//! The lifetime `'ctx` ties the context references to the caller's scope.
//!
//! # Determinism and `observed_at`
//!
//! The `observed_at` field was added to fix the C1 finding from security reviews
//! `docs/reviews/0001-d01-honeypot-evasions.md` and `docs/reviews/0002-d02-rug-pull-evasions.md`:
//! detectors used `Utc::now()` inside `make_event()`, breaking the CLAUDE.md reproducibility
//! requirement. Callers now supply `observed_at` (typically `window.end` for deterministic
//! replay or a batch-scoped timestamp in production), and detectors write it verbatim into
//! `AnomalyEvent.ingested_at`. Given the same `ctx`, two evaluations produce identical events.

use chrono::{DateTime, Utc};

use mg_onchain_common::chain::{Address, BlockRef, Chain};
use mg_onchain_storage::pg::PgStore;
use mg_onchain_token_registry::TokenRegistry;

use crate::config::DetectorConfig;

// ---------------------------------------------------------------------------
// DetectorWindow
// ---------------------------------------------------------------------------

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
    /// Block at the inclusive start of the window, for evidence population.
    pub block_start: BlockRef,
    /// Block at the exclusive end of the window, for evidence population.
    pub block_end: BlockRef,
}

// ---------------------------------------------------------------------------
// DetectorContext
// ---------------------------------------------------------------------------

/// The read-only context injected into every detector invocation.
///
/// # Borrowing and lifetime
///
/// All fields are borrowed references tied to lifetime `'ctx`. The caller
/// (scheduler or on-demand API handler) owns the storage, registry, and config;
/// this struct borrows them for the duration of one `evaluate()` call.
///
/// Detectors MUST NOT retain a reference to `DetectorContext` beyond `evaluate()`.
///
/// # Storage abstraction
///
/// `store` is a concrete `&PgStore` (OQ2 resolution). If a `QueryRunner` trait
/// abstraction is added to `crates/storage` in Phase 3, this field type can change
/// to `&dyn QueryRunner` with no changes to detector code — detectors call methods
/// on `ctx.store` by name, not by trait bound.
///
/// Unit-testable detectors should use the `fetch_rows` / `compute` split described
/// in `mock.rs`: the `compute` pure function takes a `&[MyRow]` slice and needs
/// no database at all. See `mock.rs` for the test pattern.
pub struct DetectorContext<'ctx> {
    /// The token being evaluated.
    pub token: &'ctx Address,
    /// Which chain the token lives on.
    pub chain: Chain,
    /// The observation window (block-time sourced; NOT wall-clock).
    pub window: DetectorWindow,
    /// The timestamp to record as `AnomalyEvent.ingested_at` for all events emitted
    /// during this evaluation batch.
    ///
    /// # Why this exists (C1 fix)
    ///
    /// The security reviews (0001-d01-honeypot-evasions.md §8.1,
    /// 0002-d02-rug-pull-evasions.md §8.C1) identified that using `Utc::now()` inside
    /// `make_event()` breaks CLAUDE.md's determinism requirement: "given the same block
    /// range input, output MUST be deterministic." Two evaluations of identical inputs
    /// differed in `ingested_at` by milliseconds.
    ///
    /// Callers (scheduler, on-demand API handler) set this field once per batch:
    /// - In production: the scheduler's wall-clock time at the start of the batch.
    /// - In tests: a fixed literal such as
    ///   `DateTime::parse_from_rfc3339("2026-04-21T12:00:00Z").unwrap().with_timezone(&Utc)`.
    /// - For historical replay: `window.end` (block-time), ensuring bit-identical output
    ///   for the same block range.
    pub observed_at: DateTime<Utc>,
    /// Postgres query runner.
    ///
    /// In integration tests, use a real PgStore against a testcontainers Postgres.
    /// In unit tests, avoid DB entirely by testing the `compute` pure function
    /// with canned row data — see `mock.rs` for the split pattern.
    pub store: &'ctx PgStore,
    /// Token metadata + holder classifications from `crates/token-registry`.
    ///
    /// Detectors call `ctx.registry.enrich(mint, chain).await` to get a full
    /// `TokenMeta`. For holder classification in D03: call
    /// `ctx.registry.classify_holder(address, chain).await` for addresses not yet
    /// in the `holder_classifications` sidecar table.
    pub registry: &'ctx TokenRegistry,
    /// All detector thresholds loaded from `config/detectors.toml`.
    ///
    /// Each detector accesses only its own subsection by field name, e.g.
    /// `ctx.config.honeypot_sim.sell_tax_threshold.value`.
    /// The loader guarantees this field is populated before `evaluate()` is called.
    pub config: &'ctx DetectorConfig,
    /// The chain's null/zero address, for distinguishing mint/burn from transfers.
    ///
    /// Passed explicitly rather than hardcoded so EVM and Solana adapters differ
    /// without branching inside detector logic.
    ///
    /// Solana: `"11111111111111111111111111111111"` (the system program / null key).
    /// EVM: `"0x0000000000000000000000000000000000000000"`.
    ///
    /// This is a workaround for the `common` type gap: `Address` carries no
    /// typed chain-specific null constant (gap #2 in design 0003).
    pub zero_address: &'ctx str,
}
