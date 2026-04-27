# Design 0013 — `crates/graph`: Wallet Clustering + Graph Analytics

**Date:** 2026-04-21
**Status:** Draft
**Author:** architect agent
**Sprint:** 6 / P6-3
**ADR refs:**
- ADR 0001 §D5 — Sybil/bundled-launch deferred to Phase 3 (graph prerequisite)
- ADR 0001 §D8 — three delivery modes (in-process crate, REST, WS streaming)
- ADR 0002 — Postgres-only storage; all queries in PostgreSQL dialect; NUMERIC(39,0) for u128
- ADR 0003 — self-sovereign infrastructure; no 3rd-party runtime dependencies in hot path
**Related designs:**
- `docs/designs/0003-detector-trait.md` — Detector trait + DetectorContext (graph outputs consumed here)
- `docs/designs/0008-detector-05-wash-trading.md` §2 Signal B — cluster proxy this crate eventually replaces
- `docs/designs/0006-detector-03-concentration.md` — D04 insider detection, graph-enhanced in Phase 3

---

## 1. Context and Scope

### 1.1 Why graph now

Six Phase 2 detectors shipped without wallet-graph data. Two of them carry known compromises that require graph to fix:

**D05 Signal B (wash trading cluster proxy):** The current implementation computes pairwise net-flow cancellation across the top-50 senders by USD volume in a pool (O(N²) within that cap). This is a heuristic approximation of Chainalysis Heuristic 2 — "controller funds ≥5 addresses via multi-sender, <5% buy-sell imbalance." The actual Heuristic 2 requires knowing that those N addresses share a common funding source. Without the funding graph, Signal B cannot confirm that. It emits confidence 0.50-0.60 precisely because the heuristic is approximate. With a populated `wallet_clusters` table, D05 Signal B becomes: "swap senders A and B are in the same `common_funder` cluster" — deterministic, auditable, confidence 0.75+.

**D04 insider detection:** `insider_sell_pct` uses `deployer_clusters` from V00001 (Phase 2 placeholder — populated manually or by heuristic address enumeration). The graph module replaces this with funding-graph-derived insider clusters: wallets funded by the same EOA within the same time window around token launch are insider candidates. This extends D04's Signal C scope from "deployer's direct wallets" to "all wallets reachable within N hops of the deployer at token launch time."

**D08 Sybil (Phase 3 new detector):** Liu et al. (2025) (arxiv:2505.09313) achieved >0.90 precision/recall on Sybil detection using subgraph features (temporal, amount, 2-layer graph topology) with LightGBM on 193,701 addresses. The feature computation requires `wallet_edges` and `wallet_clusters` as inputs. D08 is out of Sprint 6 scope but `crates/graph` must produce the data it needs.

### 1.2 Scope stratification

| Phase | Algorithm | Sprint |
|---|---|---|
| **MVP (this design)** | Common-funder clustering from SOL native transfer graph | Sprint 6 P6-3 design / Sprint 7 implementation |
| Phase 3 Sprint 8 | Synchronized-activity clustering (same-slot first-tx timing) | Deferred |
| Phase 3 Sprint 9 | Bytecode-similarity clustering (EVM contract factory patterns) | Deferred — EVM only, Phase 4 dependency |
| Phase 3 Sprint 8-9 | D05 Signal B graph-backed replacement | Deferred integration hook |
| Phase 3 Sprint 8 | D04 insider cluster upgrade | Deferred integration hook |
| Phase 3 Sprint 9-10 | D08 Sybil detector | Deferred |

**MVP invariant:** This design specifies only what is needed to ship `wallet_edges` population and `wallet_clusters` via common-funder algorithm. Synchronized-activity, bytecode-similarity, and cross-chain clustering are named here for interface stability but not specified in detail.

### 1.3 Primary literature

- **Liu et al. (2025):** arxiv:2505.09313. "Sybil Detection via Subgraph Features." Temporal + amount + two-layer graph topology features; LightGBM classifier; AUC > 0.90 on 23,240 labeled Sybil addresses out of 193,701 total. Key finding: common funding source + synchronized timing are the strongest features. This is the primary academic anchor for the common-funder algorithm.
- **Chainalysis (2025):** Wash Trading report. Heuristic 2 — "controller funds ≥5 addresses via token multi-sender" — is the operational motivation for common-funder clustering. $1.87B in wash volume attributed to Heuristic 2 patterns. Source confirms the graph-funding signal is financially material.
- **Messias, Yaish & Livshits (2023):** arxiv:2312.02752. Airdrop farming tactics; Sybil vulnerability. Documents the "common-funder + synchronized deposit" pattern as the primary Sybil farming method.
- **research/02-detection-methodology.md §8:** Sybil pseudocode + threshold derivation; Cross-cutting B: graph algorithm inventory.

---

## 2. Crate Shape

```
crates/graph/
  Cargo.toml
  src/
    lib.rs          # pub use: GraphIndexer, ClusterDetector, GraphConfig, ClusterRef, GraphError
    config.rs       # GraphConfig — all thresholds with Threshold<T> wrapper; TOML loader
    edges.rs        # wallet_edges aggregation: UpsertEdge, EdgeRow, GraphIndexer::index_transfers()
    clusters.rs     # common-funder algorithm: ClusterDetector::run_common_funder()
    error.rs        # GraphError (thiserror, #[non_exhaustive])
    api.rs          # Read API: ClusterStore trait + PgClusterStore impl
    mock.rs         # MockClusterStore (cfg(test) only)
  tests/
    common_funder_test.rs   # unit tests for common-funder algorithm (pure compute, no DB)
    edges_aggregation_test.rs  # unit tests for edge aggregation logic
```

**Crate dependency direction:** `graph` depends on `common` and `storage`. It does NOT depend on `detectors` or `gateway`. Detectors depend on `graph` via the `ClusterStore` trait (see §7). This keeps the dependency arrow pointing inward (graph → storage → common), never outward.

---

## 3. Type Shapes

### 3.1 GraphConfig

```rust
// config.rs
use crate::super::Threshold;

pub struct GraphConfig {
    /// Hours within which two wallets funded by the same source are considered co-funded.
    /// Default: 24. Source: Liu et al. (2025) temporal clustering parameter.
    pub cofunding_window_hours: Threshold<u32>,

    /// Maximum fractional difference in funding amounts for two wallets to be
    /// placed in the same amount-bucket.
    /// Default: 0.20 (20%). Source: calibrated against Messias et al. (2023) examples.
    pub amount_similarity_pct: Threshold<f64>,

    /// Minimum number of wallets in a cluster to emit a cluster record.
    /// Default: 3. Source: Chainalysis (2025) Heuristic 2 minimum (≥5 funded addresses);
    /// 3 chosen as MVP lower bound to catch smaller clusters while keeping FP rate acceptable.
    pub min_cluster_size: Threshold<u32>,

    /// Minimum SOL amount (in lamports) sent by a funder for the transfer to be considered
    /// a funding event. Dust filter. Default: 10_000_000 (0.01 SOL).
    pub min_funder_sol_amount: Threshold<u64>,

    /// Batch size for reading transfers from Postgres during indexing.
    /// Default: 10_000. Tune against Postgres sequential scan perf.
    pub indexer_batch_size: Threshold<u32>,

    /// How long (hours) before a cluster record is considered stale and re-computation
    /// is triggered. Default: 168 (7 days). Source: Chainalysis updates labels weekly.
    pub cluster_ttl_hours: Threshold<u32>,
}
```

`Threshold<T>` is the same wrapper defined in `crates/detectors/src/config.rs`:
```rust
pub struct Threshold<T> {
    pub value: T,
    pub rationale: String,
    pub refs: Vec<String>,
}
```

`crates/graph` re-defines this locally or a future refactor moves it to `crates/common`. For MVP, duplicate is acceptable to keep `graph` independent of `detectors`.

### 3.2 WalletEdge

```rust
// edges.rs

/// A directed funding edge in the wallet graph.
///
/// Corresponds to one row in the `wallet_edges` Postgres table.
/// Represents the aggregate of all SOL transfers from `from_wallet` to `to_wallet`
/// within the indexed block range.
///
/// Amounts are `u128` raw lamports stored as NUMERIC(39,0) via the String bridge
/// (see docs/designs/0002-storage-schemas-v1.md §type-mapping).
#[derive(Debug, Clone)]
pub struct WalletEdge {
    pub chain: String,
    pub from_wallet: String,
    pub to_wallet: String,
    /// Total SOL transferred, in lamports. Serialized as string to Postgres.
    pub total_sol_lamports: u128,
    pub tx_count: i64,
    pub first_tx_time: chrono::DateTime<chrono::Utc>,
    pub last_tx_time: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// One transfer event to upsert into wallet_edges.
/// Built by GraphIndexer from a batch of Transfer rows.
pub struct UpsertEdge {
    pub chain: String,
    pub from_wallet: String,
    pub to_wallet: String,
    pub sol_lamports: u128,
    pub tx_time: chrono::DateTime<chrono::Utc>,
}
```

### 3.3 ClusterRef

```rust
// api.rs

/// A lightweight reference to a wallet cluster, returned by the read API.
///
/// Detectors consume this: check if two wallets are in the same cluster without
/// loading all member addresses.
#[derive(Debug, Clone)]
pub struct ClusterRef {
    pub cluster_id: uuid::Uuid,
    pub chain: String,
    pub cluster_kind: ClusterKind,
    /// The funding wallet that defines this cluster. None for non-funder-based kinds.
    pub root_funder: Option<String>,
    pub member_count: u32,
    /// Confidence that this cluster represents coordinated activity. [0.0, 1.0].
    pub confidence: f64,
    pub computed_at: chrono::DateTime<chrono::Utc>,
}

/// The algorithm that produced this cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClusterKind {
    /// Common-funder: wallets funded by the same EOA within a time window.
    /// MVP — this design.
    CommonFunder,
    /// Synchronized first-tx timing (same slot or adjacent slots).
    /// Phase 3 Sprint 8 — deferred.
    SynchronizedActivity,
    /// EVM contract bytecode similarity (same factory pattern).
    /// Phase 3 Sprint 9 — deferred, EVM only.
    BytecodeSimilar,
}
```

---

## 4. Common-Funder Algorithm (MVP)

### 4.1 Definition

Two wallets A and B are placed in the same `common_funder` cluster if:

1. **Same funder:** Wallet F sent SOL to both A and B (appearing in `wallet_edges` as `from_wallet = F`).
2. **Time window:** F's first funding transfer to A and F's first funding transfer to B both occurred within `cofunding_window_hours` of each other (measured by `first_tx_time` on the edge).
3. **Amount similarity:** The SOL amounts sent by F to A and F to B differ by no more than `amount_similarity_pct`. Specifically: `|lamports_A - lamports_B| / max(lamports_A, lamports_B) <= amount_similarity_pct`.
4. **Minimum cluster size:** The total count of wallets funded by F (within the window and amount bucket) is at least `min_cluster_size`.
5. **Minimum funder amount:** `F → A` and `F → B` transfers must each exceed `min_funder_sol_amount` lamports (dust filter).

The "first-funding constraint" in the task brief — "neither A nor B were funded by any other wallet before F" — is too strict for MVP. It disqualifies wallets that received any prior SOL (e.g. from a CEX withdraw or airdrop) before the funder. Instead, MVP uses the simpler condition: F's transfer to A preceded any swap activity from A by at least one block. This is implemented as: `edge.first_tx_time < first_swap_from_A`. The full first-funding constraint is Phase 3 Sprint 8 enhancement after the `wallet_edges` table is populated for real data.

### 4.2 Algorithm sketch

```
INPUT: wallet_edges rows WHERE chain = $chain AND total_sol_lamports >= min_funder_sol_amount
OUTPUT: rows for wallet_clusters and wallet_cluster_members

STEP 1 — Group by funder:
  SELECT from_wallet AS funder,
         to_wallet   AS recipient,
         total_sol_lamports,
         first_tx_time
  FROM wallet_edges
  WHERE chain = $chain
    AND total_sol_lamports >= $min_funder_sol_amount
  ORDER BY funder, first_tx_time

STEP 2 — Partition by time window within each funder:
  For each (funder F, sorted list of (recipient, amount, time)):
    Assign recipients to time-windows of width cofunding_window_hours.
    Two recipients are in the same window if their first_tx_time values differ
    by at most cofunding_window_hours * 3600 seconds.
    Window is anchored to the first recipient seen for F (tumbling, not sliding).

STEP 3 — Partition by amount bucket within each time window:
  Within each (funder, time-window) group, sort recipients by amount.
  Group into buckets where adjacent amounts differ by ≤ amount_similarity_pct.
  (Bucket algorithm: sort ascending; bucket breaks where ratio > (1 + amount_similarity_pct))

STEP 4 — Filter by min_cluster_size:
  Emit a cluster for each (funder, time-window, amount-bucket) where bucket_size >= min_cluster_size.

STEP 5 — Compute confidence:
  confidence = compute_confidence(bucket_size, time_variance_seconds)
  (See §11 for formula.)

STEP 6 — Upsert:
  INSERT INTO wallet_clusters (cluster_id, chain, cluster_kind, root_funder, member_count, confidence, computed_at, evidence)
  INSERT INTO wallet_cluster_members (cluster_id, chain, wallet) for each member
  ON CONFLICT: update confidence + computed_at if new confidence > old confidence.
```

**Complexity:** O(N log N) where N = qualifying `wallet_edges` rows for the chain. Sort by funder + time is the dominant cost. All steps are implementable as a single Postgres query using window functions + GROUP BY, avoiding Rust-side graph library dependency for MVP.

### 4.3 Postgres query sketch

The algorithm can be expressed in SQL for execution efficiency. The Rust `ClusterDetector::run_common_funder()` method issues this query and processes results into upsert batches:

```sql
-- Step 2+3+4+5 expressed as window functions
WITH funded AS (
    SELECT
        from_wallet                                           AS funder,
        to_wallet                                            AS recipient,
        total_sol_lamports,
        first_tx_time,
        -- Assign a tumbling window ID per funder
        -- Window bucket = floor of seconds since funder's first-ever transfer
        -- divided by window_size_seconds.
        EXTRACT(EPOCH FROM (
            first_tx_time
            - FIRST_VALUE(first_tx_time) OVER (
                PARTITION BY from_wallet
                ORDER BY first_tx_time
                ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
            )
        )) / $cofunding_window_secs AS time_bucket
    FROM wallet_edges
    WHERE chain = $chain
      AND total_sol_lamports >= $min_funder_sol_amount
),
amount_buckets AS (
    -- Amount similarity grouping: use ntile or manual bucket by sorted rank.
    -- Simplified: group by integer bucket = floor(log(lamports) / log(1 + amount_similarity_pct))
    -- This places amounts within amount_similarity_pct of each other in the same bucket.
    SELECT
        funder,
        FLOOR(time_bucket)::BIGINT                           AS time_bucket_id,
        recipient,
        total_sol_lamports,
        FLOOR(LN(total_sol_lamports::DOUBLE PRECISION)
              / LN(1.0 + $amount_similarity_pct))::BIGINT    AS amount_bucket_id
    FROM funded
),
clusters AS (
    SELECT
        funder,
        time_bucket_id,
        amount_bucket_id,
        ARRAY_AGG(recipient ORDER BY recipient)               AS members,
        COUNT(*)                                              AS member_count,
        STDDEV(EXTRACT(EPOCH FROM
            (SELECT first_tx_time FROM funded f2
             WHERE f2.funder = amount_buckets.funder
               AND f2.recipient = amount_buckets.recipient
             LIMIT 1)
        ) / 1.0) AS time_stddev_seconds
    FROM amount_buckets
    GROUP BY funder, time_bucket_id, amount_bucket_id
    HAVING COUNT(*) >= $min_cluster_size
)
SELECT * FROM clusters ORDER BY funder, time_bucket_id, amount_bucket_id;
```

In practice the developer should benchmark this against `EXPLAIN ANALYZE` on a populated `wallet_edges` table and may split into smaller CTEs or move the time_stddev computation to Rust-side post-processing. The SQL above is a design sketch, not a final query.

---

## 5. Storage Schema — Migration V00009

```sql
-- =============================================================================
-- V00009__wallet_graph.sql  —  Wallet graph + clustering tables
-- =============================================================================
-- Migration tool: sqlx migrate (sqlx-cli).
-- Apply: `sqlx migrate run --database-url $DATABASE_URL`
--        or via `StorageConfig.migrations_auto_apply = true` at service startup.
--
-- Tables in this file:
--   wallet_edges            — directed funding edges (SOL native transfers, MVP)
--   wallet_clusters         — derived clusters of commonly-funded wallets
--   wallet_cluster_members  — cluster ↔ wallet membership (normalized)
--
-- Design constraints (ADR 0002):
--   - Postgres-only; no ClickHouse. wallet_edges row count at MVP scale (100
--     tracked tokens, 1 year): estimated 5-15M rows (see §8 performance).
--     Postgres handles this with a B-tree PK + partial indexes.
--   - u128 amounts (lamports): NUMERIC(39,0) via String bridge, matching the
--     pattern established in V00001 + V00002.
--   - UPSERT pattern for wallet_edges: ON CONFLICT DO UPDATE accumulates
--     total_sol_lamports, tx_count, last_tx_time — O(1) per incoming Transfer.
--   - wallet_clusters rows are periodically recomputed (cluster_ttl_hours).
--     ON CONFLICT DO UPDATE overwrites confidence + computed_at.
-- =============================================================================

-- ---------------------------------------------------------------------------
-- wallet_edges: directed funding edges (SOL native transfers in MVP)
-- ---------------------------------------------------------------------------
-- One row per (chain, from_wallet, to_wallet) — the aggregate of all
-- SOL transfers between two wallets. Raw graph: not clustered yet.
--
-- Populated by crates/graph GraphIndexer which reads from the `transfers` table
-- (already populated by the indexer) and filters for:
--   - is_mint = false AND is_burn = false (native SOL transfers only in MVP)
--   - amount_raw >= min_funder_sol_amount (dust filter)
--
-- UPSERT semantics: each new qualifying Transfer event increments tx_count,
-- adds to total_sol_lamports, updates last_tx_time, and preserves first_tx_time.
-- This is O(1) per incoming transfer — no re-scan of history needed.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS wallet_edges (
    chain               TEXT            NOT NULL,
    from_wallet         TEXT            NOT NULL,
    to_wallet           TEXT            NOT NULL,
    -- Total SOL sent across all qualifying transfers, in lamports (u128 via NUMERIC(39,0)).
    -- String bridge: bind as to_string() on write; parse::<u128>() on read.
    total_sol_lamports  NUMERIC(39,0)   NOT NULL,
    -- Count of qualifying Transfer events between this pair.
    tx_count            BIGINT          NOT NULL,
    -- Timestamp of the first qualifying transfer (used in time-window bucketing).
    first_tx_time       TIMESTAMPTZ     NOT NULL,
    -- Timestamp of the most recent qualifying transfer.
    last_tx_time        TIMESTAMPTZ     NOT NULL,
    -- Housekeeping: last time this row was written (set by application).
    updated_at          TIMESTAMPTZ     NOT NULL DEFAULT now(),

    PRIMARY KEY (chain, from_wallet, to_wallet)
);

-- Reverse lookup: "who funded wallet X?" — used in common-funder algorithm and
-- detector integration (is_in_cluster, funder_cluster_of).
CREATE INDEX IF NOT EXISTS idx_wallet_edges_to_wallet
    ON wallet_edges (chain, to_wallet);

-- Time-range scan for the common-funder algorithm: find all wallets funded by F
-- within a time window. Composite on (chain, from_wallet, first_tx_time) enables
-- efficient range scans in the GROUP BY + HAVING query.
CREATE INDEX IF NOT EXISTS idx_wallet_edges_from_time
    ON wallet_edges (chain, from_wallet, first_tx_time);

-- Amount filter partial index: pre-filters qualifying edges (avoids full scan
-- when min_funder_sol_amount filter is applied frequently).
-- Threshold mirrors config default; update if config changes significantly.
CREATE INDEX IF NOT EXISTS idx_wallet_edges_qualifying
    ON wallet_edges (chain, from_wallet, total_sol_lamports DESC)
    WHERE total_sol_lamports >= 10000000;  -- 0.01 SOL default

-- ---------------------------------------------------------------------------
-- wallet_clusters: derived groups of wallets sharing a common funding source
-- ---------------------------------------------------------------------------
-- One row per cluster. Clusters are recomputed periodically (cluster_ttl_hours).
-- Member addresses live in wallet_cluster_members (normalized, avoids array bloat).
--
-- cluster_kind CHECK constraint lists all currently defined algorithms.
-- Adding a new algorithm requires a migration to update the CHECK constraint.
-- (Alternative: remove CHECK and enforce in application code. Decision: keep CHECK
-- to catch bugs at the DB level. Cost: one migration per new algorithm, which is
-- acceptable given the Phase 3 cadence.)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS wallet_clusters (
    cluster_id      UUID            PRIMARY KEY DEFAULT gen_random_uuid(),
    chain           TEXT            NOT NULL,
    -- Algorithm that produced this cluster.
    cluster_kind    TEXT            NOT NULL
                        CHECK (cluster_kind IN (
                            'common_funder',
                            'synchronized_activity',
                            'bytecode_similar'
                        )),
    -- The wallet that funded all members (NULL for non-funder-based cluster kinds).
    root_funder     TEXT,
    -- Denormalized count for quick cardinality checks without JOIN.
    member_count    INT             NOT NULL CHECK (member_count >= 2),
    -- Confidence that this cluster represents coordinated activity.
    -- DOUBLE PRECISION is appropriate here: confidence is a probability, not a money amount.
    confidence      DOUBLE PRECISION NOT NULL
                        CHECK (confidence >= 0.0 AND confidence <= 1.0),
    -- When was the cluster last computed?
    computed_at     TIMESTAMPTZ     NOT NULL DEFAULT now(),
    -- JSON evidence: algorithm parameters used, time_variance_seconds, amount_range,
    -- representative tx hashes. JSONB for structured query support.
    evidence        JSONB           NOT NULL DEFAULT '{}'::jsonb
);

-- Common access patterns:
--   1. "All common_funder clusters on solana" (cluster computation scheduling)
CREATE INDEX IF NOT EXISTS idx_wallet_clusters_chain_kind
    ON wallet_clusters (chain, cluster_kind);

--   2. "Clusters for a given root funder" (D05/D04 integration)
CREATE INDEX IF NOT EXISTS idx_wallet_clusters_chain_funder
    ON wallet_clusters (chain, root_funder)
    WHERE root_funder IS NOT NULL;

--   3. "Recently computed clusters" (TTL staleness check)
CREATE INDEX IF NOT EXISTS idx_wallet_clusters_computed_at
    ON wallet_clusters (chain, computed_at DESC);

-- ---------------------------------------------------------------------------
-- wallet_cluster_members: cluster ↔ wallet membership
-- ---------------------------------------------------------------------------
-- Normalized: one row per (cluster_id, wallet). Avoids storing large TEXT[]
-- arrays in wallet_clusters.evidence.
--
-- Cascade delete: removing a cluster cascades to its members, keeping the
-- tables consistent during re-computation runs.
--
-- Many-to-many note: a wallet CAN appear in multiple clusters (e.g. funded by
-- two different funders in different time windows). The PRIMARY KEY on
-- (cluster_id, wallet) enforces uniqueness within a cluster, not across clusters.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS wallet_cluster_members (
    cluster_id  UUID        NOT NULL
                    REFERENCES wallet_clusters(cluster_id) ON DELETE CASCADE,
    chain       TEXT        NOT NULL,
    wallet      TEXT        NOT NULL,
    -- When this membership was recorded.
    joined_at   TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (cluster_id, wallet)
);

-- Wallet lookup: "Is wallet X in any cluster?" — primary read-API access pattern.
CREATE INDEX IF NOT EXISTS idx_wallet_cluster_members_wallet
    ON wallet_cluster_members (chain, wallet);

-- Cluster membership scan: "All members of cluster C" — used in evidence building.
-- Already covered by the PRIMARY KEY index (cluster_id, wallet) but an explicit
-- index on cluster_id alone is faster for unordered member fetches.
CREATE INDEX IF NOT EXISTS idx_wallet_cluster_members_cluster
    ON wallet_cluster_members (cluster_id);
```

---

## 6. Indexer Integration

### 6.1 Data source

`GraphIndexer` reads from the existing `transfers` table (populated by `crates/indexer`). It does NOT re-parse raw blocks or depend on Yellowstone gRPC directly. This is a deliberate separation: the indexer pipeline owns ingestion; the graph crate owns derived-graph computation.

**Filter criteria for wallet_edges population (MVP — SOL native transfers only):**

```sql
SELECT chain, from_address, to_address, amount_raw, block_time
FROM transfers
WHERE chain = $chain
  AND token = $sol_native_mint_address  -- SOL: '11111111111111111111111111111111' system program
  AND is_mint = false
  AND is_burn = false
  AND amount_raw >= $min_funder_sol_amount
  AND block_time > $last_indexed_at        -- incremental: only new transfers
ORDER BY block_time ASC
LIMIT $indexer_batch_size;
```

The SOL native mint address on Solana is represented as transfers FROM the System Program (`11111111111111111111111111111111`). In the existing `transfers` table schema, native SOL transfers between wallets are stored with `token = '11111111111111111111111111111111'` (the System Program / null address acts as the "token" for SOL). The developer must verify the exact convention used by the Yellowstone gRPC adapter for native SOL transfer representation before implementing the filter.

SPL Token transfers (activity events) are deferred. They add noise to the funding graph because token transfers do not represent economic funding of a wallet's gas budget in the same way SOL does.

### 6.2 Checkpoint

`GraphIndexer` stores its progress in `adapter_checkpoints` (V00001 table) using adapter_id = `"graph_indexer_{chain}"` (e.g. `"graph_indexer_solana"`). This reuses the existing checkpoint infrastructure without schema changes.

### 6.3 Invocation cadence

`GraphIndexer::index_transfers()` is called by `crates/server` on a scheduled interval — not in the hot detector-evaluation path. Suggested cadence: every 10 minutes for edge accumulation, every `cluster_ttl_hours` for cluster recomputation. These are separate background tasks with independent failure domains.

### 6.4 Incremental UPSERT pattern

```rust
// Pseudocode for edge accumulation (edges.rs)
for batch in transfer_batches {
    for transfer in batch {
        // UPSERT: accumulate
        sqlx::query(r#"
            INSERT INTO wallet_edges
                (chain, from_wallet, to_wallet, total_sol_lamports, tx_count,
                 first_tx_time, last_tx_time, updated_at)
            VALUES ($1, $2, $3, $4, 1, $5, $5, now())
            ON CONFLICT (chain, from_wallet, to_wallet) DO UPDATE SET
                total_sol_lamports = wallet_edges.total_sol_lamports
                                     + EXCLUDED.total_sol_lamports,
                tx_count           = wallet_edges.tx_count + 1,
                last_tx_time       = GREATEST(wallet_edges.last_tx_time,
                                              EXCLUDED.last_tx_time),
                updated_at         = now()
        "#)
        .bind(&transfer.chain)
        .bind(&transfer.from_address)
        .bind(&transfer.to_address)
        .bind(transfer.amount_raw.to_string())  // String bridge for NUMERIC(39,0)
        .bind(transfer.block_time)
        .execute(&pool).await?;
    }
}
```

The `ON CONFLICT DO UPDATE` with addition is O(1) per transfer — no aggregation scan needed. `first_tx_time` is preserved by NOT including it in the UPDATE clause (the original INSERT value is kept by the conflict exclusion).

---

## 7. Read API (ClusterStore Trait)

The read API is the surface that detectors consume. It is defined as a trait so tests can inject `MockClusterStore` without a live database.

```rust
// api.rs

use uuid::Uuid;
use crate::error::GraphError;

/// The read API consumed by detectors that need cluster membership information.
///
/// # Object-safety
///
/// This trait IS object-safe (no generic methods, no Self parameters in returns).
/// Detectors receive a `&dyn ClusterStore` in their context. This differs from
/// the `Detector` trait (which is NOT object-safe by design). The asymmetry is
/// intentional: `ClusterStore` implementations vary (Pg vs Mock) but are few;
/// `Detector` implementations vary in async signature and are many.
///
/// # Async
///
/// All methods are async. In tests, `MockClusterStore` returns `Ready` futures
/// without any runtime dependency.
pub trait ClusterStore: Send + Sync {
    /// Returns the cluster (if any) that `wallet` belongs to on `chain`.
    ///
    /// If a wallet belongs to multiple clusters, returns the highest-confidence one.
    /// Returns `Ok(None)` if the wallet is not in any cluster.
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

    /// Returns all member wallets of a cluster.
    ///
    /// Used by D05 integration: once two swap senders are found to be in the same
    /// cluster, this call fetches all cluster members to build the evidence bundle.
    async fn cluster_members(
        &self,
        cluster_id: Uuid,
    ) -> Result<Vec<String>, GraphError>;

    /// Returns the cluster anchored to `root_funder` on `chain`, if one exists.
    ///
    /// Used by D04 integration: given the deployer address, find the cluster of
    /// wallets it funded — those are the insider cluster members.
    async fn funder_cluster(
        &self,
        chain: &str,
        root_funder: &str,
    ) -> Result<Option<ClusterRef>, GraphError>;

    /// Returns true if `wallet_a` and `wallet_b` are in the same cluster on `chain`.
    ///
    /// Convenience method for the common D05 pattern: "are these two swap senders
    /// co-clustered?" Implemented as: fetch cluster for A, fetch cluster for B,
    /// compare cluster_ids. The default implementation does this; PgClusterStore
    /// may implement it with a single SQL query for efficiency.
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

/// Postgres-backed implementation of ClusterStore.
pub struct PgClusterStore {
    pub pool: sqlx::PgPool,
}

// impl ClusterStore for PgClusterStore: developer implements SQL queries.
```

### 7.1 Detector integration: how context carries the store

`DetectorContext` (defined in `docs/designs/0003-detector-trait.md`) must gain a new optional field for the graph cluster store. The design here defers the exact `DetectorContext` extension to the developer task for each integration hook (D05/D04). Proposal for the developer:

```rust
// In crates/detectors/src/context.rs (Phase 3 addition)
pub struct DetectorContext<'ctx> {
    // ... existing fields ...

    /// Graph cluster store. `None` when the graph crate is not available
    /// (pre-Phase-3 detectors) or when the cluster table is not yet populated.
    /// Phase 3 detectors that require graph MUST return
    /// `Err(DetectorError::MissingDependencyData)` when this is `None`.
    pub cluster_store: Option<&'ctx dyn crate::graph_api::ClusterStore>,
}
```

`graph_api` here is a thin re-export crate or the detector crate directly re-exports `ClusterStore` from `crates/graph`. The developer resolves the import path when implementing the integration.

---

## 8. Performance Considerations and Scaling Estimates

### 8.1 wallet_edges table size estimate

At MVP scope: 100 tracked tokens, 1 year of SOL funding transfers.

Solana Mainnet sees approximately 20-50k transactions per slot (400ms), but the vast majority are program instructions (DEX trades, NFT mints, etc.), not native SOL transfers between EOAs. Empirically, native SOL transfers between wallet accounts represent roughly 2-5% of total Solana transactions. At 2,000 TPS average (conservative) and 5% SOL transfer rate:

```
SOL transfers per second:   ~100
SOL transfers per day:      ~8.6M
SOL transfers per year:     ~3.1B total on Solana mainnet
```

However, `wallet_edges` stores aggregate rows (one row per wallet pair, not per transfer). The number of distinct wallet pairs is much smaller than the transfer count. A reasonable estimate: 1B transfers over 1 year produce perhaps 10-50M distinct (from, to) pairs. At MVP with 100 tracked tokens, only wallets that interact with those tokens (or fund wallets that interact with them) are captured. In practice: 100 tokens × ~5,000 active wallets per token × average degree 3 = ~1.5M wallet_edges rows.

Row size estimate: 2 TEXT fields (avg 44 bytes each) + NUMERIC(39,0) + BIGINT + 3× TIMESTAMPTZ = ~200 bytes per row. At 1.5M rows: ~300 MB. Well within single-Postgres comfort zone.

**If scope grows to unfiltered Solana firehose:** 50M distinct pairs × 200 bytes = ~10 GB. Postgres handles this with a B-tree PK and the partial index. If query latency degrades beyond 100ms for the common-funder algorithm, the ADR 0002 escape hatch (TimescaleDB hypertable conversion) applies.

### 8.2 wallet_clusters and wallet_cluster_members size estimate

Clusters are sparse: not every wallet pair is a cluster. Conservative estimate: 1% of wallet_edges from wallet count become cluster members.

```
wallet_edges rows:          1.5M
distinct wallets (to_wallet): ~750K
cluster members (1%):       ~7,500
clusters (avg size 5):      ~1,500
```

`wallet_clusters`: 1,500 rows × ~500 bytes (UUID + metadata + evidence JSONB) = ~750 KB. Trivial.
`wallet_cluster_members`: 7,500 rows × ~100 bytes = ~750 KB. Trivial.

At unfiltered scale (50M edges → 25M distinct wallets): clusters = ~250K, members = ~1.25M, cluster_members rows = ~1.25M × 100 bytes = ~125 MB. Still Postgres-appropriate.

### 8.3 Cluster recomputation cost

The common-funder SQL query scans `wallet_edges` with a GROUP BY + HAVING. At 1.5M rows with the `idx_wallet_edges_from_time` B-tree index on `(chain, from_wallet, first_tx_time)`:

- Full re-computation: sequential scan over the qualifying rows, O(N log N) due to the sort. At 1.5M rows: estimated 2-5 seconds.
- Incremental re-computation (only edges updated since `computed_at`): add `WHERE updated_at > $last_cluster_run` filter. Reduces to O(delta) where delta is transfers since last run.

Cluster recomputation is a background task; 2-5 second latency is acceptable.

### 8.4 Read-API latency for detectors

`wallet_cluster(chain, wallet)` = one indexed lookup on `wallet_cluster_members (chain, wallet)` + one join to `wallet_clusters`. Estimated: <5ms at 1.5M rows with the B-tree index. This is within the detector evaluation budget.

`are_co_clustered(chain, wallet_a, wallet_b)` = two indexed lookups + compare. Estimated: <10ms. Acceptable for the D05 hot path (D05 already issues multi-query SQL for Signal A; this is additive, not blocking).

### 8.5 UPSERT throughput for GraphIndexer

The incremental UPSERT pattern (§6.4) issues one `INSERT ... ON CONFLICT DO UPDATE` per qualifying transfer. At 100 qualifying transfers/second (conservative, post-filter), this is 100 Postgres writes/second — well within Postgres's single-connection write budget of ~10,000 simple writes/second. Batching is not required for MVP but can be added if throughput grows.

---

## 9. Detector Integration Plan

This section describes the integration hooks for Phase 3 work. None of this is Sprint 6 scope. The design defines the hooks to ensure `crates/graph`'s API is stable enough to support them without modification.

### 9.1 D05 Signal B — Graph-backed replacement

**Current state (Phase 2):** Signal B is a pairwise net-flow cancellation check across the top-50 senders in a pool (O(N²) within cap). Confidence 0.50-0.60 because funding provenance is unverified.

**Phase 3 replacement:**

```
FUNCTION evaluate_signal_b_graph(ctx: DetectorContext) -> Result<Vec<AnomalyEvent>>:

  sender_rows = fetch_pool_senders(ctx, cfg).await  // existing D05 query
  
  // For each pair of senders, check co-cluster membership
  cluster_pairs = []
  FOR i IN 0..sender_rows.len():
    FOR j IN i+1..sender_rows.len():
      IF ctx.cluster_store.are_co_clustered(chain, sender_rows[i].sender, sender_rows[j].sender).await?:
        cluster = ctx.cluster_store.wallet_cluster(chain, sender_rows[i].sender).await?
        cluster_pairs.push((sender_rows[i], sender_rows[j], cluster))
  
  IF cluster_pairs is non-empty:
    -- Graph-backed confidence: higher because funding provenance is verified
    confidence = 0.75 + min(0.20, 0.05 * distinct_cluster_member_count)
    evidence = {
      "wash_trading_h1/signal_b_source": "graph_cluster",
      "wash_trading_h1/cluster_id": cluster_id,
      "wash_trading_h1/cluster_kind": "common_funder",
      "wash_trading_h1/cluster_member_count": member_count,
    }
    RETURN Ok(vec![make_anomaly_event(confidence, evidence)])
  ELSE:
    -- Fall back to existing pairwise heuristic if cluster_store is None or no cluster found
    RETURN existing_signal_b_pairwise(ctx, cfg)
```

**Confidence upgrade:** 0.50-0.60 (heuristic) → 0.75-0.95 (graph-verified). This upgrade requires a re-calibration run against the wash-trading positive fixture corpus (`research/fixtures/wash_trading/POS_01_synth_single_wallet.json`) and new positive fixtures with multi-wallet patterns.

**Developer task tag:** `D05-GRAPH-INTEGRATION` — Phase 3 Sprint 8.

### 9.2 D04 — Graph-backed insider cluster

**Current state (Phase 2):** `deployer_clusters` table (V00001) stores insider wallets populated by manual enumeration or heuristic address tracing. D04 Signal C (`insider_sell_pct`) counts sells from these wallets.

**Phase 3 upgrade:**

```
FUNCTION enrich_insider_cluster_from_graph(deployer: Address, chain: Chain,
                                           cluster_store: &dyn ClusterStore)
    -> Vec<String>:  // insider wallet addresses

  // Approach 1: direct funder lookup
  cluster = cluster_store.funder_cluster(chain, deployer.as_str()).await?
  IF cluster is Some:
    members = cluster_store.cluster_members(cluster.cluster_id).await?
    RETURN members  // all wallets funded by the deployer = insider candidates

  // Approach 2: deployer IS a funded wallet (deployer was itself funded by a meta-funder)
  deployer_cluster = cluster_store.wallet_cluster(chain, deployer.as_str()).await?
  IF deployer_cluster is Some:
    members = cluster_store.cluster_members(deployer_cluster.cluster_id).await?
    RETURN members

  // Fallback: deployer has no cluster → use deployer_clusters table as before
  RETURN existing_deployer_cluster_lookup(chain, token, pg_store).await?
```

This means insider cluster size can grow from the typical 2-5 manually-enumerated deployer wallets to potentially 50+ graph-discovered wallets, improving `insider_sell_pct` accuracy.

**Developer task tag:** `D04-GRAPH-INTEGRATION` — Phase 3 Sprint 8.

### 9.3 D08 Sybil — New detector

D08 fires when a common-funder cluster has created multiple token launches within a short time window, indicating a serial rug-pull operation.

**Signal definition:** A `common_funder` cluster where ≥2 member wallets appear as `creator` in the `tokens` table within `sybil_launch_window_hours` is flagged as a Sybil/serial-launcher cluster.

**Input:** `wallet_cluster_members JOIN tokens ON (chain, wallet = creator) GROUP BY cluster_id`. No additional data sources required beyond Phase 3 baseline.

**Confidence:** `sigmoid((cluster_launches - 2) / 2)` — starts at 0.50 for 2 launches, approaches 1.0 for 5+.

**Developer task tag:** `D08-SYBIL` — Phase 3 Sprint 9-10. Out of Sprint 6 scope. Design stub registered here to lock the `wallet_clusters` + `wallet_cluster_members` schema against D08's requirements.

---

## 10. Thresholds Table

All thresholds live in `config/detectors.toml` under the `[graph]` section, following the structured `{ value, rationale, refs }` shape established in `docs/designs/0003-detector-trait.md §config.rs`.

| Key | Default | Rationale | Source |
|---|---|---|---|
| `cofunding_window_hours` | 24 | Liu et al. (2025): temporal clustering of Sybil addresses within 1 hour; extended to 24h for SOL funding (funding precedes activity by up to 1 day in practice). | arxiv:2505.09313 §4.1 |
| `amount_similarity_pct` | 0.20 (20%) | Messias et al. (2023): airdrop farmers fund wallets with near-identical amounts. 20% tolerance accounts for gas price variation. No published threshold — calibrate from data. | arxiv:2312.02752; internal calibration |
| `min_cluster_size` | 3 | Chainalysis (2025) Heuristic 2 uses ≥5; 3 chosen as MVP lower bound to maximize recall at cost of precision. Adjust upward after empirical FP measurement. | Chainalysis 2025 wash trading report |
| `min_funder_sol_amount` | 10_000_000 (0.01 SOL) | Dust filter: Solana rent-exempt minimum per account is ~890,880 lamports (~0.00089 SOL). 0.01 SOL is 11× rent-exempt minimum — meaningful funding intent. Common airdrop amounts are ≥0.05 SOL. | Solana docs (rent exemption), empirical |
| `indexer_batch_size` | 10_000 | Postgres row fetch batch; tuned for ~100ms query latency on a populated `transfers` table. Adjust based on `EXPLAIN ANALYZE` timing. | Empirical; no published reference |
| `cluster_ttl_hours` | 168 (7 days) | Chainalysis publishes cluster label updates weekly. Clusters change slowly; re-computation is O(edges) — weekly is adequate for MVP. | Chainalysis label update cadence (2025 report) |

---

## 11. Confidence Computation

The confidence of a `common_funder` cluster is a function of two factors:

1. **Cluster size:** More members = higher confidence that the cluster is coordinated rather than coincidental. Minimum size 3 corresponds to confidence 0.50; confidence saturates at 0.85 for size ≥ 10.

2. **Time variance:** Tighter timing = higher confidence. Wallets funded within minutes of each other are more suspicious than wallets funded hours apart within the window.

**Formula:**

```
size_term     = min(1.0, (member_count - min_cluster_size) / (10.0 - min_cluster_size))
                -- 0.0 at min_cluster_size=3, 1.0 at 10+

time_term     = 1.0 - min(1.0, time_stddev_seconds / (cofunding_window_hours * 3600.0))
                -- 1.0 when all wallets funded at exactly the same time,
                -- 0.0 when timing spread equals the full window

confidence    = 0.50 + 0.25 * size_term + 0.10 * time_term
                -- range: [0.50, 0.85]
```

**Calibration anchors:**
- 3 wallets funded at the same minute: 0.50 + 0.0 + 0.10 = 0.60
- 5 wallets funded within 1 hour of each other (24h window): 0.50 + 0.07 + ~0.08 = 0.65
- 10+ wallets funded within 10 minutes: 0.50 + 0.25 + 0.10 = 0.85 (maximum)

The confidence cap at 0.85 (not 1.0) reflects that common funding alone does not prove wash trading or Sybil activity — it is a necessary but not sufficient condition. Detectors that consume cluster membership combine this confidence with their own signal confidence via the scoring crate.

**Type note:** Confidence is stored as `DOUBLE PRECISION` in `wallet_clusters`. This is one of the legitimate uses of `f64` in this codebase (probability/ratio, not monetary amount) — consistent with `anomaly_events.confidence` column type.

---

## 12. Testability

Testability follows the `fetch_rows` / `compute` split established in `docs/designs/0003-detector-trait.md §mock.rs`:

1. **Algorithm tests (unit, no DB):** The common-funder bucketing logic (§4.1 steps 2-5 + confidence formula) is a pure function operating on `Vec<WalletEdge>` inputs. Tests in `tests/common_funder_test.rs` construct synthetic edge lists and assert cluster outputs without any DB.

2. **Edge aggregation tests (unit, no DB):** The UPSERT accumulation logic is a pure transformation from `Vec<UpsertEdge>` to `Vec<WalletEdge>`. Tests in `tests/edges_aggregation_test.rs` verify lamport accumulation, first/last time preservation, and tx_count increment.

3. **Storage integration tests (Postgres, Docker):** `PgClusterStore` implements `ClusterStore`. Integration tests use a real Postgres container (testcontainers or Docker Compose). These tests run separately from unit tests and are gated by `#[cfg(feature = "integration")]` or an environment variable.

4. **MockClusterStore (in mock.rs, cfg(test) only):** `MockClusterStore` implements `ClusterStore` with a `BTreeMap<(String, String), ClusterRef>` as the backing store. Detectors can construct a `MockClusterStore` with pre-populated cluster data for their unit tests, matching the pattern established by `MockPgRunner` in the detectors crate.

```rust
// mock.rs — simplified sketch
#[cfg(test)]
pub struct MockClusterStore {
    // wallet → ClusterRef mapping
    pub memberships: std::collections::BTreeMap<(String, String), Vec<ClusterRef>>,
    pub member_lists: std::collections::BTreeMap<uuid::Uuid, Vec<String>>,
}

#[cfg(test)]
impl ClusterStore for MockClusterStore {
    async fn wallet_cluster(&self, chain: &str, wallet: &str)
        -> Result<Option<ClusterRef>, GraphError>
    {
        Ok(self.memberships.get(&(chain.to_owned(), wallet.to_owned()))
                           .and_then(|v| v.first().cloned()))
    }
    // ... other methods
}
```

**Test fixtures:** Two fixture types are needed before D05/D04 integration:
- `tests/fixtures/graph/POS_01_common_funder_3wallets.json`: synthetic transfer sequence where funder F funds wallets A, B, C within 1 hour with similar amounts. Expected output: one cluster of 3 members, confidence ~0.60.
- `tests/fixtures/graph/NEG_01_independent_funding.json`: three wallets each funded by different funders with random timing. Expected output: no cluster.

---

## 13. Developer Acceptance Checklist

The following must be complete before P6-3 is marked done:

- [ ] `docs/designs/0013-graph.md` reviewed and merged (this document).
- [ ] `migrations/postgres/V00009__wallet_graph.sql` applied cleanly against a fresh Postgres instance: `sqlx migrate run` exits 0.
- [ ] `cargo check -p mg-onchain-graph` passes with no errors.
- [ ] `cargo clippy -p mg-onchain-graph --all-targets -- -D warnings` passes clean.
- [ ] `cargo test -p mg-onchain-graph` passes: unit tests for common-funder algorithm and edge aggregation.
- [ ] `GraphConfig` deserialization from `config/detectors.toml` (or `config/graph.toml`) succeeds when all required fields are present; fails with a clear error when a required field is absent.
- [ ] `ClusterStore` trait is object-safe (verify: `let _: Box<dyn ClusterStore>` compiles).
- [ ] `MockClusterStore` implements `ClusterStore` and passes unit tests without any Postgres connection.
- [ ] `PgClusterStore::wallet_cluster` query uses the `idx_wallet_cluster_members_wallet` index (verify with `EXPLAIN`).
- [ ] `GraphIndexer::index_transfers()` upsert is idempotent: running it twice on the same input produces the same `wallet_edges` state.
- [ ] `common_funder_test.rs` includes at least one positive fixture (3+ wallets, same funder, same window) and one negative fixture (independent funders). Both pass deterministically.
- [ ] `REFERENCES.md` updated with Liu et al. (2025) + Chainalysis (2025) + Messias et al. (2023) entries under the `graph` detector group.
- [ ] `config/detectors.toml` (or `config/graph.toml`) has a `[graph]` section with all six thresholds in `{ value, rationale, refs }` shape.
- [ ] No `HashMap` in any path from `GraphIndexer` or `ClusterDetector` that contributes to `wallet_edges` or `wallet_clusters` output. Use `BTreeMap` for any intermediate key-value aggregation.
- [ ] No `f64` used for lamport amounts. Lamports are `u128`; confidence is `f64` (acceptable for probability).

---

## 14. Open Questions (≤5)

**OQ1 — Native SOL transfer representation in `transfers` table:**
The `transfers` table stores SPL Token transfers (ERC-20 style). How does the existing Yellowstone gRPC adapter represent native SOL transfers between EOAs? Options: (a) stored with `token = '11111111111111111111111111111111'` (System Program), (b) stored in a separate table, (c) not stored at all. If (b) or (c), `GraphIndexer` needs a different data source. Resolve by inspecting `crates/chain-adapter/src/solana/` before implementation.

**OQ2 — First-funding constraint feasibility:**
The MVP drops the strict "no prior funder" constraint in favor of "F's transfer preceded first swap from A." This is weaker. Legitimate CEX-to-wallet distributions (many users funded by a Binance hot wallet) will still pass the filter and produce false-positive clusters. The `min_funder_sol_amount` dust filter helps but does not eliminate CEX funders. Should a known-CEX-wallet exclusion list be applied at the `wallet_edges` ingestion stage (similar to `token_status::KNOWN_PROTOCOL_MINTS`)? Decision needed before implementation.

**OQ3 — `cluster_kind` CHECK constraint vs application enforcement:**
The V00009 migration adds a CHECK constraint listing allowed `cluster_kind` values. Adding `synchronized_activity` or `bytecode_similar` in Phase 3 Sprint 8-9 will require a migration to update the CHECK constraint. Is this acceptable, or should the constraint be dropped and enforcement moved to application code? The design recommends keeping CHECK for data integrity; document as a known migration cost.

**OQ4 — `DetectorContext` extension for graph:**
The D05/D04 integration hooks (§9) require adding `cluster_store: Option<&'ctx dyn ClusterStore>` to `DetectorContext`. This is an additive, backward-compatible change (existing detectors ignore `None`). However, it adds a dependency from `crates/detectors` to `crates/graph` (via the `ClusterStore` trait). Alternative: define `ClusterStore` in a thin `crates/graph-api` crate that both `graph` and `detectors` depend on, avoiding a direct `detectors → graph` dependency. Resolve before D05 integration sprint begins.

**OQ5 — Cluster identity across re-computation runs:**
When `ClusterDetector::run_common_funder()` re-runs (every `cluster_ttl_hours`), it may produce clusters that are semantically the same as existing ones (same funder, same members) but with slightly different confidence due to new edges. The ON CONFLICT DO UPDATE clause updates `confidence` and `computed_at`. However, if a cluster gains or loses members between runs, it should be a new `cluster_id` — the old one is logically superseded. The current design does not specify a cluster identity key beyond the `(root_funder, time_bucket, amount_bucket)` triple. Resolve: define a deterministic `cluster_id` derivation (e.g. `uuid5(namespace, "funder={F}|window={W}|bucket={B}")`) so the same logical cluster always maps to the same UUID across re-computation runs.

---

## 15. Design Gaps / Future Work

### 15.1 Synchronized-activity clustering (Phase 3 Sprint 8)

Two wallets are in the same `synchronized_activity` cluster if their first-ever transaction timestamps (measured in Solana slots) differ by ≤ `max_timing_spread_slots` AND they both received tokens from the same airdrop distribution contract within one slot. This signal requires:
- `first_tx_time` per wallet (available from `wallet_edges.first_tx_time` for funded wallets, or requires a separate `wallet_first_tx` materialized view)
- Airdrop distribution contract identification (no schema exists yet)

The `wallet_clusters` table already has `cluster_kind = 'synchronized_activity'` in its CHECK constraint, reserving space. No algorithm specification in this design.

### 15.2 Bytecode-similarity clustering (Phase 3 Sprint 9, EVM only)

EVM scam token factories deploy near-identical contract bytecode for each token. Two contracts with >95% bytecode similarity (measured by edit distance on the decompiled EVM bytecode) are likely from the same factory. This requires:
- EVM chain support (Phase 4)
- Contract bytecode storage (not in current schema)
- A bytecode diff/similarity library

Out of Phase 3 MVP scope. The `cluster_kind = 'bytecode_similar'` value is reserved in the schema.

### 15.3 Cross-chain clustering (Phase 4+)

The current schema includes `chain` as a first-class column in all three tables, but all queries and the algorithm are intra-chain. Cross-chain clustering (a wallet on Solana and the "same" wallet on Ethereum, linked via a bridge transfer) requires bridge event detection and cross-chain identity resolution. This is a Phase 4+ research problem. The schema accommodates it without change: `wallet_cluster_members` rows from different chains can share a `cluster_id` if the application decides to merge them.

### 15.4 Smart-money labeling (Phase 3+)

`research/02-detection-methodology.md §7` describes smart-money tracking: wallets with top-decile P&L rank, ≥5 closed positions, ≥60% win rate over 90 days. This is a derived label from swap history, not a funding-graph signal. It belongs in a separate `wallet_labels` table alongside `wallet_clusters`. The `crates/graph` API surface could be extended to include `is_smart_money(wallet, chain) -> Option<SmartMoneyLabel>` in Phase 3 Sprint 9-10, sharing the `PgClusterStore` infrastructure.

### 15.5 Graph algorithm library selection

The research methodology (`research/02-detection-methodology.md §Cross-cutting B`) lists `petgraph` for connected components / BFS. The MVP common-funder algorithm is implemented entirely in SQL and does not need an in-memory graph library. Phase 3 Sprint 8 (synchronized-activity) may require Union-Find for connected component labeling that is too complex to express in SQL. At that point, evaluate `petgraph` vs a simple Union-Find implementation in `edges.rs`. No Rust graph library dependency is added in this design.
