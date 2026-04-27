//! `MockClusterStore` — in-memory implementation of `ClusterStore` for tests.
//!
//! # Usage
//!
//! ```rust,no_run
//! use mg_onchain_graph::mock::MockClusterStore;
//! use mg_onchain_graph::api::{ClusterRef, ClusterKind, ClusterStore};
//! use uuid::Uuid;
//! use chrono::Utc;
//!
//! let mut store = MockClusterStore::default();
//! let cluster_id = Uuid::new_v4();
//! store.add_membership(
//!     "solana",
//!     "wallet_A",
//!     ClusterRef {
//!         cluster_id,
//!         chain: "solana".into(),
//!         cluster_kind: ClusterKind::CommonFunder,
//!         root_funder: Some("funder_F".into()),
//!         member_count: 3,
//!         confidence: 0.70,
//!         computed_at: Utc::now(),
//!     },
//! );
//! store.add_membership("solana", "wallet_B", ClusterRef {
//!     cluster_id,
//!     chain: "solana".into(),
//!     cluster_kind: ClusterKind::CommonFunder,
//!     root_funder: Some("funder_F".into()),
//!     member_count: 3,
//!     confidence: 0.70,
//!     computed_at: Utc::now(),
//! });
//! store.add_members(cluster_id, vec!["wallet_A".into(), "wallet_B".into(), "wallet_C".into()]);
//! ```
//!
//! Note: `MockClusterStore` is gated behind `#[cfg(any(test, feature = "test-utils"))]`
//! so it is never shipped in production builds without explicit opt-in.

#![allow(dead_code)] // fields used in tests only

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::api::{ClusterRef, ClusterStore};
use crate::error::GraphError;
use crate::labels::{AddressLabel, GraphLabelStore, LabelType};
use crate::typed_edges::{EdgeType, GraphEdge, TypedEdgeStore};

/// An in-memory `ClusterStore` for use in unit tests.
///
/// Backed by `BTreeMap` for deterministic iteration. Thread-safe (`Send + Sync`)
/// because all fields are protected by `Arc<Mutex<_>>` in the real use case;
/// here the mock is single-threaded test-time only, so plain fields suffice.
///
/// # Design
///
/// `memberships`: `(chain, wallet) → Vec<ClusterRef>` — one wallet may appear in
/// multiple clusters.
///
/// `member_lists`: `cluster_id → Vec<String>` — for `cluster_members()` queries.
#[derive(Debug, Default)]
pub struct MockClusterStore {
    /// Wallet → cluster references. BTreeMap for deterministic iteration.
    memberships: BTreeMap<(String, String), Vec<ClusterRef>>,
    /// Cluster → member list.
    member_lists: BTreeMap<Uuid, Vec<String>>,
}

impl MockClusterStore {
    /// Add a cluster membership for `wallet` on `chain`.
    ///
    /// Multiple calls with the same `(chain, wallet)` append to the membership
    /// list. `wallet_cluster()` returns the highest-confidence entry.
    pub fn add_membership(&mut self, chain: &str, wallet: &str, cluster: ClusterRef) {
        self.memberships
            .entry((chain.to_owned(), wallet.to_owned()))
            .or_default()
            .push(cluster);
    }

    /// Register the full member list for a cluster (for `cluster_members()` queries).
    pub fn add_members(&mut self, cluster_id: Uuid, members: Vec<String>) {
        self.member_lists.insert(cluster_id, members);
    }
}

#[async_trait]
impl ClusterStore for MockClusterStore {
    async fn wallet_cluster(
        &self,
        chain: &str,
        wallet: &str,
    ) -> Result<Option<ClusterRef>, GraphError> {
        let key = (chain.to_owned(), wallet.to_owned());
        let best = self
            .memberships
            .get(&key)
            .and_then(|v| v.iter().max_by(|a, b| {
                a.confidence.partial_cmp(&b.confidence).unwrap_or(std::cmp::Ordering::Equal)
            }))
            .cloned();
        Ok(best)
    }

    async fn all_clusters_for_wallet(
        &self,
        chain: &str,
        wallet: &str,
    ) -> Result<Vec<ClusterRef>, GraphError> {
        let key = (chain.to_owned(), wallet.to_owned());
        let clusters = self
            .memberships
            .get(&key)
            .cloned()
            .unwrap_or_default();
        Ok(clusters)
    }

    async fn cluster_members(
        &self,
        cluster_id: Uuid,
    ) -> Result<Vec<String>, GraphError> {
        let members = self
            .member_lists
            .get(&cluster_id)
            .cloned()
            .unwrap_or_default();
        Ok(members)
    }

    async fn funder_cluster(
        &self,
        chain: &str,
        root_funder: &str,
    ) -> Result<Option<ClusterRef>, GraphError> {
        // Search all memberships for a ClusterRef with the matching root_funder.
        let result = self
            .memberships
            .values()
            .flatten()
            .filter(|c| {
                c.chain == chain
                    && c.root_funder.as_deref() == Some(root_funder)
            })
            .max_by(|a, b| {
                a.confidence.partial_cmp(&b.confidence).unwrap_or(std::cmp::Ordering::Equal)
            })
            .cloned();
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// MockGraphLabelStore
// ---------------------------------------------------------------------------

/// An in-memory [`GraphLabelStore`] for use in unit tests.
///
/// Backed by `BTreeMap` for deterministic iteration and results.
/// Thread-safe via `Mutex` to allow use in `Arc<MockGraphLabelStore>` across
/// async tests.
///
/// # Upsert semantics
///
/// `upsert_label` mirrors the Postgres `ON CONFLICT DO UPDATE` logic:
/// the incoming label overwrites only if its confidence is >= the existing
/// label's confidence, or if the existing label has expired.
#[derive(Debug, Default)]
pub struct MockGraphLabelStore {
    /// `(chain, address, label_type_str) → AddressLabel`
    labels: Mutex<BTreeMap<(String, String, String), AddressLabel>>,
}

impl MockGraphLabelStore {
    /// Insert a label directly, bypassing confidence comparison.
    /// Use in test setup only — not for production logic.
    pub fn seed_label(&self, label: AddressLabel) {
        let key = (
            label.chain.clone(),
            label.address.clone(),
            label.label_type.as_db_str().to_owned(),
        );
        self.labels.lock().unwrap().insert(key, label);
    }
}

#[async_trait]
impl GraphLabelStore for MockGraphLabelStore {
    async fn upsert_label(&self, label: &AddressLabel) -> Result<(), GraphError> {
        let key = (
            label.chain.clone(),
            label.address.clone(),
            label.label_type.as_db_str().to_owned(),
        );
        let mut guard = self.labels.lock().unwrap();
        let should_update = match guard.get(&key) {
            None => true,
            Some(existing) => {
                label.confidence >= existing.confidence
                    || existing
                        .expires_at
                        .map(|e| e <= Utc::now())
                        .unwrap_or(false)
            }
        };
        if should_update {
            guard.insert(key, label.clone());
        }
        Ok(())
    }

    async fn upsert_labels(&self, labels: &[AddressLabel]) -> Result<(), GraphError> {
        for label in labels {
            self.upsert_label(label).await?;
        }
        Ok(())
    }

    async fn get_labels(
        &self,
        chain: &str,
        address: &str,
    ) -> Result<Vec<AddressLabel>, GraphError> {
        let guard = self.labels.lock().unwrap();
        let now = Utc::now();
        let result: Vec<AddressLabel> = guard
            .values()
            .filter(|l| {
                l.chain == chain
                    && l.address == address
                    && l.expires_at.map(|e| e > now).unwrap_or(true)
            })
            .cloned()
            .collect();
        Ok(result)
    }

    async fn addresses_with_label(
        &self,
        chain: &str,
        label_type: LabelType,
        min_confidence: f64,
    ) -> Result<Vec<AddressLabel>, GraphError> {
        let guard = self.labels.lock().unwrap();
        let now = Utc::now();
        let label_type_str = label_type.as_db_str();
        let mut result: Vec<AddressLabel> = guard
            .values()
            .filter(|l| {
                l.chain == chain
                    && l.label_type.as_db_str() == label_type_str
                    && l.confidence >= min_confidence
                    && l.expires_at.map(|e| e > now).unwrap_or(true)
            })
            .cloned()
            .collect();
        // Match PgGraphLabelStore: ORDER BY confidence DESC, address.
        result.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.address.cmp(&b.address))
        });
        Ok(result)
    }

    async fn delete_indexer_labels_after(
        &self,
        chain: &str,
        reorg_block_time: DateTime<Utc>,
    ) -> Result<u64, GraphError> {
        let mut guard = self.labels.lock().unwrap();
        let before_len = guard.len();
        guard.retain(|_, l| {
            !(l.chain == chain
                && l.issued_at >= reorg_block_time
                && (l.source == "indexer_pool_initialize"
                    || l.source == "indexer_token_metadata"))
        });
        let after_len = guard.len();
        Ok((before_len - after_len) as u64)
    }
}

// ---------------------------------------------------------------------------
// MockTypedEdgeStore
// ---------------------------------------------------------------------------

/// Key type for `MockTypedEdgeStore`'s internal map.
///
/// Tuple: `(chain, from_address, to_address, edge_type_str, token_or_empty, block_height)`.
type EdgeKey = (String, String, String, String, String, u64);

/// An in-memory [`TypedEdgeStore`] for use in unit tests.
///
/// Stores edges in a `BTreeMap` keyed on `(chain, from_address, to_address,
/// edge_type_str, token, block_height)` for deterministic retrieval.
/// Thread-safe via `Mutex`.
#[derive(Debug, Default)]
pub struct MockTypedEdgeStore {
    /// Edge key → GraphEdge. BTreeMap for deterministic iteration.
    edges: Mutex<BTreeMap<EdgeKey, GraphEdge>>,
}

impl MockTypedEdgeStore {
    /// Insert an edge directly. Use in test setup.
    pub fn seed_edge(&self, edge: GraphEdge) {
        let key = edge_key(&edge);
        self.edges.lock().unwrap().insert(key, edge);
    }

    /// Return all stored edges (for test assertions).
    pub fn all_edges(&self) -> Vec<GraphEdge> {
        self.edges.lock().unwrap().values().cloned().collect()
    }
}

fn edge_key(e: &GraphEdge) -> EdgeKey {
    (
        e.chain.clone(),
        e.from_address.clone(),
        e.to_address.clone(),
        e.edge_type.as_db_str().to_owned(),
        e.token.clone().unwrap_or_default(),
        e.block_height,
    )
}

#[async_trait]
impl TypedEdgeStore for MockTypedEdgeStore {
    async fn insert_edge(&self, edge: &GraphEdge) -> Result<(), GraphError> {
        let key = edge_key(edge);
        // ON CONFLICT DO NOTHING: only insert if key doesn't exist.
        self.edges
            .lock()
            .unwrap()
            .entry(key)
            .or_insert_with(|| edge.clone());
        Ok(())
    }

    async fn insert_edges(&self, edges: &[GraphEdge]) -> Result<(), GraphError> {
        for edge in edges {
            self.insert_edge(edge).await?;
        }
        Ok(())
    }

    async fn get_neighbors(
        &self,
        chain: &str,
        from_address: &str,
        edge_type: EdgeType,
        limit: u32,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let guard = self.edges.lock().unwrap();
        let edge_type_str = edge_type.as_db_str();
        let mut result: Vec<GraphEdge> = guard
            .values()
            .filter(|e| {
                e.chain == chain
                    && e.from_address == from_address
                    && e.edge_type.as_db_str() == edge_type_str
            })
            .cloned()
            .collect();
        // Match PgTypedEdgeStore: ORDER BY block_height DESC, to_address.
        result.sort_by(|a, b| {
            b.block_height
                .cmp(&a.block_height)
                .then(a.to_address.cmp(&b.to_address))
        });
        result.truncate(limit as usize);
        Ok(result)
    }

    async fn get_predecessors(
        &self,
        chain: &str,
        to_address: &str,
        edge_type: EdgeType,
        limit: u32,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let guard = self.edges.lock().unwrap();
        let edge_type_str = edge_type.as_db_str();
        let mut result: Vec<GraphEdge> = guard
            .values()
            .filter(|e| {
                e.chain == chain
                    && e.to_address == to_address
                    && e.edge_type.as_db_str() == edge_type_str
            })
            .cloned()
            .collect();
        // Match PgTypedEdgeStore: ORDER BY block_height DESC, from_address.
        result.sort_by(|a, b| {
            b.block_height
                .cmp(&a.block_height)
                .then(a.from_address.cmp(&b.from_address))
        });
        result.truncate(limit as usize);
        Ok(result)
    }

    async fn token_edges(
        &self,
        chain: &str,
        token: &str,
        edge_type: EdgeType,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let guard = self.edges.lock().unwrap();
        let edge_type_str = edge_type.as_db_str();
        let mut result: Vec<GraphEdge> = guard
            .values()
            .filter(|e| {
                e.chain == chain
                    && e.token.as_deref() == Some(token)
                    && e.edge_type.as_db_str() == edge_type_str
            })
            .cloned()
            .collect();
        // Match PgTypedEdgeStore: ORDER BY block_height ASC, from_address.
        result.sort_by(|a, b| {
            a.block_height
                .cmp(&b.block_height)
                .then(a.from_address.cmp(&b.from_address))
        });
        Ok(result)
    }

    async fn delete_edges_above_block(
        &self,
        chain: &str,
        reorg_height: u64,
    ) -> Result<u64, GraphError> {
        let mut guard = self.edges.lock().unwrap();
        let before_len = guard.len();
        guard.retain(|_, e| !(e.chain == chain && e.block_height >= reorg_height));
        let after_len = guard.len();
        Ok((before_len - after_len) as u64)
    }
}

// ---------------------------------------------------------------------------
// Tests for MockClusterStore
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ClusterKind;
    use chrono::Utc;

    fn make_ref(id: Uuid, chain: &str, funder: &str, confidence: f64) -> ClusterRef {
        ClusterRef {
            cluster_id: id,
            chain: chain.to_owned(),
            cluster_kind: ClusterKind::CommonFunder,
            root_funder: Some(funder.to_owned()),
            member_count: 3,
            confidence,
            computed_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn wallet_cluster_returns_none_for_unknown_wallet() {
        let store = MockClusterStore::default();
        let result = store.wallet_cluster("solana", "unknown_wallet").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn wallet_cluster_returns_highest_confidence() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let mut store = MockClusterStore::default();
        store.add_membership("solana", "wallet_A", make_ref(id1, "solana", "funder_1", 0.60));
        store.add_membership("solana", "wallet_A", make_ref(id2, "solana", "funder_2", 0.75));

        let result = store.wallet_cluster("solana", "wallet_A").await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().cluster_id, id2, "must return highest confidence");
    }

    #[tokio::test]
    async fn all_clusters_for_wallet_returns_all() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let mut store = MockClusterStore::default();
        store.add_membership("solana", "wallet_A", make_ref(id1, "solana", "funder_1", 0.60));
        store.add_membership("solana", "wallet_A", make_ref(id2, "solana", "funder_2", 0.75));

        let clusters = store.all_clusters_for_wallet("solana", "wallet_A").await.unwrap();
        assert_eq!(clusters.len(), 2);
    }

    #[tokio::test]
    async fn cluster_members_returns_registered_members() {
        let id = Uuid::new_v4();
        let mut store = MockClusterStore::default();
        store.add_members(
            id,
            vec!["wallet_A".into(), "wallet_B".into(), "wallet_C".into()],
        );

        let members = store.cluster_members(id).await.unwrap();
        assert_eq!(members.len(), 3);
        assert!(members.contains(&"wallet_A".to_string()));
    }

    #[tokio::test]
    async fn cluster_members_empty_for_unknown_cluster() {
        let store = MockClusterStore::default();
        let members = store.cluster_members(Uuid::new_v4()).await.unwrap();
        assert!(members.is_empty());
    }

    #[tokio::test]
    async fn funder_cluster_returns_matching_funder() {
        let id = Uuid::new_v4();
        let mut store = MockClusterStore::default();
        store.add_membership("solana", "wallet_A", make_ref(id, "solana", "funder_F", 0.70));

        let result = store.funder_cluster("solana", "funder_F").await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().cluster_id, id);
    }

    #[tokio::test]
    async fn are_co_clustered_true_when_same_cluster() {
        let id = Uuid::new_v4();
        let mut store = MockClusterStore::default();
        store.add_membership("solana", "wallet_A", make_ref(id, "solana", "funder_F", 0.70));
        store.add_membership("solana", "wallet_B", make_ref(id, "solana", "funder_F", 0.70));

        let co = store
            .are_co_clustered("solana", "wallet_A", "wallet_B")
            .await
            .unwrap();
        assert!(co);
    }

    #[tokio::test]
    async fn are_co_clustered_false_when_different_clusters() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let mut store = MockClusterStore::default();
        store.add_membership("solana", "wallet_A", make_ref(id1, "solana", "funder_1", 0.70));
        store.add_membership("solana", "wallet_B", make_ref(id2, "solana", "funder_2", 0.70));

        let co = store
            .are_co_clustered("solana", "wallet_A", "wallet_B")
            .await
            .unwrap();
        assert!(!co);
    }

    #[tokio::test]
    async fn are_co_clustered_false_when_one_unclustered() {
        let id = Uuid::new_v4();
        let mut store = MockClusterStore::default();
        store.add_membership("solana", "wallet_A", make_ref(id, "solana", "funder_F", 0.70));
        // wallet_B not in any cluster.

        let co = store
            .are_co_clustered("solana", "wallet_A", "wallet_B")
            .await
            .unwrap();
        assert!(!co);
    }

    // -----------------------------------------------------------------------
    // Tests for MockGraphLabelStore
    // -----------------------------------------------------------------------

    fn make_label(chain: &str, address: &str, lt: LabelType, confidence: f64) -> AddressLabel {
        AddressLabel {
            chain: chain.to_owned(),
            address: address.to_owned(),
            label_type: lt,
            confidence,
            evidence: serde_json::json!({}),
            issued_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            expires_at: None,
            source: "test".into(),
        }
    }

    #[tokio::test]
    async fn mock_label_store_upsert_and_get() {
        let store = MockGraphLabelStore::default();
        let label = make_label("solana", "wallet_A", LabelType::DeployerEoa, 1.0);
        store.upsert_label(&label).await.unwrap();

        let labels = store.get_labels("solana", "wallet_A").await.unwrap();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].label_type, LabelType::DeployerEoa);
        assert!((labels[0].confidence - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn mock_label_store_lower_confidence_does_not_overwrite() {
        let store = MockGraphLabelStore::default();
        store
            .upsert_label(&make_label("solana", "wallet_A", LabelType::Sybil, 0.80))
            .await
            .unwrap();
        store
            .upsert_label(&make_label("solana", "wallet_A", LabelType::Sybil, 0.60))
            .await
            .unwrap();

        let labels = store.get_labels("solana", "wallet_A").await.unwrap();
        assert_eq!(labels.len(), 1);
        // Original 0.80 should remain — 0.60 must NOT overwrite.
        assert!((labels[0].confidence - 0.80).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn mock_label_store_higher_confidence_overwrites() {
        let store = MockGraphLabelStore::default();
        store
            .upsert_label(&make_label("solana", "wallet_A", LabelType::Sybil, 0.60))
            .await
            .unwrap();
        store
            .upsert_label(&make_label("solana", "wallet_A", LabelType::Sybil, 0.90))
            .await
            .unwrap();

        let labels = store.get_labels("solana", "wallet_A").await.unwrap();
        assert_eq!(labels.len(), 1);
        assert!((labels[0].confidence - 0.90).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn mock_label_store_get_returns_empty_for_unknown() {
        let store = MockGraphLabelStore::default();
        let labels = store.get_labels("solana", "unknown_wallet").await.unwrap();
        assert!(labels.is_empty());
    }

    #[tokio::test]
    async fn mock_label_store_addresses_with_label_filters_by_type_and_confidence() {
        let store = MockGraphLabelStore::default();
        store
            .upsert_label(&make_label("solana", "wallet_A", LabelType::Sybil, 0.80))
            .await
            .unwrap();
        store
            .upsert_label(&make_label("solana", "wallet_B", LabelType::Sybil, 0.40))
            .await
            .unwrap();
        store
            .upsert_label(&make_label("solana", "wallet_C", LabelType::DeployerEoa, 1.0))
            .await
            .unwrap();

        let sybils = store
            .addresses_with_label("solana", LabelType::Sybil, 0.50)
            .await
            .unwrap();
        assert_eq!(sybils.len(), 1);
        assert_eq!(sybils[0].address, "wallet_A");
    }

    #[tokio::test]
    async fn mock_label_store_delete_indexer_labels_after_reorg() {
        let store = MockGraphLabelStore::default();
        let t0 = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let t1 = DateTime::from_timestamp(1_700_001_000, 0).unwrap();

        // Label issued BEFORE reorg time, indexer source — should be retained.
        let mut label_old = make_label("solana", "wallet_A", LabelType::DeployerEoa, 1.0);
        label_old.issued_at = t0;
        label_old.source = "indexer_pool_initialize".into();
        store.upsert_label(&label_old).await.unwrap();

        // Label issued AT reorg time, indexer source — should be deleted.
        let mut label_new = make_label("solana", "wallet_B", LabelType::DeployerEoa, 1.0);
        label_new.issued_at = t1;
        label_new.source = "indexer_pool_initialize".into();
        store.upsert_label(&label_new).await.unwrap();

        // Label issued AFTER reorg, clustering source — should be retained.
        let mut label_cluster = make_label("solana", "wallet_C", LabelType::FundingSource, 0.70);
        label_cluster.issued_at = t1;
        label_cluster.source = "common_funder_clustering".into();
        store.upsert_label(&label_cluster).await.unwrap();

        let deleted = store
            .delete_indexer_labels_after("solana", t1)
            .await
            .unwrap();
        assert_eq!(deleted, 1, "only wallet_B should be deleted");

        let a = store.get_labels("solana", "wallet_A").await.unwrap();
        assert_eq!(a.len(), 1, "wallet_A label must remain");
        let b = store.get_labels("solana", "wallet_B").await.unwrap();
        assert!(b.is_empty(), "wallet_B label must be deleted");
        let c = store.get_labels("solana", "wallet_C").await.unwrap();
        assert_eq!(c.len(), 1, "clustering label must remain");
    }

    // -----------------------------------------------------------------------
    // Tests for MockTypedEdgeStore
    // -----------------------------------------------------------------------

    fn make_edge(
        chain: &str,
        from: &str,
        to: &str,
        et: EdgeType,
        token: Option<&str>,
        height: u64,
    ) -> GraphEdge {
        GraphEdge {
            chain: chain.to_owned(),
            from_address: from.to_owned(),
            to_address: to.to_owned(),
            edge_type: et,
            token: token.map(str::to_owned),
            amount_raw: None,
            block_time: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            block_height: height,
            tx_hash: Some("tx_hash".into()),
        }
    }

    #[tokio::test]
    async fn mock_edge_store_insert_and_get_neighbors() {
        let store = MockTypedEdgeStore::default();
        let edge = make_edge("solana", "creator", "mint_a", EdgeType::DeployerOf, Some("mint_a"), 100);
        store.insert_edge(&edge).await.unwrap();

        let neighbors = store
            .get_neighbors("solana", "creator", EdgeType::DeployerOf, 10)
            .await
            .unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].to_address, "mint_a");
    }

    #[tokio::test]
    async fn mock_edge_store_on_conflict_do_nothing() {
        let store = MockTypedEdgeStore::default();
        let e1 = make_edge("solana", "creator", "mint_a", EdgeType::DeployerOf, Some("mint_a"), 100);
        store.insert_edge(&e1).await.unwrap();
        // Insert again — should be no-op.
        store.insert_edge(&e1).await.unwrap();

        assert_eq!(store.all_edges().len(), 1);
    }

    #[tokio::test]
    async fn mock_edge_store_get_predecessors() {
        let store = MockTypedEdgeStore::default();
        store
            .insert_edge(&make_edge("solana", "authority_wallet", "mint_a", EdgeType::AuthorityOf, Some("mint_a"), 100))
            .await
            .unwrap();

        let preds = store
            .get_predecessors("solana", "mint_a", EdgeType::AuthorityOf, 10)
            .await
            .unwrap();
        assert_eq!(preds.len(), 1);
        assert_eq!(preds[0].from_address, "authority_wallet");
    }

    #[tokio::test]
    async fn mock_edge_store_token_edges() {
        let store = MockTypedEdgeStore::default();
        store
            .insert_edge(&make_edge("solana", "creator", "mint_a", EdgeType::DeployerOf, Some("mint_a"), 100))
            .await
            .unwrap();
        store
            .insert_edge(&make_edge("solana", "auth", "mint_a", EdgeType::AuthorityOf, Some("mint_a"), 101))
            .await
            .unwrap();

        let deployer_edges = store
            .token_edges("solana", "mint_a", EdgeType::DeployerOf)
            .await
            .unwrap();
        assert_eq!(deployer_edges.len(), 1);
        assert_eq!(deployer_edges[0].from_address, "creator");
    }

    #[tokio::test]
    async fn mock_edge_store_delete_above_block() {
        let store = MockTypedEdgeStore::default();
        store
            .insert_edge(&make_edge("solana", "a", "b", EdgeType::DeployerOf, Some("mint"), 50))
            .await
            .unwrap();
        store
            .insert_edge(&make_edge("solana", "c", "d", EdgeType::DeployerOf, Some("mint2"), 150))
            .await
            .unwrap();

        let deleted = store
            .delete_edges_above_block("solana", 100)
            .await
            .unwrap();
        assert_eq!(deleted, 1, "only the block_height=150 edge should be deleted");
        assert_eq!(store.all_edges().len(), 1, "block_height=50 edge must remain");
    }

    #[tokio::test]
    async fn mock_edge_store_get_neighbors_respects_limit() {
        let store = MockTypedEdgeStore::default();
        for i in 0u64..5 {
            store
                .insert_edge(&make_edge(
                    "solana",
                    "creator",
                    &format!("mint_{i}"),
                    EdgeType::DeployerOf,
                    Some(&format!("mint_{i}")),
                    i,
                ))
                .await
                .unwrap();
        }

        let limited = store
            .get_neighbors("solana", "creator", EdgeType::DeployerOf, 3)
            .await
            .unwrap();
        assert_eq!(limited.len(), 3);
    }
}
