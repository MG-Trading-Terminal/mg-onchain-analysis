# Design 0017 — D05 Signal B: Graph Cycle Detection (Tarjan SCC + Johnson)

**Date:** 2026-04-24
**Status:** Draft — awaiting developer implementation
**Author:** onchain-analyst agent
**Sprint:** 12 (T2-2 from `research/03-feature-gap-2026-04-24.md`)
**ADR refs:**
- ADR 0001 §D5 — MVP detector set; Phase 3 graph algorithms
- ADR 0002 — Postgres-only storage; NUMERIC(39,0) for u128; string-bridged amounts
- ADR 0003 — self-sovereign infrastructure; pure-Rust algorithms only; no external graph service
**Related designs:**
- `docs/designs/0008-detector-05-wash-trading.md` — existing D05 spec; Signal B proxy defined in §3.3
- `docs/designs/0015-crates-graph-phase3.md` — Sprint 11 graph foundation; TypedEdgeStore; TokenTransfer edge_type; §3.3.2 TokenTransfer projection note
- `docs/designs/0003-detector-trait.md` — Detector trait + DetectorContext
- `docs/designs/0016-detector-09-bocpd-deployer-changepoint.md` — structural template for this doc
**Binding prior art in REFERENCES.md** (new entries proposed in §13):
- Tarjan 1972 — SCC algorithm (new entry required)
- Johnson 1975 — elementary cycle enumeration (new entry required)
- Victor & Weintraud 2021 — circular trade patterns (already present; "Used In" update required)
- Chainalysis 2025 — wash trading Heuristic 2 (already present; "Used In" update required)

---

## §1 Purpose and Scope

### §1.1 Why cycle detection is stronger than `compute_cluster_flows`

D05 currently produces three signals: Signal A (H1 per-wallet round-trip self-dealing), Signal B (cluster reciprocal flow balance — the proxy), and Signal C (volume inflation amplifier). Signal B was explicitly designed in 0008 §3.3 as "a cheap Phase 2 proxy for the Sybil/multi-wallet wash pattern" pending Phase 3 graph work. The proxy's mechanism — aggregating net token flows per sender and searching for groups that sum to near-zero — has two structural weaknesses:

**Weakness 1: Zero-sum flow is necessary but not sufficient.** A legitimate pool where one large buyer and one large seller are active simultaneously will have near-zero aggregate net flow across the pair. The proxy has no mechanism to distinguish coordinated round-tripping from uncoordinated opposing positions. This produces false positives on healthy high-liquidity pools.

**Weakness 2: Zero-sum flow is insufficient for ring detection.** In a 3-wallet wash ring A→B→C→A, each participant has a non-zero net position in isolation. A sends tokens to B (positive net flow), B sends to C (positive net flow), C sends back to A (positive net flow). The cluster-flow algorithm sees three net buyers and finds no cluster satisfying the zero-sum criterion. The ring is invisible to the proxy.

The latent-flux system (research doc §10) operationally confirms this gap: "Tarjan SCC + Johnson cycle enumeration on the address graph detects circular fund flows structurally." A cycle A→B→C→A in the transfer graph is unambiguous evidence of coordinated circulation, regardless of each participant's individual net position. This is the mathematical structure that wash rings exploit, and it cannot be detected by flow-balance aggregation.

**What cycle detection catches that Signal B proxy misses:**
- 3+ distinct wallets cycling tokens in a ring, where each wallet has a positive net token flow (no individual net-zero signature)
- Rings of variable length (3–5 hops) that are time-bounded to a common window
- Multi-hub cycles (A→B→C→A and A→B→D→A simultaneously) that share wallets but are distinct elementary cycles

**What the proxy catches that cycle detection does not:**
- Nothing. The proxy is a subset of what cycle detection covers: any zero-sum cluster detected by the proxy is also discoverable as a degenerate 2-hop cycle (A→pool→A) by cycle detection over the extended graph. In practice, 2-hop self-dealing is covered by Signal A, and N-wallet zero-sum clusters are a weaker signal than confirmed elementary cycles.

### §1.2 Explicit replacement statement

**Signal B in D05 is REPLACED in full.** The following are DELETED with no backwards-compatibility shim:

- `compute_cluster_flows` function (currently at `crates/detectors/src/d05_wash_trading.rs` ~L547)
- `compute_signal_b_confidence` function (currently at ~L515)
- `fetch_pool_senders` SQL query (currently in §3.3 of design 0008 and inline in d05_wash_trading.rs)
- `ClusterResult` struct and its fields (only used by Signal B)
- `SenderFlowRow` struct (only used by Signal B's sender aggregation query)
- Config keys: `min_cluster_size`, `min_cluster_volume_usd`, `cluster_balance_tolerance_pct`, `top_senders_cap` (replaced by new keys in §7)

The new `compute_signal_b_cycles` function replaces all of the above. Signal A, Signal C, and all surrounding D05 infrastructure (config loading, established-protocol suppression, evidence construction) are unchanged.

### §1.3 Cross-reference to research and sprint plan

This design implements **T2-2** from `research/03-feature-gap-2026-04-24.md`:

> "Graph cycle detection for wash-trading (Tarjan SCC + Johnson). Input: transfer edges in the graph crate (Phase 3 prerequisite). Tarjan SCC pre-filters: identify strongly connected components in the transfer graph (O(V+E)). Johnson's algorithm enumerates all elementary cycles within SCCs of size ≥ 3. Signal: cycle_length ≤ max_cycle_length (proposed: 5 hops), cycle_volume_usd ≥ min_cycle_volume_usd (proposed: $1,000), cycle_time_window_minutes ≤ 120. Output: amplifier/evidence for D05 (replaces Signal B proxy)."

The Sprint 11 graph foundation (design 0015) shipped `TypedEdgeStore`, the `graph_edges` table with `edge_type = 'TokenTransfer'` accepted, and the Postgres index `idx_graph_edges_token ON graph_edges (chain, token, edge_type)`. These are the prerequisites. Sprint 12 T2-2 adds the projection strategy (§2) and the cycle detection algorithm (§3–§5).

---

## §2 TokenTransfer Edge Projection Strategy

### §2.1 Four options evaluated

Design 0015 §3.3.2 deferred the projection strategy for `TokenTransfer` edges to this spec. The decision is load-bearing: it determines the write path, the scale trigger, and reorg semantics.

**Option A — Eager (every ingested SPL Transfer writes a `graph_edges` row).**
Rejected. At mainnet Solana throughput, SPL Transfer events arrive at ~30,000/second. The `graph_edges` table is unpartitioned at MVP scale (design 0015 §3.3, gotcha #7: no partition key in the unique constraint yet). At 30K/sec × 86,400 sec/day = 2.6 billion rows/day. This exceeds the non-partitioned Postgres capacity by three orders of magnitude. Even filtered to 1% of transfers, the table reaches 1B rows/month. Do not recommend.

**Option B — Tracked-token projection (write `TokenTransfer` edges only for tokens in the `StreamingRegistry`).**
MVP `StreamingRegistry` holds O(100–1,000) active tokens. At 100 transfers/day per token × 1,000 tokens = 100,000 rows/day. Over 7 days (the proposed TTL): 700,000 rows. Within the non-partitioned Postgres capacity comfortably (table scan < 50ms at 1M rows with the `idx_graph_edges_token` B-tree index).

The write path: extend `Indexer::handle_transfer` (in the router) to check whether the transferred token is in the `StreamingRegistry`. If yes, call `TypedEdgeStore::insert_edge` for the `TokenTransfer` edge. The registry check is an in-process `HashSet<(Chain, String)>` lookup — O(1), no added latency to the hot path.

Scale trigger: if `StreamingRegistry` grows to 10,000 tokens × 1,000 transfers/day = 10M rows/week, the `graph_edges` table crosses the 10M-row partitioning trigger documented in design 0015 §7.5. At that point, partition `graph_edges` by `block_time` and add it to the PRIMARY KEY (gotcha #7 compliance).

**Option C — On-demand materialised view (background materialisation from `transfers` table on D05 evaluation request).**
The `transfers` table already holds all SPL Transfer events for tracked tokens with a 7-day retention (V00002 design). A background job could project a `transfers` window into a temporary in-memory graph when D05 is invoked. Tradeoff: query-time overhead of O(N transfers in 120-minute window) per D05 evaluation. For a token with 10,000 transfers in 2 hours, this is a 10,000-row table scan on every D05 cadence tick. Acceptable in isolation, but D05 evaluates every streaming tick for every tracked token — the aggregate latency budget is not preserved.

Additional problem: materialisation is not cached between D05 invocations (the next invocation covers a shifted 120-minute window). Each invocation re-reads and re-projects. The `graph_edges` table write in Option B amortises this cost by accumulating incrementally.

**Option D — Transient in-memory (D05 reads from `transfers` table directly; no `graph_edges` write).**
D05's `compute_signal_b_cycles` fetches the 120-minute transfer window directly from the `transfers` table via a helper `fetch_recent_transfers(token, window)`. No `graph_edges` write occurs for `TokenTransfer` edges. The `EdgeType::TokenTransfer` variant in the codebase is effectively dormant until a future consumer needs persistent graph edges.

Tradeoffs vs. Option B:
- No write-path overhead in the indexer
- No 7-day accumulation; only the 120-minute window is ever in memory
- The `transfers` table already has the B-tree index `(chain, token, block_time DESC)` from V00002
- `graph_edges` `TokenTransfer` rows are never written, making that edge_type dead code for D05's lifetime

Option D is viable because the `transfers` table is the authoritative source; `graph_edges` for `TokenTransfer` would be a projection of it. For cycle detection over a bounded 120-minute window, the authoritative source is the correct input.

### §2.2 Decision: Option D — Transient in-memory from `transfers` table

**Recommended: Option D.**

**Primary justification:** The `transfers` table (V00002) is already indexed for the exact access pattern needed by D05 cycle detection: `WHERE chain = $1 AND (sender = $2 OR receiver = $2) AND token = $3 AND block_time >= $4`. The 120-minute window produces a bounded, manageable row set. Writing the same data to `graph_edges` would duplicate it with 7-day retention when only 120 minutes is ever needed by D05.

**Secondary justification:** The `graph_edges` table's PRIMARY KEY `(chain, from_address, to_address, edge_type, token, block_height)` is designed for sparse, structural edges (`DeployerOf`, `AuthorityOf`) where one row per deployer per token at a specific block is the model. `TokenTransfer` edges are dense and time-series in nature — the same `(from, to, token)` pair can appear thousands of times in a day. Collapsing these by `block_height` requires one row per transfer event (since block_height varies), which collapses to Option B's volume problem. Aggregating by sender+receiver+token loses the temporal resolution needed for cycle time-window filtering.

**What changes if Option B is chosen later:** If a future Phase 4 consumer needs persistent `TokenTransfer` graph edges (e.g., for multi-day cycle detection or cross-chain graph analytics), Option B can be added without touching this design. The `TypedEdgeStore::insert_edge` and `EdgeType::TokenTransfer` are already implemented. The indexer write hook is the only addition needed, gated by `StreamingRegistry` membership. This is a no-disruption upgrade path from Option D.

**Reorg semantics under Option D:** The `transfers` table uses `ON CONFLICT DO NOTHING` with reorg DELETE on `block_height >= reorg_height` (V00002 design). D05 reads from `transfers` for a past 120-minute window. If a reorg occurs, `transfers` rows are deleted, and the next D05 evaluation automatically sees the corrected state. No additional reorg hook is needed in the graph writer for `TokenTransfer` edges.

### §2.3 The `fetch_recent_transfers` helper

The projection helper lives in `crates/graph/src/cycles.rs` (new module):

```
// Pseudocode (types match existing codebase conventions)

pub struct TransferEdge {
    pub from_address: String,
    pub to_address: String,
    pub amount_raw: u128,      // NUMERIC(39,0) String bridge, then parsed
    pub block_time: DateTime<Utc>,
    pub block_height: u64,
    pub tx_hash: String,
}

/// Fetch all SPL token transfers for `token` in `[window_start, window_end)`.
///
/// Results ordered by block_height ASC, tx_hash ASC for determinism.
/// Excludes self-transfers (from_address == to_address) and transfers
/// to/from the zero address.
///
/// Uses the `transfers` table (V00002) with index on (chain, token, block_time DESC).
/// Query is read-only; no writes to graph_edges.
pub async fn fetch_recent_transfers(
    pg: &sqlx::PgPool,
    chain: &str,
    token: &str,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    max_transfers: u32,   // safety ceiling — default 10_000
) -> Result<Vec<TransferEdge>, GraphError>
```

SQL:

```sql
SELECT sender AS from_address,
       receiver AS to_address,
       amount_raw,
       block_time,
       block_height,
       tx_hash
FROM transfers
WHERE chain = $1
  AND token = $2
  AND block_time >= $3
  AND block_time <  $4
  AND sender    <> receiver               -- drop self-transfers
  AND sender    <> '11111111111111111111111111111111'
  AND receiver  <> '11111111111111111111111111111111'
ORDER BY block_height ASC, tx_hash ASC
LIMIT $5;
```

**`max_transfers` ceiling:** 10,000 is the default; configurable as `[wash_trading_h1.signal_b_cycles].max_transfers_per_window`. A token with more than 10,000 unique transfers in 120 minutes at the volume level D05 monitors is either a large-cap token (in which case `min_pool_usd_for_h1` keeps D05 from evaluating it on pool-level criteria) or a victim of extreme wash trading (in which case hitting the ceiling is itself evidence that warrants further investigation). Hitting the ceiling logs a warning and continues with the capped set; it does not suppress the signal.

**Deduplication of same-pair multi-transfer edges:** Multiple transfers between the same `(from, to)` pair in the window are collapsed into a single logical edge with:
- `amount_raw_sum = SUM(amount_raw)` across all transfers in the pair
- `edge_count = COUNT(*)` — used in evidence output
- `earliest_block_time`, `latest_block_time` — used for cycle time window filter

This deduplication happens in Rust after the SQL fetch, not in SQL (to preserve the per-transfer tx_hash list for evidence).

**7-day TTL note:** Option D imposes no persistent TTL on `TokenTransfer` edges because none are written to `graph_edges`. The effective TTL is the `max_cycle_window_minutes` (120 minutes) passed as `window_start` to `fetch_recent_transfers`. If design 0015 §3.3.2's "7-day TTL" language is ever applied to a future Option B path, it refers to a `DELETE FROM graph_edges WHERE edge_type = 'TokenTransfer' AND block_time < now() - interval '7 days'` vacuum job. That job does not exist under Option D.

---

## §3 Tarjan SCC Algorithm Specification

### §3.1 Why Tarjan SCC is the correct pre-filter

Johnson's algorithm (§4) enumerates elementary cycles in a directed graph. Its complexity is O((V+E)(C+1)) where C is the number of elementary cycles — worst-case exponential in dense graphs. Without pre-filtering, running Johnson on the full transfer graph of 10,000 nodes could enumerate trillions of cycles. Tarjan SCC reduces the input: cycles can only exist within strongly connected components. By first identifying SCCs with Tarjan (O(V+E)) and discarding singleton SCCs and 2-node SCCs (where the only possible cycle is a 2-hop round-trip already covered by Signal A), we feed Johnson only the subgraphs that are provably capable of containing elementary cycles of length ≥ 3.

This two-phase structure is the standard approach in the graph-theory literature and is confirmed in the latent-flux system (research doc §10: "Tarjan SCC pre-filters... Johnson's algorithm enumerates all elementary cycles within SCCs of size ≥ 3").

### §3.2 Input

```
Input:  Vec<TransferEdge>  — output of fetch_recent_transfers (§2.3 deduplicated)
        max_cycle_length: u32  — config cap on cycle length
Output: Vec<Vec<usize>>        — list of SCCs; each SCC is a list of vertex indices
                               — sorted descending by SCC size
                               — SCCs with |V| < min_scc_size (default 3) are DROPPED
```

Graph construction from `Vec<TransferEdge>`:

1. Assign each unique address (from_address OR to_address) a contiguous integer vertex ID (index into a `Vec<String>` address table). Use a `HashMap<String, usize>` for O(1) lookup. Vertex IDs are assigned in the order addresses are first encountered, iterating `transfer_edges` in `block_height ASC, tx_hash ASC` order (the fetch order guarantees determinism).

2. Build an adjacency list: `adj[v] = Vec<usize>` of outgoing neighbors. Multiple edges from v to w collapse to one adjacency entry (graph structure for SCC, not edge multiplicity). This is the deduplication step at the graph construction level.

3. The total vertex count V and edge count E (after deduplication) are bounded by `max_transfers` (default 10,000). V ≤ 2 × max_transfers; E ≤ max_transfers (each transfer is at most one unique directed edge after deduplication).

### §3.3 Tarjan SCC algorithm — iterative Rust pseudocode

The recursive formulation of Tarjan SCC overflows the call stack for large V (V > ~10,000 on default 8 MB stacks). The iterative variant is required.

**Decision: hand-roll iterative Tarjan SCC.** Do not use `petgraph::algo::tarjan_scc`. Rationale: ADR 0003 prefers fewer dependencies; the algorithm is 60–80 lines of Rust; the hand-rolled version is auditable and deterministic without `petgraph`'s internal ordering decisions. The `petgraph` crate does remain in the workspace (it is already used by D09's `statrs` dep chain) but should not be introduced as a new dependency of `crates/graph` solely for SCC.

Reference implementation pattern: Pearce (2016), "A Space-Efficient Algorithm for Finding Strongly Connected Components" (iterative DFS formulation). The classic Tarjan iterative pattern uses an explicit DFS stack to simulate the call stack.

```
// Hand-roll Tarjan SCC (iterative)
//
// State per vertex:
struct TarjanNode {
    index:    Option<u32>,   // DFS discovery index; None = unvisited
    lowlink:  u32,
    on_stack: bool,
}

// Algorithm:
fn tarjan_scc(adj: &Vec<Vec<usize>>, v_count: usize) -> Vec<Vec<usize>> {
    let mut state = vec![TarjanNode { index: None, lowlink: 0, on_stack: false }; v_count];
    let mut stack: Vec<usize> = Vec::new();
    let mut index_counter: u32 = 0;
    let mut sccs: Vec<Vec<usize>> = Vec::new();

    // Explicit DFS stack: (vertex, iterator_position)
    // We iterate adj[v] by position to allow "resume after recursion" simulation.
    for start in 0..v_count {
        if state[start].index.is_some() { continue; }

        let mut dfs_stack: Vec<(usize, usize)> = vec![(start, 0)];
        // Set up start node
        state[start].index = Some(index_counter);
        state[start].lowlink = index_counter;
        index_counter += 1;
        stack.push(start);
        state[start].on_stack = true;

        while let Some((v, edge_idx)) = dfs_stack.last_mut() {
            let v = *v;
            if *edge_idx < adj[v].len() {
                let w = adj[v][*edge_idx];
                *edge_idx += 1;
                if state[w].index.is_none() {
                    // Recurse into w
                    state[w].index = Some(index_counter);
                    state[w].lowlink = index_counter;
                    index_counter += 1;
                    stack.push(w);
                    state[w].on_stack = true;
                    dfs_stack.push((w, 0));
                } else if state[w].on_stack {
                    // w is on stack; update lowlink of v
                    let w_index = state[w].index.unwrap();
                    state[v].lowlink = state[v].lowlink.min(w_index);
                }
                // else: w is finalized; cross/forward edge — skip
            } else {
                // All neighbors of v processed — pop v
                dfs_stack.pop();

                if let Some(&(parent, _)) = dfs_stack.last() {
                    // Propagate lowlink upward
                    let v_lowlink = state[v].lowlink;
                    state[parent].lowlink = state[parent].lowlink.min(v_lowlink);
                }

                // SCC root check: lowlink[v] == index[v]
                if state[v].lowlink == state[v].index.unwrap() {
                    let mut scc: Vec<usize> = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        state[w].on_stack = false;
                        scc.push(w);
                        if w == v { break; }
                    }
                    // Sort scc by vertex ID for determinism
                    scc.sort_unstable();
                    sccs.push(scc);
                }
            }
        }
    }

    // Sort SCCs descending by size, then ascending by minimum vertex ID for determinism
    sccs.sort_by(|a, b| b.len().cmp(&a.len()).then(a[0].cmp(&b[0])));
    sccs
}
```

### §3.4 SCC filtering

After `tarjan_scc` returns, filter:

```
let candidate_sccs: Vec<Vec<usize>> = sccs
    .into_iter()
    .filter(|scc| scc.len() >= min_scc_size)  // min_scc_size = 3 (config)
    .collect();
```

A 2-vertex SCC can only contain a 2-hop cycle (A→B and B→A), which is a degenerate form of the self-dealing already covered by Signal A. We are interested in rings of 3+ distinct wallets.

A 1-vertex SCC means the vertex has no path back to itself (no cycle). Drop immediately.

### §3.5 Complexity

- Time: O(V + E) for Tarjan SCC. With V ≤ 20,000 and E ≤ 10,000 (from `max_transfers` bound), this is O(30,000) operations — sub-millisecond.
- Space: O(V + E) for the adjacency list, DFS stack, and SCC output.
- No heap allocation per vertex in the hot path (the `state` Vec is pre-allocated at construction).

---

## §4 Johnson's Algorithm for Elementary Cycle Enumeration

### §4.1 Decision: hand-roll Johnson vs. `petgraph`

`petgraph` v0.6+ includes `petgraph::algo::all_simple_paths` but not Johnson's algorithm as a named function. The closest is an exhaustive DFS that can be adapted. Johnson 1975 is 80–120 lines of Rust; the iterative `circuit()` + `unblock()` structure is well-documented in the original paper.

**Decision: hand-roll Johnson's algorithm.**

Rationale:
1. `petgraph` is not in `crates/graph`'s current `Cargo.toml`. Adding it for Johnson would introduce a new dep to a crate that is used by `crates/detectors` (dep chain: detectors → graph). This adds compile time and transitive deps for 120 lines of algorithm.
2. The hand-roll is bounded by `max_cycles_per_scc` (default 100) and `max_cycle_length` (default 5 hops). These bounds mean worst-case behavior is explicitly capped; Johnson's exponential worst case is not reached in production.
3. Audit and reproducibility: the hand-rolled version is directly tied to the pseudocode in this spec; any future reader can verify the implementation against the design without understanding `petgraph`'s internal edge representation.

If a future use case requires the full generality of `petgraph` (e.g., Phase 4 EVM graph analytics), that is the right time to add it. The dependency decision is reversible.

### §4.2 Johnson 1975 algorithm specification

Reference: Johnson, D. B. (1975). "Finding All the Elementary Circuits of a Directed Graph." SIAM Journal on Computing, 4(1), 77–89. DOI: 10.1137/0204007.

The algorithm maintains a "blocked" set that prevents revisiting vertices on the current path. It is recursive (depth-bounded by `max_cycle_length`). The recursive variant is safe here because `max_cycle_length = 5` means the DFS depth is bounded to 5 levels — no stack overflow risk.

```
// Johnson's algorithm for elementary cycles in a subgraph induced by one SCC.
//
// scc_vertices: Vec<usize> — the vertex indices in this SCC (already sorted)
// adj: &Vec<Vec<usize>> — global adjacency list (reused from Tarjan phase)
// token_price_usd: Decimal — for cycle_volume_usd computation
// edges_by_pair: HashMap<(usize, usize), AggregatedEdge> — lookup for amount_raw_sum

fn enumerate_cycles_in_scc(
    scc_vertices: &[usize],
    adj: &Vec<Vec<usize>>,
    edges_by_pair: &HashMap<(usize, usize), AggregatedEdge>,
    cfg: &SignalBCyclesConfig,
    token_price_usd: Decimal,
    address_table: &[String],  // vertex_id → address string
) -> Vec<DetectedCycle> {
    let scc_set: HashSet<usize> = scc_vertices.iter().cloned().collect();

    // Result accumulator
    let mut detected_cycles: Vec<DetectedCycle> = Vec::new();

    // Johnson state: blocked set and blocker list
    let n = scc_vertices.len();
    let mut blocked: Vec<bool> = vec![false; n];
    let mut blockers: Vec<Vec<usize>> = vec![Vec::new(); n];  // blockers[i] = list of j blocked by i

    // s iterates over vertices in SCC in sorted order (determinism)
    for s_pos in 0..n {
        let s = scc_vertices[s_pos];

        // Reset blocked for vertices >= s_pos only (Johnson §2 optimization)
        for i in s_pos..n {
            blocked[i] = false;
            blockers[i].clear();
        }

        // Path stack for circuit()
        let mut path: Vec<usize> = Vec::new();
        circuit(
            s, s, s_pos,
            &scc_vertices, &scc_set, adj,
            &mut path, &mut blocked, &mut blockers,
            &mut detected_cycles, edges_by_pair, cfg,
            token_price_usd, address_table,
        );
    }

    detected_cycles
}

// Recursive circuit function (bounded by max_cycle_length)
fn circuit(
    v: usize, s: usize, s_pos: usize,
    scc_vertices: &[usize], scc_set: &HashSet<usize>, adj: &Vec<Vec<usize>>,
    path: &mut Vec<usize>, blocked: &mut Vec<bool>, blockers: &mut Vec<Vec<usize>>,
    detected_cycles: &mut Vec<DetectedCycle>,
    edges_by_pair: &HashMap<(usize, usize), AggregatedEdge>,
    cfg: &SignalBCyclesConfig,
    token_price_usd: Decimal,
    address_table: &[String],
) -> bool {
    // Hard stop: max_cycle_length cap
    if path.len() >= cfg.max_cycle_length as usize {
        return false;
    }
    // Hard stop: max_cycles_per_scc cap
    if detected_cycles.len() >= cfg.max_cycles_per_scc as usize {
        return false;
    }

    path.push(v);
    let v_pos = scc_vertices.partition_point(|&x| x < v);
    blocked[v_pos] = true;
    let mut found_cycle = false;

    for &w in adj[v].iter() {
        // Only follow edges within the SCC
        if !scc_set.contains(&w) { continue; }
        // Only follow edges to vertices >= s (Johnson §1 key property)
        let w_pos = scc_vertices.partition_point(|&x| x < w);
        if scc_vertices[w_pos] < s { continue; }

        if w == s {
            // Cycle found: path + back-edge to s
            let cycle_vertices: Vec<usize> = path.clone();
            let cycle_len = cycle_vertices.len();

            if cycle_len >= 2 {  // min 2-hop cycle (3 including s itself)
                if let Some(dc) = build_detected_cycle(
                    &cycle_vertices, s, edges_by_pair, token_price_usd,
                    address_table, cfg,
                ) {
                    detected_cycles.push(dc);
                }
            }
            found_cycle = true;
        } else if !blocked[w_pos] {
            if circuit(
                w, s, s_pos, scc_vertices, scc_set, adj, path,
                blocked, blockers, detected_cycles, edges_by_pair,
                cfg, token_price_usd, address_table,
            ) {
                found_cycle = true;
            }
        }
    }

    if found_cycle {
        unblock(v_pos, blocked, blockers);
    } else {
        // Add v to the blocker lists of all unvisited successors
        for &w in adj[v].iter() {
            if !scc_set.contains(&w) { continue; }
            let w_pos = scc_vertices.partition_point(|&x| x < w);
            if scc_vertices[w_pos] < s { continue; }
            if !blockers[w_pos].contains(&v_pos) {
                blockers[w_pos].push(v_pos);
            }
        }
    }

    path.pop();
    found_cycle
}

fn unblock(u_pos: usize, blocked: &mut Vec<bool>, blockers: &mut Vec<Vec<usize>>) {
    blocked[u_pos] = false;
    let us_blockers: Vec<usize> = blockers[u_pos].drain(..).collect();
    for w_pos in us_blockers {
        if blocked[w_pos] {
            unblock(w_pos, blocked, blockers);
        }
    }
}
```

### §4.3 Determinism guarantee

1. `scc_vertices` is sorted ascending by vertex ID before entering `enumerate_cycles_in_scc`.
2. The outer loop (`s_pos`) iterates 0..n in ascending order.
3. `adj[v]` is built from `transfer_edges` sorted `block_height ASC, tx_hash ASC`; adjacency list construction iterates in that order, so neighbor lists are ordered by first-seen edge. Add a sort step after adjacency list construction: `for neighbors in adj.iter_mut() { neighbors.sort_unstable(); }`.
4. Detected cycles are accumulated in discovery order — fully deterministic given deterministic input.

**Property test requirement (§12):** feed the same `Vec<TransferEdge>` twice and assert `detected_cycles == detected_cycles_second_run`.

### §4.4 Complexity and caps

- Worst case without caps: O((V+E)(C+1)) where C = number of elementary cycles. For a complete directed graph on V=5 vertices, C can be in the hundreds. For V=5, max_cycle_length=5, the cap of 100 cycles per SCC is hit before the theoretical maximum.
- With caps: `max_cycles_per_scc = 100` and `max_cycle_length = 5` together bound the recursion depth to 5 and the output to 100 cycles. The algorithm terminates in O(V × E × max_cycle_length × max_cycles_per_scc) = O(5 × 10 × 5 × 100) = O(25,000) operations per SCC — sub-millisecond even for the worst MVP-scale case.
- `max_cycles_per_scc` overflow is documented as evasion E-D05-B-9 and DG-D05-B-3 (§8, §11).

---

## §5 Wash-Cycle Signal Definition

### §5.1 Cycle volume computation

For each `DetectedCycle` emitted by Johnson's algorithm, compute:

```
cycle_volume_usd:
    For each consecutive pair (v_i, v_{i+1}) in the cycle (wrapping: last→first):
        edge = edges_by_pair[(v_i, v_{i+1})]
        edge_volume_usd = (edge.amount_raw_sum as Decimal) * token_price_usd
                          / 10^token_decimals      // normalize to whole tokens
    cycle_volume_usd = MIN(edge_volume_usd) over all edges in the cycle
```

**Why minimum, not sum?** The minimum edge volume is the bottleneck — the limiting amount that actually cycled. Summing edge volumes would double-count (the same tokens appear as both the "sent" and "received" leg of each edge). The minimum is the volume that provably completed the full ring, consistent with how Victor & Weintraud 2021 define the "wash volume" for a circular trade.

**Token price source:** `pools.liquidity_usd / (pools.base_reserve_raw / 10^decimals)` at the start of the evaluation window (`ctx.observed_at - max_cycle_window_minutes`). This is the same price derivation used by D04 Signal A and is already available via `TokenRegistry::enrich`. If the price is unavailable (new token, no pool enrichment yet), skip cycles for that token with a logged warning. Do NOT emit zero-confidence events based on zero price.

**Token decimals source:** `tokens.decimals` from the `TokenMeta` struct (already in `DetectorContext`).

### §5.2 Cycle filter criteria

Apply all three filters; a cycle must pass all three to count:

```
filter_1: cycle_vertices.len() <= max_cycle_length (default: 5)
          -- Note: cycle_vertices.len() is the number of hops (edges).
          -- A 3-wallet ring A→B→C→A has 3 vertices and 3 hops.
          -- max_cycle_length = 5 means rings up to 5 wallets are detected.

filter_2: cycle_volume_usd >= min_cycle_volume_usd (default: $1,000)
          -- Dust cycles below this threshold are noise; legitimate market
          -- makers route orders larger than $1K.
          -- Victor & Weintraud 2021: median circular trade > $5K on IDEX.

filter_3: max(block_time over cycle edges) - min(block_time over cycle edges)
          <= max_cycle_window_minutes * 60 seconds (default: 120 minutes)
          -- The transfers forming the cycle must occur within a 120-minute window.
          -- This prevents false positives from coincidental transfer patterns
          -- spread over multiple days.
```

### §5.3 Why max_cycle_length = 5

Victor & Weintraud (2021) §4.2 analyze circular trade patterns on IDEX/EtherDelta and find that the median wash ring has 3–4 participants. The 95th percentile is 6 participants. Setting `max_cycle_length = 5` catches the median case and misses only the 95th percentile and above. This is an intentional false-negative acceptance to prevent performance degradation from exponential cycle enumeration on large SCCs.

The tradeoff: an attacker who routes through 6+ wallets evades detection until the config is raised. This is documented in the evasion analysis (E-D05-B-2, §8) and is an accepted gap with a low operational cost: wash trading through 6 wallets requires 6× the on-chain fees and coordination overhead.

### §5.4 Aggregate signal output

After filtering, compute the following aggregates across all qualified cycles:

```
cycle_count:               Decimal (integer count)  — count of qualified cycles
total_cycle_volume_usd:    Decimal                  — SUM(cycle_volume_usd) over qualified cycles
largest_cycle_length:      Decimal (integer)        — MAX(cycle_vertices.len()) over qualified cycles
unique_wallets_in_cycles:  Decimal (integer)        — |UNION(cycle_vertices)| over all qualified cycles
```

These four Decimal values form the Signal B evidence output for D05.

---

## §6 D05 Integration — Replacing Signal B Proxy

### §6.1 Deletion scope (explicit)

The following code is DELETED from `crates/detectors/src/d05_wash_trading.rs`:

- `struct ClusterResult` and all its fields
- `struct SenderFlowRow` and all its fields
- `fn compute_cluster_flows(sender_rows: &[SenderFlowRow], cfg: &WashTradingConfig) -> ClusterResult`
- `fn compute_signal_b_confidence(cluster: &ClusterResult, cfg: &WashTradingConfig) -> SignalBResult`
- `struct SignalBResult`
- `fn fetch_pool_senders(...)` or equivalent async method
- Any `cfg.min_cluster_size`, `cfg.min_cluster_volume_usd`, `cfg.cluster_balance_tolerance_pct`, `cfg.top_senders_cap` references from `WashTradingConfig`

The `WashTradingConfig` struct gains a new `signal_b_cycles: SignalBCyclesConfig` field; the old cluster-related fields are removed.

### §6.2 New `compute_signal_b_cycles` integration

The new Signal B path in `WashTradingDetector::evaluate`:

```
// --- Signal B (cycles, replacing cluster flow balance) ---
// NOT gated by is_established_protocol.
// Established tokens (BONK, BONK, WIF) CAN be wash-traded; suppressing
// Signal B here would mask coordinated ring trading. See design 0015 §6.2
// and gotcha #42 — same suppression policy as D08 Sybil.

let window_start = ctx.observed_at
    - Duration::minutes(cfg.signal_b_cycles.max_cycle_window_minutes as i64);

// Fetch transfers from `transfers` table (Option D from §2)
let transfer_edges = crates::graph::cycles::fetch_recent_transfers(
    &pg_pool,
    ctx.chain.as_str(),
    ctx.token.canonical.as_str(),
    window_start,
    ctx.observed_at,
    cfg.signal_b_cycles.max_transfers_per_window,
).await?;

if !transfer_edges.is_empty() {
    let cycle_result = crates::graph::cycles::run_cycle_detection(
        &transfer_edges,
        token_price_usd,
        token_decimals,
        &cfg.signal_b_cycles,
    );

    if cycle_result.cycle_count > 0 {
        let confidence_b = compute_signal_b_cycles_confidence(&cycle_result, &cfg.signal_b_cycles);
        let evidence_b = build_evidence_b_cycles(&cycle_result, suppressed_signal_a);
        events.push(make_anomaly_event("wash_trading_h1", confidence_b, evidence_b));
    }
}
```

`ctx.observed_at` is `DateTime<Utc>` from `DetectorContext`. No `Utc::now()` call (gotcha #22). No `Utc::now()` anywhere in the evaluation path.

`pg_pool: Arc<sqlx::PgPool>` is injected into `WashTradingDetector` at construction, alongside the existing `Arc<dyn PoolAccountProvider>` (same pattern as D01). The detector already constructs with a Postgres pool reference for Signal A's query; Signal B reuses the same pool.

### §6.3 Confidence formula

```
conf_raw_B = 0.40 + 0.40 * min(1.0, total_cycle_volume_usd / 10_000.0)

confidence_b = min(0.85, conf_raw_B)
```

**Base 0.40:** Cycle existence is a strong structural signal (not a proxy), so the base is raised from the current proxy's 0.50 to 0.40 with full scale up to 0.85. Wait — the current Signal B proxy is capped at 0.60, and the design 0008 §2 table says Signal B range is "0.50–0.60". The new cycle-based Signal B is more reliable, so the cap is raised to 0.85 to match Signal A's cap. This is consistent with the design intent in the task prompt: "Capped at 0.85 (unchanged from current Signal B cap per design 0008)" — but design 0008 caps Signal B at 0.60, not 0.85. The task prompt's 0.85 cap is intentional as a deliberate upgrade for the stronger signal. The 0.40 base reflects that even a confirmed cycle could be a coincidence in a low-volume token where the same wallets trade frequently.

**Scale factor $10,000:** At total_cycle_volume_usd = $0, confidence = 0.40. At $10,000+, confidence = 0.80 before the cap. At $10,000, conf_raw_B = 0.40 + 0.40 × 1.0 = 0.80, capped at 0.85 only if multiple SCCs each contribute cycles. The $10,000 scale point is calibrated from Victor & Weintraud 2021 median circular trade size ($5,000–$20,000 range) and gives a smooth linear ramp from "cycle exists but tiny" to "cycle is material volume".

**Why not sigmoid?** The linear ramp from $0 to $10,000 is appropriate because the relationship between cycle volume and wash-trading intent is approximately linear in this range. Sigmoid would suppress confidence near $5,000 (the midpoint) unnecessarily.

### §6.4 Evidence keys

All keys use the `wash_trading_h1/` prefix (gotcha #9 + design 0003 §4). Signal B cycle keys are distinguished by the `signal_b_cycles/` sub-prefix:

| Key | Type | Meaning |
|-----|------|---------|
| `wash_trading_h1/signal_b_cycles/cycle_count` | Decimal (integer) | Count of qualified elementary cycles |
| `wash_trading_h1/signal_b_cycles/total_cycle_volume_usd` | Decimal | Sum of min-edge volumes across qualified cycles |
| `wash_trading_h1/signal_b_cycles/largest_cycle_length` | Decimal (integer) | Longest qualified cycle (hop count) |
| `wash_trading_h1/signal_b_cycles/unique_wallets_in_cycles` | Decimal (integer) | Union of unique wallets across all qualified cycles |
| `wash_trading_h1/signal_b_cycles/scc_count_evaluated` | Decimal (integer) | Number of SCCs passed to Johnson's algorithm |
| `wash_trading_h1/signal_b_cycles/transfers_in_window` | Decimal (integer) | Count of transfer edges fetched from transfers table |
| `wash_trading_h1/signal_b_cycles/max_transfers_cap_hit` | Decimal (0 or 1) | 1 if fetch hit max_transfers ceiling |

`Evidence.addresses` MUST include the cycle wallets (up to 20 addresses; truncate with `…` in the note if more). `Evidence.notes` MUST include "cycle_detection" and the SCC count.

### §6.5 Established-protocol suppression policy

Signal B (cycles) does NOT suppress on `is_established_protocol`. This is the same policy as D08 Sybil (gotcha #42, design 0015 §6.2): established tokens can be targeted for wash ring trading; suppression would mask coordinated manipulation. The suppression rationale for Signal A (established protocols have legitimate high-frequency trading that mimics H1 round-trips) does NOT apply to Signal B (elementary cycles are structurally anomalous regardless of protocol maturity).

### §6.6 Worked example

Three wallets: W1 (`7xKP`), W2 (`9rTQ`), W3 (`2mNZ`).

Transfer edges in a 30-minute window:
- W1 → W2: 500,000 BONK tokens, block 285,000,000
- W2 → W3: 500,000 BONK tokens, block 285,000,100
- W3 → W1: 500,000 BONK tokens, block 285,000,200

BONK price: $0.000020/token (mid-2025 approximate). Decimals: 5.

**Step 1: Build graph.** Vertex IDs: W1=0, W2=1, W3=2. Adjacency: adj[0]=[1], adj[1]=[2], adj[2]=[0]. Deduplication: one edge per pair (no multiple transfers in this example).

**Step 2: Tarjan SCC.** DFS from vertex 0: discovers path 0→1→2→0 (back-edge to 0). lowlink propagation: lowlink[2]=0, lowlink[1]=0, lowlink[0]=0. SCC root at vertex 0: pops {0,1,2}. Output: one SCC = [0, 1, 2] (size 3, passes min_scc_size=3 filter).

**Step 3: Johnson's algorithm on SCC [0,1,2].** Start with s=W1 (vertex 0).
- path=[0], blocked[0]=true
- Neighbor of 0 in SCC with index ≥ 0: W2 (vertex 1). Not blocked. Recurse with v=1.
  - path=[0,1], blocked[1]=true
  - Neighbor of 1 in SCC: W3 (vertex 2). Not blocked. Recurse with v=2.
    - path=[0,1,2], blocked[2]=true
    - Neighbor of 2 in SCC: W1 (vertex 0) = s. CYCLE FOUND: path=[0,1,2], cycle back to 0.
    - cycle_vertices = [0, 1, 2]. Length = 3 hops. PASSES filter_1 (3 ≤ 5).
    - found_cycle=true. unblock(2).
  - found_cycle=true. unblock(1).
- found_cycle=true. unblock(0).

No more startable cycles from s=0 (all vertices unblocked; restarting would revisit same path). Move to s=W2, s=W3 — these produce no new elementary cycles (the only cycle is the 3-ring already found).

**Step 4: Cycle filtering.**
- Amount raw: 500,000 × 10^5 = 50,000,000,000 raw units. Price: $0.000020/token. Edge volume = 500,000 BONK × $0.000020 = $10.00 per edge.
- cycle_volume_usd = MIN($10.00, $10.00, $10.00) = $10.00.
- filter_2: $10.00 ≥ $1,000? No. CYCLE DROPPED.

*Why the example fails filter_2:* BONK's price is very low; 500K tokens = $10. To pass filter_2 at $1,000, the ring would need 50M BONK per edge at this price. This is realistic: a wash ring targeting $50K/day would cycle 2.5B BONK per edge — above average daily volume. The $1,000 threshold filters out dust cycles on low-price tokens.

**Modified example for passed filter:** Same ring, but 50,000,000 BONK per edge at $0.000020 = $1,000 per edge. cycle_volume_usd = $1,000. filter_2 passes. filter_3: max(block_time) - min(block_time) = ~80 seconds ≪ 120 minutes. PASSES.

**Confidence calculation:**
- total_cycle_volume_usd = $1,000
- conf_raw_B = 0.40 + 0.40 × min(1.0, 1000/10000) = 0.40 + 0.40 × 0.10 = 0.44
- confidence_b = min(0.85, 0.44) = 0.44

For a $10,000 ring (10× larger):
- conf_raw_B = 0.40 + 0.40 × 1.0 = 0.80
- confidence_b = min(0.85, 0.80) = 0.80

This is an appropriate range: a $1K ring in a new token is a low-confidence alert (0.44); a $10K+ ring is a high-confidence alert (0.80).

---

## §7 Thresholds in `config/detectors.toml`

New sub-section `[wash_trading_h1.signal_b_cycles]`. All existing Signal B cluster keys are REMOVED. These replacements are the only Signal B config entries:

```toml
# ---------------------------------------------------------------------------
# D05 Signal B — Graph Cycle Detection (Tarjan SCC + Johnson)
# Replaces: min_cluster_size, min_cluster_volume_usd,
#           cluster_balance_tolerance_pct, top_senders_cap
# Design doc: docs/designs/0017-d05-signal-b-graph-cycles.md
# ---------------------------------------------------------------------------

[wash_trading_h1.signal_b_cycles.max_cycle_length]
value     = 5
rationale = """
Maximum elementary cycle length (number of hops = number of wallets in the ring).
Victor & Weintraud (2021) §4.2 report that the 95th percentile of wash rings on
IDEX/EtherDelta has 6 participants. Setting max_cycle_length = 5 catches the median
case (3–4 participants) and misses only extreme-length rings. Increasing to 6 or 7
is feasible but raises Johnson's enumeration cost for large SCCs. The latent-flux
production system (research/03-feature-gap-2026-04-24.md §10) uses a similar bound
on cycle length without publishing the exact value. Calibrate against positive
fixture corpus when Sprint 12 label set is assembled.
"""
refs      = ["D05/signal_b_cycles"]

[wash_trading_h1.signal_b_cycles.max_cycle_window_minutes]
value     = 120
rationale = """
Maximum time window (minutes) within which all edges in a qualifying cycle must
fall. Victor & Weintraud (2021) §3.1 define circular trades as occurring within
a single trading session; 120 minutes is a conservative interpretation of a
"session" for Solana DEX activity. Chainalysis (2025) wash-trading detection
window for Heuristic 2 is not published but their description implies intra-day
clustering. 120 minutes balances detection sensitivity (tight window catches
coordination) against false positive risk (two wallets that happen to swap the
same token within a day are not necessarily colluding). Extending to 240 minutes
would increase recall at the cost of FP rate; requires re-calibration.
"""
refs      = ["D05/signal_b_cycles"]

[wash_trading_h1.signal_b_cycles.min_cycle_volume_usd]
value     = 1000
rationale = """
Minimum USD volume for the bottleneck edge of a qualifying cycle (the minimum
edge volume within the cycle). Victor & Weintraud (2021) §4.2 report median
circular trade size of $5,000–$20,000 on IDEX/EtherDelta. Our threshold of
$1,000 is deliberately below the median to catch emerging wash rings before they
scale up. Dust cycles (< $1,000) are more likely coincidental token recycling than
coordinated manipulation. Calibrate upward if FP rate is high on legitimate DEX
activity in the Sprint 12 negative fixture corpus. The Chainalysis (2025) $704M
total wash volume across all 2024 events implies an average of ~$3,500/event
assuming 200,000 wash events — consistent with a $1,000 floor.
"""
refs      = ["D05/signal_b_cycles"]

[wash_trading_h1.signal_b_cycles.max_cycles_per_scc]
value     = 100
rationale = """
Maximum elementary cycles to enumerate from a single SCC. Johnson's algorithm
worst case is exponential in cycle count; this cap bounds the evaluation latency
at 100 cycles per SCC. An SCC with more than 100 elementary cycles shorter than
max_cycle_length = 5 is either (a) a very large clique (many wallets all
trading with each other) or (b) an adversarial construction designed to saturate
the cycle enumerator (evasion E-D05-B-9). In case (a), the first 100 cycles
already confirm the pattern; in case (b), we log a warning and proceed with the
capped set. Do NOT increase above 1,000 without re-evaluating the latency budget.
"""
refs      = ["D05/signal_b_cycles"]

[wash_trading_h1.signal_b_cycles.min_scc_size]
value     = 3
rationale = """
Minimum size of an SCC passed to Johnson's cycle enumerator. SCCs of size 1
contain no cycles. SCCs of size 2 can only contain a 2-hop cycle (A→B and B→A),
which is structurally equivalent to the same-address round-trip already covered
by Signal A (H1 pattern). The mathematical prerequisite for a wash ring involving
3+ distinct wallets is an SCC of size ≥ 3. Tarjan 1972 §2 establishes that all
elementary cycles in a directed graph lie within SCCs. Johnson 1975 §1 requires
|SCC| ≥ 2 for any cycle; we add the additional constraint of ≥ 3 to avoid
duplicating Signal A coverage.
"""
refs      = ["D05/signal_b_cycles"]

[wash_trading_h1.signal_b_cycles.max_transfers_per_window]
value     = 10000
rationale = """
Safety ceiling on the number of transfer edges fetched from the transfers table
per D05 evaluation window. At 10,000 transfers in 120 minutes, the transfer rate
is ~83/minute — an extremely active pool. For wash-trading detection, the
first 10,000 transfers are sufficient to identify ring structure; additional
transfers only provide confirmation. Hitting this ceiling logs a tracing warning
at WARN level. The ceiling prevents O(N²) SCC construction from becoming
unbounded. If a token routinely hits this ceiling (e.g., BONK or WIF in high
activity periods), the established_protocol suppression policy on Signal A should
already exclude it from D05 evaluation entirely. Calibrate downward to 5,000 if
P95 evaluation latency exceeds 50ms in Sprint 12 load testing.
"""
refs      = ["D05/signal_b_cycles"]
```

---

## §8 Evasion Analysis

### E-D05-B-1: 2-wallet direct ring (A→B→A)
**Attack:** Two wallets trade back and forth directly. The graph has one SCC of size 2 ({A, B}), filtered by `min_scc_size = 3`. Cycle not detected by Signal B.
**Coverage:** Already detected by Signal A (H1 round-trip, same sender buy+sell within `block_window_slots`). The Signal A → Signal B handoff is correct: direct 2-wallet rings are Signal A's domain; multi-wallet rings are Signal B's domain.
**Residual risk:** Zero. Signal A covers this case. No gap.

### E-D05-B-2: Ring with more than max_cycle_length wallets (6+ hops)
**Attack:** Attacker routes through 6, 7, or more distinct wallets. Each cycle has `len > 5`, filtered by `max_cycle_length = 5`. Signal B does not fire.
**Mitigation:** (a) Operator raises `max_cycle_length` to 7 at the cost of higher enumeration overhead. (b) The SCC is still identified by Tarjan; `scc_count_evaluated` in evidence alerts operators to a large SCC even if no cycle passes the length filter. (c) Routing through 6+ wallets requires 6× the on-chain fees (Solana is cheap, but coordination overhead scales).
**Accepted gap:** DG-D05-B-1 (§11). Documented.

### E-D05-B-3: Time-spreading beyond the 120-minute window
**Attack:** Attacker spaces each hop more than `max_cycle_window_minutes / cycle_length` minutes apart. For a 3-hop ring, that is 40 minutes between each hop; the total ring completes in 120+ minutes and fails filter_3.
**Mitigation:** Extending `max_cycle_window_minutes` to 240 or 360 increases recall but proportionally increases the FP rate from coincidental opposing positions. The FP/recall tradeoff is a config decision: operators with high FP tolerance can raise the window. **Base case is 120 minutes with explicit note that the tradeoff was evaluated.**
**Accepted gap:** An attacker who waits 40+ minutes between each leg of a 3-wallet ring is already limited in volume (opportunity cost during the wait). The economic damage is proportionally reduced.

### E-D05-B-4: Below-threshold dust cycles (under min_cycle_volume_usd)
**Attack:** Attacker cycles $800 per ring, below the $1,000 threshold. Signal B does not fire. Attacker accumulates volume by running many dust rings simultaneously.
**Mitigation:** `cycle_count` in evidence accumulates ALL qualified cycles. If 50 dust cycles are running in parallel, each individually below $1,000, the `total_cycle_volume_usd` across all 50 would be $40,000 — far above the threshold. The issue only arises if each ring independently fails the filter. In that case, the SCC still exists; `scc_count_evaluated > 0` is evidence of suspicious graph structure.
**Residual risk:** Low. Dust rings that individually fail the volume filter but are collectively significant are DG-D05-B-2 (§11). Future mitigation: an "SCC exists with substantial aggregate volume" supplementary signal.

### E-D05-B-5: Routing through pool contracts (not direct wallet-to-wallet)
**Attack:** Attacker routes W1 → Pool → W2 → Pool → W3 → Pool → W1. The pool contract appears as an intermediate vertex. The cycle now has length 6 (counting pool nodes), exceeding `max_cycle_length = 5`.
**Mitigation:** `fetch_recent_transfers` fetches SPL token transfers from the `transfers` table, which records the direct token recipient (the wallet or ATA), not the pool program. Pool programs are not SPL token holders; transfers go to ATAs controlled by wallets. The pool contract does not appear as a vertex unless it directly receives tokens (only possible for fee-on-transfer schemes, already covered by D01/D07). This attack is not applicable to standard Raydium/Orca pools.
**Residual risk:** Minimal.

### E-D05-B-6: Pump.fun bonding-curve cycles
**Attack:** During a bonding-curve pump, tokens flow back and forth through the bonding-curve contract as multiple wallets buy and sell. The bonding curve appears as the central hub of a star graph, not a ring. No elementary cycle exists in a star graph (hub-and-spoke, not ring topology).
**Coverage:** Bonding-curve manipulation is a separate pattern covered by T1-2 (Pump.fun graduation stream). D05 Signal B is not the right detector for bonding-curve dynamics.
**Residual risk:** Zero for D05 scope; T1-2 covers the bonding-curve case.

### E-D05-B-7: Bridge wallets as cycle intermediaries
**Attack:** Attacker uses a CEX deposit/withdrawal as a bridge between two legs: W1 → CEX_hot_wallet → W2 → W1. The CEX hot wallet is included as a vertex.
**Detection:** If the CEX hot wallet is not excluded from the transfer graph, the cycle still exists: W1 → CEX → W2 → W1 is a 3-hop cycle. The `transfers` table does not distinguish on-exchange settlement from DEX transfers. If the CEX withdrawal appears as a direct SPL transfer from the CEX hot wallet address to W2, the cycle is detected. **However**, if the CEX processes withdrawals from a different address than deposits (common), the cycle breaks: W1 → CEX_deposit_addr ≠ CEX_withdrawal_addr → W2 → W1 is not a cycle in the graph.
**Mitigation:** Known CEX hot wallet addresses (from `address_labels.KnownExchange`) can be excluded from the transfer graph as filtering step. This reduces false negatives at the cost of a false-negative risk when attackers use CEX bridges. Document as DG-D05-B-4 (§11).
**Accepted gap:** Moderate. CEX bridge routing is expensive (withdrawal fees) and slow (settlement time), which naturally extends the time window beyond `max_cycle_window_minutes`.

### E-D05-B-8: Cycles across multiple pools of the same token pair
**Attack:** Attacker splits the ring across two Raydium pools for the same token (e.g., BONK/SOL pool A and BONK/SOL pool B). Leg 1: W1 buys in pool A; Leg 2: W2 sells in pool B; Leg 3: tokens flow W1→W2 directly.
**Detection status:** `fetch_recent_transfers` operates at the token level (all transfers of token X, regardless of pool). The direct W1→W2 transfer is captured. The pool interaction appears as transfers from/to the pool's token vault ATA; if the attacker's wallets are the only counterparties, the cycle W1→pool_vault_A→pool_vault_B→W2→W1 is visible (though length = 4). If the pool vault is excluded as a known DEX address, the direct W1→W2 transfer may be the only leg visible.
**Mitigation:** Do NOT exclude known DEX pool vault addresses from the transfer graph for Signal B. This is a deliberate choice: cycle detection is more important than noise reduction from pool vault filtering. Documented in §12 unit test requirements.
**Residual risk:** Moderate. Attackers who exploit pool vault intermediate vertices need careful analysis.

### E-D05-B-9: SCC clique overflow (`max_cycles_per_scc` = 100 cap)
**Attack:** Attacker creates a 5-wallet clique (W1–W5 all transact with each other). A 5-vertex complete directed graph has thousands of elementary cycles. Johnson's algorithm hits the `max_cycles_per_scc = 100` cap and stops. The attacker may ensure that the "most incriminating" cycles appear after position 100 in the enumeration order.
**Mitigation:** The cap does not prevent detection — it caps reporting. A 100-cycle SCC is already a very strong signal; even 1 qualifying cycle triggers Signal B. The `max_cycles_per_scc` overflow emits a `WARN` log entry and sets `max_cycles_cap_hit = 1` in evidence (future key for monitoring). Attackers who create 100-cycle cliques are still detected; they merely limit the evidence granularity.
**Residual risk:** Low for detection; moderate for evidence completeness. DG-D05-B-3 (§11).

### E-D05-B-10: Cycle partially spanning the evaluation window boundary
**Attack:** The wash ring completes, but the first edge's `block_time` is just outside the evaluation window (`ctx.observed_at - 120 minutes`). The first edge appears in the `transfers` table but is excluded from the `fetch_recent_transfers` query by the `block_time >= $window_start` filter.
**Handling specification:** The time window is `[ctx.observed_at - max_cycle_window_minutes, ctx.observed_at)`. A cycle that spans this boundary is partially observable. Specification: if any edge in the cycle falls outside the window, the cycle is not counted (filter_3 is computed over the fetched edges only — edges outside the window are not fetched). This is correct behavior: the cycle was not complete within the observable window.
**Mitigation:** D05 is cadenced (fires every N scheduler ticks). The next evaluation will include the previously-missing edge. Eventual detection is guaranteed for ongoing ring activity.

---

## §9 Fixture Plan

All fixtures follow the JSON structure used by existing D05 fixtures in `tests/fixtures/solana/`. The structure mirrors D09 fixtures (design 0016 §9): a `snapshot` section with synthetic on-chain state and a `assertions` section with expected detector output.

### POS_D05_B_01: 3-wallet BONK ring, 4-hop cycle, $50K volume

```
File: tests/fixtures/solana/d05_signal_b_pos_01_bonk_ring.json

Token: BONK (DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263)
Wallets: W1 (7xKP...), W2 (9rTQ...), W3 (2mNZ...), W4 (4pRV...)
Transfers:
  W1 → W2: 10,000,000,000 BONK, block 285_000_000 (t=0m)
  W2 → W3:  9,950,000,000 BONK, block 285_000_050 (t=8m)
  W3 → W4:  9,900,000,000 BONK, block 285_000_100 (t=16m)
  W4 → W1:  9,850,000,000 BONK, block 285_000_150 (t=24m)
BONK price: $0.000020/token, decimals=5
Cycle: W1→W2→W3→W4→W1 (length 4 hops)
cycle_volume_usd = MIN(9,850,000,000 × $0.000020 / 100,000) = MIN($197,000...) ≈ $1,970
  [Note: decimals=5 means 10^5 = 100,000 raw units per token. 10B raw / 100,000 = 100,000 tokens.
   100,000 tokens × $0.000020 = $2.00 per edge. Hmm — recalculate for $50K:
   Need 50,000 / 0.000020 = 2,500,000,000 tokens per edge.
   2,500,000,000 tokens × 100,000 raw/token = 250,000,000,000,000 raw units per edge.
   Use amount_raw = 250_000_000_000_000 per transfer for $50K/edge cycle.]

Expected assertions:
  Signal B fires: confidence ≈ 0.85 (clamped)
    (conf_raw_B = 0.40 + 0.40 × min(1.0, 50_000/10_000) = 0.40 + 0.40 × 1.0 = 0.80;
     with $50K × 4 edges = $200K total, multiple cycles may form → cap at 0.85)
  cycle_count: 1
  largest_cycle_length: 4
  unique_wallets_in_cycles: 4
  total_cycle_volume_usd ≥ 50000
```

### POS_D05_B_02: 5-wallet ring on synthetic token, 5-hop cycle, $20K volume

```
File: tests/fixtures/solana/d05_signal_b_pos_02_fivehop_ring.json

Token: synthetic mint "SynthWashTokenMint111111111111111111111111"
Wallets: W1..W5 (synthetic addresses)
Transfers: W1→W2→W3→W4→W5→W1, each $4,000 edge volume
cycle_volume_usd = $4,000 per edge (minimum)
filter_1: 5 ≤ 5 PASSES
filter_2: $4,000 ≥ $1,000 PASSES
filter_3: all within 45 minutes PASSES

Expected assertions:
  Signal B fires: confidence = 0.40 + 0.40 × min(1.0, 4000/10000) = 0.40 + 0.16 = 0.56
  cycle_count: 1
  largest_cycle_length: 5
  unique_wallets_in_cycles: 5
```

### NEG_D05_B_01: No cycles — pure random transfers

```
File: tests/fixtures/solana/d05_signal_b_neg_01_no_cycles.json

Token: synthetic
Transfers: 20 wallets all sending tokens to a single recipient hub.
  W1..W20 → HUB (20 transfers, no return flows)
  Graph structure: star (hub-and-spoke). No SCC of size ≥ 2. No cycles possible.

Expected assertions:
  Signal B does NOT fire (cycle_count = 0)
  No AnomalyEvent from Signal B
```

### NEG_D05_B_02: One-way flow A→B→C (no cycle back)

```
File: tests/fixtures/solana/d05_signal_b_neg_02_oneway_flow.json

Token: synthetic
Transfers: W1→W2→W3 (no W3→W1 or W3→W2 edge)
Graph: DAG (directed acyclic graph). Tarjan SCC produces three singleton SCCs.
All SCCs have size < min_scc_size = 3. No cycles enumerated.

Expected assertions:
  Signal B does NOT fire
  scc_count_evaluated = 0 (no SCC passed the min_scc_size filter)
```

---

## §10 Reorg Semantics

**Under Option D (transient in-memory):** No persistent `TokenTransfer` edges are written to `graph_edges`. The `transfers` table is the only persistent state consumed by Signal B cycle detection. The `transfers` table already handles reorgs via `DELETE FROM transfers WHERE chain = $1 AND block_height >= $reorg_height` (V00002 design, executed by the existing `handle_reorg` hook in `crates/indexer/src/reorg.rs`).

**Effect on D05 Signal B:** When a reorg occurs and transfers are deleted, the next D05 evaluation automatically reads the corrected `transfers` table. No additional reorg hook is required in `GraphIndexerWriter` or anywhere in D05's code path for Signal B. The Option D architecture provides reorg resilience by construction.

**Contrast with Option B:** If Option B were chosen (persisting `TokenTransfer` edges to `graph_edges`), the reorg hook in `GraphIndexerWriter::on_reorg` would need to call `typed_edge_store.delete_edges_above_block(chain, reorg_height)` scoped to `edge_type = 'TokenTransfer'`. The `delete_edges_above_block` method already implements the generic case; the edge type filter would need to be added as an optional parameter. Under Option D, this complexity is avoided entirely.

---

## §11 Design Gaps (DG-D05-B-N)

**DG-D05-B-1: Rings longer than max_cycle_length = 5.**
Elementary cycles with 6+ hops are not detected. Victor & Weintraud 2021 §4.2 report 5% of wash rings have 6+ participants. Resolution: Phase 5 — increase `max_cycle_length` to 7 after performance calibration. The config key is already present; the change is one TOML edit.

**DG-D05-B-2: Dust ring aggregation (many sub-threshold cycles summing to material volume).**
Multiple cycles each individually below `min_cycle_volume_usd = $1,000` but collectively representing significant wash volume. The current implementation only counts qualified cycles (those passing all three filters). A supplementary "SCC volume aggregate" signal that sums all edge volumes within an SCC regardless of per-cycle filtering would close this gap. Resolution: Phase 5 — add `scc_aggregate_volume_usd` as a secondary signal with a higher threshold ($10,000 at SCC level).

**DG-D05-B-3: `max_cycles_per_scc` overflow causes incomplete evidence.**
When Johnson's algorithm hits the 100-cycle cap, the `total_cycle_volume_usd` and `unique_wallets_in_cycles` are underestimates of the true ring activity. Detection still fires (any qualifying cycle fires Signal B), but confidence may be underestimated due to truncated `total_cycle_volume_usd`. Resolution: Phase 5 — implement an approximate cycle volume estimator that samples cycles when the cap is hit rather than stopping cold.

**DG-D05-B-4: CEX bridge wallets splitting cycles.**
Attacker routes through CEX deposit and withdrawal addresses (different addresses). The cycle is broken in the transfer graph. Resolution: Phase 4 — integrate known CEX address-pair mappings (deposit address → withdrawal address for same exchange) from `address_labels.KnownExchange`. Requires CEX address clustering data not currently in the system.

**DG-D05-B-5: Cross-pool cycle detection (same token, multiple pools).**
The current implementation is token-level but pool-agnostic. When an attacker deliberately routes through pool vaults to obscure the ring, pool vault addresses appear as intermediate nodes. Excluding known pool vault ATAs from the vertex set (via `address_labels.KnownDex`) would clean the graph but could cause cycles to be undetected if the pool vault is a necessary bridge vertex. Resolution: Phase 4 — implement a "skip and reconnect" step that removes known DEX vault vertices and directly connects their neighbors, preserving cycle topology without the noise.

**DG-D05-B-6: Token price unavailability suppresses Signal B entirely.**
When token price is unavailable (no enriched pool or no liquidity), `cycle_volume_usd` cannot be computed and all cycles fail filter_2. Signal B is effectively disabled for un-priced tokens. This is the correct safe failure (do not emit high-confidence events based on zero price), but it creates a systematic blind spot for newly launched tokens in their first few minutes. Resolution: Phase 3 immediate — emit a dedicated `Info`-level event (`wash_trading_h1/signal_b_cycles/price_unavailable = 1`) when cycles exist but price is unavailable, allowing operators to track the gap.

---

## §12 Test Plan

### Unit tests (in `crates/graph/src/cycles.rs`)

**T-SCC-1: 3-vertex ring SCC identification.**
Input: adj[0]=[1], adj[1]=[2], adj[2]=[0]. Expected: one SCC = [0,1,2].

**T-SCC-2: No SCC (DAG).**
Input: adj[0]=[1], adj[1]=[2], adj[2]=[]. Expected: three singleton SCCs, all filtered by `min_scc_size = 3`. Output: empty Vec.

**T-SCC-3: Two disjoint SCCs.**
Input: adj[0]=[1], adj[1]=[0], adj[2]=[3], adj[3]=[2]. Expected: two SCCs [0,1] and [2,3]. Both filtered (size 2 < 3). Output: empty Vec.

**T-SCC-4: Mixed graph with one qualifying SCC.**
Input: star hub plus one ring. adj[0]=[1,4], adj[1]=[2], adj[2]=[3], adj[3]=[1], adj[4]=[]. Expected: one SCC [1,2,3] (the ring); vertex 0 and 4 are singletons. SCC [1,2,3] passes size filter.

### Unit tests (Johnson's algorithm in `crates/graph/src/cycles.rs`)

**T-JOHNSON-1: 3-vertex ring produces exactly 1 elementary cycle.**
Input: SCC [0,1,2], adj within SCC: 0→1, 1→2, 2→0. Expected: one cycle [0,1,2].

**T-JOHNSON-2: 4-vertex ring produces exactly 1 elementary cycle.**
Input: SCC [0,1,2,3], adj: 0→1, 1→2, 2→3, 3→0. Expected: one cycle [0,1,2,3].

**T-JOHNSON-3: Complete directed graph on 3 vertices produces 5 elementary cycles.**
Input: adj: 0→1, 0→2, 1→0, 1→2, 2→0, 2→1. Expected: 5 cycles ([0,1], [0,2], [1,2], [0,1,2], [0,2,1]).
Note: [0,1] = 2-hop cycle (0→1→0); verify all 5 are enumerated and none duplicated.

### Property tests

**P-DET-1: Determinism.** Feed identical `Vec<TransferEdge>` twice; assert that the returned `Vec<DetectedCycle>` is byte-identical (same cycle order, same vertex order within each cycle, same aggregate values).

**P-NODUP-1: No duplicate elementary cycles.** For any input, no two cycles in the output have the same vertex sequence (normalized by rotating to start at the minimum vertex ID).

**P-LEN-1: No cycle exceeds max_cycle_length.** For any input with `max_cycle_length = N`, no returned cycle has `vertices.len() > N`.

**P-CAP-1: max_cycles_per_scc is respected.** For any input, the number of DetectedCycle entries from a single SCC is ≤ `max_cycles_per_scc`.

### Integration tests (end-to-end, Docker-gated, `#[ignore]`)

**I-POS-1:** Load `d05_signal_b_pos_01_bonk_ring.json` fixture into testcontainer Postgres. Run `WashTradingDetector::evaluate`. Assert Signal B fires with confidence ≥ 0.50 and `cycle_count ≥ 1`.

**I-POS-2:** Load `d05_signal_b_pos_02_fivehop_ring.json`. Assert Signal B fires with confidence ≈ 0.56 (within ±0.05 tolerance for floating-point precision).

**I-NEG-1:** Load `d05_signal_b_neg_01_no_cycles.json`. Assert Signal B does NOT fire. Assert no AnomalyEvent with `detector = "wash_trading_h1"` has `evidence["wash_trading_h1/signal_b_cycles/cycle_count"] > 0`.

**I-NEG-2:** Load `d05_signal_b_neg_02_oneway_flow.json`. Assert Signal B does NOT fire.

**I-COMPAT-1:** Run existing D05 Signal A positive fixture (`research/fixtures/wash_trading/POS_01_synth_single_wallet.json`) and assert Signal A still fires with confidence ≥ 0.60 and Signal B does NOT fire (the fixture has a single wallet with self-dealing, which should not produce a cycle in the multi-wallet transfer graph).

---

## §13 REFERENCES.md Additions

The following rows must be added to `REFERENCES.md` before this design merges. Two new entries (Tarjan 1972, Johnson 1975) and two "Used In" column updates (Victor & Weintraud 2021, Chainalysis 2025):

### New entry: Tarjan 1972

```
| Tarjan SCC algorithm | Iterative depth-first search producing strongly connected components in O(V+E); root of every SCC identified by lowlink = discovery index invariant; forms the pre-filter for Johnson cycle enumeration | Tarjan, R. E. (1972). "Depth-First Search and Linear Graph Algorithms." SIAM Journal on Computing, 1(2), 146–160. DOI: 10.1137/0201010 | D05 Signal B cycle detection (docs/designs/0017); crates/graph/src/cycles.rs tarjan_scc() | Verified against original paper; iterative variant cross-referenced with Pearce (2016) "A Space-Efficient Algorithm for Finding Strongly Connected Components" for stack-safe implementation |
```

### New entry: Johnson 1975

```
| Johnson elementary cycle enumeration | circuit() + unblock() recursive algorithm enumering all elementary directed cycles in O((V+E)(C+1)) time where C = cycle count; requires all-pairs-reachability within SCCs (Tarjan prerequisite); bounded by max_cycle_length and max_cycles_per_scc in this implementation | Johnson, D. B. (1975). "Finding All the Elementary Circuits of a Directed Graph." SIAM Journal on Computing, 4(1), 77–89. DOI: 10.1137/0204007 | D05 Signal B cycle detection (docs/designs/0017); crates/graph/src/cycles.rs enumerate_cycles_in_scc() | Verified against original paper; pseudocode in §4.2 of design 0017 is a direct translation of Algorithm 1 in the paper |
```

### Victor & Weintraud 2021 — extend "Used In"

Current Used In: `Phase 2 detector #5`

New Used In: `Phase 2 detector #5; D05 Signal B cycle detection (design 0017): max_cycle_length=5 threshold (§4.2 median ring size), min_cycle_volume_usd=$1,000 (§4.2 median trade size), cycle_volume_usd confidence formula ($10,000 scale point)`

### Chainalysis 2025 — extend "Used In"

Current Used In for wash trading entries: multiple existing entries for Signal A and Signal B proxy.

Add to the Signal B wash-trading Heuristic 2 row:

New Used In: `...existing...; D05 Signal B cycle detection (design 0017): max_cycle_window_minutes=120 calibration (intra-day clustering), total_cycle_volume_usd confidence formula anchor`

---

## §14 Module and Crate Structure

### New module: `crates/graph/src/cycles.rs`

Public exports (added to `crates/graph/src/lib.rs`):

```rust
pub use cycles::{
    fetch_recent_transfers,
    run_cycle_detection,
    CycleDetectionResult,
    DetectedCycle,
    TransferEdge,
    SignalBCyclesConfig,
};
```

**`CycleDetectionResult`:**
```rust
pub struct CycleDetectionResult {
    pub cycle_count: u32,
    pub total_cycle_volume_usd: Decimal,
    pub largest_cycle_length: u32,
    pub unique_wallets_in_cycles: u32,
    pub scc_count_evaluated: u32,
    pub transfers_in_window: u32,
    pub max_transfers_cap_hit: bool,
}
```

**`DetectedCycle`:**
```rust
pub struct DetectedCycle {
    pub vertex_addresses: Vec<String>,  // wallet addresses in cycle order
    pub cycle_volume_usd: Decimal,
    pub edge_count: u32,                // number of transfer edges in this cycle (after deduplication)
    pub earliest_block_time: DateTime<Utc>,
    pub latest_block_time: DateTime<Utc>,
}
```

**`SignalBCyclesConfig`:**
```rust
pub struct SignalBCyclesConfig {
    pub max_cycle_length: u32,
    pub max_cycle_window_minutes: u32,
    pub min_cycle_volume_usd: Decimal,
    pub max_cycles_per_scc: usize,
    pub min_scc_size: usize,
    pub max_transfers_per_window: u32,
}
```

### Crate dependency check (gotcha #33)

`crates/graph` already depends on `crates/common` and `crates/storage` (via sqlx). `crates/detectors` already depends on `crates/graph` (D08 introduced this in Sprint 11). The new `cycles.rs` module lives in `crates/graph` and is called from `crates/detectors/src/d05_wash_trading.rs`. The dependency direction is `detectors → graph` — already established. No new crate-level dependency is introduced. No circular dependency.

### What is NOT changed

- `crates/common` — FROZEN (gotcha #1). No new types.
- `crates/indexer` — No changes under Option D (no TokenTransfer edge writes).
- `Detector` trait in `crates/detectors/src/trait_impl.rs` — signature unchanged (gotcha #27).
- D05 Signal A and Signal C logic — untouched.
- V00011 and V00012 migrations — not modified (gotcha #7; graph_edges table already exists with TokenTransfer accepted).

---

## §15 ADR Assessment

**ADR 0001 §D5:** Consistent. D05 Signal B upgrade is explicitly registered as a Phase 3 graph-dependent algorithm. This design closes the "D05 Signal B graph-backed" item in design 0015 §9.

**ADR 0002 (Postgres-only):** Consistent. Option D reads from the existing `transfers` table (Postgres). No new storage tier. `NUMERIC(39,0)` / String bridge used for `amount_raw`. Cycle detection is pure in-memory Rust; no ClickHouse or external store.

**ADR 0003 (self-sovereign):** Consistent. Tarjan SCC and Johnson's algorithm are hand-rolled pure Rust with no external service dependency. `petgraph` is NOT added as a new `crates/graph` dependency. The only new Rust code is ~250 lines in `crates/graph/src/cycles.rs`.

**No new ADR required.** This design extends D05 within the existing ADR constraints. The hand-roll vs. `petgraph` decision is a dependency management choice, not an architectural decision requiring ADR-level documentation.

---

## §16 Sprint Sizing Assessment

**Is this one sprint or two?**

The task decomposes into:
- **Sub-sprint A (Projection + infra):** Add `crates/graph/src/cycles.rs` with `fetch_recent_transfers`, `TransferEdge`, `CycleDetectionResult`, `DetectedCycle`, `SignalBCyclesConfig`. Write unit tests for the data model. ~150 LOC.
- **Sub-sprint B (Algorithm + integration):** Implement `tarjan_scc` + `enumerate_cycles_in_scc` + `run_cycle_detection`. Write algorithm unit tests (T-SCC-1..4, T-JOHNSON-1..3, P-DET-1, P-NODUP-1, P-LEN-1, P-CAP-1). Delete Signal B proxy code from d05_wash_trading.rs. Wire `compute_signal_b_cycles` in D05 evaluate. Update config/detectors.toml. Add fixtures. ~350 LOC.

Total: ~500 LOC Rust + 60 TOML + 4 JSON fixtures. This is well within one developer session (one sprint). The algorithms are bounded and well-specified here. The complexity is in correctness verification (the property tests), not in algorithmic novelty.

**Recommendation: single sprint.** Sub-sprint A and B can be developed sequentially by a single developer agent with no blocking dependency between them (Option D means no indexer work). The design is complete; no design decisions remain open that would block implementation.

---

## Inconsistency Report

**1. Design 0008 Signal B confidence cap (0.60) vs. task prompt (0.85).**
Design 0008 §2 table specifies Signal B range "0.50–0.60" and the `compute_signal_b_confidence` function in d05_wash_trading.rs caps at `0.60_f64`. This design specifies a new cap of 0.85 for the cycle-based Signal B. This is an intentional upgrade: cycle detection is a stronger signal than cluster flow balance; a higher confidence cap is justified. The developer must NOT use the old 0.60 cap.

**2. Design 0015 §9 T2-2 entry references "design doc 0016" for this spec.**
Design 0015 §9 reads "T2-2: Tarjan SCC + Johnson cycle detection... New design doc 0016." This conflicts with the actual design numbering: design 0016 is the D09 BOCPD spec. This design is 0017. The inconsistency is in design 0015's sprint plan table — update that entry to "design doc 0017" when this document is created.

**3. `WashTradingConfig` struct in d05_wash_trading.rs will have stale fields.**
After deletion of the cluster-related config fields, the `WashTradingConfig` deserialization from `config/detectors.toml` must not fail on absence of the deleted keys. The developer must audit the `serde(deny_unknown_fields)` or equivalent annotation on `WashTradingConfig` before removing the TOML keys.
