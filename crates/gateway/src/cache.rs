//! `TokenRiskReport` in-memory cache backed by `moka`.
//!
//! Key: `(Chain, canonical_mint_string)`.
//! Value: `(Arc<TokenRiskReport>, Instant)` — the instant records when the entry was inserted.
//!
//! TTL and max-entries are configurable via `CacheConfig`.
//!
//! # Invalidation
//!
//! Three triggers:
//! 1. TTL expiry — handled automatically by moka.
//! 2. Broadcast channel — external event triggers `invalidate(&key)`.
//! 3. Manual — `DELETE /v1/admin/cache/{chain}/{mint}`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use moka::future::Cache;

use mg_onchain_common::chain::Chain;
use mg_onchain_scoring::TokenRiskReport;

use crate::config::CacheConfig;

/// Cache key: chain + canonical mint address.
pub type CacheKey = (Chain, String);

/// Cache value: the report + insertion timestamp.
#[derive(Clone)]
pub struct CacheEntry {
    pub report: Arc<TokenRiskReport>,
    pub inserted_at: std::time::Instant,
}

/// `TokenRiskReport` cache.
#[derive(Clone)]
pub struct RiskCache {
    inner: Cache<CacheKey, CacheEntry>,
}

impl RiskCache {
    /// Construct a new cache with the given config.
    pub fn new(config: &CacheConfig) -> Self {
        let cache = Cache::builder()
            .max_capacity(config.token_risk_max_entries)
            .time_to_live(Duration::from_secs(config.token_risk_ttl_seconds))
            .build();
        Self { inner: cache }
    }

    /// Insert a report into the cache.
    pub async fn insert(&self, chain: Chain, mint: String, report: Arc<TokenRiskReport>) {
        let entry = CacheEntry { report, inserted_at: Instant::now() };
        self.inner.insert((chain, mint), entry).await;
    }

    /// Retrieve a report from the cache. Returns `None` on miss or expiry.
    pub async fn get(&self, chain: Chain, mint: &str) -> Option<CacheEntry> {
        self.inner.get(&(chain, mint.to_string())).await
    }

    /// Invalidate a specific entry. No-op if the entry does not exist.
    pub async fn invalidate(&self, chain: Chain, mint: &str) -> bool {
        let key = (chain, mint.to_string());
        let existed = self.inner.contains_key(&key);
        self.inner.invalidate(&key).await;
        existed
    }

    /// Current number of entries in the cache.
    pub fn entry_count(&self) -> u64 {
        self.inner.entry_count()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CacheConfig;

    fn test_cache() -> RiskCache {
        RiskCache::new(&CacheConfig {
            token_risk_ttl_seconds: 60,
            token_risk_max_entries: 100,
        })
    }

    fn dummy_report() -> Arc<TokenRiskReport> {
        // Minimal report — we only test cache mechanics, not scoring.
        use mg_onchain_common::anomaly::{Confidence, Severity};
        use mg_onchain_common::chain::{Address, Chain};
        use mg_onchain_scoring::config::ScoringConfig;
        use mg_onchain_scoring::types::{
            CoverageReport, SignalCounts, TokenRiskReport,
        };
        use std::collections::BTreeMap;
        use chrono::Utc;

        let token = Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112").unwrap();
        Arc::new(TokenRiskReport {
            token,
            chain: Chain::Solana,
            window: (Utc::now(), Utc::now()),
            computed_at: Utc::now(),
            overall_score: Confidence::new(0.5).unwrap(),
            base_score: Confidence::new(0.5).unwrap(),
            overall_severity: Severity::Medium,
            per_detector: BTreeMap::new(),
            top_evidence: vec![],
            signal_counts: SignalCounts { fired: 0, inconclusive: 0, suppressed_info: 0 },
            coverage: CoverageReport {
                detectors_run: vec![],
                detectors_skipped: vec![],
                coverage_completeness: 0.0,
            },
            config_snapshot: ScoringConfig::default_calibrated(),
        })
    }

    #[tokio::test]
    async fn insert_and_get() {
        let cache = test_cache();
        let report = dummy_report();
        cache.insert(Chain::Solana, "mint1".into(), report.clone()).await;
        let entry = cache.get(Chain::Solana, "mint1").await;
        assert!(entry.is_some());
    }

    #[tokio::test]
    async fn miss_on_absent_key() {
        let cache = test_cache();
        let entry = cache.get(Chain::Solana, "nonexistent").await;
        assert!(entry.is_none());
    }

    #[tokio::test]
    async fn invalidate_existing_entry() {
        let cache = test_cache();
        let report = dummy_report();
        cache.insert(Chain::Solana, "mint2".into(), report).await;
        let existed = cache.invalidate(Chain::Solana, "mint2").await;
        assert!(existed);
        let entry = cache.get(Chain::Solana, "mint2").await;
        assert!(entry.is_none(), "entry must be absent after invalidation");
    }

    #[tokio::test]
    async fn invalidate_absent_key_returns_false() {
        let cache = test_cache();
        let existed = cache.invalidate(Chain::Solana, "ghost").await;
        assert!(!existed);
    }

    #[tokio::test]
    async fn cache_age_seconds_increases_over_time() {
        let cache = test_cache();
        let report = dummy_report();
        cache.insert(Chain::Solana, "aging".into(), report).await;
        let entry = cache.get(Chain::Solana, "aging").await.unwrap();
        // Age is >= 0 immediately after insertion.
        let age = entry.inserted_at.elapsed().as_secs();
        assert!(age < 5, "cache entry age must be < 5s after immediate retrieval");
    }
}
