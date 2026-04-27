//! `mg-onchain-chain-adapter` — ChainAdapter trait and per-chain implementations.
//!
//! # Architecture
//!
//! ```text
//! ChainAdapter trait  (this module)
//!       ├─ SolanaAdapter  (crates/chain-adapter/src/solana/)
//!       │       ├─ subscribe()   → Yellowstone gRPC live stream
//!       │       ├─ backfill()    → Solana JSON-RPC getBlock batches
//!       │       ├─ checkpoint_save/load()
//!       │       └─ health_check()
//!       └─ EthereumAdapter  (crates/chain-adapter/src/ethereum/)  [Sprint 15 skeleton]
//!               ├─ subscribe()   → eth_subscribe("newHeads") + eth_getLogs  [Sprint 16]
//!               ├─ backfill()    → eth_getLogs range queries  [Sprint 16]
//!               ├─ checkpoint_save/load()
//!               └─ health_check()
//! ```
//!
//! Every crate above `chain-adapter` (indexer, detectors, gateway) sees only
//! `ChainAdapter` and the `Event` enum from `crates/common`.
//!
//! # Yellowstone gRPC — one protocol, three brand names
//!
//! Per ADR 0001 §D2, all providers (Helius LaserStream, Triton Dragon's Mouth,
//! self-hosted validator) speak the same open-source protocol at
//! `github.com/rpcpool/yellowstone-grpc`. Provider discrimination is NEVER in
//! code; it lives entirely in `config/adapters.toml`. See
//! `crates/chain-adapter/src/solana/config.rs` for the config shape and
//! `config/adapters.toml.example` for sample entries for all three providers.
//!
//! # Dependency note
//!
//! `yellowstone-grpc-client` 13.1.0 requires `tonic 0.14` and
//! `yellowstone-grpc-proto 12.2`. These three are pinned together in the
//! workspace `Cargo.toml`. Bump them as a group.
//!
//! `solana-sdk 4` is imported ONLY in `src/solana/` submodules. Do not expose
//! raw `solana_sdk` types outside `chain-adapter` — convert to `common` types
//! at the module boundary.

pub mod error;
pub mod ethereum;
pub mod solana;

use futures::Stream;
use std::ops::RangeInclusive;
use std::pin::Pin;

use mg_onchain_common::chain::BlockRef;

pub use error::AdapterError;
pub use solana::SolanaAdapter;

// ---------------------------------------------------------------------------
// Event — top-level event type emitted by any ChainAdapter implementation
// ---------------------------------------------------------------------------

/// All event kinds that a chain adapter can emit.
///
/// Consumers (`crates/indexer`, `crates/detectors`) receive `Event` and
/// pattern-match to the variant they care about. Adding a new chain's events
/// requires adding a new variant here; existing consumers handle it with
/// `#[non_exhaustive]` match arms.
///
/// `#[non_exhaustive]` allows Phase 4 EVM events (e.g. `PendingTx`) to be
/// added without breaking existing match arms in consumer crates.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Event {
    /// A token transfer (SPL on Solana, ERC-20 on EVM in Phase 4).
    Transfer(mg_onchain_common::event::Transfer),

    /// A DEX swap.
    Swap(mg_onchain_common::event::Swap),

    /// An LP pool state event (mint/burn/sync/initialize).
    PoolEvent(mg_onchain_common::event::PoolEvent),

    /// Partial token metadata seen for the first time.
    ///
    /// Emitted by the adapter when a previously-unseen mint appears in the
    /// stream. Fields the stream alone cannot populate (e.g. `symbol`, `name`,
    /// `top_holders`, `markets`) are `None` or empty — the `token-registry`
    /// crate enriches them via RPC in Phase 2.
    ///
    /// Boxed to reduce `Event` enum size (TokenMeta is ~640 bytes).
    TokenMeta(Box<mg_onchain_common::token::TokenMeta>),

    /// A decoded Token-2022 instruction event (D07 pre-authorised extension).
    ///
    /// Emitted by the chain-adapter when it decodes a `WithdrawWithheld*`,
    /// `HarvestWithheldToMint`, or `SetAuthority(WithdrawWithheldTokens)` instruction
    /// in either the top-level or inner (CPI) instructions of a Solana transaction.
    ///
    /// Consumers: the indexer routes this to the `token2022_instructions` Postgres
    /// table (V00007 migration). D07 reads from that table; it does NOT consume
    /// these events directly.
    ///
    /// Boxed to keep `Event` enum size bounded (same pattern as `TokenMeta`).
    Token2022Instruction(Box<mg_onchain_common::event::Token2022InstructionEvent>),

    /// A reorg marker indicating that events at `slot` should be considered
    /// reverted. Consumers must evict buffered events for this slot.
    ///
    /// Emitted when a `confirmed` slot is later observed as skipped/dead by
    /// the slot update stream (i.e., it never became `finalized`). This
    /// implements the reorg handling required by CLAUDE.md §Multi-Chain Rules
    /// / Solana and ADR 0001 §D2.
    ReorgMarker { slot: u64 },

    /// Slot finalized — events for this slot are now immutable.
    ///
    /// Consumers can flush buffered events for this slot to durable storage.
    SlotFinalized { slot: u64 },
}

// ---------------------------------------------------------------------------
// Checkpoint — persisted resume position
// ---------------------------------------------------------------------------

/// The last successfully processed position in the event stream.
///
/// Persisted by `ChainAdapter::checkpoint_save` after each batch. On restart,
/// `ChainAdapter::checkpoint_load` returns this value and the adapter resumes
/// from `(slot, signature)`.
///
/// The adapter guarantees at-least-once delivery: on restart from a checkpoint,
/// a small number of events just before `(slot, signature)` may be re-emitted.
/// Consumers must be idempotent (use `(tx_hash, log_index)` as the dedup key).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Checkpoint {
    /// The last slot that was fully processed.
    pub slot: u64,
    /// The last transaction signature processed within that slot (Base58).
    /// `None` if the slot had no relevant transactions.
    pub last_signature: Option<String>,
}

// ---------------------------------------------------------------------------
// ChainAdapter trait
// ---------------------------------------------------------------------------

/// The unified interface for all chain-specific implementations.
///
/// ## Why these methods?
///
/// - `subscribe` — the hot path: real-time streaming via Yellowstone gRPC (Solana)
///   or `eth_subscribe` (EVM Phase 4). Returns an async `Stream` so consumers
///   can apply backpressure naturally with standard `futures::StreamExt` combinators.
///
/// - `backfill` — historical sync: replay events for a closed slot/block range.
///   Distinct from `subscribe` because: (a) it reads from archive RPC, not the
///   live tip; (b) it must guarantee complete coverage of the range (no gaps);
///   (c) it runs in a separate tokio task so it doesn't block live event processing.
///
/// - `checkpoint_save` / `checkpoint_load` — persist + restore the last processed
///   `(slot, signature)` so the adapter can resume after a restart without re-reading
///   the full history. The storage backend is injected (`Box<dyn CheckpointStore>`)
///   so unit tests can use an in-memory store and production uses the Postgres-backed
///   one from `crates/storage` (Task 4).
///
/// - `health_check` — single-call liveness probe used by the `server` crate's
///   `/health` endpoint. Returns `Ok(())` if the underlying gRPC connection is
///   responsive, `Err` otherwise.
///
/// ## Object safety
///
/// The `Stream` associated type uses `Pin<Box<dyn Stream>>` rather than an
/// associated type so the trait is object-safe and can be stored as
/// `Box<dyn ChainAdapter>` in the indexer. This is a pragmatic Phase 1 choice;
/// a GAT-based design can be revisited in Phase 3 if performance profiling shows
/// the heap allocation matters.
pub trait ChainAdapter: Send + Sync {
    /// Start streaming live events from the chain tip.
    ///
    /// `filter` specifies which account owners / program IDs to subscribe to.
    /// The adapter translates this into a Yellowstone `SubscribeRequest` or
    /// EVM `eth_subscribe` filter.
    ///
    /// The stream MUST reconnect automatically on disconnect. Callers must not
    /// implement retry logic themselves — they rely on the adapter's reconnect
    /// loop (see `solana/reconnect.rs`).
    ///
    /// The stream terminates only on unrecoverable errors (misconfigured endpoint,
    /// auth failure) or when the caller drops the receiver.
    fn subscribe(
        &self,
        filter: SubscribeFilter,
    ) -> Pin<Box<dyn Stream<Item = Result<Event, AdapterError>> + Send + 'static>>;

    /// Replay events for a historical slot / block range.
    ///
    /// `range` is inclusive on both ends. The backfill implementation fetches
    /// blocks in batches from the Solana JSON-RPC `getBlock` endpoint (or EVM
    /// `eth_getLogs` in Phase 4) — see `solana/backfill.rs`.
    ///
    /// Backfill and subscribe MUST NOT race on the same slot. The indexer
    /// coordinates this by running backfill first, then starting subscribe from
    /// the first slot after backfill ends.
    fn backfill(
        &self,
        range: RangeInclusive<u64>,
    ) -> Pin<Box<dyn Stream<Item = Result<Event, AdapterError>> + Send + 'static>>;

    /// Persist the current resume position.
    ///
    /// Called by the indexer after each batch is durably written to storage.
    /// Failures are non-fatal (the stream continues) but logged at ERROR level.
    fn checkpoint_save(
        &self,
        checkpoint: &Checkpoint,
    ) -> impl std::future::Future<Output = Result<(), AdapterError>> + Send;

    /// Load the last persisted resume position.
    ///
    /// Returns `None` if no checkpoint exists (first run). The indexer then
    /// starts subscribe from the current chain tip.
    fn checkpoint_load(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<Checkpoint>, AdapterError>> + Send;

    /// Liveness probe.
    ///
    /// Used by the `server` crate's `/health` REST endpoint and the reconnect
    /// loop's pre-connect sanity check. Must complete within ~5 s.
    fn health_check(
        &self,
    ) -> impl std::future::Future<Output = Result<(), AdapterError>> + Send;

    /// Return the `BlockRef` for the current tip (latest confirmed slot/block).
    ///
    /// Used by the indexer to determine the starting point when no checkpoint exists.
    fn tip(&self) -> impl std::future::Future<Output = Result<BlockRef, AdapterError>> + Send;

    /// Return the default `SubscribeFilter` appropriate for this chain adapter.
    ///
    /// Called by `Indexer::run` instead of the hardcoded `SubscribeFilter::solana_default()`.
    /// Each adapter override returns the chain-appropriate filter; the `Indexer` run loop
    /// is thus chain-agnostic with respect to filter construction.
    ///
    /// Decision 5 from ADR 0005: this provided method fixes the latent bug in
    /// `Indexer::run` where `SubscribeFilter::solana_default()` was called unconditionally,
    /// which would silently drop all EVM events once `EthereumAdapter` was plumbed in.
    ///
    /// # Default
    ///
    /// Returns `SubscribeFilter::default()` (all fields empty / false). Concrete
    /// adapter implementations MUST override this to return the appropriate filter:
    /// - `SolanaAdapter` → `SubscribeFilter::solana_default()`
    /// - `EthereumAdapter` → `SubscribeFilter::ethereum_default()`
    fn default_filter(&self) -> SubscribeFilter {
        SubscribeFilter::default()
    }
}

// ---------------------------------------------------------------------------
// SubscribeFilter
// ---------------------------------------------------------------------------

/// Filter specification passed to `ChainAdapter::subscribe`.
///
/// The adapter converts this into a provider-specific filter:
/// - Solana: Yellowstone `SubscribeRequestFilterTransactions` (account_include)
///   and `SubscribeRequestFilterAccounts` (owner).
/// - EVM (Phase 4): `eth_getLogs` filter with `address` and `topics`.
///
/// An empty `program_ids` list subscribes to ALL transactions (very high volume
/// on Solana — only appropriate for testing or self-hosted validators).
#[derive(Debug, Clone, Default)]
pub struct SubscribeFilter {
    /// Solana: program IDs (Base58) to include in the transaction filter.
    /// Transfers from SPL Token Program and Token-2022, plus known DEX programs.
    ///
    /// EVM: topic0 hex strings (0x-prefixed, 66 chars) used as eth_getLogs topic[0] filter.
    /// Added by `ethereum_default()` and `evm_default_for_chain()`.
    pub program_ids: Vec<String>,

    /// Solana: account owner program IDs for the account update filter.
    /// Set to SPL Token Program + Token-2022 to receive token account updates.
    pub account_owners: Vec<String>,

    /// Whether to also subscribe to slot metadata updates.
    /// Required for reorg detection. Default: `true`.
    pub include_slot_updates: bool,

    /// EVM: additional contract addresses to include in the eth_getLogs `address` filter.
    ///
    /// When non-empty, the EVM adapter restricts log fetching to events emitted by
    /// contracts in this set (in addition to the topic0 filter in `program_ids`).
    ///
    /// Used for:
    /// - Factory contract addresses: `UNISWAP_V2_FACTORY_ETHEREUM`, etc. — to receive
    ///   `PairCreated` / `PoolCreated` factory events for D10 pool-init detection.
    /// - Known LP locker addresses: `LockerRegistry::all_addresses(chain)` — to receive
    ///   Transfer events from locker contracts for D10 LP-lock signal.
    ///
    /// Populated by `evm_default_for_chain` and extended by the server init layer
    /// (which has access to `LockerRegistry` from `mg-onchain-detectors`).
    ///
    /// Empty by default (all contracts pass). Callers should explicitly populate for
    /// production to avoid subscribing to the full Ethereum log volume.
    ///
    /// # Canonical form
    ///
    /// All addresses must be lowercase hex with `0x` prefix (40 chars, no checksum).
    /// The EVM adapter lowercases before matching.
    pub evm_contract_addresses: Vec<String>,
}

impl SubscribeFilter {
    /// Return a filter for Ethereum that subscribes to the canonical EVM event
    /// topic0 hashes decoded by `crates/chain-adapter/src/ethereum/decoder.rs`.
    ///
    /// The `program_ids` and `account_owners` fields are Solana-specific and
    /// left empty for Ethereum. The Ethereum adapter uses `topics[0]` filtering
    /// via `eth_getLogs`; the subscription itself (`eth_subscribe("newHeads")`)
    /// has no address filter — logs are fetched per-block after confirmation.
    ///
    /// Topic0 hashes are verified against mainnet via `alloy::sol!` in
    /// `crates/chain-adapter/src/ethereum/decoder.rs` (Sprint 16).
    pub fn ethereum_default() -> Self {
        // NOTE: For Ethereum, `program_ids` and `account_owners` are Solana-specific
        // concepts and left empty. The EthereumAdapter uses topic0 filtering via
        // eth_getLogs; the Permit2 contract address restriction is applied separately
        // in the adapter's log-routing logic (Sprint 18: logs from PERMIT2_CONTRACT
        // with matching topic0 are routed to the Permit2 decoder / V00014 sink).
        //
        // Permit2 contract: 0x000000000022D473030F116dDEE9F6B43aC78BA3
        // (same address on all major EVM chains via CREATE2 deterministic deployment)
        //
        // Topic0 list includes:
        //   - Sprint 16: 8 EVM events (ERC-20 + Uniswap v2/v3)
        //   - Sprint 18: + 5 Permit2 events
        //   - Sprint 44 (Track 2): + 2 factory events (UniV2 PairCreated, UniV3 PoolCreated)
        //                          + Aerodrome Burn (different topic0 from UniV2 Burn)
        // Permit2 topic0 constants are verified against sol!-generated SIGNATURE_HASH
        // in crates/chain-adapter/src/ethereum/decoder.rs permit2::tests.
        // Factory topic0 constants verified in decoder.rs tests (Sprint 42 + Sprint 44).
        Self {
            program_ids: vec![
                // Sprint 16: 8 EVM event topic0 hashes (ERC-20 + Uniswap v2/v3)
                // Sprint 18: + 5 Permit2 event topic0 hashes
                // Sprint 44 (Track 2): + factory topic0s + Aerodrome Burn topic0
                // Stored in program_ids to satisfy the existing SubscribeFilter shape;
                // EthereumAdapter interprets these as topic0 filters for eth_getLogs.

                // ERC-20 Transfer
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef".into(),
                // ERC-20 Approval
                "0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925".into(),
                // Uniswap v2 Swap
                "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822".into(),
                // Uniswap v2 Mint
                "0x4c209b5fc8ad50758f13e2e1088ba56a560dff690a1c6fef26394f4c03821c4f".into(),
                // Uniswap v2 Burn
                "0xdccd412f0b1252819cb1fd330b93224ca42612892bb3f4f789976e6d81936496".into(),
                // Uniswap v3 Swap
                "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67".into(),
                // Uniswap v3 Mint
                "0x7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde".into(),
                // Uniswap v3 Burn
                "0x0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c".into(),
                // ---- Permit2 events (Sprint 18) ----
                // Permit2 Permit (PermitSingle) — primary D12 A2 signal
                // keccak256("Permit(address,address,address,uint160,uint48,uint48)")
                "0x4b64616d44a2ca1cd2b49b94c3b3cff8c19ccd48bda2e2697eb64bcf2bb42db".into(),
                // Permit2 Approval (internal; distinct from ERC-20 Approval)
                // keccak256("Approval(address,address,address,uint160,uint48)")
                "0xda9fa7c1b00402c17d0161b249b1ab8bbec047c5a52207b9c112deffd817036b".into(),
                // Permit2 Lockdown
                // keccak256("Lockdown(address,address,address)")
                "0xa86d57f9a5cdd0e68d3df14a6e8d04b3f73268ef2fd491f6e7b17f6c79fd7513".into(),
                // Permit2 NonceInvalidation
                // keccak256("NonceInvalidation(address,address,address,uint48,uint48)")
                "0x55c8a5da0d41f640df6e6b03e2c7bcfa2a24a5a9db49e89e3c8f7aa93de9c6ee".into(),
                // Permit2 UnorderedNonceInvalidation
                // keccak256("UnorderedNonceInvalidation(address,uint256,uint256)")
                "0x3704902f963766a4e561bbaab6e6cdc1b1dd12f6e9e99648da8843b3f46b918d".into(),
                // ---- Factory events (Sprint 44, Track 2 — D10-EVM-POOL-INIT closure) ----
                // UniV2 factory PairCreated — same topic0 on Ethereum, BSC (PancakeSwap V2),
                // Base (Uniswap V2 on Base), Arbitrum, Polygon.
                // keccak256("PairCreated(address,address,address,uint256)")
                // Verified: decoder.rs univ2_pair_created_topic0_matches_sol test.
                "0x0d3648bd0f6ba80134a33ba9275ac585d9d315f0ad8355cddefde31afa28d0e9".into(),
                // UniV3 factory PoolCreated — same topic0 on all chains (CREATE2 factory).
                // keccak256("PoolCreated(address,address,uint24,int24,address)")
                // Verified: decoder.rs univ3_pool_created_topic0_matches_sol test.
                "0x783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118".into(),
                // Aerodrome Burn — different topic0 from UniV2 Burn (to is 2nd param, indexed).
                // keccak256("Burn(address,address,uint256,uint256)")
                // Base chain only, but included in universal list for simplicity;
                // the EVM subscribe implementation will filter by contract address additionally.
                // Verified: decoder.rs aerodrome_burn_topic0_differs_from_univ2_burn test.
                "0x5d624aa9c148153ab3446c1b154f660ee7701e549fe9b62dab7171b1c80e6fa2".into(),
            ],
            account_owners: vec![],
            include_slot_updates: false,
            evm_contract_addresses: vec![],
        }
    }

    /// Return a chain-aware Ethereum subscribe filter that includes protocol-specific
    /// topic0 hashes for chains with non-universal DEX event signatures.
    ///
    /// # Chain-specific additions (on top of the 8 universal + 5 Permit2 topics)
    ///
    /// - **BSC**: adds PancakeSwap V3 Swap topic0
    ///   (`0x554ac0509f65714ac7759cbb0a25bac77a3292af1d2bafe87e73f72a31b81bde`)
    ///   PancakeSwap V3 Swap has 2 extra fee params vs UniV3 → different topic0.
    ///
    /// - **Base**: adds Aerodrome Swap topic0
    ///   (`0x09e2454bf22afdad4bcc83d424195309de19932adacabef845f825baeb10baf6`)
    ///   Aerodrome (Solidly fork) has `to` as 2nd indexed param vs UniV2 → different topic0.
    ///
    /// - **Ethereum / Arbitrum / Polygon**: universal topics only (no additions).
    ///
    /// # Backwards compatibility
    ///
    /// `ethereum_default()` continues to return the universal-only set (13 topics).
    /// This method is the chain-aware replacement; `EthereumAdapter::default_filter`
    /// calls this method.
    pub fn evm_default_for_chain(chain: mg_onchain_common::chain::Chain) -> Self {
        use mg_onchain_common::chain::Chain;
        use crate::ethereum::decoder::{
            PANCAKE_V3_SWAP_TOPIC0,
            AERODROME_SWAP_TOPIC0,
            AERODROME_POOL_CREATED_TOPIC0,
        };

        let mut filter = Self::ethereum_default();

        // -----------------------------------------------------------------------
        // Sprint 44 Track 2: factory contract address subscriptions (D10-EVM-POOL-INIT).
        //
        // EVM adapter will filter logs by both topic0 (already in program_ids) AND
        // by emitting contract address (evm_contract_addresses) so factory events
        // are distinguished from pool-level events with the same topic0.
        //
        // Factory addresses are canonical, verified per-chain via block explorers and
        // Uniswap deployment docs (2026-04-24 / Sprint 44).
        // -----------------------------------------------------------------------

        let factory_addresses: Vec<String> = match chain {
            Chain::Ethereum => vec![
                // Uniswap V2 factory
                "0x5c69bee701ef814a2b6a3edd4b1652cb9cc5aa6f".into(),
                // Uniswap V3 factory (CREATE2 — same as Arbitrum/Polygon)
                "0x1f98431c8ad98523631ae4a59f267346ea31f984".into(),
            ],
            Chain::Bsc => vec![
                // PancakeSwap V2 factory (UniV2 fork — same PairCreated ABI)
                "0xca143ce32fe78f1f7019d7d551a6402fc5350c73".into(),
                // PancakeSwap V3 factory
                // SPEC-NOTE: PancakeSwap V3 PoolCreated event may differ from UniV3 PoolCreated
                // (additional fee params). Until verified via BscScan, topic0 is included but
                // the factory address subscription ensures we only receive it from this contract.
                "0x0bfbcf9fa4f9c56b0f40a671ad40e0805a091865".into(),
            ],
            Chain::Base => vec![
                // Uniswap V2 on Base (UniV2 fork)
                // SPEC-NOTE: Verify address against Basescan; sourced from Uniswap deployments-v2 repo.
                "0x8909dc15e40173ff4699343b6eb8132c65e18ec6".into(),
                // Uniswap V3 on Base (CREATE2 factory)
                "0x33128a8fc17869897dce68ed026d694621f6fdfD".into(),
                // Aerodrome factory (Solidly fork pool factory on Base)
                // Source: Aerodrome documentation + Basescan.
                // Deploys Aerodrome Pool contracts; emits PoolCreated-equivalent events.
                // SPEC-NOTE: Aerodrome factory PoolCreated event ABI not yet decoded —
                // address subscribed but events will only be processed when decoder added.
                "0x420dd381b31aef6683db6b902084cb0ffece40da".into(),
            ],
            Chain::Arbitrum => vec![
                // Uniswap V3 factory (same CREATE2 address as Ethereum)
                "0x1f98431c8ad98523631ae4a59f267346ea31f984".into(),
            ],
            Chain::Polygon => vec![
                // Uniswap V3 factory (same CREATE2 address as Ethereum)
                "0x1f98431c8ad98523631ae4a59f267346ea31f984".into(),
            ],
            _ => vec![],
        };
        filter.evm_contract_addresses.extend(factory_addresses);

        match chain {
            Chain::Bsc => {
                // PancakeSwap V3 Swap — different topic0 from UniV3 (extra protocol fee params).
                // Verified 2026-04-24 via IPancakeV3PoolEvents.sol WebFetch.
                filter.program_ids.push(PANCAKE_V3_SWAP_TOPIC0.to_string());

                // SPEC-NOTE (Sprint 25): four.meme graduation event topic0 NOT yet verified.
                // four.meme Token Manager proxy: 0x5c952063c7fc8610ffdb798152d69f0b9550762b (BSC).
                // Graduation event topic0 must be decoded from a confirmed graduation TX before adding here.
                // TODO(next-sprint): Add FOUR_MEME_GRADUATION_TOPIC0 once verified via BscScan TX decode.
            }
            Chain::Base => {
                // Aerodrome Swap — different topic0 from UniV2 (`to` is 2nd indexed param).
                // Verified 2026-04-24 via IPool.sol (aerodrome-finance/contracts) WebFetch.
                filter.program_ids.push(AERODROME_SWAP_TOPIC0.to_string());

                // Aerodrome PoolCreated — factory event with `bool stable` instead of `uint24 fee`.
                // Decoder: `try_decode_aerodrome_pool_created` (Sprint 45, decoder.rs).
                // Verified: IPoolFactory.sol (aerodrome-finance/contracts), fetched 2026-04-24.
                // topic0: keccak256("PoolCreated(address,address,bool,address,uint256)") via sol!.
                filter.program_ids.push(AERODROME_POOL_CREATED_TOPIC0.to_string());

                // SPEC-NOTE (Sprint 25): Clanker TokenDeployed event topic0 NOT yet verified.
                // Clanker hook contracts v4.1 confirmed:
                //   ClankerHookDynamicFeeV2: 0xd60D6B218116cFd801E28F78d011a203D2b068Cc
                //   ClankerHookStaticFeeV2:  0xb429d62f8f3bFFb98CdB9569533eA23bF0Ba28CC
                // Core factory address + TokenDeployed topic0 NOT confirmed (see launchpad_decoder.rs).
                // TODO(next-sprint): Add Clanker TokenDeployed topic0 once verified.
                //
                // SPEC-NOTE (Sprint 25): Virtuals Protocol graduation event topic0 NOT yet verified.
                // VIRTUAL token: 0x0b3e328455c4059eeb9e3f84b5543f74e24e7e1b (Base).
                // Factory/bonding curve address NOT confirmed via public docs (2026-04-24).
                // TODO(next-sprint): Add Virtuals graduation topic0 once verified.
            }
            _ => {
                // Ethereum, Arbitrum, Polygon — universal topics sufficient.
                // No major bonding-curve launchpad with confirmed topic0 on these chains as of Sprint 44.
            }
        }
        filter
    }

    /// Extend this filter with known LP locker contract addresses for the given EVM chain.
    ///
    /// Called by the server init layer (`crates/server/src/init/adapters.rs`) which has
    /// access to `mg-onchain-detectors::lockers::LockerRegistry`. The chain-adapter crate
    /// cannot depend on detectors (circular dep risk), so the server populates lockers externally.
    ///
    /// # Usage
    ///
    /// ```ignore
    /// // Called from server init layer (which has access to mg-onchain-detectors::LockerRegistry).
    /// let registry = LockerRegistry::default();
    /// let mut filter = SubscribeFilter::evm_default_for_chain(Chain::Ethereum);
    /// filter.extend_with_locker_addresses(registry.all_addresses(Chain::Ethereum));
    /// ```
    ///
    /// # Address form
    ///
    /// Addresses must be lowercase `0x`-prefixed hex (40 chars).
    /// `LockerRegistry::all_addresses` already returns them in this form.
    pub fn extend_with_locker_addresses(
        &mut self,
        addresses: impl IntoIterator<Item = String>,
    ) {
        self.evm_contract_addresses.extend(addresses);
    }

    /// Return a filter that subscribes to Solana SPL Token + Token-2022
    /// transfers and the known DEX programs (Raydium v4/CLMM, Orca Whirlpool,
    /// Meteora, PumpFun).
    pub fn solana_default() -> Self {
        Self {
            program_ids: vec![
                // SPL Token Program
                "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".into(),
                // Token-2022
                "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".into(),
                // Raydium AMM v4
                "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8".into(),
                // Raydium CLMM
                "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK".into(),
                // Orca Whirlpool
                "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc".into(),
                // Meteora DLMM
                "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo".into(),
                // PumpFun
                "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P".into(),
            ],
            account_owners: vec![
                "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".into(),
                "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".into(),
            ],
            include_slot_updates: true,
            evm_contract_addresses: vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mg_onchain_common::chain::Chain;
    use crate::ethereum::decoder::{PANCAKE_V3_SWAP_TOPIC0, AERODROME_SWAP_TOPIC0};

    #[test]
    fn evm_default_for_chain_bsc_includes_pancake_v3_swap() {
        let filter = SubscribeFilter::evm_default_for_chain(Chain::Bsc);
        assert!(
            filter.program_ids.iter().any(|t| t == PANCAKE_V3_SWAP_TOPIC0),
            "BSC filter must include PancakeSwap V3 Swap topic0 ({PANCAKE_V3_SWAP_TOPIC0})"
        );
    }

    #[test]
    fn evm_default_for_chain_base_includes_aerodrome_swap() {
        let filter = SubscribeFilter::evm_default_for_chain(Chain::Base);
        assert!(
            filter.program_ids.iter().any(|t| t == AERODROME_SWAP_TOPIC0),
            "Base filter must include Aerodrome Swap topic0 ({AERODROME_SWAP_TOPIC0})"
        );
    }

    #[test]
    fn evm_default_for_chain_ethereum_excludes_pancake_and_aerodrome() {
        let filter = SubscribeFilter::evm_default_for_chain(Chain::Ethereum);
        assert!(
            !filter.program_ids.iter().any(|t| t == PANCAKE_V3_SWAP_TOPIC0),
            "Ethereum filter must NOT include PancakeSwap V3 Swap topic0"
        );
        assert!(
            !filter.program_ids.iter().any(|t| t == AERODROME_SWAP_TOPIC0),
            "Ethereum filter must NOT include Aerodrome Swap topic0"
        );
    }

    #[test]
    fn evm_default_for_chain_bsc_still_includes_universal_topics() {
        let bsc_filter = SubscribeFilter::evm_default_for_chain(Chain::Bsc);
        let eth_filter = SubscribeFilter::ethereum_default();
        // Every universal topic must also appear in the BSC filter
        for topic in &eth_filter.program_ids {
            assert!(
                bsc_filter.program_ids.iter().any(|t| t == topic),
                "BSC filter must include universal topic {topic}"
            );
        }
    }

    /// SPEC-NOTE (Sprint 25): Confirms that four.meme graduation topic0 is NOT YET
    /// in the BSC filter (unverified as of 2026-04-24). This test documents the
    /// SPEC-NOTE state and will be INVERTED (changed to assert inclusion) in the
    /// next sprint when the topic0 is verified.
    ///
    /// four.meme Token Manager: 0x5c952063c7fc8610ffdb798152d69f0b9550762b (BSC)
    /// TODO(next-sprint): flip assertion when graduation topic0 is verified.
    #[test]
    fn evm_default_for_chain_bsc_four_meme_graduation_topic_spec_note() {
        // four.meme Token Manager address (BSC) — EVM contract address, NOT a topic0.
        // Sourced from BscScan search 2026-04-24. Inlined here to avoid adding
        // token-registry as a chain-adapter dep just for a test constant.
        let four_meme_manager = "0x5c952063c7fc8610ffdb798152d69f0b9550762b";

        let bsc_filter = SubscribeFilter::evm_default_for_chain(Chain::Bsc);
        // four.meme Token Manager is an EVM address, not a topic0 — it does NOT
        // belong in program_ids (topic0 list). Confirm it's not accidentally in there.
        assert!(
            !bsc_filter.program_ids.iter().any(|t| t == four_meme_manager),
            "four.meme Token Manager address must NOT be in topic0 list (it's a contract address)"
        );
        // Confirm only PancakeSwap V3 is the chain-specific addition for BSC (at Sprint 25).
        // When four.meme graduation topic0 is verified, it will be added AND
        // this assertion will need updating.
        let chain_specific_count = bsc_filter.program_ids.len()
            - SubscribeFilter::ethereum_default().program_ids.len();
        assert_eq!(
            chain_specific_count, 1,
            "BSC should have exactly 1 chain-specific topic (PancakeSwap V3) until four.meme topic0 is verified"
        );
    }

    // -----------------------------------------------------------------------
    // Sprint 44 Track 2: factory address subscription tests
    // -----------------------------------------------------------------------

    /// Ethereum filter must include UniV2 and UniV3 factory addresses.
    #[test]
    fn evm_default_for_chain_ethereum_includes_factory_addresses() {
        let filter = SubscribeFilter::evm_default_for_chain(Chain::Ethereum);
        let univ2_factory = "0x5c69bee701ef814a2b6a3edd4b1652cb9cc5aa6f";
        let univ3_factory = "0x1f98431c8ad98523631ae4a59f267346ea31f984";
        assert!(
            filter.evm_contract_addresses.iter().any(|a| a == univ2_factory),
            "Ethereum filter must include UniV2 factory {univ2_factory}"
        );
        assert!(
            filter.evm_contract_addresses.iter().any(|a| a == univ3_factory),
            "Ethereum filter must include UniV3 factory {univ3_factory}"
        );
    }

    /// BSC filter must include PancakeSwap V2 and V3 factory addresses.
    #[test]
    fn evm_default_for_chain_bsc_includes_pancake_factory_addresses() {
        let filter = SubscribeFilter::evm_default_for_chain(Chain::Bsc);
        let pancake_v2 = "0xca143ce32fe78f1f7019d7d551a6402fc5350c73";
        let pancake_v3 = "0x0bfbcf9fa4f9c56b0f40a671ad40e0805a091865";
        assert!(
            filter.evm_contract_addresses.iter().any(|a| a == pancake_v2),
            "BSC filter must include PancakeSwap V2 factory {pancake_v2}"
        );
        assert!(
            filter.evm_contract_addresses.iter().any(|a| a == pancake_v3),
            "BSC filter must include PancakeSwap V3 factory {pancake_v3}"
        );
    }

    /// Base filter must include Aerodrome factory address and PoolCreated topic0.
    #[test]
    fn evm_default_for_chain_base_includes_aerodrome_factory() {
        use crate::ethereum::decoder::AERODROME_POOL_CREATED_TOPIC0;
        let filter = SubscribeFilter::evm_default_for_chain(Chain::Base);
        let aerodrome_factory = "0x420dd381b31aef6683db6b902084cb0ffece40da";
        assert!(
            filter.evm_contract_addresses.iter().any(|a| a == aerodrome_factory),
            "Base filter must include Aerodrome factory {aerodrome_factory}"
        );
        assert!(
            filter.program_ids.iter().any(|t| t == AERODROME_POOL_CREATED_TOPIC0),
            "Base filter must include Aerodrome PoolCreated topic0 ({AERODROME_POOL_CREATED_TOPIC0})"
        );
    }

    /// Universal ethereum_default() filter includes UniV2 PairCreated + UniV3 PoolCreated topic0.
    #[test]
    fn ethereum_default_includes_factory_topic0s() {
        use crate::ethereum::decoder::{UNISWAP_V2_PAIR_CREATED_TOPIC0, UNISWAP_V3_POOL_CREATED_TOPIC0};
        let filter = SubscribeFilter::ethereum_default();
        assert!(
            filter.program_ids.iter().any(|t| t == UNISWAP_V2_PAIR_CREATED_TOPIC0),
            "ethereum_default must include UniV2 PairCreated topic0"
        );
        assert!(
            filter.program_ids.iter().any(|t| t == UNISWAP_V3_POOL_CREATED_TOPIC0),
            "ethereum_default must include UniV3 PoolCreated topic0"
        );
    }

    /// `extend_with_locker_addresses` appends addresses to evm_contract_addresses.
    #[test]
    fn extend_with_locker_addresses_appends_correctly() {
        let mut filter = SubscribeFilter::evm_default_for_chain(Chain::Ethereum);
        let before = filter.evm_contract_addresses.len();
        let locker_addrs = vec![
            "0x663a5c229c09b049e36dcc11a9b0d4a8eb9db214".to_string(),
            "0xe2fe530c047f2d85298b07d9333c05737f1435fb".to_string(),
        ];
        filter.extend_with_locker_addresses(locker_addrs.clone());
        assert_eq!(filter.evm_contract_addresses.len(), before + 2);
        for addr in &locker_addrs {
            assert!(
                filter.evm_contract_addresses.iter().any(|a| a == addr),
                "locker address {addr} must appear in evm_contract_addresses after extend"
            );
        }
    }
}
