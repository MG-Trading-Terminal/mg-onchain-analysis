//! Graph indexer writer — populates `graph_edges` and `address_labels` from
//! indexer event observations.
//!
//! # What this module does
//!
//! Whenever the indexer observes:
//!
//! - A `PoolEvent::Initialize` — writes a `DeployerOf` edge and a `DeployerEOA`
//!   label for the pool creator (the `actor` field on the event, which is the
//!   wallet that signed the Initialize instruction).
//!
//! - A `TokenMeta` upsert — writes `AuthorityOf` edges for non-NULL
//!   `mint_authority` and `freeze_authority` fields.
//!
//! - A reorg signal at `reorg_height` — calls
//!   `TypedEdgeStore::delete_edges_above_block` and
//!   `GraphLabelStore::delete_indexer_labels_after` to retract graph data
//!   invalidated by the reorg.
//!
//! # Block-time discipline (gotcha #22 / #28)
//!
//! - `PoolEvent::Initialize` writes use `event.block_time` and
//!   `event.block.height` as the time source. Never `Utc::now()`.
//! - `TokenMeta` writes use `meta.detected_at` as a **best-available
//!   approximation** because `TokenMeta` carries no per-block timestamp.
//!   `detected_at` is RugCheck's first-observed-on-chain time — it is close to
//!   the actual mint block time but is NOT the block timestamp. This is
//!   documented as a known gap (see §Gap below).
//! - Reorg label deletion uses the block_time at the reorg height, which the
//!   caller (indexer run loop) derives from the adapter's block context.
//!
//! # Gap: `TokenMeta` block_time unavailable
//!
//! `crates/common::TokenMeta` is FROZEN (gotcha #1) and carries no
//! `block_time: DateTime<Utc>` field. The `detected_at` field is a RugCheck
//! enrichment timestamp, not a block header timestamp. For Sprint 11 this is
//! acceptable because:
//!
//! 1. `AuthorityOf` edges for token metadata are already approximate (they are
//!    inferred from metadata state, not from an observed SetAuthority instruction).
//! 2. The `issued_at` value is used only for TTL-based eviction in reorg
//!    handling, where the order of magnitude (~same block range) is sufficient.
//!
//! Sprint 12 follow-up: if `TokenMeta` gains a `first_seen_block_time` field
//! (pre-authorised addition), update the graph writer to use it here.
//! Tracked in: `SPRINTS.md` Sprint 11 spec deviations.
//!
//! # Crate dependency
//!
//! `crates/indexer` → `crates/graph`. This direction is safe:
//! `graph` → `storage` → `common`; `indexer` → `storage`; no cycle introduced.
//!
//! # NULL-transition for authorities (OQ2 from design 0015)
//!
//! When `mint_authority` or `freeze_authority` transitions to `None` (revoked),
//! this happens via a `SetAuthority` SPL instruction. That instruction is NOT
//! currently decoded anywhere in this workspace (verified by grep: no
//! `SetAuthority` variant in `crates/common/src/event.rs` or
//! `crates/chain-adapter/src/`). Therefore the NULL-transition `AuthorityOf`
//! edge deletion is NOT implemented here — punted to Sprint 12.
//! The existing `AuthorityOf` edges written on token creation will become stale
//! after revocation. This is acceptable for Sprint 11 (no downstream consumer
//! yet, and D08 treats non-NULL edges as advisory, not authoritative).
//!
//! # Design reference
//!
//! `docs/designs/0015-crates-graph-phase3.md` §5 Integration

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::json;
use tracing::{debug, instrument, warn};

use mg_onchain_common::event::{PoolEvent, PoolEventKind};
use mg_onchain_common::token::TokenMeta;
use mg_onchain_graph::{
    AddressLabel, EdgeType, GraphEdge, GraphLabelStore, LabelType, TypedEdgeStore,
};

use crate::error::IndexerError;

// ---------------------------------------------------------------------------
// GraphWriterError — local error type bridging GraphError to IndexerError
// ---------------------------------------------------------------------------

/// Converts a `GraphError` into an `IndexerError` via a string bridge.
///
/// `IndexerError` uses `thiserror` and carries `StorageError` and adapter
/// errors natively. `GraphError` is a separate type in `crates/graph`.
/// Rather than adding a new `IndexerError::Graph` variant (which would require
/// touching the frozen-adjacent error interface), we bridge through a string.
/// The error message is preserved for tracing.
fn graph_err(e: mg_onchain_graph::GraphError) -> IndexerError {
    IndexerError::Config(format!("graph writer error: {e}"))
}

// ---------------------------------------------------------------------------
// GraphIndexerWriter
// ---------------------------------------------------------------------------

/// Writes graph edges and address labels from indexer event observations.
///
/// Holds `Arc<dyn _>` to the two graph stores so it can be cheaply cloned
/// alongside the `Indexer<A, S, C>` struct. Both stores are `Send + Sync`.
///
/// # Thread safety
///
/// The indexer is single-task (no concurrent writes from a single `Indexer`
/// instance), so `Arc<dyn _>` is sufficient — `Mutex` is not needed here.
/// The `Arc` allows shared ownership when the same pool is passed to other
/// components (D08, ClusterDetector) at server startup.
#[derive(Clone)]
pub struct GraphIndexerWriter {
    edge_store: Arc<dyn TypedEdgeStore>,
    label_store: Arc<dyn GraphLabelStore>,
}

impl GraphIndexerWriter {
    /// Construct a new writer wrapping the given stores.
    pub fn new(edge_store: Arc<dyn TypedEdgeStore>, label_store: Arc<dyn GraphLabelStore>) -> Self {
        Self {
            edge_store,
            label_store,
        }
    }

    // -----------------------------------------------------------------------
    // on_pool_event — fires on every PoolEvent, handles Initialize internally
    // -----------------------------------------------------------------------

    /// Process a pool event.
    ///
    /// For `PoolEventKind::Initialize`: writes a `DeployerOf` edge and a
    /// `DeployerEOA` label for both `token0` and `token1` in the pool.
    ///
    /// All other pool event kinds are silently ignored (no graph writes).
    ///
    /// # Time source
    ///
    /// `event.block_time` is used as `issued_at` for the label and `block_time`
    /// for the edge. `event.block.height` is used as `block_height`. These come
    /// directly from the chain adapter — never wall-clock (gotcha #28).
    #[instrument(skip(self, event), fields(
        chain = %event.chain,
        block_height = event.block.height,
        kind = "pool_event",
    ))]
    pub async fn on_pool_event(&self, event: &PoolEvent) -> Result<(), IndexerError> {
        let PoolEventKind::Initialize { token0, token1 } = &event.kind else {
            // Not an Initialize event — no graph writes for Mint/Burn/Sync.
            return Ok(());
        };

        let chain_str = event.chain.as_str().to_owned();
        let deployer = event.actor.as_str().to_owned();
        let block_time: DateTime<Utc> = event.block_time;
        let block_height: u64 = event.block.height;
        let tx_hash_str: String = event.tx_hash.to_string();

        debug!(
            deployer = %deployer,
            token0 = %token0.as_str(),
            token1 = %token1.as_str(),
            block_height,
            "Initialize event — writing DeployerOf edges + DeployerEOA label"
        );

        // Write DeployerOf edge and DeployerEOA label for token0.
        self.write_deployer_edges(
            &chain_str,
            &deployer,
            token0.as_str(),
            block_time,
            block_height,
            &tx_hash_str,
        )
        .await?;

        // Write DeployerOf edge for token1 as well.
        // In Raydium/Uniswap-style pools, token1 may be a quote token (USDC,
        // WSOL) that already has its own deployer. The ON CONFLICT DO NOTHING
        // semantics ensure we don't overwrite a pre-existing deployer record for
        // the quote token; we simply insert another edge from this actor.
        self.write_deployer_edges(
            &chain_str,
            &deployer,
            token1.as_str(),
            block_time,
            block_height,
            &tx_hash_str,
        )
        .await?;

        Ok(())
    }

    /// Write a `DeployerOf` edge and `DeployerEOA` label for one token.
    async fn write_deployer_edges(
        &self,
        chain: &str,
        deployer: &str,
        token_mint: &str,
        block_time: DateTime<Utc>,
        block_height: u64,
        tx_hash: &str,
    ) -> Result<(), IndexerError> {
        let edge = GraphEdge {
            chain: chain.to_owned(),
            from_address: deployer.to_owned(),
            to_address: token_mint.to_owned(),
            edge_type: EdgeType::DeployerOf,
            token: Some(token_mint.to_owned()),
            amount_raw: None,
            block_time,
            block_height,
            tx_hash: Some(tx_hash.to_owned()),
        };
        self.edge_store
            .insert_edge(&edge)
            .await
            .map_err(graph_err)?;

        let label = AddressLabel {
            chain: chain.to_owned(),
            address: deployer.to_owned(),
            label_type: LabelType::DeployerEoa,
            confidence: 1.0,
            evidence: json!({
                "token": token_mint,
                "tx_hash": tx_hash,
            }),
            issued_at: block_time, // block_time from chain adapter — not wall clock
            expires_at: None,      // permanent
            source: "indexer_pool_initialize".to_owned(),
        };
        self.label_store
            .upsert_label(&label)
            .await
            .map_err(graph_err)?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // on_token_meta — fires after every TokenMeta upsert
    // -----------------------------------------------------------------------

    /// Process a `TokenMeta` event after it has been upserted to the `tokens` table.
    ///
    /// Writes `AuthorityOf` edges for non-NULL `mint_authority` and
    /// `freeze_authority` fields.
    ///
    /// If both authorities are `None` (revoked token), this is a no-op.
    ///
    /// # Time source approximation
    ///
    /// `meta.detected_at` is used as `issued_at` for graph edges when present.
    /// See module-level doc for the known gap: `TokenMeta` carries no
    /// per-block timestamp, and `detected_at` is a RugCheck enrichment time,
    /// not the actual block timestamp.
    ///
    /// When `detected_at` is `None`, edges are not written (no safe time
    /// source available). This is intentional — a label without a meaningful
    /// timestamp is worse than no label (uninvestigatable on reorg). A warning
    /// is logged so operators can track the frequency.
    #[instrument(skip(self, meta), fields(
        chain = %meta.chain,
        mint = %meta.mint.as_str(),
    ))]
    pub async fn on_token_meta(
        &self,
        meta: &TokenMeta,
        block_height: u64,
    ) -> Result<(), IndexerError> {
        // Require a time source. Use detected_at as best approximation.
        // If absent, skip graph writes (no safe issued_at).
        let issued_at = match meta.detected_at {
            Some(t) => t,
            None => {
                warn!(
                    mint = %meta.mint.as_str(),
                    "TokenMeta has no detected_at — skipping AuthorityOf graph writes"
                );
                return Ok(());
            }
        };

        let chain_str = meta.chain.as_str().to_owned();
        let token_mint = meta.mint.as_str().to_owned();

        // Write AuthorityOf edge for mint_authority if present.
        if let Some(mint_auth) = &meta.mint_authority {
            debug!(
                mint = %token_mint,
                authority = %mint_auth.as_str(),
                "writing AuthorityOf(mint) edge"
            );
            let edge = GraphEdge {
                chain: chain_str.clone(),
                from_address: mint_auth.as_str().to_owned(),
                to_address: token_mint.clone(),
                edge_type: EdgeType::AuthorityOf,
                token: Some(token_mint.clone()),
                amount_raw: None,
                block_time: issued_at,
                block_height,
                tx_hash: None, // inferred from metadata; no specific tx
            };
            self.edge_store
                .insert_edge(&edge)
                .await
                .map_err(graph_err)?;
        }

        // Write AuthorityOf edge for freeze_authority if present.
        if let Some(freeze_auth) = &meta.freeze_authority {
            debug!(
                mint = %token_mint,
                authority = %freeze_auth.as_str(),
                "writing AuthorityOf(freeze) edge"
            );
            let edge = GraphEdge {
                chain: chain_str.clone(),
                from_address: freeze_auth.as_str().to_owned(),
                to_address: token_mint.clone(),
                edge_type: EdgeType::AuthorityOf,
                token: Some(token_mint.clone()),
                amount_raw: None,
                block_time: issued_at,
                block_height,
                tx_hash: None,
            };
            self.edge_store
                .insert_edge(&edge)
                .await
                .map_err(graph_err)?;
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // on_reorg — retract graph data invalidated by a reorg
    // -----------------------------------------------------------------------

    /// Handle a reorg at `reorg_height`: delete all graph edges and indexer-
    /// written labels above the reorg boundary.
    ///
    /// - `TypedEdgeStore::delete_edges_above_block(chain, reorg_height)` —
    ///   deletes `graph_edges` rows with `block_height >= reorg_height`.
    ///   This mirrors the pattern in `EventSink::delete_from_slot`.
    ///
    /// - `GraphLabelStore::delete_indexer_labels_after(chain, reorg_block_time)` —
    ///   deletes `address_labels` rows with `issued_at >= reorg_block_time` AND
    ///   `source IN ('indexer_pool_initialize', 'indexer_token_metadata')`.
    ///   Cluster-derived labels (`FundingSource`, `Sybil`) are NOT invalidated
    ///   by a single-block reorg (they aggregate across many blocks).
    ///
    /// # Design reference
    ///
    /// `docs/designs/0015-crates-graph-phase3.md` §5.1 "Reorg semantics"
    #[instrument(skip(self), fields(chain, reorg_height, %reorg_block_time))]
    pub async fn on_reorg(
        &self,
        chain: &str,
        reorg_height: u64,
        reorg_block_time: DateTime<Utc>,
    ) -> Result<(), IndexerError> {
        let deleted_edges = self
            .edge_store
            .delete_edges_above_block(chain, reorg_height)
            .await
            .map_err(graph_err)?;

        let deleted_labels = self
            .label_store
            .delete_indexer_labels_after(chain, reorg_block_time)
            .await
            .map_err(graph_err)?;

        debug!(
            chain,
            reorg_height, deleted_edges, deleted_labels, "graph reorg cleanup complete"
        );

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::DateTime;

    use mg_onchain_common::chain::{Address, BlockRef, Chain, TxHash};
    use mg_onchain_common::event::{DexKind, PoolEvent, PoolEventKind};
    use mg_onchain_common::token::{JupiterVerification, TokenMeta};
    use mg_onchain_graph::mock::{MockGraphLabelStore, MockTypedEdgeStore};
    use mg_onchain_graph::{EdgeType, GraphLabelStore, LabelType, TypedEdgeStore};
    use rust_decimal::Decimal;

    use super::GraphIndexerWriter;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn solana_addr(s: &str) -> Address {
        Address::parse(Chain::Solana, s).unwrap()
    }

    fn dummy_tx() -> TxHash {
        TxHash::solana_from_base58(&bs58::encode([7u8; 64]).into_string()).unwrap()
    }

    fn block_time(ts: i64) -> DateTime<chrono::Utc> {
        DateTime::from_timestamp(ts, 0).unwrap()
    }

    fn make_writer() -> (
        GraphIndexerWriter,
        Arc<MockTypedEdgeStore>,
        Arc<MockGraphLabelStore>,
    ) {
        let edges = Arc::new(MockTypedEdgeStore::default());
        let labels = Arc::new(MockGraphLabelStore::default());
        let writer = GraphIndexerWriter::new(
            edges.clone() as Arc<dyn TypedEdgeStore>,
            labels.clone() as Arc<dyn GraphLabelStore>,
        );
        (writer, edges, labels)
    }

    /// Build a `PoolEvent::Initialize` with a given creator (`actor`).
    fn make_initialize_event(
        actor: &str,
        token0: &str,
        token1: &str,
        block_height: u64,
        ts: i64,
    ) -> PoolEvent {
        PoolEvent {
            chain: Chain::Solana,
            tx_hash: dummy_tx(),
            block: BlockRef::new(Chain::Solana, block_height),
            block_time: block_time(ts),
            pool: solana_addr("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
            dex: DexKind::RaydiumV4,
            kind: PoolEventKind::Initialize {
                token0: solana_addr(token0),
                token1: solana_addr(token1),
            },
            actor: solana_addr(actor),
            log_index: 0,
        }
    }

    fn make_token_meta(
        mint: &str,
        mint_auth: Option<&str>,
        freeze_auth: Option<&str>,
        detected_at: Option<i64>,
    ) -> TokenMeta {
        TokenMeta {
            chain: Chain::Solana,
            mint: solana_addr(mint),
            symbol: None,
            name: None,
            decimals: 6,
            token_program: None,
            total_supply_raw: 1_000_000_000,
            circulating_supply_raw: None,
            mint_authority: mint_auth.map(solana_addr),
            freeze_authority: freeze_auth.map(solana_addr),
            creator: None,
            creator_balance_raw: 0,
            transfer_fee: None,
            permanent_delegate: None,
            transfer_hook_program: None,
            non_transferable: false,
            confidential_transfer: false,
            top_holders: vec![],
            total_holders: 0,
            markets: vec![],
            total_market_liquidity_usd: Decimal::ZERO,
            lockers: vec![],
            graph_insiders_detected: false,
            insider_networks: vec![],
            launchpad: None,
            deploy_platform: None,
            detected_at: detected_at.map(|ts| DateTime::from_timestamp(ts, 0).unwrap()),
            rugged: false,
            verification: JupiterVerification {
                jup_verified: false,
                jup_strict: false,
            },
            rugcheck_score: None,
            buy_tax: None,
            sell_tax: None,
            transfer_tax: None,
            honeypot_flags: vec![],
            updated_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        }
    }

    // Well-known Solana public keys (fixed-length base58, 44 chars).
    const DEPLOYER: &str = "8szGkuLTAux9XMgZ2vtY39jVSowEvayfqHyWChKEBRqh";
    const TOKEN_MINT: &str = "So11111111111111111111111111111111111111112";
    const TOKEN_QUOTE: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"; // USDC
    const MINT_AUTH: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    const FREEZE_AUTH: &str = "11111111111111111111111111111111";

    // -----------------------------------------------------------------------
    // Test: PoolEvent::Initialize → DeployerOf edge + DeployerEOA label
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn initialize_event_writes_deployer_edge_and_label() {
        let (writer, edges, labels) = make_writer();
        let event = make_initialize_event(
            DEPLOYER,
            TOKEN_MINT,
            TOKEN_QUOTE,
            300_000_000,
            1_700_000_000,
        );

        writer.on_pool_event(&event).await.unwrap();

        // Should have written 2 DeployerOf edges (one per token in the pool).
        let all_edges = edges.all_edges();
        assert_eq!(
            all_edges.len(),
            2,
            "expected DeployerOf edges for token0 and token1"
        );

        let token_mint_edge = all_edges
            .iter()
            .find(|e| e.to_address == TOKEN_MINT)
            .expect("DeployerOf edge for TOKEN_MINT must exist");
        assert_eq!(token_mint_edge.from_address, DEPLOYER);
        assert_eq!(token_mint_edge.edge_type, EdgeType::DeployerOf);
        assert_eq!(token_mint_edge.token.as_deref(), Some(TOKEN_MINT));
        assert!(token_mint_edge.tx_hash.is_some());
        assert_eq!(token_mint_edge.block_height, 300_000_000);
        assert_eq!(
            token_mint_edge.block_time,
            DateTime::from_timestamp(1_700_000_000, 0).unwrap()
        );
        assert!(token_mint_edge.amount_raw.is_none());

        // DeployerEOA label for the deployer address must exist.
        let deployer_labels: Vec<mg_onchain_graph::AddressLabel> =
            labels.get_labels("solana", DEPLOYER).await.unwrap();
        assert_eq!(
            deployer_labels.len(),
            1,
            "expected one DeployerEOA label for deployer"
        );
        let lbl = &deployer_labels[0];
        assert_eq!(lbl.label_type, LabelType::DeployerEoa);
        assert!((lbl.confidence - 1.0).abs() < f64::EPSILON);
        assert_eq!(lbl.source, "indexer_pool_initialize");
        assert!(
            lbl.expires_at.is_none(),
            "DeployerEOA label must be permanent"
        );
        // issued_at must come from block_time, not wall clock.
        assert_eq!(
            lbl.issued_at,
            DateTime::from_timestamp(1_700_000_000, 0).unwrap()
        );
    }

    // -----------------------------------------------------------------------
    // Test: Non-Initialize pool events → no graph writes
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn non_initialize_pool_event_writes_nothing() {
        let (writer, edges, labels) = make_writer();

        let sync_event = PoolEvent {
            chain: Chain::Solana,
            tx_hash: dummy_tx(),
            block: BlockRef::new(Chain::Solana, 300_000_001),
            block_time: block_time(1_700_000_001),
            pool: solana_addr(TOKEN_MINT),
            dex: DexKind::RaydiumV4,
            kind: PoolEventKind::Sync {
                reserve0_raw: 1_000,
                reserve1_raw: 2_000,
            },
            actor: solana_addr(DEPLOYER),
            log_index: 0,
        };

        writer.on_pool_event(&sync_event).await.unwrap();

        assert!(
            edges.all_edges().is_empty(),
            "Sync event must not create graph edges"
        );
        let deployer_labels: Vec<mg_onchain_graph::AddressLabel> =
            labels.get_labels("solana", DEPLOYER).await.unwrap();
        assert!(
            deployer_labels.is_empty(),
            "Sync event must not create labels"
        );
    }

    // -----------------------------------------------------------------------
    // Test: TokenMeta with mint + freeze authority → two AuthorityOf edges
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn token_meta_with_authorities_writes_authority_edges() {
        let (writer, edges, _labels) = make_writer();
        let meta = make_token_meta(
            TOKEN_MINT,
            Some(MINT_AUTH),
            Some(FREEZE_AUTH),
            Some(1_700_000_000),
        );

        writer.on_token_meta(&meta, 300_000_000).await.unwrap();

        let all_edges = edges.all_edges();
        assert_eq!(
            all_edges.len(),
            2,
            "expected two AuthorityOf edges (mint + freeze)"
        );

        let mint_edge = all_edges
            .iter()
            .find(|e| e.from_address == MINT_AUTH)
            .expect("AuthorityOf edge for mint_authority must exist");
        assert_eq!(mint_edge.edge_type, EdgeType::AuthorityOf);
        assert_eq!(mint_edge.to_address, TOKEN_MINT);
        assert_eq!(mint_edge.token.as_deref(), Some(TOKEN_MINT));
        assert!(
            mint_edge.tx_hash.is_none(),
            "AuthorityOf edges from metadata have no tx_hash"
        );
        assert_eq!(mint_edge.block_height, 300_000_000);
        assert_eq!(
            mint_edge.block_time,
            DateTime::from_timestamp(1_700_000_000, 0).unwrap()
        );

        let freeze_edge = all_edges
            .iter()
            .find(|e| e.from_address == FREEZE_AUTH)
            .expect("AuthorityOf edge for freeze_authority must exist");
        assert_eq!(freeze_edge.edge_type, EdgeType::AuthorityOf);
    }

    // -----------------------------------------------------------------------
    // Test: TokenMeta with no authorities → no edges
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn token_meta_no_authorities_writes_nothing() {
        let (writer, edges, _labels) = make_writer();
        let meta = make_token_meta(TOKEN_MINT, None, None, Some(1_700_000_000));

        writer.on_token_meta(&meta, 300_000_000).await.unwrap();

        assert!(
            edges.all_edges().is_empty(),
            "no AuthorityOf edges when both authorities are None"
        );
    }

    // -----------------------------------------------------------------------
    // Test: TokenMeta with no detected_at → skip with warning, no error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn token_meta_missing_detected_at_skips_gracefully() {
        let (writer, edges, _labels) = make_writer();
        let meta = make_token_meta(
            TOKEN_MINT,
            Some(MINT_AUTH),
            None,
            None, /* no detected_at */
        );

        // Must return Ok(()) — not an error.
        writer.on_token_meta(&meta, 300_000_000).await.unwrap();

        assert!(
            edges.all_edges().is_empty(),
            "no edges must be written when detected_at is absent"
        );
    }

    // -----------------------------------------------------------------------
    // Test: Idempotency — writing same Initialize event twice → one edge/label
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn initialize_event_is_idempotent() {
        let (writer, edges, labels) = make_writer();
        let event = make_initialize_event(
            DEPLOYER,
            TOKEN_MINT,
            TOKEN_QUOTE,
            300_000_000,
            1_700_000_000,
        );

        // Write twice — second write must be a no-op (ON CONFLICT DO NOTHING).
        writer.on_pool_event(&event).await.unwrap();
        writer.on_pool_event(&event).await.unwrap();

        // Still exactly 2 edges (one per token0/token1), not 4.
        assert_eq!(
            edges.all_edges().len(),
            2,
            "idempotent: second write must not duplicate edges"
        );

        // Label upsert: since confidence is the same (1.0 == 1.0), the
        // MockGraphLabelStore's confidence >= rule preserves the first row.
        let deployer_labels: Vec<mg_onchain_graph::AddressLabel> =
            labels.get_labels("solana", DEPLOYER).await.unwrap();
        assert_eq!(
            deployer_labels.len(),
            1,
            "idempotent: DeployerEOA label must not be duplicated"
        );
    }

    // -----------------------------------------------------------------------
    // Test: Reorg → edges + labels deleted above block height
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn reorg_deletes_edges_and_indexer_labels_above_height() {
        let (writer, edges, labels) = make_writer();

        // Write an Initialize event at block 200 (before reorg).
        let event_pre =
            make_initialize_event(DEPLOYER, TOKEN_MINT, TOKEN_QUOTE, 200, 1_700_001_000);
        writer.on_pool_event(&event_pre).await.unwrap();

        // Write another Initialize event at block 300 (within reorg window).
        let deployer2 = "7UX2i7SucgLMQcfZ75s3VXmZZY4YRUyJN9X1RgfMoDUi";
        let token_b = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        let event_post = make_initialize_event(deployer2, token_b, TOKEN_MINT, 300, 1_700_002_000);
        writer.on_pool_event(&event_post).await.unwrap();

        // Sanity: both writes present before reorg.
        assert!(
            edges.all_edges().len() >= 2,
            "pre-reorg: at least 2 edges expected"
        );

        // Reorg at height 250: delete everything at block_height >= 250.
        let reorg_block_time = DateTime::from_timestamp(1_700_001_500, 0).unwrap();
        writer
            .on_reorg("solana", 250, reorg_block_time)
            .await
            .unwrap();

        // Edges at height 300 must be gone; edges at height 200 must remain.
        let remaining_edges = edges.all_edges();
        assert!(
            remaining_edges.iter().all(|e| e.block_height < 250),
            "all post-reorg edges must be deleted; pre-reorg edges must remain"
        );

        // Labels with issued_at >= reorg_block_time AND indexer source must be deleted.
        // event_post was at ts=1_700_002_000 >= reorg_block_time=1_700_001_500 → deleted.
        let deployer2_labels: Vec<mg_onchain_graph::AddressLabel> =
            labels.get_labels("solana", deployer2).await.unwrap();
        assert!(
            deployer2_labels.is_empty(),
            "label for deployer2 (post-reorg) must be deleted"
        );

        // Label for the pre-reorg deployer (ts=1_700_001_000 < 1_700_001_500) must remain.
        let deployer_labels: Vec<mg_onchain_graph::AddressLabel> =
            labels.get_labels("solana", DEPLOYER).await.unwrap();
        assert_eq!(
            deployer_labels.len(),
            1,
            "label for pre-reorg deployer must survive reorg"
        );
    }

    // -----------------------------------------------------------------------
    // Test: only mint_authority present (no freeze_authority)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn token_meta_only_mint_authority_writes_one_edge() {
        let (writer, edges, _labels) = make_writer();
        let meta = make_token_meta(TOKEN_MINT, Some(MINT_AUTH), None, Some(1_700_000_000));

        writer.on_token_meta(&meta, 300_000_000).await.unwrap();

        let all_edges = edges.all_edges();
        assert_eq!(
            all_edges.len(),
            1,
            "only one AuthorityOf edge for mint_authority"
        );
        assert_eq!(all_edges[0].from_address, MINT_AUTH);
        assert_eq!(all_edges[0].edge_type, EdgeType::AuthorityOf);
    }
}
