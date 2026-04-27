//! Postgres-backed `CheckpointStore` implementation.
//!
//! # Coordination point: CheckpointStore trait location
//!
//! The `CheckpointStore` trait is currently defined in
//! `crates/chain-adapter/src/solana/checkpoint.rs`. Ideally it would live in
//! `crates/common` to keep `crates/storage` free of an `mg-onchain-chain-adapter`
//! dependency (and avoid a circular dep if chain-adapter ever imports storage).
//!
//! **Decision:** The trait is re-defined here as a local async version
//! (`AsyncCheckpointStore`) rather than importing the synchronous trait from
//! `chain-adapter`. Rationale:
//!
//! 1. The trait in `chain-adapter` is synchronous (`fn save(&self, ..) -> Result<..>`)
//!    because `FileCheckpointStore` uses blocking `std::fs` I/O. The Postgres-backed
//!    store must be async. Making it implement the synchronous trait would require
//!    `tokio::task::block_in_place` — a workaround that leaks the wrong abstraction.
//!
//! 2. Adding `mg-onchain-chain-adapter` as a dependency of `crates/storage` would
//!    create a potential circular dependency when `chain-adapter` eventually needs
//!    storage (for the Postgres checkpoint).
//!
//! **Recommended follow-up (open question):** Promote `CheckpointStore` (in async
//! form) to `crates/common` and have both `chain-adapter` and `storage` implement
//! it. This would require:
//!   - Moving `Checkpoint` struct to `crates/common`.
//!   - Changing `FileCheckpointStore` to use `tokio::fs` (or wrapping in spawn_blocking).
//!   - Both crates depend on `crates/common` already, so no new dep cycles.
//!
//! Until that refactor, the `SolanaAdapter` in `chain-adapter` uses the synchronous
//! `CheckpointStore` from its own module, and the `PgCheckpointStore` here is used
//! directly by the `server` crate when wiring up production adapters.

use async_trait::async_trait;

use crate::error::StorageError;
use crate::pg::PgStore;

// ---------------------------------------------------------------------------
// Checkpoint data type (mirrors chain-adapter::Checkpoint)
// ---------------------------------------------------------------------------

/// Last successfully processed position in the event stream.
///
/// Mirrors `mg_onchain_chain_adapter::Checkpoint`. Once `Checkpoint` is
/// promoted to `crates/common` (see module doc), this type will be replaced
/// by the shared one.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Checkpoint {
    /// Last slot / block number fully processed.
    pub slot: u64,
    /// Last transaction signature within that slot. `None` if the slot had no
    /// relevant transactions.
    pub last_signature: Option<String>,
}

// ---------------------------------------------------------------------------
// AsyncCheckpointStore trait
// ---------------------------------------------------------------------------

/// Async checkpoint persistence — Postgres-backed production impl.
///
/// See module doc for the rationale for defining this separately from the
/// synchronous `CheckpointStore` in `crates/chain-adapter`.
#[async_trait]
pub trait AsyncCheckpointStore: Send + Sync {
    /// Persist `checkpoint`, overwriting any previous value.
    async fn save(&self, adapter_id: &str, checkpoint: &Checkpoint) -> Result<(), StorageError>;

    /// Load the last persisted checkpoint. Returns `None` on first run.
    async fn load(&self, adapter_id: &str) -> Result<Option<Checkpoint>, StorageError>;
}

// ---------------------------------------------------------------------------
// PgCheckpointStore
// ---------------------------------------------------------------------------

/// Postgres-backed checkpoint store for production use.
///
/// Wraps `PgStore` and implements `AsyncCheckpointStore`. The underlying
/// `adapter_checkpoints` table uses an atomic upsert (INSERT … ON CONFLICT DO UPDATE)
/// to guarantee checkpoint writes are safe under concurrent adapter restarts.
#[derive(Debug, Clone)]
pub struct PgCheckpointStore {
    pg: PgStore,
}

impl PgCheckpointStore {
    /// Construct a `PgCheckpointStore` from an existing `PgStore`.
    pub fn new(pg: PgStore) -> Self {
        Self { pg }
    }
}

#[async_trait]
impl AsyncCheckpointStore for PgCheckpointStore {
    async fn save(&self, adapter_id: &str, checkpoint: &Checkpoint) -> Result<(), StorageError> {
        self.pg
            .save_checkpoint(
                adapter_id,
                checkpoint.slot as i64,
                checkpoint.last_signature.as_deref(),
            )
            .await
    }

    async fn load(&self, adapter_id: &str) -> Result<Option<Checkpoint>, StorageError> {
        let row = self.pg.load_checkpoint(adapter_id).await?;
        Ok(row.map(|r| Checkpoint {
            slot: r.last_slot as u64,
            last_signature: r.last_signature,
        }))
    }
}

// ---------------------------------------------------------------------------
// InMemoryCheckpointStore (for integration tests)
// ---------------------------------------------------------------------------

/// In-memory async checkpoint store for integration tests.
///
/// Thread-safe via `tokio::sync::Mutex`. Does not touch the database.
pub struct InMemoryAsyncCheckpointStore {
    inner: tokio::sync::Mutex<std::collections::HashMap<String, Checkpoint>>,
}

impl InMemoryAsyncCheckpointStore {
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl Default for InMemoryAsyncCheckpointStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AsyncCheckpointStore for InMemoryAsyncCheckpointStore {
    async fn save(&self, adapter_id: &str, checkpoint: &Checkpoint) -> Result<(), StorageError> {
        let mut guard = self.inner.lock().await;
        guard.insert(adapter_id.to_string(), checkpoint.clone());
        Ok(())
    }

    async fn load(&self, adapter_id: &str) -> Result<Option<Checkpoint>, StorageError> {
        let guard = self.inner.lock().await;
        Ok(guard.get(adapter_id).cloned())
    }
}

// ---------------------------------------------------------------------------
// Tests (unit — no DB needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_load_empty() {
        let store = InMemoryAsyncCheckpointStore::new();
        let result = store.load("solana").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn in_memory_save_and_load() {
        let store = InMemoryAsyncCheckpointStore::new();
        let cp = Checkpoint {
            slot: 12345,
            last_signature: Some("abc123sig".to_string()),
        };
        store.save("solana", &cp).await.unwrap();
        let loaded = store.load("solana").await.unwrap().unwrap();
        assert_eq!(loaded.slot, 12345);
        assert_eq!(loaded.last_signature.as_deref(), Some("abc123sig"));
    }

    #[tokio::test]
    async fn in_memory_overwrite() {
        let store = InMemoryAsyncCheckpointStore::new();
        store
            .save("solana", &Checkpoint { slot: 100, last_signature: None })
            .await
            .unwrap();
        store
            .save("solana", &Checkpoint { slot: 200, last_signature: Some("sig2".into()) })
            .await
            .unwrap();
        let loaded = store.load("solana").await.unwrap().unwrap();
        assert_eq!(loaded.slot, 200);
    }

    #[tokio::test]
    async fn in_memory_multiple_adapters() {
        let store = InMemoryAsyncCheckpointStore::new();
        store
            .save("solana", &Checkpoint { slot: 100, last_signature: None })
            .await
            .unwrap();
        store
            .save("ethereum", &Checkpoint { slot: 200, last_signature: Some("0xhash".into()) })
            .await
            .unwrap();
        let sol = store.load("solana").await.unwrap().unwrap();
        let eth = store.load("ethereum").await.unwrap().unwrap();
        assert_eq!(sol.slot, 100);
        assert_eq!(eth.slot, 200);
        // Neither leaks into the other
        assert!(store.load("bsc").await.unwrap().is_none());
    }

    #[test]
    fn checkpoint_serde_roundtrip() {
        let cp = Checkpoint {
            slot: 999_999_999,
            last_signature: Some("5xvRdemoBase58Sig".into()),
        };
        let json = serde_json::to_string(&cp).unwrap();
        let back: Checkpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back.slot, cp.slot);
        assert_eq!(back.last_signature, cp.last_signature);
    }
}
