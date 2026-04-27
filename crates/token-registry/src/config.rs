//! Configuration for the token-registry service.
//!
//! All values have compile-time defaults suitable for development.
//! Production overrides live in `config/token-registry.toml` (gitignored).
//! The example file at `config/token-registry.toml.example` documents every key.
//!
//! # Rate-limit & retry policy
//!
//! Retry uses exponential backoff with full jitter (prevents thundering-herd on
//! a shared Helius endpoint). Parameters:
//!   base_delay_ms    = 250ms   (first retry waits 0–250ms)
//!   max_delay_ms     = 30_000ms (cap at 30s per attempt)
//!   max_attempts     = 5
//!   backoff_factor   = 2.0
//!   429 treatment: counted as a retryable error; same backoff applies.
//!
//! Sources:
//!   - Helius rate-limit docs: https://docs.helius.dev/welcome/pricing#rate-limits
//!   - AWS SDK exponential backoff: https://aws.amazon.com/blogs/architecture/exponential-backoff-and-jitter/

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Full configuration for the token-registry service.
///
/// Loaded from `config/token-registry.toml` at startup (see `config.rs`).
/// All fields have sane defaults via [`Default`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RegistryConfig {
    /// RPC endpoints in priority order. First is the primary (Helius when key available);
    /// subsequent entries are fallbacks. Rotated on RPC error or rate-limit.
    /// Config key: `rpc_endpoints` (array of strings)
    pub rpc_endpoints: Vec<String>,

    /// TTL for metadata fields (symbol, name, supply, authorities).
    /// After this duration the record is re-enriched from RPC.
    /// Config key: `ttl_metadata_secs`
    pub ttl_metadata_secs: u64,

    /// TTL for holder data (`top_holders`, `total_holders`, `HolderSnapshot`).
    /// Longer than metadata TTL because holder queries are expensive.
    /// Config key: `ttl_holders_secs`
    pub ttl_holders_secs: u64,

    /// How often the periodic holder-snapshot job runs.
    /// Config key: `snapshot_interval_hours`
    pub snapshot_interval_hours: u64,

    /// Maximum number of concurrent enrichments. Controls the semaphore bound.
    /// Config key: `concurrency_limit`
    pub concurrency_limit: usize,

    /// Maximum number of top holders to fetch per token.
    /// Solana `getTokenLargestAccounts` returns at most 20.
    /// Config key: `top_holders_limit`
    pub top_holders_limit: usize,

    /// Retry policy.
    pub retry: RetryConfig,
}

/// Exponential backoff + jitter parameters for RPC retries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetryConfig {
    /// Initial delay before first retry (milliseconds).
    pub base_delay_ms: u64,
    /// Maximum delay cap (milliseconds).
    pub max_delay_ms: u64,
    /// Maximum total attempts (1 = no retry).
    pub max_attempts: u32,
    /// Backoff multiplier applied to base_delay_ms on each attempt.
    pub backoff_factor: f64,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            // Public mainnet endpoint as default. Replace with Helius in production.
            rpc_endpoints: vec![
                "https://api.mainnet-beta.solana.com".to_owned(),
            ],
            ttl_metadata_secs: 15 * 60,   // 15 minutes
            ttl_holders_secs: 60 * 60,    // 1 hour
            snapshot_interval_hours: 6,
            concurrency_limit: 8,
            top_holders_limit: 20,
            retry: RetryConfig::default(),
        }
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            base_delay_ms: 250,
            max_delay_ms: 30_000,
            max_attempts: 5,
            backoff_factor: 2.0,
        }
    }
}

impl RetryConfig {
    /// Compute the delay for attempt `n` (0-indexed) with full jitter.
    ///
    /// Formula: `min(max_delay, base * factor^n)` then uniform random in `[0, computed]`.
    /// Full jitter prevents thundering-herd when many callers hit backoff simultaneously.
    /// Source: https://aws.amazon.com/blogs/architecture/exponential-backoff-and-jitter/
    pub fn delay_for_attempt(&self, n: u32) -> Duration {
        let factor = self.backoff_factor.powi(n as i32);
        let computed_ms = (self.base_delay_ms as f64 * factor).min(self.max_delay_ms as f64) as u64;
        // Full jitter: uniform random in [0, computed_ms]
        let jitter_ms = if computed_ms == 0 {
            0
        } else {
            // Use simple pseudo-random based on current time nanos — no rand dep.
            // Good enough for jitter; not for crypto.
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos() as u64;
            nanos % (computed_ms + 1)
        };
        Duration::from_millis(jitter_ms)
    }

    /// Whether we have attempts remaining after `n` failures (0-indexed).
    pub fn should_retry(&self, attempts_made: u32) -> bool {
        attempts_made < self.max_attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sane_values() {
        let cfg = RegistryConfig::default();
        assert!(!cfg.rpc_endpoints.is_empty(), "must have at least one RPC endpoint");
        assert!(cfg.ttl_metadata_secs > 0);
        assert!(cfg.ttl_holders_secs >= cfg.ttl_metadata_secs, "holder TTL must be >= metadata TTL");
        assert!(cfg.concurrency_limit > 0);
        assert!(cfg.top_holders_limit <= 20, "Solana getTokenLargestAccounts max is 20");
    }

    #[test]
    fn retry_config_should_retry_until_max() {
        let cfg = RetryConfig { max_attempts: 3, ..Default::default() };
        assert!(cfg.should_retry(0));
        assert!(cfg.should_retry(1));
        assert!(cfg.should_retry(2));
        assert!(!cfg.should_retry(3));
    }

    #[test]
    fn retry_delay_does_not_exceed_max() {
        let cfg = RetryConfig {
            base_delay_ms: 250,
            max_delay_ms: 1_000,
            max_attempts: 10,
            backoff_factor: 2.0,
        };
        // Even at attempt 100 the delay is capped.
        let delay = cfg.delay_for_attempt(100);
        assert!(delay.as_millis() <= 1_000, "delay must not exceed max_delay_ms");
    }

    #[test]
    fn retry_delay_is_zero_for_base_zero() {
        let cfg = RetryConfig {
            base_delay_ms: 0,
            max_delay_ms: 0,
            max_attempts: 3,
            backoff_factor: 2.0,
        };
        assert_eq!(cfg.delay_for_attempt(0).as_millis(), 0);
    }
}
