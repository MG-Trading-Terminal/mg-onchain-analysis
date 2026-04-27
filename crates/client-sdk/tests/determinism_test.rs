//! Determinism tests: same inputs produce identical request bytes.
//!
//! The SDK must not add timestamps or random jitter to request bodies.
//! Idempotency-key generation is consumer opt-in (not tested here).

use mg_onchain_client_sdk::types::{AnalyzeRequest, EventsFilter};
use mg_onchain_common::chain::Chain;

#[test]
fn analyze_request_serde_is_deterministic() {
    let req = AnalyzeRequest {
        chain: Chain::Solana,
        mint: "So11111111111111111111111111111111111111112".into(),
        window_hours: Some(24),
    };

    // Serialise twice: must produce identical bytes.
    let json1 = serde_json::to_string(&req).expect("first serialize");
    let json2 = serde_json::to_string(&req).expect("second serialize");

    assert_eq!(json1, json2, "serialization is not deterministic");
}

#[test]
fn analyze_request_no_hidden_timestamp() {
    let req = AnalyzeRequest {
        chain: Chain::Solana,
        mint: "So11111111111111111111111111111111111111112".into(),
        window_hours: None,
    };

    let json = serde_json::to_string(&req).expect("serialize");
    // The body must not contain any ISO 8601 timestamp.
    // A timestamp contains a 'T' separator between date and time.
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let obj = v.as_object().expect("should be an object");

    for (key, val) in obj {
        if let Some(s) = val.as_str() {
            // A timestamp looks like "2026-04-21T..." — none should be present.
            assert!(
                !s.contains('T') || key == "chain" || key == "mint",
                "unexpected timestamp-like field '{key}': '{s}'"
            );
        }
    }
}

#[test]
fn events_filter_default_has_no_required_fields() {
    // EventsFilter must be constructable with all-None fields.
    let filter = EventsFilter::default();
    assert!(filter.chain.is_none());
    assert!(filter.token.is_none());
    assert!(filter.detector_id.is_none());
    assert!(filter.severity_min.is_none());
    assert!(filter.from.is_none());
    assert!(filter.to.is_none());
    assert!(filter.limit.is_none());
    assert!(filter.cursor.is_none());
}

#[test]
fn retry_jitter_is_bounded_by_cap() {
    // Internal helper: jitter output must always be < cap.
    use mg_onchain_client_sdk::retry::RetryPolicy;
    use std::time::Duration;

    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(200),
        max_delay: Duration::from_secs(8),
    };

    for attempt in 0..10u32 {
        for seed in [0u64, 1, 42, 999, u64::MAX / 2] {
            let delay = policy.backoff_delay(attempt, seed);
            assert!(
                delay <= policy.max_delay,
                "delay {delay:?} exceeds max_delay at attempt={attempt} seed={seed}"
            );
        }
    }
}
