//! Graph cycle detection for D05 Signal B upgrade (T2-2).
//!
//! Implements Tarjan SCC (1972) + Johnson elementary cycle enumeration (1975).
//! Hand-rolled to avoid `petgraph` dep; deterministic; bounded by
//! `max_cycle_length` to prevent stack overflow.
//!
//! # Design reference
//!
//! `docs/designs/0017-d05-signal-b-graph-cycles.md` §3 + §4
//!
//! # Citations
//!
//! - Tarjan, R. E. (1972). "Depth-First Search and Linear Graph Algorithms."
//!   SIAM Journal on Computing, 1(2), 146–160. DOI: 10.1137/0201010
//! - Johnson, D. B. (1975). "Finding All the Elementary Circuits of a Directed Graph."
//!   SIAM Journal on Computing, 4(1), 77–89. DOI: 10.1137/0204007
//! - Victor & Weintraud (2021). "Detecting and Quantifying Wash Trading on
//!   Decentralized Cryptocurrency Exchanges." https://arxiv.org/abs/2102.07001
//!
//! # Determinism guarantee
//!
//! Given the same `Vec<TransferEdge>` input (ordered by block_height ASC, tx_hash ASC),
//! `detect_cycles` always produces bit-identical output. The adjacency list is sorted
//! after construction; Tarjan SCC output is sorted; Johnson iterates in sorted order.
//! No `HashMap` appears in the output path (only in the internal vertex-id lookup).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::Row as _;
use tracing::{instrument, warn};

use crate::error::GraphError;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// One directed edge from the `transfers` table projected for cycle detection.
///
/// Multiple transfers between the same `(from_address, to_address)` pair are
/// collapsed by `detect_cycles` into a single logical edge (amount_raw summed).
#[derive(Debug, Clone)]
pub struct TransferEdge {
    /// Sender address (canonical for chain, e.g. Base58 for Solana).
    pub from_address: String,
    /// Recipient address (canonical for chain).
    pub to_address: String,
    /// Raw token amount in the token's native unit (NUMERIC(39,0) String bridge).
    pub amount_raw: u128,
    /// Block timestamp. Derived from `block_time` column, NEVER `Utc::now()`.
    pub block_time: DateTime<Utc>,
    /// Block height. Used for transfer ordering and cycle time-window filter.
    pub block_height: u64,
}

/// One elementary directed cycle detected in the transfer graph.
///
/// A cycle of length N has N vertices (wallets) and N directed edges,
/// where the last vertex connects back to the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    /// Addresses participating in the cycle, in traversal order.
    /// Length = number of distinct wallets = number of hops.
    pub vertices: Vec<String>,
    /// Per-edge raw token amounts, in traversal order. Edge `i` is
    /// `vertices[i] → vertices[(i + 1) % N]`. Length equals `vertices.len()`.
    /// Each element is the post-deduplication `amount_raw_sum` for that directed
    /// pair over the observation window. Callers derive bottleneck USD volume
    /// by computing `MIN(per_edge_amounts_raw[i] * price / 10^decimals)` —
    /// matches spec 0017 §5.1 (bottleneck edge, not average).
    pub per_edge_amounts_raw: Vec<u128>,
    /// Time span in minutes: `(max(block_time) - min(block_time)).num_minutes()`.
    pub block_time_span_minutes: u64,
}

/// Configuration for cycle detection (mirrors `[wash_trading_h1.signal_b_cycles]` TOML).
#[derive(Debug, Clone)]
pub struct CycleDetectionConfig {
    /// Maximum cycle length (number of hops / wallets). Default: 5.
    pub max_cycle_length: usize,
    /// Maximum cycles to enumerate per SCC. Default: 100.
    pub max_cycles_per_scc: usize,
    /// Minimum SCC size to pass to Johnson's algorithm. Default: 3.
    pub min_scc_size: usize,
}

impl Default for CycleDetectionConfig {
    fn default() -> Self {
        Self {
            max_cycle_length: 5,
            max_cycles_per_scc: 100,
            min_scc_size: 3,
        }
    }
}

// ---------------------------------------------------------------------------
// fetch_recent_transfers
// ---------------------------------------------------------------------------

/// Read transfers for a token over `[window_start, window_end)` from the
/// `transfers` table.
///
/// Implementation queries `transfers` directly — Option D per spec §2.
/// No writes to `graph_edges`. Self-transfers are excluded at the SQL level.
///
/// Results ordered by `block_height ASC, tx_hash ASC` for determinism.
///
/// # Safety cap
///
/// Hard-caps at `max_transfers` rows to prevent OOM on tokens with extreme
/// transfer volume. Hitting the cap logs a `WARN` and returns the capped set;
/// cycle detection continues on the available data.
#[instrument(skip(pool), fields(chain, token, window_start = %window_start, window_end = %window_end))]
pub async fn fetch_recent_transfers(
    pool: &sqlx::PgPool,
    chain: &str,
    token_mint: &str,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    max_transfers: u32,
) -> Result<Vec<TransferEdge>, GraphError> {
    let rows = sqlx::query(
        r#"
        SELECT from_address,
               to_address,
               amount_raw::TEXT AS amount_raw_str,
               block_time,
               block_height
        FROM transfers
        WHERE chain      = $1
          AND token      = $2
          AND block_time >= $3
          AND block_time <  $4
          AND from_address <> to_address
          AND from_address <> '11111111111111111111111111111111'
          AND to_address   <> '11111111111111111111111111111111'
        ORDER BY block_height ASC, tx_hash ASC
        LIMIT $5
        "#,
    )
    .bind(chain)
    .bind(token_mint)
    .bind(window_start)
    .bind(window_end)
    .bind(max_transfers as i64)
    .fetch_all(pool)
    .await
    .map_err(GraphError::Database)?;

    if rows.len() == max_transfers as usize {
        warn!(
            chain,
            token = token_mint,
            cap = max_transfers,
            "fetch_recent_transfers hit max_transfers cap; cycle detection uses capped set"
        );
    }

    let mut edges = Vec::with_capacity(rows.len());
    for row in &rows {
        let from_address: String = row
            .try_get("from_address")
            .map_err(|e| GraphError::ParseField {
                field: "from_address",
                reason: e.to_string(),
            })?;
        let to_address: String = row
            .try_get("to_address")
            .map_err(|e| GraphError::ParseField {
                field: "to_address",
                reason: e.to_string(),
            })?;
        let amount_raw_str: String = row
            .try_get("amount_raw_str")
            .map_err(|e| GraphError::ParseField {
                field: "amount_raw",
                reason: e.to_string(),
            })?;
        let amount_raw = amount_raw_str.parse::<u128>().map_err(|e| {
            GraphError::ParseField {
                field: "amount_raw",
                reason: format!("parse u128: {e}"),
            }
        })?;
        let block_time: DateTime<Utc> = row
            .try_get("block_time")
            .map_err(|e| GraphError::ParseField {
                field: "block_time",
                reason: e.to_string(),
            })?;
        let block_height_i64: i64 = row
            .try_get("block_height")
            .map_err(|e| GraphError::ParseField {
                field: "block_height",
                reason: e.to_string(),
            })?;

        edges.push(TransferEdge {
            from_address,
            to_address,
            amount_raw,
            block_time,
            block_height: block_height_i64 as u64,
        });
    }

    Ok(edges)
}

// ---------------------------------------------------------------------------
// detect_cycles — public entry point
// ---------------------------------------------------------------------------

/// Detect elementary cycles in the directed transfer graph.
///
/// # Algorithm
///
/// 1. Build a directed graph from `edges`: assign each unique address a vertex
///    ID; deduplicate `(from, to)` pairs (sum `amount_raw`).
/// 2. Sort adjacency lists for determinism.
/// 3. Run iterative Tarjan SCC (Tarjan 1972).
/// 4. For each SCC with `|V| >= cfg.min_scc_size`, run Johnson's algorithm
///    bounded by `cfg.max_cycle_length` and `cfg.max_cycles_per_scc`.
/// 5. Return `Vec<Cycle>` with `per_edge_amounts_raw` populated per edge;
///    callers compute bottleneck USD volume as
///    `MIN(per_edge_amounts_raw[i] * price / 10^decimals)`.
///
/// # Determinism
///
/// Given the same input (ordered by block_height ASC, tx_hash ASC as returned
/// by `fetch_recent_transfers`), output is bit-identical on replay.
pub fn detect_cycles(edges: &[TransferEdge], cfg: &CycleDetectionConfig) -> Vec<Cycle> {
    if edges.is_empty() {
        return Vec::new();
    }

    // -----------------------------------------------------------------------
    // Step 1: Build vertex table and adjacency list.
    // -----------------------------------------------------------------------

    // address → vertex id; assigned in first-seen order (input is ordered by
    // block_height ASC, tx_hash ASC — deterministic from fetch).
    let mut addr_to_id: HashMap<String, usize> = HashMap::new();
    let mut id_to_addr: Vec<String> = Vec::new();

    let mut get_or_insert = |addr: &str| -> usize {
        if let Some(&id) = addr_to_id.get(addr) {
            return id;
        }
        let id = id_to_addr.len();
        addr_to_id.insert(addr.to_owned(), id);
        id_to_addr.push(addr.to_owned());
        id
    };

    // Aggregated edge data: (from_id, to_id) → (sum of amount_raw, min block_time, max block_time)
    // Using a Vec to preserve insertion ordering for later adjacency sort.
    let mut edge_map: HashMap<(usize, usize), AggEdgeData> = HashMap::new();

    for te in edges {
        let from_id = get_or_insert(&te.from_address);
        let to_id = get_or_insert(&te.to_address);
        if from_id == to_id {
            continue; // skip self-loops (redundant with SQL filter, but be safe)
        }
        let entry = edge_map.entry((from_id, to_id)).or_insert(AggEdgeData {
            amount_raw_sum: 0,
            min_block_time: te.block_time,
            max_block_time: te.block_time,
        });
        entry.amount_raw_sum = entry.amount_raw_sum.saturating_add(te.amount_raw);
        if te.block_time < entry.min_block_time {
            entry.min_block_time = te.block_time;
        }
        if te.block_time > entry.max_block_time {
            entry.max_block_time = te.block_time;
        }
    }

    let v_count = id_to_addr.len();
    if v_count == 0 {
        return Vec::new();
    }

    // Build adjacency list (sorted for determinism).
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); v_count];
    for &(from_id, to_id) in edge_map.keys() {
        adj[from_id].push(to_id);
    }
    for neighbors in adj.iter_mut() {
        neighbors.sort_unstable();
        neighbors.dedup();
    }

    // -----------------------------------------------------------------------
    // Step 2: Tarjan SCC (iterative).
    // -----------------------------------------------------------------------

    let sccs = tarjan_scc(&adj, v_count);

    // -----------------------------------------------------------------------
    // Step 3: Filter SCCs and run Johnson's algorithm.
    // -----------------------------------------------------------------------

    let mut all_cycles: Vec<Cycle> = Vec::new();

    for scc in &sccs {
        if scc.len() < cfg.min_scc_size {
            continue;
        }

        let detected = enumerate_cycles_in_scc(scc, &adj, &edge_map, &id_to_addr, cfg);
        all_cycles.extend(detected);
    }

    all_cycles
}

// ---------------------------------------------------------------------------
// Internal data structures
// ---------------------------------------------------------------------------

/// Aggregated data for a deduplicated (from, to) edge pair.
#[derive(Debug, Clone)]
struct AggEdgeData {
    amount_raw_sum: u128,
    min_block_time: DateTime<Utc>,
    max_block_time: DateTime<Utc>,
}

/// State for one vertex during Tarjan SCC.
#[derive(Debug, Clone)]
struct TarjanNode {
    /// DFS discovery index; `None` = unvisited.
    index: Option<u32>,
    lowlink: u32,
    on_stack: bool,
}

// ---------------------------------------------------------------------------
// Tarjan SCC — iterative (spec §3.3)
// ---------------------------------------------------------------------------

/// Compute strongly connected components using iterative Tarjan DFS.
///
/// Returns SCCs sorted descending by size, then ascending by minimum vertex ID
/// for full determinism. Each SCC is a `Vec<usize>` of vertex IDs sorted ascending.
///
/// # Safety
///
/// Iterative (no recursion) — safe for large V without stack overflow.
fn tarjan_scc(adj: &[Vec<usize>], v_count: usize) -> Vec<Vec<usize>> {
    let mut state: Vec<TarjanNode> = vec![
        TarjanNode {
            index: None,
            lowlink: 0,
            on_stack: false,
        };
        v_count
    ];
    let mut stack: Vec<usize> = Vec::new();
    let mut index_counter: u32 = 0;
    let mut sccs: Vec<Vec<usize>> = Vec::new();

    for start in 0..v_count {
        if state[start].index.is_some() {
            continue;
        }

        // DFS stack: (vertex, current_edge_index_in_adj[vertex])
        let mut dfs_stack: Vec<(usize, usize)> = vec![(start, 0)];

        // Initialise start node.
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
                    // Tree edge: descend into w.
                    state[w].index = Some(index_counter);
                    state[w].lowlink = index_counter;
                    index_counter += 1;
                    stack.push(w);
                    state[w].on_stack = true;
                    dfs_stack.push((w, 0));
                } else if state[w].on_stack {
                    // Back edge: update lowlink of v.
                    let w_index = state[w].index.unwrap_or(0);
                    if w_index < state[v].lowlink {
                        state[v].lowlink = w_index;
                    }
                }
                // Cross/forward edge — skip.
            } else {
                // All neighbours of v processed.
                dfs_stack.pop();

                if let Some(&(parent, _)) = dfs_stack.last() {
                    // Propagate lowlink upward.
                    let v_lowlink = state[v].lowlink;
                    if v_lowlink < state[parent].lowlink {
                        state[parent].lowlink = v_lowlink;
                    }
                }

                // SCC root check.
                if state[v].lowlink == state[v].index.unwrap_or(u32::MAX) {
                    let mut scc: Vec<usize> = Vec::new();
                    loop {
                        let w = stack.pop().unwrap_or(v);
                        state[w].on_stack = false;
                        scc.push(w);
                        if w == v {
                            break;
                        }
                    }
                    scc.sort_unstable();
                    sccs.push(scc);
                }
            }
        }
    }

    // Sort: descending by size, then ascending by first vertex ID.
    sccs.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].cmp(&b[0])));
    sccs
}

// ---------------------------------------------------------------------------
// Johnson's algorithm — elementary cycle enumeration (spec §4.2)
// ---------------------------------------------------------------------------

/// Enumerate all elementary cycles in the subgraph induced by one SCC.
///
/// Uses the Johnson 1975 `circuit` + `unblock` pattern. Bounded by
/// `cfg.max_cycle_length` (limits recursion depth) and `cfg.max_cycles_per_scc`
/// (limits total output). The recursion depth is at most `max_cycle_length = 5`
/// — no stack overflow risk.
fn enumerate_cycles_in_scc(
    scc_vertices: &[usize],
    adj: &[Vec<usize>],
    edge_map: &HashMap<(usize, usize), AggEdgeData>,
    id_to_addr: &[String],
    cfg: &CycleDetectionConfig,
) -> Vec<Cycle> {
    // Map vertex id → position in scc_vertices (or usize::MAX if not in SCC).
    let max_v = scc_vertices.iter().copied().max().unwrap_or(0);
    let mut scc_pos: Vec<usize> = vec![usize::MAX; max_v + 1];
    for (pos, &v) in scc_vertices.iter().enumerate() {
        scc_pos[v] = pos;
    }
    let n = scc_vertices.len();

    let mut detected: Vec<Cycle> = Vec::new();
    let mut blocked: Vec<bool> = vec![false; n];
    let mut blockers: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut path: Vec<usize> = Vec::new();

    // s iterates over vertex positions in ascending SCC position (determinism).
    for s_pos in 0..n {
        let s = scc_vertices[s_pos];

        // Reset blocked for positions >= s_pos.
        for i in s_pos..n {
            blocked[i] = false;
            blockers[i].clear();
        }

        circuit(
            s,
            s,
            s_pos,
            scc_vertices,
            &scc_pos,
            adj,
            &mut path,
            &mut blocked,
            &mut blockers,
            &mut detected,
            edge_map,
            id_to_addr,
            cfg,
        );

        if detected.len() >= cfg.max_cycles_per_scc {
            break;
        }
    }

    detected
}

/// Recursive circuit function (Johnson 1975 Algorithm 1).
///
/// Bounded by `cfg.max_cycle_length` — recursion depth ≤ max_cycle_length = 5.
///
/// Returns `true` when at least one cycle was found through the current path.
#[allow(clippy::too_many_arguments, clippy::only_used_in_recursion)]
fn circuit(
    v: usize,
    s: usize,
    s_pos: usize,
    scc_vertices: &[usize],
    scc_pos: &[usize],
    adj: &[Vec<usize>],
    path: &mut Vec<usize>,
    blocked: &mut Vec<bool>,
    blockers: &mut Vec<Vec<usize>>,
    detected: &mut Vec<Cycle>,
    edge_map: &HashMap<(usize, usize), AggEdgeData>,
    id_to_addr: &[String],
    cfg: &CycleDetectionConfig,
) -> bool {
    // Hard stop: max_cycle_length bounds the path length.
    if path.len() >= cfg.max_cycle_length {
        return false;
    }
    // Hard stop: max_cycles_per_scc cap.
    if detected.len() >= cfg.max_cycles_per_scc {
        return false;
    }

    path.push(v);
    let v_pos = if v < scc_pos.len() {
        scc_pos[v]
    } else {
        usize::MAX
    };
    if v_pos < blocked.len() {
        blocked[v_pos] = true;
    }

    let mut found_cycle = false;

    for &w in &adj[v] {
        // Only follow edges within the SCC.
        let w_pos = if w < scc_pos.len() {
            scc_pos[w]
        } else {
            usize::MAX
        };
        if w_pos == usize::MAX || w_pos >= scc_vertices.len() {
            continue;
        }
        // Only follow edges to vertices with SCC position >= s_pos (Johnson §1 key property).
        if scc_vertices[w_pos] < s {
            continue;
        }

        if w == s {
            // Cycle found: path + back-edge to s.
            let cycle_len = path.len();
            // A valid cycle has at least 2 edges (3 vertices with the back edge to s).
            // path.len() is the number of distinct vertices (not counting s at the end).
            // Minimum meaningful cycle: 2 vertices in path (path=[v1, v2]) → 3-hop ring v1→v2→s→v1.
            // But per spec §3.4, min_scc_size=3 already ensures 3+ vertices.
            // Accept any path of length >= 2 (2 vertices + back-edge = 3-hop cycle).
            if cycle_len >= 2
                && let Some(cycle) = build_cycle(path, s, edge_map, id_to_addr)
            {
                detected.push(cycle);
            }
            found_cycle = true;
        } else if w_pos < blocked.len()
            && !blocked[w_pos]
            && circuit(
                w, s, s_pos, scc_vertices, scc_pos, adj, path, blocked, blockers, detected,
                edge_map, id_to_addr, cfg,
            )
        {
            found_cycle = true;
        }

        // Check cap again after recursive call.
        if detected.len() >= cfg.max_cycles_per_scc {
            break;
        }
    }

    if found_cycle {
        unblock(v_pos, blocked, blockers);
    } else {
        // Add v_pos to blockers of all eligible successors.
        for &w in &adj[v] {
            let w_pos = if w < scc_pos.len() {
                scc_pos[w]
            } else {
                usize::MAX
            };
            if w_pos == usize::MAX || w_pos >= scc_vertices.len() {
                continue;
            }
            if scc_vertices[w_pos] < s {
                continue;
            }
            if v_pos < blockers.len() && !blockers[w_pos].contains(&v_pos) {
                blockers[w_pos].push(v_pos);
            }
        }
    }

    path.pop();
    found_cycle
}

/// Johnson's `unblock` routine: clear the blocked flag and recursively unblock
/// all vertices that were blocked by this vertex.
fn unblock(u_pos: usize, blocked: &mut Vec<bool>, blockers: &mut Vec<Vec<usize>>) {
    if u_pos >= blocked.len() {
        return;
    }
    blocked[u_pos] = false;
    let us_blockers: Vec<usize> = blockers[u_pos].drain(..).collect();
    for w_pos in us_blockers {
        if w_pos < blocked.len() && blocked[w_pos] {
            unblock(w_pos, blocked, blockers);
        }
    }
}

// ---------------------------------------------------------------------------
// Cycle builder
// ---------------------------------------------------------------------------

/// Construct a `Cycle` from the current DFS path and back-edge to `s`.
///
/// `path` = the ordered list of vertex IDs on the current path (not including `s` again).
/// The cycle is: `path[0] → path[1] → … → path[n-1] → s = path[0]`.
///
/// Returns `None` if any required edge is missing from `edge_map` (defensive).
fn build_cycle(
    path: &[usize],
    s: usize,
    edge_map: &HashMap<(usize, usize), AggEdgeData>,
    id_to_addr: &[String],
) -> Option<Cycle> {
    let cycle_len = path.len(); // number of vertices in the ring

    // Build address list and per-edge amounts in traversal order.
    let mut vertices: Vec<String> = Vec::with_capacity(cycle_len);
    let mut per_edge_amounts_raw: Vec<u128> = Vec::with_capacity(cycle_len);
    let mut min_block_time: Option<DateTime<Utc>> = None;
    let mut max_block_time: Option<DateTime<Utc>> = None;

    // Edges: path[0]→path[1], path[1]→path[2], …, path[n-1]→s
    for (i, &v) in path.iter().enumerate() {
        vertices.push(id_to_addr.get(v)?.to_owned());

        let next = if i + 1 < cycle_len { path[i + 1] } else { s };
        let edge_data = edge_map.get(&(v, next))?;

        per_edge_amounts_raw.push(edge_data.amount_raw_sum);

        match min_block_time {
            None => min_block_time = Some(edge_data.min_block_time),
            Some(t) if edge_data.min_block_time < t => min_block_time = Some(edge_data.min_block_time),
            _ => {}
        }
        match max_block_time {
            None => max_block_time = Some(edge_data.max_block_time),
            Some(t) if edge_data.max_block_time > t => max_block_time = Some(edge_data.max_block_time),
            _ => {}
        }
    }

    let span_minutes = match (min_block_time, max_block_time) {
        (Some(min_t), Some(max_t)) => {
            let diff = max_t.signed_duration_since(min_t);
            (diff.num_seconds().max(0) as u64) / 60
        }
        _ => 0,
    };

    Some(Cycle {
        vertices,
        per_edge_amounts_raw,
        block_time_span_minutes: span_minutes,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn t(offset_seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + offset_seconds, 0)
            .unwrap()
    }

    fn edge(from: &str, to: &str, amount: u128, offset_s: i64) -> TransferEdge {
        TransferEdge {
            from_address: from.to_owned(),
            to_address: to.to_owned(),
            amount_raw: amount,
            block_time: t(offset_s),
            block_height: offset_s as u64,
        }
    }

    fn cfg_default() -> CycleDetectionConfig {
        CycleDetectionConfig::default()
    }

    // -----------------------------------------------------------------------
    // Tarjan SCC correctness
    // -----------------------------------------------------------------------

    #[test]
    fn tarjan_scc_correctness_simple_triangle() {
        // Graph: 0→1, 1→2, 2→0 (one SCC of size 3)
        let adj = vec![
            vec![1],    // 0 → 1
            vec![2],    // 1 → 2
            vec![0],    // 2 → 0
        ];
        let sccs = tarjan_scc(&adj, 3);
        assert_eq!(sccs.len(), 1, "exactly one SCC expected");
        assert_eq!(sccs[0].len(), 3, "SCC must contain all 3 vertices");
        assert_eq!(sccs[0], vec![0, 1, 2]);
    }

    #[test]
    fn tarjan_scc_correctness_two_components() {
        // 0→1→2→0 (SCC A); 3→4→3 (SCC B); 2→3 (bridge between them)
        let adj = vec![
            vec![1],    // 0 → 1
            vec![2],    // 1 → 2
            vec![0, 3], // 2 → 0 (closing triangle) + bridge to 3
            vec![4],    // 3 → 4
            vec![3],    // 4 → 3
        ];
        let sccs = tarjan_scc(&adj, 5);
        assert_eq!(sccs.len(), 2, "two SCCs expected");
        // Larger first.
        assert_eq!(sccs[0].len(), 3);
        assert_eq!(sccs[1].len(), 2);
        // Verify SCC membership.
        assert!(sccs[0].contains(&0) && sccs[0].contains(&1) && sccs[0].contains(&2));
        assert!(sccs[1].contains(&3) && sccs[1].contains(&4));
    }

    #[test]
    fn tarjan_scc_correctness_four_singletons_plus_scc() {
        // 5-wallet ring: 0→1→2→3→4→0; plus isolated vertex 5.
        let adj = vec![
            vec![1], // 0 → 1
            vec![2], // 1 → 2
            vec![3], // 2 → 3
            vec![4], // 3 → 4
            vec![0], // 4 → 0
            vec![],  // 5 isolated
        ];
        let sccs = tarjan_scc(&adj, 6);
        // One SCC of size 5, one singleton.
        assert_eq!(sccs.len(), 2);
        assert_eq!(sccs[0].len(), 5);
        assert_eq!(sccs[1].len(), 1);
        assert_eq!(sccs[1][0], 5);
    }

    #[test]
    fn tarjan_scc_correctness_complex_with_three_sccs() {
        // A: {0,1,2} (triangle), B: {3,4} (2-cycle), bridge: 2→3
        // C: {5} singleton. 4→5 bridge.
        let adj = vec![
            vec![1],       // 0 → 1
            vec![2],       // 1 → 2
            vec![0, 3],    // 2 → 0, 2 → 3
            vec![4],       // 3 → 4
            vec![3, 5],    // 4 → 3, 4 → 5
            vec![],        // 5 isolated
        ];
        let sccs = tarjan_scc(&adj, 6);
        assert_eq!(sccs.len(), 3);
        // Sizes: 3, 2, 1 (sorted desc by size).
        assert_eq!(sccs[0].len(), 3);
        assert_eq!(sccs[1].len(), 2);
        assert_eq!(sccs[2].len(), 1);
    }

    #[test]
    fn tarjan_scc_no_cycle_dag() {
        // DAG: 0→1→2→3 (no cycles)
        let adj = vec![
            vec![1],
            vec![2],
            vec![3],
            vec![],
        ];
        let sccs = tarjan_scc(&adj, 4);
        // All SCCs are singletons (no back edges).
        assert_eq!(sccs.len(), 4);
        for scc in &sccs {
            assert_eq!(scc.len(), 1, "DAG must produce only singleton SCCs");
        }
    }

    // -----------------------------------------------------------------------
    // Johnson cycles correctness
    // -----------------------------------------------------------------------

    #[test]
    fn johnson_cycles_correctness_triangle() {
        // Simple 3-wallet ring: A→B→C→A, single cycle.
        let edges = vec![
            edge("A", "B", 1_000_000, 0),
            edge("B", "C", 1_000_000, 10),
            edge("C", "A", 1_000_000, 20),
        ];
        let cycles = detect_cycles(&edges, &cfg_default());
        assert_eq!(cycles.len(), 1, "one elementary cycle expected");
        let c = &cycles[0];
        assert_eq!(c.vertices.len(), 3, "3-wallet ring has 3 vertices");
        // All three wallets appear.
        assert!(c.vertices.contains(&"A".to_owned()));
        assert!(c.vertices.contains(&"B".to_owned()));
        assert!(c.vertices.contains(&"C".to_owned()));
    }

    #[test]
    fn johnson_cycles_correctness_two_independent_triangles() {
        // Ring 1: A→B→C→A; Ring 2: D→E→F→D
        let edges = vec![
            edge("A", "B", 1_000, 0),
            edge("B", "C", 1_000, 1),
            edge("C", "A", 1_000, 2),
            edge("D", "E", 2_000, 3),
            edge("E", "F", 2_000, 4),
            edge("F", "D", 2_000, 5),
        ];
        let cycles = detect_cycles(&edges, &cfg_default());
        // Two separate SCCs → two cycles.
        assert_eq!(cycles.len(), 2, "two independent triangles → 2 cycles");
    }

    #[test]
    fn johnson_cycles_correctness_diamond_with_shared_hub() {
        // Hub topology: A→B→D→A and A→C→D→A (two 3-hop cycles sharing A and D).
        // Graph edges: A→B, B→D, D→A, A→C, C→D
        let edges = vec![
            edge("A", "B", 1_000, 0),
            edge("B", "D", 1_000, 1),
            edge("D", "A", 1_000, 2),
            edge("A", "C", 1_000, 3),
            edge("C", "D", 1_000, 4),
        ];
        let cycles = detect_cycles(&edges, &cfg_default());
        // Expect 2 elementary cycles (A→B→D→A and A→C→D→A).
        assert_eq!(cycles.len(), 2, "diamond hub: 2 elementary cycles expected");
    }

    // -----------------------------------------------------------------------
    // Johnson respects max_cycle_length
    // -----------------------------------------------------------------------

    #[test]
    fn johnson_respects_max_cycle_length() {
        // 5-wallet ring: A→B→C→D→E→A (5-hop cycle, exactly at max_cycle_length=5).
        let edges = vec![
            edge("A", "B", 100, 0),
            edge("B", "C", 100, 1),
            edge("C", "D", 100, 2),
            edge("D", "E", 100, 3),
            edge("E", "A", 100, 4),
        ];

        // With max_cycle_length=5, the cycle should be found.
        let cfg5 = CycleDetectionConfig {
            max_cycle_length: 5,
            max_cycles_per_scc: 100,
            min_scc_size: 3,
        };
        let cycles5 = detect_cycles(&edges, &cfg5);
        assert_eq!(cycles5.len(), 1, "5-hop cycle with max_cycle_length=5 must be detected");

        // With max_cycle_length=4, the 5-hop cycle should be excluded.
        let cfg4 = CycleDetectionConfig {
            max_cycle_length: 4,
            max_cycles_per_scc: 100,
            min_scc_size: 3,
        };
        let cycles4 = detect_cycles(&edges, &cfg4);
        assert_eq!(
            cycles4.len(),
            0,
            "5-hop cycle with max_cycle_length=4 must be excluded"
        );
    }

    // -----------------------------------------------------------------------
    // Johnson respects max_cycles_per_scc
    // -----------------------------------------------------------------------

    #[test]
    fn johnson_respects_max_cycles_per_scc() {
        // Build a 6-clique (all pairs connected both directions) — many cycles.
        // A 6-node complete directed graph has far more than 100 elementary cycles.
        let names = ["A", "B", "C", "D", "E", "F"];
        let mut edges = Vec::new();
        for (i, &a) in names.iter().enumerate() {
            for (j, &b) in names.iter().enumerate() {
                if i != j {
                    edges.push(edge(a, b, 1_000, (i * 10 + j) as i64));
                }
            }
        }

        let cfg_cap10 = CycleDetectionConfig {
            max_cycle_length: 5,
            max_cycles_per_scc: 10,
            min_scc_size: 3,
        };
        let cycles = detect_cycles(&edges, &cfg_cap10);
        assert!(
            cycles.len() <= 10,
            "cap at 10 cycles must be respected, got {}",
            cycles.len()
        );
        assert!(!cycles.is_empty(), "at least one cycle must be found in a clique");
    }

    // -----------------------------------------------------------------------
    // Determinism
    // -----------------------------------------------------------------------

    #[test]
    fn determinism_same_input_produces_identical_output() {
        let edges = vec![
            edge("W1", "W2", 500_000, 100),
            edge("W2", "W3", 500_000, 200),
            edge("W3", "W1", 500_000, 300),
            edge("W1", "W4", 200_000, 400),
            edge("W4", "W1", 200_000, 500),
            edge("W2", "W4", 100_000, 600),
        ];
        let cfg = cfg_default();

        let run1 = detect_cycles(&edges, &cfg);
        let run2 = detect_cycles(&edges, &cfg);
        let run3 = detect_cycles(&edges, &cfg);

        assert_eq!(
            run1.len(),
            run2.len(),
            "determinism: run 1 and 2 must have same cycle count"
        );
        assert_eq!(
            run1.len(),
            run3.len(),
            "determinism: run 1 and 3 must have same cycle count"
        );

        for (i, (c1, c2)) in run1.iter().zip(run2.iter()).enumerate() {
            assert_eq!(
                c1.vertices, c2.vertices,
                "determinism: cycle {i} vertices differ between run 1 and 2"
            );
            assert_eq!(
                c1.per_edge_amounts_raw, c2.per_edge_amounts_raw,
                "determinism: cycle {i} per_edge_amounts_raw differs between run 1 and 2"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Deduplication of multi-edges
    // -----------------------------------------------------------------------

    #[test]
    fn dedup_multi_edges_by_from_to() {
        // Five transfers from A→B with different amounts, same pair.
        // Should be collapsed to one logical edge with summed amount.
        let edges = vec![
            edge("A", "B", 100, 0),
            edge("A", "B", 200, 1),
            edge("A", "B", 300, 2),
            edge("A", "B", 400, 3),
            edge("A", "B", 500, 4),
            edge("B", "A", 600, 5), // Return edge to form 2-hop cycle.
        ];

        // 2-node SCC → 2-hop cycle; but min_scc_size=3 filters it out.
        let cfg = cfg_default(); // min_scc_size=3
        let cycles = detect_cycles(&edges, &cfg);
        // 2-node SCC filtered out by min_scc_size=3.
        assert_eq!(
            cycles.len(),
            0,
            "2-node SCC with min_scc_size=3 must produce no cycles"
        );

        // With min_scc_size=2, one cycle is found and deduplication holds.
        let cfg2 = CycleDetectionConfig {
            max_cycle_length: 5,
            max_cycles_per_scc: 100,
            min_scc_size: 2,
        };
        let cycles2 = detect_cycles(&edges, &cfg2);
        assert_eq!(cycles2.len(), 1, "one 2-hop cycle must be found");
        // Summed amount from A→B: 100+200+300+400+500 = 1500. B→A: 600.
        // Per-edge amounts are retained in traversal order; the bottleneck is 600.
        assert_eq!(
            cycles2[0].per_edge_amounts_raw.len(),
            2,
            "2-hop cycle must have 2 per-edge amounts"
        );
        let mut sorted = cycles2[0].per_edge_amounts_raw.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec![600u128, 1500u128],
            "deduplicated edges must retain per-edge sums (1500 one way, 600 back)"
        );
    }

    // -----------------------------------------------------------------------
    // Empty input
    // -----------------------------------------------------------------------

    #[test]
    fn detect_cycles_empty_input_returns_empty() {
        let cycles = detect_cycles(&[], &cfg_default());
        assert!(cycles.is_empty());
    }

    // -----------------------------------------------------------------------
    // No cycle in a DAG
    // -----------------------------------------------------------------------

    #[test]
    fn detect_cycles_dag_no_cycles() {
        // One-way chain: A→B→C→D (no return edge).
        let edges = vec![
            edge("A", "B", 1_000, 0),
            edge("B", "C", 1_000, 1),
            edge("C", "D", 1_000, 2),
        ];
        let cycles = detect_cycles(&edges, &cfg_default());
        assert!(
            cycles.is_empty(),
            "DAG with no return edges must produce no cycles"
        );
    }

    // -----------------------------------------------------------------------
    // 5-wallet ring at max_cycle_length boundary
    // -----------------------------------------------------------------------

    #[test]
    fn detect_cycles_five_wallet_ring() {
        let edges = vec![
            edge("W1", "W2", 1_000_000, 0),
            edge("W2", "W3", 1_000_000, 60),
            edge("W3", "W4", 1_000_000, 120),
            edge("W4", "W5", 1_000_000, 180),
            edge("W5", "W1", 1_000_000, 240),
        ];
        let cycles = detect_cycles(&edges, &cfg_default());
        assert_eq!(cycles.len(), 1, "exactly one 5-wallet ring");
        assert_eq!(cycles[0].vertices.len(), 5);
        assert!(cycles[0].block_time_span_minutes >= 4, "span must be >= 4 minutes");
    }
}
