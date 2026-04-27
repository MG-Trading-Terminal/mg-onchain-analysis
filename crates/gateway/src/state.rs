//! `AppState` — shared state injected into every axum handler via `State<Arc<AppState>>`.
//!
//! All fields are either `Clone`-cheap (Arc-wrapped) or `Send + Sync` value types.
//! Construction happens once at startup in `crates/server`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::broadcast;

use mg_onchain_common::chain::Chain;
use mg_onchain_detectors::config::DetectorConfig;
use mg_onchain_scoring::ScoringEngine;
use mg_onchain_storage::pg::PgStore;
use mg_onchain_token_registry::TokenRegistry;

use crate::auth::jwt::JwtKeys;
use crate::cache::RiskCache;
use crate::config::GatewayConfig;
use crate::metrics::GatewayMetrics;
use crate::ratelimit::RateLimitManager;

// ---------------------------------------------------------------------------
// Invalidation event
// ---------------------------------------------------------------------------

/// Broadcast when a token's cached risk report should be invalidated.
///
/// # New fields (Phase 1 streaming plumbing — design 0014 §8 step 4)
///
/// `block_time` — Unix timestamp (seconds) of the latest block in the event
/// batch.  The `DetectorScheduler` uses this to set `observed_at`
/// deterministically (never `Utc::now()`).  A value of `0` means the indexer
/// call site has not yet been updated; the scheduler will skip events where
/// `block_time == 0` rather than fall back to wall-clock.
///
/// `slot_hints` — Solana slot numbers (or equivalent chain heights) carried
/// by the batch.  Used by the scheduler to accumulate the full range of slots
/// covered by a debounced job.
#[derive(Clone, Debug)]
pub struct InvalidationEvent {
    pub chain: Chain,
    pub mint: String,
    /// Unix timestamp (seconds) of the latest block in the triggering batch.
    /// Must be `> 0`; `0` is a sentinel meaning "not populated by caller".
    pub block_time: i64,
    /// Chain slot numbers covered by this batch (may be empty for legacy callers).
    pub slot_hints: Vec<u64>,
}

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

/// Shared application state.
///
/// Wrapped in `Arc<AppState>` for axum state injection.
/// All fields must be `Send + Sync`.
pub struct AppState {
    /// Full gateway config (includes auth, ratelimit, cache, ws).
    pub config: GatewayConfig,

    /// Postgres storage.
    pub store: PgStore,

    /// Token metadata registry.
    pub registry: TokenRegistry,

    /// Stateless scoring engine.
    pub scoring: ScoringEngine,

    /// Loaded detector configs (thresholds, references).
    pub detector_config: DetectorConfig,

    /// JWT signing + verification keys.
    pub jwt_keys: JwtKeys,

    /// `TokenRiskReport` in-memory cache.
    pub risk_cache: RiskCache,

    /// Per-subject rate limiters.
    pub rate_limiter: RateLimitManager,

    /// Prometheus metrics.
    pub metrics: GatewayMetrics,

    /// Broadcast channel for cache invalidation events.
    /// The indexer-facing component (or admin endpoint) sends on this channel;
    /// the WS dispatcher also subscribes for live event updates.
    pub invalidation_tx: broadcast::Sender<InvalidationEvent>,

    /// In-flight analyze set: prevents duplicate concurrent analyze calls for the same token.
    /// Key: `"chain/mint"`.
    pub in_flight_analyzes: Arc<Mutex<HashSet<String>>>,

    /// Service start time for uptime computation.
    pub started_at: Instant,
}

impl AppState {
    /// Construct `AppState`. All components must already be initialized.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: GatewayConfig,
        store: PgStore,
        registry: TokenRegistry,
        scoring: ScoringEngine,
        detector_config: DetectorConfig,
        jwt_keys: JwtKeys,
        metrics: GatewayMetrics,
    ) -> Arc<Self> {
        let risk_cache = RiskCache::new(&config.gateway.cache);
        let rate_limiter = RateLimitManager::new(config.gateway.ratelimit.clone());
        let (invalidation_tx, _) = broadcast::channel(1024);

        Arc::new(Self {
            config,
            store,
            registry,
            scoring,
            detector_config,
            jwt_keys,
            risk_cache,
            rate_limiter,
            metrics,
            invalidation_tx,
            in_flight_analyzes: Arc::new(Mutex::new(HashSet::new())),
            started_at: Instant::now(),
        })
    }

    /// Compute uptime in seconds.
    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}
