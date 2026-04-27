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
use mg_onchain_common::chain::Chain;

/// The shared invocation contract for every anomaly detector.
///
/// # Object safety note
///
/// The `#[allow(async_fn_in_trait)]` annotation below acknowledges that this
/// trait uses native async fn syntax (Rust 2024 / MSRV 1.75) and is NOT
/// object-safe. This is an intentional design decision — see module-level doc.
/// The `Send` bound is explicit in the doc comment for `evaluate`; object-safe
/// dispatch is left to the `ErasedDetector` wrapper pattern in `crates/server`.
///
/// # Object safety
///
/// This trait is NOT object-safe due to native async fn and the associated
/// `impl Future` return type. Object-safe dispatch (for a scheduler holding
/// `Vec<Box<dyn ???>>`) requires an `ErasedDetector` wrapper at the call
/// site — see module-level doc for the recommended approach.
///
/// # Returns
///
/// `Ok(Vec<AnomalyEvent>)` — may be empty (no anomaly detected), one element
/// (most detectors), or multiple (e.g. D05 wash trading may flag multiple actors).
///
/// `Err(DetectorError)` — see `DetectorError` variants for retry semantics.
pub trait Detector: Send + Sync {
    /// A stable machine-readable identifier for this detector.
    ///
    /// Convention: `snake_case`. Must match the prefix used in `Evidence::metrics`
    /// keys and the subsection name in `config/detectors.toml`.
    ///
    /// Examples: `"honeypot_sim"`, `"rug_pull_lp_drain"`, `"holder_concentration"`,
    /// `"pump_dump"`, `"wash_trading_h1"`, `"mint_burn_anomaly"`.
    fn id(&self) -> &'static str;

    /// The minimum severity this detector can emit.
    ///
    /// Used by schedulers to skip evaluations when the consumer's severity filter
    /// is higher than this floor. For example, if the consumer only wants
    /// `Severity::High` or above and the detector floor is `Severity::Low`, the
    /// scheduler can skip the detector entirely.
    fn severity_floor(&self) -> mg_onchain_common::anomaly::Severity;

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
    fn evaluate<'ctx>(
        &'ctx self,
        ctx: &'ctx DetectorContext<'ctx>,
    ) -> impl std::future::Future<Output = Result<Vec<AnomalyEvent>, DetectorError>> + Send + 'ctx;

    /// Return the chains supported by this detector.
    ///
    /// The `SchedulerWorker` calls this before dispatch: if `job.chain` is not
    /// in the returned slice, the job is skipped for this detector without error.
    ///
    /// # Default
    ///
    /// Returns `&[Chain::Solana]`. All existing D01-D11 detectors inherit this
    /// default at zero cost — they are Solana-specific at the query level.
    ///
    /// # Non-breaking
    ///
    /// This is a provided method. Adding it to the trait does not break any
    /// existing `Detector` implementation. EVM detectors added in Sprint 18+
    /// will override to `&[Chain::Ethereum]` or the appropriate slice.
    ///
    /// # Object safety
    ///
    /// This method is compatible with the existing `ErasedDetector` wrapper:
    /// `ErasedDetector::supported_chains` mirrors this method and is callable
    /// through `Arc<dyn ErasedDetector>` in `SchedulerWorker`.
    ///
    /// ADR 0005 Decision 2.
    fn supported_chains(&self) -> &[Chain] {
        &[Chain::Solana]
    }
}
