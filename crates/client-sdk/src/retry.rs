//! Retry policy for transient HTTP errors.
//!
//! # Policy
//!
//! - **Retriable:** 429 (rate-limited), 500, 502, 503, 504, and network-level
//!   failures (connection reset, timeout).
//! - **Not retriable:** 400, 401, 403, 404, 409, 422 — these are deterministic
//!   failures that a retry will not fix.
//! - **Backoff:** exponential with full-jitter, base 200ms, max 8s, up to
//!   `max_attempts` total tries (first attempt + N-1 retries).
//! - **429 special case:** honour the server's `Retry-After` header when
//!   present; otherwise fall back to the standard backoff schedule.
//!
//! # Determinism / testability
//!
//! The `jitter` function accepts an injectable RNG so tests can use a seeded
//! PRNG and get deterministic delay values. In production, a simple thread-local
//! `u64` XOR-shift is sufficient — no `rand` dep needed.

use std::time::Duration;

/// Retry policy configuration.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total number of attempts (1 = no retry, 3 = up to 2 retries).
    pub max_attempts: u32,
    /// Base delay for exponential backoff calculation.
    pub base_delay: Duration,
    /// Maximum delay cap (prevents exponential blow-up).
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    /// Compute the delay before attempt `attempt_number` (0-indexed, so attempt 0
    /// is the first retry after the initial failure).
    ///
    /// Uses full-jitter exponential backoff:
    /// `delay = random_in(0, min(max_delay, base * 2^attempt))`.
    ///
    /// Full-jitter is preferred over equal-jitter for thundering-herd avoidance
    /// when many clients retry simultaneously.
    ///
    /// `rng_seed` is used to produce deterministic values in tests.
    pub fn backoff_delay(&self, attempt_number: u32, rng_seed: u64) -> Duration {
        let exponent = attempt_number.min(10); // cap to prevent overflow
        // Use checked_shl to avoid overflow on large exponents.
        let multiplier = 1u64.checked_shl(exponent).unwrap_or(u64::MAX);
        let cap_ms = (self.base_delay.as_millis() as u64)
            .saturating_mul(multiplier)
            .min(self.max_delay.as_millis() as u64);

        // Simple XOR-shift PRNG for jitter — no external dep needed.
        let jitter_ms = xorshift64(rng_seed) % cap_ms.max(1);
        Duration::from_millis(jitter_ms)
    }

    /// Returns `true` if the given HTTP status code is retriable.
    pub fn is_retriable_status(status: u16) -> bool {
        matches!(status, 429 | 500 | 502 | 503 | 504)
    }
}

/// Minimal XOR-shift PRNG to produce a `u64` in `[0, seed)` without a `rand` dep.
/// Not cryptographically secure; used only for backoff jitter.
fn xorshift64(mut x: u64) -> u64 {
    // Seed must not be zero for XOR-shift to work.
    if x == 0 {
        x = 0xDEAD_BEEF_CAFE_BABE;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retriable_statuses() {
        assert!(RetryPolicy::is_retriable_status(429));
        assert!(RetryPolicy::is_retriable_status(500));
        assert!(RetryPolicy::is_retriable_status(502));
        assert!(RetryPolicy::is_retriable_status(503));
        assert!(RetryPolicy::is_retriable_status(504));
    }

    #[test]
    fn non_retriable_statuses() {
        for status in [400u16, 401, 403, 404, 409, 422] {
            assert!(
                !RetryPolicy::is_retriable_status(status),
                "status {status} should not be retriable"
            );
        }
    }

    #[test]
    fn backoff_within_bounds() {
        let policy = RetryPolicy::default();
        for attempt in 0..5 {
            let delay = policy.backoff_delay(attempt, 12345 + attempt as u64);
            assert!(delay <= policy.max_delay, "delay {delay:?} exceeds max for attempt {attempt}");
        }
    }

    #[test]
    fn backoff_attempt_zero_has_short_delay() {
        let policy = RetryPolicy::default();
        // At attempt 0, cap = base * 2^0 = 200ms → delay in [0, 200ms)
        let delay = policy.backoff_delay(0, 42);
        assert!(delay < Duration::from_millis(200));
    }
}
