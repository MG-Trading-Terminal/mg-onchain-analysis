//! `ErasedDetector` — dyn-compatible wrapper around the `Detector` trait.
//!
//! The `Detector` trait uses native async fn syntax (Rust 2024) and is NOT
//! object-safe (async fn is not dyn-compatible in stable Rust without special
//! handling).  The scheduler needs a heterogeneous `Vec<Arc<dyn ???>>`;
//! this module provides the wrapper.
//!
//! # Pattern
//!
//! ```text
//! ErasedDetector trait  (dyn-compatible, returning a pinned boxed Future)
//!   └── impl<T: Detector> ErasedDetector for T
//! ```
//!
//! Call sites use `Arc<dyn ErasedDetector>`.  The concrete erasing impls are
//! constructed in Phase 2 when detectors are wired in.
//!
//! # Design reference
//!
//! `docs/designs/0014-streaming-detector.md` §4 "Trait erasure (rev 1 reword)":
//! "Vec<Arc<dyn Detector>> uses the existing async-trait pattern already
//! adopted for SolanaRpc and PoolAccountProvider."
//!
//! # Note on `Send`
//!
//! `Detector::evaluate` is a native async fn whose returned future does not
//! carry an explicit `+ Send` bound.  `ErasedDetector::evaluate_erased` boxes
//! the future as `Pin<Box<dyn Future<...>>>` (no Send requirement).  Workers
//! `.await` the future in-place on the same task; the future is never sent
//! across threads, so the absence of `Send` is correct and safe.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use mg_onchain_common::anomaly::{AnomalyEvent, Severity};
use mg_onchain_common::chain::Chain;
use mg_onchain_detectors::context::DetectorContext;
use mg_onchain_detectors::detector::Detector;
use mg_onchain_detectors::error::DetectorError;

/// A boxed future returned by `ErasedDetector::evaluate_erased`.
///
/// `+ Send` is required because worker tasks are spawned with `tokio::spawn`,
/// which requires the spawned future to be `Send`.  The native async fn
/// futures returned by `Detector::evaluate` are `Send` as long as all
/// captured references are `Send` (PgStore / TokenRegistry are Arc-backed
/// and `Send`).
type DetectorFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<AnomalyEvent>, DetectorError>> + Send + 'a>>;

/// A dyn-compatible version of `Detector` that boxes the async future.
///
/// Implemented for all `T: Detector + Send + Sync`.
pub trait ErasedDetector: Send + Sync {
    /// Stable detector identifier — mirrors `Detector::id()`.
    fn id(&self) -> &'static str;

    /// Minimum severity — mirrors `Detector::severity_floor()`.
    fn severity_floor(&self) -> Severity;

    /// Chains supported by this detector — mirrors `Detector::supported_chains()`.
    ///
    /// The `SchedulerWorker` uses this to skip detectors for unsupported chains
    /// before calling `evaluate_erased`. All D01-D11 return `&[Chain::Solana]`
    /// (via the provided default on `Detector`).
    ///
    /// ADR 0005 Decision 2.
    fn supported_chains(&self) -> &[Chain];

    /// OAK Technique ID — mirrors `Detector::oak_technique_id()`.
    fn oak_technique_id(&self) -> Option<&str>;

    /// Evaluate and return a boxed, `Send` future.
    ///
    /// The `+ Send` bound on the returned future is required so that
    /// `SchedulerWorker::run()` (which awaits this future) can itself be
    /// `Send` and thus passed to `tokio::spawn`.
    fn evaluate_erased<'ctx>(&'ctx self, ctx: &'ctx DetectorContext<'ctx>) -> DetectorFuture<'ctx>;
}

impl<T> ErasedDetector for T
where
    T: Detector + Send + Sync,
{
    fn id(&self) -> &'static str {
        Detector::id(self)
    }

    fn severity_floor(&self) -> Severity {
        Detector::severity_floor(self)
    }

    fn supported_chains(&self) -> &[Chain] {
        Detector::supported_chains(self)
    }

    fn oak_technique_id(&self) -> Option<&str> {
        Detector::oak_technique_id(self)
    }

    fn evaluate_erased<'ctx>(&'ctx self, ctx: &'ctx DetectorContext<'ctx>) -> DetectorFuture<'ctx> {
        Box::pin(Detector::evaluate(self, ctx))
    }
}

/// Convenience alias: the storage type used by `SchedulerWorker::detectors`.
pub type ArcErasedDetector = Arc<dyn ErasedDetector>;
