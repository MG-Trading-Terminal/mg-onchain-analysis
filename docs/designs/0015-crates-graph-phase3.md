# Design 0015 — `crates/graph` Phase 3 Foundation (Sprint 11)

**Date:** 2026-04-24
**Status:** Draft
**Author:** architect agent
**Sprint:** 11
**ADR refs:**
- ADR 0001 §D5 — Sybil/bundled-launch deferred to Phase 3 (graph prerequisite)
- ADR 0002 — Postgres-only storage; NUMERIC(39,0) for u128; monthly partitions
- ADR 0003 — self-sovereign infrastructure; no 3rd-party runtime dependencies
**Related designs:**
- `docs/designs/0013-graph.md` — original graph design; crate already shipped (Sprint 6/7)
- `docs/designs/0003-detector-trait.md` — Detector trait + DetectorContext
- `docs/designs/0014-streaming-detector.md` — streaming scheduler (D08 slots in here)
- `docs/designs/0002-storage-schemas-v1.md` — existing schema; V00009 covers wallet_edges

**Binding prior art in REFERENCES.md:**
- Liu et al. 2025 (arxiv:2505.09313) — Sybil detection, subgraph features — D08
- Messias, Yaish & Livshits 2023 (arxiv:2312.02752) — airdrop farming, common-funder — D08
- Chainalysis 2025 — wash trading, Heuristic 2 common-funder — D05 Signal B upgrade
- Tarjan 1972 — SCC for cycle detection — Sprint 12 T2-2
- Adams & MacKay 2007 (online BOCPD) — deployer changepoint — Sprint 12 T2-1

---

## §1 Purpose

`crates/graph` was designed in Sprint 6 (design 0013) and its MVP layer shipped in Sprint 7:
`GraphIndexer` populates `wallet_edges` (native SOL funding transfers), `ClusterDetector`
runs common-funder clustering into `wallet_clusters` + `wallet_cluster_members`, and
`PgClusterStore` exposes the read API consumed by detectors. The V00009 migration landed all
three backing tables.

What is missing before Phase 3 detectors can fire:

1. **Address labels** — graph-specific node annotations (DeployerEOA, KnownDex, Sybil, …)
   that live in a distinct table from `holder_classifications`. The latter is holder-centric
   (per-token holder role: vesting, CEX, dex_pool). Address labels are graph-global: they
   annotate a wallet address across all tokens and chains, carry confidence + TTL, and are
   written by clustering algorithms and read by detectors.

2. **Typed graph edges beyond SOL-funding** — `wallet_edges` covers only one edge type
   (native SOL transfers, i.e. the funding graph). The feature-gap analysis (research
   doc `03-feature-gap-2026-04-24.md`, T2-2) calls for cycle detection over
   `TokenTransfer` edges and the latent-flux production system uses deployer-network graphs
   that require `DeployerOf` and `AuthorityOf` edges. These are structurally different from
   funding edges: they are sparse (one `DeployerOf` edge per token, one `AuthorityOf` per
   token × authority type), do not aggregate over multiple transfers, and are token-specific.
   A unified `graph_edges` table with an `edge_type` discriminator can hold all three
   additional types without schema proliferation.

3. **D08 Sybil detector** — the Phase 3 detector that consumes common-funder cluster
   membership, holder-overlap data, and address labels to emit Sybil anomaly events.
   Liu et al. 2025 (already in REFERENCES.md) validates the subgraph feature approach with
   >0.90 precision/recall. D08 is a cadenced streaming detector (not per-event), triggered
   on new-token appearance in the streaming registry.

Together, items 1–3 close the gap between "graph infrastructure ships" (done) and "graph
actually feeds detector output" (this sprint). The two externally-validated Tier 2 algorithms
from the gap analysis — BOCPD deployer changepoint (T2-1) and Tarjan SCC wash-ring
detection (T2-2) — both depend on the data foundation laid here. This design is therefore the
prerequisite for Sprint 12 T2-1 and T2-2.

---

## §2 Scope Boundary

### Sprint 11 (this design)

- Postgres migration **V00011**: `address_labels` table + `graph_edges` table with
  `DeployerOf` and `AuthorityOf` edge types. (`wallet_edges` from V00009 is unchanged;
  it covers the Funding edge type already. TokenTransfer edges are additive in V00011.)
- `GraphStore` trait extension: `insert_label`, `get_labels`, `insert_edge`, `get_neighbors`
  methods. These extend the existing `ClusterStore` trait surface; `PgClusterStore` gains
  the new methods. No new crate — additions to existing `crates/graph` files.
- Indexer writer: when the indexer observes a `PoolEvent::Initialize` or
  `Transfer` with `is_mint=true` (token creation context), write `DeployerOf` and
  `AuthorityOf` edges. Hook into the existing `PgEventSink` write path.
- **D08 Sybil detector** (`crates/detectors/src/d08_sybil.rs`): cadenced detector consuming
  common-funder cluster membership + holder overlap. Slots into the streaming scheduler as a
  cadenced detector (same pattern as D01). Config in `config/detectors.toml` under
  `[sybil_detection]`. Evidence keys `sybil_detection/cluster_id`,
  `sybil_detection/cluster_size`, `sybil_detection/top_holder_overlap_pct`.
- `config/detectors.toml` additions: `[sybil_detection]` section with
  `sybil_cluster_top_holder_pct_threshold` and `sybil_cluster_min_size`.
- Positive + negative fixture entries for D08 in `tests/fixtures/solana/`.
- REFERENCES.md entry for D08 citing Liu et al. 2025 + Chainalysis 2025.

### Sprint 12+ (deferred, not designed here)

- Synchronized-activity clustering (same-slot first-tx timing). Schema hook: `cluster_kind`
  CHECK constraint in `wallet_clusters` already includes `'synchronized_activity'`.
- Bytecode-similarity clustering (EVM only, Phase 4).
- Smart-money labelling (historical P&L cohort compute).
- **T2-1: Bayesian changepoint detection on deployer behavior** (Adams & MacKay 2007).
  Requires per-deployer time-series; `address_labels` + `graph_edges` are the data source.
- **T2-2: Tarjan SCC + Johnson cycle enumeration** for wash-ring detection.
  Consumes `graph_edges` rows with `edge_type = 'TokenTransfer'`. O(V+E) Tarjan SCC
  is a pure-Rust algorithm; the petgraph crate is the reference implementation.
- Vesting-unlock calendar signal (on-chain Jup Lock parsing).
- D05 Signal B upgrade to graph-backed confirmation (cluster membership replaces
  cluster-flow-balance proxy). Requires Sprint 12 integration hook.
- D04 insider cluster upgrade (graph-derived insider set replaces deployer_clusters stub).
- `token_risk_reports` migration (V00011 candidate per SESSION-KICKOFF gotcha #31) —
  this migration is now blocked from being V00011 by the graph tables claiming that number.
  The developer must use V00012 for `token_risk_reports`.

### Explicitly out of scope for this project

- Cross-chain graphs (Phase 4 EVM). The schema is chain-tagged; EVM chains plug in without
  migration changes.
- Non-wallet contract-level bytecode clustering (EVM only, Phase 4 dependency).

---

## §3 Data Model

### §3.1 Existing tables (unchanged)

V00009 tables are **not modified** by this design. They are summarised for context:

| Table | Purpose | Key columns |
|-------|---------|------------|
| `wallet_edges` | Aggregate SOL funding edges | `(chain, from_wallet, to_wallet)` PK; `total_sol_lamports NUMERIC(39,0)`, `first_tx_time`, `last_tx_time` |
| `wallet_clusters` | Derived clusters | `cluster_id UUID` PK; `cluster_kind TEXT` CHECK; `root_funder TEXT`; `confidence DOUBLE PRECISION` |
| `wallet_cluster_members` | Cluster ↔ wallet membership | `(cluster_id, wallet)` PK; FK to `wallet_clusters` |

V00003 `holder_classifications` is also unchanged. Its `kind` values
(`burn_address`, `dex_pool`, `vesting_contract`, `cex_hot_wallet`, `liquid`) are
holder-role annotations, not graph-global labels. The two tables serve orthogonal purposes
and should not be merged.

### §3.2 New table: `address_labels`

One row per `(chain, address, label_type)`. Multiple label types can apply to the same
address (e.g. an address can be both `DeployerEOA` and `Sybil`).

```sql
CREATE TABLE IF NOT EXISTS address_labels (
    -- Identity
    chain           TEXT            NOT NULL,
    address         TEXT            NOT NULL,
    label_type      TEXT            NOT NULL,
        -- Enumerated below in §3.2.1

    -- Confidence in [0.0, 1.0]. DOUBLE PRECISION (probability, not money).
    confidence      DOUBLE PRECISION NOT NULL
                        CHECK (confidence >= 0.0 AND confidence <= 1.0),

    -- JSONB evidence: algorithm parameters, supporting tx hashes, cluster_id if
    -- this label was derived from a clustering run, etc.
    evidence        JSONB           NOT NULL DEFAULT '{}'::jsonb,

    -- When this label was assigned. NOT wall-clock in streaming path (see §3.2.2).
    issued_at       TIMESTAMPTZ     NOT NULL,

    -- Optional TTL. NULL = permanent. Sybil labels expire; KnownExchange is permanent.
    expires_at      TIMESTAMPTZ,

    -- Who created this label: 'common_funder_clustering', 'd08_sybil',
    -- 'manual', 'token_registry', etc.
    source          TEXT            NOT NULL,

    -- Housekeeping
    updated_at      TIMESTAMPTZ     NOT NULL DEFAULT now(),

    PRIMARY KEY (chain, address, label_type)
);
```

No partitioning. `address_labels` is bounded (one row per address × label type per chain).
At MVP scale (100s of tracked tokens, thousands of wallets), this table stays well under 1M
rows. If it grows beyond 10M rows (Phase 4 multi-chain with smart-money labelling), add a
partial index on `(chain, label_type, expires_at)` and a background eviction job.

#### §3.2.1 `label_type` enumeration

The `label_type` column is `TEXT` with application-level enforcement (not a Postgres CHECK
constraint). This avoids a migration for every new label type in future sprints.

Valid values for Sprint 11:

| label_type | Semantics | Written by | TTL |
|-----------|-----------|-----------|-----|
| `DeployerEOA` | Address that deployed at least one token contract on this chain | Indexer writer (on `PoolEvent::Initialize`) | NULL (permanent) |
| `FundingSource` | Address that funded ≥3 wallets within a common-funder cluster | ClusterDetector run | 168h (matches `cluster_ttl_hours`) |
| `KnownDex` | Known DEX program or router address | Static seed (token-registry) | NULL |
| `KnownBurn` | Known burn address (Solana: `11111...1111` null key) | Static seed | NULL |
| `KnownExchange` | Known CEX hot wallet | Static seed | NULL |
| `SmartMoney` | Address with historical P&L above threshold | Deferred Sprint 12 | 720h |
| `Sybil` | Confirmed Sybil address from D08 evaluation | D08 at emit time | 168h |

Sprint 11 writes: `DeployerEOA` (indexer), `FundingSource` (ClusterDetector), `Sybil` (D08).
Sprint 12+ writes: `SmartMoney`. Static seeds (`KnownDex`, `KnownBurn`, `KnownExchange`) are
seeded from `token-registry/data/*.json` files via a one-time migration helper, not runtime writes.

#### §3.2.2 Time source discipline

`issued_at` in the indexer write path MUST be derived from `block_time`, not `now()`.
Detectors MUST use `ctx.observed_at` (from `DetectorContext`), which the scheduler derives
from `block_time` (gotcha #28). The `source = 'd08_sybil'` label written by D08 uses
`ctx.observed_at` as `issued_at`. Only background jobs (ClusterDetector, static-seed loader)
may use `now()` for `issued_at`.

#### §3.2.3 Indexes on `address_labels`

```sql
-- Primary lookup: "what labels does this address have?"
CREATE INDEX IF NOT EXISTS idx_address_labels_addr
    ON address_labels (chain, address);

-- Label-type scan: "all Sybil addresses on this chain"
CREATE INDEX IF NOT EXISTS idx_address_labels_type
    ON address_labels (chain, label_type);

-- TTL eviction: "which labels have expired?"
CREATE INDEX IF NOT EXISTS idx_address_labels_expires
    ON address_labels (expires_at)
    WHERE expires_at IS NOT NULL;
```

No BRIN — this table is not partitioned, and address lookups are random-access (B-tree
outperforms BRIN for random-access patterns).

### §3.3 New table: `graph_edges`

Typed directed edges not covered by `wallet_edges`. The key design question is
**single table vs per-type tables**.

**Decision: single `graph_edges` table with `edge_type` discriminator.**

Rationale:

- Query patterns for Sprint 11 are narrow: `DeployerOf` edges are read once at token
  ingestion (to populate `tokens.creator`); `AuthorityOf` edges are read at D08 evaluation
  (to confirm the deployer is also the mint authority). Neither query pattern benefits from
  a dedicated table that avoids the discriminator filter.
- T2-2 (Tarjan SCC) will query `TokenTransfer` edges and does not need to join with
  `DeployerOf`. A `WHERE edge_type = 'TokenTransfer'` filter on a B-tree index has the same
  cost as a dedicated table with the same row count.
- Fewer tables reduces migration count and schema complexity at current scale.
- The tradeoff reverses if `TokenTransfer` row count grows to 100M+ (Phase 4 EVM with
  un-filtered token transfer indexing). At that point, a separate `token_transfer_edges`
  table partitioned by `block_time` is the escape hatch. This is documented as a
  TimescaleDB-class scale trigger, identical to the ADR 0002 escape hatch for event tables.

```sql
CREATE TABLE IF NOT EXISTS graph_edges (
    -- Edge identity
    chain           TEXT            NOT NULL,
    from_address    TEXT            NOT NULL,
    to_address      TEXT            NOT NULL,
    edge_type       TEXT            NOT NULL,
        -- 'Funding'        — legacy alias; prefer wallet_edges for SOL funding
        --                    (this type is reserved but wallet_edges remains primary)
        -- 'TokenTransfer'  — SPL token transfer (from transfers table projection)
        -- 'DeployerOf'     — deployer EOA → token mint address
        -- 'AuthorityOf'    — mint_authority or freeze_authority → token mint address

    -- Token context (NULL for Funding type; set for token-specific edge types)
    token           TEXT,

    -- Raw amount in token's native unit (NUMERIC(39,0) via String bridge).
    -- NULL for DeployerOf / AuthorityOf (no amount semantics).
    amount_raw      NUMERIC(39,0),

    -- Block context (must come from block_time, not wall-clock — gotcha #28)
    block_time      TIMESTAMPTZ     NOT NULL,
    block_height    BIGINT          NOT NULL,

    -- Transaction hash for traceability. NULL for derived edges (AuthorityOf)
    -- that are inferred from token metadata, not from an observed transaction.
    tx_hash         TEXT,

    -- Chain identifier for forward-compat (same value as `chain` column)
    -- Retained explicitly so cross-chain join queries can filter without
    -- scanning the chain TEXT column (minor: avoids collation overhead).
    -- Note: this is redundant with `chain`; kept for query readability.

    -- Housekeeping
    updated_at      TIMESTAMPTZ     NOT NULL DEFAULT now(),

    -- Dedup: one row per (chain, from, to, edge_type, token, block_height).
    -- block_time NOT included in PK because graph_edges is NOT partitioned at MVP scale.
    -- At scale trigger (>50M rows or T2-2 TokenTransfer volume), partition by
    -- block_time and include it in the unique constraint (gotcha #7 compliance).
    PRIMARY KEY (chain, from_address, to_address, edge_type, token, block_height)
);
```

**Note on gotcha #7 (partition key in unique constraints):** `graph_edges` is NOT
partitioned at Sprint 11 scale. The PRIMARY KEY does not include `block_time` because
partitioning is not applied. If `graph_edges` is partitioned in a future migration, the
developer MUST add `block_time` to the PRIMARY KEY at that time (following the pattern in
V00002 `transfers_dedup_key`). This is explicitly flagged in the migration comment.

**Note on reorg handling (gotcha #6):** Reorgs are handled by the indexer's existing
`SLOT_DEAD` marker. The graph indexer should respond to a reorg signal by
`DELETE FROM graph_edges WHERE chain = $1 AND block_height >= $reorg_height`. This is
the same pattern used by the event tables. The developer must hook into the existing
reorg callback in `crates/indexer`.

#### §3.3.1 Indexes on `graph_edges`

```sql
-- Forward lookup: "all tokens deployed by this address"
CREATE INDEX IF NOT EXISTS idx_graph_edges_from_type
    ON graph_edges (chain, from_address, edge_type);

-- Reverse lookup: "who is the deployer/authority of this token?"
CREATE INDEX IF NOT EXISTS idx_graph_edges_to_type
    ON graph_edges (chain, to_address, edge_type);

-- Token-centric: "all edges for token X" (D08 reads this)
CREATE INDEX IF NOT EXISTS idx_graph_edges_token
    ON graph_edges (chain, token, edge_type)
    WHERE token IS NOT NULL;

-- Block-height range scan: reorg DELETE and T2-2 time-windowed cycle detection
CREATE INDEX IF NOT EXISTS idx_graph_edges_block_height
    ON graph_edges (chain, block_height DESC);
```

BRIN on `block_time` is appropriate IF `graph_edges` is partitioned in a future migration.
At Sprint 11 scale (unpartitioned), BRIN adds less value than B-tree; omit for now.

#### §3.3.2 Edge type write triggers

| Edge type | Write trigger | Source event | `amount_raw` | `tx_hash` |
|-----------|-------------|--------------|-------------|----------|
| `DeployerOf` | `PoolEvent::Initialize` — first pool creation for this token | `tokens.creator` field + pool init block | NULL | Pool init tx hash |
| `AuthorityOf` | Token metadata upsert (`tokens` table INSERT) — when `mint_authority` is non-NULL | `tokens.mint_authority` + `tokens.freeze_authority` fields | NULL | NULL (metadata inference) |
| `TokenTransfer` | `Transfer` event ingestion (SPL token transfers only, not SOL) | `transfers` table rows; batch projection | `amount_raw` from `transfers` | `transfers.tx_hash` |

`TokenTransfer` edges are written lazily — only for tokens that appear in the streaming
registry (`StreamingRegistry`) or for backfill runs. At MVP scale (100–1000 tracked tokens)
this is bounded. Writing `TokenTransfer` for every SPL transfer on the chain would exceed
the `graph_edges` table's non-partitioned capacity quickly; that is the scale trigger for
partitioning (see §3.3 note).

**Sprint 11 scope:** only `DeployerOf` and `AuthorityOf` writes land in S11-4 (indexer
writer). `TokenTransfer` edge projection is deferred to Sprint 12 T2-2 (needed by Tarjan
SCC). The `graph_edges` table schema supports it from day one.

---

## §4 Public API Changes to `crates/graph`

The existing public surface of `crates/graph` is:

```
pub use api::{ClusterKind, ClusterRef, ClusterStore, PgClusterStore};
pub use clusters::{bucket_edges, compute_confidence, derive_cluster_id, CandidateCluster, ClusterDetector, ClusterStats, FundingEdge};
pub use config::{load_graph_config, GraphConfig, Threshold};
pub use edges::{aggregate_edges, GraphIndexer, IndexStats, UpsertEdge, WalletEdge, SYSTEM_PROGRAM_ADDRESS};
pub use error::GraphError;
```

All of this is **unchanged**. Sprint 11 adds the following to `lib.rs` re-exports:

```
pub use labels::{AddressLabel, LabelType, GraphLabelStore, PgGraphLabelStore};
pub use typed_edges::{GraphEdge, EdgeType, TypedEdgeStore, PgTypedEdgeStore};
```

### §4.1 New module `labels.rs`

```
// Rust pseudo-code (design level — developer fills in sqlx calls)

/// Enumeration of graph-global label types.
/// Serialises as snake_case for TOML config + JSON evidence.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LabelType {
    DeployerEoa,
    FundingSource,
    KnownDex,
    KnownBurn,
    KnownExchange,
    SmartMoney,   // Sprint 12
    Sybil,
}

impl LabelType {
    pub fn as_db_str(&self) -> &'static str { ... }
    pub fn from_db_str(s: &str) -> Option<Self> { ... }
}

/// One row from `address_labels`.
#[derive(Debug, Clone)]
pub struct AddressLabel {
    pub chain: String,
    pub address: String,
    pub label_type: LabelType,
    pub confidence: f64,         // probability — f64 is correct here
    pub evidence: serde_json::Value,
    pub issued_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub source: String,
}

/// Read/write API for address_labels.
/// Uses async_trait for dyn-compatibility (same pattern as ClusterStore).
#[async_trait]
pub trait GraphLabelStore: Send + Sync {
    /// Insert or update a label. ON CONFLICT (chain, address, label_type)
    /// DO UPDATE only when confidence >= existing or expires_at < now().
    async fn upsert_label(&self, label: &AddressLabel) -> Result<(), GraphError>;

    /// Batch upsert — use INSERT ... ON CONFLICT for efficiency.
    async fn upsert_labels(&self, labels: &[AddressLabel]) -> Result<(), GraphError>;

    /// All current (non-expired) labels for this address.
    async fn get_labels(
        &self,
        chain: &str,
        address: &str,
    ) -> Result<Vec<AddressLabel>, GraphError>;

    /// All addresses with a given label type on this chain.
    /// Used by D08 to fetch all current Sybil-labelled addresses.
    async fn addresses_with_label(
        &self,
        chain: &str,
        label_type: LabelType,
        min_confidence: f64,
    ) -> Result<Vec<AddressLabel>, GraphError>;
}

pub struct PgGraphLabelStore { pub pool: sqlx::PgPool }

#[async_trait]
impl GraphLabelStore for PgGraphLabelStore { ... }
```

### §4.2 New module `typed_edges.rs`

```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EdgeType {
    Funding,        // reserved; wallet_edges is the primary store for Funding
    TokenTransfer,
    DeployerOf,
    AuthorityOf,
}

/// One row from `graph_edges`.
#[derive(Debug, Clone)]
pub struct GraphEdge {
    pub chain: String,
    pub from_address: String,
    pub to_address: String,
    pub edge_type: EdgeType,
    pub token: Option<String>,
    pub amount_raw: Option<u128>,
    pub block_time: DateTime<Utc>,
    pub block_height: u64,
    pub tx_hash: Option<String>,
}

#[async_trait]
pub trait TypedEdgeStore: Send + Sync {
    /// Insert an edge. ON CONFLICT DO NOTHING (idempotent).
    async fn insert_edge(&self, edge: &GraphEdge) -> Result<(), GraphError>;

    /// Batch insert — single multi-row INSERT ... ON CONFLICT DO NOTHING.
    async fn insert_edges(&self, edges: &[GraphEdge]) -> Result<(), GraphError>;

    /// Outgoing neighbors of an address, filtered by edge type.
    /// `limit` is mandatory; callers must not pass unbounded queries.
    async fn get_neighbors(
        &self,
        chain: &str,
        from_address: &str,
        edge_type: EdgeType,
        limit: u32,
    ) -> Result<Vec<GraphEdge>, GraphError>;

    /// Incoming neighbors (reverse lookup).
    async fn get_predecessors(
        &self,
        chain: &str,
        to_address: &str,
        edge_type: EdgeType,
        limit: u32,
    ) -> Result<Vec<GraphEdge>, GraphError>;

    /// All edges for a token (used by D08 and T2-2).
    async fn token_edges(
        &self,
        chain: &str,
        token: &str,
        edge_type: EdgeType,
    ) -> Result<Vec<GraphEdge>, GraphError>;
}

pub struct PgTypedEdgeStore { pub pool: sqlx::PgPool }

#[async_trait]
impl TypedEdgeStore for PgTypedEdgeStore { ... }
```

### §4.3 `ClusterDetector` extension

`ClusterDetector::run_common_funder` already writes cluster rows. Sprint 11 extends it to
**also write `FundingSource` labels** for each `root_funder` that produced a cluster:

```
// After writing the cluster row, also upsert a FundingSource label:
label_store.upsert_label(&AddressLabel {
    chain: chain.to_owned(),
    address: cluster.funder.clone(),
    label_type: LabelType::FundingSource,
    confidence: confidence,
    evidence: json!({ "cluster_id": cluster_id, "member_count": member_count }),
    issued_at: now,  // ClusterDetector is a background job; now() is correct here
    expires_at: Some(now + cluster_ttl_duration),
    source: "common_funder_clustering".into(),
}).await?;
```

`ClusterDetector` gains a `label_store: &dyn GraphLabelStore` parameter to `run_common_funder`.

### §4.4 `GraphError` additions

Add two variants:

```rust
/// An address string could not be parsed for a graph operation.
#[error("invalid address for graph op: {0}")]
InvalidAddress(String),

/// A label_type string was not recognised.
#[error("unknown label_type: {0}")]
UnknownLabelType(String),
```

---

## §5 Integration

### §5.1 Indexer write path — `DeployerOf` and `AuthorityOf` edges

The indexer's `PgEventSink` (or equivalent sink in `crates/server`) writes events to
Postgres. Sprint 11 adds a graph write hook triggered on two event classes:

**On `PoolEvent::Initialize`:**

```
// When a new pool is created:
// 1. from_address = tokens.creator (the wallet that signed the Initialize instruction)
// 2. to_address = tokens.mint (the token mint address)
// 3. edge_type = DeployerOf
// 4. block_time / block_height from the Initialize event
// 5. tx_hash = the Initialize transaction signature
typed_edge_store.insert_edge(&GraphEdge {
    chain: chain.to_string(),
    from_address: creator_address,
    to_address: token_mint,
    edge_type: EdgeType::DeployerOf,
    token: Some(token_mint.clone()),
    amount_raw: None,
    block_time: event.block_time,     // from block_time, NOT Utc::now() — gotcha #28
    block_height: event.block_height,
    tx_hash: Some(event.tx_hash.clone()),
}).await?;
```

Also write a `DeployerEOA` label for the creator:

```
label_store.upsert_label(&AddressLabel {
    chain: ...,
    address: creator_address,
    label_type: LabelType::DeployerEoa,
    confidence: 1.0,    // direct observation from on-chain Initialize instruction
    evidence: json!({ "token": token_mint, "tx_hash": event.tx_hash }),
    issued_at: event.block_time,    // block_time — not wall-clock
    expires_at: None,               // permanent
    source: "indexer_pool_initialize".into(),
}).await?;
```

**On token metadata upsert (when `mint_authority` is non-NULL):**

```
// When tokens table is upserted with a non-NULL mint_authority:
typed_edge_store.insert_edge(&GraphEdge {
    chain: ...,
    from_address: mint_authority_address,
    to_address: token_mint,
    edge_type: EdgeType::AuthorityOf,
    token: Some(token_mint.clone()),
    amount_raw: None,
    block_time: event.block_time,
    block_height: event.block_height,
    tx_hash: None,  // inferred from metadata; no specific tx
}).await?;
```

Same pattern for `freeze_authority` when non-NULL. Both `mint_authority` and
`freeze_authority` can be authority of the same token — two separate `AuthorityOf` edges
with different `from_address` values. They share the same `(chain, to_address, edge_type,
token, block_height)` key only if both authorities are the same wallet (edge deduplication
is correct in that case: one row for one wallet that holds both authorities).

**Reorg semantics:** on a `SLOT_DEAD` event, the graph writer calls:
```sql
DELETE FROM graph_edges WHERE chain = $1 AND block_height >= $2;
DELETE FROM address_labels WHERE chain = $1 AND issued_at >= $block_time_at_reorg_height
    AND source IN ('indexer_pool_initialize', 'indexer_token_metadata');
```
The label delete is scoped to indexer-written sources only; clustering-derived labels
are not invalidated by a single-block reorg (they are derived from aggregates across many blocks).

### §5.2 Streaming scheduler — D08 as cadenced detector

D08 runs as a **cadenced detector** in the streaming scheduler, identical in shape to D01
(honeypot simulation). D01 is triggered every N ticks per token (default `streaming_d01_cadence_n = 10`).
D08's cadence is higher: every M ticks per chain (not per token), because D08 evaluates
cluster membership across all tokens for which a common-funder cluster has been updated
since the last D08 run.

**Scheduling model:**

D08 does NOT fire per-token like D02–D06. It fires on the cadence `streaming_d08_cadence_ticks`
(default: 50 ticks = every 50 scheduler ticks × 500ms debounce ≈ ~25 seconds) for each
chain. The scheduler worker maintains a `HashMap<Chain, u64>` tick counter for D08
(separate from the D01 per-token counter). When the D08 counter reaches
`streaming_d08_cadence_ticks`, the worker:

1. Calls `cluster_store.funder_cluster(chain, root_funder)` for all funders that have
   had `wallet_edges` updated since the last D08 run (using the `adapter_checkpoints`
   table with `adapter_id = "d08_sybil_{chain}"`).
2. For each fresh cluster, calls `D08::evaluate(ctx)` where the context carries the cluster
   and the holder snapshots for all tokens where cluster members are top holders.
3. Emits `AnomalyEvent { detector: "sybil_detection", ... }` if the signal fires.

**Why cadenced and not per-event:** Common-funder clustering is a batch algorithm that must
complete before D08 can fire. Rerunning `ClusterDetector::run_common_funder` on every tick
would be too expensive (it reads all `wallet_edges` for the chain). Instead:
- `ClusterDetector` runs on its own background schedule (`cluster_ttl_hours`, default 168h).
- D08 polls for fresh clusters using a checkpoint and evaluates them.
This decouples the expensive clustering job from the per-tick latency budget.

**D08 `observe_at` source:** The scheduler passes `observed_at =
DateTime::from_timestamp(block_time, 0)` from the most recent `InvalidationEvent` slot hint
for the chain (gotcha #22, #28). D08 must NOT call `Utc::now()`.

**`Detector` trait compliance:** D08 implements the existing `Detector` trait with
`async fn evaluate<'ctx>(&self, ctx: &'ctx DetectorContext<'ctx>) -> Result<Vec<AnomalyEvent>, DetectorError>`.
The trait signature is unchanged (gotcha #27). The cadence logic lives in the scheduler
worker, not in `D08::evaluate`. This satisfies the design 0014 §2.1 OQ1 resolution:
"The Detector trait is UNCHANGED."

**`DetectorContext` extension for D08:** D08 needs access to `&dyn ClusterStore` and
`&dyn GraphLabelStore`. These are not currently in `DetectorContext`. Two options:

Option A: Add `cluster_store: &'ctx dyn ClusterStore` and
`label_store: &'ctx dyn GraphLabelStore` as optional fields to `DetectorContext`.

Option B: D08 stores references to these stores as struct fields
(D08 is constructed with them at service startup) and does not receive them via context.

**Decision: Option B.** `DetectorContext` is in `crates/detectors` which is frozen in
structure (not definition — new fields can be added, but each addition requires checking
all 8 existing detector impls for breakage). The existing detectors do not use graph data;
adding two new required fields to `DetectorContext` would compile-break all of them if
they do not pass the new fields. The cleanest approach is for D08 to hold its own
`Arc<dyn ClusterStore>` and `Arc<dyn GraphLabelStore>` constructed at server startup
alongside the other detectors. This is the same pattern D01 uses for `Arc<dyn PoolAccountProvider>`.

If a future design moves graph stores into `DetectorContext`, that is a separate refactor.
For Sprint 11, D08 is self-contained.

### §5.3 `token_risk_reports` note (gotcha #31 + sprint plan)

The SESSION-KICKOFF flagged `token_risk_reports` as the next migration candidate
(V00011 in Sprint 10). This design now occupies V00011 with graph tables. The developer
MUST use **V00012** for `token_risk_reports`. The design 0014 §2.5 reference to
"V00011 migration" is superseded; update the SESSION-KICKOFF and CHANGELOG accordingly.

---

## §6 D08 Sybil Detector — Signal Design

### §6.1 Prior art

| Source | Mechanism | Used in |
|--------|----------|---------|
| Liu et al. 2025 (arxiv:2505.09313) | Subgraph features: common funding source + synchronized timing → LightGBM classifier; AUC > 0.90 on 23,240 labeled Sybil addresses | D08 Signal A + B feature basis |
| Messias, Yaish & Livshits 2023 (arxiv:2312.02752) | Airdrop farming tactics: Sybil clusters identified by common-funder + synchronized-deposit pattern | D08 Signal A confirmation |
| Chainalysis 2025 (wash trading report) | Common-funder Heuristic 2: controller funds ≥5 addresses; confirmed for $1.87B wash volume | D08 confidence threshold anchor |

Both citations are already in REFERENCES.md. No new REFERENCES.md entries are required
for D08 Sprint 11 scope.

### §6.2 Signal definitions

**Signal A — Sybil cluster top-holder overlap:**

```
sybil_cluster_member_count = count of wallets in cluster c
token_holders_in_cluster   = count of cluster members that appear in holder_snapshots
                             for token t with is_liquid = true
top_holder_overlap_pct     = token_holders_in_cluster / sybil_cluster_member_count

if top_holder_overlap_pct >= sybil_cluster_top_holder_pct_threshold
    AND sybil_cluster_member_count >= sybil_cluster_min_size:
    conf_raw_A = 0.40 + 0.40 * (top_holder_overlap_pct / 1.0)
                                             -- linear scale from 0.40 to 0.80
```

This is the primary signal: a known common-funder cluster holds a coordinated position
across multiple wallets in the token's holder set. It does not prove malicious intent
(airdrop farmers, legitimate multi-wallet users also pattern-match), hence the 0.40 base.

**Signal B — Cluster confidence amplifier:**

```
cluster_confidence = wallet_clusters.confidence  // [0.50, 0.85] from ClusterDetector

conf_raw_B = conf_raw_A * (0.50 + 0.50 * cluster_confidence)
           -- amplify: high-confidence cluster (0.85) → ×0.925 multiplier
           -- attenuate: low-confidence cluster (0.50) → ×0.75 multiplier
```

The cluster confidence from `compute_confidence` (design 0013 §11) reflects how tightly
the cluster was co-funded (size + time synchrony). A high-confidence cluster driving
top-holder overlap is more likely Sybil than a loose cluster.

**Final confidence:**

```
confidence = clamp(conf_raw_B, 0.0, 0.95)
```

Capped at 0.95 because D08 in Sprint 11 has no synchronized-activity confirmation (that is
Sprint 12). Even a tight common-funder cluster with 100% top-holder overlap could be a
legitimate airdrop recipient group. A missed label is expected by CLAUDE.md heuristic:
"False positives are cheap. False negatives are expensive" — err high on confidence given
this is a financial-loss scenario.

**Established-protocol suppression:** Not applicable to D08. `is_established_protocol`
suppresses state-based latent signals on established protocols. D08 fires on evidence of
coordinated wallet behavior — the cluster membership IS the signal, not a structural
precondition. Established tokens (BONK, WIF, RAY) can be Sybil-targeted for wash trading;
suppressing D08 on `jup_strict` tokens would mask those. Do NOT suppress.

### §6.3 Evidence keys

All keys use the `sybil_detection/` prefix (CLAUDE.md gotcha #9 + design 0003 §4):

| Key | Type | Meaning |
|-----|------|---------|
| `sybil_detection/cluster_id` | Decimal (0 = UUID encoded; see note) | UUID of the triggering cluster |
| `sybil_detection/cluster_size` | Decimal (integer) | `wallet_clusters.member_count` |
| `sybil_detection/cluster_confidence` | Decimal | `wallet_clusters.confidence` |
| `sybil_detection/top_holder_overlap_pct` | Decimal | Signal A metric |
| `sybil_detection/token_holders_in_cluster` | Decimal (integer) | Numerator of Signal A |
| `sybil_detection/sybil_cluster_min_size_threshold` | Decimal | Config threshold used |

**Note on cluster_id in evidence:** `Evidence::metrics` is `BTreeMap<String, Decimal>`.
A UUID cannot be stored as a Decimal. Store it in `Evidence.notes` as a plain string
"cluster_id={uuid}", and store the member count + confidence as Decimal metrics. This is
consistent with how D01 stores the pool address in `Evidence.addresses`.

`Evidence.addresses` MUST include the root_funder address (from `wallet_clusters.root_funder`).
`Evidence.notes` MUST include the cluster UUID and algorithm name.

### §6.4 Config keys (in `config/detectors.toml` under `[sybil_detection]`)

```toml
[sybil_detection.sybil_cluster_top_holder_pct_threshold]
value = 0.30
rationale = """
30% of a common-funder cluster appearing as top holders of one token is a strong signal
of coordinated accumulation. Liu et al. (2025) arxiv:2505.09313 use 'fraction of cluster
members holding the token' as a top-5 feature in their LightGBM classifier; the paper
does not publish an exact threshold but the feature importance rank justifies a low floor
to maximise recall (CLAUDE.md: false negatives are expensive).
"""
refs = ["D08/sybil_detection"]

[sybil_detection.sybil_cluster_min_size]
value = 3
rationale = """
Minimum 3 cluster members. Chainalysis (2025) Heuristic 2 requires >= 5 funded addresses
for wash trading attribution; 3 is the MVP lower bound matching min_cluster_size in
graph.toml, to ensure D08 fires on any cluster that ClusterDetector emits.
Raise to 5 after empirical FP calibration on Sprint 11 fixture corpus.
"""
refs = ["D08/sybil_detection"]
```

### §6.5 Test fixtures

Following CLAUDE.md §Detector Rules: two labelled fixtures required.

**Positive (POS_D08_01):** A token with 5+ top holders all belonging to a common-funder
cluster with confidence ≥ 0.70. Synthesised from the existing common-funder positive
fixtures in `tests/fixtures/solana/` + an injected `wallet_cluster_members` row set.
File: `tests/fixtures/solana/d08_positive_01_sybil_cluster.json`.

**Negative (NEG_D08_01):** A token with top holders drawn from known-CEX wallets (Binance,
Coinbase hot wallets) that are excluded from clustering by the CEX exclusion filter in
`ClusterDetector` (design 0013 §OQ2). The common-funder query already excludes
`kind = 'cex_hot_wallet'` via LEFT JOIN on `holder_classifications`. This negative fixture
confirms D08 does not fire on CEX-distributed holder bases.
File: `tests/fixtures/solana/d08_negative_01_cex_holders.json`.

---

## §7 Performance and Scale

### §7.1 Edge volume estimate (Solana, MVP scope)

At MVP scale: 100–1000 tracked tokens, each with:
- 1 `DeployerOf` edge per token: max 1,000 rows
- 2 `AuthorityOf` edges per token (mint + freeze): max 2,000 rows
- `TokenTransfer` edges (Sprint 12, deferred): bounded by `StreamingRegistry` (max 5,000
  active tokens × avg 100 transfers/day = 500,000 rows/day, but Sprint 12 only)

**Sprint 11 `graph_edges` row count:** ~3,000 rows. No partitioning needed.
Full table scan is trivially fast at this size; indexes are for future scale.

### §7.2 `address_labels` volume estimate

- `DeployerEOA`: one per unique deployer address. With 1,000 tokens × avg 5% unique
  deployers per token = ~50 unique deployers. Order of magnitude: hundreds.
- `FundingSource`: one per root_funder of a cluster. Same order as deployers: hundreds.
- `Sybil`: grows as D08 fires. Bounded by `max_streaming_tokens = 5000` × cluster density.
  Order of magnitude: thousands.

**Sprint 11 `address_labels` row count:** under 10,000 rows. No partitioning needed.

### §7.3 `wallet_edges` growth (existing table, for context)

The existing `wallet_edges` table (V00009) grows at the rate of unique
`(from_wallet, to_wallet)` SOL funding pairs among tracked addresses. The UPSERT
accumulates, so it does not grow unboundedly. At 1,000 tracked tokens × avg 10
funded wallets per deployer × avg 2 re-funding events = ~20,000 edges/day at MVP.
Over one year: ~7M rows. This is within Postgres B-tree index capacity; no escape
hatch needed before 100M rows.

### §7.4 D08 latency budget

D08 runs on a `streaming_d08_cadence_ticks = 50` cadence (every ~25 seconds). The
evaluation path:
1. Checkpoint query (1 row): < 1ms
2. `ClusterStore.funder_cluster` per fresh funder: < 5ms per cluster (index scan)
3. `holder_snapshots` JOIN for top holders: < 10ms per token (existing query path used by D03)
4. Signal computation (pure Rust): < 1ms

At 10 fresh clusters per cadence tick, total D08 evaluation: < 200ms per cadence period.
This is well within the 500ms debounce window and does not block any other detector
evaluations (D08 runs in the shared worker pool alongside D01–D07).

### §7.5 Scale triggers for future migration

If any of these conditions materialise, open a follow-up design:
- `graph_edges` exceeds 10M rows: partition by `block_time` (add it to PRIMARY KEY).
- `address_labels` exceeds 10M rows: add monthly partitioning.
- T2-2 TokenTransfer edge projection exceeds `graph_edges` capacity: separate
  `token_transfer_edges` partitioned table.

---

## §8 Open Questions

The following items need clarification or user input before S11-2 begins:

**OQ1: `token_risk_reports` migration number conflict.**
This design claims V00011. The SESSION-KICKOFF Sprint 10 task list identifies
`token_risk_reports` as "next V00011." The developer must use V00012 for
`token_risk_reports`. The CHANGELOG and SESSION-KICKOFF should be updated to reflect this.
**No user input required; design decision made here — flag for developer awareness.**

**OQ2: `AuthorityOf` edge for revoked authorities.**
When a token's `mint_authority` is set to NULL (authority renounced), should the
existing `AuthorityOf` edge be deleted, or should a new `AuthorityOf` edge with a
sentinel `to_address = '11111...1111'` (revocation marker) be added? The current design
writes `AuthorityOf` only for non-NULL authorities and never deletes them. This could
leave stale `AuthorityOf` edges after revocation.
**Recommendation:** On authority revocation (observed via `tokens` table update where
`mint_authority` changes to NULL), DELETE the corresponding `AuthorityOf` edge from
`graph_edges`. This requires the indexer writer to detect the NULL transition, not just
the non-NULL case.
**User input helpful:** Does the authority revocation event appear as a distinct indexed
transaction, or only as a state change detected at next token metadata poll? If the latter,
edge deletion timing is non-deterministic relative to actual revocation.

**OQ3: D08 cadence tuning.**
`streaming_d08_cadence_ticks = 50` (≈ 25 seconds) is an estimate. The actual
`ClusterDetector` recomputation runs every 168 hours (`cluster_ttl_hours`). D08 checking
for fresh clusters every 25 seconds adds negligible overhead (checkpoint read is O(1)) but
the cadence value should be validated after the first sprint. Is there a consumer SLO for
Sybil alert freshness?
**User input helpful:** What is the acceptable latency from "cluster updated" to "Sybil
anomaly event in `anomaly_events` table"? This drives the cadence calibration.

**OQ4: `TokenTransfer` edge volume for T2-2.**
Sprint 12 T2-2 (Tarjan SCC) will project `TokenTransfer` edges from the `transfers` table.
The volume estimate in §7.1 assumes only `StreamingRegistry`-tracked tokens. If T2-2
requires a complete transfer graph for cycle detection, the volume could be 10–100× higher
and would require partitioning `graph_edges` before Sprint 12 begins.
**User input helpful:** For T2-2, is cycle detection scoped to streaming-tracked tokens
only, or to all tokens in the database?

**OQ5: `address_labels` TTL for Sybil labels.**
Currently proposed at 168 hours (7 days). This means a Sybil label expires and D08 must
re-fire for the label to persist. If the trading bot consumer uses `address_labels` to
pre-screen transactions, a 7-day expiry means it re-screens every week. Should Sybil labels
be permanent once emitted, with explicit retraction only on re-evaluation with lower
confidence? Or is the TTL pattern (write, expire, re-evaluate) preferable for correctness?
**Design recommendation:** Permanent with UPDATE semantics (ON CONFLICT DO UPDATE when
EXCLUDED.confidence >= existing.confidence). Do not expire Sybil labels. The
`expires_at` column remains NULL for Sybil. The 168h TTL proposed above is retracted.
**No user input required; recommendation stands unless user has a specific rotation policy.**

---

## §9 Phased Implementation Plan

### Sprint 11

| Sub-task | Owner | LOC estimate | Scope |
|----------|-------|-------------|-------|
| S11-2: V00011 migration | developer (data-engineer) | ~60 SQL | `address_labels` + `graph_edges` tables + indexes |
| S11-3: `crates/graph` extension | developer | ~400 Rust | `labels.rs`, `typed_edges.rs`, `GraphError` variants, `lib.rs` re-exports, mock impl for both traits |
| S11-4: Indexer writer | developer | ~200 Rust | `DeployerOf` + `AuthorityOf` edge writes + `DeployerEOA` label writes in `PgEventSink`; reorg DELETE hook |
| S11-5: `ClusterDetector` `FundingSource` label | developer | ~80 Rust | Extend `run_common_funder` to upsert `FundingSource` labels; inject `GraphLabelStore` parameter |
| S11-6: D08 Sybil detector | developer | ~350 Rust | `d08_sybil.rs`; `SybilConfig`; cadence hook in streaming worker; 2 fixtures; config entries; REFERENCES.md |
| S11-7: Main-session verification | main session | N/A | `cargo clippy --workspace --all-targets -- -D warnings`; 890+ tests still passing; no migration regressions |

**Total estimate:** ~1,090 LOC (Rust + SQL). Within one sub-agent session per task; S11-3
and S11-4 may be batched by a single developer agent.

### Sprint 12+

| Item | Dependency | Design needed? |
|------|-----------|----------------|
| T2-2 Tarjan SCC + Johnson cycle detection (D05 Signal B upgrade) | `TokenTransfer` edge projection from `graph_edges` | New design doc 0016 |
| T2-1 BOCPD deployer changepoint | Per-deployer time-series from `address_labels` + `graph_edges` | New design doc 0017 |
| Synchronized-activity clustering | `wallet_clusters.cluster_kind = 'synchronized_activity'` | Extend design 0013 |
| Smart-money labelling | Historical P&L query on `swaps` | Extend this doc (§4.1 LabelType.SmartMoney) |
| D05 Signal B graph-backed | Sprint 12 cluster store | Extend design 0008 |
| D04 insider cluster upgrade | Sprint 11 `DeployerOf` edges available | Extend design 0007 |
| `token_risk_reports` V00012 | V00011 landed | Update SESSION-KICKOFF |

---

## §10 ADR Assessment

### Does this design contradict or extend ADRs 0001–0003?

**ADR 0001 §D5 (MVP detector set):** Extended. D08 Sybil is a Phase 3 detector, not in
the Phase 2 MVP set. This is consistent with ADR 0001 §D5's explicit deferral.

**ADR 0002 (Postgres-only):** Consistent. Both new tables use Postgres declarative
structure. The `graph_edges` table is unpartitioned at Sprint 11 scale with a documented
escape hatch for partition-by-block_time if row count exceeds 10M. This is the same
TimescaleDB escape hatch documented in ADR 0002 for event tables.

**ADR 0003 (self-sovereign):** Consistent. No 3rd-party graph database (Neo4j, Dgraph, AWS
Neptune) is introduced. The design explicitly avoids these. If the graph size eventually
warrants a purpose-built graph DB (Phase 5+), that decision requires a new ADR. For the
Phase 3 + Phase 4 volume projections, Postgres with B-tree indexes on
`(chain, from_address, edge_type)` and `(chain, to_address, edge_type)` is sufficient.
A bidirectional neighbor query is two B-tree index scans, P95 < 5ms at MVP scale.

**ADR 0004 proposal:** Not warranted. The graph data model extension (two new Postgres
tables, two new `crates/graph` modules) is an additive change fully within the existing
ADR constraints. A new ADR would be warranted only if this design introduced a
fundamentally different storage tier (e.g., a graph DB) or changed the streaming topology.
Neither applies here. The design 0014 §2.1 precedent applies: this document IS the record
of the design decisions. No ADR 0004.

---

## Inconsistency Report

The following inconsistencies between existing documents and the current codebase state
were discovered during the pre-read for this design:

**1. Design 0013 vs. actual shipped state (gotcha #14 applies):**
Design 0013 describes `crates/graph` as unimplemented ("Sprint 6 P6-3 design / Sprint 7
implementation"). As of Sprint 10, `crates/graph` is fully implemented: `edges.rs`,
`clusters.rs`, `api.rs`, `config.rs`, `mock.rs`, `lib.rs` are all present and tested.
V00009 migration is applied. This design (0015) treats the crate as already shipped and
designs only the additive extensions.

**2. SESSION-KICKOFF "Sprint 11" claim that graph is a new task:**
The SESSION-KICKOFF and task list (S11-2 through S11-7) imply building `crates/graph` from
scratch. The actual scope is narrower: extend an existing, tested crate with two new
modules and one new migration. The LOC estimates in §9 reflect the additive scope.

**3. `token_risk_reports` migration number conflict (OQ1):**
SESSION-KICKOFF Sprint 10 task list assigns V00011 to `token_risk_reports`. This design
assigns V00011 to graph tables. The developer must use V00012 for `token_risk_reports` and
update SESSION-KICKOFF + CHANGELOG accordingly.

**4. Research doc framing of D08:**
`research/03-feature-gap-2026-04-24.md` §T1-1 proposes D08 as a "launch audit" detector
(initial liquidity floor + LP lock at genesis). The ROADMAP Phase 3 and this task prompt
define D08 as a "Sybil detector" (graph-backed common-funder cluster). These are different
detectors. The research recommendation (T1-1 as highest-ROI) conflicts with the Sybil
framing in the task prompt. The task prompt takes precedence (user decision). T1-1
(launch audit) should be tracked as a separate detector candidate (D09 or inline within D02
as a temporal signal at genesis) for Sprint 12+.

**5. `crates/graph/src/clusters.rs` `run_common_funder` missing label writes:**
The existing implementation writes `wallet_clusters` and `wallet_cluster_members` but does
not write `FundingSource` labels to `address_labels`. This is expected (address_labels does
not exist yet). S11-5 adds the label write as specified in §4.3.
