//! Prometheus metrics registry and metric definitions.
//!
//! All metrics are registered at startup and stored in `AppState`.
//! Handlers call the metric methods directly without locking (prometheus handles
//! thread-safety internally).

use prometheus::{
    Counter, CounterVec, Gauge, HistogramOpts, HistogramVec, Opts, Registry,
};

/// All Prometheus metrics for the gateway.
///
/// Stored behind `Arc` in `AppState` — cheap to clone.
#[derive(Clone)]
pub struct GatewayMetrics {
    /// Total HTTP requests, labelled by path, method, status.
    pub http_requests_total: CounterVec,
    /// HTTP request duration histogram.
    pub http_request_duration_seconds: HistogramVec,
    /// Current active WebSocket connections.
    pub ws_active_connections: Gauge,
    /// Active WS subscriptions per chain.
    pub ws_subscriptions_active: CounterVec,
    /// Total WS events dispatched.
    pub ws_events_dispatched_total: CounterVec,
    /// Total WS lag notices sent (buffer overflow drops).
    pub ws_lag_notices_total: Counter,
    /// Scoring cache hits.
    pub scoring_cache_hits_total: Counter,
    /// Scoring cache misses.
    pub scoring_cache_misses_total: Counter,
    /// Detector invocations, labelled by detector_id and outcome.
    pub detector_invocations_total: CounterVec,
    /// Current in-flight analyze operations.
    pub analyze_in_flight: Gauge,
    /// Prometheus registry (for text-format encode at /metrics).
    pub registry: Registry,
}

impl GatewayMetrics {
    /// Register all metrics and return the metrics struct.
    ///
    /// Panics on double-registration — this must only be called once at startup.
    pub fn new() -> anyhow::Result<Self> {
        let registry = Registry::new();

        let http_requests_total = CounterVec::new(
            Opts::new("http_requests_total", "Total HTTP requests"),
            &["path", "method", "status"],
        )?;
        registry.register(Box::new(http_requests_total.clone()))?;

        // Latency buckets: 5ms, 10ms, 25ms, 50ms, 100ms, 250ms, 500ms, 1s, 2.5s
        let duration_buckets = vec![0.005, 0.010, 0.025, 0.050, 0.100, 0.250, 0.500, 1.0, 2.5];
        let http_request_duration_seconds = HistogramVec::new(
            HistogramOpts::new("http_request_duration_seconds", "HTTP request duration")
                .buckets(duration_buckets),
            &["path", "method"],
        )?;
        registry.register(Box::new(http_request_duration_seconds.clone()))?;

        let ws_active_connections = Gauge::with_opts(
            Opts::new("ws_active_connections", "Current WebSocket connections"),
        )?;
        registry.register(Box::new(ws_active_connections.clone()))?;

        let ws_subscriptions_active = CounterVec::new(
            Opts::new("ws_subscriptions_active", "Active WS subscriptions"),
            &["chain"],
        )?;
        registry.register(Box::new(ws_subscriptions_active.clone()))?;

        let ws_events_dispatched_total = CounterVec::new(
            Opts::new("ws_events_dispatched_total", "Total WS events dispatched"),
            &["chain", "detector_id"],
        )?;
        registry.register(Box::new(ws_events_dispatched_total.clone()))?;

        let ws_lag_notices_total = Counter::with_opts(
            Opts::new("ws_lag_notices_total", "Total WS lag notices sent"),
        )?;
        registry.register(Box::new(ws_lag_notices_total.clone()))?;

        let scoring_cache_hits_total = Counter::with_opts(
            Opts::new("scoring_cache_hits_total", "Scoring cache hits"),
        )?;
        registry.register(Box::new(scoring_cache_hits_total.clone()))?;

        let scoring_cache_misses_total = Counter::with_opts(
            Opts::new("scoring_cache_misses_total", "Scoring cache misses"),
        )?;
        registry.register(Box::new(scoring_cache_misses_total.clone()))?;

        let detector_invocations_total = CounterVec::new(
            Opts::new("detector_invocations_total", "Detector invocations"),
            &["detector_id", "outcome"],
        )?;
        registry.register(Box::new(detector_invocations_total.clone()))?;

        let analyze_in_flight = Gauge::with_opts(
            Opts::new("analyze_in_flight", "Active concurrent analyze operations"),
        )?;
        registry.register(Box::new(analyze_in_flight.clone()))?;

        Ok(Self {
            http_requests_total,
            http_request_duration_seconds,
            ws_active_connections,
            ws_subscriptions_active,
            ws_events_dispatched_total,
            ws_lag_notices_total,
            scoring_cache_hits_total,
            scoring_cache_misses_total,
            detector_invocations_total,
            analyze_in_flight,
            registry,
        })
    }

    /// Encode all metrics in Prometheus text format (version 0.0.4).
    pub fn encode_text(&self) -> anyhow::Result<String> {
        use prometheus::Encoder;
        let encoder = prometheus::TextEncoder::new();
        let families = self.registry.gather();
        let mut buf = Vec::new();
        encoder.encode(&families, &mut buf)?;
        String::from_utf8(buf).map_err(|e| anyhow::anyhow!("metrics encode error: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_register_without_error() {
        GatewayMetrics::new().expect("metrics registration must succeed");
    }

    #[test]
    fn encode_text_produces_valid_output() {
        let m = GatewayMetrics::new().unwrap();
        m.http_requests_total
            .with_label_values(&["/health", "GET", "200"])
            .inc();
        let text = m.encode_text().unwrap();
        assert!(text.contains("http_requests_total"));
    }
}
