//! Typed directed graph edges: `graph_edges` table (V00011).
//!
//! # What this module provides
//!
//! - [`EdgeType`] — enumeration of all edge types (`DeployerOf`, `AuthorityOf`,
//!   `TokenTransfer`, `Funding`).
//! - [`GraphEdge`] — one row from the `graph_edges` table.
//! - [`TypedEdgeStore`] trait — read/write API, dyn-compatible via `async_trait`.
//! - [`PgTypedEdgeStore`] — Postgres implementation.
//!
//! # Relationship to `wallet_edges`
//!
//! `wallet_edges` (V00009) is the primary store for SOL native-transfer (Funding)
//! edges. `graph_edges` is the store for all other edge types plus a `Funding`
//! alias reserved for future cross-type queries. Write paths should prefer
//! `wallet_edges` for Funding edges and `graph_edges` for typed edges.
//!
//! # Reorg handling
//!
//! `delete_edges_above_block` deletes all `graph_edges` rows with
//! `block_height >= reorg_height` for a given chain. This is the primary
//! reorg recovery mechanism for the indexer write path (gotcha #6).
//!
//! # u128 amount encoding
//!
//! `amount_raw` maps to `NUMERIC(39,0)` via the String bridge pattern (ADR 0002):
//! - Write: `bind(value.to_string())` — Postgres casts TEXT → NUMERIC.
//! - Read: `get::<String, _>()` → `parse::<u128>()`.
//!
//! # Design reference
//!
//! `docs/designs/0015-crates-graph-phase3.md` §3.3 + §4.2

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row as _;
use tracing::instrument;

use crate::error::GraphError;

// ---------------------------------------------------------------------------
// EdgeType
// ---------------------------------------------------------------------------

/// Enumeration of graph edge types stored in `graph_edges`.
///
/// `#[non_exhaustive]` allows Phase 4 edge types (e.g. EVM `Approval` edges)
/// without breaking downstream crates.
///
/// # Note on `Funding`
///
/// `Funding` is reserved for backward-compatibility with the `wallet_edges` table
/// naming. New Funding-type writes should use `wallet_edges` (V00009).
/// `graph_edges` with `edge_type = 'Funding'` is not written in Sprint 11.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EdgeType {
    /// Reserved. SOL native-transfer funding edges; `wallet_edges` is primary.
    Funding,
    /// SPL token transfer (from `transfers` table). Used by T2-2 (Tarjan SCC).
    TokenTransfer,
    /// Deployer EOA → token mint address. Written on `PoolEvent::Initialize`.
    DeployerOf,
    /// mint_authority or freeze_authority → token mint address.
    /// Written on token metadata upsert when authority is non-NULL.
    AuthorityOf,
}

impl EdgeType {
    /// Returns the DB column string for this edge type.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            EdgeType::Funding => "Funding",
            EdgeType::TokenTransfer => "TokenTransfer",
            EdgeType::DeployerOf => "DeployerOf",
            EdgeType::AuthorityOf => "AuthorityOf",
        }
    }

    /// Parse from the DB column string. Returns `None` for unknown values.
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "Funding" => Some(EdgeType::Funding),
            "TokenTransfer" => Some(EdgeType::TokenTransfer),
            "DeployerOf" => Some(EdgeType::DeployerOf),
            "AuthorityOf" => Some(EdgeType::AuthorityOf),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// GraphEdge
// ---------------------------------------------------------------------------

/// One row from the `graph_edges` table.
///
/// Represents a typed directed edge in the address graph. Multiple edges can
/// exist between the same `(chain, from_address, to_address)` pair if they
/// have different `edge_type`, `token`, or `block_height` values.
#[derive(Debug, Clone)]
pub struct GraphEdge {
    /// Chain identifier (e.g. `"solana"`, `"ethereum"`).
    pub chain: String,
    /// Source wallet/address of the edge.
    pub from_address: String,
    /// Target wallet/address (token mint for DeployerOf/AuthorityOf).
    pub to_address: String,
    /// Edge type discriminator.
    pub edge_type: EdgeType,
    /// Token mint address. `None` for `Funding` edges; required for all
    /// token-specific edge types (`DeployerOf`, `AuthorityOf`, `TokenTransfer`).
    pub token: Option<String>,
    /// Raw amount in the token's native unit (NUMERIC(39,0) String bridge).
    /// `None` for `DeployerOf` and `AuthorityOf` edges (no amount semantics).
    pub amount_raw: Option<u128>,
    /// Block timestamp. MUST be derived from `block_time`, not `Utc::now()` (gotcha #28).
    pub block_time: DateTime<Utc>,
    /// Block height. Used in reorg DELETE and T2-2 time-windowed queries.
    pub block_height: u64,
    /// Transaction hash. `None` for `AuthorityOf` edges (inferred from metadata).
    pub tx_hash: Option<String>,
}

// ---------------------------------------------------------------------------
// TypedEdgeStore trait
// ---------------------------------------------------------------------------

/// Read/write API for the `graph_edges` table.
///
/// Uses `#[async_trait]` for dyn-compatibility (same pattern as `ClusterStore`).
/// All implementations must be `Send + Sync` (gotcha #27).
#[async_trait]
pub trait TypedEdgeStore: Send + Sync {
    /// Insert an edge. `ON CONFLICT DO NOTHING` — idempotent.
    ///
    /// Conflict on the PRIMARY KEY `(chain, from_address, to_address, edge_type,
    /// token, block_height)` is a silent no-op. This handles replayed events from
    /// boundary slots without error.
    async fn insert_edge(&self, edge: &GraphEdge) -> Result<(), GraphError>;

    /// Batch insert — single transaction, `ON CONFLICT DO NOTHING` per row.
    ///
    /// Empty slice is a no-op (no query issued).
    async fn insert_edges(&self, edges: &[GraphEdge]) -> Result<(), GraphError>;

    /// Outgoing neighbors of `from_address`, filtered by `edge_type`.
    ///
    /// `limit` is mandatory; callers MUST NOT pass unbounded queries.
    /// Returns edges ordered by `block_height DESC, to_address` for determinism.
    async fn get_neighbors(
        &self,
        chain: &str,
        from_address: &str,
        edge_type: EdgeType,
        limit: u32,
    ) -> Result<Vec<GraphEdge>, GraphError>;

    /// Incoming neighbors of `to_address` (reverse lookup), filtered by `edge_type`.
    ///
    /// Used for: "who is the deployer of this token?" and "who holds authority
    /// over this token?". `limit` is mandatory.
    /// Returns edges ordered by `block_height DESC, from_address` for determinism.
    async fn get_predecessors(
        &self,
        chain: &str,
        to_address: &str,
        edge_type: EdgeType,
        limit: u32,
    ) -> Result<Vec<GraphEdge>, GraphError>;

    /// All edges for a token, filtered by `edge_type`.
    ///
    /// Used by D08 (`DeployerOf` + `AuthorityOf` for a token) and T2-2
    /// (`TokenTransfer` edges for cycle detection). Returns ordered by
    /// `block_height ASC, from_address` for reproducibility.
    async fn token_edges(
        &self,
        chain: &str,
        token: &str,
        edge_type: EdgeType,
    ) -> Result<Vec<GraphEdge>, GraphError>;

    /// Delete all edges above a given block height (inclusive).
    ///
    /// Used for reorg recovery (gotcha #6). Deletes rows where
    /// `chain = $chain AND block_height >= $reorg_height`.
    /// Returns the count of deleted rows.
    async fn delete_edges_above_block(
        &self,
        chain: &str,
        reorg_height: u64,
    ) -> Result<u64, GraphError>;
}

// ---------------------------------------------------------------------------
// PgTypedEdgeStore
// ---------------------------------------------------------------------------

/// Postgres-backed implementation of [`TypedEdgeStore`].
pub struct PgTypedEdgeStore {
    pub pool: sqlx::PgPool,
}

impl PgTypedEdgeStore {
    /// Construct a new `PgTypedEdgeStore` wrapping the given pool.
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TypedEdgeStore for PgTypedEdgeStore {
    #[instrument(skip(self, edge), fields(chain = %edge.chain, edge_type = %edge.edge_type.as_db_str()))]
    async fn insert_edge(&self, edge: &GraphEdge) -> Result<(), GraphError> {
        sqlx::query(
            r#"
            INSERT INTO graph_edges
                (chain, from_address, to_address, edge_type, token,
                 amount_raw, block_time, block_height, tx_hash, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now())
            ON CONFLICT (chain, from_address, to_address, edge_type, token, block_height)
                DO NOTHING
            "#,
        )
        .bind(&edge.chain)
        .bind(&edge.from_address)
        .bind(&edge.to_address)
        .bind(edge.edge_type.as_db_str())
        .bind(&edge.token)
        .bind(edge.amount_raw.map(|a| a.to_string()))
        .bind(edge.block_time)
        .bind(edge.block_height as i64)
        .bind(&edge.tx_hash)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    #[instrument(skip(self, edges), fields(count = edges.len()))]
    async fn insert_edges(&self, edges: &[GraphEdge]) -> Result<(), GraphError> {
        if edges.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for edge in edges {
            sqlx::query(
                r#"
                INSERT INTO graph_edges
                    (chain, from_address, to_address, edge_type, token,
                     amount_raw, block_time, block_height, tx_hash, updated_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now())
                ON CONFLICT (chain, from_address, to_address, edge_type, token, block_height)
                    DO NOTHING
                "#,
            )
            .bind(&edge.chain)
            .bind(&edge.from_address)
            .bind(&edge.to_address)
            .bind(edge.edge_type.as_db_str())
            .bind(&edge.token)
            .bind(edge.amount_raw.map(|a| a.to_string()))
            .bind(edge.block_time)
            .bind(edge.block_height as i64)
            .bind(&edge.tx_hash)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    #[instrument(skip(self), fields(chain, from_address, edge_type = %edge_type.as_db_str(), limit))]
    async fn get_neighbors(
        &self,
        chain: &str,
        from_address: &str,
        edge_type: EdgeType,
        limit: u32,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let rows = sqlx::query(
            r#"
            SELECT chain, from_address, to_address, edge_type, token,
                   amount_raw, block_time, block_height, tx_hash
            FROM graph_edges
            WHERE chain = $1
              AND from_address = $2
              AND edge_type = $3
            ORDER BY block_height DESC, to_address
            LIMIT $4
            "#,
        )
        .bind(chain)
        .bind(from_address)
        .bind(edge_type.as_db_str())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(parse_graph_edge).collect()
    }

    #[instrument(skip(self), fields(chain, to_address, edge_type = %edge_type.as_db_str(), limit))]
    async fn get_predecessors(
        &self,
        chain: &str,
        to_address: &str,
        edge_type: EdgeType,
        limit: u32,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let rows = sqlx::query(
            r#"
            SELECT chain, from_address, to_address, edge_type, token,
                   amount_raw, block_time, block_height, tx_hash
            FROM graph_edges
            WHERE chain = $1
              AND to_address = $2
              AND edge_type = $3
            ORDER BY block_height DESC, from_address
            LIMIT $4
            "#,
        )
        .bind(chain)
        .bind(to_address)
        .bind(edge_type.as_db_str())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(parse_graph_edge).collect()
    }

    #[instrument(skip(self), fields(chain, token, edge_type = %edge_type.as_db_str()))]
    async fn token_edges(
        &self,
        chain: &str,
        token: &str,
        edge_type: EdgeType,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let rows = sqlx::query(
            r#"
            SELECT chain, from_address, to_address, edge_type, token,
                   amount_raw, block_time, block_height, tx_hash
            FROM graph_edges
            WHERE chain = $1
              AND token = $2
              AND edge_type = $3
            ORDER BY block_height ASC, from_address
            "#,
        )
        .bind(chain)
        .bind(token)
        .bind(edge_type.as_db_str())
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(parse_graph_edge).collect()
    }

    #[instrument(skip(self), fields(chain, reorg_height))]
    async fn delete_edges_above_block(
        &self,
        chain: &str,
        reorg_height: u64,
    ) -> Result<u64, GraphError> {
        let result = sqlx::query(
            r#"
            DELETE FROM graph_edges
            WHERE chain = $1
              AND block_height >= $2
            "#,
        )
        .bind(chain)
        .bind(reorg_height as i64)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }
}

// ---------------------------------------------------------------------------
// Row parser helper
// ---------------------------------------------------------------------------

fn parse_graph_edge(row: &sqlx::postgres::PgRow) -> Result<GraphEdge, GraphError> {
    let chain: String = row.try_get("chain").map_err(|e| GraphError::ParseField {
        field: "chain",
        reason: e.to_string(),
    })?;
    let from_address: String =
        row.try_get("from_address").map_err(|e| GraphError::ParseField {
            field: "from_address",
            reason: e.to_string(),
        })?;
    let to_address: String =
        row.try_get("to_address").map_err(|e| GraphError::ParseField {
            field: "to_address",
            reason: e.to_string(),
        })?;
    let edge_type_str: String =
        row.try_get("edge_type").map_err(|e| GraphError::ParseField {
            field: "edge_type",
            reason: e.to_string(),
        })?;
    let edge_type = EdgeType::from_db_str(&edge_type_str).ok_or_else(|| {
        GraphError::ParseField {
            field: "edge_type",
            reason: format!("unknown edge_type: {edge_type_str}"),
        }
    })?;
    let token: Option<String> = row.try_get("token").map_err(|e| GraphError::ParseField {
        field: "token",
        reason: e.to_string(),
    })?;
    // amount_raw is NUMERIC(39,0) — read as Option<String> then parse as u128.
    let amount_raw_str: Option<String> =
        row.try_get("amount_raw").map_err(|e| GraphError::ParseField {
            field: "amount_raw",
            reason: e.to_string(),
        })?;
    let amount_raw: Option<u128> = amount_raw_str
        .map(|s| {
            s.parse::<u128>().map_err(|e| GraphError::ParseField {
                field: "amount_raw",
                reason: format!("parse u128: {e}"),
            })
        })
        .transpose()?;
    let block_time: DateTime<Utc> =
        row.try_get("block_time").map_err(|e| GraphError::ParseField {
            field: "block_time",
            reason: e.to_string(),
        })?;
    let block_height_i64: i64 =
        row.try_get("block_height").map_err(|e| GraphError::ParseField {
            field: "block_height",
            reason: e.to_string(),
        })?;
    let tx_hash: Option<String> =
        row.try_get("tx_hash").map_err(|e| GraphError::ParseField {
            field: "tx_hash",
            reason: e.to_string(),
        })?;

    Ok(GraphEdge {
        chain,
        from_address,
        to_address,
        edge_type,
        token,
        amount_raw,
        block_time,
        block_height: block_height_i64 as u64,
        tx_hash,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_type_roundtrip_db_str() {
        let all = [
            EdgeType::Funding,
            EdgeType::TokenTransfer,
            EdgeType::DeployerOf,
            EdgeType::AuthorityOf,
        ];
        for et in &all {
            let s = et.as_db_str();
            let parsed = EdgeType::from_db_str(s);
            assert_eq!(
                parsed.as_ref(),
                Some(et),
                "roundtrip failed for {s}"
            );
        }
    }

    #[test]
    fn edge_type_unknown_string_returns_none() {
        assert!(EdgeType::from_db_str("unknown_type").is_none());
        assert!(EdgeType::from_db_str("").is_none());
        assert!(EdgeType::from_db_str("deployer_of").is_none()); // wrong case
    }

    #[test]
    fn edge_type_serde_roundtrip() {
        let et = EdgeType::DeployerOf;
        let json = serde_json::to_string(&et).expect("serialize");
        assert_eq!(json, r#""deployer_of""#);
        let parsed: EdgeType = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, et);
    }

    #[test]
    fn edge_type_token_transfer_serde() {
        let et = EdgeType::TokenTransfer;
        let json = serde_json::to_string(&et).expect("serialize");
        assert_eq!(json, r#""token_transfer""#);
    }

    #[test]
    fn typed_edge_store_is_dyn_compatible() {
        fn _accepts_dyn(_s: &dyn TypedEdgeStore) {}
        fn _accepts_box(_s: Box<dyn TypedEdgeStore>) {}
        fn _accepts_arc(_s: std::sync::Arc<dyn TypedEdgeStore>) {}
    }

    #[test]
    fn graph_edge_debug_does_not_panic() {
        let edge = GraphEdge {
            chain: "solana".into(),
            from_address: "creator_wallet".into(),
            to_address: "token_mint".into(),
            edge_type: EdgeType::DeployerOf,
            token: Some("token_mint".into()),
            amount_raw: None,
            block_time: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            block_height: 250_000_000,
            tx_hash: Some("abc123".into()),
        };
        let s = format!("{edge:?}");
        assert!(s.contains("DeployerOf"));
    }

    #[test]
    fn graph_edge_with_amount_raw() {
        let edge = GraphEdge {
            chain: "solana".into(),
            from_address: "sender".into(),
            to_address: "receiver".into(),
            edge_type: EdgeType::TokenTransfer,
            token: Some("mint".into()),
            amount_raw: Some(u128::MAX),
            block_time: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            block_height: 1,
            tx_hash: Some("tx".into()),
        };
        assert_eq!(edge.amount_raw, Some(u128::MAX));
    }

    #[test]
    fn amount_raw_string_bridge_roundtrip() {
        // Verify the String bridge logic used in the DB encode/decode path.
        let amount: u128 = 340_282_366_920_938_463_463_374_607_431_768_211_455; // u128::MAX
        let s = amount.to_string();
        let parsed: u128 = s.parse().expect("must parse back");
        assert_eq!(parsed, amount);
    }
}
