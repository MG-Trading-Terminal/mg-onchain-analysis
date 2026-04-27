//! `mg-onchain-graph` — wallet graph + cluster analytics.
//!
//! # What this crate provides
//!
//! - **`GraphIndexer`**: reads native SOL transfers from the `transfers` table
//!   and aggregates them into `wallet_edges` (directed funding graph).
//!
//! - **`ClusterDetector`**: runs the common-funder clustering algorithm over
//!   `wallet_edges` and writes clusters to `wallet_clusters` +
//!   `wallet_cluster_members`.
//!
//! - **`ClusterStore` trait + `PgClusterStore`**: the read API consumed by
//!   detectors (D04, D05) to query cluster membership.
//!
//! - **`GraphLabelStore` trait + `PgGraphLabelStore`** (Sprint 11): read/write
//!   API for the `address_labels` table (V00011). Graph-global node annotations:
//!   `DeployerEOA`, `FundingSource`, `KnownDex`, `Sybil`, etc.
//!
//! - **`TypedEdgeStore` trait + `PgTypedEdgeStore`** (Sprint 11): read/write
//!   API for the `graph_edges` table (V00011). Typed directed edges:
//!   `DeployerOf`, `AuthorityOf`, `TokenTransfer`, `Funding`.
//!
//! - **`MockClusterStore`**, **`MockGraphLabelStore`**, **`MockTypedEdgeStore`**
//!   (test-utils only): in-memory implementations for unit tests.
//!
//! # Dependency direction
//!
//! ```text
//! graph → storage → common
//! ```
//!
//! `graph` does NOT depend on `detectors` or `gateway`. Detectors depend on
//! `graph` (specifically `ClusterStore`, `GraphLabelStore`, `TypedEdgeStore`)
//! in Phase 3 Sprint 11+.
//!
//! # Design references
//!
//! - `docs/designs/0013-graph.md` — original graph design (Sprint 7).
//! - `docs/designs/0015-crates-graph-phase3.md` — Sprint 11 extension (V00011).

pub mod api;
pub mod clusters;
pub mod config;
pub mod cycles;
pub mod edges;
pub mod error;
pub mod labels;
pub mod smart_money;
pub mod smart_money_lookup;
pub mod typed_edges;

#[cfg(any(test, feature = "test-utils"))]
pub mod mock;

// Flat re-exports for convenient use by downstream crates.
pub use api::{ClusterKind, ClusterRef, ClusterStore, PgClusterStore};
pub use clusters::{
    bucket_edges, compute_confidence, derive_cluster_id, CandidateCluster, ClusterDetector,
    ClusterStats, FundingEdge,
};
pub use config::{load_graph_config, GraphConfig, Threshold};
pub use edges::{aggregate_edges, GraphIndexer, IndexStats, UpsertEdge, WalletEdge,
    SYSTEM_PROGRAM_ADDRESS};
pub use error::GraphError;

// Sprint 11 additions — address labels + typed edges.
pub use labels::{AddressLabel, GraphLabelStore, LabelType, PgGraphLabelStore};
pub use typed_edges::{EdgeType, GraphEdge, PgTypedEdgeStore, TypedEdgeStore};

// Sprint 12 T2-2 — cycle detection for D05 Signal B upgrade.
pub use cycles::{
    detect_cycles, fetch_recent_transfers, Cycle, CycleDetectionConfig, TransferEdge,
};

// Sprint 22 — smart-money labelling pipeline (Stage 1 + Stage 3).
pub use smart_money::{
    BatchStats, PumpEvent, RoundTrip, SmartMoneyConfig, SmartMoneyError,
    SmartMoneyLabeller, SmartMoneyTier, SwapFetcher, SwapRow, SwapSide,
    classify_tier, compute_cross_event_recurrence, compute_mean_holding_seconds,
    compute_realized_pnl_round_trips, compute_timing_lead_secs, compute_win_rate,
};
#[cfg(any(test, feature = "test-utils"))]
pub use smart_money::MockSwapFetcher;

// Sprint 23 — smart-money consumer integration (D04/D08/D05 amplification).
pub use smart_money_lookup::{GraphSmartMoneyLookup, SmartMoneyLookup, SmartMoneyLookupError};
#[cfg(any(test, feature = "test-utils"))]
pub use smart_money_lookup::MockSmartMoneyLookup;

// Test-utils re-exports (not compiled in production builds without `test-utils` feature).
#[cfg(any(test, feature = "test-utils"))]
pub use mock::{MockClusterStore, MockGraphLabelStore, MockTypedEdgeStore};
