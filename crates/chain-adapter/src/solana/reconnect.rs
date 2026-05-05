//! Reconnect loop with exponential backoff for the Solana JSON-RPC WebSocket stream.
//!
//! # Design
//!
//! Uses `tokio-retry` (v0.3) with `ExponentialBackoff` because:
//! - It provides full-jitter exponential backoff out of the box.
//! - It integrates cleanly with `tokio::time::sleep` (no extra deps).
//! - `backoff` crate (the alternative) is more complex and designed for sync code.
//!
//! # Reconnect vs rate-limit
//!
//! Two distinct backoff schedules:
//! 1. **Normal reconnect** (`base_delay_ms → max_delay_ms × 2^attempt` + jitter):
//!    triggered by `AdapterError::Transport`, `AdapterError::StreamEnded`.
//! 2. **Rate-limit backoff** (`rate_limit_base_ms → max_delay_ms × 2^attempt`):
//!    triggered by `AdapterError::RateLimit`.
//!    Applied with an INITIAL delay before the first reconnect attempt.
//!
//! # Reconnect failure
//!
//! After `max_attempts` consecutive failures, the reconnect loop terminates and
//! propagates the last error. The `server` crate's health check detects this and
//! marks the adapter unhealthy.

use std::time::Duration;

use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tracing::{error, info, warn};

use crate::{error::AdapterError, solana::config::ReconnectPolicy};

/// Result of a single reconnect strategy evaluation.
#[derive(Debug)]
pub enum ReconnectDecision {
    /// Retry after the computed delay.
    Retry { delay: Duration, attempt: u32 },
    /// Give up — max attempts reached or error is not reconnectable.
    Abort { reason: String },
}

/// Compute the reconnect delay for the given attempt number and error type.
///
/// Returns `None` if the error is not reconnectable (e.g., `Config`) — callers
/// should not retry on non-reconnectable errors.
pub fn compute_delay(policy: &ReconnectPolicy, attempt: u32, error: &AdapterError) -> Option<Duration> {
    if !error.is_reconnectable() {
        return None;
    }

    let base_ms = if error.is_rate_limit() {
        policy.rate_limit_base_ms
    } else {
        policy.base_delay_ms
    };

    // Exponential backoff: base * 2^attempt, capped at max_delay_ms.
    let delay_ms = (base_ms as u128)
        .saturating_mul(1u128 << attempt.min(30))
        .min(policy.max_delay_ms as u128);

    Some(Duration::from_millis(delay_ms as u64))
}

/// Evaluate whether to reconnect after an error and at what attempt count.
pub fn decide_reconnect(
    policy: &ReconnectPolicy,
    attempt: u32,
    error: &AdapterError,
) -> ReconnectDecision {
    if !error.is_reconnectable() {
        return ReconnectDecision::Abort {
            reason: format!("non-reconnectable error: {error}"),
        };
    }

    if policy.max_attempts > 0 && attempt >= policy.max_attempts {
        return ReconnectDecision::Abort {
            reason: format!(
                "max reconnect attempts ({}) reached; last error: {error}",
                policy.max_attempts
            ),
        };
    }

    let delay = compute_delay(policy, attempt, error).unwrap_or(Duration::from_millis(
        policy.base_delay_ms,
    ));

    if error.is_rate_limit() {
        warn!(
            attempt,
            delay_ms = delay.as_millis(),
            "rate limited by provider — applying extended backoff"
        );
    } else {
        warn!(
            attempt,
            delay_ms = delay.as_millis(),
            "WS stream error — will reconnect after delay"
        );
    }

    ReconnectDecision::Retry { delay, attempt }
}

/// Execute `action` with exponential-backoff reconnect, using `tokio-retry`.
///
/// - `action(attempt)` is called with the current attempt number (0-indexed).
/// - Returns the first `Ok` value, or the last `Err` after all attempts.
/// - Aborts immediately on non-reconnectable errors.
///
/// This is the primary entrypoint for the subscribe loop. The caller passes
/// a closure that establishes a WebSocket connection and returns the stream.
pub async fn with_reconnect<F, Fut, T>(
    policy: &ReconnectPolicy,
    operation_name: &'static str,
    mut action: F,
) -> Result<T, AdapterError>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, AdapterError>>,
{
    let strategy = ExponentialBackoff::from_millis(policy.base_delay_ms)
        .max_delay(Duration::from_millis(policy.max_delay_ms))
        .map(jitter)
        .take(policy.max_attempts as usize);

    let mut attempt = 0u32;
    let mut last_error: Option<AdapterError> = None;

    for delay in strategy {
        if attempt > 0 {
            info!(
                attempt,
                operation = operation_name,
                delay_ms = delay.as_millis(),
                "reconnecting after delay"
            );
            tokio::time::sleep(delay).await;
        }

        match action(attempt).await {
            Ok(result) => {
                if attempt > 0 {
                    info!(attempt, operation = operation_name, "reconnected successfully");
                }
                return Ok(result);
            }
            Err(e) => {
                if !e.is_reconnectable() {
                    error!(
                        attempt,
                        operation = operation_name,
                        error = %e,
                        "non-reconnectable error — aborting"
                    );
                    return Err(e);
                }

                // Apply rate-limit extended delay immediately.
                if e.is_rate_limit() {
                    let rl_delay = Duration::from_millis(
                        (policy.rate_limit_base_ms as u128)
                            .saturating_mul(1u128 << attempt.min(30))
                            .min(policy.max_delay_ms as u128) as u64,
                    );
                    warn!(
                        attempt,
                        operation = operation_name,
                        delay_ms = rl_delay.as_millis(),
                        "rate-limit response — applying extended backoff"
                    );
                    tokio::time::sleep(rl_delay).await;
                } else {
                    warn!(
                        attempt,
                        operation = operation_name,
                        error = %e,
                        "transient error"
                    );
                }

                last_error = Some(e);
                attempt += 1;
            }
        }
    }

    error!(
        operation = operation_name,
        attempts = attempt,
        "max reconnect attempts reached"
    );

    Err(last_error.unwrap_or(AdapterError::StreamEnded { slot: 0 }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solana::config::ReconnectPolicy;

    fn policy() -> ReconnectPolicy {
        ReconnectPolicy {
            base_delay_ms: 100,
            max_delay_ms: 10_000,
            max_attempts: 5,
            rate_limit_base_ms: 500,
        }
    }

    fn transport_err() -> AdapterError {
        AdapterError::Transport("test disconnect".into())
    }

    fn rate_limit_err() -> AdapterError {
        AdapterError::RateLimit { slot: 200 }
    }

    fn config_err() -> AdapterError {
        AdapterError::Config("bad endpoint".into())
    }

    fn stream_ended_err() -> AdapterError {
        AdapterError::StreamEnded { slot: 0 }
    }

    // --- compute_delay ---

    #[test]
    fn compute_delay_transport_error_returns_some() {
        let delay = compute_delay(&policy(), 0, &transport_err());
        assert!(delay.is_some());
        assert!(delay.unwrap() >= Duration::ZERO);
    }

    #[test]
    fn compute_delay_config_error_returns_none() {
        let delay = compute_delay(&policy(), 0, &config_err());
        assert!(delay.is_none(), "Config errors must not be retried");
    }

    #[test]
    fn compute_delay_rate_limit_uses_larger_base() {
        let rl_delay = compute_delay(&policy(), 0, &rate_limit_err()).unwrap();
        let normal_delay = compute_delay(&policy(), 0, &transport_err()).unwrap();
        // Rate-limit base (500ms) must produce a >= delay than normal base (100ms) at attempt 0.
        assert!(rl_delay >= normal_delay);
    }

    #[test]
    fn compute_delay_caps_at_max() {
        let delay = compute_delay(&policy(), 30, &transport_err()).unwrap();
        assert!(delay.as_millis() <= policy().max_delay_ms as u128);
    }

    // --- decide_reconnect ---

    #[test]
    fn decide_reconnect_transport_returns_retry() {
        let decision = decide_reconnect(&policy(), 0, &transport_err());
        assert!(matches!(decision, ReconnectDecision::Retry { .. }));
    }

    #[test]
    fn decide_reconnect_config_error_returns_abort() {
        let decision = decide_reconnect(&policy(), 0, &config_err());
        assert!(matches!(decision, ReconnectDecision::Abort { .. }));
    }

    #[test]
    fn decide_reconnect_max_attempts_reached_returns_abort() {
        let decision = decide_reconnect(&policy(), 5, &transport_err()); // attempt == max_attempts
        assert!(matches!(decision, ReconnectDecision::Abort { .. }));
    }

    #[test]
    fn decide_reconnect_unlimited_retries_never_aborts_on_attempt() {
        let unlimited_policy = ReconnectPolicy {
            max_attempts: 0, // 0 = unlimited
            ..policy()
        };
        let decision = decide_reconnect(&unlimited_policy, 999, &transport_err());
        // With max_attempts=0, should not abort due to attempt count.
        assert!(matches!(decision, ReconnectDecision::Retry { .. }));
    }

    // --- with_reconnect ---

    #[tokio::test]
    async fn with_reconnect_succeeds_on_first_try() {
        let result = with_reconnect(&policy(), "test_op", |attempt| async move {
            assert_eq!(attempt, 0);
            Ok::<u32, AdapterError>(42)
        })
        .await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn with_reconnect_retries_and_eventually_succeeds() {
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cc = call_count.clone();

        let result = with_reconnect(&policy(), "test_retry_op", move |_attempt| {
            let cc = cc.clone();
            async move {
                let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n < 2 {
                    Err(AdapterError::Transport("simulated disconnect".into()))
                } else {
                    Ok::<u32, AdapterError>(99)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 99);
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "should have been called 3 times (2 failures + 1 success)"
        );
    }

    #[tokio::test]
    async fn with_reconnect_aborts_on_config_error() {
        let result = with_reconnect(&policy(), "test_abort", |_| async {
            Err::<u32, AdapterError>(AdapterError::Config("bad config".into()))
        })
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn with_reconnect_aborts_after_max_attempts() {
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cc = call_count.clone();

        let tight_policy = ReconnectPolicy {
            base_delay_ms: 1, // near-instant for tests
            max_delay_ms: 1,
            max_attempts: 3,
            rate_limit_base_ms: 1,
        };

        let result = with_reconnect(&tight_policy, "test_max_attempts", move |_| {
            let cc = cc.clone();
            async move {
                cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err::<u32, AdapterError>(stream_ended_err())
            }
        })
        .await;

        assert!(result.is_err());
        // Should have tried `max_attempts` times (3), not more.
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "should stop after max_attempts"
        );
    }
}
