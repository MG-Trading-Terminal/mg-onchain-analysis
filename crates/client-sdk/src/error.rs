//! `ClientError` — all errors the SDK can surface to callers.
//!
//! `#[non_exhaustive]` prevents match-exhaustiveness breaks when new variants
//! are added in future SDK versions.
//!
//! # Error philosophy
//!
//! - Transport failures → `Network` or `Timeout`.
//! - Semantic HTTP errors → dedicated variants so callers can match and recover.
//! - Generic server problems → `ServerError` (5xx) or `ProblemDetail` (RFC 7807).
//! - WebSocket lifecycle → `WebSocketClosed` with optional close code.
//! - JSON parse failures → `SerdeError`.

use std::time::Duration;

use thiserror::Error;

/// RFC 7807 problem-detail payload returned by the gateway on error responses.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProblemDetail {
    /// URI identifying the problem type.
    #[serde(rename = "type")]
    pub problem_type: String,
    /// Short, human-readable summary.
    pub title: String,
    /// HTTP status code.
    pub status: u16,
    /// Human-readable explanation for this occurrence.
    pub detail: String,
    /// URI of the endpoint that generated the error.
    pub instance: String,
    /// Request trace ID for server-side log correlation.
    pub trace_id: Option<String>,
}

/// All errors the SDK can return to callers.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClientError {
    /// Transport-level failure (connection refused, DNS resolution failure, etc.).
    #[error("network error: {0}")]
    Network(#[source] reqwest::Error),

    /// The configured request timeout was exceeded before a response arrived.
    #[error("request timed out")]
    Timeout,

    /// HTTP 429 — rate limit exceeded. The `retry_after` field, when present,
    /// is parsed from the `Retry-After` response header (seconds).
    #[error("rate limited; retry after {retry_after:?}")]
    RateLimited { retry_after: Option<Duration> },

    /// HTTP 401 — missing, expired, or unparseable JWT.
    #[error("unauthenticated: {detail}")]
    Unauthenticated { detail: String },

    /// HTTP 403 — valid JWT but missing required scope.
    #[error("unauthorized (insufficient scope): {detail}")]
    Unauthorized { detail: String },

    /// HTTP 404 — the requested resource does not exist.
    #[error("not found: {resource}")]
    NotFound { resource: String },

    /// HTTP 400 — request was syntactically or semantically malformed.
    #[error("invalid input — {field}: {detail}")]
    InvalidInput { field: String, detail: String },

    /// HTTP 409 — analyze already in flight for the same (chain, mint).
    #[error("analyze already in flight: {detail}")]
    AnalyzeInFlight { detail: String },

    /// HTTP 5xx — unexpected server-side error.
    #[error("server error {status}: {detail}")]
    ServerError { status: u16, detail: String },

    /// Generic RFC 7807 payload — fallback for any structured problem-detail
    /// that does not map to a more specific variant above.
    #[error("problem detail (type={}, status={}): {}", .0.problem_type, .0.status, .0.detail)]
    ProblemDetail(Box<ProblemDetail>),

    /// WebSocket connection was closed (either by the server or network).
    #[error("websocket closed (code={code:?}): {reason}")]
    WebSocketClosed { code: Option<u16>, reason: String },

    /// JSON serialisation / deserialisation failure.
    #[error("serde error: {0}")]
    SerdeError(#[source] serde_json::Error),

    /// URL construction error (invalid base_url, bad path parameter, etc.).
    #[error("URL error: {0}")]
    UrlError(#[source] url::ParseError),

    /// WebSocket protocol-level error from tokio-tungstenite.
    #[error("websocket protocol error: {0}")]
    WebSocketProtocol(#[source] tokio_tungstenite::tungstenite::Error),
}

impl From<serde_json::Error> for ClientError {
    fn from(e: serde_json::Error) -> Self {
        ClientError::SerdeError(e)
    }
}

impl From<url::ParseError> for ClientError {
    fn from(e: url::ParseError) -> Self {
        ClientError::UrlError(e)
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for ClientError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        use tokio_tungstenite::tungstenite::Error as TtError;
        match e {
            TtError::ConnectionClosed => ClientError::WebSocketClosed {
                code: None,
                reason: "connection closed".into(),
            },
            TtError::AlreadyClosed => ClientError::WebSocketClosed {
                code: None,
                reason: "already closed".into(),
            },
            other => ClientError::WebSocketProtocol(other),
        }
    }
}
