//! `VerdictCacheStore` trait + `PgVerdictCacheStore` implementation.
//!
//! Per-detector cached verdicts under ADR 0007 (Pull-Based Query Engine).
//! Indexer trigger path:
//!
//! 1. `trigger_evaluate(token)` checks cache via `get(...)` — returns fresh verdict if `expires_at > now()`.
//! 2. On cache miss / expired: detector runs, computes a fresh verdict.
//! 3. Indexer calls `upsert(...)` with `expires_at = now() + VerdictCacheConfig::ttl_for_detector_id`.
//!
//! Background task purges expired rows hourly via `purge_expired()`.
//!
//! # Design reference
//!
//! `docs/adr/0007-pull-based-query-engine.md` §9.5 (TTL classes).
//! `docs/designs/0028-lightweight-query-engine-deployment.md` §8 + §11.4 + §11.5.
//! Migration: `migrations/postgres/V00018__verdict_cache_and_retention.sql`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::Row;
use std::str::FromStr;

use mg_onchain_common::chain::{Address, Chain};

use crate::error::StorageError;

// ---------------------------------------------------------------------------
// CachedVerdict
// ---------------------------------------------------------------------------

/// Mirrors the `verdict_cache` row shape.
///
/// All numeric fields stored as `Decimal` (NUMERIC in Postgres) — never `f64` for
/// stored state per CLAUDE.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedVerdict {
    pub chain: String,
    pub token_address: String,
    pub detector_id: String,
    pub confidence: Decimal,
    pub severity: String,
    pub evidence: serde_json::Value,
    pub cached_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// VerdictCacheStore trait
// ---------------------------------------------------------------------------

/// Async dyn-compatible trait for verdict cache storage.
#[async_trait]
pub trait VerdictCacheStore: Send + Sync {
    /// Get cached verdict if present (regardless of expiry — caller checks `expires_at`).
    async fn get(
        &self,
        chain: Chain,
        token: &Address,
        detector_id: &str,
    ) -> Result<Option<CachedVerdict>, StorageError>;

    /// Insert or replace a verdict.
    async fn upsert(&self, verdict: &CachedVerdict) -> Result<(), StorageError>;

    /// Delete rows where `expires_at < now()`. Returns deleted count.
    async fn purge_expired(&self) -> Result<u64, StorageError>;
}

// ---------------------------------------------------------------------------
// PgVerdictCacheStore — Postgres-backed implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct PgVerdictCacheStore {
    pool: sqlx::PgPool,
}

impl PgVerdictCacheStore {
    #[must_use]
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl VerdictCacheStore for PgVerdictCacheStore {
    async fn get(
        &self,
        chain: Chain,
        token: &Address,
        detector_id: &str,
    ) -> Result<Option<CachedVerdict>, StorageError> {
        let chain_str = chain.to_string();
        let token_str = token.to_string();
        let row = sqlx::query(
            r#"
            SELECT chain, token_address, detector_id,
                   confidence::TEXT AS confidence_str, severity, evidence,
                   cached_at, expires_at
            FROM verdict_cache
            WHERE chain = $1 AND token_address = $2 AND detector_id = $3
            "#,
        )
        .bind(&chain_str)
        .bind(&token_str)
        .bind(detector_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        match row {
            None => Ok(None),
            Some(r) => {
                let confidence_str: String = r.try_get("confidence_str").map_err(StorageError::Postgres)?;
                Ok(Some(CachedVerdict {
                    chain: r.try_get("chain").map_err(StorageError::Postgres)?,
                    token_address: r.try_get("token_address").map_err(StorageError::Postgres)?,
                    detector_id: r.try_get("detector_id").map_err(StorageError::Postgres)?,
                    confidence: Decimal::from_str(&confidence_str)
                        .map_err(|e| StorageError::Other(format!("verdict_cache.confidence parse: {e}")))?,
                    severity: r.try_get("severity").map_err(StorageError::Postgres)?,
                    evidence: r.try_get("evidence").map_err(StorageError::Postgres)?,
                    cached_at: r.try_get("cached_at").map_err(StorageError::Postgres)?,
                    expires_at: r.try_get("expires_at").map_err(StorageError::Postgres)?,
                }))
            }
        }
    }

    async fn upsert(&self, v: &CachedVerdict) -> Result<(), StorageError> {
        sqlx::query(
            r#"
            INSERT INTO verdict_cache
                (chain, token_address, detector_id, confidence, severity, evidence, cached_at, expires_at)
            VALUES ($1, $2, $3, $4::NUMERIC, $5, $6, $7, $8)
            ON CONFLICT (chain, token_address, detector_id) DO UPDATE SET
                confidence = EXCLUDED.confidence,
                severity   = EXCLUDED.severity,
                evidence   = EXCLUDED.evidence,
                cached_at  = EXCLUDED.cached_at,
                expires_at = EXCLUDED.expires_at
            "#,
        )
        .bind(&v.chain)
        .bind(&v.token_address)
        .bind(&v.detector_id)
        .bind(v.confidence.to_string())
        .bind(&v.severity)
        .bind(&v.evidence)
        .bind(v.cached_at)
        .bind(v.expires_at)
        .execute(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;
        Ok(())
    }

    async fn purge_expired(&self) -> Result<u64, StorageError> {
        let result = sqlx::query("DELETE FROM verdict_cache WHERE expires_at < now()")
            .execute(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
        Ok(result.rows_affected())
    }
}

// ---------------------------------------------------------------------------
// MockVerdictCacheStore (test-utils)
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "test-utils"))]
mod mock {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// In-memory mock for unit tests. Keyed on (chain, token_address, detector_id).
    pub struct MockVerdictCacheStore {
        rows: Mutex<BTreeMap<(String, String, String), CachedVerdict>>,
    }

    impl Default for MockVerdictCacheStore {
        fn default() -> Self {
            Self { rows: Mutex::new(BTreeMap::new()) }
        }
    }

    impl MockVerdictCacheStore {
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }
    }

    #[async_trait]
    impl VerdictCacheStore for MockVerdictCacheStore {
        async fn get(
            &self,
            chain: Chain,
            token: &Address,
            detector_id: &str,
        ) -> Result<Option<CachedVerdict>, StorageError> {
            let key = (chain.to_string(), token.to_string(), detector_id.to_owned());
            let g = self.rows.lock().unwrap();
            Ok(g.get(&key).cloned())
        }

        async fn upsert(&self, v: &CachedVerdict) -> Result<(), StorageError> {
            let key = (v.chain.clone(), v.token_address.clone(), v.detector_id.clone());
            let mut g = self.rows.lock().unwrap();
            g.insert(key, v.clone());
            Ok(())
        }

        async fn purge_expired(&self) -> Result<u64, StorageError> {
            let now = Utc::now();
            let mut g = self.rows.lock().unwrap();
            let before = g.len();
            g.retain(|_, v| v.expires_at >= now);
            Ok((before - g.len()) as u64)
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
pub use mock::MockVerdictCacheStore;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    // Use `Decimal::new(mantissa, scale)` instead of `dec!()` to avoid pulling in `rust_decimal_macros`.
    // `Decimal::new(7500, 4)` == `0.7500`.

    fn sample_verdict(detector_id: &str, expires_in: Duration) -> CachedVerdict {
        let now = Utc::now();
        CachedVerdict {
            chain: "solana".to_string(),
            token_address: "So11111111111111111111111111111111111111112".to_string(),
            detector_id: detector_id.to_owned(),
            confidence: Decimal::new(7500, 4),
            severity: "HIGH".to_string(),
            evidence: serde_json::json!({"sample": "data"}),
            cached_at: now,
            expires_at: now + expires_in,
        }
    }

    #[tokio::test]
    async fn mock_upsert_then_get_returns_same_verdict() {
        let store = MockVerdictCacheStore::new();
        let v = sample_verdict("d01_honeypot_v1", Duration::minutes(15));
        store.upsert(&v).await.unwrap();
        let chain = Chain::Solana;
        let token = Address::parse(Chain::Solana, &v.token_address).unwrap();
        let got = store.get(chain, &token, "d01_honeypot_v1").await.unwrap();
        assert_eq!(got, Some(v));
    }

    #[tokio::test]
    async fn mock_upsert_replaces_existing_row() {
        let store = MockVerdictCacheStore::new();
        let v1 = sample_verdict("d04_pump_dump_v1", Duration::minutes(5));
        let mut v2 = v1.clone();
        v2.confidence = Decimal::new(9000, 4);
        v2.severity = "CRITICAL".to_string();
        store.upsert(&v1).await.unwrap();
        store.upsert(&v2).await.unwrap();
        let chain = Chain::Solana;
        let token = Address::parse(Chain::Solana, &v1.token_address).unwrap();
        let got = store.get(chain, &token, "d04_pump_dump_v1").await.unwrap().unwrap();
        assert_eq!(got.confidence, Decimal::new(9000, 4));
        assert_eq!(got.severity, "CRITICAL");
    }

    #[tokio::test]
    async fn mock_purge_expired_removes_only_expired_rows() {
        let store = MockVerdictCacheStore::new();
        let fresh = sample_verdict("d04_pump_dump_v1", Duration::minutes(5));
        let mut expired = sample_verdict("d05_wash_trading_v1", Duration::minutes(-5));
        expired.cached_at = Utc::now() - Duration::minutes(10);
        store.upsert(&fresh).await.unwrap();
        store.upsert(&expired).await.unwrap();
        let purged = store.purge_expired().await.unwrap();
        assert_eq!(purged, 1);
        let chain = Chain::Solana;
        let token = Address::parse(Chain::Solana, &fresh.token_address).unwrap();
        assert!(store.get(chain, &token, "d04_pump_dump_v1").await.unwrap().is_some());
        assert!(store.get(chain, &token, "d05_wash_trading_v1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn mock_get_missing_key_returns_none() {
        let store = MockVerdictCacheStore::new();
        let token = Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112").unwrap();
        let got = store.get(Chain::Solana, &token, "d01_honeypot_v1").await.unwrap();
        assert_eq!(got, None);
    }
}
