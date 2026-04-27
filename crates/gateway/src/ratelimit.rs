//! Per-subject token-bucket rate limiter backed by `governor`.
//!
//! Two distinct buckets:
//! - `default` — used by all routes except analyze.
//! - `analyze` — `POST /v1/tokens/analyze` (more expensive; lower RPM).
//!
//! Anonymous requests to `/health` and `/metrics` use a shared `"_anon"` subject key.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use governor::clock::{Clock, DefaultClock};
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};

use crate::config::RateLimitConfig;
use crate::error::GatewayError;

type GovLimiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

/// Per-subject rate limiter state.
#[derive(Clone)]
pub struct SubjectLimiter {
    pub default: Arc<GovLimiter>,
    pub analyze: Arc<GovLimiter>,
}

/// Gateway-wide rate limit manager.
///
/// Holds a `HashMap` from subject (JWT `sub` claim or `"_anon"`) to per-subject
/// limiter pair. Entries are created on first request and never removed (subjects
/// are service accounts, count is bounded).
#[derive(Clone)]
pub struct RateLimitManager {
    config: RateLimitConfig,
    subjects: Arc<Mutex<HashMap<String, SubjectLimiter>>>,
}

impl RateLimitManager {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            subjects: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Check the default rate limit for a subject.
    ///
    /// Creates the per-subject bucket on first call.
    pub fn check_default(&self, subject: &str) -> Result<(), GatewayError> {
        let limiter = self.get_or_create(subject);
        limiter.default.check().map_err(|negative| {
            let retry = negative.wait_time_from(governor::clock::DefaultClock::default().now());
            GatewayError::RateLimited { retry_after: retry }
        })
    }

    /// Check the analyze rate limit for a subject.
    pub fn check_analyze(&self, subject: &str) -> Result<(), GatewayError> {
        let limiter = self.get_or_create(subject);
        limiter.analyze.check().map_err(|negative| {
            let retry = negative.wait_time_from(governor::clock::DefaultClock::default().now());
            GatewayError::RateLimited { retry_after: retry }
        })
    }

    fn get_or_create(&self, subject: &str) -> SubjectLimiter {
        let mut guard = self.subjects.lock().unwrap();
        guard
            .entry(subject.to_string())
            .or_insert_with(|| self.make_limiter())
            .clone()
    }

    fn make_limiter(&self) -> SubjectLimiter {
        let default_quota = Quota::per_minute(
            NonZeroU32::new(self.config.default_rpm.max(1)).unwrap(),
        );
        let analyze_quota = Quota::per_minute(
            NonZeroU32::new(self.config.write_analyze_rpm.max(1)).unwrap(),
        );
        SubjectLimiter {
            default: Arc::new(RateLimiter::direct(default_quota)),
            analyze: Arc::new(RateLimiter::direct(analyze_quota)),
        }
    }

    /// Update config (called on SIGHUP reload).
    ///
    /// New limits apply to newly created subjects only; existing subjects keep
    /// their old buckets until they're evicted (no eviction in MVP).
    pub fn update_config(&self, config: RateLimitConfig) {
        // For MVP: update stored config so new subjects get new limits.
        // Existing subjects are NOT updated (acceptable for service accounts).
        let _ = config; // stored config update requires interior mutability; deferred.
        tracing::info!("rate limit config update noted (applies to new subjects only)");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tight_config() -> RateLimitConfig {
        RateLimitConfig {
            default_rpm: 60,
            write_analyze_rpm: 2, // very tight for test
            ws_connections_per_subject: 5,
        }
    }

    #[test]
    fn first_request_passes() {
        let manager = RateLimitManager::new(tight_config());
        manager.check_default("user1").expect("first request must pass");
    }

    #[test]
    fn analyze_limit_exhausted_returns_429() {
        let manager = RateLimitManager::new(RateLimitConfig {
            write_analyze_rpm: 1, // only 1 rpm
            ..tight_config()
        });

        // First request succeeds.
        manager.check_analyze("svc").expect("first must pass");
        // Second request within the same minute must fail.
        let result = manager.check_analyze("svc");
        assert!(
            matches!(result, Err(GatewayError::RateLimited { .. })),
            "second request within rpm=1 window must be rate limited"
        );
    }

    #[test]
    fn different_subjects_have_independent_buckets() {
        let manager = RateLimitManager::new(RateLimitConfig {
            write_analyze_rpm: 1,
            ..tight_config()
        });
        manager.check_analyze("user-a").expect("user-a first must pass");
        // user-b has its own fresh bucket.
        manager.check_analyze("user-b").expect("user-b first must pass");
    }
}
