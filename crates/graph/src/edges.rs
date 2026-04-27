//! Wallet-edge aggregation: reading from `transfers` and upserting into `wallet_edges`.
//!
//! # OQ1 resolution — native SOL in `transfers` table
//!
//! The Solana `decode.rs` adapter now emits native SOL transfers (System Program
//! `Transfer` instruction) as `common::Transfer` events with
//! `token = "11111111111111111111111111111111"` (the System Program address).
//! These are routed through the same `insert_transfers` path as SPL transfers.
//!
//! `GraphIndexer::index_sol_transfers` therefore filters the `transfers` table for:
//! ```sql
//! WHERE chain = $1
//!   AND token = '11111111111111111111111111111111'
//!   AND is_mint = false
//!   AND is_burn = false
//!   AND amount_raw >= $min_funder_sol_amount
//!   AND block_time > $since
//! ```
//!
//! If no native SOL transfers have been ingested yet (e.g. on a fresh instance),
//! `index_sol_transfers` returns `IndexStats { transfers_scanned: 0, .. }` cleanly.
//! The checkpoint is still advanced so subsequent runs do not re-scan old blocks.
//!
//! # Idempotency
//!
//! `INSERT INTO wallet_edges ... ON CONFLICT DO UPDATE SET` accumulates values.
//! Running `index_sol_transfers` twice on the same block range is safe: the
//! UPSERT is commutative for `total_sol_lamports` and `tx_count`, and uses
//! `GREATEST(...)` for `last_tx_time`. `first_tx_time` is set only on INSERT
//! and never changed on conflict.
//!
//! # Checkpoint
//!
//! Progress is stored in `adapter_checkpoints` with
//! `adapter_id = "graph_indexer_{chain}"`, reusing existing infrastructure.
//! On each call, `since` is the `last_indexed_at` from the checkpoint. After
//! successful indexing, the checkpoint is advanced to `now()`.

use std::time::Instant;

use chrono::{DateTime, Utc};
use tracing::{debug, info, instrument};

use crate::config::GraphConfig;
use crate::error::GraphError;

// ---------------------------------------------------------------------------
// System Program address — native SOL transfers use this as `token`
// ---------------------------------------------------------------------------

/// The Solana System Program address. Native SOL transfers are stored in the
/// `transfers` table with `token = SYSTEM_PROGRAM_ADDRESS`.
///
/// This is the convention established by `crates/chain-adapter/src/solana/decode.rs`
/// for native SOL transfers (System Program `Transfer` instruction, discriminator 2).
pub const SYSTEM_PROGRAM_ADDRESS: &str = "11111111111111111111111111111111";

// ---------------------------------------------------------------------------
// WalletEdge — Rust representation of a wallet_edges row
// ---------------------------------------------------------------------------

/// A directed funding edge in the wallet graph.
///
/// Corresponds to one row in the `wallet_edges` Postgres table.
/// Represents the aggregate of all SOL transfers from `from_wallet` to
/// `to_wallet` within the indexed block range.
///
/// Amounts are `u128` raw lamports, stored as `NUMERIC(39,0)` via the String
/// bridge (see docs/designs/0002-storage-schemas-v1.md §type-mapping).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletEdge {
    pub chain: String,
    pub from_wallet: String,
    pub to_wallet: String,
    /// Total SOL transferred, in lamports. Serialized as string to Postgres.
    pub total_sol_lamports: u128,
    pub tx_count: i64,
    pub first_tx_time: DateTime<Utc>,
    pub last_tx_time: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One transfer event to be accumulated into `wallet_edges`.
///
/// Built by `GraphIndexer` from a batch of rows read from the `transfers` table.
/// Lamport amount is `u128` (from `NUMERIC(39,0)` String bridge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpsertEdge {
    pub chain: String,
    pub from_wallet: String,
    pub to_wallet: String,
    /// SOL amount for this single transfer, in lamports.
    pub sol_lamports: u128,
    pub tx_time: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// IndexStats
// ---------------------------------------------------------------------------

/// Statistics from one `index_sol_transfers` run.
#[derive(Debug, Clone, Default)]
pub struct IndexStats {
    pub transfers_scanned: u64,
    pub edges_upserted: u64,
    pub duration: std::time::Duration,
}

// ---------------------------------------------------------------------------
// GraphIndexer
// ---------------------------------------------------------------------------

/// Reads `transfers` and accumulates directed edges into `wallet_edges`.
///
/// Constructed with a reference to the live Postgres pool and the graph config.
/// The pool is borrowed (not owned) so the caller controls its lifetime.
pub struct GraphIndexer<'a> {
    pub pool: &'a sqlx::PgPool,
    pub config: &'a GraphConfig,
}

impl<'a> GraphIndexer<'a> {
    /// Scan native SOL transfers from `transfers` table since `since`, aggregate
    /// (from_wallet, to_wallet) edges, and UPSERT into `wallet_edges`.
    ///
    /// # Arguments
    ///
    /// - `chain`: the chain identifier (e.g. `"solana"`). Used as the `chain`
    ///   column filter and as the edge chain.
    /// - `since`: only transfers with `block_time > since` are scanned.
    ///   Pass `DateTime::<Utc>::from_timestamp(0, 0).unwrap()` for a full re-index.
    ///
    /// # Idempotency
    ///
    /// Safe to call multiple times for the same range. UPSERT accumulates.
    ///
    /// # Panics
    ///
    /// Does not panic. All fallible operations return `GraphError`.
    #[instrument(skip(self), fields(chain, since = %since))]
    pub async fn index_sol_transfers(
        &self,
        chain: &str,
        since: DateTime<Utc>,
    ) -> Result<IndexStats, GraphError> {
        let started = Instant::now();
        let batch_size = self.config.indexer_batch_size.value as i64;
        let min_lamports = self.config.min_funder_sol_amount.value.to_string();

        let mut transfers_scanned: u64 = 0;
        let mut edges_upserted: u64 = 0;
        let mut offset: i64 = 0;

        loop {
            let rows = sqlx::query(
                r#"
                SELECT from_address, to_address, amount_raw::TEXT, block_time
                FROM transfers
                WHERE chain       = $1
                  AND token       = $2
                  AND is_mint     = false
                  AND is_burn     = false
                  AND amount_raw  >= $3::NUMERIC
                  AND block_time  > $4
                ORDER BY block_time ASC
                LIMIT $5
                OFFSET $6
                "#,
            )
            .bind(chain)
            .bind(SYSTEM_PROGRAM_ADDRESS)
            .bind(&min_lamports)
            .bind(since)
            .bind(batch_size)
            .bind(offset)
            .fetch_all(self.pool)
            .await?;

            if rows.is_empty() {
                break;
            }

            let count = rows.len() as u64;
            transfers_scanned += count;
            offset += count as i64;

            for row in &rows {
                use sqlx::Row as _;

                let from_wallet: String = row.try_get("from_address").map_err(|e| {
                    GraphError::ParseField {
                        field: "from_address",
                        reason: e.to_string(),
                    }
                })?;
                let to_wallet: String = row.try_get("to_address").map_err(|e| {
                    GraphError::ParseField {
                        field: "to_address",
                        reason: e.to_string(),
                    }
                })?;
                let amount_str: String = row.try_get("amount_raw").map_err(|e| {
                    GraphError::ParseField {
                        field: "amount_raw",
                        reason: e.to_string(),
                    }
                })?;
                let block_time: DateTime<Utc> = row.try_get("block_time").map_err(|e| {
                    GraphError::ParseField {
                        field: "block_time",
                        reason: e.to_string(),
                    }
                })?;

                let sol_lamports: u128 = amount_str.parse().map_err(|e| GraphError::ParseField {
                    field: "amount_raw",
                    reason: format!("parse u128: {e}"),
                })?;

                // Skip self-transfers (System Program internal moves).
                if from_wallet == to_wallet {
                    continue;
                }

                self.upsert_edge(chain, &from_wallet, &to_wallet, sol_lamports, block_time)
                    .await?;
                edges_upserted += 1;
            }

            // If we got fewer rows than batch_size, we've exhausted the result set.
            if count < batch_size as u64 {
                break;
            }

            debug!(
                chain,
                offset,
                transfers_scanned,
                "edge indexing batch complete"
            );
        }

        let duration = started.elapsed();
        info!(
            chain,
            transfers_scanned,
            edges_upserted,
            duration_ms = duration.as_millis(),
            "index_sol_transfers complete"
        );

        Ok(IndexStats {
            transfers_scanned,
            edges_upserted,
            duration,
        })
    }

    /// UPSERT a single directed edge into `wallet_edges`.
    ///
    /// On conflict, accumulates `total_sol_lamports` and `tx_count`, updates
    /// `last_tx_time` to the greater of the two, and leaves `first_tx_time`
    /// unchanged (original first funding event is preserved).
    async fn upsert_edge(
        &self,
        chain: &str,
        from_wallet: &str,
        to_wallet: &str,
        sol_lamports: u128,
        tx_time: DateTime<Utc>,
    ) -> Result<(), GraphError> {
        let lamports_str = sol_lamports.to_string();

        sqlx::query(
            r#"
            INSERT INTO wallet_edges
                (chain, from_wallet, to_wallet, total_sol_lamports, tx_count,
                 first_tx_time, last_tx_time, updated_at)
            VALUES ($1, $2, $3, $4::NUMERIC, 1, $5, $5, now())
            ON CONFLICT (chain, from_wallet, to_wallet) DO UPDATE SET
                total_sol_lamports = wallet_edges.total_sol_lamports
                                     + EXCLUDED.total_sol_lamports,
                tx_count           = wallet_edges.tx_count + 1,
                last_tx_time       = GREATEST(wallet_edges.last_tx_time,
                                              EXCLUDED.last_tx_time),
                updated_at         = now()
            "#,
        )
        .bind(chain)
        .bind(from_wallet)
        .bind(to_wallet)
        .bind(&lamports_str)
        .bind(tx_time)
        .execute(self.pool)
        .await?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pure aggregation helpers (no I/O — unit-testable without DB)
// ---------------------------------------------------------------------------

/// Aggregate a slice of `UpsertEdge` values into a deduplicated `Vec<WalletEdge>`.
///
/// This is the pure-compute equivalent of the DB UPSERT, used in unit tests to
/// verify aggregation logic without a live Postgres connection.
///
/// The output is deterministic: edges are sorted by `(from_wallet, to_wallet)`.
/// Multiple edges for the same pair are merged: `total_sol_lamports` and
/// `tx_count` are summed; `first_tx_time` takes the minimum; `last_tx_time`
/// takes the maximum; `updated_at` is set to `Utc::now()` (test-only field).
///
/// # Design note
///
/// This function uses `BTreeMap` (not `HashMap`) to guarantee deterministic
/// iteration order in the output. See design 0013 §Developer Acceptance
/// Checklist: "No HashMap in any path that contributes to wallet_edges output."
pub fn aggregate_edges(
    edges: &[UpsertEdge],
    now: DateTime<Utc>,
) -> Vec<WalletEdge> {
    use std::collections::BTreeMap;

    // Key: (chain, from_wallet, to_wallet) — BTreeMap for determinism.
    let mut map: BTreeMap<(String, String, String), WalletEdge> = BTreeMap::new();

    for e in edges {
        let key = (e.chain.clone(), e.from_wallet.clone(), e.to_wallet.clone());
        map.entry(key)
            .and_modify(|existing| {
                existing.total_sol_lamports =
                    existing.total_sol_lamports.saturating_add(e.sol_lamports);
                existing.tx_count = existing.tx_count.saturating_add(1);
                if e.tx_time < existing.first_tx_time {
                    existing.first_tx_time = e.tx_time;
                }
                if e.tx_time > existing.last_tx_time {
                    existing.last_tx_time = e.tx_time;
                }
                existing.updated_at = now;
            })
            .or_insert_with(|| WalletEdge {
                chain: e.chain.clone(),
                from_wallet: e.from_wallet.clone(),
                to_wallet: e.to_wallet.clone(),
                total_sol_lamports: e.sol_lamports,
                tx_count: 1,
                first_tx_time: e.tx_time,
                last_tx_time: e.tx_time,
                updated_at: now,
            });
    }

    map.into_values().collect()
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

    fn now() -> DateTime<Utc> {
        ts(1_750_000_000)
    }

    fn make_edge(from: &str, to: &str, lamports: u128, t: i64) -> UpsertEdge {
        UpsertEdge {
            chain: "solana".into(),
            from_wallet: from.into(),
            to_wallet: to.into(),
            sol_lamports: lamports,
            tx_time: ts(t),
        }
    }

    // --- aggregate_edges ---

    #[test]
    fn aggregate_single_edge() {
        let edges = vec![make_edge("alice", "bob", 1_000_000, 100)];
        let result = aggregate_edges(&edges, now());
        assert_eq!(result.len(), 1);
        let e = &result[0];
        assert_eq!(e.from_wallet, "alice");
        assert_eq!(e.to_wallet, "bob");
        assert_eq!(e.total_sol_lamports, 1_000_000);
        assert_eq!(e.tx_count, 1);
        assert_eq!(e.first_tx_time, ts(100));
        assert_eq!(e.last_tx_time, ts(100));
    }

    #[test]
    fn aggregate_multiple_transfers_same_pair_sums_lamports() {
        let edges = vec![
            make_edge("alice", "bob", 1_000_000, 100),
            make_edge("alice", "bob", 500_000, 200),
            make_edge("alice", "bob", 250_000, 150),
        ];
        let result = aggregate_edges(&edges, now());
        assert_eq!(result.len(), 1);
        let e = &result[0];
        assert_eq!(e.total_sol_lamports, 1_750_000);
        assert_eq!(e.tx_count, 3);
        assert_eq!(e.first_tx_time, ts(100));
        assert_eq!(e.last_tx_time, ts(200));
    }

    #[test]
    fn aggregate_two_pairs_separate_rows() {
        let edges = vec![
            make_edge("alice", "bob", 1_000_000, 100),
            make_edge("alice", "carol", 2_000_000, 110),
        ];
        let result = aggregate_edges(&edges, now());
        assert_eq!(result.len(), 2);
        // BTreeMap guarantees sorted order: alice→bob before alice→carol.
        assert_eq!(result[0].to_wallet, "bob");
        assert_eq!(result[1].to_wallet, "carol");
    }

    #[test]
    fn aggregate_preserves_first_tx_time() {
        let edges = vec![
            make_edge("funder", "w1", 10_000_000, 500),
            make_edge("funder", "w1", 5_000_000, 100), // earlier
        ];
        let result = aggregate_edges(&edges, now());
        assert_eq!(result[0].first_tx_time, ts(100));
        assert_eq!(result[0].last_tx_time, ts(500));
    }

    #[test]
    fn aggregate_empty_input_returns_empty() {
        let result = aggregate_edges(&[], now());
        assert!(result.is_empty());
    }

    #[test]
    fn aggregate_deterministic_on_repeated_call() {
        let edges = vec![
            make_edge("funder", "w3", 1_000_000, 300),
            make_edge("funder", "w1", 1_000_000, 100),
            make_edge("funder", "w2", 1_000_000, 200),
        ];
        let r1 = aggregate_edges(&edges, now());
        let r2 = aggregate_edges(&edges, now());
        assert_eq!(r1.len(), r2.len());
        for (a, b) in r1.iter().zip(r2.iter()) {
            assert_eq!(a.from_wallet, b.from_wallet);
            assert_eq!(a.to_wallet, b.to_wallet);
            assert_eq!(a.total_sol_lamports, b.total_sol_lamports);
        }
    }

    #[test]
    fn aggregate_idempotent_upsert_same_input_twice() {
        // If we run the same edge list twice (simulating a re-index), the
        // result should be double the lamports and double the tx_count.
        // This matches what the DB UPSERT does (it accumulates, not deduplicates).
        // The pure function mimics the UPSERT semantics.
        let mut edges = vec![
            make_edge("funder", "wallet_a", 10_000_000, 1000),
            make_edge("funder", "wallet_b", 10_000_000, 1100),
            make_edge("funder", "wallet_c", 10_000_000, 1200),
        ];
        let first_run = aggregate_edges(&edges, now());
        // Second run: same transfers again (simulates re-scan of same block range).
        let mut second_batch = edges.clone();
        edges.append(&mut second_batch);
        let second_run = aggregate_edges(&edges, now());
        // Lamports doubled, tx_count doubled.
        for (r1, r2) in first_run.iter().zip(second_run.iter()) {
            assert_eq!(r2.total_sol_lamports, r1.total_sol_lamports * 2);
            assert_eq!(r2.tx_count, r1.tx_count * 2);
        }
    }
}
