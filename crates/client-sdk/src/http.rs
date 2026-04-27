//! reqwest-backed HTTP client with retry, auth, and error mapping.
//!
//! # Error mapping
//!
//! HTTP responses are mapped to `ClientError` variants based on status code:
//!
//! | Status | ClientError variant |
//! |--------|---------------------|
//! | 400    | `InvalidInput`      |
//! | 401    | `Unauthenticated`   |
//! | 403    | `Unauthorized`      |
//! | 404    | `NotFound`          |
//! | 409    | `AnalyzeInFlight`   |
//! | 429    | `RateLimited`       |
//! | 5xx    | `ServerError`       |
//! | other  | `ProblemDetail`     |
//!
//! # Retry
//!
//! Transient errors (429, 500, 502, 503, 504, network timeout) are retried per
//! the `RetryPolicy`. 429 responses honour the `Retry-After` header (seconds).
//!
//! # Instrumentation
//!
//! Every HTTP request logs at DEBUG with `method`, `url`, `status`, and
//! `attempt` fields via `tracing`.

use std::sync::Arc;
use std::time::Duration;

use reqwest::{Method, RequestBuilder, Response, StatusCode};
use tokio::sync::RwLock;
use tracing::{debug, instrument, warn};
use url::Url;

use crate::auth::BearerToken;
use crate::error::{ClientError, ProblemDetail};
use crate::retry::RetryPolicy;

/// Inner HTTP transport. Shared via `Arc` across the public client and clones.
#[derive(Debug)]
pub(crate) struct HttpClient {
    pub(crate) inner: reqwest::Client,
    pub(crate) base_url: Url,
    pub(crate) token: Arc<RwLock<BearerToken>>,
    pub(crate) retry: RetryPolicy,
}

impl HttpClient {
    /// Perform a GET request, with retry on transient errors.
    #[instrument(skip(self), fields(url = %url))]
    pub async fn get(&self, url: Url) -> Result<Response, ClientError> {
        self.execute_with_retry(Method::GET, url, |rb| rb).await
    }

    /// Perform a DELETE request, with retry on transient errors.
    #[instrument(skip(self), fields(url = %url))]
    pub async fn delete(&self, url: Url) -> Result<Response, ClientError> {
        self.execute_with_retry(Method::DELETE, url, |rb| rb).await
    }

    /// Perform a POST request with a JSON body, with retry on transient errors.
    #[instrument(skip(self, body), fields(url = %url))]
    pub async fn post_json<B: serde::Serialize>(
        &self,
        url: Url,
        body: &B,
    ) -> Result<Response, ClientError> {
        let body_bytes = serde_json::to_vec(body)?;
        self.execute_with_retry(Method::POST, url, move |rb| {
            rb.header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body_bytes.clone())
        })
        .await
    }

    /// Core retry loop.
    ///
    /// `modifier` receives a `RequestBuilder` after auth headers are injected and
    /// can add body / extra headers. The builder is re-created on each attempt
    /// because `reqwest::RequestBuilder` is consumed by `send()`.
    async fn execute_with_retry<F>(
        &self,
        method: Method,
        url: Url,
        modifier: F,
    ) -> Result<Response, ClientError>
    where
        F: Fn(RequestBuilder) -> RequestBuilder + Send + Sync,
    {
        let mut last_error: Option<ClientError> = None;

        for attempt in 0..self.retry.max_attempts {
            // Apply backoff delay before retries (not before the first attempt).
            if attempt > 0 {
                let seed = u64::from(attempt) ^ (url.as_str().len() as u64).wrapping_mul(0x9e37);
                let delay = match &last_error {
                    Some(ClientError::RateLimited { retry_after: Some(ra) }) => *ra,
                    _ => self.retry.backoff_delay(attempt - 1, seed),
                };
                debug!(attempt, ?delay, "retrying after backoff");
                tokio::time::sleep(delay).await;
            }

            let auth_header = {
                let tok = self.token.read().await;
                tok.header_value()
            };

            let rb = self
                .inner
                .request(method.clone(), url.clone())
                .header(reqwest::header::AUTHORIZATION, &auth_header);

            let rb = modifier(rb);

            let response = match rb.send().await {
                Ok(r) => r,
                Err(e) if e.is_timeout() => {
                    warn!(attempt, "request timed out");
                    last_error = Some(ClientError::Timeout);
                    continue; // retry on timeout
                }
                Err(e) if e.is_connect() || e.is_request() => {
                    warn!(attempt, error = %e, "network error");
                    last_error = Some(ClientError::Network(e));
                    continue; // retry on network failure
                }
                Err(e) => {
                    // Non-retriable reqwest error (e.g. redirect loop).
                    return Err(ClientError::Network(e));
                }
            };

            let status = response.status();
            debug!(attempt, status = status.as_u16(), url = %url, "HTTP response");

            if status.is_success() {
                return Ok(response);
            }

            // Capture Retry-After before consuming the response body.
            let retry_after_hint = if status == StatusCode::TOO_MANY_REQUESTS {
                parse_retry_after(response.headers())
            } else {
                None
            };

            // Map error statuses.
            let mut err = map_error_response(response).await;

            // Inject Retry-After into the RateLimited variant.
            if let ClientError::RateLimited { retry_after: ref mut ra } = err {
                *ra = retry_after_hint;
            }

            if RetryPolicy::is_retriable_status(status.as_u16()) {
                last_error = Some(err);
                continue;
            }

            // Non-retriable — return immediately.
            return Err(err);
        }

        Err(last_error.unwrap_or(ClientError::Timeout))
    }

    /// Build a URL by appending a path to `base_url`.
    ///
    /// Returns `Err(UrlError)` if the resulting URL is invalid.
    pub fn url(&self, path: &str) -> Result<Url, ClientError> {
        self.base_url
            .join(path)
            .map_err(ClientError::UrlError)
    }

    /// Build a URL with query parameters.
    pub fn url_with_query<I, K, V>(&self, path: &str, params: I) -> Result<Url, ClientError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut url = self.url(path)?;
        for (k, v) in params {
            url.query_pairs_mut().append_pair(k.as_ref(), v.as_ref());
        }
        Ok(url)
    }
}

/// Attempt to parse a RFC 7807 problem-detail body, then map to `ClientError`.
async fn map_error_response(response: Response) -> ClientError {
    let status = response.status();
    let status_u16 = status.as_u16();

    // Try to parse RFC 7807 body first.
    let body = response.bytes().await.unwrap_or_default();
    let problem: Option<ProblemDetail> = serde_json::from_slice(&body).ok();

    let detail = problem
        .as_ref()
        .map(|p| p.detail.clone())
        .unwrap_or_else(|| {
            String::from_utf8_lossy(&body)
                .chars()
                .take(256)
                .collect()
        });

    match status {
        StatusCode::BAD_REQUEST => ClientError::InvalidInput {
            field: "request".into(),
            detail,
        },
        StatusCode::UNAUTHORIZED => ClientError::Unauthenticated { detail },
        StatusCode::FORBIDDEN => ClientError::Unauthorized { detail },
        StatusCode::NOT_FOUND => ClientError::NotFound { resource: detail },
        StatusCode::CONFLICT => ClientError::AnalyzeInFlight { detail },
        StatusCode::TOO_MANY_REQUESTS => {
            // Parse Retry-After header (seconds).
            ClientError::RateLimited {
                retry_after: None, // Retry-After not accessible from consumed body; caller sets it
            }
        }
        s if s.is_server_error() => ClientError::ServerError {
            status: status_u16,
            detail,
        },
        _ => {
            if let Some(p) = problem {
                ClientError::ProblemDetail(Box::new(p))
            } else {
                ClientError::ServerError {
                    status: status_u16,
                    detail,
                }
            }
        }
    }
}

/// Parse the `Retry-After` header from a raw reqwest response as `Duration`.
///
/// The gateway sends seconds (an integer). Falls back to `None` if absent or
/// unparseable.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
}
