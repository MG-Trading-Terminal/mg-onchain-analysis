//! `ServiceConfig` — top-level configuration for `onchain-service`.
//!
//! Loaded from `config/service.toml` via `toml::from_str`.
//!
//! # Design decisions
//!
//! - D-A: migration policy — auto-run on startup unless `--no-migrate` passed.
//! - D-B: single binary.
//! - D-C: `token_risk_reports_enabled` default `false` (gotcha #47).
//! - D-D: drain timeout 30s default.
//! - D-E: Solana `enabled=true`, Ethereum `enabled=false` by default.
//!
//! # Secret hygiene
//!
//! `ServiceConfig` never derives `Debug` on fields containing DB credentials.
//! Use `ServiceConfig::redacted_display()` for logging.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::streaming_config::StreamingConfig;

// ---------------------------------------------------------------------------
// ServiceConfig — top-level
// ---------------------------------------------------------------------------

/// Full configuration for `onchain-service`.
///
/// Loaded from `config/service.toml`. All sub-configs have sane defaults so
/// operators only need to populate the fields they want to override.
#[derive(Clone, Deserialize, Serialize)]
pub struct ServiceConfig {
    /// Shutdown drain window and graceful-shutdown settings (D-D).
    #[serde(default)]
    pub shutdown: ShutdownConfig,

    /// Observability: log filter, optional OTLP endpoint.
    #[serde(default)]
    pub observability: ObservabilityConfig,

    /// PostgreSQL connection.
    #[serde(default)]
    pub postgres: PostgresConfig,

    /// Per-chain enable flags and endpoint URLs (D-E).
    #[serde(default)]
    pub chains: ChainsConfig,

    /// Streaming scheduler config. Nested under `[streaming]` in TOML.
    ///
    /// All `StreamingConfig` fields live here (D-C: `token_risk_reports_enabled`
    /// defaults `false` inside `StreamingConfig`).
    #[serde(default)]
    pub streaming: StreamingConfig,

    /// Gateway bind address and endpoint paths.
    #[serde(default)]
    pub gateway: ServiceGatewayConfig,

    /// Periodic scan worker configuration (T26-6, ADR 0007 §6.4).
    ///
    /// Nested under `[periodic_scan]` in TOML. Absent = use `PeriodicScanConfig::default()`.
    #[serde(default)]
    pub periodic_scan: Option<crate::init::periodic_scan::PeriodicScanConfig>,
}

impl ServiceConfig {
    /// Load `ServiceConfig` from a TOML file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the TOML is malformed.
    /// The error message includes the file path for operator diagnostics.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read config file {}: {e}", path.display()))?;
        let config: Self = toml::from_str(&contents).map_err(|e| {
            anyhow::anyhow!("config parse error in {}: {e}", path.display())
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Parse `ServiceConfig` from a TOML string (for tests).
    pub fn from_toml(s: &str) -> anyhow::Result<Self> {
        let config: Self =
            toml::from_str(s).map_err(|e| anyhow::anyhow!("config parse error: {e}"))?;
        config.validate()?;
        Ok(config)
    }

    /// Validate that required fields are populated.
    ///
    /// Also validates each enabled EVM chain's `ws_url` to reject placeholder values.
    /// Boot fails fast when a chain is enabled but `ws_url` is still a placeholder.
    fn validate(&self) -> anyhow::Result<()> {
        if self.postgres.url.is_empty() {
            anyhow::bail!("postgres.url must not be empty");
        }
        let any_enabled = self.chains.solana.enabled
            || self.chains.ethereum.enabled
            || self.chains.bsc.enabled
            || self.chains.base.enabled
            || self.chains.arbitrum.enabled
            || self.chains.polygon.enabled;
        if !any_enabled {
            tracing::warn!(
                "all chains disabled — no events will be ingested; coordinator will have no adapters"
            );
        }

        // Per-chain ws_url validation (deliverable 4).
        // Fails fast if any enabled chain has a placeholder ws_url.
        self.chains.ethereum.validate_ws_url("ethereum")?;
        self.chains.bsc.validate_ws_url("bsc")?;
        self.chains.base.validate_ws_url("base")?;
        self.chains.arbitrum.validate_ws_url("arbitrum")?;
        self.chains.polygon.validate_ws_url("polygon")?;

        Ok(())
    }

    /// Return a string safe to log (Postgres credentials redacted).
    pub fn redacted_display(&self) -> String {
        let evm_enabled: Vec<&str> = [
            ("ethereum", self.chains.ethereum.enabled),
            ("bsc", self.chains.bsc.enabled),
            ("base", self.chains.base.enabled),
            ("arbitrum", self.chains.arbitrum.enabled),
            ("polygon", self.chains.polygon.enabled),
        ]
        .iter()
        .filter(|(_, en)| *en)
        .map(|(name, _)| *name)
        .collect();

        format!(
            "ServiceConfig {{ chains.solana.enabled={}, evm_enabled=[{}], \
             streaming.enabled={}, gateway.bind_addr={:?}, \
             shutdown.drain_timeout_seconds={} }}",
            self.chains.solana.enabled,
            evm_enabled.join(","),
            self.streaming.enabled,
            self.gateway.bind_addr,
            self.shutdown.drain_timeout_seconds,
        )
    }
}

// ---------------------------------------------------------------------------
// ShutdownConfig (D-D)
// ---------------------------------------------------------------------------

/// Graceful-shutdown drain window configuration.
///
/// Decision D-D: default 30s, configurable via `[shutdown] drain_timeout_seconds`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ShutdownConfig {
    /// Seconds to wait for indexer + scheduler to drain in-flight work.
    ///
    /// 30s provides headroom for Postgres WAL flush spikes and checkpoint saves
    /// while fitting within the standard Kubernetes 30s SIGKILL grace period.
    /// Decision D-D.
    #[serde(default = "ShutdownConfig::default_drain_timeout_seconds")]
    pub drain_timeout_seconds: u64,
}

impl ShutdownConfig {
    fn default_drain_timeout_seconds() -> u64 {
        30
    }
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            drain_timeout_seconds: Self::default_drain_timeout_seconds(),
        }
    }
}

// ---------------------------------------------------------------------------
// ObservabilityConfig
// ---------------------------------------------------------------------------

/// Tracing + optional OTLP exporter configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ObservabilityConfig {
    /// `EnvFilter` string used by `tracing_subscriber`.
    ///
    /// Overridden by the `RUST_LOG` environment variable at runtime.
    /// Default: `"info"`.
    #[serde(default = "ObservabilityConfig::default_log_filter")]
    pub log_filter: String,

    /// Optional OTLP gRPC collector endpoint.
    ///
    /// When `None` (or not set in config), no OTLP exporter is constructed
    /// and the binary has zero runtime dep on an observability collector.
    /// Can also be provided via `OTLP_ENDPOINT` environment variable at runtime.
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
}

impl ObservabilityConfig {
    fn default_log_filter() -> String {
        "info".to_string()
    }
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            log_filter: Self::default_log_filter(),
            otlp_endpoint: None,
        }
    }
}

// ---------------------------------------------------------------------------
// PostgresConfig
// ---------------------------------------------------------------------------

/// PostgreSQL connection configuration.
#[derive(Clone, Deserialize, Serialize)]
pub struct PostgresConfig {
    /// PostgreSQL connection URL.
    ///
    /// Format: `postgres://user:password@host:port/database`
    /// Never logged — use `ServiceConfig::redacted_display()`.
    #[serde(default = "PostgresConfig::default_url")]
    pub url: String,

    /// Maximum pool connections.
    ///
    /// Default: 32. At 12 detectors × N workers, 32 provides headroom for
    /// concurrent anomaly_event inserts + scheduler reads.
    #[serde(default = "PostgresConfig::default_max_connections")]
    pub max_connections: u32,

    /// Maximum connection retry attempts at startup.
    ///
    /// Default: 5. 2s exponential backoff between attempts.
    #[serde(default = "PostgresConfig::default_connect_retries")]
    pub connect_retries: u32,
}

impl PostgresConfig {
    fn default_url() -> String {
        "postgres://onchain:onchain@localhost/onchain".to_string()
    }

    fn default_max_connections() -> u32 {
        32
    }

    fn default_connect_retries() -> u32 {
        5
    }
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self {
            url: Self::default_url(),
            max_connections: Self::default_max_connections(),
            connect_retries: Self::default_connect_retries(),
        }
    }
}

// ---------------------------------------------------------------------------
// ChainsConfig (D-E)
// ---------------------------------------------------------------------------

/// Per-chain enable/disable and connection configuration.
///
/// Decision D-E: Solana `enabled=true`, Ethereum `enabled=false` by default.
/// All additional EVM chains (BSC, Base, Arbitrum, Polygon) default to `enabled=false`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChainsConfig {
    /// Solana chain configuration. Enabled by default (D-E).
    #[serde(default)]
    pub solana: SolanaChainConfig,

    /// Ethereum chain configuration. Disabled by default (D-E).
    #[serde(default)]
    pub ethereum: EvmChainConfig,

    /// BSC (BNB Smart Chain) configuration. Disabled by default.
    ///
    /// Production: operator MUST replace ws_url with self-hosted BSC node endpoint
    /// per ADR 0003 (bnbchain/bsc node, or Reth-compatible fork).
    #[serde(default)]
    pub bsc: EvmChainConfig,

    /// Base chain configuration. Disabled by default.
    ///
    /// Production: operator MUST replace ws_url with self-hosted Base node endpoint
    /// per ADR 0003 (base-reth or op-reth).
    #[serde(default)]
    pub base: EvmChainConfig,

    /// Arbitrum One configuration. Disabled by default.
    ///
    /// Production: operator MUST replace ws_url with self-hosted Arbitrum Nitro node
    /// per ADR 0003.
    #[serde(default)]
    pub arbitrum: EvmChainConfig,

    /// Polygon PoS configuration. Disabled by default.
    ///
    /// Production: operator MUST replace ws_url with self-hosted Bor (polygon) node
    /// per ADR 0003.
    ///
    /// SPEC-NOTE: Polygon reorg depth may be higher than 12 on mainnet due to faster
    /// block times (~2 s). Operators should verify and set a conservative value (e.g. 64)
    /// via the `reorg_depth` config key when enabling Polygon.
    #[serde(default)]
    pub polygon: EvmChainConfig,
}

impl ChainsConfig {
    /// Return all enabled EVM chains paired with their config.
    ///
    /// Used by `init::adapters::build_evm_adapters` to spawn one `EthereumAdapter`
    /// per enabled EVM chain without hard-coding the chain list at the call site.
    pub fn enabled_evm_chains(&self) -> Vec<(mg_onchain_common::chain::Chain, &EvmChainConfig)> {
        use mg_onchain_common::chain::Chain;
        let mut result = Vec::new();
        if self.ethereum.enabled {
            result.push((Chain::Ethereum, &self.ethereum));
        }
        if self.bsc.enabled {
            result.push((Chain::Bsc, &self.bsc));
        }
        if self.base.enabled {
            result.push((Chain::Base, &self.base));
        }
        if self.arbitrum.enabled {
            result.push((Chain::Arbitrum, &self.arbitrum));
        }
        if self.polygon.enabled {
            result.push((Chain::Polygon, &self.polygon));
        }
        result
    }
}

/// Solana chain configuration.
///
/// Decision D-E: `enabled = true` (Solana is the primary chain for shitcoin detection).
///
/// Sprint 26 (T26-2): replaced the single Yellowstone gRPC `rpc_url` field with two
/// standard JSON-RPC endpoints: `http_url` (port 8899) and `ws_url` (port 8900).
/// This mirrors `EvmChainConfig` and aligns with ADR 0007.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SolanaChainConfig {
    /// Whether to start the Solana indexer.
    ///
    /// Default: `true` (D-E). Set to `false` to disable Solana ingestion.
    #[serde(default = "SolanaChainConfig::default_enabled")]
    pub enabled: bool,

    /// Solana JSON-RPC HTTP endpoint.
    ///
    /// Used for one-shot requests: `getBlock`, `getTransaction`, `getSlot`, etc.
    /// Default: `http://127.0.0.1:8899` (standard Agave RPC-only HTTP port).
    #[serde(default = "SolanaChainConfig::default_http_url")]
    pub http_url: String,

    /// Solana JSON-RPC WebSocket endpoint.
    ///
    /// Used for push subscriptions: `programSubscribe`, `logsSubscribe`, etc.
    /// Default: `ws://127.0.0.1:8900` (standard Agave RPC-only WebSocket port).
    #[serde(default = "SolanaChainConfig::default_ws_url")]
    pub ws_url: String,

    /// Checkpoint file path for the Solana adapter.
    #[serde(default = "SolanaChainConfig::default_checkpoint_path")]
    pub checkpoint_path: String,
}

impl SolanaChainConfig {
    fn default_enabled() -> bool {
        true // D-E: Solana on by default
    }

    fn default_http_url() -> String {
        "http://127.0.0.1:8899".to_string()
    }

    fn default_ws_url() -> String {
        "ws://127.0.0.1:8900".to_string()
    }

    fn default_checkpoint_path() -> String {
        "./checkpoints/solana.json".to_string()
    }
}

impl Default for SolanaChainConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            http_url: Self::default_http_url(),
            ws_url: Self::default_ws_url(),
            checkpoint_path: Self::default_checkpoint_path(),
        }
    }
}

/// EVM chain configuration — shared across Ethereum, BSC, Base, Arbitrum, Polygon.
///
/// Each chain section in `config/service.toml` deserializes into this struct.
/// Per-chain defaults are applied at the `ChainsConfig` field level via `#[serde(default)]`.
///
/// Decision D-E: all EVM chains default to `enabled = false` until the operator
/// confirms their self-hosted node is operational (ADR 0003 + ADR 0004).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EvmChainConfig {
    /// Whether to start the indexer for this EVM chain.
    ///
    /// Default: `false` (D-E). Flip to `true` when the self-hosted node is ready.
    #[serde(default = "EvmChainConfig::default_enabled")]
    pub enabled: bool,

    /// WebSocket RPC endpoint for this EVM chain.
    ///
    /// Default: `ws://127.0.0.1:8546` (placeholder for Ethereum; other chains use
    /// distinct ports in `config/service.toml`). Production: operator MUST supply
    /// the real self-hosted endpoint per ADR 0003.
    #[serde(default = "EvmChainConfig::default_ws_url")]
    pub ws_url: String,

    /// Block confirmation depth before events are emitted.
    ///
    /// Default: 12 (≈2.4 min on Ethereum mainnet). Per ADR 0004 §Finality.
    /// SPEC-NOTE: Polygon PoS produces blocks every ~2 s — a depth of 12 is only
    /// ~24 s. Polygon operators should set a higher value (e.g. 64) matching the
    /// checkpoint interval. This is intentionally left configurable here.
    #[serde(default = "EvmChainConfig::default_reorg_depth")]
    pub reorg_depth: u64,

    /// Checkpoint file path for this chain's adapter.
    #[serde(default = "EvmChainConfig::default_checkpoint_path")]
    pub checkpoint_path: String,
}

impl EvmChainConfig {
    fn default_enabled() -> bool {
        false // D-E: all EVM chains off by default
    }

    fn default_ws_url() -> String {
        // Ethereum default port — overridden per-chain in service.toml
        "ws://127.0.0.1:8546".to_string()
    }

    fn default_reorg_depth() -> u64 {
        12
    }

    fn default_checkpoint_path() -> String {
        "./checkpoints/ethereum.json".to_string()
    }

    /// Return the recommended default reorg depth for a given EVM chain.
    ///
    /// This function encodes per-chain finality characteristics:
    ///
    /// | Chain    | Depth | Rationale |
    /// |----------|-------|-----------|
    /// | Ethereum | 12    | Post-Merge LMD-GHOST; ~2.4 min (ADR 0004 §Finality) |
    /// | Base     | 12    | OP Stack L2; inherits L1 safety window at depth 12 |
    /// | Arbitrum | 12    | Nitro rollup; L2 safety at 12 blocks (~3 s Nitro) |
    /// | BSC      | 15    | 3 s blocks; Parlia PoA; slightly elevated risk of short forks |
    /// |          |       | ref: bnbchain.org/docs/bnbSmartChain/concepts/consensus/ |
    /// | Polygon  | 64    | Bor PoS; heimdall checkpoint every 256 blocks (~8.5 min). |
    /// |          |       | 64-block soft finality matches the Polygon staking docs |
    /// |          |       | recommendation for exchange confirmations. |
    /// |          |       | ref: docs.polygon.technology/pos/architecture/heimdall/ |
    ///
    /// Operators may still override per-chain via `config/service.toml`.
    pub fn default_reorg_depth_for_chain(chain: mg_onchain_common::chain::Chain) -> u64 {
        use mg_onchain_common::chain::Chain;
        match chain {
            Chain::Polygon => 64,
            Chain::Bsc => 15,
            // Ethereum, Base, Arbitrum, and any future EVM chains default to 12.
            _ => 12,
        }
    }

    /// Validate `ws_url` when the chain is enabled.
    ///
    /// Rejects placeholder localhost addresses (`ws://127.0.0.1:854[6-9]`) to
    /// prevent the service from starting with unreal endpoints that would silently
    /// fail to connect. Disabled chains are permitted to keep placeholder values.
    ///
    /// Production: replace `ws://127.0.0.1:854X` with a real self-hosted Reth-
    /// compatible endpoint per ADR 0003. The binary WILL REFUSE TO START if a chain
    /// is enabled but `ws_url` is still a placeholder.
    ///
    /// # Placeholder pattern
    ///
    /// Ports 8546–8549 are the per-chain example ports in `config/service.toml`.
    /// Detection: URL starts with `"ws://127.0.0.1:854"` AND the next char is 6–9.
    pub fn validate_ws_url(&self, chain_name: &str) -> anyhow::Result<()> {
        if !self.enabled {
            // Disabled chains may keep placeholder values.
            return Ok(());
        }
        let url = &self.ws_url;

        // Must use ws:// or wss:// scheme.
        if !url.starts_with("ws://") && !url.starts_with("wss://") {
            anyhow::bail!(
                "chain '{}' is enabled but ws_url '{}' does not use ws:// or wss:// scheme. \
                 Provide a real self-hosted endpoint per ADR 0003.",
                chain_name,
                url,
            );
        }

        // Reject placeholder localhost ports 8546–8549 (service.toml examples).
        // Pattern: "ws://127.0.0.1:854" followed by a digit 6–9.
        let placeholder_prefix = "ws://127.0.0.1:854";
        if let Some(after) = url.strip_prefix(placeholder_prefix) {
            // Check the next character after the prefix is '6'..'9'.
            if let Some(ch) = after.chars().next()
                && ('6'..='9').contains(&ch)
            {
                anyhow::bail!(
                    "chain '{}' is enabled but ws_url '{}' is a placeholder (ws://127.0.0.1:854[6-9]). \
                     Replace with a real self-hosted endpoint per ADR 0003. \
                     The service will not start with placeholder endpoints on enabled chains.",
                    chain_name,
                    url,
                );
            }
        }

        Ok(())
    }
}

impl Default for EvmChainConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            ws_url: Self::default_ws_url(),
            reorg_depth: Self::default_reorg_depth(),
            checkpoint_path: Self::default_checkpoint_path(),
        }
    }
}

/// Backwards-compat type alias. Code that references `EthereumChainConfig` by name
/// (e.g. `init::adapters`) continues to compile without changes.
pub type EthereumChainConfig = EvmChainConfig;

// ---------------------------------------------------------------------------
// ServiceGatewayConfig
// ---------------------------------------------------------------------------

/// Gateway bind address and probe endpoint paths.
///
/// Note: detailed gateway config (auth, rate-limit, cache, WS) lives in
/// `config/gateway.toml` and is loaded by `GatewayConfig::from_file`. This
/// struct only carries the top-level knobs needed by `main.rs` startup.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServiceGatewayConfig {
    /// TCP bind address for the gateway.
    #[serde(default = "ServiceGatewayConfig::default_bind_addr")]
    pub bind_addr: String,

    /// Path to `config/gateway.toml` for full gateway config.
    #[serde(default = "ServiceGatewayConfig::default_gateway_toml")]
    pub gateway_toml: String,
}

impl ServiceGatewayConfig {
    fn default_bind_addr() -> String {
        "127.0.0.1:8080".to_string()
    }

    fn default_gateway_toml() -> String {
        "config/gateway.toml".to_string()
    }
}

impl Default for ServiceGatewayConfig {
    fn default() -> Self {
        Self {
            bind_addr: Self::default_bind_addr(),
            gateway_toml: Self::default_gateway_toml(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_TOML: &str = r#"
[postgres]
url = "postgres://test:test@localhost/test"
"#;

    #[test]
    fn minimal_config_parses_with_defaults() {
        let cfg = ServiceConfig::from_toml(MINIMAL_TOML).expect("minimal config must parse");
        // D-D: 30s drain timeout
        assert_eq!(cfg.shutdown.drain_timeout_seconds, 30);
        // D-E: Solana enabled, Ethereum disabled
        assert!(cfg.chains.solana.enabled, "Solana must be enabled by default (D-E)");
        assert!(!cfg.chains.ethereum.enabled, "Ethereum must be disabled by default (D-E)");
        // D-C: token_risk_reports_enabled = false (gotcha #47)
        assert!(
            !cfg.streaming.token_risk_reports_enabled,
            "token_risk_reports_enabled must default to false (D-C / gotcha #47)"
        );
    }

    #[test]
    fn all_chains_disabled_does_not_error_but_warns() {
        let toml = r#"
[postgres]
url = "postgres://test:test@localhost/test"

[chains.solana]
enabled = false

[chains.ethereum]
enabled = false
"#;
        // Should succeed with a warning (not an error).
        let cfg = ServiceConfig::from_toml(toml).expect("all-chains-disabled must not error");
        assert!(!cfg.chains.solana.enabled);
        assert!(!cfg.chains.ethereum.enabled);
        // New EVM chains must default to disabled.
        assert!(!cfg.chains.bsc.enabled, "BSC must default to disabled");
        assert!(!cfg.chains.base.enabled, "Base must default to disabled");
        assert!(!cfg.chains.arbitrum.enabled, "Arbitrum must default to disabled");
        assert!(!cfg.chains.polygon.enabled, "Polygon must default to disabled");
    }

    #[test]
    fn new_evm_chains_default_to_disabled() {
        let cfg = ServiceConfig::from_toml(MINIMAL_TOML).expect("minimal config must parse");
        assert!(!cfg.chains.bsc.enabled, "BSC must default to disabled");
        assert!(!cfg.chains.base.enabled, "Base must default to disabled");
        assert!(!cfg.chains.arbitrum.enabled, "Arbitrum must default to disabled");
        assert!(!cfg.chains.polygon.enabled, "Polygon must default to disabled");
    }

    #[test]
    fn enabled_evm_chains_empty_when_all_disabled() {
        let cfg = ServiceConfig::from_toml(MINIMAL_TOML).expect("minimal config must parse");
        let enabled = cfg.chains.enabled_evm_chains();
        assert!(
            enabled.is_empty(),
            "enabled_evm_chains() must be empty when all EVM chains disabled"
        );
    }

    #[test]
    fn enabled_evm_chains_returns_enabled_chains() {
        let toml = r#"
[postgres]
url = "postgres://test:test@localhost/test"

[chains.ethereum]
enabled = true
ws_url = "ws://192.168.1.10:8546"

[chains.bsc]
enabled = true
ws_url = "wss://192.168.1.11:8547"
"#;
        let cfg = ServiceConfig::from_toml(toml).expect("config must parse");
        let enabled = cfg.chains.enabled_evm_chains();
        assert_eq!(enabled.len(), 2, "two EVM chains enabled");
        let chains: Vec<_> = enabled.iter().map(|(c, _)| *c).collect();
        use mg_onchain_common::chain::Chain;
        assert!(chains.contains(&Chain::Ethereum));
        assert!(chains.contains(&Chain::Bsc));
    }

    #[test]
    fn empty_postgres_url_is_rejected() {
        let toml = r#"
[postgres]
url = ""
"#;
        let result = ServiceConfig::from_toml(toml);
        assert!(result.is_err(), "empty postgres.url must fail validation");
    }

    #[test]
    fn drain_timeout_configurable() {
        let toml = r#"
[postgres]
url = "postgres://test:test@localhost/test"

[shutdown]
drain_timeout_seconds = 60
"#;
        let cfg = ServiceConfig::from_toml(toml).unwrap();
        assert_eq!(cfg.shutdown.drain_timeout_seconds, 60);
    }

    #[test]
    fn ethereum_can_be_enabled_via_config() {
        // Must use a non-placeholder ws_url now that enabled chains are validated.
        let toml = r#"
[postgres]
url = "postgres://test:test@localhost/test"

[chains.solana]
enabled = true

[chains.ethereum]
enabled = true
ws_url = "ws://192.168.1.10:8546"
"#;
        let cfg = ServiceConfig::from_toml(toml).unwrap();
        assert!(cfg.chains.ethereum.enabled);
        assert_eq!(cfg.chains.ethereum.ws_url, "ws://192.168.1.10:8546");
    }

    #[test]
    fn token_risk_reports_can_be_opted_in() {
        let toml = r#"
[postgres]
url = "postgres://test:test@localhost/test"

[streaming]
token_risk_reports_enabled = true
"#;
        let cfg = ServiceConfig::from_toml(toml).unwrap();
        assert!(cfg.streaming.token_risk_reports_enabled);
    }

    #[test]
    fn redacted_display_does_not_contain_postgres_url() {
        let toml = r#"
[postgres]
url = "postgres://secret_user:secret_pw@localhost/mydb"
"#;
        let cfg = ServiceConfig::from_toml(toml).unwrap();
        let display = cfg.redacted_display();
        assert!(
            !display.contains("secret_user") && !display.contains("secret_pw"),
            "redacted_display must not expose DB credentials: {display}"
        );
    }

    // -------------------------------------------------------------------------
    // validate_ws_url tests (deliverable 4)
    // -------------------------------------------------------------------------

    /// Enabled chain with a valid non-placeholder ws_url → Ok.
    #[test]
    fn validate_ws_url_enabled_valid_url_ok() {
        let cfg = EvmChainConfig {
            enabled: true,
            ws_url: "ws://192.168.1.10:8546".to_string(),
            reorg_depth: 12,
            checkpoint_path: "./checkpoints/ethereum.json".to_string(),
        };
        assert!(
            cfg.validate_ws_url("ethereum").is_ok(),
            "enabled chain with valid ws_url must pass validation"
        );
    }

    /// Enabled chain with wss:// scheme → Ok.
    #[test]
    fn validate_ws_url_enabled_wss_scheme_ok() {
        let cfg = EvmChainConfig {
            enabled: true,
            ws_url: "wss://my-node.example.com:443".to_string(),
            reorg_depth: 12,
            checkpoint_path: "./checkpoints/ethereum.json".to_string(),
        };
        assert!(
            cfg.validate_ws_url("ethereum").is_ok(),
            "enabled chain with wss:// url must pass validation"
        );
    }

    /// Enabled chain with placeholder ws_url (ws://127.0.0.1:854[6-9]) → Err.
    #[test]
    fn validate_ws_url_enabled_placeholder_rejects() {
        for port in ["8546", "8547", "8548", "8549"] {
            let cfg = EvmChainConfig {
                enabled: true,
                ws_url: format!("ws://127.0.0.1:{port}"),
                reorg_depth: 12,
                checkpoint_path: "./checkpoints/test.json".to_string(),
            };
            let result = cfg.validate_ws_url("test_chain");
            assert!(
                result.is_err(),
                "enabled chain with placeholder port {port} must fail validation"
            );
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("placeholder"),
                "error must mention placeholder for port {port}: {err}"
            );
        }
    }

    /// Enabled chain with http:// scheme → Err (wrong scheme).
    #[test]
    fn validate_ws_url_enabled_http_scheme_rejects() {
        let cfg = EvmChainConfig {
            enabled: true,
            ws_url: "http://192.168.1.10:8545".to_string(),
            reorg_depth: 12,
            checkpoint_path: "./checkpoints/ethereum.json".to_string(),
        };
        let result = cfg.validate_ws_url("ethereum");
        assert!(
            result.is_err(),
            "enabled chain with http:// scheme must fail validation"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("ws://") || err.contains("wss://"),
            "error must mention ws:// or wss:// requirement: {err}"
        );
    }

    /// Disabled chain with placeholder ws_url → Ok (disabled chains are permissive).
    #[test]
    fn validate_ws_url_disabled_chain_placeholder_ok() {
        let cfg = EvmChainConfig {
            enabled: false,
            ws_url: "ws://127.0.0.1:8546".to_string(), // placeholder
            reorg_depth: 12,
            checkpoint_path: "./checkpoints/ethereum.json".to_string(),
        };
        assert!(
            cfg.validate_ws_url("ethereum").is_ok(),
            "disabled chain with placeholder ws_url must pass validation (permissive)"
        );
    }

    // -------------------------------------------------------------------------
    // default_reorg_depth_for_chain tests (per-chain finality, deliverable 4)
    // -------------------------------------------------------------------------

    /// Ethereum, Base, Arbitrum all return 12 (LMD-GHOST / rollup safety window).
    #[test]
    fn default_reorg_depth_eth_base_arbitrum_is_12() {
        use mg_onchain_common::chain::Chain;
        for chain in [Chain::Ethereum, Chain::Base, Chain::Arbitrum] {
            assert_eq!(
                EvmChainConfig::default_reorg_depth_for_chain(chain),
                12,
                "expected depth 12 for {chain}"
            );
        }
    }

    /// BSC returns 15 (3 s blocks + Parlia PoA short-fork risk).
    #[test]
    fn default_reorg_depth_bsc_is_15() {
        use mg_onchain_common::chain::Chain;
        assert_eq!(
            EvmChainConfig::default_reorg_depth_for_chain(Chain::Bsc),
            15,
            "BSC reorg depth must be 15 (Parlia PoA, 3 s blocks)"
        );
    }

    /// Polygon returns 64 (Bor PoS heimdall checkpoint interval safety buffer).
    #[test]
    fn default_reorg_depth_polygon_is_64() {
        use mg_onchain_common::chain::Chain;
        assert_eq!(
            EvmChainConfig::default_reorg_depth_for_chain(Chain::Polygon),
            64,
            "Polygon reorg depth must be 64 (heimdall checkpoint interval)"
        );
    }

    /// Boot fails when enabled chain has placeholder ws_url — end-to-end via from_toml.
    #[test]
    fn boot_fails_fast_enabled_chain_placeholder_ws_url() {
        let toml = r#"
[postgres]
url = "postgres://test:test@localhost/test"

[chains.ethereum]
enabled = true
ws_url = "ws://127.0.0.1:8546"
"#;
        let result = ServiceConfig::from_toml(toml);
        assert!(
            result.is_err(),
            "from_toml must fail when enabled chain has placeholder ws_url"
        );
        let err = result.err().unwrap().to_string();
        assert!(
            err.contains("placeholder"),
            "error message must mention placeholder: {err}"
        );
    }
}
