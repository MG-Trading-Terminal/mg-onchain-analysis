//! Address labels: graph-global node annotations.
//!
//! # What this module provides
//!
//! - [`LabelType`] — enumeration of all label types (Sprint 11 set + future types).
//! - [`AddressLabel`] — one row from the `address_labels` table (V00011).
//! - [`GraphLabelStore`] trait — read/write API, dyn-compatible via `async_trait`.
//! - [`PgGraphLabelStore`] — Postgres implementation.
//!
//! # Distinction from `holder_classifications`
//!
//! `holder_classifications` (V00003) annotates per-token holder roles
//! (e.g. `vesting_contract`, `cex_hot_wallet`, `dex_pool`). These are local to
//! one token's holder set.
//!
//! `address_labels` (V00011) annotates a wallet address **globally** across all
//! tokens and chains. A single address can be both `DeployerEOA` and `Sybil`
//! concurrently. Labels carry confidence + TTL and are written by clustering
//! algorithms and detectors.
//!
//! # Time source discipline
//!
//! `issued_at` in the indexer write path MUST be derived from `block_time`,
//! not `Utc::now()` (gotcha #22 / #28). Background jobs (ClusterDetector,
//! static-seed loaders) may use `Utc::now()`.
//!
//! # Design reference
//!
//! `docs/designs/0015-crates-graph-phase3.md` §3.2 + §4.1

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row as _;
use tracing::instrument;

use crate::error::GraphError;

// ---------------------------------------------------------------------------
// LabelType
// ---------------------------------------------------------------------------

/// Enumeration of graph-global label types.
///
/// Serialises as `snake_case` for TOML config + JSON evidence fields.
///
/// `#[non_exhaustive]` allows future variants (Sprint 12: `SmartMoney` writes;
/// Phase 4: EVM-specific labels) without breaking downstream crates.
///
/// # Application-level enforcement
///
/// The `address_labels.label_type` column is `TEXT` (no Postgres CHECK constraint)
/// to avoid a migration for every new label type. This enum is the authoritative
/// list; `as_db_str` / `from_db_str` are the canonical conversions.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LabelType {
    /// Address deployed at least one token contract on this chain.
    /// Written by the indexer on `PoolEvent::Initialize`. Permanent (no TTL).
    DeployerEoa,
    /// Address that funded >= min_cluster_size wallets within a common-funder cluster.
    /// Written by `ClusterDetector::run_common_funder`. TTL = `cluster_ttl_hours`.
    FundingSource,
    /// Known DEX program or router address.
    /// Seeded from `token-registry/data/*.json`. Permanent.
    KnownDex,
    /// Known burn address (e.g. Solana null key `11111...1111`).
    /// Seeded from static data. Permanent.
    KnownBurn,
    /// Known CEX hot wallet.
    /// Seeded from static data. Permanent.
    KnownExchange,
    /// Address with historical P&L above threshold.
    /// Written by Sprint 12 SmartMoney labeller. TTL = 720h.
    SmartMoney,
    /// Confirmed Sybil address from D08 evaluation.
    /// Permanent with ON CONFLICT UPDATE (confidence refreshed on re-evaluation).
    Sybil,
}

impl LabelType {
    /// Returns the DB column string for this label type.
    ///
    /// These strings are the canonical values stored in `address_labels.label_type`.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            LabelType::DeployerEoa => "DeployerEOA",
            LabelType::FundingSource => "FundingSource",
            LabelType::KnownDex => "KnownDex",
            LabelType::KnownBurn => "KnownBurn",
            LabelType::KnownExchange => "KnownExchange",
            LabelType::SmartMoney => "SmartMoney",
            LabelType::Sybil => "Sybil",
        }
    }

    /// Parse from the DB column string. Returns `None` for unknown values.
    ///
    /// Unknown values should be surfaced as [`GraphError::UnknownLabelType`]
    /// at the call site, not silently dropped.
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "DeployerEOA" => Some(LabelType::DeployerEoa),
            "FundingSource" => Some(LabelType::FundingSource),
            "KnownDex" => Some(LabelType::KnownDex),
            "KnownBurn" => Some(LabelType::KnownBurn),
            "KnownExchange" => Some(LabelType::KnownExchange),
            "SmartMoney" => Some(LabelType::SmartMoney),
            "Sybil" => Some(LabelType::Sybil),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// AddressLabel
// ---------------------------------------------------------------------------

/// One row from the `address_labels` table.
///
/// Represents a graph-global annotation for a wallet address on a specific chain.
/// Multiple `AddressLabel` rows can exist for the same address (one per `label_type`).
#[derive(Debug, Clone)]
pub struct AddressLabel {
    /// Chain identifier (e.g. `"solana"`, `"ethereum"`).
    pub chain: String,
    /// Canonical wallet address for the chain.
    pub address: String,
    /// The label type.
    pub label_type: LabelType,
    /// Confidence in [0.0, 1.0]. `f64` is correct: probability, not money.
    pub confidence: f64,
    /// Structured evidence: algorithm parameters, tx hashes, cluster_id, etc.
    pub evidence: serde_json::Value,
    /// When this label was assigned. In indexer paths: derived from `block_time`.
    pub issued_at: DateTime<Utc>,
    /// Optional TTL. `None` = permanent.
    pub expires_at: Option<DateTime<Utc>>,
    /// Label source identifier. Examples: `"indexer_pool_initialize"`,
    /// `"common_funder_clustering"`, `"d08_sybil"`.
    pub source: String,
}

// ---------------------------------------------------------------------------
// GraphLabelStore trait
// ---------------------------------------------------------------------------

/// Read/write API for the `address_labels` table.
///
/// Uses `#[async_trait]` for dyn-compatibility — the same pattern as
/// `ClusterStore` in `api.rs` and `CheckpointStore` in `crates/storage`.
///
/// All implementations must be `Send + Sync` for use across `tokio::spawn`
/// task boundaries (gotcha #27).
#[async_trait]
pub trait GraphLabelStore: Send + Sync {
    /// Insert or update a label.
    ///
    /// `ON CONFLICT (chain, address, label_type) DO UPDATE` when the incoming
    /// confidence is >= the existing row's confidence, or when `expires_at` has
    /// passed. This ensures labels only move toward higher confidence, preventing
    /// noisy low-confidence re-evaluations from overwriting established labels.
    async fn upsert_label(&self, label: &AddressLabel) -> Result<(), GraphError>;

    /// Batch upsert — single `INSERT ... ON CONFLICT DO UPDATE` for efficiency.
    ///
    /// Empty slice is a no-op (no query issued).
    async fn upsert_labels(&self, labels: &[AddressLabel]) -> Result<(), GraphError>;

    /// All current (non-expired) labels for a given address.
    ///
    /// Returns labels where `expires_at IS NULL OR expires_at > now()`.
    /// Uses `idx_address_labels_addr` on `(chain, address)`.
    async fn get_labels(
        &self,
        chain: &str,
        address: &str,
    ) -> Result<Vec<AddressLabel>, GraphError>;

    /// All addresses with a given label type on a chain, filtered by minimum confidence.
    ///
    /// Used by D08 to fetch all current Sybil-labelled addresses for a chain.
    /// Returns only non-expired labels (`expires_at IS NULL OR expires_at > now()`).
    /// Results are ordered by `confidence DESC` for deterministic output.
    async fn addresses_with_label(
        &self,
        chain: &str,
        label_type: LabelType,
        min_confidence: f64,
    ) -> Result<Vec<AddressLabel>, GraphError>;

    /// Delete address labels written by indexer sources above a given block time.
    ///
    /// Used for reorg handling: when a reorg is detected at `reorg_block_time`,
    /// indexer-written labels (source IN `indexer_pool_initialize`,
    /// `indexer_token_metadata`) that were issued at or after that time are retracted.
    /// Cluster-derived labels are not affected (they are aggregate labels spanning
    /// many blocks).
    async fn delete_indexer_labels_after(
        &self,
        chain: &str,
        reorg_block_time: DateTime<Utc>,
    ) -> Result<u64, GraphError>;
}

// ---------------------------------------------------------------------------
// PgGraphLabelStore
// ---------------------------------------------------------------------------

/// Postgres-backed implementation of [`GraphLabelStore`].
pub struct PgGraphLabelStore {
    pub pool: sqlx::PgPool,
}

impl PgGraphLabelStore {
    /// Construct a new `PgGraphLabelStore` wrapping the given pool.
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl GraphLabelStore for PgGraphLabelStore {
    #[instrument(skip(self, label), fields(chain = %label.chain, address = %label.address, label_type = %label.label_type.as_db_str()))]
    async fn upsert_label(&self, label: &AddressLabel) -> Result<(), GraphError> {
        sqlx::query(
            r#"
            INSERT INTO address_labels
                (chain, address, label_type, confidence, evidence,
                 issued_at, expires_at, source, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())
            ON CONFLICT (chain, address, label_type) DO UPDATE
                SET confidence  = EXCLUDED.confidence,
                    evidence    = EXCLUDED.evidence,
                    issued_at   = EXCLUDED.issued_at,
                    expires_at  = EXCLUDED.expires_at,
                    source      = EXCLUDED.source,
                    updated_at  = now()
                WHERE EXCLUDED.confidence >= address_labels.confidence
                   OR address_labels.expires_at IS NOT NULL
                      AND address_labels.expires_at <= now()
            "#,
        )
        .bind(&label.chain)
        .bind(&label.address)
        .bind(label.label_type.as_db_str())
        .bind(label.confidence)
        .bind(&label.evidence)
        .bind(label.issued_at)
        .bind(label.expires_at)
        .bind(&label.source)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    #[instrument(skip(self, labels), fields(count = labels.len()))]
    async fn upsert_labels(&self, labels: &[AddressLabel]) -> Result<(), GraphError> {
        if labels.is_empty() {
            return Ok(());
        }
        // Execute sequentially within a single transaction for consistency.
        // At Sprint 11 scale (hundreds of labels per clustering run), this is
        // adequate. If batch performance becomes a concern, migrate to COPY or
        // a multi-row VALUES clause.
        let mut tx = self.pool.begin().await?;
        for label in labels {
            sqlx::query(
                r#"
                INSERT INTO address_labels
                    (chain, address, label_type, confidence, evidence,
                     issued_at, expires_at, source, updated_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())
                ON CONFLICT (chain, address, label_type) DO UPDATE
                    SET confidence  = EXCLUDED.confidence,
                        evidence    = EXCLUDED.evidence,
                        issued_at   = EXCLUDED.issued_at,
                        expires_at  = EXCLUDED.expires_at,
                        source      = EXCLUDED.source,
                        updated_at  = now()
                    WHERE EXCLUDED.confidence >= address_labels.confidence
                       OR address_labels.expires_at IS NOT NULL
                          AND address_labels.expires_at <= now()
                "#,
            )
            .bind(&label.chain)
            .bind(&label.address)
            .bind(label.label_type.as_db_str())
            .bind(label.confidence)
            .bind(&label.evidence)
            .bind(label.issued_at)
            .bind(label.expires_at)
            .bind(&label.source)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    #[instrument(skip(self), fields(chain, address))]
    async fn get_labels(
        &self,
        chain: &str,
        address: &str,
    ) -> Result<Vec<AddressLabel>, GraphError> {
        let rows = sqlx::query(
            r#"
            SELECT chain, address, label_type, confidence, evidence,
                   issued_at, expires_at, source
            FROM address_labels
            WHERE chain = $1
              AND address = $2
              AND (expires_at IS NULL OR expires_at > now())
            ORDER BY label_type
            "#,
        )
        .bind(chain)
        .bind(address)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(parse_address_label).collect()
    }

    #[instrument(skip(self), fields(chain, label_type = %label_type.as_db_str(), min_confidence))]
    async fn addresses_with_label(
        &self,
        chain: &str,
        label_type: LabelType,
        min_confidence: f64,
    ) -> Result<Vec<AddressLabel>, GraphError> {
        let rows = sqlx::query(
            r#"
            SELECT chain, address, label_type, confidence, evidence,
                   issued_at, expires_at, source
            FROM address_labels
            WHERE chain = $1
              AND label_type = $2
              AND confidence >= $3
              AND (expires_at IS NULL OR expires_at > now())
            ORDER BY confidence DESC, address
            "#,
        )
        .bind(chain)
        .bind(label_type.as_db_str())
        .bind(min_confidence)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(parse_address_label).collect()
    }

    #[instrument(skip(self), fields(chain, %reorg_block_time))]
    async fn delete_indexer_labels_after(
        &self,
        chain: &str,
        reorg_block_time: DateTime<Utc>,
    ) -> Result<u64, GraphError> {
        let result = sqlx::query(
            r#"
            DELETE FROM address_labels
            WHERE chain = $1
              AND issued_at >= $2
              AND source IN ('indexer_pool_initialize', 'indexer_token_metadata')
            "#,
        )
        .bind(chain)
        .bind(reorg_block_time)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }
}

// ---------------------------------------------------------------------------
// Row parser helper
// ---------------------------------------------------------------------------

fn parse_address_label(row: &sqlx::postgres::PgRow) -> Result<AddressLabel, GraphError> {
    let chain: String = row.try_get("chain").map_err(|e| GraphError::ParseField {
        field: "chain",
        reason: e.to_string(),
    })?;
    let address: String = row.try_get("address").map_err(|e| GraphError::ParseField {
        field: "address",
        reason: e.to_string(),
    })?;
    let label_type_str: String =
        row.try_get("label_type").map_err(|e| GraphError::ParseField {
            field: "label_type",
            reason: e.to_string(),
        })?;
    let label_type = LabelType::from_db_str(&label_type_str)
        .ok_or_else(|| GraphError::UnknownLabelType(label_type_str.clone()))?;
    let confidence: f64 =
        row.try_get("confidence").map_err(|e| GraphError::ParseField {
            field: "confidence",
            reason: e.to_string(),
        })?;
    let evidence: serde_json::Value =
        row.try_get("evidence").map_err(|e| GraphError::ParseField {
            field: "evidence",
            reason: e.to_string(),
        })?;
    let issued_at: DateTime<Utc> =
        row.try_get("issued_at").map_err(|e| GraphError::ParseField {
            field: "issued_at",
            reason: e.to_string(),
        })?;
    let expires_at: Option<DateTime<Utc>> =
        row.try_get("expires_at").map_err(|e| GraphError::ParseField {
            field: "expires_at",
            reason: e.to_string(),
        })?;
    let source: String = row.try_get("source").map_err(|e| GraphError::ParseField {
        field: "source",
        reason: e.to_string(),
    })?;

    Ok(AddressLabel {
        chain,
        address,
        label_type,
        confidence,
        evidence,
        issued_at,
        expires_at,
        source,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_type_roundtrip_db_str() {
        let all = [
            LabelType::DeployerEoa,
            LabelType::FundingSource,
            LabelType::KnownDex,
            LabelType::KnownBurn,
            LabelType::KnownExchange,
            LabelType::SmartMoney,
            LabelType::Sybil,
        ];
        for lt in &all {
            let s = lt.as_db_str();
            let parsed = LabelType::from_db_str(s);
            assert_eq!(
                parsed.as_ref(),
                Some(lt),
                "roundtrip failed for {s}"
            );
        }
    }

    #[test]
    fn label_type_unknown_string_returns_none() {
        assert!(LabelType::from_db_str("nonexistent_label").is_none());
        assert!(LabelType::from_db_str("").is_none());
        assert!(LabelType::from_db_str("deployer_eoa").is_none()); // wrong case
    }

    #[test]
    fn label_type_serde_roundtrip() {
        let original = LabelType::Sybil;
        let json = serde_json::to_string(&original).expect("serialize");
        assert_eq!(json, r#""sybil""#);
        let parsed: LabelType = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);
    }

    #[test]
    fn label_type_deployer_eoa_serde() {
        // snake_case serde: DeployerEoa → "deployer_eoa"
        let lt = LabelType::DeployerEoa;
        let json = serde_json::to_string(&lt).expect("serialize");
        assert_eq!(json, r#""deployer_eoa""#);
    }

    #[test]
    fn label_store_is_dyn_compatible() {
        // Compile-time check: GraphLabelStore must be usable as a trait object.
        fn _accepts_dyn(_s: &dyn GraphLabelStore) {}
        fn _accepts_box(_s: Box<dyn GraphLabelStore>) {}
        fn _accepts_arc(_s: std::sync::Arc<dyn GraphLabelStore>) {}
    }

    #[test]
    fn address_label_debug_does_not_panic() {
        let label = AddressLabel {
            chain: "solana".into(),
            address: "Abc123".into(),
            label_type: LabelType::DeployerEoa,
            confidence: 1.0,
            evidence: serde_json::json!({"token": "mint123"}),
            issued_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            expires_at: None,
            source: "indexer_pool_initialize".into(),
        };
        let s = format!("{label:?}");
        assert!(s.contains("DeployerEoa"));
    }
}
