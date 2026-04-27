//! `GatewayConfig` — full configuration for the HTTP + WebSocket gateway.
//!
//! Loaded from `config/gateway.toml` at startup. All runtime-reloadable fields are
//! documented inline. Secret-bearing fields (jwt_signing_key_path content) are
//! loaded separately and never logged.

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

/// Full gateway configuration, loaded from `config/gateway.toml`.
///
/// # Secret hygiene
///
/// `GatewayConfig` does NOT implement `Debug` automatically — it is intentionally
/// absent to prevent accidental logging of the `auth` sub-config path contents.
/// Use the explicit display below.
#[derive(Clone, Deserialize)]
pub struct GatewayConfig {
    pub gateway: GatewayInner,
}

impl GatewayConfig {
    /// Load from a TOML string (for tests / programmatic construction).
    pub fn from_toml(s: &str) -> anyhow::Result<Self> {
        toml::from_str(s).map_err(|e| anyhow::anyhow!("gateway config parse error: {e}"))
    }

    /// Load from a file path.
    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read gateway config {path}: {e}"))?;
        Self::from_toml(&s)
    }
}

/// Implements a redacted Debug that never prints key paths or sensitive fields.
impl std::fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("bind_address", &self.gateway.bind_address)
            .field("auth", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Deserialize)]
pub struct GatewayInner {
    /// TCP bind address, e.g. `"0.0.0.0:8080"`.
    pub bind_address: String,

    /// Seconds to wait for in-flight requests on SIGTERM.
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout_seconds: u64,

    /// Retry attempts for initial DB connection at startup.
    #[serde(default = "default_db_connect_retries")]
    pub db_connect_retries: u32,

    pub auth: AuthConfig,
    pub ratelimit: RateLimitConfig,
    pub cache: CacheConfig,
    pub ws: WsConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
}

fn default_shutdown_timeout() -> u64 { 30 }
fn default_db_connect_retries() -> u32 { 5 }

// ---------------------------------------------------------------------------
// Auth config
// ---------------------------------------------------------------------------

/// Authentication configuration.
///
/// # Secret hygiene
///
/// `jwt_signing_key_path` contains a filesystem path, not the key bytes themselves.
/// Key bytes are loaded at startup and stored in `AppState.jwt_keys` — never in config.
#[derive(Clone, Deserialize)]
pub struct AuthConfig {
    /// Path to the Ed25519 private key PEM file.
    /// Generate: `openssl genpkey -algorithm ed25519 -out priv.ed25519`
    pub jwt_signing_key_path: String,

    /// JWT `iss` claim.
    #[serde(default = "default_jwt_issuer")]
    pub jwt_issuer: String,

    /// JWT `aud` claim.
    #[serde(default = "default_jwt_audience")]
    pub jwt_audience: String,

    /// Token lifetime in hours.
    #[serde(default = "default_jwt_expiry_hours")]
    pub jwt_expiry_hours: u64,

    /// Argon2id parameters for password hashing.
    #[serde(default)]
    pub argon2_params: Argon2Params,
}

fn default_jwt_issuer() -> String { "mg-onchain".to_string() }
fn default_jwt_audience() -> String { "mg-onchain-api".to_string() }
fn default_jwt_expiry_hours() -> u64 { 24 }

/// Argon2id password hashing parameters.
/// Defaults meet OWASP 2024 recommendations for interactive logins.
#[derive(Clone, Deserialize)]
pub struct Argon2Params {
    /// Memory cost in KiB. Default 65536 (64 MiB).
    #[serde(default = "default_memory_kib")]
    pub memory_kib: u32,
    /// Iteration count. Default 3.
    #[serde(default = "default_iterations")]
    pub iterations: u32,
    /// Parallelism degree. Default 4.
    #[serde(default = "default_parallelism")]
    pub parallelism: u32,
}

impl Default for Argon2Params {
    fn default() -> Self {
        Self {
            memory_kib: default_memory_kib(),
            iterations: default_iterations(),
            parallelism: default_parallelism(),
        }
    }
}

fn default_memory_kib() -> u32 { 65536 }
fn default_iterations() -> u32 { 3 }
fn default_parallelism() -> u32 { 4 }

// ---------------------------------------------------------------------------
// Rate-limit config
// ---------------------------------------------------------------------------

#[derive(Clone, Deserialize)]
pub struct RateLimitConfig {
    /// Requests per minute per authenticated subject (default bucket).
    #[serde(default = "default_default_rpm")]
    pub default_rpm: u32,

    /// RPM for `POST /v1/tokens/analyze` (expensive — runs all detectors).
    #[serde(default = "default_write_analyze_rpm")]
    pub write_analyze_rpm: u32,

    /// Max concurrent WebSocket connections per subject.
    #[serde(default = "default_ws_connections_per_subject")]
    pub ws_connections_per_subject: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            default_rpm: default_default_rpm(),
            write_analyze_rpm: default_write_analyze_rpm(),
            ws_connections_per_subject: default_ws_connections_per_subject(),
        }
    }
}

fn default_default_rpm() -> u32 { 60 }
fn default_write_analyze_rpm() -> u32 { 10 }
fn default_ws_connections_per_subject() -> u32 { 5 }

// ---------------------------------------------------------------------------
// Cache config
// ---------------------------------------------------------------------------

#[derive(Clone, Deserialize)]
pub struct CacheConfig {
    /// TTL for `TokenRiskReport` cache entries, in seconds.
    #[serde(default = "default_token_risk_ttl")]
    pub token_risk_ttl_seconds: u64,

    /// Maximum number of entries in the `TokenRiskReport` cache.
    #[serde(default = "default_token_risk_max_entries")]
    pub token_risk_max_entries: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            token_risk_ttl_seconds: default_token_risk_ttl(),
            token_risk_max_entries: default_token_risk_max_entries(),
        }
    }
}

fn default_token_risk_ttl() -> u64 { 60 }
fn default_token_risk_max_entries() -> u64 { 10_000 }

// ---------------------------------------------------------------------------
// WebSocket config
// ---------------------------------------------------------------------------

#[derive(Clone, Deserialize)]
pub struct WsConfig {
    /// Server sends ping every N seconds.
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_seconds: u64,

    /// Client must pong within N seconds or be disconnected.
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout_seconds: u64,

    /// Max subscriptions per connection.
    #[serde(default = "default_max_subscriptions")]
    pub max_subscriptions_per_connection: usize,

    /// Per-subscriber send buffer capacity (messages).
    #[serde(default = "default_send_buffer_capacity")]
    pub send_buffer_capacity: usize,

    /// Only push `TokenRiskReport` when score delta exceeds this value.
    #[serde(default = "default_report_delta_threshold")]
    pub ws_report_delta_threshold: f64,

    /// Max event replay lookback window on reconnect (minutes).
    #[serde(default = "default_replay_lookback_minutes")]
    pub replay_lookback_minutes: u64,

    /// Broadcast channel capacity (gateway-wide).
    #[serde(default = "default_broadcast_channel_capacity")]
    pub broadcast_channel_capacity: usize,

    /// Send lag_notice after this many dropped events.
    #[serde(default = "default_lag_notice_threshold")]
    pub lag_notice_threshold: usize,

    /// Polling interval for new events (milliseconds).
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval_seconds: default_heartbeat_interval(),
            heartbeat_timeout_seconds: default_heartbeat_timeout(),
            max_subscriptions_per_connection: default_max_subscriptions(),
            send_buffer_capacity: default_send_buffer_capacity(),
            ws_report_delta_threshold: default_report_delta_threshold(),
            replay_lookback_minutes: default_replay_lookback_minutes(),
            broadcast_channel_capacity: default_broadcast_channel_capacity(),
            lag_notice_threshold: default_lag_notice_threshold(),
            poll_interval_ms: default_poll_interval_ms(),
        }
    }
}

fn default_heartbeat_interval() -> u64 { 30 }
fn default_heartbeat_timeout() -> u64 { 60 }
fn default_max_subscriptions() -> usize { 100 }
fn default_send_buffer_capacity() -> usize { 1000 }
fn default_report_delta_threshold() -> f64 { 0.10 }
fn default_replay_lookback_minutes() -> u64 { 5 }
fn default_broadcast_channel_capacity() -> usize { 10_000 }
fn default_lag_notice_threshold() -> usize { 10 }
fn default_poll_interval_ms() -> u64 { 500 }

// ---------------------------------------------------------------------------
// Telemetry config
// ---------------------------------------------------------------------------

#[derive(Clone, Deserialize, Default)]
pub struct TelemetryConfig {
    /// OTLP gRPC endpoint. If absent, stdout JSON tracing is used.
    pub otlp_endpoint: Option<String>,

    /// Log level filter string (e.g. `"info"`, `"debug,mg_onchain_gateway=trace"`).
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

fn default_log_level() -> String { "info".to_string() }

// ---------------------------------------------------------------------------
// Metrics config
// ---------------------------------------------------------------------------

#[derive(Clone, Deserialize, Default)]
pub struct MetricsConfig {
    /// If true, require bearer auth to scrape `/metrics`.
    #[serde(default)]
    pub require_auth: bool,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_TOML: &str = r#"
[gateway]
bind_address = "0.0.0.0:8080"

[gateway.auth]
jwt_signing_key_path = "/tmp/test.pem"

[gateway.ratelimit]
default_rpm = 60
write_analyze_rpm = 10
ws_connections_per_subject = 5

[gateway.cache]
token_risk_ttl_seconds = 60
token_risk_max_entries = 10000

[gateway.ws]
heartbeat_interval_seconds = 30
heartbeat_timeout_seconds = 60
"#;

    #[test]
    fn minimal_config_parses() {
        let cfg = GatewayConfig::from_toml(MINIMAL_TOML).expect("minimal config must parse");
        assert_eq!(cfg.gateway.bind_address, "0.0.0.0:8080");
        assert_eq!(cfg.gateway.auth.jwt_signing_key_path, "/tmp/test.pem");
        assert_eq!(cfg.gateway.ratelimit.default_rpm, 60);
        assert_eq!(cfg.gateway.cache.token_risk_ttl_seconds, 60);
    }

    #[test]
    fn defaults_applied_when_sections_absent() {
        let toml = r#"
[gateway]
bind_address = "127.0.0.1:8080"
[gateway.auth]
jwt_signing_key_path = "/tmp/test.pem"
[gateway.ratelimit]
[gateway.cache]
[gateway.ws]
"#;
        let cfg = GatewayConfig::from_toml(toml).expect("defaults config must parse");
        assert_eq!(cfg.gateway.ratelimit.default_rpm, 60);
        assert_eq!(cfg.gateway.cache.token_risk_ttl_seconds, 60);
        assert_eq!(cfg.gateway.ws.heartbeat_interval_seconds, 30);
        assert_eq!(cfg.gateway.shutdown_timeout_seconds, 30);
    }

    #[test]
    fn debug_redacts_auth() {
        let cfg = GatewayConfig::from_toml(MINIMAL_TOML).unwrap();
        let debug_str = format!("{cfg:?}");
        assert!(debug_str.contains("<redacted>"), "auth section must be redacted in Debug output");
        assert!(!debug_str.contains("jwt_signing_key_path"), "key path must not appear in Debug");
    }
}
