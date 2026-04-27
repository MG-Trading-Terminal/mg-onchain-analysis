//! Tracing subscriber initialization.
//!
//! Reads `RUST_LOG` env var (fallback to `config.observability.log_filter`).
//! Optional OTLP exporter is NOT wired in Sprint 19 — runtime-conditional from
//! `OTLP_ENDPOINT` env var is a Sprint 20 follow-up (design 0020 §8).
//!
//! # Gotcha #22
//!
//! No `Utc::now()` here. Tracing timestamps are wall-clock from the
//! `tracing_subscriber` fmt layer — that is correct for log output, not for
//! detector `observed_at` fields.

use crate::config::ObservabilityConfig;

/// Initialize the global `tracing` subscriber.
///
/// Must be called before any other startup step so all subsequent errors
/// are observable as structured log lines.
///
/// # Errors
///
/// Returns `Err` if the subscriber cannot be installed (e.g. already installed).
pub fn init_tracing(config: &ObservabilityConfig) -> anyhow::Result<()> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    // Prefer RUST_LOG env var; fall back to config value.
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| config.log_filter.clone());

    let env_filter = EnvFilter::try_new(&filter)
        .map_err(|e| anyhow::anyhow!("invalid RUST_LOG / log_filter '{filter}': {e}"))?;

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().with_target(true))
        .try_init()
        .map_err(|e| anyhow::anyhow!("tracing subscriber init failed: {e}"))?;

    // TODO(sprint-20): if OTLP_ENDPOINT env var is set, attach opentelemetry_otlp
    // exporter layer here. Skipped in Sprint 19 — design 0020 §8 / gotcha #74.
    if let Some(otlp) = std::env::var("OTLP_ENDPOINT")
        .ok()
        .or_else(|| config.otlp_endpoint.clone())
    {
        tracing::info!(
            otlp_endpoint = %otlp,
            "OTLP_ENDPOINT configured but OTLP exporter deferred to Sprint 20 (design 0020 §8)"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_tracing_with_default_filter() {
        // Two subscribers cannot coexist in a process. We test the filter
        // construction path without actually installing it.
        use tracing_subscriber::EnvFilter;
        let config = ObservabilityConfig::default();
        let filter_str = std::env::var("RUST_LOG")
            .unwrap_or_else(|_| config.log_filter.clone());
        // Must not panic or return error for the default "info" filter.
        EnvFilter::try_new(&filter_str).expect("default log filter must be valid");
    }

    #[test]
    fn invalid_filter_string_returns_error() {
        use tracing_subscriber::EnvFilter;
        // A filter string with an invalid directive should fail.
        let result = EnvFilter::try_new("!!!invalid_filter!!!");
        assert!(result.is_err(), "invalid filter must fail EnvFilter::try_new");
    }
}
