//! Tracing subscriber initialization with optional OTLP exporter.
//!
//! Reads `RUST_LOG` env var (fallback to `config.observability.log_filter`).
//! When `OTEL_EXPORTER_OTLP_ENDPOINT` env (or `config.otlp_endpoint`) is set, an
//! OTLP gRPC exporter is attached as a `tracing-opentelemetry` layer alongside
//! the stdout fmt layer. When unset, stdout-only fallback is preserved.
//!
//! # Sprint 26 T26-7 (ADR 0007 §9.5)
//!
//! Wires `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp`,
//! `tracing-opentelemetry` from workspace deps. Service resource attributes:
//! `service.name = "mg-onchain-service"`, `service.version = CARGO_PKG_VERSION`.
//! Span attribute conventions per design 0028 §9: HTTP/RPC OTel-standard for
//! REST/WS spans + `mg.detector.*` for detector-evaluation spans.
//!
//! # Gotcha #22
//!
//! No `Utc::now()` here. Tracing timestamps are wall-clock from the
//! `tracing_subscriber` fmt layer — that is correct for log output, not for
//! detector `observed_at` fields.

use crate::config::ObservabilityConfig;

const OTLP_ENV_VAR: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
const LEGACY_OTLP_ENV_VAR: &str = "OTLP_ENDPOINT";

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

    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| config.log_filter.clone());

    let env_filter = EnvFilter::try_new(&filter)
        .map_err(|e| anyhow::anyhow!("invalid RUST_LOG / log_filter '{filter}': {e}"))?;

    let otlp_endpoint = std::env::var(OTLP_ENV_VAR)
        .ok()
        .or_else(|| std::env::var(LEGACY_OTLP_ENV_VAR).ok())
        .or_else(|| config.otlp_endpoint.clone());

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().with_target(true));

    if let Some(endpoint) = otlp_endpoint {
        let otel_layer = build_otel_layer(&endpoint)?;
        registry
            .with(otel_layer)
            .try_init()
            .map_err(|e| anyhow::anyhow!("tracing subscriber init failed: {e}"))?;
        tracing::info!(otlp_endpoint = %endpoint, "OTLP exporter wired (gRPC)");
    } else {
        registry
            .try_init()
            .map_err(|e| anyhow::anyhow!("tracing subscriber init failed: {e}"))?;
    }

    Ok(())
}

/// Build the OpenTelemetry layer that exports spans over OTLP gRPC.
fn build_otel_layer<S>(
    endpoint: &str,
) -> anyhow::Result<tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::Resource;

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.to_string())
        .build()
        .map_err(|e| anyhow::anyhow!("OTLP span exporter build failed: {e}"))?;

    let resource = Resource::new(vec![
        KeyValue::new("service.name", "mg-onchain-service"),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
    ]);

    let provider = opentelemetry_sdk::trace::TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(resource)
        .build();
    let tracer = provider.tracer("mg-onchain-service");
    opentelemetry::global::set_tracer_provider(provider);

    Ok(tracing_opentelemetry::layer().with_tracer(tracer))
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
