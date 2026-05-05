//! `init/` — startup initializers for `onchain-service`.
//!
//! Each submodule is a pure constructor with no side effects beyond `tracing`.
//! `main.rs` calls them in sequence per design 0020 §4.
//!
//! # Module layout
//!
//! ```text
//! init/
//!   mod.rs          — pub re-exports (this file)
//!   tracing.rs      — init_tracing(): tracing_subscriber + EnvFilter
//!   storage.rs      — connect_postgres() + run_migrations() (D-A)
//!   adapters.rs     — build_solana_adapter() + build_ethereum_adapter() (D-E)
//!   coordinator.rs  — build_coordinator() + coordinator_to_invalidation_bridge()
//!   hooks.rs        — build_pool_initialize_hook() composites D09 + D10 (gotcha #48)
//!   detectors.rs    — build_all_detectors() for all 12 detectors (D01-D12)
//!   smart_money.rs  — build_smart_money_labeller() + spawn_smart_money_labeller() (Sprint 22)
//!   periodic_scan.rs — watchlist_rescore_worker + launch_discovery_worker (Sprint 26 T26-4)
//! ```

pub mod adapters;
pub mod coordinator;
pub mod detectors;
pub mod hooks;
pub mod known_bridges;
pub mod known_drainers;
pub mod locker_watcher;
pub mod metadata_fetchers;
/// Periodic scan workers for the on-demand query engine (ADR 0007 / design 0028 §7.2).
///
/// `spawn_watchlist_rescore_worker` and `spawn_launch_discovery_worker` run on a
/// configurable cadence (default 5 min per ADR 0007 §9.4) as `tokio::spawn` tasks,
/// calling `MultiChainCoordinator::trigger_evaluate` for each token.
pub mod periodic_scan;
pub mod smart_money;
pub mod storage;
pub mod tracing_init;

pub use adapters::{build_ethereum_adapter, build_evm_adapters, build_solana_adapter};
pub use coordinator::{build_coordinator, build_coordinator_multi, coordinator_to_invalidation_bridge};
pub use detectors::build_all_detectors;
pub use hooks::build_pool_initialize_hook;
pub use locker_watcher::{LockerHit, LockerWatcher, write_locker_hit};
pub use metadata_fetchers::{EvmTokenMetadataFetcher, SolanaTokenMetadataFetcher};
pub use periodic_scan::{
    PeriodicScanConfig, spawn_launch_discovery_worker, spawn_watchlist_rescore_worker,
};
pub use smart_money::{build_smart_money_config, build_smart_money_labeller, spawn_smart_money_labeller};
pub use storage::{connect_postgres, run_migrations};
pub use tracing_init::init_tracing;
