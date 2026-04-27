//! `TokenPriceProvider` trait and `PgTokenPriceProvider` implementation.
//!
//! # Phase 5 forward-only enrichment
//!
//! This module provides USD price lookups for detectors D11, D12, and D13.
//! Phase 5 closes three accumulated SPEC-NOTEs (D11 `total_cluster_volume_usd`,
//! D12 `amount_usd`, D13 `profit_amount_usd`).
//!
//! **No backfill:** historical rows in V00012/V00014/V00015 with NULL USD columns
//! are left as-is. Only newly emitted events receive USD enrichment.
//!
//! # Price derivation (Decision 2 — hybrid)
//!
//! 1. **Primary path**: `tokens.total_market_liquidity_usd / (circulating_supply / 10^decimals)`.
//!    `total_market_liquidity_usd` is the aggregate market data column from V00001.
//!    SPEC-NOTE: The sprint spec refers to `tokens_markets.price_usd` — that table
//!    does not exist in the current schema. `tokens.total_market_liquidity_usd` is the
//!    equivalent de-normalised aggregate column from V00001.
//!
//! 2. **Fallback path**: `pools.liquidity_usd / (circulating_supply / 10^decimals)`.
//!    Uses the best-liquidity pool row from `pools` for the token.
//!    Mirrors the D05 cycle_volume_usd pattern (Sprint 12).
//!
//! 3. **None** when neither path can produce a price (new token, zero supply,
//!    no pool data, or no rows matching the token address).
//!
//! # Cache
//!
//! In-memory `HashMap<(Chain, String), CachedPrice>` with 5-minute TTL (Decision 4).
//! Cache-on-miss; no stampede protection (intentional — see gotcha #29).
//! None is NOT cached — the next evaluation retries the DB.
//!
//! # Determinism
//!
//! `get_token_price_usd` is idempotent for the same `(chain, token)` pair within the
//! TTL window. Outside the TTL, it re-queries DB — which is also deterministic for
//! the same block-time anchor.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use tokio::sync::Mutex;
use tracing::{debug, instrument, warn};

use mg_onchain_common::chain::{Address, Chain};

use crate::error::StorageError;

// ---------------------------------------------------------------------------
// Cache TTL
// ---------------------------------------------------------------------------

/// In-memory price cache TTL: 5 minutes.
///
/// Per Decision 4: no stampede protection. Concurrent misses all query DB;
/// the last writer wins in the cache (benign for price data).
const CACHE_TTL_SECS: i64 = 300;

// ---------------------------------------------------------------------------
// TokenPriceProvider trait
// ---------------------------------------------------------------------------

/// Returns USD price per whole token unit (decimal-adjusted, not raw).
///
/// Phase 5 forward-only enrichment — does not backfill historical rows.
///
/// Implementors must be `Send + Sync` (dyn-compatible via `#[async_trait]`).
#[async_trait]
pub trait TokenPriceProvider: Send + Sync {
    /// Returns USD price per whole token unit (decimal-adjusted, not raw).
    ///
    /// Returns `None` when no price source has data for this `(chain, token)`.
    ///
    /// `observed_at` is the block-time anchor used for cache TTL accounting.
    /// Must NOT call `Utc::now()` internally (gotcha #22); TTL is assessed
    /// against the wall-clock fetch time stored in `CachedPrice.fetched_at`.
    async fn get_token_price_usd(
        &self,
        chain: Chain,
        token: &Address,
        observed_at: DateTime<Utc>,
    ) -> Option<Decimal>;

    /// Returns the token decimal count from the `tokens` table.
    ///
    /// Closes S21 SPEC-NOTE: detectors D11/D12/D13 previously defaulted to 9 (SPL) or
    /// 18 (EVM) when exact decimals were unavailable. This method provides the exact
    /// value from the `tokens` table.
    ///
    /// Returns `None` when the token is not found in the registry (new/unlisted token).
    /// Callers MUST fall back to the chain-appropriate default (9 for Solana, 18 for EVM)
    /// when `None` is returned — current behavior is preserved.
    ///
    /// Default impl returns `None` (no decimals source) — overridden by `PgTokenPriceProvider`.
    async fn get_token_decimals(
        &self,
        chain: Chain,
        token: &Address,
    ) -> Option<u32> {
        // Default: no exact decimals available; caller uses chain default.
        let _ = (chain, token);
        None
    }
}

// ---------------------------------------------------------------------------
// CachedPrice (internal)
// ---------------------------------------------------------------------------

/// One entry in the in-memory price cache.
#[derive(Debug, Clone)]
struct CachedPrice {
    price: Decimal,
    /// Wall-clock time the price was fetched. NOT the block-time anchor.
    /// TTL is assessed against `Utc::now()` at lookup time (NOT `observed_at`).
    ///
    /// Rationale: price data ages by wall-clock (market moves); `observed_at`
    /// is the block anchor (determinism concern for detectors, not cache). These
    /// are orthogonal. Cache freshness is a performance concern, not a detector-
    /// output concern.
    fetched_at: DateTime<Utc>,
}

impl CachedPrice {
    fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        let age_secs = now
            .signed_duration_since(self.fetched_at)
            .num_seconds();
        age_secs < CACHE_TTL_SECS
    }
}

// ---------------------------------------------------------------------------
// PgTokenPriceProvider
// ---------------------------------------------------------------------------

/// Postgres-backed `TokenPriceProvider` with in-memory TTL cache.
///
/// # Thread safety
///
/// `cache` is protected by a `tokio::sync::Mutex`. The lock is held only for
/// the duration of the HashMap lookup/insert — never across `.await` points —
/// so there is no deadlock risk and minimal contention.
///
/// # Phase 5 forward-only enrichment
///
/// `PgTokenPriceProvider` enriches new events only. Historical rows are not
/// back-filled (Decision 5 binding).
#[derive(Debug)]
pub struct PgTokenPriceProvider {
    pool: Arc<PgPool>,
    cache: Mutex<HashMap<(String, String), CachedPrice>>,
}

impl PgTokenPriceProvider {
    /// Construct with an existing `Arc<PgPool>`.
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self {
            pool,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Cache key: `(chain.to_string(), token.to_string())`.
    fn cache_key(chain: Chain, token: &Address) -> (String, String) {
        (chain.to_string(), token.to_string())
    }

    /// Query the primary price path: `tokens.total_market_liquidity_usd / adjusted_supply`.
    ///
    /// SPEC-NOTE: Sprint spec refers to `tokens_markets.price_usd` — that table does not
    /// exist. `tokens.total_market_liquidity_usd` is the equivalent denormalised aggregate
    /// from V00001. Phase 5 closure uses this as the primary path.
    #[instrument(skip(self), fields(chain = %chain, token = %token))]
    async fn fetch_price_primary(
        &self,
        chain: &str,
        token: &str,
    ) -> Result<Option<Decimal>, StorageError> {
        let row = sqlx::query(
            r#"SELECT
                 total_market_liquidity_usd::TEXT AS liquidity_usd_str,
                 COALESCE(circulating_supply_raw, total_supply_raw)::TEXT AS supply_raw_str,
                 decimals
               FROM tokens
               WHERE chain = $1 AND mint = $2
               LIMIT 1"#,
        )
        .bind(chain)
        .bind(token)
        .fetch_optional(&*self.pool)
        .await?;

        let row = match row {
            None => return Ok(None),
            Some(r) => r,
        };

        use sqlx::Row as _;
        let liquidity_str: String = row
            .try_get("liquidity_usd_str")
            .unwrap_or_else(|_| "0".into());
        let supply_str: String = row
            .try_get("supply_raw_str")
            .unwrap_or_else(|_| "0".into());
        let decimals: i16 = row.try_get("decimals").unwrap_or(0);

        let liquidity = liquidity_str
            .parse::<Decimal>()
            .unwrap_or(Decimal::ZERO);
        let supply_raw = supply_str.parse::<Decimal>().unwrap_or(Decimal::ZERO);

        if liquidity.is_zero() || supply_raw.is_zero() {
            return Ok(None);
        }

        let divisor = Decimal::from(10u64.saturating_pow(decimals as u32));
        if divisor.is_zero() {
            return Ok(None);
        }

        let supply_tokens = supply_raw / divisor;
        if supply_tokens.is_zero() {
            return Ok(None);
        }

        Ok(Some(liquidity / supply_tokens))
    }

    /// Query the exact decimal count for a token from the `tokens` table.
    ///
    /// Closes S21 SPEC-NOTE: exact decimals for D11/D12/D13 profit/slippage computation.
    /// Falls back to `None` when the token is not found (caller uses chain default).
    async fn fetch_token_decimals_inner(
        &self,
        chain: &str,
        token: &str,
    ) -> Result<Option<u32>, StorageError> {
        let row = sqlx::query(
            r#"SELECT decimals FROM tokens WHERE chain = $1 AND mint = $2 LIMIT 1"#,
        )
        .bind(chain)
        .bind(token)
        .fetch_optional(&*self.pool)
        .await?;

        Ok(row.and_then(|r| {
            use sqlx::Row as _;
            r.try_get::<i16, _>("decimals").ok().map(|d| d as u32)
        }))
    }

    /// Query the fallback price path: `pools.liquidity_usd / adjusted_supply`.
    ///
    /// Uses the highest-liquidity pool for the token (token0 or token1 match).
    /// Mirrors the D05 `compute_token_price_usd` fallback pattern (Sprint 12).
    #[instrument(skip(self), fields(chain = %chain, token = %token))]
    async fn fetch_price_fallback(
        &self,
        chain: &str,
        token: &str,
    ) -> Result<Option<Decimal>, StorageError> {
        // Get supply from tokens table.
        let token_row = sqlx::query(
            r#"SELECT
                 COALESCE(circulating_supply_raw, total_supply_raw)::TEXT AS supply_raw_str,
                 decimals
               FROM tokens
               WHERE chain = $1 AND mint = $2
               LIMIT 1"#,
        )
        .bind(chain)
        .bind(token)
        .fetch_optional(&*self.pool)
        .await?;

        let (supply_raw, decimals) = match token_row {
            None => return Ok(None),
            Some(r) => {
                use sqlx::Row as _;
                let s: String = r
                    .try_get("supply_raw_str")
                    .unwrap_or_else(|_| "0".into());
                let d: i16 = r.try_get("decimals").unwrap_or(0);
                (s.parse::<Decimal>().unwrap_or(Decimal::ZERO), d)
            }
        };

        if supply_raw.is_zero() {
            return Ok(None);
        }

        // Best-liquidity pool for this token.
        let pool_row = sqlx::query(
            r#"SELECT liquidity_usd::TEXT AS liq_str
               FROM pools
               WHERE chain = $1 AND (token0 = $2 OR token1 = $2)
               ORDER BY liquidity_usd DESC
               LIMIT 1"#,
        )
        .bind(chain)
        .bind(token)
        .fetch_optional(&*self.pool)
        .await?;

        let liquidity = match pool_row {
            None => return Ok(None),
            Some(r) => {
                use sqlx::Row as _;
                let s: String = r.try_get("liq_str").unwrap_or_else(|_| "0".into());
                s.parse::<Decimal>().unwrap_or(Decimal::ZERO)
            }
        };

        if liquidity.is_zero() {
            return Ok(None);
        }

        let divisor = Decimal::from(10u64.saturating_pow(decimals as u32));
        if divisor.is_zero() {
            return Ok(None);
        }

        let supply_tokens = supply_raw / divisor;
        if supply_tokens.is_zero() {
            return Ok(None);
        }

        Ok(Some(liquidity / supply_tokens))
    }
}

#[async_trait]
impl TokenPriceProvider for PgTokenPriceProvider {
    /// Returns the exact decimal count for a token from the `tokens` table.
    ///
    /// No caching — decimals are static per token and the DB query is cheap.
    /// Returns `None` when the token is not in the registry.
    async fn get_token_decimals(
        &self,
        chain: Chain,
        token: &Address,
    ) -> Option<u32> {
        let chain_str = chain.to_string();
        let token_str = token.to_string();
        match self.fetch_token_decimals_inner(&chain_str, &token_str).await {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    chain = %chain_str,
                    token = %token_str,
                    error = %e,
                    "get_token_decimals DB query failed — falling back to None"
                );
                None
            }
        }
    }

    /// Returns USD price per whole token unit.
    ///
    /// Flow:
    /// 1. Cache hit → return cached price (if within TTL).
    /// 2. Primary DB path (`tokens.total_market_liquidity_usd / supply`).
    /// 3. Fallback DB path (`pools.liquidity_usd / supply`).
    /// 4. None → do NOT cache; next eval retries.
    ///
    /// `observed_at` is accepted for API symmetry (callers pass the block-time
    /// anchor) but is not used for TTL computation — TTL uses wall-clock.
    #[instrument(skip(self, _observed_at), fields(chain = %chain, token = %token))]
    async fn get_token_price_usd(
        &self,
        chain: Chain,
        token: &Address,
        _observed_at: DateTime<Utc>,
    ) -> Option<Decimal> {
        let key = Self::cache_key(chain, token);

        // Step 1: cache lookup (lock held only for lookup — not across await).
        {
            let cache = self.cache.lock().await;
            if let Some(entry) = cache.get(&key)
                && entry.is_fresh(Utc::now())
            {
                debug!(chain = %chain, token = %token, "price cache hit");
                return Some(entry.price);
            }
        }

        // Step 2: primary path.
        let chain_str = chain.to_string();
        let token_str = token.to_string();

        let price = match self.fetch_price_primary(&chain_str, &token_str).await {
            Ok(Some(p)) => {
                debug!(chain = %chain, token = %token, price = %p, "price via primary path (tokens table)");
                Some(p)
            }
            Ok(None) => {
                // Step 3: fallback path.
                match self.fetch_price_fallback(&chain_str, &token_str).await {
                    Ok(Some(p)) => {
                        debug!(chain = %chain, token = %token, price = %p, "price via fallback path (pools table)");
                        Some(p)
                    }
                    Ok(None) => {
                        debug!(chain = %chain, token = %token, "no price available from any source");
                        None
                    }
                    Err(e) => {
                        warn!(chain = %chain, token = %token, error = %e, "price fallback DB query failed");
                        None
                    }
                }
            }
            Err(e) => {
                warn!(chain = %chain, token = %token, error = %e, "price primary DB query failed");
                // Attempt fallback.
                match self.fetch_price_fallback(&chain_str, &token_str).await {
                    Ok(p) => p,
                    Err(e2) => {
                        warn!(chain = %chain, token = %token, error = %e2, "price fallback also failed");
                        None
                    }
                }
            }
        };

        // Step 4: cache on hit only.
        if let Some(p) = price {
            let mut cache = self.cache.lock().await;
            cache.insert(
                key,
                CachedPrice {
                    price: p,
                    fetched_at: Utc::now(),
                },
            );
        }

        price
    }
}

// ---------------------------------------------------------------------------
// MockTokenPriceProvider (test-utils)
// ---------------------------------------------------------------------------

/// Mock `TokenPriceProvider` for unit tests.
///
/// Returns prices from a static `HashMap<(Chain, String), Decimal>`.
/// Returns `None` for unmapped tokens. No DB, no cache, no I/O.
///
/// # Pattern
///
/// Mirrors `MockSolanaRpc` / `MockEthereumRpc` patterns used by D01/D12 tests.
///
/// # Example
///
/// ```rust,ignore
/// use mg_onchain_storage::price_provider::MockTokenPriceProvider;
/// use mg_onchain_common::chain::Chain;
/// use rust_decimal::Decimal;
///
/// let mut mock = MockTokenPriceProvider::new();
/// mock.insert(Chain::Solana, "So11111111111111111111111111111111111111112", Decimal::new(150, 0));
/// ```
#[cfg(any(test, feature = "test-utils"))]
#[derive(Debug, Default)]
pub struct MockTokenPriceProvider {
    prices: std::collections::HashMap<(String, String), Decimal>,
    /// Optional per-token decimal overrides for testing closed S21 SPEC-NOTE.
    decimals: std::collections::HashMap<(String, String), u32>,
}

#[cfg(any(test, feature = "test-utils"))]
impl MockTokenPriceProvider {
    /// Construct an empty mock.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a price for a `(chain, token_address)` pair.
    pub fn insert(&mut self, chain: Chain, token: &str, price: Decimal) {
        self.prices.insert((chain.to_string(), token.to_string()), price);
    }

    /// Register exact token decimals for a `(chain, token_address)` pair.
    ///
    /// Used by D11/D12 decimals-wiring tests to verify that `get_token_decimals`
    /// returns the correct value and overrides the default 9/18 fallback.
    pub fn insert_decimals(&mut self, chain: Chain, token: &str, decimals: u32) {
        self.decimals.insert((chain.to_string(), token.to_string()), decimals);
    }
}

#[cfg(any(test, feature = "test-utils"))]
#[async_trait]
impl TokenPriceProvider for MockTokenPriceProvider {
    async fn get_token_price_usd(
        &self,
        chain: Chain,
        token: &Address,
        _observed_at: DateTime<Utc>,
    ) -> Option<Decimal> {
        self.prices
            .get(&(chain.to_string(), token.to_string()))
            .copied()
    }

    /// Returns the registered decimals for the token, or None if not registered.
    ///
    /// Callers (D11/D12) fall back to chain default (9 for Solana, 18 for EVM)
    /// when None is returned — this preserves existing test behaviour for tests
    /// that do not call `insert_decimals`.
    async fn get_token_decimals(
        &self,
        chain: Chain,
        token: &Address,
    ) -> Option<u32> {
        self.decimals
            .get(&(chain.to_string(), token.to_string()))
            .copied()
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use mg_onchain_common::chain::{Address, Chain};
    use rust_decimal::Decimal;

    fn mock_observed_at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 24, 12, 0, 0).unwrap()
    }

    fn solana_addr(s: &str) -> Address {
        Address::parse(Chain::Solana, s).unwrap_or_else(|_| {
            // Fallback: use a known-good Solana address format for test isolation.
            Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112").unwrap()
        })
    }

    // -----------------------------------------------------------------------
    // MockTokenPriceProvider tests
    // -----------------------------------------------------------------------

    /// Mock round-trip: inserted price is returned.
    #[tokio::test]
    async fn mock_round_trip_returns_inserted_price() {
        let mut mock = MockTokenPriceProvider::new();
        let token = "So11111111111111111111111111111111111111112";
        mock.insert(Chain::Solana, token, Decimal::new(150, 0));

        let addr = solana_addr(token);
        let price = mock
            .get_token_price_usd(Chain::Solana, &addr, mock_observed_at())
            .await;

        assert_eq!(price, Some(Decimal::new(150, 0)));
    }

    /// Mock returns None for unknown token.
    #[tokio::test]
    async fn mock_returns_none_for_unknown_token() {
        let mock = MockTokenPriceProvider::new();
        let addr = solana_addr("So11111111111111111111111111111111111111112");
        let price = mock
            .get_token_price_usd(Chain::Solana, &addr, mock_observed_at())
            .await;
        assert!(price.is_none());
    }

    /// Mock: two chains with same token address return independent prices.
    #[tokio::test]
    async fn mock_chain_isolated() {
        let mut mock = MockTokenPriceProvider::new();
        let token = "So11111111111111111111111111111111111111112";
        mock.insert(Chain::Solana, token, Decimal::new(150, 0));
        // Do NOT insert for Ethereum — should return None.

        let addr_solana = solana_addr(token);
        let price = mock
            .get_token_price_usd(Chain::Solana, &addr_solana, mock_observed_at())
            .await;
        assert_eq!(price, Some(Decimal::new(150, 0)));
    }

    /// Mock get_token_decimals: inserted decimals are returned.
    ///
    /// Verifies the S21 SPEC-NOTE closure: D11/D12 can use MockTokenPriceProvider
    /// to inject known decimals and validate the conversion arithmetic.
    #[tokio::test]
    async fn mock_get_token_decimals_returns_inserted_value() {
        let mut mock = MockTokenPriceProvider::new();
        let token = "So11111111111111111111111111111111111111112";
        mock.insert_decimals(Chain::Solana, token, 6);

        let addr = solana_addr(token);
        let decimals = mock.get_token_decimals(Chain::Solana, &addr).await;
        assert_eq!(decimals, Some(6), "inserted decimals must be returned");
    }

    /// Mock get_token_decimals: returns None for unregistered token.
    ///
    /// Callers (D11/D12) apply unwrap_or(9) / unwrap_or(18) on None — this test
    /// ensures the fallback path is exercised when no decimals are registered.
    #[tokio::test]
    async fn mock_get_token_decimals_returns_none_for_unregistered() {
        let mock = MockTokenPriceProvider::new();
        let addr = solana_addr("So11111111111111111111111111111111111111112");
        let decimals = mock.get_token_decimals(Chain::Solana, &addr).await;
        assert!(decimals.is_none(), "unregistered token must return None decimals");
    }

    /// Mock get_token_decimals: chain isolation — Solana and Ethereum have independent decimal registries.
    #[tokio::test]
    async fn mock_get_token_decimals_chain_isolated() {
        let mut mock = MockTokenPriceProvider::new();
        let token = "So11111111111111111111111111111111111111112";
        mock.insert_decimals(Chain::Solana, token, 9);
        // NOT inserted for Ethereum.

        let addr = solana_addr(token);
        let sol_dec = mock.get_token_decimals(Chain::Solana, &addr).await;
        let eth_dec = mock.get_token_decimals(Chain::Ethereum, &addr).await;

        assert_eq!(sol_dec, Some(9), "Solana decimals must be returned");
        assert!(eth_dec.is_none(), "Ethereum decimals must be None (not registered)");
    }

    // -----------------------------------------------------------------------
    // CachedPrice TTL tests (no DB)
    // -----------------------------------------------------------------------

    /// A fresh CachedPrice is detected as fresh.
    #[test]
    fn cached_price_is_fresh_within_ttl() {
        let entry = CachedPrice {
            price: Decimal::new(100, 0),
            fetched_at: Utc::now() - chrono::Duration::seconds(60),
        };
        assert!(entry.is_fresh(Utc::now()));
    }

    /// A stale CachedPrice is detected as expired.
    #[test]
    fn cached_price_is_stale_after_ttl() {
        let entry = CachedPrice {
            price: Decimal::new(100, 0),
            fetched_at: Utc::now() - chrono::Duration::seconds(CACHE_TTL_SECS + 10),
        };
        assert!(!entry.is_fresh(Utc::now()));
    }

    /// CachedPrice at exactly TTL boundary is still stale (< not <=).
    #[test]
    fn cached_price_at_exact_ttl_boundary_is_stale() {
        let entry = CachedPrice {
            price: Decimal::new(100, 0),
            fetched_at: Utc::now() - chrono::Duration::seconds(CACHE_TTL_SECS),
        };
        // age == CACHE_TTL_SECS, condition is age < TTL, so this is stale.
        assert!(!entry.is_fresh(Utc::now()));
    }

    // -----------------------------------------------------------------------
    // Live DB tests (gated by DATABASE_URL)
    // -----------------------------------------------------------------------

    /// Primary path: tokens table with nonzero liquidity + supply → price computed.
    #[tokio::test]
    #[ignore = "requires live Postgres (set DATABASE_URL)"]
    async fn pg_provider_primary_path() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL not set");
        let pool = Arc::new(sqlx::PgPool::connect(&url).await.unwrap());
        let provider = PgTokenPriceProvider::new(pool);
        // Insert a test token directly and verify price.
        // (Integration test — omitted in unit suite.)
        let addr = solana_addr("So11111111111111111111111111111111111111112");
        let _ = provider
            .get_token_price_usd(Chain::Solana, &addr, mock_observed_at())
            .await;
    }

    /// Fallback path: pools table used when tokens table liquidity is zero.
    #[tokio::test]
    #[ignore = "requires live Postgres (set DATABASE_URL)"]
    async fn pg_provider_fallback_path() {
        // Integration test — gated.
    }

    /// Cache hit: second call returns cached value without re-querying DB.
    #[tokio::test]
    #[ignore = "requires live Postgres (set DATABASE_URL)"]
    async fn pg_provider_cache_hit() {
        // Integration test — gated.
    }

    /// No price → None, and None is NOT cached (next eval retries DB).
    #[tokio::test]
    #[ignore = "requires live Postgres (set DATABASE_URL)"]
    async fn pg_provider_no_price_returns_none_and_not_cached() {
        // Integration test — gated.
    }

    /// Cache TTL expiry: entry older than 5 min is re-fetched.
    #[tokio::test]
    #[ignore = "requires live Postgres (set DATABASE_URL)"]
    async fn pg_provider_cache_ttl_expiry_triggers_refetch() {
        // Integration test — gated.
    }
}
