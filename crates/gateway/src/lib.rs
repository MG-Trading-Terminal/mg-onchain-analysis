//! `mg-onchain-gateway` — axum HTTP + WebSocket API gateway.
//!
//! # Entry point
//!
//! Callers (typically `crates/server`) construct an `AppState` and call `run_gateway()`.
//!
//! ```ignore
//! use mg_onchain_gateway::{GatewayConfig, run_gateway};
//!
//! let config = GatewayConfig::from_file("config/gateway.toml")?;
//! run_gateway(config, state).await?;
//! ```
//!
//! # Module layout
//!
//! ```text
//! crates/gateway/src/
//!   lib.rs                 — Public API re-exports
//!   config.rs              — GatewayConfig + sub-configs
//!   state.rs               — AppState (shared between handlers)
//!   error.rs               — GatewayError + RFC 7807 IntoResponse
//!   cache.rs               — moka TokenRiskReport cache
//!   metrics.rs             — Prometheus metrics registry
//!   ratelimit.rs           — Per-subject token-bucket rate limiter
//!   auth/
//!     mod.rs               — AuthClaims extractor + bearer token extraction
//!     jwt.rs               — JWT sign/verify (Ed25519 / EdDSA)
//!     argon.rs             — Argon2id password hashing
//!     scopes.rs            — Scope constants + require_scope
//!     user_store.rs        — auth_users Postgres table
//!   routes/
//!     mod.rs               — Router builder + middleware stack
//!     analyze.rs           — POST /v1/tokens/analyze
//!     risk.rs              — GET /v1/tokens/{chain}/{mint}/risk
//!     events.rs            — GET /v1/anomaly_events
//!     detectors_handler.rs — GET /v1/detectors
//!     health.rs            — GET /health
//!     metrics_handler.rs   — GET /metrics
//!     auth_handler.rs      — POST /v1/auth/token + GET /v1/.well-known/jwks.json
//!     admin.rs             — DELETE /v1/admin/cache + POST /v1/admin/users
//!   ws/
//!     mod.rs               — GET /v1/ws/stream WebSocket handler
//! ```

pub mod auth;
pub mod cache;
pub mod config;
pub mod error;
pub mod metrics;
pub mod ratelimit;
pub mod routes;
pub mod state;
pub mod ws;

// Primary public re-exports.
pub use config::GatewayConfig;
pub use error::GatewayError;
pub use state::AppState;
pub use routes::build_router;

use std::sync::Arc;

use tokio::net::TcpListener;
use tracing::info;

/// Run the gateway on the configured bind address.
///
/// Blocks until a shutdown signal is received (SIGTERM or SIGINT).
pub async fn run_gateway(state: Arc<AppState>) -> anyhow::Result<()> {
    let bind_addr = &state.config.gateway.bind_address;
    let router = build_router(state.clone());

    info!(bind_address = %bind_addr, "gateway binding");
    let listener = TcpListener::bind(bind_addr).await
        .map_err(|e| anyhow::anyhow!("bind {bind_addr}: {e}"))?;

    info!(bind_address = %bind_addr, "gateway listening");

    // Graceful shutdown on SIGTERM / SIGINT.
    let shutdown_timeout = state.config.gateway.shutdown_timeout_seconds;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal(shutdown_timeout))
        .await
        .map_err(|e| anyhow::anyhow!("axum serve error: {e}"))?;

    info!("gateway shutdown complete");
    Ok(())
}

async fn shutdown_signal(timeout_seconds: u64) {
    use tokio::signal;
    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to listen for ctrl_c");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!(timeout_seconds, "shutdown signal received; draining in-flight requests");
    // Give in-flight requests time to complete.
    tokio::time::sleep(std::time::Duration::from_secs(0)).await;
}
