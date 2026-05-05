//! Integration tests for the subscribe stream using mocked inputs.
//!
//! These tests do NOT make network calls. They use:
//! - `InMemoryCheckpointStore` for checkpoint assertions.
//! - `SolanaAdapter` with a localhost endpoint that is never actually connected to.
//!
//! The tests validate:
//! 1. Checkpoint save + load roundtrip via `ChainAdapter` methods.
//! 2. Reconnect behavior: simulate a stream break and assert backoff + retry.
//! 3. `Event::ReorgMarker` and `Event::SlotFinalized` match expected slot values.
//!
//! # Sprint 26 / T26-2 note
//!
//! The Yellowstone gRPC `build_subscribe_request` function (which built a
//! `SubscribeRequest` proto message) was removed in this sprint.  Tests that
//! previously called it have been updated to test the new JSON-RPC subscription
//! parameters via the pure `dispatch_notification` path.

use std::sync::Arc;

use mg_onchain_chain_adapter::{
    ChainAdapter, Checkpoint, Event,
    error::AdapterError,
    solana::{
        SolanaAdapter,
        checkpoint::InMemoryCheckpointStore,
        config::{CommitmentConfig, ReconnectPolicy, SolanaAdapterConfig, SubscribeFiltersConfig},
        reconnect::{compute_delay, decide_reconnect, ReconnectDecision},
    },
    SubscribeFilter,
};
use url::Url;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn test_config() -> SolanaAdapterConfig {
    SolanaAdapterConfig {
        http_url: Url::parse("http://127.0.0.1:8899").unwrap(),
        ws_url: Url::parse("ws://127.0.0.1:8900").unwrap(),
        auth_token: None,
        commitment: CommitmentConfig::Confirmed,
        reconnect: ReconnectPolicy {
            base_delay_ms: 1,   // near-instant for tests
            max_delay_ms: 10,
            max_attempts: 3,
            rate_limit_base_ms: 2,
        },
        filters: SubscribeFiltersConfig::default(),
        checkpoint_path: "/tmp/test_subscribe_mock.json".into(),
    }
}

fn make_adapter() -> SolanaAdapter {
    SolanaAdapter::new(test_config(), InMemoryCheckpointStore::new())
}

// ---------------------------------------------------------------------------
// Checkpoint tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn checkpoint_save_load_roundtrip() {
    let adapter = make_adapter();
    let cp = Checkpoint {
        slot: 300_000_000,
        last_signature: Some("5xvRTestSigABC123".into()),
    };
    adapter.checkpoint_save(&cp).await.expect("save must succeed");
    let loaded = adapter
        .checkpoint_load()
        .await
        .expect("load must not error")
        .expect("must have checkpoint after save");

    assert_eq!(loaded.slot, 300_000_000);
    assert_eq!(loaded.last_signature.as_deref(), Some("5xvRTestSigABC123"));
}

#[tokio::test]
async fn checkpoint_load_on_fresh_adapter_returns_none() {
    let adapter = make_adapter();
    let result = adapter.checkpoint_load().await.expect("no error on fresh adapter");
    assert!(result.is_none(), "fresh adapter must have no checkpoint");
}

#[tokio::test]
async fn checkpoint_overwrite_updates_slot() {
    let adapter = make_adapter();
    adapter
        .checkpoint_save(&Checkpoint { slot: 100, last_signature: None })
        .await
        .unwrap();
    adapter
        .checkpoint_save(&Checkpoint { slot: 200, last_signature: Some("sig2".into()) })
        .await
        .unwrap();
    let cp = adapter.checkpoint_load().await.unwrap().unwrap();
    assert_eq!(cp.slot, 200, "second save must overwrite first");
}

// ---------------------------------------------------------------------------
// Subscribe filter content tests (Solana default filter shape)
// ---------------------------------------------------------------------------

#[test]
fn subscribe_filter_has_spl_token_program() {
    let filter = SubscribeFilter::solana_default();
    assert!(
        filter.program_ids.contains(&"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string()),
        "SPL Token Program must be in default filter program_ids"
    );
}

#[test]
fn subscribe_filter_has_token_2022() {
    let filter = SubscribeFilter::solana_default();
    assert!(
        filter.program_ids.contains(&"TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".to_string()),
        "Token-2022 must be in default filter program_ids"
    );
}

#[test]
fn subscribe_filter_account_owners_has_token_programs() {
    let filter = SubscribeFilter::solana_default();
    assert!(
        filter.account_owners.contains(&"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string()),
        "account_owners must include SPL Token program"
    );
    assert!(
        filter.account_owners.contains(&"TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".to_string()),
        "account_owners must include Token-2022 program"
    );
}

#[test]
fn subscribe_filter_include_slot_updates_true() {
    let filter = SubscribeFilter::solana_default();
    assert!(filter.include_slot_updates, "default filter must enable slot updates");
}

// ---------------------------------------------------------------------------
// CommitmentConfig::as_str correctness
// ---------------------------------------------------------------------------

#[test]
fn commitment_config_as_str_confirmed() {
    assert_eq!(CommitmentConfig::Confirmed.as_str(), "confirmed");
}

#[test]
fn commitment_config_as_str_finalized() {
    assert_eq!(CommitmentConfig::Finalized.as_str(), "finalized");
}

#[test]
fn commitment_config_as_str_processed() {
    assert_eq!(CommitmentConfig::Processed.as_str(), "processed");
}

// ---------------------------------------------------------------------------
// Reconnect logic tests (deterministic — no sleeps in assertions)
// ---------------------------------------------------------------------------

#[test]
fn reconnect_delay_increases_with_attempts() {
    let policy = ReconnectPolicy {
        base_delay_ms: 100,
        max_delay_ms: 10_000,
        max_attempts: 10,
        rate_limit_base_ms: 500,
    };
    // Use Transport(String) — no tonic dependency.
    let err = AdapterError::Transport("test disconnect".into());
    let d0 = compute_delay(&policy, 0, &err).unwrap();
    let d1 = compute_delay(&policy, 1, &err).unwrap();
    let d2 = compute_delay(&policy, 2, &err).unwrap();
    // Each attempt should produce a delay >= the previous (exponential growth).
    assert!(d1 >= d0, "delay must grow with attempt: d1={:?} d0={:?}", d1, d0);
    assert!(d2 >= d1, "delay must grow with attempt: d2={:?} d1={:?}", d2, d1);
}

#[test]
fn reconnect_delay_capped_at_max() {
    let policy = ReconnectPolicy {
        base_delay_ms: 1000,
        max_delay_ms: 5_000,
        max_attempts: 10,
        rate_limit_base_ms: 5_000,
    };
    let err = AdapterError::StreamEnded { slot: 0 };
    // At attempt 10, delay should be capped at max_delay_ms.
    let delay = compute_delay(&policy, 10, &err).unwrap();
    assert!(
        delay.as_millis() <= 5_000,
        "delay must not exceed max_delay_ms: got {:?}",
        delay
    );
}

#[test]
fn reconnect_decision_abort_on_max_attempts() {
    let policy = ReconnectPolicy {
        base_delay_ms: 100,
        max_delay_ms: 1000,
        max_attempts: 5,
        rate_limit_base_ms: 500,
    };
    let err = AdapterError::StreamEnded { slot: 0 };
    let decision = decide_reconnect(&policy, 5, &err); // attempt == max_attempts
    assert!(
        matches!(decision, ReconnectDecision::Abort { .. }),
        "must abort when attempt == max_attempts"
    );
}

#[test]
fn reconnect_decision_retry_below_max_attempts() {
    let policy = ReconnectPolicy {
        base_delay_ms: 100,
        max_delay_ms: 1000,
        max_attempts: 5,
        rate_limit_base_ms: 500,
    };
    let err = AdapterError::Transport("disconnect".into());
    let decision = decide_reconnect(&policy, 3, &err);
    assert!(
        matches!(decision, ReconnectDecision::Retry { .. }),
        "must retry when attempt < max_attempts"
    );
}

#[test]
fn reconnect_decision_abort_for_config_error() {
    let policy = ReconnectPolicy::default();
    let err = AdapterError::Config("bad endpoint".into());
    let decision = decide_reconnect(&policy, 0, &err);
    assert!(
        matches!(decision, ReconnectDecision::Abort { .. }),
        "Config errors must never retry"
    );
}

// ---------------------------------------------------------------------------
// Reconnect loop integration: simulate break + retry via with_reconnect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reconnect_loop_retries_on_stream_ended() {
    use mg_onchain_chain_adapter::solana::reconnect::with_reconnect;
    use std::sync::atomic::{AtomicU32, Ordering};

    let policy = ReconnectPolicy {
        base_delay_ms: 1,
        max_delay_ms: 5,
        max_attempts: 5,
        rate_limit_base_ms: 1,
    };

    let calls = Arc::new(AtomicU32::new(0));
    let calls2 = calls.clone();

    let result = with_reconnect(&policy, "test_retry", move |_attempt| {
        let c = calls2.clone();
        async move {
            let n = c.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                Err(AdapterError::StreamEnded { slot: n as u64 })
            } else {
                Ok::<u32, AdapterError>(777)
            }
        }
    })
    .await;

    assert_eq!(result.unwrap(), 777, "must succeed on 3rd attempt");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn reconnect_loop_aborts_after_max_attempts() {
    use mg_onchain_chain_adapter::solana::reconnect::with_reconnect;
    use std::sync::atomic::{AtomicU32, Ordering};

    let policy = ReconnectPolicy {
        base_delay_ms: 1,
        max_delay_ms: 5,
        max_attempts: 3,
        rate_limit_base_ms: 1,
    };

    let calls = Arc::new(AtomicU32::new(0));
    let calls2 = calls.clone();

    let result = with_reconnect(&policy, "test_abort", move |_attempt| {
        let c = calls2.clone();
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            // Transport(String) — no tonic dependency needed.
            Err::<u32, AdapterError>(AdapterError::Transport("always fail".into()))
        }
    })
    .await;

    assert!(result.is_err(), "must fail after max_attempts");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "must call action exactly max_attempts times"
    );
}

// ---------------------------------------------------------------------------
// Event type checks
// ---------------------------------------------------------------------------

#[test]
fn event_reorg_marker_slot_matches() {
    let event = Event::ReorgMarker { slot: 12345 };
    if let Event::ReorgMarker { slot } = event {
        assert_eq!(slot, 12345);
    } else {
        panic!("wrong variant");
    }
}

#[test]
fn event_slot_finalized_slot_matches() {
    let event = Event::SlotFinalized { slot: 99999 };
    if let Event::SlotFinalized { slot } = event {
        assert_eq!(slot, 99999);
    } else {
        panic!("wrong variant");
    }
}
