//! `mg-onchain-client-sdk` — thin Rust client for the MG Onchain Analysis gateway.
//!
//! # Quick start
//!
//! ```rust,no_run
//! use mg_onchain_client_sdk::{OnchainAnalysisClient, types::AnomalyFilter};
//! use mg_onchain_common::chain::Chain;
//! use std::time::Duration;
//!
//! # async fn example() -> Result<(), mg_onchain_client_sdk::error::ClientError> {
//! let client = OnchainAnalysisClient::builder()
//!     .base_url("https://gateway.internal:8080")
//!     .bearer_token("eyJ...")
//!     .timeout(Duration::from_millis(500))
//!     .build()?;
//!
//! // Trigger a full analysis run.
//! let report = client.analyze_token(Chain::Solana, "FeqiF7TEVmpYuuj4goD8WgmgyhFgFnZiWtw226wF4hNm", None).await?;
//!
//! // Read a cached/freshly-scored report.
//! let report = client.get_risk(Chain::Solana, "FeqiF7TEVmpYuuj4goD8WgmgyhFgFnZiWtw226wF4hNm").await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Modules
//!
//! - [`builder`] — fluent builder for constructing the client
//! - [`types`] — request/response types mirroring `openapi.yaml`
//! - [`error`] — `ClientError` with all error variants
//! - [`auth`] — `BearerToken` wrapper (secrecy-guarded)
//! - [`retry`] — configurable retry policy
//! - [`http`] — reqwest-backed HTTP transport (internal)
//! - [`ws`] — WebSocket client with auto-reconnect (internal)
//!
//! # Design invariants
//!
//! - No `f64` for monetary / financial amounts — uses `rust_decimal::Decimal` or
//!   `Confidence` (probability wrapper) from `mg-onchain-common`.
//! - No gateway server-side crates depended upon.
//! - No provider SDKs.
//! - Bearer token never appears in `Debug` output.
//! - SDK does not generate timestamps or random nonces in request bodies.

pub mod auth;
pub mod builder;
pub mod error;
pub mod http;
pub mod retry;
pub mod types;
pub mod ws;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tracing::instrument;
use url::Url;

use crate::auth::BearerToken;
use crate::builder::OnchainAnalysisClientBuilder;
use crate::error::ClientError;
use crate::http::HttpClient;
use crate::types::{
    AnalyzeRequest, AnalyzeResponse, AnomalyEventPage, AnomalyFilter, AuthRequest, AuthResponse,
    DetectorListResponse, EventsFilter, HealthResponse, RiskResponse, TokenRiskReport,
};
use crate::ws::{AnomalyStream, WsConfig};

use mg_onchain_common::chain::Chain;

// ---------------------------------------------------------------------------
// ClientConfig
// ---------------------------------------------------------------------------

/// Immutable configuration snapshot embedded in `OnchainAnalysisClient`.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Gateway base URL.
    pub base_url: Url,
    /// Per-request HTTP timeout.
    pub timeout: Duration,
    /// Maximum WebSocket reconnect attempts before giving up.
    pub max_reconnect_attempts: u32,
}

// ---------------------------------------------------------------------------
// OnchainAnalysisClient
// ---------------------------------------------------------------------------

/// The main client for the MG Onchain Analysis gateway.
///
/// # Cloning
///
/// `clone()` produces a new handle sharing the same HTTP connection pool and
/// bearer token. Rotating the token via `refresh_token` on any clone updates
/// all other clones.
///
/// # Debug
///
/// The `Debug` impl does not print the bearer token — it shows `[REDACTED]`.
#[derive(Clone)]
pub struct OnchainAnalysisClient {
    pub(crate) http: Arc<HttpClient>,
    pub(crate) token: Arc<RwLock<BearerToken>>,
    pub(crate) config: ClientConfig,
}

impl std::fmt::Debug for OnchainAnalysisClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnchainAnalysisClient")
            .field("base_url", &self.config.base_url.as_str())
            .field("token", &"[REDACTED]")
            .field("timeout", &self.config.timeout)
            .field("max_reconnect_attempts", &self.config.max_reconnect_attempts)
            .finish()
    }
}

impl OnchainAnalysisClient {
    /// Return a fluent builder.
    pub fn builder() -> OnchainAnalysisClientBuilder {
        OnchainAnalysisClientBuilder::default()
    }

    /// Rotate the bearer token in place.
    ///
    /// Thread-safe: all concurrent requests and WebSocket connections will pick
    /// up the new token on their next use.
    pub async fn refresh_token(&self, new_token: impl Into<String>) {
        let mut guard = self.token.write().await;
        *guard = BearerToken::new(new_token);
    }

    // -------------------------------------------------------------------------
    // Auth
    // -------------------------------------------------------------------------

    /// Exchange username + password for a JWT.
    ///
    /// `POST /v1/auth/token` — no authentication required on this endpoint.
    ///
    /// After a successful call, callers typically call `refresh_token(response.access_token)`
    /// to inject the new JWT into the client.
    #[instrument(skip(self, password), fields(username))]
    pub async fn authenticate(
        &self,
        username: &str,
        password: &str,
    ) -> Result<AuthResponse, ClientError> {
        let url = self.http.url("/v1/auth/token")?;
        let body = AuthRequest {
            username: username.to_owned(),
            password: password.to_owned(),
        };
        let response = self.http.post_json(url, &body).await?;
        response.json::<AuthResponse>().await.map_err(ClientError::Network)
    }

    // -------------------------------------------------------------------------
    // Analysis
    // -------------------------------------------------------------------------

    /// Trigger a full on-demand detector run and return the risk report.
    ///
    /// `POST /v1/tokens/analyze` — requires scope `write:analyze`.
    ///
    /// # Arguments
    ///
    /// - `chain` — the chain the token lives on.
    /// - `mint` — token address in chain-canonical form.
    /// - `window_hours` — observation window in hours (1–168). `None` defaults
    ///   to 24 hours server-side.
    ///
    /// # Errors
    ///
    /// - `AnalyzeInFlight` (409) if an analyze for the same `(chain, mint)` is
    ///   already running. Retry after ~500ms.
    /// - `RateLimited` (429) if the `write:analyze` quota is exceeded.
    #[instrument(skip(self), fields(chain = chain.as_str(), mint))]
    pub async fn analyze_token(
        &self,
        chain: Chain,
        mint: &str,
        window_hours: Option<u32>,
    ) -> Result<TokenRiskReport, ClientError> {
        let url = self.http.url("/v1/tokens/analyze")?;
        let body = AnalyzeRequest {
            chain,
            mint: mint.to_owned(),
            window_hours,
        };
        let response = self.http.post_json(url, &body).await?;
        let parsed: AnalyzeResponse =
            response.json::<AnalyzeResponse>().await.map_err(ClientError::Network)?;
        Ok(parsed.report)
    }

    /// Like `analyze_token` but returns the full `AnalyzeResponse` including
    /// `analysis_duration_ms` and `cache_age_seconds`.
    #[instrument(skip(self), fields(chain = chain.as_str(), mint))]
    pub async fn analyze_token_full(
        &self,
        chain: Chain,
        mint: &str,
        window_hours: Option<u32>,
    ) -> Result<AnalyzeResponse, ClientError> {
        let url = self.http.url("/v1/tokens/analyze")?;
        let body = AnalyzeRequest {
            chain,
            mint: mint.to_owned(),
            window_hours,
        };
        let response = self.http.post_json(url, &body).await?;
        response.json::<AnalyzeResponse>().await.map_err(ClientError::Network)
    }

    // -------------------------------------------------------------------------
    // Risk (cached read)
    // -------------------------------------------------------------------------

    /// Return a cached or freshly-scored risk report without running detectors.
    ///
    /// `GET /v1/tokens/{chain}/{mint}/risk` — requires scope `read:risk`.
    ///
    /// Faster than `analyze_token` on cache hit (<5ms). On cache miss the
    /// gateway queries Postgres and runs scoring synchronously (<100ms).
    ///
    /// Returns `NotFound` if the token is not known to the gateway. Use
    /// `analyze_token` to create a report for an unknown token.
    #[instrument(skip(self), fields(chain = chain.as_str(), mint))]
    pub async fn get_risk(
        &self,
        chain: Chain,
        mint: &str,
    ) -> Result<TokenRiskReport, ClientError> {
        let path = format!("/v1/tokens/{}/{}/risk", chain.as_str(), mint);
        let url = self.http.url(&path)?;
        let response = self.http.get(url).await?;
        let parsed: RiskResponse =
            response.json::<RiskResponse>().await.map_err(ClientError::Network)?;
        Ok(parsed.report)
    }

    /// Like `get_risk` but returns the full `RiskResponse` with `cached` and
    /// `cache_age_seconds` metadata.
    #[instrument(skip(self), fields(chain = chain.as_str(), mint))]
    pub async fn get_risk_full(
        &self,
        chain: Chain,
        mint: &str,
    ) -> Result<RiskResponse, ClientError> {
        let path = format!("/v1/tokens/{}/{}/risk", chain.as_str(), mint);
        let url = self.http.url(&path)?;
        let response = self.http.get(url).await?;
        response.json::<RiskResponse>().await.map_err(ClientError::Network)
    }

    // -------------------------------------------------------------------------
    // Anomaly events (paginated)
    // -------------------------------------------------------------------------

    /// Return one page of historical anomaly events matching the given filter.
    ///
    /// `GET /v1/anomaly_events` — requires scope `read:events`.
    ///
    /// Iterate via `next_cursor`:
    ///
    /// ```rust,no_run
    /// # use mg_onchain_client_sdk::{OnchainAnalysisClient, types::EventsFilter};
    /// # async fn example(client: OnchainAnalysisClient) -> Result<(), mg_onchain_client_sdk::error::ClientError> {
    /// let mut cursor = None;
    /// loop {
    ///     let page = client.list_anomaly_events(EventsFilter {
    ///         limit: Some(100),
    ///         cursor: cursor.clone(),
    ///         ..Default::default()
    ///     }).await?;
    ///     // process page.events ...
    ///     cursor = page.next_cursor;
    ///     if cursor.is_none() { break; }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[instrument(skip(self))]
    pub async fn list_anomaly_events(
        &self,
        filter: EventsFilter,
    ) -> Result<AnomalyEventPage, ClientError> {
        let mut params: Vec<(String, String)> = Vec::new();

        if let Some(c) = &filter.chain {
            params.push(("chain".into(), c.as_str().to_owned()));
        }
        if let Some(t) = &filter.token {
            params.push(("token".into(), t.clone()));
        }
        if let Some(d) = &filter.detector_id {
            params.push(("detector_id".into(), d.clone()));
        }
        if let Some(s) = &filter.severity_min {
            params.push(("severity_min".into(), severity_param_str(*s).to_owned()));
        }
        if let Some(from) = filter.from {
            params.push(("from".into(), from.to_rfc3339()));
        }
        if let Some(to) = filter.to {
            params.push(("to".into(), to.to_rfc3339()));
        }
        if let Some(limit) = filter.limit {
            params.push(("limit".into(), limit.to_string()));
        }
        if let Some(cursor) = &filter.cursor {
            params.push(("cursor".into(), cursor.clone()));
        }

        let params_ref: Vec<(&str, &str)> = params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let url = self.http.url_with_query("/v1/anomaly_events", params_ref)?;
        let response = self.http.get(url).await?;
        response.json::<AnomalyEventPage>().await.map_err(ClientError::Network)
    }

    // -------------------------------------------------------------------------
    // Detector introspection
    // -------------------------------------------------------------------------

    /// List all configured detectors and their thresholds.
    ///
    /// `GET /v1/detectors` — requires scope `read:events`.
    ///
    /// Reads in-memory config on the gateway side — no database query.
    #[instrument(skip(self))]
    pub async fn list_detectors(&self) -> Result<DetectorListResponse, ClientError> {
        let url = self.http.url("/v1/detectors")?;
        let response = self.http.get(url).await?;
        response.json::<DetectorListResponse>().await.map_err(ClientError::Network)
    }

    // -------------------------------------------------------------------------
    // WebSocket streaming
    // -------------------------------------------------------------------------

    /// Subscribe to a real-time stream of anomaly events.
    ///
    /// `GET /v1/ws/stream` — requires scope `read:events`.
    ///
    /// Returns an `AnomalyStream` that yields `StreamMessage` values. The stream
    /// auto-reconnects on disconnect, up to `max_reconnect_attempts`. See
    /// `StreamMessage` for the full message taxonomy.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use mg_onchain_client_sdk::{OnchainAnalysisClient, types::{AnomalyFilter, StreamMessage}};
    /// # use mg_onchain_common::anomaly::Severity;
    /// # use mg_onchain_common::chain::Chain;
    /// # async fn example(client: OnchainAnalysisClient) -> Result<(), mg_onchain_client_sdk::error::ClientError> {
    /// let mut stream = client.subscribe_anomalies(AnomalyFilter {
    ///     chain: Some(Chain::Solana),
    ///     severity_min: Some(Severity::Medium),
    ///     ..Default::default()
    /// }).await?;
    ///
    /// while let Some(msg) = stream.next().await {
    ///     match msg? {
    ///         StreamMessage::Anomaly(event) => { /* handle */ }
    ///         StreamMessage::LagNotice { dropped } => tracing::warn!(dropped, "lagging"),
    ///         StreamMessage::Reconnected => tracing::info!("reconnected"),
    ///         _ => {}
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn subscribe_anomalies(
        &self,
        filter: AnomalyFilter,
    ) -> Result<AnomalyStream, ClientError> {
        let ws_http_url = self.http.url("/v1/ws/stream")?;

        let config = WsConfig {
            ws_url: ws_http_url,
            token: self.token.clone(),
            filter,
            max_reconnect_attempts: self.config.max_reconnect_attempts,
        };

        Ok(ws::spawn_ws_stream(config))
    }

    // -------------------------------------------------------------------------
    // Admin
    // -------------------------------------------------------------------------

    /// Invalidate the cached `TokenRiskReport` for a token.
    ///
    /// `DELETE /v1/admin/cache/{chain}/{mint}` — requires scope `admin`.
    ///
    /// Returns `true` if a cache entry was removed, `false` if it was already absent.
    #[instrument(skip(self), fields(chain = chain.as_str(), mint))]
    pub async fn invalidate_cache(
        &self,
        chain: Chain,
        mint: &str,
    ) -> Result<bool, ClientError> {
        let path = format!("/v1/admin/cache/{}/{}", chain.as_str(), mint);
        let url = self.http.url(&path)?;
        let response = self.http.delete(url).await?;
        let body: serde_json::Value =
            response.json().await.map_err(ClientError::Network)?;
        Ok(body
            .get("invalidated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }

    // -------------------------------------------------------------------------
    // Health
    // -------------------------------------------------------------------------

    /// Check gateway health.
    ///
    /// `GET /health` — no authentication required.
    #[instrument(skip(self))]
    pub async fn health(&self) -> Result<HealthResponse, ClientError> {
        let url = self.http.url("/health")?;
        let response = self.http.get(url).await?;
        response.json::<HealthResponse>().await.map_err(ClientError::Network)
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn severity_param_str(s: types::Severity) -> &'static str {
    use types::Severity;
    match s {
        Severity::Info => "info",
        Severity::Low => "low",
        Severity::Medium => "medium",
        Severity::High => "high",
        Severity::Critical => "critical",
        _ => "info",
    }
}

/// Shared jitter helper used by retry and WS reconnect modules.
pub(crate) fn retry_jitter(seed: u64, cap: u64) -> u64 {
    if cap == 0 {
        return 0;
    }
    let mut x = seed;
    if x == 0 {
        x = 0xDEAD_BEEF;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x % cap
}
