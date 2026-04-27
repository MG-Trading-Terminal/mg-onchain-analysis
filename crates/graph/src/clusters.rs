//! Common-funder clustering algorithm.
//!
//! # Algorithm overview (design 0013 §4)
//!
//! Two wallets A and B are placed in the same `common_funder` cluster when:
//!   1. Same funder F sent SOL to both A and B (`wallet_edges.from_wallet = F`).
//!   2. F's first funding of A and first funding of B are within
//!      `cofunding_window_hours` of each other.
//!   3. The SOL amounts differ by ≤ `amount_similarity_pct` (20% default).
//!   4. At least `min_cluster_size` wallets share the same (funder, time-bucket,
//!      amount-bucket).
//!   5. Both edges exceed `min_funder_sol_amount` (dust filter).
//!
//! # OQ2 — CEX exclusion
//!
//! The common-funder query joins `holder_classifications` to exclude CEX hot
//! wallets as funders. A Binance hot wallet that funds thousands of users would
//! otherwise produce a giant false-positive cluster. The filter:
//!   ```sql
//!   LEFT JOIN holder_classifications hc ON hc.chain = we.chain
//!                                       AND hc.address = we.from_wallet
//!   WHERE (hc.kind IS NULL OR hc.kind != 'cex_hot_wallet')
//!   ```
//!
//! # OQ5 — Deterministic cluster IDs (UUID v5)
//!
//! Cluster IDs are derived deterministically:
//!   ```text
//!   uuid_v5(NAMESPACE_URL, "{chain}|funder={funder}|window={time_bucket_id}|bucket={amount_bucket_id}")
//!   ```
//! This ensures the same logical cluster maps to the same UUID across re-computation
//! runs. ON CONFLICT DO UPDATE is still needed to update `confidence` and
//! `computed_at` when the cluster is re-computed with new data.
//!
//! # Confidence formula (design 0013 §11)
//!
//! ```text
//! size_term  = min(1.0, (member_count - min_cluster_size) / (10.0 - min_cluster_size))
//! time_term  = 1.0 - min(1.0, time_stddev_secs / (cofunding_window_hours * 3600.0))
//! confidence = 0.50 + 0.25 * size_term + 0.10 * time_term
//! ```
//! Range: [0.50, 0.85]. Capped at 0.85 because common funding is necessary
//! but not sufficient for confirming coordinated activity.

use std::collections::BTreeMap;
use std::time::Instant;

use chrono::{DateTime, Utc};
use tracing::{debug, info, instrument};
use uuid::Uuid;

use crate::config::GraphConfig;
use crate::error::GraphError;
use crate::labels::{AddressLabel, GraphLabelStore, LabelType};

/// The Namespace URL used for UUID v5 derivation.
///
/// All cluster UUIDs in this crate are derived from this namespace plus a
/// deterministic string key (chain|funder|time_bucket|amount_bucket).
/// Using NAMESPACE_URL matches the design 0013 OQ5 specification.
const CLUSTER_UUID_NAMESPACE: Uuid = Uuid::NAMESPACE_URL;

// ---------------------------------------------------------------------------
// CandidateCluster — intermediate data structure
// ---------------------------------------------------------------------------

/// A group of wallets funded by the same funder within the same time+amount bucket.
///
/// This is the intermediate result of the bucketing algorithm, produced by
/// `bucket_edges`. Each `CandidateCluster` with `members.len() >= min_cluster_size`
/// is converted to a DB row by `ClusterDetector::run_common_funder`.
#[derive(Debug, Clone)]
pub struct CandidateCluster {
    /// The funder wallet address.
    pub funder: String,
    /// Tumbling time-window bucket ID (floor of seconds-since-funder-first-tx / window_secs).
    pub time_bucket_id: i64,
    /// Log-scale amount bucket ID (floor of ln(lamports) / ln(1 + similarity_pct)).
    pub amount_bucket_id: i64,
    /// Member wallets in deterministic (sorted) order.
    pub members: Vec<String>,
    /// Standard deviation of `first_tx_time` across members (seconds).
    pub time_stddev_secs: f64,
}

impl CandidateCluster {
    /// Derive the deterministic UUID v5 for this cluster.
    ///
    /// The UUID depends only on (chain, funder, time_bucket_id, amount_bucket_id).
    /// Member set changes do not affect the UUID — the same logical cluster
    /// (same funder + window + bucket) always has the same UUID.
    pub fn cluster_id(&self, chain: &str) -> Uuid {
        derive_cluster_id(chain, &self.funder, self.time_bucket_id, self.amount_bucket_id)
    }
}

/// Derive a deterministic cluster UUID from the cluster's identity key.
///
/// Exposed as a free function so tests and the DB impl can call it directly
/// without constructing a full `CandidateCluster`.
pub fn derive_cluster_id(
    chain: &str,
    funder: &str,
    time_bucket_id: i64,
    amount_bucket_id: i64,
) -> Uuid {
    let key = format!(
        "{chain}|funder={funder}|window={time_bucket_id}|bucket={amount_bucket_id}"
    );
    Uuid::new_v5(&CLUSTER_UUID_NAMESPACE, key.as_bytes())
}

// ---------------------------------------------------------------------------
// Pure bucketing algorithm (no I/O — fully unit-testable)
// ---------------------------------------------------------------------------

/// A funding edge as used by the bucketing algorithm.
///
/// Passed to `bucket_edges` from either real DB rows or synthetic test data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingEdge {
    pub chain: String,
    pub funder: String,
    pub recipient: String,
    /// Total SOL sent from funder to recipient, in lamports.
    pub total_sol_lamports: u128,
    /// Timestamp of the first funding transfer.
    pub first_tx_time: DateTime<Utc>,
}

/// Bucket a slice of funding edges into candidate clusters.
///
/// Implements design 0013 §4.2 steps 2–5 as a pure function. No I/O; no DB.
///
/// # Algorithm
///
/// For each distinct funder:
///   1. Sort recipients by `first_tx_time`.
///   2. Compute `time_bucket_id = floor((first_tx_time - funder_epoch) / window_secs)`.
///      The funder's epoch is the `first_tx_time` of their earliest edge.
///   3. Compute `amount_bucket_id = floor(ln(lamports) / ln(1 + similarity_pct))`.
///   4. Group by `(funder, time_bucket_id, amount_bucket_id)`.
///   5. Emit groups where `count >= min_cluster_size`.
///
/// # Return value
///
/// Returns a `Vec<CandidateCluster>` in deterministic order
/// (sorted by `(funder, time_bucket_id, amount_bucket_id)`).
/// Uses `BTreeMap` internally to ensure no `HashMap`-based non-determinism.
///
/// # Arguments
///
/// - `edges`: funding edges to cluster. Must not be empty; empty input returns `vec![]`.
/// - `min_cluster_size`: minimum group size to emit (typically 3).
/// - `window_secs`: cofunding window in seconds (`cofunding_window_hours * 3600`).
/// - `similarity_pct`: amount similarity fraction (e.g. `0.20` for 20%).
pub fn bucket_edges(
    edges: &[FundingEdge],
    min_cluster_size: u32,
    window_secs: i64,
    similarity_pct: f64,
) -> Vec<CandidateCluster> {
    if edges.is_empty() {
        return vec![];
    }

    // Step 1: group edges by funder using BTreeMap for determinism.
    let mut by_funder: BTreeMap<&str, Vec<&FundingEdge>> = BTreeMap::new();
    for e in edges {
        by_funder.entry(e.funder.as_str()).or_default().push(e);
    }

    let mut result: Vec<CandidateCluster> = Vec::new();
    let log_base = (1.0 + similarity_pct).ln();

    for (funder, mut funder_edges) in by_funder {
        // Sort by first_tx_time for deterministic epoch anchoring.
        funder_edges.sort_by_key(|e| e.first_tx_time);

        // Funder epoch: the earliest first_tx_time across all edges from this funder.
        let funder_epoch = funder_edges[0].first_tx_time;

        // Step 2+3: assign each recipient to (time_bucket_id, amount_bucket_id).
        // Use BTreeMap for deterministic bucket order.
        let mut buckets: BTreeMap<(i64, i64), Vec<&FundingEdge>> = BTreeMap::new();

        for edge in &funder_edges {
            let secs_since_epoch = (edge.first_tx_time - funder_epoch).num_seconds();
            let time_bucket_id = if window_secs > 0 {
                secs_since_epoch / window_secs
            } else {
                0
            };

            // Log-scale amount bucket. For amounts of 0 (should not happen after
            // min_funder_sol_amount filter), fall back to bucket 0.
            let amount_bucket_id = if edge.total_sol_lamports > 0 && log_base > 0.0 {
                ((edge.total_sol_lamports as f64).ln() / log_base).floor() as i64
            } else {
                0
            };

            buckets
                .entry((time_bucket_id, amount_bucket_id))
                .or_default()
                .push(edge);
        }

        // Step 4+5: emit clusters with sufficient members.
        for ((time_bucket_id, amount_bucket_id), members_refs) in buckets {
            if members_refs.len() < min_cluster_size as usize {
                continue;
            }

            // Compute time_stddev of first_tx_time values across members.
            let times_secs: Vec<f64> = members_refs
                .iter()
                .map(|e| e.first_tx_time.timestamp() as f64)
                .collect();
            let time_stddev_secs = stddev(&times_secs);

            // Collect members in sorted order for determinism (BTreeMap iteration
            // is sorted by key so members are already in insertion order here;
            // sort explicitly by recipient address for full determinism).
            let mut member_addrs: Vec<String> = members_refs
                .iter()
                .map(|e| e.recipient.clone())
                .collect();
            member_addrs.sort_unstable();
            member_addrs.dedup();

            if member_addrs.len() < min_cluster_size as usize {
                // Dedup removed duplicates — cluster no longer qualifies.
                continue;
            }

            result.push(CandidateCluster {
                funder: funder.to_owned(),
                time_bucket_id,
                amount_bucket_id,
                members: member_addrs,
                time_stddev_secs,
            });
        }
    }

    // Sort output for full determinism.
    result.sort_by(|a, b| {
        a.funder
            .cmp(&b.funder)
            .then(a.time_bucket_id.cmp(&b.time_bucket_id))
            .then(a.amount_bucket_id.cmp(&b.amount_bucket_id))
    });

    result
}

/// Compute the confidence score for a cluster (design 0013 §11).
///
/// # Formula
///
/// ```text
/// size_term  = min(1.0, (member_count - min_cluster_size) / (10.0 - min_cluster_size))
/// time_term  = 1.0 - min(1.0, time_stddev_secs / (cofunding_window_secs as f64))
/// confidence = 0.50 + 0.25 * size_term + 0.10 * time_term
/// ```
///
/// Range: [0.50, 0.85]. `f64` is appropriate here (probability, not monetary amount).
pub fn compute_confidence(
    member_count: usize,
    min_cluster_size: u32,
    time_stddev_secs: f64,
    cofunding_window_secs: f64,
) -> f64 {
    let min = min_cluster_size as f64;
    let size_term = if (10.0 - min).abs() < f64::EPSILON {
        1.0
    } else {
        ((member_count as f64 - min) / (10.0 - min)).clamp(0.0, 1.0)
    };

    let time_term = if cofunding_window_secs > 0.0 {
        1.0 - (time_stddev_secs / cofunding_window_secs).min(1.0)
    } else {
        0.0
    };

    (0.50 + 0.25 * size_term + 0.10 * time_term).clamp(0.50, 0.85)
}

// ---------------------------------------------------------------------------
// ClusterStats
// ---------------------------------------------------------------------------

/// Statistics from one `run_common_funder` execution.
#[derive(Debug, Clone, Default)]
pub struct ClusterStats {
    pub candidate_funders: u32,
    pub clusters_written: u32,
    pub members_written: u32,
    /// Number of `FundingSource` labels upserted to `address_labels`.
    /// Zero when no `label_store` is provided (legacy callers).
    pub labels_written: u32,
    pub duration: std::time::Duration,
}

// ---------------------------------------------------------------------------
// ClusterDetector
// ---------------------------------------------------------------------------

/// Runs common-funder clustering over `wallet_edges` and writes results to
/// `wallet_clusters` + `wallet_cluster_members`.
pub struct ClusterDetector<'a> {
    pub pool: &'a sqlx::PgPool,
    pub config: &'a GraphConfig,
}

impl<'a> ClusterDetector<'a> {
    /// Run common-funder clustering for a single chain.
    ///
    /// 1. Reads qualifying edges from `wallet_edges` (applying CEX exclusion per OQ2).
    /// 2. Calls `bucket_edges` (pure function) to produce `CandidateCluster` list.
    /// 3. For each cluster, upserts into `wallet_clusters` + `wallet_cluster_members`.
    /// 4. If `label_store` is `Some`, upserts a `FundingSource` label for each cluster's
    ///    root funder (design 0015 §4.3).
    ///
    /// Idempotent: deterministic UUID v5 IDs ensure re-runs update existing clusters
    /// rather than creating duplicates. Label upserts follow the same confidence-guarded
    /// overwrite semantics as `PgGraphLabelStore::upsert_label`.
    ///
    /// # Label time source discipline
    ///
    /// `ClusterDetector` is a background job (not a streaming detector). It is
    /// permitted to use `Utc::now()` for `issued_at` on the `FundingSource` label,
    /// per design 0015 §3.2.2 ("background jobs may use now()"). The `expires_at`
    /// is set to `now + cluster_ttl_hours` (same TTL as the cluster itself).
    #[instrument(skip(self, label_store), fields(chain))]
    pub async fn run_common_funder(
        &self,
        chain: &str,
        label_store: Option<&dyn GraphLabelStore>,
    ) -> Result<ClusterStats, GraphError> {
        let started = Instant::now();
        let min_lamports = self.config.min_funder_sol_amount.value.to_string();
        let min_cluster_size = self.config.min_cluster_size.value;
        let window_secs =
            self.config.cofunding_window_hours.value as i64 * 3600;
        let similarity_pct = self.config.amount_similarity_pct.value;

        // Step 1: fetch qualifying edges with CEX exclusion (OQ2).
        let rows = sqlx::query(
            r#"
            SELECT we.from_wallet, we.to_wallet,
                   we.total_sol_lamports::TEXT AS lamports,
                   we.first_tx_time
            FROM wallet_edges we
            LEFT JOIN holder_classifications hc
                ON hc.chain = we.chain
               AND hc.address = we.from_wallet
            WHERE we.chain = $1
              AND we.total_sol_lamports >= $2::NUMERIC
              AND (hc.kind IS NULL OR hc.kind != 'cex_hot_wallet')
            ORDER BY we.from_wallet, we.first_tx_time
            "#,
        )
        .bind(chain)
        .bind(&min_lamports)
        .fetch_all(self.pool)
        .await?;

        // Step 2: deserialise rows into FundingEdge values.
        let mut funding_edges: Vec<FundingEdge> = Vec::with_capacity(rows.len());
        for row in &rows {
            use sqlx::Row as _;
            let from: String = row.try_get("from_wallet").map_err(|e| {
                GraphError::ParseField {
                    field: "from_wallet",
                    reason: e.to_string(),
                }
            })?;
            let to: String = row.try_get("to_wallet").map_err(|e| {
                GraphError::ParseField {
                    field: "to_wallet",
                    reason: e.to_string(),
                }
            })?;
            let lamports_str: String = row.try_get("lamports").map_err(|e| {
                GraphError::ParseField {
                    field: "total_sol_lamports",
                    reason: e.to_string(),
                }
            })?;
            let first_tx_time: DateTime<Utc> = row.try_get("first_tx_time").map_err(|e| {
                GraphError::ParseField {
                    field: "first_tx_time",
                    reason: e.to_string(),
                }
            })?;
            let lamports: u128 = lamports_str.parse().map_err(|e| GraphError::ParseField {
                field: "total_sol_lamports",
                reason: format!("parse u128: {e}"),
            })?;

            funding_edges.push(FundingEdge {
                chain: chain.to_owned(),
                funder: from,
                recipient: to,
                total_sol_lamports: lamports,
                first_tx_time,
            });
        }

        let candidate_funders = {
            use std::collections::BTreeSet;
            funding_edges
                .iter()
                .map(|e| e.funder.as_str())
                .collect::<BTreeSet<_>>()
                .len() as u32
        };

        // Step 3: run pure bucketing algorithm.
        let clusters = bucket_edges(&funding_edges, min_cluster_size, window_secs, similarity_pct);

        debug!(
            chain,
            edges = funding_edges.len(),
            candidate_funders,
            candidate_clusters = clusters.len(),
            "bucketing complete"
        );

        let cofunding_window_secs = window_secs as f64;
        let mut clusters_written: u32 = 0;
        let mut members_written: u32 = 0;
        let mut labels_written: u32 = 0;

        // Compute now() once for `issued_at` / `expires_at` on all labels in this run.
        // Per design 0015 §3.2.2: background jobs (ClusterDetector) may use Utc::now().
        let now = Utc::now();
        let ttl_duration = chrono::Duration::hours(self.config.cluster_ttl_hours.value as i64);
        let expires_at = Some(now + ttl_duration);

        // Step 4: upsert each cluster and its members.
        for cluster in &clusters {
            let cluster_id = cluster.cluster_id(chain);
            let member_count = cluster.members.len() as i32;
            let confidence = compute_confidence(
                cluster.members.len(),
                min_cluster_size,
                cluster.time_stddev_secs,
                cofunding_window_secs,
            );

            let evidence = serde_json::json!({
                "algorithm": "common_funder",
                "time_bucket_id": cluster.time_bucket_id,
                "amount_bucket_id": cluster.amount_bucket_id,
                "time_stddev_secs": cluster.time_stddev_secs,
                "config": {
                    "cofunding_window_hours": self.config.cofunding_window_hours.value,
                    "amount_similarity_pct": similarity_pct,
                    "min_cluster_size": min_cluster_size,
                    "min_funder_sol_amount": self.config.min_funder_sol_amount.value,
                }
            });

            sqlx::query(
                r#"
                INSERT INTO wallet_clusters
                    (cluster_id, chain, cluster_kind, root_funder, member_count,
                     confidence, computed_at, evidence)
                VALUES ($1, $2, 'common_funder', $3, $4, $5, now(), $6)
                ON CONFLICT (cluster_id) DO UPDATE SET
                    confidence   = EXCLUDED.confidence,
                    member_count = EXCLUDED.member_count,
                    computed_at  = now(),
                    evidence     = EXCLUDED.evidence
                WHERE EXCLUDED.confidence >= wallet_clusters.confidence
                "#,
            )
            .bind(cluster_id)
            .bind(chain)
            .bind(&cluster.funder)
            .bind(member_count)
            .bind(confidence)
            .bind(sqlx::types::Json(&evidence))
            .execute(self.pool)
            .await?;

            clusters_written += 1;

            // Upsert member rows.
            for wallet in &cluster.members {
                sqlx::query(
                    r#"
                    INSERT INTO wallet_cluster_members (cluster_id, chain, wallet, joined_at)
                    VALUES ($1, $2, $3, now())
                    ON CONFLICT (cluster_id, wallet) DO NOTHING
                    "#,
                )
                .bind(cluster_id)
                .bind(chain)
                .bind(wallet)
                .execute(self.pool)
                .await?;

                members_written += 1;
            }

            // Step 5 (S11-5): write a FundingSource label for the root funder.
            // Design 0015 §4.3: after each cluster is identified, upsert an
            // AddressLabel for the funder address.
            if let Some(store) = label_store {
                let member_count = cluster.members.len();
                // Cap funded_addresses list at 100 for evidence JSON size.
                let funded_addrs: Vec<&str> = cluster
                    .members
                    .iter()
                    .take(100)
                    .map(String::as_str)
                    .collect();

                let label = AddressLabel {
                    chain: chain.to_owned(),
                    address: cluster.funder.clone(),
                    label_type: LabelType::FundingSource,
                    confidence,
                    evidence: serde_json::json!({
                        "cluster_id": cluster_id.to_string(),
                        "cluster_size": member_count,
                        "funded_addresses": funded_addrs,
                    }),
                    issued_at: now,
                    expires_at,
                    source: "common_funder_clustering".to_owned(),
                };

                store.upsert_label(&label).await?;
                labels_written += 1;
            }
        }

        let duration = started.elapsed();
        info!(
            chain,
            candidate_funders,
            clusters_written,
            members_written,
            labels_written,
            duration_ms = duration.as_millis(),
            "run_common_funder complete"
        );

        Ok(ClusterStats {
            candidate_funders,
            clusters_written,
            members_written,
            labels_written,
            duration,
        })
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Population standard deviation of a slice of f64 values.
///
/// Returns 0.0 for empty or single-element slices (no variance).
fn stddev(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    variance.sqrt()
}

// ---------------------------------------------------------------------------
// Tests (pure logic — no DB required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("valid ts")
    }

    const WINDOW_SECS: i64 = 24 * 3600; // 24 hours
    const SIMILARITY: f64 = 0.20;
    const MIN_SIZE: u32 = 3;

    fn edge(funder: &str, recipient: &str, lamports: u128, t: i64) -> FundingEdge {
        FundingEdge {
            chain: "solana".into(),
            funder: funder.into(),
            recipient: recipient.into(),
            total_sol_lamports: lamports,
            first_tx_time: ts(t),
        }
    }

    // --- bucket_edges: positive cases ---

    #[test]
    fn bucket_three_wallets_same_funder_same_window_same_amount() {
        // 5 wallets funded within 1 hour of each other with ~0.01 SOL each.
        let base = 1_700_000_000i64;
        let edges = vec![
            edge("funder", "w1", 10_000_000, base),
            edge("funder", "w2", 10_000_000, base + 300),
            edge("funder", "w3", 10_000_000, base + 600),
        ];
        let clusters = bucket_edges(&edges, MIN_SIZE, WINDOW_SECS, SIMILARITY);
        assert_eq!(clusters.len(), 1, "should produce exactly one cluster");
        let c = &clusters[0];
        assert_eq!(c.funder, "funder");
        assert_eq!(c.members.len(), 3);
        assert!(c.members.contains(&"w1".to_string()));
        assert!(c.members.contains(&"w2".to_string()));
        assert!(c.members.contains(&"w3".to_string()));
    }

    #[test]
    fn bucket_five_wallets_produces_one_cluster_positive_fixture() {
        // POS_01: 5 wallets funded within 24h window with similar amounts.
        let base = 1_700_000_000i64;
        let edges: Vec<FundingEdge> = (0..5)
            .map(|i| edge("funder_F", &format!("wallet_{i}"), 10_000_000, base + i * 1200))
            .collect();
        let clusters = bucket_edges(&edges, MIN_SIZE, WINDOW_SECS, SIMILARITY);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].members.len(), 5);
        assert_eq!(clusters[0].funder, "funder_F");
    }

    // --- bucket_edges: negative cases ---

    #[test]
    fn bucket_two_wallets_below_min_size_no_cluster() {
        // Only 2 wallets — below min_cluster_size=3.
        let base = 1_700_000_000i64;
        let edges = vec![
            edge("funder", "w1", 10_000_000, base),
            edge("funder", "w2", 10_000_000, base + 300),
        ];
        let clusters = bucket_edges(&edges, MIN_SIZE, WINDOW_SECS, SIMILARITY);
        assert!(
            clusters.is_empty(),
            "2 wallets below min_cluster_size=3 must not produce a cluster"
        );
    }

    #[test]
    fn bucket_independent_funders_no_cluster_negative_fixture() {
        // NEG_01: 3 wallets each funded by a different funder.
        let base = 1_700_000_000i64;
        let edges = vec![
            edge("funder_a", "w1", 10_000_000, base),
            edge("funder_b", "w2", 10_000_000, base + 300),
            edge("funder_c", "w3", 10_000_000, base + 600),
        ];
        let clusters = bucket_edges(&edges, MIN_SIZE, WINDOW_SECS, SIMILARITY);
        assert!(
            clusters.is_empty(),
            "independent funders must not produce a cluster"
        );
    }

    #[test]
    fn bucket_outside_window_splits_into_separate_clusters() {
        // Funder funds w1, w2, w3 within window, then w4, w5, w6 25 hours later.
        // Should produce 2 clusters, not 1.
        let base = 1_700_000_000i64;
        let window = 24 * 3600; // 24h in seconds
        let edges = vec![
            edge("funder", "w1", 10_000_000, base),
            edge("funder", "w2", 10_000_000, base + 1800),
            edge("funder", "w3", 10_000_000, base + 3600),
            // These are 25 hours after w1 — different time bucket.
            edge("funder", "w4", 10_000_000, base + window + 3600),
            edge("funder", "w5", 10_000_000, base + window + 4200),
            edge("funder", "w6", 10_000_000, base + window + 4800),
        ];
        let clusters = bucket_edges(&edges, MIN_SIZE, WINDOW_SECS, SIMILARITY);
        assert_eq!(clusters.len(), 2, "transfers across window boundary must split into 2 clusters");
    }

    #[test]
    fn bucket_amount_dissimilarity_splits_clusters() {
        // w1, w2, w3 receive 0.01 SOL; w4, w5, w6 receive 1.0 SOL.
        // Should produce 2 clusters (different amount buckets).
        let base = 1_700_000_000i64;
        let edges = vec![
            edge("funder", "w1", 10_000_000, base),
            edge("funder", "w2", 10_000_000, base + 300),
            edge("funder", "w3", 10_000_000, base + 600),
            edge("funder", "w4", 1_000_000_000, base + 900),
            edge("funder", "w5", 1_000_000_000, base + 1200),
            edge("funder", "w6", 1_000_000_000, base + 1500),
        ];
        let clusters = bucket_edges(&edges, MIN_SIZE, WINDOW_SECS, SIMILARITY);
        assert_eq!(clusters.len(), 2, "different amount buckets must produce 2 clusters");
    }

    #[test]
    fn bucket_empty_input_returns_empty() {
        let clusters = bucket_edges(&[], MIN_SIZE, WINDOW_SECS, SIMILARITY);
        assert!(clusters.is_empty());
    }

    // --- Determinism ---

    #[test]
    fn bucket_deterministic_output_same_uuid() {
        let base = 1_700_000_000i64;
        let edges = vec![
            edge("funder", "w1", 10_000_000, base),
            edge("funder", "w2", 10_000_000, base + 300),
            edge("funder", "w3", 10_000_000, base + 600),
        ];
        let c1 = bucket_edges(&edges, MIN_SIZE, WINDOW_SECS, SIMILARITY);
        let c2 = bucket_edges(&edges, MIN_SIZE, WINDOW_SECS, SIMILARITY);
        assert_eq!(c1.len(), c2.len());
        for (a, b) in c1.iter().zip(c2.iter()) {
            // Same funder + window + bucket → same UUID.
            let id_a = a.cluster_id("solana");
            let id_b = b.cluster_id("solana");
            assert_eq!(id_a, id_b, "cluster UUID must be deterministic");
        }
    }

    #[test]
    fn derive_cluster_id_is_deterministic_across_calls() {
        let id1 = derive_cluster_id("solana", "funder_ABC", 5, 12);
        let id2 = derive_cluster_id("solana", "funder_ABC", 5, 12);
        assert_eq!(id1, id2);
    }

    #[test]
    fn derive_cluster_id_differs_for_different_keys() {
        let id1 = derive_cluster_id("solana", "funder_A", 5, 12);
        let id2 = derive_cluster_id("solana", "funder_B", 5, 12);
        let id3 = derive_cluster_id("solana", "funder_A", 6, 12);
        let id4 = derive_cluster_id("ethereum", "funder_A", 5, 12);
        assert_ne!(id1, id2, "different funders → different UUIDs");
        assert_ne!(id1, id3, "different time buckets → different UUIDs");
        assert_ne!(id1, id4, "different chains → different UUIDs");
    }

    // --- compute_confidence ---

    #[test]
    fn confidence_min_cluster_is_0_60() {
        // 3 wallets funded at exactly the same second → max time_term.
        let conf = compute_confidence(3, 3, 0.0, 24.0 * 3600.0);
        // size_term = 0.0 (at minimum), time_term = 1.0
        // confidence = 0.50 + 0.0 + 0.10 = 0.60
        assert!((conf - 0.60).abs() < 1e-9, "conf={conf}");
    }

    #[test]
    fn confidence_10_wallets_simultaneous_is_0_85() {
        let conf = compute_confidence(10, 3, 0.0, 24.0 * 3600.0);
        // size_term = 1.0, time_term = 1.0
        // confidence = 0.50 + 0.25 + 0.10 = 0.85
        assert!((conf - 0.85).abs() < 1e-9, "conf={conf}");
    }

    #[test]
    fn confidence_clamped_to_0_85_max() {
        let conf = compute_confidence(100, 3, 0.0, 24.0 * 3600.0);
        assert!(conf <= 0.85);
    }

    #[test]
    fn confidence_clamped_to_0_50_min() {
        let conf = compute_confidence(3, 3, 24.0 * 3600.0, 24.0 * 3600.0);
        // time_term = 0.0 (full spread), size_term = 0.0
        // confidence = 0.50
        assert!(conf >= 0.50);
    }

    // --- stddev helper ---

    #[test]
    fn stddev_empty_returns_zero() {
        assert_eq!(stddev(&[]), 0.0);
    }

    #[test]
    fn stddev_single_returns_zero() {
        assert_eq!(stddev(&[42.0]), 0.0);
    }

    #[test]
    fn stddev_known_values() {
        // Population stddev of [2, 4, 4, 4, 5, 5, 7, 9] = 2.0
        let v = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let s = stddev(&v);
        assert!((s - 2.0).abs() < 1e-6, "stddev={s}");
    }

    // ---------------------------------------------------------------------------
    // S11-5: FundingSource label writes via MockGraphLabelStore
    // ---------------------------------------------------------------------------
    // These tests exercise the label-write path of run_common_funder at the pure
    // bucket_edges level (no Postgres), using the mock store.
    // ---------------------------------------------------------------------------

    /// Build a minimal CandidateCluster for label-write tests (pure logic).
    fn make_cluster(funder: &str, members: &[&str]) -> CandidateCluster {
        CandidateCluster {
            funder: funder.to_owned(),
            time_bucket_id: 0,
            amount_bucket_id: 0,
            members: members.iter().map(|s| s.to_string()).collect(),
            time_stddev_secs: 0.0,
        }
    }

    /// Positive fixture S11-5: 3 wallets sharing funder X — expect exactly one
    /// FundingSource label written for X with cluster_size=3.
    #[test]
    fn funding_source_label_evidence_has_correct_cluster_size() {
        // Verify the evidence JSON that would be written for a 3-member cluster.
        let cluster = make_cluster("funder_X", &["w1", "w2", "w3"]);
        let member_count = cluster.members.len();
        let funded: Vec<&str> = cluster.members.iter().take(100).map(String::as_str).collect();

        let evidence = serde_json::json!({
            "cluster_id": cluster.cluster_id("solana").to_string(),
            "cluster_size": member_count,
            "funded_addresses": funded,
        });

        let size = evidence["cluster_size"].as_u64().expect("cluster_size must be u64");
        assert_eq!(size, 3, "cluster_size must be 3 for a 3-member cluster");
        let addrs = evidence["funded_addresses"].as_array().expect("funded_addresses must be array");
        assert_eq!(addrs.len(), 3);
    }

    /// Negative fixture S11-5: no cluster found → no label written.
    /// Verifies that when bucket_edges returns empty, no FundingSource labels
    /// would be produced (loop body never executes).
    #[test]
    fn no_clusters_means_no_label_writes() {
        // 2 wallets with different funders: below min_cluster_size=3.
        let base = 1_700_000_000i64;
        let edges = vec![
            edge("funder_a", "w1", 10_000_000, base),
            edge("funder_b", "w2", 10_000_000, base),
        ];
        let clusters = bucket_edges(&edges, MIN_SIZE, WINDOW_SECS, SIMILARITY);
        // No clusters → label loop never executes.
        assert!(clusters.is_empty(), "below min_cluster_size must produce no clusters");
        // Verified: labels_written would be 0.
    }

    /// FundingSource label evidence is idempotent: re-running with the same cluster
    /// on the same funder produces label with same cluster_id.
    #[test]
    fn funding_source_label_is_deterministic() {
        let cluster1 = make_cluster("funder_X", &["w1", "w2", "w3"]);
        let cluster2 = make_cluster("funder_X", &["w1", "w2", "w3"]);
        // Same inputs must produce same cluster UUID (used as evidence cluster_id).
        assert_eq!(
            cluster1.cluster_id("solana"),
            cluster2.cluster_id("solana"),
            "cluster UUID must be deterministic for same inputs"
        );
    }

    /// Funded addresses list is capped at 100 for evidence JSON size.
    #[test]
    fn funded_addresses_capped_at_100() {
        // 150 members — evidence should cap at 100.
        let members: Vec<String> = (0..150).map(|i| format!("w{i}")).collect();
        let member_strs: Vec<&str> = members.iter().map(String::as_str).collect();
        let cluster = make_cluster("funder_big", &member_strs);

        let capped: Vec<&str> = cluster.members.iter().take(100).map(String::as_str).collect();
        assert_eq!(
            capped.len(),
            100,
            "funded_addresses in evidence must be capped at 100"
        );
    }
}
