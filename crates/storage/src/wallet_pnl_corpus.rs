//! `WalletPnlCorpusStore` trait and `PgWalletPnlCorpusStore` implementation.
//!
//! # What this module provides
//!
//! The `wallet_pnl_corpus` table (V00016) materializes per-wallet-token realized PnL
//! metrics computed by the smart-money labelling pipeline (`crates/graph/src/smart_money.rs`).
//!
//! One row per `(chain, wallet, token)` — updated in place by the `SmartMoneyLabeller`
//! background task on each 6-hour batch cycle.
//!
//! # String bridge (ADR 0002)
//!
//! All NUMERIC columns round-trip through TEXT: write `bind(value.to_string())` and
//! read `get::<String, _>()` → `Decimal::from_str()`. This matches the pattern used
//! by all other monetary columns in `pg.rs`.
//!
//! # Incremental update strategy
//!
//! `fetch_stale_wallets` returns wallets whose `last_updated` is before `since`,
//! enabling the labeller to recompute only wallets with new swap activity — a 10-100×
//! scope reduction vs full-table recompute at 100K+ wallet scale.
//!
//! # Design reference
//!
//! `docs/designs/0022-smart-money-labelling-mvp.md` §12 (Decision 4: materialized storage).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::str::FromStr;
use tracing::{debug, instrument};
use uuid::Uuid;

use mg_onchain_common::chain::{Address, Chain};

use crate::error::StorageError;

// ---------------------------------------------------------------------------
// WalletPnlCorpusRow
// ---------------------------------------------------------------------------

/// One row from the `wallet_pnl_corpus` table.
///
/// Mirrors V00016 columns. All monetary fields use `Decimal` (string-bridged via
/// NUMERIC ↔ TEXT per ADR 0002). Non-monetary percentage fields also use `Decimal`
/// to avoid `f64` for any stored quantity.
#[derive(Debug, Clone)]
pub struct WalletPnlCorpusRow {
    /// Postgres auto-increment primary key (set after INSERT; 0 for new rows).
    pub id: i64,
    /// Chain identifier (e.g. `"solana"`).
    pub chain: String,
    /// Canonical wallet address.
    pub wallet: String,
    /// Token mint / contract address.
    pub token: String,

    // --- Round-trip counts ---
    /// Total completed round-trips (including those with NULL pnl_usd).
    pub round_trip_count: i64,
    /// Round-trips with non-NULL price data (eligible for PnL computation).
    pub non_null_pnl_count: i64,

    // --- PnL metrics (None when non_null_pnl_count = 0) ---
    /// Sum of (exit_price - entry_price) * closed_qty over priced round-trips.
    pub total_pnl_usd: Option<Decimal>,
    /// Fraction of priced round-trips with positive PnL. `None` when no priced round-trips.
    pub win_rate: Option<Decimal>,
    /// Average holding time in seconds. `None` when `round_trip_count = 0`.
    pub mean_holding_time_secs: Option<Decimal>,

    // --- Stage 3 timing features ---
    /// Fraction of pump events where wallet sold before price peak.
    pub sell_before_peak_rate: Option<Decimal>,
    /// Count of distinct pump events where wallet appeared in pre-event window.
    pub recurrence_count: i64,
    /// Median entry lead vs event peak in seconds. `None` when `recurrence_count = 0`.
    pub median_timing_lead_secs: Option<Decimal>,
    /// Percentile rank among co-participants (0.0 = latest, 1.0 = earliest).
    pub timing_lead_pct_rank: Option<Decimal>,

    // --- Cross-token detail ---
    /// Top-10 tokens by absolute PnL as `{token_mint: "pnl_usd_string"}` JSON.
    pub per_token_pnl: Option<serde_json::Value>,

    // --- Audit ---
    pub first_trade_at: Option<DateTime<Utc>>,
    pub last_round_trip_at: Option<DateTime<Utc>>,
    /// When this row was last recomputed by the batch task.
    pub last_updated: DateTime<Utc>,
    /// UUID of the batch run that computed this row.
    pub batch_run_id: Uuid,
}

// ---------------------------------------------------------------------------
// WalletPnlCorpusStore trait
// ---------------------------------------------------------------------------

/// Read/write API for the `wallet_pnl_corpus` table.
///
/// Implementors must be `Send + Sync` (dyn-compatible via `#[async_trait]`).
/// This mirrors the `GraphLabelStore` and `TokenPriceProvider` trait patterns
/// established in Sprint 11 and Sprint 21 respectively.
#[async_trait]
pub trait WalletPnlCorpusStore: Send + Sync {
    /// Upsert a corpus row.
    ///
    /// Uses `ON CONFLICT (chain, wallet, token) DO UPDATE` to overwrite the
    /// existing row unconditionally (batch jobs always have fresher data than
    /// the stored row at the time of writing).
    async fn upsert_corpus_row(&self, row: &WalletPnlCorpusRow) -> Result<(), StorageError>;

    /// Fetch a single corpus row by `(chain, wallet, token)`.
    ///
    /// Returns `None` if no row exists for this wallet-token pair.
    async fn fetch_corpus_row(
        &self,
        chain: Chain,
        wallet: &Address,
        token: &str,
    ) -> Result<Option<WalletPnlCorpusRow>, StorageError>;

    /// Fetch all corpus rows for a chain updated since `since`.
    ///
    /// Used by the labeller to re-score wallets that had corpus updates in the
    /// most recent batch window. Ordered by `last_updated DESC`. Limited to
    /// `max_rows` (safety ceiling).
    async fn fetch_corpus_window(
        &self,
        chain: Chain,
        since: DateTime<Utc>,
        max_rows: u32,
    ) -> Result<Vec<WalletPnlCorpusRow>, StorageError>;

    /// Wallets on `chain` whose corpus row has `last_updated < since`.
    ///
    /// These are wallets that have had new swap activity but whose corpus has
    /// not been recomputed in the current batch window. The labeller calls this
    /// at the start of each batch cycle to find stale rows requiring recomputation.
    ///
    /// Returns only the wallet address string (not full corpus rows); the
    /// labeller recomputes the full corpus from the `swaps` table.
    async fn fetch_stale_wallets(
        &self,
        chain: Chain,
        since: DateTime<Utc>,
        max_rows: u32,
    ) -> Result<Vec<String>, StorageError>;
}

// ---------------------------------------------------------------------------
// PgWalletPnlCorpusStore
// ---------------------------------------------------------------------------

/// Postgres-backed implementation of [`WalletPnlCorpusStore`].
pub struct PgWalletPnlCorpusStore {
    pool: sqlx::PgPool,
}

impl PgWalletPnlCorpusStore {
    /// Construct a new store wrapping the given pool.
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl WalletPnlCorpusStore for PgWalletPnlCorpusStore {
    #[instrument(skip(self, row), fields(chain = %row.chain, wallet = %row.wallet, token = %row.token))]
    async fn upsert_corpus_row(&self, row: &WalletPnlCorpusRow) -> Result<(), StorageError> {
        sqlx::query(
            r#"
            INSERT INTO wallet_pnl_corpus (
                chain, wallet, token,
                round_trip_count, non_null_pnl_count,
                total_pnl_usd, win_rate, mean_holding_time_secs,
                sell_before_peak_rate, recurrence_count, median_timing_lead_secs,
                timing_lead_pct_rank, per_token_pnl,
                first_trade_at, last_round_trip_at, last_updated, batch_run_id
            )
            VALUES (
                $1, $2, $3,
                $4, $5,
                $6::NUMERIC, $7::NUMERIC, $8::NUMERIC,
                $9::NUMERIC, $10, $11::NUMERIC,
                $12::NUMERIC, $13,
                $14, $15, $16, $17
            )
            ON CONFLICT (chain, wallet, token) DO UPDATE
                SET round_trip_count        = EXCLUDED.round_trip_count,
                    non_null_pnl_count      = EXCLUDED.non_null_pnl_count,
                    total_pnl_usd           = EXCLUDED.total_pnl_usd,
                    win_rate                = EXCLUDED.win_rate,
                    mean_holding_time_secs  = EXCLUDED.mean_holding_time_secs,
                    sell_before_peak_rate   = EXCLUDED.sell_before_peak_rate,
                    recurrence_count        = EXCLUDED.recurrence_count,
                    median_timing_lead_secs = EXCLUDED.median_timing_lead_secs,
                    timing_lead_pct_rank    = EXCLUDED.timing_lead_pct_rank,
                    per_token_pnl           = EXCLUDED.per_token_pnl,
                    first_trade_at          = COALESCE(wallet_pnl_corpus.first_trade_at, EXCLUDED.first_trade_at),
                    last_round_trip_at      = EXCLUDED.last_round_trip_at,
                    last_updated            = EXCLUDED.last_updated,
                    batch_run_id            = EXCLUDED.batch_run_id
            "#,
        )
        .bind(&row.chain)
        .bind(&row.wallet)
        .bind(&row.token)
        .bind(row.round_trip_count)
        .bind(row.non_null_pnl_count)
        .bind(row.total_pnl_usd.map(|d| d.to_string()))
        .bind(row.win_rate.map(|d| d.to_string()))
        .bind(row.mean_holding_time_secs.map(|d| d.to_string()))
        .bind(row.sell_before_peak_rate.map(|d| d.to_string()))
        .bind(row.recurrence_count)
        .bind(row.median_timing_lead_secs.map(|d| d.to_string()))
        .bind(row.timing_lead_pct_rank.map(|d| d.to_string()))
        .bind(&row.per_token_pnl)
        .bind(row.first_trade_at)
        .bind(row.last_round_trip_at)
        .bind(row.last_updated)
        .bind(row.batch_run_id)
        .execute(&self.pool)
        .await?;

        debug!(chain = %row.chain, wallet = %row.wallet, token = %row.token, "corpus row upserted");
        Ok(())
    }

    #[instrument(skip(self), fields(%chain, wallet = %wallet.as_str()))]
    async fn fetch_corpus_row(
        &self,
        chain: Chain,
        wallet: &Address,
        token: &str,
    ) -> Result<Option<WalletPnlCorpusRow>, StorageError> {
        let row = sqlx::query(
            r#"
            SELECT id, chain, wallet, token,
                   round_trip_count, non_null_pnl_count,
                   total_pnl_usd::TEXT, win_rate::TEXT, mean_holding_time_secs::TEXT,
                   sell_before_peak_rate::TEXT, recurrence_count, median_timing_lead_secs::TEXT,
                   timing_lead_pct_rank::TEXT, per_token_pnl,
                   first_trade_at, last_round_trip_at, last_updated, batch_run_id
            FROM wallet_pnl_corpus
            WHERE chain = $1 AND wallet = $2 AND token = $3
            "#,
        )
        .bind(chain.as_str())
        .bind(wallet.as_str())
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|r| parse_corpus_row(&r)).transpose()
    }

    #[instrument(skip(self), fields(%chain, %since, max_rows))]
    async fn fetch_corpus_window(
        &self,
        chain: Chain,
        since: DateTime<Utc>,
        max_rows: u32,
    ) -> Result<Vec<WalletPnlCorpusRow>, StorageError> {
        let rows = sqlx::query(
            r#"
            SELECT id, chain, wallet, token,
                   round_trip_count, non_null_pnl_count,
                   total_pnl_usd::TEXT, win_rate::TEXT, mean_holding_time_secs::TEXT,
                   sell_before_peak_rate::TEXT, recurrence_count, median_timing_lead_secs::TEXT,
                   timing_lead_pct_rank::TEXT, per_token_pnl,
                   first_trade_at, last_round_trip_at, last_updated, batch_run_id
            FROM wallet_pnl_corpus
            WHERE chain = $1
              AND last_updated >= $2
            ORDER BY last_updated DESC
            LIMIT $3
            "#,
        )
        .bind(chain.as_str())
        .bind(since)
        .bind(i64::from(max_rows))
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(parse_corpus_row).collect()
    }

    #[instrument(skip(self), fields(%chain, %since, max_rows))]
    async fn fetch_stale_wallets(
        &self,
        chain: Chain,
        since: DateTime<Utc>,
        max_rows: u32,
    ) -> Result<Vec<String>, StorageError> {
        // Fetch distinct wallets with any corpus row older than `since`.
        // This identifies wallets that have had new swap activity but whose
        // corpus has not been refreshed in the current batch window.
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"
            SELECT DISTINCT wallet
            FROM wallet_pnl_corpus
            WHERE chain = $1
              AND last_updated < $2
            ORDER BY wallet
            LIMIT $3
            "#,
        )
        .bind(chain.as_str())
        .bind(since)
        .bind(i64::from(max_rows))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|(w,)| w).collect())
    }
}

// ---------------------------------------------------------------------------
// Row parser
// ---------------------------------------------------------------------------

fn parse_corpus_row(row: &sqlx::postgres::PgRow) -> Result<WalletPnlCorpusRow, StorageError> {
    use sqlx::Row as _;

    let id: i64 = row.try_get("id").map_err(StorageError::Postgres)?;
    let chain: String = row.try_get("chain").map_err(StorageError::Postgres)?;
    let wallet: String = row.try_get("wallet").map_err(StorageError::Postgres)?;
    let token: String = row.try_get("token").map_err(StorageError::Postgres)?;
    let round_trip_count: i64 = row.try_get("round_trip_count").map_err(StorageError::Postgres)?;
    let non_null_pnl_count: i64 = row.try_get("non_null_pnl_count").map_err(StorageError::Postgres)?;
    let recurrence_count: i64 = row.try_get("recurrence_count").map_err(StorageError::Postgres)?;

    let total_pnl_usd = parse_optional_decimal(row, "total_pnl_usd")?;
    let win_rate = parse_optional_decimal(row, "win_rate")?;
    let mean_holding_time_secs = parse_optional_decimal(row, "mean_holding_time_secs")?;
    let sell_before_peak_rate = parse_optional_decimal(row, "sell_before_peak_rate")?;
    let median_timing_lead_secs = parse_optional_decimal(row, "median_timing_lead_secs")?;
    let timing_lead_pct_rank = parse_optional_decimal(row, "timing_lead_pct_rank")?;

    let per_token_pnl: Option<serde_json::Value> = row.try_get("per_token_pnl").map_err(StorageError::Postgres)?;
    let first_trade_at: Option<DateTime<Utc>> = row.try_get("first_trade_at").map_err(StorageError::Postgres)?;
    let last_round_trip_at: Option<DateTime<Utc>> = row.try_get("last_round_trip_at").map_err(StorageError::Postgres)?;
    let last_updated: DateTime<Utc> = row.try_get("last_updated").map_err(StorageError::Postgres)?;
    let batch_run_id: Uuid = row.try_get("batch_run_id").map_err(StorageError::Postgres)?;

    Ok(WalletPnlCorpusRow {
        id,
        chain,
        wallet,
        token,
        round_trip_count,
        non_null_pnl_count,
        total_pnl_usd,
        win_rate,
        mean_holding_time_secs,
        sell_before_peak_rate,
        recurrence_count,
        median_timing_lead_secs,
        timing_lead_pct_rank,
        per_token_pnl,
        first_trade_at,
        last_round_trip_at,
        last_updated,
        batch_run_id,
    })
}

/// Parse an `Option<Decimal>` from a TEXT-cast NUMERIC column.
fn parse_optional_decimal(
    row: &sqlx::postgres::PgRow,
    col: &'static str,
) -> Result<Option<Decimal>, StorageError> {
    use sqlx::Row as _;
    let s: Option<String> = row
        .try_get(col)
        .map_err(StorageError::Postgres)?;
    match s {
        None => Ok(None),
        Some(ref txt) => Decimal::from_str(txt)
            .map(Some)
            .map_err(|e| StorageError::Other(format!("parse NUMERIC {col}: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// MockWalletPnlCorpusStore (test-utils)
// ---------------------------------------------------------------------------

/// In-memory [`WalletPnlCorpusStore`] for unit tests.
///
/// Gated by `#[cfg(any(test, feature = "test-utils"))]` — never compiled into
/// production builds without explicit opt-in. Mirrors the `MockTokenPriceProvider`
/// pattern from Sprint 21.
#[cfg(any(test, feature = "test-utils"))]
pub struct MockWalletPnlCorpusStore {
    rows: std::sync::Mutex<
        std::collections::BTreeMap<(String, String, String), WalletPnlCorpusRow>,
    >,
}

#[cfg(any(test, feature = "test-utils"))]
impl MockWalletPnlCorpusStore {
    /// Construct an empty store.
    pub fn new() -> Self {
        Self {
            rows: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Seed a corpus row directly (bypasses the standard upsert logic).
    pub fn seed(&self, row: WalletPnlCorpusRow) {
        let key = (row.chain.clone(), row.wallet.clone(), row.token.clone());
        self.rows.lock().unwrap().insert(key, row);
    }

    /// Return all stored rows for assertions.
    pub fn all_rows(&self) -> Vec<WalletPnlCorpusRow> {
        self.rows.lock().unwrap().values().cloned().collect()
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl Default for MockWalletPnlCorpusStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "test-utils"))]
#[async_trait]
impl WalletPnlCorpusStore for MockWalletPnlCorpusStore {
    async fn upsert_corpus_row(&self, row: &WalletPnlCorpusRow) -> Result<(), StorageError> {
        let key = (row.chain.clone(), row.wallet.clone(), row.token.clone());
        self.rows.lock().unwrap().insert(key, row.clone());
        Ok(())
    }

    async fn fetch_corpus_row(
        &self,
        chain: Chain,
        wallet: &Address,
        token: &str,
    ) -> Result<Option<WalletPnlCorpusRow>, StorageError> {
        let key = (chain.as_str().to_owned(), wallet.as_str().to_owned(), token.to_owned());
        Ok(self.rows.lock().unwrap().get(&key).cloned())
    }

    async fn fetch_corpus_window(
        &self,
        chain: Chain,
        since: DateTime<Utc>,
        max_rows: u32,
    ) -> Result<Vec<WalletPnlCorpusRow>, StorageError> {
        let guard = self.rows.lock().unwrap();
        let mut result: Vec<WalletPnlCorpusRow> = guard
            .values()
            .filter(|r| r.chain == chain.as_str() && r.last_updated >= since)
            .cloned()
            .collect();
        result.sort_by_key(|r| std::cmp::Reverse(r.last_updated));
        result.truncate(max_rows as usize);
        Ok(result)
    }

    async fn fetch_stale_wallets(
        &self,
        chain: Chain,
        since: DateTime<Utc>,
        max_rows: u32,
    ) -> Result<Vec<String>, StorageError> {
        let guard = self.rows.lock().unwrap();
        // Collect distinct wallets with any stale corpus row.
        // BTreeSet is sorted — deterministic ordering is preserved.
        let wallets: std::collections::BTreeSet<String> = guard
            .values()
            .filter(|r| r.chain == chain.as_str() && r.last_updated < since)
            .map(|r| r.wallet.clone())
            .collect();
        let result: Vec<String> = wallets.iter().take(max_rows as usize).cloned().collect();
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use uuid::Uuid;

    // Valid Solana base58 public keys used as fixture addresses.
    // System Program: 11111111111111111111111111111111 (32 zero-bytes in base58).
    // TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA — SPL Token Program (well-known).
    // These pass `Address::parse(Chain::Solana, ...)` without RPC calls.
    const SOLANA_WALLET_A: &str = "11111111111111111111111111111111";
    const SOLANA_WALLET_B: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    const SOLANA_MINT_X: &str = "So11111111111111111111111111111111111111112";   // Wrapped SOL
    const SOLANA_MINT_Y: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"; // USDC

    fn make_row(chain: &str, wallet: &str, token: &str, last_updated: DateTime<Utc>) -> WalletPnlCorpusRow {
        WalletPnlCorpusRow {
            id: 0,
            chain: chain.to_owned(),
            wallet: wallet.to_owned(),
            token: token.to_owned(),
            round_trip_count: 10,
            non_null_pnl_count: 8,
            total_pnl_usd: Some(Decimal::from(5000)),
            win_rate: Some(Decimal::from_str("0.6").unwrap()),
            mean_holding_time_secs: Some(Decimal::from(3600)),
            sell_before_peak_rate: None,
            recurrence_count: 2,
            median_timing_lead_secs: None,
            timing_lead_pct_rank: None,
            per_token_pnl: None,
            first_trade_at: None,
            last_round_trip_at: None,
            last_updated,
            batch_run_id: Uuid::new_v4(),
        }
    }

    #[tokio::test]
    async fn mock_store_upsert_and_fetch() {
        let store = MockWalletPnlCorpusStore::new();
        let t0 = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let row = make_row("solana", SOLANA_WALLET_A, SOLANA_MINT_X, t0);
        store.upsert_corpus_row(&row).await.unwrap();

        let chain = Chain::Solana;
        let addr = Address::parse(chain, SOLANA_WALLET_A)
            .expect("SOLANA_WALLET_A is a valid Solana base58 address");
        let fetched = store.fetch_corpus_row(chain, &addr, SOLANA_MINT_X).await.unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.wallet, SOLANA_WALLET_A);
        assert_eq!(fetched.round_trip_count, 10);
    }

    #[tokio::test]
    async fn mock_store_fetch_none_for_unknown_wallet() {
        let store = MockWalletPnlCorpusStore::new();
        let chain = Chain::Solana;
        // Use a valid address not seeded in the store.
        let addr = Address::parse(chain, SOLANA_WALLET_B)
            .expect("SOLANA_WALLET_B is a valid Solana base58 address");
        let result = store.fetch_corpus_row(chain, &addr, SOLANA_MINT_X).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn mock_store_upsert_overwrites() {
        let store = MockWalletPnlCorpusStore::new();
        let t0 = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let t1 = Utc.timestamp_opt(1_700_010_000, 0).unwrap();

        store.upsert_corpus_row(&make_row("solana", SOLANA_WALLET_A, SOLANA_MINT_X, t0)).await.unwrap();

        let mut row2 = make_row("solana", SOLANA_WALLET_A, SOLANA_MINT_X, t1);
        row2.round_trip_count = 20;
        store.upsert_corpus_row(&row2).await.unwrap();

        let chain = Chain::Solana;
        let addr = Address::parse(chain, SOLANA_WALLET_A)
            .expect("SOLANA_WALLET_A is a valid Solana base58 address");
        let fetched = store
            .fetch_corpus_row(chain, &addr, SOLANA_MINT_X)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.round_trip_count, 20, "upsert must overwrite with newer data");
    }

    #[tokio::test]
    async fn mock_store_fetch_corpus_window_filters_by_since() {
        let store = MockWalletPnlCorpusStore::new();
        let t0 = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let t1 = Utc.timestamp_opt(1_700_010_000, 0).unwrap();
        let since = Utc.timestamp_opt(1_700_005_000, 0).unwrap();

        store.upsert_corpus_row(&make_row("solana", SOLANA_WALLET_A, SOLANA_MINT_X, t0)).await.unwrap();
        store.upsert_corpus_row(&make_row("solana", SOLANA_WALLET_B, SOLANA_MINT_Y, t1)).await.unwrap();

        let rows = store.fetch_corpus_window(Chain::Solana, since, 100).await.unwrap();
        // Only WALLET_B (t1 >= since); WALLET_A (t0 < since) excluded.
        assert_eq!(rows.len(), 1, "only rows updated after 'since' returned");
        assert_eq!(rows[0].wallet, SOLANA_WALLET_B);
    }

    #[tokio::test]
    async fn mock_store_fetch_stale_wallets_returns_wallets_before_since() {
        let store = MockWalletPnlCorpusStore::new();
        let t0 = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let t1 = Utc.timestamp_opt(1_700_010_000, 0).unwrap();
        let since = Utc.timestamp_opt(1_700_005_000, 0).unwrap();

        store.upsert_corpus_row(&make_row("solana", SOLANA_WALLET_A, SOLANA_MINT_X, t0)).await.unwrap();
        store.upsert_corpus_row(&make_row("solana", SOLANA_WALLET_B, SOLANA_MINT_Y, t1)).await.unwrap();

        let stale = store.fetch_stale_wallets(Chain::Solana, since, 100).await.unwrap();
        // WALLET_A (t0 < since) is stale; WALLET_B (t1 >= since) is current.
        assert_eq!(stale, vec![SOLANA_WALLET_A.to_owned()]);
    }
}
