//! `OnchainAnalysisClientBuilder` — fluent builder for constructing
//! `OnchainAnalysisClient`.
//!
//! # Required fields
//!
//! - `base_url` — HTTP base URL of the gateway (e.g. `"https://gateway.internal:8080"`).
//!   Must be a valid URL; an `Err` is returned from `build()` if it is not.
//!
//! # Optional fields
//!
//! - `bearer_token` — pre-existing JWT. If not set, the client works but all
//!   authenticated endpoints will fail with `Unauthenticated`. Call
//!   `authenticate(username, password)` after building to obtain a token.
//! - `timeout` — per-request timeout. Defaults to 10 seconds.
//! - `max_reconnect_attempts` — max WS reconnect retries. Defaults to 10.
//!
//! # Example
//!
//! ```rust,no_run
//! use mg_onchain_client_sdk::{OnchainAnalysisClient, ClientConfig};
//! use std::time::Duration;
//!
//! let client = OnchainAnalysisClient::builder()
//!     .base_url("https://gateway.internal:8080")
//!     .bearer_token("eyJ...")
//!     .timeout(Duration::from_millis(500))
//!     .build()
//!     .unwrap();
//! ```

use std::time::Duration;

use url::Url;

use crate::auth::BearerToken;
use crate::error::ClientError;
use crate::retry::RetryPolicy;
use crate::{ClientConfig, OnchainAnalysisClient};

/// Fluent builder for `OnchainAnalysisClient`.
#[derive(Debug, Default)]
pub struct OnchainAnalysisClientBuilder {
    base_url: Option<String>,
    bearer_token: Option<String>,
    timeout: Option<Duration>,
    max_reconnect_attempts: Option<u32>,
    retry_policy: Option<RetryPolicy>,
}

impl OnchainAnalysisClientBuilder {
    /// Set the gateway base URL.
    ///
    /// Must be an `http://` or `https://` URL. The path component, if any, is
    /// used as a prefix for all endpoint paths.
    ///
    /// Example: `"https://gateway.internal:8080"` or `"http://localhost:8080"`.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// Set the JWT bearer token for authentication.
    ///
    /// The token is wrapped in a `secrecy::SecretString` internally — it will
    /// not appear in `Debug` output on the client struct.
    pub fn bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }

    /// Set the per-request HTTP timeout. Default 10 seconds.
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = Some(t);
        self
    }

    /// Set maximum WebSocket reconnect attempts. Default 10.
    ///
    /// After this many failed reconnect attempts the `AnomalyStream` closes
    /// with a `WebSocketClosed` error.
    pub fn max_reconnect_attempts(mut self, n: u32) -> Self {
        self.max_reconnect_attempts = Some(n);
        self
    }

    /// Override the default retry policy (max_attempts=3, base 200ms, cap 8s).
    pub fn retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = Some(policy);
        self
    }

    /// Build the client.
    ///
    /// Returns `Err(ClientError::UrlError)` if `base_url` is absent or invalid.
    pub fn build(self) -> Result<OnchainAnalysisClient, ClientError> {
        let raw_url = self
            .base_url
            .ok_or(ClientError::InvalidInput {
                field: "base_url".into(),
                detail: "base_url is required".into(),
            })?;

        let base_url: Url = raw_url.parse().map_err(ClientError::UrlError)?;

        let timeout = self.timeout.unwrap_or(Duration::from_secs(10));
        let retry = self.retry_policy.unwrap_or_default();
        let max_reconnect_attempts = self
            .max_reconnect_attempts
            .unwrap_or(crate::ws::DEFAULT_MAX_RECONNECT_ATTEMPTS);

        let token = BearerToken::new(self.bearer_token.unwrap_or_default());

        let config = ClientConfig {
            base_url: base_url.clone(),
            timeout,
            max_reconnect_attempts,
        };

        let reqwest_client = reqwest::Client::builder()
            .timeout(timeout)
            .use_rustls_tls()
            .build()
            .map_err(|e| ClientError::InvalidInput {
                field: "reqwest_client".into(),
                detail: e.to_string(),
            })?;

        let token = std::sync::Arc::new(tokio::sync::RwLock::new(token));

        let http = crate::http::HttpClient {
            inner: reqwest_client,
            base_url,
            token: token.clone(),
            retry,
        };

        Ok(OnchainAnalysisClient {
            http: std::sync::Arc::new(http),
            token,
            config,
        })
    }
}
