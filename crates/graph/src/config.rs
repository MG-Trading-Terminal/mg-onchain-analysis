//! Graph crate configuration — thresholds for clustering and indexing.
//!
//! All thresholds use the `Threshold<T>` wrapper (value + rationale + refs)
//! established in `crates/detectors/src/config.rs`. The wrapper is re-defined
//! here to keep `crates/graph` independent of `crates/detectors` (dependency
//! direction: graph → storage → common, never graph → detectors).
//!
//! # Loading
//!
//! At startup, [`load_graph_config`] parses `config/graph.toml` and validates
//! that all required keys are present. Missing keys produce a hard error at
//! startup — the graph indexer never falls back to silent defaults.
//!
//! # Threshold sources (design 0013 §10)
//!
//! | Key                      | Default | Source                               |
//! |--------------------------|---------|--------------------------------------|
//! | cofunding_window_hours   | 24      | Liu et al. (2025) arxiv:2505.09313   |
//! | amount_similarity_pct    | 0.20    | Messias et al. (2023) arxiv:2312.02752|
//! | min_cluster_size         | 3       | Chainalysis (2025) wash trading report|
//! | min_funder_sol_amount    | 10_000_000 | Solana docs (rent exemption)      |
//! | indexer_batch_size       | 10_000  | Empirical; no published reference    |
//! | cluster_ttl_hours        | 168     | Chainalysis label update cadence     |

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::path::Path;

// ---------------------------------------------------------------------------
// Threshold wrapper (local copy — avoids detectors dependency)
// ---------------------------------------------------------------------------

/// A typed threshold value with its cited rationale.
///
/// Every threshold in `config/graph.toml` uses this shape:
/// ```toml
/// [graph.threshold_name]
/// value     = 24
/// rationale = "Liu et al. (2025): temporal clustering parameter."
/// refs      = ["graph/common_funder"]
/// ```
///
/// This is structurally identical to `mg_onchain_detectors::config::Threshold<T>`.
/// It is duplicated here to preserve the `graph → storage → common` dependency
/// direction (no `graph → detectors` arrow). A future refactor may move it to
/// `crates/common`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Threshold<T> {
    /// The threshold value.
    pub value: T,
    /// Human-readable rationale explaining the chosen value and its source.
    pub rationale: String,
    /// REFERENCES.md entry IDs that justify this threshold.
    pub refs: Vec<String>,
}

// ---------------------------------------------------------------------------
// GraphConfig
// ---------------------------------------------------------------------------

/// All tunable parameters for `crates/graph` clustering and indexing.
///
/// Loaded from `config/graph.toml` at service startup. All fields are
/// required; missing keys produce a hard error (serde `Deserialize` panics on
/// missing required keys — this is intentional).
///
/// Design source: docs/designs/0013-graph.md §3.1 + §10.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GraphConfig {
    /// Hours within which two wallets funded by the same source are considered
    /// co-funded. Defines the tumbling time-window in the common-funder algorithm.
    ///
    /// Default: 24. Source: Liu et al. (2025) temporal clustering parameter.
    pub cofunding_window_hours: Threshold<u32>,

    /// Maximum fractional difference in funding amounts for two wallets to be
    /// placed in the same amount-bucket.
    ///
    /// Amount bucketing formula (log-scale):
    ///   `bucket = floor(ln(lamports) / ln(1.0 + amount_similarity_pct))`
    ///
    /// This ensures amounts within `amount_similarity_pct` of each other share
    /// a bucket, regardless of absolute magnitude (handles $1 vs $100 ranges).
    ///
    /// Default: 0.20 (20%). Source: Messias et al. (2023) arxiv:2312.02752.
    pub amount_similarity_pct: Threshold<f64>,

    /// Minimum number of wallets in a cluster for the cluster to be emitted.
    ///
    /// Default: 3. Source: Chainalysis (2025) Heuristic 2 minimum (≥5 funded
    /// addresses); 3 chosen as MVP lower bound to maximize recall at cost of
    /// precision. Adjust upward after empirical FP measurement.
    pub min_cluster_size: Threshold<u32>,

    /// Minimum SOL amount (in lamports) sent by a funder for the transfer to be
    /// considered a funding event. Dust filter.
    ///
    /// Default: 10_000_000 (0.01 SOL). Source: Solana docs (rent-exempt minimum
    /// per account is ~890,880 lamports ≈ 0.00089 SOL; 0.01 SOL is 11× that).
    pub min_funder_sol_amount: Threshold<u64>,

    /// Batch size for reading transfers from Postgres during edge indexing.
    ///
    /// Default: 10_000. Tune against `EXPLAIN ANALYZE` timing on a populated
    /// `transfers` table. Larger batches reduce round-trips but increase memory use.
    pub indexer_batch_size: Threshold<u32>,

    /// How long (hours) before a cluster record is considered stale and
    /// re-computation is triggered.
    ///
    /// Default: 168 (7 days). Source: Chainalysis updates labels weekly.
    pub cluster_ttl_hours: Threshold<u32>,
}

// ---------------------------------------------------------------------------
// Config container for the TOML file root
// ---------------------------------------------------------------------------

/// Root of `config/graph.toml` — wraps `GraphConfig` under the `[graph]` key.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GraphConfigFile {
    pub graph: GraphConfig,
}

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

/// Load and validate `config/graph.toml` (or the provided path).
///
/// # Errors
///
/// Returns `anyhow::Error` if:
/// - The file does not exist or cannot be read.
/// - The TOML fails to parse.
/// - Any required subsection or threshold key is missing.
pub fn load_graph_config(path: impl AsRef<Path>) -> anyhow::Result<GraphConfig> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read graph config from {}", path.display()))?;
    let file: GraphConfigFile = toml::from_str(&content)
        .with_context(|| format!("failed to parse graph config from {}", path.display()))?;
    Ok(file.graph)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        PathBuf::from(manifest_dir)
            .parent() // crates/
            .expect("crates dir")
            .parent() // workspace root
            .expect("workspace root")
            .to_path_buf()
    }

    #[test]
    fn load_graph_toml_succeeds() {
        let path = workspace_root().join("config/graph.toml");
        let config = load_graph_config(&path).expect("config/graph.toml must parse");
        // Pin default values so a typo in config is caught.
        assert_eq!(config.cofunding_window_hours.value, 24);
        assert!((config.amount_similarity_pct.value - 0.20).abs() < f64::EPSILON);
        assert_eq!(config.min_cluster_size.value, 3);
        assert_eq!(config.min_funder_sol_amount.value, 10_000_000);
        assert_eq!(config.indexer_batch_size.value, 10_000);
        assert_eq!(config.cluster_ttl_hours.value, 168);
    }

    #[test]
    fn all_thresholds_have_refs() {
        let path = workspace_root().join("config/graph.toml");
        let config = load_graph_config(&path).expect("config/graph.toml must parse");
        assert!(!config.cofunding_window_hours.refs.is_empty());
        assert!(!config.amount_similarity_pct.refs.is_empty());
        assert!(!config.min_cluster_size.refs.is_empty());
        assert!(!config.min_funder_sol_amount.refs.is_empty());
        assert!(!config.indexer_batch_size.refs.is_empty());
        assert!(!config.cluster_ttl_hours.refs.is_empty());
    }

    #[test]
    fn all_thresholds_have_rationale() {
        let path = workspace_root().join("config/graph.toml");
        let config = load_graph_config(&path).expect("config/graph.toml must parse");
        assert!(!config.cofunding_window_hours.rationale.is_empty());
        assert!(!config.amount_similarity_pct.rationale.is_empty());
        assert!(!config.min_cluster_size.rationale.is_empty());
        assert!(!config.min_funder_sol_amount.rationale.is_empty());
        assert!(!config.indexer_batch_size.rationale.is_empty());
        assert!(!config.cluster_ttl_hours.rationale.is_empty());
    }

    #[test]
    fn missing_config_file_returns_descriptive_error() {
        let err = load_graph_config("/nonexistent/graph.toml")
            .expect_err("must fail on missing file");
        assert!(err.to_string().contains("failed to read graph config"));
    }

    #[test]
    fn invalid_toml_returns_parse_error() {
        use std::io::Write;
        // Write a temp file with bad TOML and attempt to parse it.
        let tmp_path = std::env::temp_dir().join("mg_graph_bad_test.toml");
        {
            let mut f = std::fs::File::create(&tmp_path).unwrap();
            write!(f, "[graph\nbad toml").unwrap();
        }
        let err = load_graph_config(&tmp_path).expect_err("must fail on bad TOML");
        let _ = std::fs::remove_file(&tmp_path);
        assert!(err.to_string().contains("failed to parse graph config"));
    }
}
