//! LP Locker Transfer watcher — detects ERC-20 Transfer events where the
//! recipient (`to`) is a known LP locker contract.
//!
//! # Purpose
//!
//! When a token deployer locks LP tokens, they transfer ERC-20 LP tokens to a
//! well-known locker contract (Unicrypt, Team Finance, TrustSwap, etc.). This
//! module watches for those transfers in the coordinator event stream and
//! records them via `PgStore::upsert_locker`.
//!
//! # Architecture
//!
//! `LockerWatcher` is pure logic — it holds a `LockerRegistry` reference and
//! exposes a synchronous `check_transfer` method that returns `Option<LockerHit>`.
//! The caller (coordinator bridge) is responsible for the I/O (calling
//! `PgStore::upsert_locker`).
//!
//! This design keeps the hot-path logic testable without a database.
//!
//! # D10-EVM-LP-LOCK closure (Sprint 44)
//!
//! V00017 migration shipped (Sprint 44, Track 1): `tokens.metadata_jsonb` JSONB column added.
//! `PgStore::upsert_locker` now persists locker hits to `metadata_jsonb -> 'lockers'` array.
//! `PgStore::upsert_locker_hit` + `fetch_lockers` provide the full read/write round-trip.
//!
//! `SubscribeFilter::evm_default_for_chain` now includes factory contract addresses
//! in `evm_contract_addresses` (Sprint 44, Track 2). When the EVM subscribe loop
//! is implemented (Sprint 16 stub → production wiring), these addresses will be used
//! to filter eth_getLogs to factory and locker events.
//!
//! # Sprint 44 wiring
//!
//! The `LockerWatcher` is constructed in `main.rs` and passed to the coordinator
//! bridge (see `coordinator_to_invalidation_bridge_with_locker`). The bridge
//! calls `watcher.check_transfer()` for every `Event::Transfer` and fires
//! `PgStore::upsert_locker` on hits, which now persists to DB via V00017.

use std::sync::Arc;

use tracing::{debug, instrument};

use mg_onchain_chain_adapter::Event;
use mg_onchain_common::chain::Chain;
use mg_onchain_common::event::Transfer;
use mg_onchain_detectors::lockers::{LockerRegistry};
use mg_onchain_storage::PgStore;

// ---------------------------------------------------------------------------
// LockerHit — result of a successful locker detection
// ---------------------------------------------------------------------------

/// Describes a single LP locker transfer detection.
///
/// Returned by `LockerWatcher::check_transfer` when the transfer recipient
/// is a known locker.
#[derive(Debug, Clone)]
pub struct LockerHit {
    /// Chain of the detected transfer.
    pub chain: Chain,
    /// ERC-20 token contract that was transferred.
    pub token_mint: String,
    /// Locker contract address (recipient of the LP transfer).
    pub locker_address: String,
    /// Protocol name from the registry (e.g. "Unicrypt", "Team Finance").
    pub protocol_name: Option<&'static str>,
    /// Raw LP token amount transferred to the locker.
    pub locked_amount_raw: u128,
}

// ---------------------------------------------------------------------------
// LockerWatcher
// ---------------------------------------------------------------------------

/// Watches transfer events for LP locker recipients.
///
/// Constructed at startup with a `LockerRegistry` reference. Thread-safe:
/// `LockerRegistry` is `Sync` and `LockerWatcher` wraps it in `Arc`.
pub struct LockerWatcher {
    registry: Arc<LockerRegistry>,
}

impl LockerWatcher {
    /// Construct from a shared `LockerRegistry`.
    pub fn new(registry: Arc<LockerRegistry>) -> Self {
        Self { registry }
    }

    /// Build with the default compile-time locker registry.
    pub fn with_default_registry() -> Self {
        Self::new(Arc::new(LockerRegistry::default()))
    }

    /// Check a single `Transfer` event against the locker registry.
    ///
    /// Returns `Some(LockerHit)` if `transfer.to` is a known locker on
    /// `transfer.chain`. Returns `None` for all other transfers.
    ///
    /// The `to` address is lowercased before lookup (canonical form).
    #[instrument(skip(self, transfer), fields(
        chain = %transfer.chain,
        token = %transfer.token.as_str(),
        to = %transfer.to.as_str()
    ))]
    pub fn check_transfer(&self, transfer: &Transfer) -> Option<LockerHit> {
        let chain = transfer.chain;
        // Locker detection is EVM-only (LP lockers are an EVM concept).
        if !chain.is_evm() {
            return None;
        }

        let to_addr = transfer.to.as_str();
        if !self.registry.is_locker(chain, to_addr) {
            return None;
        }

        let protocol_name = self.registry.protocol_of(chain, to_addr).map(|p| p.name());

        debug!(
            chain = %chain,
            token = %transfer.token.as_str(),
            locker = to_addr,
            protocol = ?protocol_name,
            amount = transfer.amount_raw,
            "LP locker transfer detected"
        );

        Some(LockerHit {
            chain,
            token_mint: transfer.token.as_str().to_owned(),
            locker_address: to_addr.to_owned(),
            protocol_name,
            locked_amount_raw: transfer.amount_raw,
        })
    }

    /// Process a coordinator `Event`, returning a locker hit if applicable.
    ///
    /// Convenience wrapper that pattern-matches on the event variant and calls
    /// `check_transfer` for `Event::Transfer`. All other event variants return `None`.
    pub fn check_event(&self, event: &Event) -> Option<LockerHit> {
        match event {
            Event::Transfer(t) => self.check_transfer(t),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// write_locker_hit — I/O helper for callers
// ---------------------------------------------------------------------------

/// Write a `LockerHit` to storage via `PgStore::upsert_locker`.
///
/// This function bridges the pure-logic `LockerWatcher` output to the storage
/// layer. It is `async` because `upsert_locker` is an async method.
///
/// V00017 migration shipped (Sprint 44): locker hits are persisted to
/// `tokens.metadata_jsonb -> 'lockers'` via `PgStore::upsert_locker_hit`.
pub async fn write_locker_hit(
    store: &PgStore,
    hit: &LockerHit,
) -> Result<(), mg_onchain_storage::StorageError> {
    store
        .upsert_locker(
            hit.chain.as_str(),
            &hit.token_mint,
            &hit.locker_address,
            hit.locked_amount_raw,
            hit.protocol_name,
        )
        .await
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::Utc;
    use mg_onchain_common::chain::{Address, BlockRef, Chain, TxHash};
    use mg_onchain_common::event::Transfer;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_erc20_transfer(chain: Chain, to_addr: &str, token_addr: &str, amount: u128) -> Transfer {
        let from = Address::parse(chain, "0x1111111111111111111111111111111111111111").unwrap();
        let to = Address::parse(chain, to_addr).unwrap();
        let token = Address::parse(chain, token_addr).unwrap();
        let tx = TxHash::evm_from_hex(
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();

        Transfer {
            chain,
            tx_hash: tx,
            block: BlockRef::new(chain, 21_000_000),
            block_time: Utc::now(),
            token,
            from,
            to,
            amount_raw: amount,
            decimals: 18,
            log_index: 0,
        }
    }

    // -----------------------------------------------------------------------
    // Test: Transfer to Unicrypt Ethereum locker → LockerHit returned
    // -----------------------------------------------------------------------

    /// Unicrypt Ethereum (0x663a...db214) → locker hit.
    #[test]
    fn transfer_to_unicrypt_ethereum_returns_hit() {
        let watcher = LockerWatcher::with_default_registry();

        // LP token contract (any ERC-20 address — not a locker).
        let lp_token = "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc";
        // Unicrypt V2 locker on Ethereum (checksummed input — watcher lowercases).
        let unicrypt = "0x663A5C229c09b049E36dCc11a9B0d4a8Eb9db214";

        let transfer =
            make_erc20_transfer(Chain::Ethereum, unicrypt, lp_token, 1_000_000_000_000_000_000);

        let hit = watcher.check_transfer(&transfer).expect("Unicrypt must be detected");

        assert_eq!(hit.chain, Chain::Ethereum);
        assert_eq!(hit.token_mint, lp_token);
        // Locker address stored lowercase (canonical form).
        assert_eq!(
            hit.locker_address,
            "0x663a5c229c09b049e36dcc11a9b0d4a8eb9db214"
        );
        assert_eq!(hit.protocol_name, Some("Unicrypt"));
        assert_eq!(hit.locked_amount_raw, 1_000_000_000_000_000_000u128);
    }

    // -----------------------------------------------------------------------
    // Test: Transfer to Team Finance BSC → LockerHit returned
    // -----------------------------------------------------------------------

    #[test]
    fn transfer_to_team_finance_bsc_returns_hit() {
        let watcher = LockerWatcher::with_default_registry();

        let lp_token = "0x0ed7e52944161450477ee417de9cd3a859b14fd0";
        // Team Finance BSC locker.
        let team_finance_bsc = "0x0C89C0407775dd89b12918B9c0aa42Bf96518820";

        let transfer = make_erc20_transfer(Chain::Bsc, team_finance_bsc, lp_token, 5_000_000_000);

        let hit = watcher
            .check_transfer(&transfer)
            .expect("Team Finance BSC must be detected");

        assert_eq!(hit.chain, Chain::Bsc);
        assert_eq!(hit.protocol_name, Some("Team Finance"));
    }

    // -----------------------------------------------------------------------
    // Test: Non-locker address → None
    // -----------------------------------------------------------------------

    #[test]
    fn transfer_to_random_address_returns_none() {
        let watcher = LockerWatcher::with_default_registry();

        let random = "0xdeaddeaddeaddeaddeaddeaddeaddeaddeaddead";
        let lp_token = "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc";

        let transfer = make_erc20_transfer(Chain::Ethereum, random, lp_token, 1_000);

        assert!(
            watcher.check_transfer(&transfer).is_none(),
            "non-locker address must return None"
        );
    }

    // -----------------------------------------------------------------------
    // Test: Solana transfer → None (EVM-only feature)
    // -----------------------------------------------------------------------

    #[test]
    fn solana_transfer_always_returns_none() {
        let watcher = LockerWatcher::with_default_registry();

        // Use Solana addresses (base58).
        let from =
            Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112").unwrap();
        let to =
            Address::parse(Chain::Solana, "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
        let token = from.clone();
        let tx = TxHash::solana_from_base58(
            &bs58::encode([0xddu8; 64]).into_string(),
        )
        .unwrap();

        let transfer = Transfer {
            chain: Chain::Solana,
            tx_hash: tx,
            block: BlockRef::new(Chain::Solana, 300_000_000),
            block_time: Utc::now(),
            token,
            from,
            to,
            amount_raw: 1_000_000,
            decimals: 9,
            log_index: 0,
        };

        assert!(
            watcher.check_transfer(&transfer).is_none(),
            "Solana transfers must never match locker registry"
        );
    }

    // -----------------------------------------------------------------------
    // Test: check_event wraps check_transfer correctly
    // -----------------------------------------------------------------------

    #[test]
    fn check_event_returns_hit_for_transfer_to_locker() {
        let watcher = LockerWatcher::with_default_registry();
        let unicrypt = "0x663A5C229c09b049E36dCc11a9B0d4a8Eb9db214";
        let lp_token = "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc";
        let transfer = make_erc20_transfer(Chain::Ethereum, unicrypt, lp_token, 42);
        let event = Event::Transfer(transfer);

        assert!(
            watcher.check_event(&event).is_some(),
            "check_event must detect locker hit for Transfer event"
        );
    }

    #[test]
    fn check_event_returns_none_for_non_transfer() {
        let watcher = LockerWatcher::with_default_registry();

        // Non-transfer event: SlotFinalized.
        let event = Event::SlotFinalized { slot: 12345 };
        assert!(
            watcher.check_event(&event).is_none(),
            "non-Transfer events must always return None"
        );
    }
}
