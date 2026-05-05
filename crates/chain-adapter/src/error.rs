//! `AdapterError` — typed error hierarchy for all chain adapter operations.
//!
//! Design goals:
//! - Every decode failure is `DecodeError` (logged + skipped in the hot path — never panics).
//! - Every connectivity failure is `Transport` (triggers reconnect logic in `reconnect.rs`).
//! - `RateLimit` is distinct from generic transport so the reconnect loop can apply a longer
//!   backoff rather than an exponential-capped retry.
//! - Checkpoint failures are `Checkpoint` — they are non-fatal for the stream but must be
//!   logged at `ERROR` level because they risk losing resume position.

use thiserror::Error;

/// Top-level error type returned by all chain adapter operations.
///
/// Callers in `subscribe.rs` and `backfill.rs` pattern-match on this to decide whether to:
/// - log-and-skip (`DecodeError`, `MissingField`) — the event is malformed; move on.
/// - reconnect (`Transport`, `StreamEnded`) — the connection dropped; apply backoff.
/// - wait-and-retry with longer backoff (`RateLimit`).
/// - log at ERROR level (`Checkpoint`) — persist later, keep streaming.
#[derive(Debug, Error)]
pub enum AdapterError {
    // --- Connectivity ---
    /// A WebSocket or HTTP transport error (connection dropped, timeout, etc.).
    /// Triggers the reconnect loop in `reconnect.rs`.
    #[error("transport error: {0}")]
    Transport(String),

    /// A JSON-RPC or WebSocket client error (request-level, not stream-level).
    /// Treated as reconnectable: callers should apply backoff and retry.
    #[error("JSON-RPC client error: {0}")]
    GrpcClient(String),

    /// The underlying stream ended unexpectedly (server-side close without error).
    #[error("stream ended unexpectedly (slot={slot})")]
    StreamEnded { slot: u64 },

    /// Provider returned a rate-limit response (HTTP 429 or equivalent).
    ///
    /// The reconnect loop applies `reconnect_policy.rate_limit_base_ms` × 2^attempts backoff
    /// instead of the normal `base_delay_ms` × 2^attempts to avoid hammering the endpoint.
    #[error("rate limited by provider (slot={slot})")]
    RateLimit { slot: u64 },

    // --- Decoding ---
    /// A required field was absent from the response payload.
    ///
    /// This error causes the single event to be skipped; the stream continues.
    #[error("missing required field '{field}' in {context}")]
    MissingField {
        field: &'static str,
        context: &'static str,
    },

    /// General decode failure — invalid bytes, unexpected enum variant, etc.
    ///
    /// Logs the inner message and skips the event. The stream continues.
    #[error("decode error in {context}: {reason}")]
    DecodeError {
        context: &'static str,
        reason: String,
    },

    /// A `u128` overflow occurred converting a raw amount.
    ///
    /// Solana SPL amounts fit in `u64`; this fires only for pathological Token-2022
    /// values or encoding bugs. Logged and skipped.
    #[error("amount overflow in {context}: value={value}")]
    AmountOverflow { context: &'static str, value: u64 },

    // --- Checkpoint ---
    /// Failed to persist the checkpoint. The stream continues but may re-process
    /// events on restart. Logged at ERROR level.
    #[error("checkpoint write failed: {0}")]
    Checkpoint(String),

    // --- Config ---
    /// Invalid adapter configuration detected at startup.
    #[error("invalid adapter config: {0}")]
    Config(String),

    // --- Backfill ---
    /// Solana JSON-RPC returned an error during backfill.
    #[error("RPC error during backfill (slot={slot}): {reason}")]
    RpcError { slot: u64, reason: String },

    // --- eth_call ---
    /// An EVM `eth_call` reverted on-chain.
    ///
    /// `reason` is the ABI-decoded revert message (if available) or a hex-encoded
    /// revert payload (if the revert reason is not a standard string). An empty
    /// `reason` means the call reverted with no data (e.g. `require(false)`).
    ///
    /// Distinct from `RpcError` so detectors can pattern-match on this specific
    /// case without parsing string error messages.
    #[error("eth_call reverted: {reason}")]
    CallReverted { reason: String },
}

impl AdapterError {
    /// Returns `true` if this error indicates the stream should reconnect.
    pub fn is_reconnectable(&self) -> bool {
        matches!(
            self,
            AdapterError::Transport(_)
                | AdapterError::GrpcClient(_)
                | AdapterError::StreamEnded { .. }
                | AdapterError::RateLimit { .. }
        )
    }

    /// Returns `true` if this error should cause the event to be skipped but the stream to continue.
    pub fn is_skippable(&self) -> bool {
        matches!(
            self,
            AdapterError::MissingField { .. }
                | AdapterError::DecodeError { .. }
                | AdapterError::AmountOverflow { .. }
        )
    }

    /// Returns `true` if this is a rate-limit error (needs extended backoff).
    pub fn is_rate_limit(&self) -> bool {
        matches!(self, AdapterError::RateLimit { .. })
    }
}
