//! Cluster read API: `ClusterStore` trait + `PgClusterStore` impl.
//!
//! Detectors that need cluster membership information depend on the `ClusterStore`
//! trait, not the concrete `PgClusterStore`. This allows tests to inject a
//! `MockClusterStore` without a live Postgres connection.
//!
//! # Object safety and dyn dispatch
//!
//! `ClusterStore` uses `#[async_trait]` (not native AFIT) to remain dyn-compatible.
//! AFIT (Rust 2024 `async fn` in traits) produces `impl Future` return types that
//! are NOT dyn-compatible. Since the design requires `&dyn ClusterStore` to pass
//! the store through `DetectorContext` (Phase 3 Sprint 8), we use `async_trait`
//! which boxes the futures internally — exactly the pattern already established by
//! `CheckpointStore` in `crates/storage`.
//!
//! Verify object safety: `let _: Box<dyn ClusterStore>;` must compile (checked in tests).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::Row as _;
use tracing::instrument;
use uuid::Uuid;

use crate::error::GraphError;

// ---------------------------------------------------------------------------
// ClusterKind
// ---------------------------------------------------------------------------

/// The algorithm that produced a cluster.
///
/// Variants correspond 1:1 to `cluster_kind` TEXT values in `wallet_clusters`.
/// `#[non_exhaustive]` allows Phase 3 variants without breaking downstream crates.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClusterKind {
    /// Common-funder: wallets funded by the same EOA within a time window (MVP).
    CommonFunder,
    /// Synchronized first-tx timing (Phase 3 Sprint 8 — deferred).
    SynchronizedActivity,
    /// EVM contract bytecode similarity (Phase 3 Sprint 9 — deferred, EVM only).
    BytecodeSimilar,
}

impl ClusterKind {
    /// Returns the DB column string for this kind.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            ClusterKind::CommonFunder => "common_funder",
            ClusterKind::SynchronizedActivity => "synchronized_activity",
            ClusterKind::BytecodeSimilar => "bytecode_similar",
        }
    }

    /// Parse from the DB column string.
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "common_funder" => Some(ClusterKind::CommonFunder),
            "synchronized_activity" => Some(ClusterKind::SynchronizedActivity),
            "bytecode_similar" => Some(ClusterKind::BytecodeSimilar),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// ClusterRef
// ---------------------------------------------------------------------------

/// A lightweight reference to a wallet cluster, returned by the read API.
///
/// Detectors consume this: check if two wallets are in the same cluster without
/// loading all member addresses.
#[derive(Debug, Clone)]
pub struct ClusterRef {
    pub cluster_id: Uuid,
    pub chain: String,
    pub cluster_kind: ClusterKind,
    /// The funding wallet that defines this cluster. `None` for non-funder kinds.
    pub root_funder: Option<String>,
    pub member_count: u32,
    /// Confidence that this cluster represents coordinated activity. [0.0, 1.0].
    /// `f64` is the correct type here (probability/ratio, not money).
    pub confidence: f64,
    pub computed_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// ClusterStore trait
// ---------------------------------------------------------------------------

/// The read API consumed by detectors that need cluster membership information.
///
/// All implementations must be `Send + Sync` for safe use across task boundaries.
///
/// # Dyn compatibility
///
/// This trait uses `#[async_trait]` so it is compatible with `dyn ClusterStore`
/// dispatch. This is necessary for Phase 3 Sprint 8 integration where the store
/// is carried in `DetectorContext` as `&dyn ClusterStore`. The pattern follows
/// `CheckpointStore` in `crates/storage`.
///
/// # Default method
///
/// `are_co_clustered` has a default implementation: two `wallet_cluster` lookups,
/// plus UUID comparison. `PgClusterStore` may override with a more efficient SQL query.
#[async_trait]
pub trait ClusterStore: Send + Sync {
    /// Returns the highest-confidence cluster (if any) that `wallet` belongs to
    /// on `chain`. Returns `Ok(None)` if the wallet is not clustered.
    async fn wallet_cluster(
        &self,
        chain: &str,
        wallet: &str,
    ) -> Result<Option<ClusterRef>, GraphError>;

    /// Returns all clusters that `wallet` belongs to on `chain`.
    ///
    /// Most detectors only need `wallet_cluster()`. This method is for evidence
    /// building where all cluster memberships are relevant.
    async fn all_clusters_for_wallet(
        &self,
        chain: &str,
        wallet: &str,
    ) -> Result<Vec<ClusterRef>, GraphError>;

    /// Returns all member wallet addresses of a cluster.
    ///
    /// Used by D05 integration: once two swap senders are found to be in the same
    /// cluster, fetch all members to build the evidence bundle.
    async fn cluster_members(&self, cluster_id: Uuid) -> Result<Vec<String>, GraphError>;

    /// Returns the cluster anchored to `root_funder` on `chain`, if one exists.
    ///
    /// Used by D04 integration: given the deployer address, find the cluster of
    /// wallets it funded.
    async fn funder_cluster(
        &self,
        chain: &str,
        root_funder: &str,
    ) -> Result<Option<ClusterRef>, GraphError>;

    /// Returns `true` if `wallet_a` and `wallet_b` are in the same cluster.
    ///
    /// Default implementation: two `wallet_cluster` lookups + UUID comparison.
    /// `PgClusterStore` may override with a single SQL query for efficiency.
    async fn are_co_clustered(
        &self,
        chain: &str,
        wallet_a: &str,
        wallet_b: &str,
    ) -> Result<bool, GraphError> {
        let a = self.wallet_cluster(chain, wallet_a).await?;
        let b = self.wallet_cluster(chain, wallet_b).await?;
        match (a, b) {
            (Some(ca), Some(cb)) => Ok(ca.cluster_id == cb.cluster_id),
            _ => Ok(false),
        }
    }
}

// ---------------------------------------------------------------------------
// PgClusterStore
// ---------------------------------------------------------------------------

/// Postgres-backed implementation of `ClusterStore`.
pub struct PgClusterStore {
    pub pool: sqlx::PgPool,
}

impl PgClusterStore {
    /// Construct a new `PgClusterStore` wrapping the given pool.
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ClusterStore for PgClusterStore {
    #[instrument(skip(self), fields(chain, wallet))]
    async fn wallet_cluster(
        &self,
        chain: &str,
        wallet: &str,
    ) -> Result<Option<ClusterRef>, GraphError> {
        // Uses idx_wallet_cluster_members_wallet on (chain, wallet).
        let row = sqlx::query(
            r#"
            SELECT wc.cluster_id, wc.chain, wc.cluster_kind, wc.root_funder,
                   wc.member_count, wc.confidence, wc.computed_at
            FROM wallet_cluster_members wcm
            JOIN wallet_clusters wc ON wc.cluster_id = wcm.cluster_id
            WHERE wcm.chain = $1
              AND wcm.wallet = $2
            ORDER BY wc.confidence DESC
            LIMIT 1
            "#,
        )
        .bind(chain)
        .bind(wallet)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            None => Ok(None),
            Some(r) => Ok(Some(parse_cluster_ref(&r)?)),
        }
    }

    #[instrument(skip(self), fields(chain, wallet))]
    async fn all_clusters_for_wallet(
        &self,
        chain: &str,
        wallet: &str,
    ) -> Result<Vec<ClusterRef>, GraphError> {
        let rows = sqlx::query(
            r#"
            SELECT wc.cluster_id, wc.chain, wc.cluster_kind, wc.root_funder,
                   wc.member_count, wc.confidence, wc.computed_at
            FROM wallet_cluster_members wcm
            JOIN wallet_clusters wc ON wc.cluster_id = wcm.cluster_id
            WHERE wcm.chain = $1
              AND wcm.wallet = $2
            ORDER BY wc.confidence DESC
            "#,
        )
        .bind(chain)
        .bind(wallet)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(parse_cluster_ref).collect()
    }

    #[instrument(skip(self), fields(%cluster_id))]
    async fn cluster_members(&self, cluster_id: Uuid) -> Result<Vec<String>, GraphError> {
        let rows = sqlx::query(
            r#"
            SELECT wallet
            FROM wallet_cluster_members
            WHERE cluster_id = $1
            ORDER BY wallet
            "#,
        )
        .bind(cluster_id)
        .fetch_all(&self.pool)
        .await?;

        let wallets: Vec<String> = rows
            .into_iter()
            .map(|r| {
                r.try_get::<String, _>("wallet").map_err(|e| GraphError::ParseField {
                    field: "wallet",
                    reason: e.to_string(),
                })
            })
            .collect::<Result<_, _>>()?;

        Ok(wallets)
    }

    #[instrument(skip(self), fields(chain, root_funder))]
    async fn funder_cluster(
        &self,
        chain: &str,
        root_funder: &str,
    ) -> Result<Option<ClusterRef>, GraphError> {
        let row = sqlx::query(
            r#"
            SELECT cluster_id, chain, cluster_kind, root_funder,
                   member_count, confidence, computed_at
            FROM wallet_clusters
            WHERE chain = $1
              AND root_funder = $2
            ORDER BY confidence DESC
            LIMIT 1
            "#,
        )
        .bind(chain)
        .bind(root_funder)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            None => Ok(None),
            Some(r) => Ok(Some(parse_cluster_ref(&r)?)),
        }
    }
}

// ---------------------------------------------------------------------------
// Row parser helper
// ---------------------------------------------------------------------------

fn parse_cluster_ref(row: &sqlx::postgres::PgRow) -> Result<ClusterRef, GraphError> {
    let cluster_id: Uuid = row.try_get("cluster_id").map_err(|e| GraphError::ParseField {
        field: "cluster_id",
        reason: e.to_string(),
    })?;
    let chain: String = row.try_get("chain").map_err(|e| GraphError::ParseField {
        field: "chain",
        reason: e.to_string(),
    })?;
    let kind_str: String = row.try_get("cluster_kind").map_err(|e| GraphError::ParseField {
        field: "cluster_kind",
        reason: e.to_string(),
    })?;
    let cluster_kind = ClusterKind::from_db_str(&kind_str).ok_or_else(|| GraphError::ParseField {
        field: "cluster_kind",
        reason: format!("unknown kind: {kind_str}"),
    })?;
    let root_funder: Option<String> = row.try_get("root_funder").map_err(|e| {
        GraphError::ParseField {
            field: "root_funder",
            reason: e.to_string(),
        }
    })?;
    let member_count: i32 = row.try_get("member_count").map_err(|e| GraphError::ParseField {
        field: "member_count",
        reason: e.to_string(),
    })?;
    let confidence: f64 = row.try_get("confidence").map_err(|e| GraphError::ParseField {
        field: "confidence",
        reason: e.to_string(),
    })?;
    let computed_at: DateTime<Utc> = row.try_get("computed_at").map_err(|e| {
        GraphError::ParseField {
            field: "computed_at",
            reason: e.to_string(),
        }
    })?;

    Ok(ClusterRef {
        cluster_id,
        chain,
        cluster_kind,
        root_funder,
        member_count: member_count as u32,
        confidence,
        computed_at,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that `Box<dyn ClusterStore>` compiles.
    ///
    /// `async_trait` makes the trait dyn-compatible by boxing futures.
    /// If this compiles, the trait can be used as `&dyn ClusterStore` in
    /// `DetectorContext` (Phase 3 Sprint 8).
    #[test]
    fn cluster_store_is_dyn_compatible() {
        // Both forms must compile.
        fn _accepts_dyn(_s: &dyn ClusterStore) {}
        fn _accepts_box(_s: Box<dyn ClusterStore>) {}
        // No runtime assertion — test passes if it compiles.
    }

    #[test]
    fn cluster_kind_roundtrip_db_str() {
        for kind in [
            ClusterKind::CommonFunder,
            ClusterKind::SynchronizedActivity,
            ClusterKind::BytecodeSimilar,
        ] {
            let s = kind.as_db_str();
            let parsed = ClusterKind::from_db_str(s);
            assert_eq!(parsed.as_ref(), Some(&kind), "roundtrip failed for {s}");
        }
    }

    #[test]
    fn cluster_kind_unknown_string_returns_none() {
        assert!(ClusterKind::from_db_str("nonexistent_kind").is_none());
    }
}
