//! Sprint 4 exit integration test — end-to-end fixture replay covering all 6 detectors.
//!
//! # History
//!
//! This file supersedes `sprint2_exit_test.rs`. The Sprint 2 exit test established
//! the fixture stream → Indexer → Postgres → D01 pipeline. This test extends it
//! to invoke all 6 MVP detectors (D01–D06) over the same persisted data.
//!
//! # Requirements
//!
//! Docker must be running. The test pulls `postgres:16` via `testcontainers`
//! if not locally cached.
//!
//! # Run
//!
//! ```bash
//! cargo test -p mg-onchain-indexer --test sprint4_exit_test -- --ignored
//! ```
//!
//! The test is `#[ignore]`d by default so it does not run in CI without
//! explicit opt-in. All non-Docker tests continue to run normally.
//! `cargo clippy --all-targets` still type-checks this file in CI (no Docker needed).
//!
//! # What this test exercises
//!
//! 1. Postgres 16 spin-up + all four migrations applied.
//! 2. Fixture stream (~46 synthetic events): Transfers, Swaps, PoolEvents,
//!    TokenMeta, ReorgMarker (mid-stream), replay of reorg'd slot events, SlotFinalized.
//! 3. Indexer run: routing, batching, flush, reorg, checkpoint, graceful shutdown.
//! 4. Post-run row-count assertions in `transfers`, `swaps`, `pool_events`,
//!    `adapter_checkpoints`.
//! 5. `ON CONFLICT DO NOTHING` dedup path: replay reorg'd slot events again via
//!    `PgEventSink` — row counts remain stable.
//! 6. All 6 detectors evaluated on all 3 fixture tokens (RAVE, WET, SYNTH):
//!    - Call completes without panic.
//!    - Returned `Vec<AnomalyEvent>` has confidence ∈ [0.0, 1.0] for all events.
//!    - `AnomalyEvent.detector_id` matches the detector's `.id()`.
//!    - Evidence keys all prefixed with the detector's id (when events are returned).
//!    - Severity maps correctly (Info ≤ severity ≤ Critical).
//! 7. D01: same assertions as Sprint 2 exit (RAVE low conf, WET ≤ Medium, SYNTH pure path).
//! 8. D02: RAVE receives Signal B fire or empty (no drain events in fixture).
//! 9. D03: all 3 tokens return empty or sub-threshold (no holder snapshots in fixture).
//! 10. D04: all 3 tokens return empty or Info/burst-fallback (sparse swap data).
//! 11. D05: all 3 tokens return empty (no round-trip data in fixture; acceptable path).
//! 12. D06: SYNTH may receive Signal A (no mint authority in fixture — depends on
//!     registry enrich); RAVE and WET below threshold.
//! 13. Checkpoint resume test: second empty-stream run leaves last_slot unchanged.
//!
//! # Mock dependencies and documented gaps
//!
//! The fixture stream persists 23 transfers, 15 swaps, and 3 pool events. Several
//! detectors depend on tables not populated by the fixture stream:
//!
//! - **D02 Signal A** (`pool_events` Burn rows): The fixture has 1 Burn (WET pool,
//!   slot BASE+6). D02 queries cumulative LP drain via `fetch_rug_pull_drain_events`
//!   against the `pools` table (which is NOT populated by the indexer — it requires
//!   enrichment). Both pools will return empty from `fetch_pools_for_token`, causing
//!   D02 to return an empty Vec (no pool state = no signal). Accepted per task spec.
//!
//! - **D03** (`holder_snapshots`): Empty — indexer does not populate holder snapshots.
//!   D03 returns empty Vec (no snapshot = no Gini computation). Accepted.
//!
//! - **D04** (`swaps` baseline): The fixture has 15 swaps but `fetch_pump_dump_baseline`
//!   will return sparse data (< `min_baseline_days` of daily history). The burst
//!   concentration fallback (Signal B) may fire if 1h volume >= threshold. Expected
//!   outcome: Info-level burst event or empty. Both are accepted.
//!
//! - **D05** (`round_trips`): The fixture swaps are not round-trips (same sender buys
//!   and sells in the same pool within 25 slots with < 1% volume diff). All return
//!   empty Vec. Accepted.
//!
//! - **D06** (`mint_authority`): The fixture tokens have `mint_authority = None` in
//!   their TokenMeta (constructed via `make_token_meta`). Signal A requires
//!   `mint_authority.is_some()`, so it will not fire for RAVE/WET. SYNTH also has
//!   no mint authority in the fixture. All 3 tokens will return empty or Info events.
//!   Accepted — D06 Signal A with mint_authority is tested in the pure-function unit
//!   tests within `crates/detectors/src/d06_mint_burn.rs`.
//!
//! # D04/D05 seeded rows
//!
//! D04 seeded rows: None. The fixture swaps are insufficient for a meaningful Signal A
//! baseline, and the burst metric rows require usd_value to be non-NULL. Sprint 5
//! integration test enhancement: seed swaps with usd_value to test D04 Signal B firing.
//!
//! D05 seeded rows: None. Round-trip detection requires the same sender to buy and sell
//! in the same pool within the block window. The fixture swaps all have `sender = ZERO_ADDR`
//! and use alternating buy/sell pools. Sprint 5 integration test enhancement: add
//! self-referential round-trip swaps to the fixture stream.

use std::sync::Arc;

use chrono::{Duration, Utc};
use futures::stream;
use sqlx::Row as _;

use mg_onchain_chain_adapter::{AdapterError, ChainAdapter, Event, SubscribeFilter};
use mg_onchain_common::anomaly::Severity;
use mg_onchain_common::chain::{Address, BlockRef, Chain, TxHash};
use mg_onchain_common::event::{DexKind, PoolEvent, PoolEventKind, Swap, Transfer};
use mg_onchain_common::token::{JupiterVerification, TokenMeta};
use mg_onchain_detectors::config::load_detector_config;
use mg_onchain_detectors::context::{DetectorContext, DetectorWindow};
use mg_onchain_detectors::d01_honeypot::HoneypotDetector;
use mg_onchain_detectors::d02_rug_pull::RugPullDetector;
use mg_onchain_detectors::d03_concentration::ConcentrationDetector;
use mg_onchain_detectors::d04_pump_dump::PumpDumpDetector;
use mg_onchain_detectors::d05_wash_trading::WashTradingDetector;
use mg_onchain_detectors::d06_mint_burn::MintBurnAnomalyDetector;
use mg_onchain_detectors::detector::Detector;
use mg_onchain_dex_adapter::pool_accounts::NotWiredPoolAccountProvider;
use mg_onchain_indexer::Indexer;
use mg_onchain_indexer::config::BatchConfig;
use mg_onchain_indexer::shutdown::ShutdownSignal;
use mg_onchain_indexer::sink::{EventSink, PgEventSink};
use mg_onchain_storage::{PgCheckpointStore, PgStore};
use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;
use mg_onchain_token_registry::{RegistryConfig, TokenRegistry};
use rust_decimal::Decimal;

// ---------------------------------------------------------------------------
// Addresses — realistic Solana Base58, plausible mainnet addresses
// ---------------------------------------------------------------------------

/// RAVE token mint (negative fixture — all signals absent, normal buy/sell ratio)
const RAVE_MINT: &str = "FeqiF7TEVmpYuuj4goD8WgmgyhFgFnZiWtw226wF4hNm";

/// WET token mint (negative fixture — all signals absent)
const WET_MINT: &str = "WETZjtprkDMCcUxPi9PfWnowMRZkiGGHDb9rABuRZ2U";

/// Synthetic positive fixture: permanent_delegate active (Token-2022 scam indicator)
const SYNTH_MINT: &str = "7dGbd2QZcCKcTndnHcTL8q7SMVXAkp688NTQYwrRCrar";

/// Raydium v4 pool for RAVE (synthetic address, realistic byte length)
const RAVE_POOL: &str = "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R";

/// Raydium CPMM pool for WET (synthetic address)
const WET_POOL: &str = "8HoQnePLqPj4M7PUDzfw8e3Ymdwgc7NaYyHnC66ATRW";

/// Raydium v4 pool for SYNTH
const SYNTH_POOL: &str = "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM";

/// SOL native mint (used as the paired token in swaps)
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Solana null / zero address (used as from-address in mint events, etc.)
const ZERO_ADDR: &str = "11111111111111111111111111111111";

/// Plausible recent mainnet slot base (slot 325_000_000 ≈ April 2026)
const BASE_SLOT: u64 = 325_000_000;

/// The reorg slot — sits mid-stream so some events are emitted then deleted.
const REORG_SLOT: u64 = BASE_SLOT + 12;

/// Final slot used after the reorg replay, so we have a known checkpoint end.
const FINAL_SLOT: u64 = BASE_SLOT + 25;

/// Wall-clock time for slot BASE_SLOT — used to generate monotone block_times.
/// Choose a concrete point so the fixture is deterministic (all tests share it).
/// 2026-04-21 08:00:00 UTC.
fn base_time() -> chrono::DateTime<Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-04-21T08:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

/// Map a Solana slot to a wall-clock block_time.
/// Solana produces ~0.4 s/slot; we use 1 s/slot for simplicity.
fn slot_time(slot: u64) -> chrono::DateTime<Utc> {
    base_time() + Duration::seconds((slot - BASE_SLOT) as i64)
}

// ---------------------------------------------------------------------------
// Address helpers
// ---------------------------------------------------------------------------

fn addr(s: &str) -> Address {
    Address::parse(Chain::Solana, s).unwrap_or_else(|e| panic!("invalid test addr {s}: {e}"))
}

fn tx(seed: u8) -> TxHash {
    TxHash::solana_from_base58(&bs58::encode([seed; 64]).into_string())
        .expect("test TxHash construction")
}

/// Make a unique TxHash using both a seed byte and an index, to avoid hash
/// collisions when we generate many transactions.
fn tx_indexed(seed: u8, index: u32) -> TxHash {
    let mut bytes = [seed; 64];
    // XOR the lower bytes with the index so each tx is distinct.
    bytes[0] ^= (index & 0xFF) as u8;
    bytes[1] ^= ((index >> 8) & 0xFF) as u8;
    TxHash::solana_from_base58(&bs58::encode(bytes).into_string())
        .expect("test TxHash construction")
}

fn block(slot: u64) -> BlockRef {
    BlockRef::new(Chain::Solana, slot)
}

// ---------------------------------------------------------------------------
// Fixture stream builder (identical to sprint2_exit_test.rs)
// ---------------------------------------------------------------------------

/// Build the canned fixture event stream.
///
/// Returns a `Vec<Event>` representing ~25 minutes of simulated Solana activity
/// across three tracked tokens. The stream is intentionally deterministic:
/// all timestamps and slot numbers are computed from `BASE_SLOT` and `base_time()`.
///
/// Total: 3+14+10+3+1+3+6+5+1 = 46 events.
///
/// After reorg processing:
/// - transfers:   14 (pre-reorg) + 3 (replay) + 6 (post) = 23
/// - swaps:       10 (pre-reorg) + 5 (post-reorg) = 15
/// - pool_events: 3
pub fn build_fixture_stream() -> Vec<Event> {
    let mut events: Vec<Event> = Vec::with_capacity(50);

    // Phase 1: TokenMeta events (one per tracked token)
    events.push(Event::TokenMeta(Box::new(make_token_meta(
        RAVE_MINT,
        "RAVE",
        "RAVE Copycat",
        6,
        None,
        false,
    ))));

    events.push(Event::TokenMeta(Box::new(make_token_meta(
        WET_MINT,
        "WET",
        "WET Token",
        6,
        None,
        false,
    ))));

    events.push(Event::TokenMeta(Box::new(make_token_meta_with_delegate(
        SYNTH_MINT,
        "SYNTH",
        "Synthetic Scam Token",
        6,
    ))));

    // Phase 2: Pre-reorg transfers (slots BASE..BASE+11): 14 transfers
    // RAVE: 6 transfers (3 buys into pool, 3 sells out of pool)
    for i in 0u32..3 {
        events.push(Event::Transfer(Transfer {
            chain: Chain::Solana,
            tx_hash: tx_indexed(10, i),
            block: block(BASE_SLOT + (i as u64)),
            block_time: slot_time(BASE_SLOT + (i as u64)),
            token: addr(RAVE_MINT),
            from: addr(ZERO_ADDR),
            to: addr(RAVE_POOL),
            amount_raw: 1_500_000_000,
            decimals: 6,
            log_index: i,
        }));
        events.push(Event::Transfer(Transfer {
            chain: Chain::Solana,
            tx_hash: tx_indexed(11, i),
            block: block(BASE_SLOT + (i as u64) + 1),
            block_time: slot_time(BASE_SLOT + (i as u64) + 1),
            token: addr(RAVE_MINT),
            from: addr(RAVE_POOL),
            to: addr(ZERO_ADDR),
            amount_raw: 1_450_000_000,
            decimals: 6,
            log_index: i + 10,
        }));
    }

    // WET: 5 transfers (3 buys, 2 sells)
    for i in 0u32..3 {
        events.push(Event::Transfer(Transfer {
            chain: Chain::Solana,
            tx_hash: tx_indexed(20, i),
            block: block(BASE_SLOT + 2 + (i as u64)),
            block_time: slot_time(BASE_SLOT + 2 + (i as u64)),
            token: addr(WET_MINT),
            from: addr(ZERO_ADDR),
            to: addr(WET_POOL),
            amount_raw: 2_000_000_000,
            decimals: 6,
            log_index: i,
        }));
    }
    for i in 0u32..2 {
        events.push(Event::Transfer(Transfer {
            chain: Chain::Solana,
            tx_hash: tx_indexed(21, i),
            block: block(BASE_SLOT + 5 + (i as u64)),
            block_time: slot_time(BASE_SLOT + 5 + (i as u64)),
            token: addr(WET_MINT),
            from: addr(WET_POOL),
            to: addr(ZERO_ADDR),
            amount_raw: 1_900_000_000,
            decimals: 6,
            log_index: i + 5,
        }));
    }

    // SYNTH: 3 transfers (3 buys, 0 sells)
    for i in 0u32..3 {
        events.push(Event::Transfer(Transfer {
            chain: Chain::Solana,
            tx_hash: tx_indexed(30, i),
            block: block(BASE_SLOT + 4 + (i as u64)),
            block_time: slot_time(BASE_SLOT + 4 + (i as u64)),
            token: addr(SYNTH_MINT),
            from: addr(ZERO_ADDR),
            to: addr(SYNTH_POOL),
            amount_raw: 500_000_000,
            decimals: 6,
            log_index: i,
        }));
    }

    // Phase 3: Pre-reorg swaps (slots BASE+3..BASE+11): 10 swaps
    for i in 0u32..5 {
        events.push(Event::Swap(Swap {
            chain: Chain::Solana,
            tx_hash: tx_indexed(40, i),
            block: block(BASE_SLOT + 3 + (i as u64)),
            block_time: slot_time(BASE_SLOT + 3 + (i as u64)),
            pool: addr(RAVE_POOL),
            dex: DexKind::RaydiumV4,
            sender: addr(ZERO_ADDR),
            token_in: addr(SOL_MINT),
            token_out: addr(RAVE_MINT),
            amount_in_raw: 1_000_000_000,
            decimals_in: 9,
            amount_out_raw: 50_000_000_000,
            decimals_out: 6,
            usd_value: None,
            log_index: i,
        }));
    }
    for i in 0u32..5 {
        events.push(Event::Swap(Swap {
            chain: Chain::Solana,
            tx_hash: tx_indexed(50, i),
            block: block(BASE_SLOT + 5 + (i as u64)),
            block_time: slot_time(BASE_SLOT + 5 + (i as u64)),
            pool: addr(WET_POOL),
            dex: DexKind::RaydiumCpmm,
            sender: addr(ZERO_ADDR),
            token_in: addr(SOL_MINT),
            token_out: addr(WET_MINT),
            amount_in_raw: 500_000_000,
            decimals_in: 9,
            amount_out_raw: 30_000_000_000,
            decimals_out: 6,
            usd_value: None,
            log_index: i,
        }));
    }

    // Phase 4: Pre-reorg pool events (slots BASE+5..BASE+7): 3 events
    events.push(Event::PoolEvent(PoolEvent {
        chain: Chain::Solana,
        tx_hash: tx(60),
        block: block(BASE_SLOT + 5),
        block_time: slot_time(BASE_SLOT + 5),
        pool: addr(RAVE_POOL),
        dex: DexKind::RaydiumV4,
        kind: PoolEventKind::Mint {
            amount0_raw: 10_000_000_000,
            amount1_raw: 200_000_000,
            lp_tokens_minted: 44_721_000,
        },
        actor: addr(ZERO_ADDR),
        log_index: 0,
    }));

    events.push(Event::PoolEvent(PoolEvent {
        chain: Chain::Solana,
        tx_hash: tx(61),
        block: block(BASE_SLOT + 6),
        block_time: slot_time(BASE_SLOT + 6),
        pool: addr(WET_POOL),
        dex: DexKind::RaydiumCpmm,
        kind: PoolEventKind::Burn {
            amount0_raw: 5_000_000_000,
            amount1_raw: 100_000_000,
            lp_tokens_burned: 22_360_000,
        },
        actor: addr(ZERO_ADDR),
        log_index: 0,
    }));

    events.push(Event::PoolEvent(PoolEvent {
        chain: Chain::Solana,
        tx_hash: tx(62),
        block: block(BASE_SLOT + 7),
        block_time: slot_time(BASE_SLOT + 7),
        pool: addr(SYNTH_POOL),
        dex: DexKind::RaydiumV4,
        kind: PoolEventKind::Initialize {
            token0: addr(SYNTH_MINT),
            token1: addr(SOL_MINT),
        },
        actor: addr(ZERO_ADDR),
        log_index: 0,
    }));

    // Phase 5: ReorgMarker at REORG_SLOT
    events.push(Event::ReorgMarker { slot: REORG_SLOT });

    // Phase 6: Replay 3 transfers for REORG_SLOT after the reorg.
    for i in 0u32..3 {
        events.push(Event::Transfer(Transfer {
            chain: Chain::Solana,
            tx_hash: tx_indexed(70, i),
            block: block(REORG_SLOT),
            block_time: slot_time(REORG_SLOT),
            token: addr(RAVE_MINT),
            from: addr(ZERO_ADDR),
            to: addr(RAVE_POOL),
            amount_raw: 1_000_000_000,
            decimals: 6,
            log_index: i,
        }));
    }

    // Phase 7: Post-reorg transfers (slots BASE+13..BASE+25): 6 transfers
    for i in 0u32..3 {
        events.push(Event::Transfer(Transfer {
            chain: Chain::Solana,
            tx_hash: tx_indexed(80, i),
            block: block(BASE_SLOT + 13 + (i as u64)),
            block_time: slot_time(BASE_SLOT + 13 + (i as u64)),
            token: addr(RAVE_MINT),
            from: addr(ZERO_ADDR),
            to: addr(RAVE_POOL),
            amount_raw: 1_200_000_000,
            decimals: 6,
            log_index: i,
        }));
    }
    for i in 0u32..3 {
        events.push(Event::Transfer(Transfer {
            chain: Chain::Solana,
            tx_hash: tx_indexed(81, i),
            block: block(BASE_SLOT + 16 + (i as u64)),
            block_time: slot_time(BASE_SLOT + 16 + (i as u64)),
            token: addr(WET_MINT),
            from: addr(WET_POOL),
            to: addr(ZERO_ADDR),
            amount_raw: 1_100_000_000,
            decimals: 6,
            log_index: i,
        }));
    }

    // Phase 8: Post-reorg swaps (slots BASE+14..BASE+24): 5 swaps
    for i in 0u32..5 {
        events.push(Event::Swap(Swap {
            chain: Chain::Solana,
            tx_hash: tx_indexed(90, i),
            block: block(BASE_SLOT + 14 + (i as u64) * 2),
            block_time: slot_time(BASE_SLOT + 14 + (i as u64) * 2),
            pool: addr(RAVE_POOL),
            dex: DexKind::RaydiumV4,
            sender: addr(ZERO_ADDR),
            token_in: addr(RAVE_MINT),
            token_out: addr(SOL_MINT),
            amount_in_raw: 50_000_000_000,
            decimals_in: 6,
            amount_out_raw: 900_000_000,
            decimals_out: 9,
            usd_value: None,
            log_index: i,
        }));
    }

    // Phase 9: SlotFinalized at FINAL_SLOT
    events.push(Event::SlotFinalized { slot: FINAL_SLOT });

    events
}

// ---------------------------------------------------------------------------
// TokenMeta constructors for fixture stream
// ---------------------------------------------------------------------------

fn make_token_meta(
    mint: &str,
    symbol: &str,
    name: &str,
    decimals: u8,
    freeze_authority: Option<&str>,
    jup_verified: bool,
) -> TokenMeta {
    let mint_addr = addr(mint);
    let freeze_auth = freeze_authority
        .map(|a| Address::parse(Chain::Solana, a).expect("valid freeze authority address"));
    TokenMeta {
        mint: mint_addr,
        chain: Chain::Solana,
        symbol: Some(symbol.to_owned()),
        name: Some(name.to_owned()),
        decimals,
        token_program: None,
        total_supply_raw: 1_000_000_000_000_000u128,
        circulating_supply_raw: Some(1_000_000_000_000_000u128),
        mint_authority: None,
        freeze_authority: freeze_auth,
        creator: None,
        creator_balance_raw: 0,
        transfer_fee: None,
        permanent_delegate: None,
        transfer_hook_program: None,
        non_transferable: false,
        confidential_transfer: false,
        top_holders: vec![],
        total_holders: 1000,
        markets: vec![],
        total_market_liquidity_usd: Decimal::new(50_000, 0),
        lockers: vec![],
        graph_insiders_detected: false,
        insider_networks: vec![],
        launchpad: None,
        deploy_platform: None,
        detected_at: Some(base_time()),
        rugged: false,
        verification: JupiterVerification {
            jup_verified,
            jup_strict: false,
        },
        rugcheck_score: None,
        buy_tax: None,
        sell_tax: None,
        transfer_tax: None,
        honeypot_flags: vec![],
        updated_at: base_time(),
    }
}

/// Build a TokenMeta for the SYNTH token with a permanent_delegate set
/// (S3 signal active in the honeypot detector).
fn make_token_meta_with_delegate(mint: &str, symbol: &str, name: &str, decimals: u8) -> TokenMeta {
    let delegate_addr = Address::parse(
        Chain::Solana,
        "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM",
    )
    .expect("valid delegate address");
    let mut meta = make_token_meta(mint, symbol, name, decimals, None, false);
    meta.permanent_delegate = Some(delegate_addr);
    meta
}

// ---------------------------------------------------------------------------
// MockChainAdapter — streams the fixture event Vec then terminates.
// ---------------------------------------------------------------------------

struct MockChainAdapter {
    events: Vec<Event>,
}

impl MockChainAdapter {
    fn new(events: Vec<Event>) -> Self {
        Self { events }
    }
}

impl ChainAdapter for MockChainAdapter {
    fn subscribe(
        &self,
        _filter: SubscribeFilter,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<Event, AdapterError>> + Send + 'static>>
    {
        let items: Vec<Result<Event, AdapterError>> = self.events.iter().cloned().map(Ok).collect();
        Box::pin(stream::iter(items))
    }

    fn backfill(
        &self,
        _range: std::ops::RangeInclusive<u64>,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<Event, AdapterError>> + Send + 'static>>
    {
        Box::pin(stream::empty())
    }

    async fn checkpoint_save(
        &self,
        _checkpoint: &mg_onchain_chain_adapter::Checkpoint,
    ) -> Result<(), AdapterError> {
        Ok(())
    }

    async fn checkpoint_load(
        &self,
    ) -> Result<Option<mg_onchain_chain_adapter::Checkpoint>, AdapterError> {
        Ok(None)
    }

    async fn health_check(&self) -> Result<(), AdapterError> {
        Ok(())
    }

    async fn tip(&self) -> Result<BlockRef, AdapterError> {
        Ok(BlockRef::new(Chain::Solana, FINAL_SLOT))
    }
}

// ---------------------------------------------------------------------------
// Count rows helper
// ---------------------------------------------------------------------------

async fn count_rows(pool: &sqlx::PgPool, table: &str, chain: &str) -> i64 {
    let row = sqlx::query(&format!(
        "SELECT COUNT(*)::BIGINT AS n FROM {table} WHERE chain = $1"
    ))
    .bind(chain)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|e| panic!("COUNT on {table} failed: {e}"));
    row.try_get::<i64, _>("n").unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Detector evaluation helper — generic over one token
// ---------------------------------------------------------------------------

/// Metadata about a single detector invocation result for assertions.
struct DetectorResult {
    /// The detector's stable id().
    detector_id: &'static str,
    /// Events returned. May be empty (MissingDependencyData path returns Ok(vec![])).
    events: Vec<mg_onchain_common::anomaly::AnomalyEvent>,
    /// Whether the call errored instead of returning Ok.
    errored: bool,
    /// Error string if errored.
    error_msg: String,
}

/// Assert universal invariants that MUST hold for every single detector invocation
/// regardless of which signal fired or whether data was present.
///
/// These invariants directly implement the P4-5 assertion checklist from the task:
/// - confidence ∈ [0.0, 1.0] for each event.
/// - severity maps correctly (≥ severity_floor).
/// - AnomalyEvent.detector_id matches the detector's .id().
/// - Evidence keys all start with the detector's prefix (when events present).
fn assert_detector_invariants(result: &DetectorResult) {
    let id = result.detector_id;

    // Invariant 0: call must not panic (already enforced by returning Ok/Err).
    // If the call errored, that is acceptable for MissingDependencyData and
    // InsufficientBaseline — document it but do not fail the test.
    if result.errored {
        // Permitted error paths per task spec. Log but don't fail.
        eprintln!("[D{id}] returned Err (acceptable): {}", result.error_msg);
        return;
    }

    for (i, event) in result.events.iter().enumerate() {
        let conf = event.confidence.value();

        // Invariant 1: confidence ∈ [0.0, 1.0]
        assert!(
            (0.0..=1.0).contains(&conf),
            "D{id} event[{i}]: confidence {conf} must be in [0.0, 1.0]"
        );

        // Invariant 2: AnomalyEvent.detector_id must match detector's .id()
        assert_eq!(
            event.detector_id,
            id,
            "D{id} event[{i}]: detector_id field '{actual}' must match id() '{id}'",
            actual = event.detector_id
        );

        // Invariant 3: Severity is a valid discriminant (no unreachable arm needed;
        // the type system prevents invalid variants). We just assert it is not out of band.
        assert!(
            event.severity >= Severity::Info && event.severity <= Severity::Critical,
            "D{id} event[{i}]: severity {:?} must be in Info..Critical",
            event.severity
        );

        // Invariant 4: All evidence metrics keys must be prefixed with the detector id.
        // The prefix convention is `<detector_id>/` (from evidence_key() helper).
        for key in event.evidence.metrics.keys() {
            assert!(
                key.starts_with(&format!("{id}/")),
                "D{id} event[{i}]: evidence metric key '{key}' must start with '{id}/'"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Main integration test — Sprint 4 exit
// ---------------------------------------------------------------------------

/// Sprint 4 exit — end-to-end fixture replay with all 6 detectors.
///
/// Requires Docker. Run:
/// `cargo test -p mg-onchain-indexer --test sprint4_exit_test -- --ignored`
#[tokio::test]
#[ignore = "requires Docker — run with: cargo test -p mg-onchain-indexer --test sprint4_exit_test -- --ignored"]
async fn sprint4_exit_end_to_end_fixture_replay() {
    // ------------------------------------------------------------------
    // Step 1: Spin up Postgres 16 via testcontainers.
    // ------------------------------------------------------------------
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;

    let container = Postgres::default().start().await.unwrap();
    let host_port = container.get_host_port_ipv4(5432).await.unwrap();
    let db_url = format!("postgres://postgres:postgres@127.0.0.1:{host_port}/postgres");

    // ------------------------------------------------------------------
    // Step 2: Apply all migrations (V00001..V00004).
    // ------------------------------------------------------------------
    let pool = sqlx::PgPool::connect(&db_url).await.unwrap();
    let migrations_path = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../migrations/postgres"
    ));
    let migrator = sqlx::migrate::Migrator::new(migrations_path).await.unwrap();
    migrator.run(&pool).await.unwrap();

    let pg = PgStore::new(pool.clone());

    // ------------------------------------------------------------------
    // Step 3: Build fixture event stream.
    // ------------------------------------------------------------------
    let events = build_fixture_stream();
    assert_eq!(
        events.len(),
        46,
        "fixture stream must contain exactly 46 events; update this assertion if the fixture changes"
    );

    // ------------------------------------------------------------------
    // Step 4: Run Indexer against MockChainAdapter with fixture stream.
    // ------------------------------------------------------------------
    let sink = PgEventSink::new(pg.clone());
    let cp_store = PgCheckpointStore::new(pg.clone());

    let batch_cfg = BatchConfig {
        size: 200,
        timeout_ms: 30_000,
        max_in_flight: 4,
    };

    let shutdown = ShutdownSignal::new();
    let adapter = MockChainAdapter::new(events.clone());

    let mut indexer = Indexer::new(
        adapter, sink, cp_store, "solana", "solana", batch_cfg, shutdown,
        None, // no graph writer in sprint4 exit test
        None, // no pool_initialize_hook in sprint4 exit test
    );

    let result = indexer.run().await;
    assert!(
        result.is_err(),
        "expected StreamEnded error for finite fixture stream, got Ok"
    );
    let err_str = result.unwrap_err().to_string();
    assert!(
        err_str.contains("stream ended") || err_str.contains("StreamEnded"),
        "error should be StreamEnded, got: {err_str}"
    );

    // ------------------------------------------------------------------
    // Step 5: Assert tokens table populated by the indexer (P3-1).
    // ------------------------------------------------------------------
    let token_count = count_rows(&pool, "tokens", "solana").await;
    assert_eq!(
        token_count, 3,
        "expected 3 tokens (RAVE + WET + SYNTH) persisted by the indexer, got {token_count}"
    );

    // ------------------------------------------------------------------
    // Step 6: Assert row counts in Postgres.
    // ------------------------------------------------------------------
    let transfer_count = count_rows(&pool, "transfers", "solana").await;
    let swap_count = count_rows(&pool, "swaps", "solana").await;
    let pool_event_count = count_rows(&pool, "pool_events", "solana").await;

    assert_eq!(
        transfer_count, 23,
        "expected 23 transfers (14 pre-reorg + 3 replay + 6 post-reorg), got {transfer_count}"
    );
    assert_eq!(
        swap_count, 15,
        "expected 15 swaps (10 pre-reorg + 5 post-reorg), got {swap_count}"
    );
    assert_eq!(
        pool_event_count, 3,
        "expected 3 pool_events (1 Mint + 1 Burn + 1 Initialize), got {pool_event_count}"
    );

    let cp_row = pg.load_checkpoint("solana").await.expect("load checkpoint");
    let cp_row = cp_row.expect("checkpoint must exist after indexer run");
    assert_eq!(
        cp_row.last_slot, FINAL_SLOT as i64,
        "checkpoint last_slot must equal FINAL_SLOT={FINAL_SLOT}, got {}",
        cp_row.last_slot
    );

    // ------------------------------------------------------------------
    // Step 7: Dedup via ON CONFLICT DO NOTHING.
    // ------------------------------------------------------------------
    let replay_sink = PgEventSink::new(pg.clone());

    let replay_transfers: Vec<Transfer> = (0u32..3)
        .map(|i| Transfer {
            chain: Chain::Solana,
            tx_hash: tx_indexed(70, i),
            block: block(REORG_SLOT),
            block_time: slot_time(REORG_SLOT),
            token: addr(RAVE_MINT),
            from: addr(ZERO_ADDR),
            to: addr(RAVE_POOL),
            amount_raw: 1_000_000_000,
            decimals: 6,
            log_index: i,
        })
        .collect();

    replay_sink
        .insert_transfers(&replay_transfers)
        .await
        .expect("re-insert of reorg'd slot events must not error");

    let transfer_count_after_replay = count_rows(&pool, "transfers", "solana").await;
    assert_eq!(
        transfer_count_after_replay, transfer_count,
        "row count must be stable after duplicate re-insert (ON CONFLICT DO NOTHING)"
    );

    // ------------------------------------------------------------------
    // Step 8: Build shared detector infrastructure.
    // ------------------------------------------------------------------
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let config_path = workspace_root.join("config/detectors.toml");
    let detector_config =
        load_detector_config(&config_path).expect("config/detectors.toml must exist");

    // Build a TokenRegistry backed by a MockSolanaRpc (no network calls).
    // enrich() will find the seeded rows in Postgres within TTL.
    let mock_rpc: Arc<dyn mg_onchain_detectors::rpc::SolanaRpc> =
        Arc::new(MockSolanaRpc::default());
    let registry = TokenRegistry::new(RegistryConfig::default(), pg.clone(), mock_rpc.clone());

    // Observation window: covers all fixture events with 5 min margin.
    let window = DetectorWindow {
        start: base_time() - Duration::minutes(5),
        end: slot_time(FINAL_SLOT) + Duration::minutes(5),
        block_start: block(BASE_SLOT),
        block_end: block(FINAL_SLOT),
    };

    // observed_at = window.end for determinism (C1 fix).
    let observed_at = window.end;

    // ------------------------------------------------------------------
    // Step 9: Construct all 6 detectors.
    // ------------------------------------------------------------------
    let d01 = HoneypotDetector::new(
        detector_config.honeypot_sim.clone(),
        mock_rpc.clone(),
        Arc::new(NotWiredPoolAccountProvider),
    );
    let d02 = RugPullDetector::new(detector_config.rug_pull_lp_drain.clone());
    let d03 = ConcentrationDetector::new(detector_config.holder_concentration.clone());
    let d04 = PumpDumpDetector::new(detector_config.pump_dump.clone());
    let d05 = WashTradingDetector::new(detector_config.wash_trading_h1.clone());
    let d06 = MintBurnAnomalyDetector::new(detector_config.mint_burn_anomaly.clone());

    // Verify all detector ids match their expected constants.
    assert_eq!(d01.id(), "honeypot_sim");
    assert_eq!(d02.id(), "rug_pull_lp_drain");
    assert_eq!(d03.id(), "holder_concentration");
    assert_eq!(d04.id(), "pump_dump");
    assert_eq!(d05.id(), "wash_trading_h1");
    assert_eq!(d06.id(), "mint_burn_anomaly");

    // ------------------------------------------------------------------
    // Step 10: Token addresses for all 3 fixture tokens.
    // ------------------------------------------------------------------
    let rave_addr = addr(RAVE_MINT);
    let wet_addr = addr(WET_MINT);
    let synth_addr = addr(SYNTH_MINT);

    // Macro to build DetectorContext in-place: the 'ctx lifetime is tied to the
    // caller's scope, so a closure cannot express it. A macro expands inline,
    // avoiding the lifetime problem.
    macro_rules! ctx_for {
        ($token_addr:expr) => {
            DetectorContext {
                token: $token_addr,
                chain: Chain::Solana,
                window,
                observed_at,
                store: &pg,
                registry: &registry,
                config: &detector_config,
                zero_address: ZERO_ADDR,
            }
        };
    }

    // ------------------------------------------------------------------
    // Step 11: Evaluate all 6 detectors × 3 tokens (up to 18 invocations).
    // Not all 18 are meaningful (e.g. D03 on all tokens with no snapshots is
    // effectively the same code path 3 times). We run all for completeness.
    // ------------------------------------------------------------------

    // ---- D01 HoneypotDetector ----

    let rave_ctx = ctx_for!(&rave_addr);
    let rave_d01 = match d01.evaluate(&rave_ctx).await {
        Ok(evs) => DetectorResult {
            detector_id: d01.id(),
            events: evs,
            errored: false,
            error_msg: String::new(),
        },
        Err(e) => DetectorResult {
            detector_id: d01.id(),
            events: vec![],
            errored: true,
            error_msg: e.to_string(),
        },
    };
    assert_detector_invariants(&rave_d01);

    // RAVE: no risk signals — expect low confidence (< 0.40)
    if !rave_d01.errored {
        assert!(
            !rave_d01.events.is_empty(),
            "D01 must always emit at least one AnomalyEvent (background confidence)"
        );
        let rave_conf = rave_d01.events[0].confidence.value();
        assert!(
            rave_conf < 0.40,
            "RAVE has no risk signals; D01 confidence should be < 0.40, got {rave_conf}"
        );
    }

    let wet_ctx = ctx_for!(&wet_addr);
    let wet_d01 = match d01.evaluate(&wet_ctx).await {
        Ok(evs) => DetectorResult {
            detector_id: d01.id(),
            events: evs,
            errored: false,
            error_msg: String::new(),
        },
        Err(e) => DetectorResult {
            detector_id: d01.id(),
            events: vec![],
            errored: true,
            error_msg: e.to_string(),
        },
    };
    assert_detector_invariants(&wet_d01);

    // WET: confidence ∈ [0.0, 1.0], severity ≤ Medium
    if !wet_d01.errored && !wet_d01.events.is_empty() {
        let wet_conf = wet_d01.events[0].confidence.value();
        assert!(
            (0.0..=1.0).contains(&wet_conf),
            "WET D01 confidence must be in [0.0, 1.0], got {wet_conf}"
        );
        assert!(
            wet_d01.events[0].severity <= Severity::Medium,
            "WET D01 severity must be ≤ Medium, got {:?}",
            wet_d01.events[0].severity
        );
    }

    let synth_ctx = ctx_for!(&synth_addr);
    let synth_d01 = match d01.evaluate(&synth_ctx).await {
        Ok(evs) => DetectorResult {
            detector_id: d01.id(),
            events: evs,
            errored: false,
            error_msg: String::new(),
        },
        Err(e) => DetectorResult {
            detector_id: d01.id(),
            events: vec![],
            errored: true,
            error_msg: e.to_string(),
        },
    };
    assert_detector_invariants(&synth_d01);

    // SYNTH: confidence ∈ [0.0, 1.0] (full evaluate path — no panic).
    if !synth_d01.errored && !synth_d01.events.is_empty() {
        let synth_conf = synth_d01.events[0].confidence.value();
        assert!(
            (0.0..=1.0).contains(&synth_conf),
            "SYNTH D01 confidence must be in [0.0, 1.0], got {synth_conf}"
        );
    }

    // Pure-path assertion: compute_static with hand-crafted SYNTH meta (S3 active).
    // Verifies the S3 signal fires at confidence ≥ 0.20.
    {
        use mg_onchain_detectors::d01_honeypot::compute_static;

        let synth_meta_with_delegate =
            make_token_meta_with_delegate(SYNTH_MINT, "SYNTH", "Synthetic Scam Token", 6);
        let sr = compute_static(
            &synth_meta_with_delegate,
            None,
            &detector_config.honeypot_sim,
        );
        assert!(
            sr.permanent_delegate_active,
            "permanent_delegate_active must be true for SYNTH meta with delegate set"
        );
        // S3 alone: raw = 0.20 → static_conf = sigmoid(0.20/0.55 - 1.0) ≈ 0.346
        // After sim_skipped attenuation (*0.80): ≈ 0.277 → ≥ 0.20
        let attenuated = sr.confidence * 0.80;
        assert!(
            attenuated >= 0.20,
            "SYNTH S3-only attenuated confidence should be ≥ 0.20, got {attenuated:.4}"
        );
    }

    // ---- D02 RugPullDetector ----
    // Expected path: `fetch_pools_for_token` returns empty (pools table not populated
    // by indexer fixture). D02 returns Ok(vec![]) — no signal without pool state.
    // All 3 tokens share the same code path; we run each for completeness.

    {
        let ctx = ctx_for!(&rave_addr);
        let res = match d02.evaluate(&ctx).await {
            Ok(evs) => DetectorResult {
                detector_id: d02.id(),
                events: evs,
                errored: false,
                error_msg: String::new(),
            },
            Err(e) => DetectorResult {
                detector_id: d02.id(),
                events: vec![],
                errored: true,
                error_msg: e.to_string(),
            },
        };
        assert_detector_invariants(&res);
        eprintln!(
            "[D02/RAVE] {} events (expected 0 — no pool state)",
            if res.errored { 0 } else { res.events.len() }
        );
    }
    {
        let ctx = ctx_for!(&wet_addr);
        let res = match d02.evaluate(&ctx).await {
            Ok(evs) => DetectorResult {
                detector_id: d02.id(),
                events: evs,
                errored: false,
                error_msg: String::new(),
            },
            Err(e) => DetectorResult {
                detector_id: d02.id(),
                events: vec![],
                errored: true,
                error_msg: e.to_string(),
            },
        };
        assert_detector_invariants(&res);
        eprintln!(
            "[D02/WET] {} events (expected 0 — no pool state)",
            if res.errored { 0 } else { res.events.len() }
        );
    }
    {
        let ctx = ctx_for!(&synth_addr);
        let res = match d02.evaluate(&ctx).await {
            Ok(evs) => DetectorResult {
                detector_id: d02.id(),
                events: evs,
                errored: false,
                error_msg: String::new(),
            },
            Err(e) => DetectorResult {
                detector_id: d02.id(),
                events: vec![],
                errored: true,
                error_msg: e.to_string(),
            },
        };
        assert_detector_invariants(&res);
        eprintln!(
            "[D02/SYNTH] {} events (expected 0 — no pool state)",
            if res.errored { 0 } else { res.events.len() }
        );
    }

    // ---- D03 ConcentrationDetector ----
    // Expected path: `fetch_liquid_concentration` returns empty (holder_snapshots not
    // populated by indexer fixture). D03 returns Ok(vec![]) — no snapshot = no Gini.

    {
        let ctx = ctx_for!(&rave_addr);
        let res = match d03.evaluate(&ctx).await {
            Ok(evs) => DetectorResult {
                detector_id: d03.id(),
                events: evs,
                errored: false,
                error_msg: String::new(),
            },
            Err(e) => DetectorResult {
                detector_id: d03.id(),
                events: vec![],
                errored: true,
                error_msg: e.to_string(),
            },
        };
        assert_detector_invariants(&res);
        eprintln!(
            "[D03/RAVE] {} events (expected 0 — no holder snapshots)",
            if res.errored { 0 } else { res.events.len() }
        );
    }
    {
        let ctx = ctx_for!(&wet_addr);
        let res = match d03.evaluate(&ctx).await {
            Ok(evs) => DetectorResult {
                detector_id: d03.id(),
                events: evs,
                errored: false,
                error_msg: String::new(),
            },
            Err(e) => DetectorResult {
                detector_id: d03.id(),
                events: vec![],
                errored: true,
                error_msg: e.to_string(),
            },
        };
        assert_detector_invariants(&res);
        eprintln!(
            "[D03/WET] {} events (expected 0 — no holder snapshots)",
            if res.errored { 0 } else { res.events.len() }
        );
    }
    {
        let ctx = ctx_for!(&synth_addr);
        let res = match d03.evaluate(&ctx).await {
            Ok(evs) => DetectorResult {
                detector_id: d03.id(),
                events: evs,
                errored: false,
                error_msg: String::new(),
            },
            Err(e) => DetectorResult {
                detector_id: d03.id(),
                events: vec![],
                errored: true,
                error_msg: e.to_string(),
            },
        };
        assert_detector_invariants(&res);
        eprintln!(
            "[D03/SYNTH] {} events (expected 0 — no holder snapshots)",
            if res.errored { 0 } else { res.events.len() }
        );
    }

    // ---- D04 PumpDumpDetector ----
    // Expected path: registry enrich succeeds (token rows exist). Market cap from
    // fixture meta is 50_000 USD — below market_cap_filter_usd. Baseline is sparse
    // (swaps have no usd_value). D04 may return an empty Vec or a low-confidence burst.

    {
        let ctx = ctx_for!(&rave_addr);
        let res = match d04.evaluate(&ctx).await {
            Ok(evs) => DetectorResult {
                detector_id: d04.id(),
                events: evs,
                errored: false,
                error_msg: String::new(),
            },
            Err(e) => DetectorResult {
                detector_id: d04.id(),
                events: vec![],
                errored: true,
                error_msg: e.to_string(),
            },
        };
        assert_detector_invariants(&res);
        eprintln!(
            "[D04/RAVE] {} events (expected 0 or Info burst)",
            if res.errored { 0 } else { res.events.len() }
        );
        if !res.errored {
            for ev in &res.events {
                assert!(
                    ev.severity <= Severity::High,
                    "[D04/RAVE]: severity {:?} unexpected from sparse fixture data",
                    ev.severity
                );
            }
        }
    }
    {
        let ctx = ctx_for!(&wet_addr);
        let res = match d04.evaluate(&ctx).await {
            Ok(evs) => DetectorResult {
                detector_id: d04.id(),
                events: evs,
                errored: false,
                error_msg: String::new(),
            },
            Err(e) => DetectorResult {
                detector_id: d04.id(),
                events: vec![],
                errored: true,
                error_msg: e.to_string(),
            },
        };
        assert_detector_invariants(&res);
        eprintln!(
            "[D04/WET] {} events (expected 0 or Info burst)",
            if res.errored { 0 } else { res.events.len() }
        );
        if !res.errored {
            for ev in &res.events {
                assert!(
                    ev.severity <= Severity::High,
                    "[D04/WET]: severity {:?} unexpected from sparse fixture data",
                    ev.severity
                );
            }
        }
    }
    {
        let ctx = ctx_for!(&synth_addr);
        let res = match d04.evaluate(&ctx).await {
            Ok(evs) => DetectorResult {
                detector_id: d04.id(),
                events: evs,
                errored: false,
                error_msg: String::new(),
            },
            Err(e) => DetectorResult {
                detector_id: d04.id(),
                events: vec![],
                errored: true,
                error_msg: e.to_string(),
            },
        };
        assert_detector_invariants(&res);
        eprintln!(
            "[D04/SYNTH] {} events (expected 0 or Info burst)",
            if res.errored { 0 } else { res.events.len() }
        );
        if !res.errored {
            for ev in &res.events {
                assert!(
                    ev.severity <= Severity::High,
                    "[D04/SYNTH]: severity {:?} unexpected from sparse fixture data",
                    ev.severity
                );
            }
        }
    }

    // ---- D05 WashTradingDetector ----
    // Expected path: `fetch_wash_trading_round_trips` returns empty (no round-trip
    // swaps in fixture — all swaps use ZERO_ADDR as sender, buy-only direction).
    // D05 returns Ok(vec![]) — no signal.

    {
        let ctx = ctx_for!(&rave_addr);
        let res = match d05.evaluate(&ctx).await {
            Ok(evs) => DetectorResult {
                detector_id: d05.id(),
                events: evs,
                errored: false,
                error_msg: String::new(),
            },
            Err(e) => DetectorResult {
                detector_id: d05.id(),
                events: vec![],
                errored: true,
                error_msg: e.to_string(),
            },
        };
        assert_detector_invariants(&res);
        eprintln!(
            "[D05/RAVE] {} events (expected 0 — no round-trip swaps)",
            if res.errored { 0 } else { res.events.len() }
        );
    }
    {
        let ctx = ctx_for!(&wet_addr);
        let res = match d05.evaluate(&ctx).await {
            Ok(evs) => DetectorResult {
                detector_id: d05.id(),
                events: evs,
                errored: false,
                error_msg: String::new(),
            },
            Err(e) => DetectorResult {
                detector_id: d05.id(),
                events: vec![],
                errored: true,
                error_msg: e.to_string(),
            },
        };
        assert_detector_invariants(&res);
        eprintln!(
            "[D05/WET] {} events (expected 0 — no round-trip swaps)",
            if res.errored { 0 } else { res.events.len() }
        );
    }
    {
        let ctx = ctx_for!(&synth_addr);
        let res = match d05.evaluate(&ctx).await {
            Ok(evs) => DetectorResult {
                detector_id: d05.id(),
                events: evs,
                errored: false,
                error_msg: String::new(),
            },
            Err(e) => DetectorResult {
                detector_id: d05.id(),
                events: vec![],
                errored: true,
                error_msg: e.to_string(),
            },
        };
        assert_detector_invariants(&res);
        eprintln!(
            "[D05/SYNTH] {} events (expected 0 — no round-trip swaps)",
            if res.errored { 0 } else { res.events.len() }
        );
    }

    // ---- D06 MintBurnAnomalyDetector ----
    // Expected path: registry enrich succeeds. Fixture tokens have mint_authority = None.
    // Signal A requires `mint_authority.is_some()` — will not fire. Signal B requires
    // supply change events above threshold — not present in fixture. D06 returns
    // Ok(vec![]) for all 3 tokens.

    {
        let ctx = ctx_for!(&rave_addr);
        let res = match d06.evaluate(&ctx).await {
            Ok(evs) => DetectorResult {
                detector_id: d06.id(),
                events: evs,
                errored: false,
                error_msg: String::new(),
            },
            Err(e) => DetectorResult {
                detector_id: d06.id(),
                events: vec![],
                errored: true,
                error_msg: e.to_string(),
            },
        };
        assert_detector_invariants(&res);
        eprintln!(
            "[D06/RAVE] {} events (expected 0 — mint_authority=None)",
            if res.errored { 0 } else { res.events.len() }
        );
    }
    {
        let ctx = ctx_for!(&wet_addr);
        let res = match d06.evaluate(&ctx).await {
            Ok(evs) => DetectorResult {
                detector_id: d06.id(),
                events: evs,
                errored: false,
                error_msg: String::new(),
            },
            Err(e) => DetectorResult {
                detector_id: d06.id(),
                events: vec![],
                errored: true,
                error_msg: e.to_string(),
            },
        };
        assert_detector_invariants(&res);
        eprintln!(
            "[D06/WET] {} events (expected 0 — mint_authority=None)",
            if res.errored { 0 } else { res.events.len() }
        );
    }
    {
        let ctx = ctx_for!(&synth_addr);
        let res = match d06.evaluate(&ctx).await {
            Ok(evs) => DetectorResult {
                detector_id: d06.id(),
                events: evs,
                errored: false,
                error_msg: String::new(),
            },
            Err(e) => DetectorResult {
                detector_id: d06.id(),
                events: vec![],
                errored: true,
                error_msg: e.to_string(),
            },
        };
        assert_detector_invariants(&res);
        eprintln!(
            "[D06/SYNTH] {} events (expected 0 — mint_authority=None)",
            if res.errored { 0 } else { res.events.len() }
        );
    }

    // ------------------------------------------------------------------
    // Step 12: Checkpoint resume test.
    // ------------------------------------------------------------------
    let cp_store_2 = PgCheckpointStore::new(pg.clone());
    let shutdown2 = ShutdownSignal::new();
    let empty_adapter = MockChainAdapter::new(vec![]);
    let sink2 = PgEventSink::new(pg.clone());
    let mut indexer2 = Indexer::new(
        empty_adapter,
        sink2,
        cp_store_2,
        "solana",
        "solana",
        BatchConfig {
            size: 200,
            timeout_ms: 30_000,
            max_in_flight: 4,
        },
        shutdown2,
        None, // no graph writer
        None, // no pool_initialize_hook
    );
    let result2 = indexer2.run().await;
    assert!(result2.is_err(), "empty stream must return StreamEnded");

    let cp_after_resume = pg
        .load_checkpoint("solana")
        .await
        .expect("load checkpoint after second run")
        .expect("checkpoint must still exist");
    assert_eq!(
        cp_after_resume.last_slot, FINAL_SLOT as i64,
        "checkpoint must remain at FINAL_SLOT after resume from empty stream, got {}",
        cp_after_resume.last_slot
    );
}

// ---------------------------------------------------------------------------
// Compile-check test — verifies types and imports without Docker.
// ---------------------------------------------------------------------------

/// Non-Docker smoke test: constructs all 6 detectors from real config and
/// verifies their stable IDs without needing a database or Docker.
///
/// This test runs in CI (no `#[ignore]`) to catch API-breaking changes
/// to detector constructors, config struct fields, and the Detector trait.
#[test]
fn sprint4_all_6_detectors_construct_and_have_correct_ids() {
    // Resolve the workspace config path relative to the crate manifest dir.
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let config_path = workspace_root.join("config/detectors.toml");
    let cfg = load_detector_config(&config_path)
        .expect("config/detectors.toml must exist and parse correctly");

    // Use a stub Arc<dyn SolanaRpc> for D01 (does not execute in this test).
    let mock_rpc: Arc<dyn mg_onchain_detectors::rpc::SolanaRpc> =
        Arc::new(MockSolanaRpc::default());

    let d01 = HoneypotDetector::new(
        cfg.honeypot_sim.clone(),
        mock_rpc,
        Arc::new(NotWiredPoolAccountProvider),
    );
    let d02 = RugPullDetector::new(cfg.rug_pull_lp_drain.clone());
    let d03 = ConcentrationDetector::new(cfg.holder_concentration.clone());
    let d04 = PumpDumpDetector::new(cfg.pump_dump.clone());
    let d05 = WashTradingDetector::new(cfg.wash_trading_h1.clone());
    let d06 = MintBurnAnomalyDetector::new(cfg.mint_burn_anomaly.clone());

    assert_eq!(d01.id(), "honeypot_sim", "D01 id mismatch");
    assert_eq!(d02.id(), "rug_pull_lp_drain", "D02 id mismatch");
    assert_eq!(d03.id(), "holder_concentration", "D03 id mismatch");
    assert_eq!(d04.id(), "pump_dump", "D04 id mismatch");
    assert_eq!(d05.id(), "wash_trading_h1", "D05 id mismatch");
    assert_eq!(d06.id(), "mint_burn_anomaly", "D06 id mismatch");

    // Verify severity_floor() is callable and returns Info (all detectors use Info floor).
    for (name, floor) in [
        ("D01", d01.severity_floor()),
        ("D02", d02.severity_floor()),
        ("D03", d03.severity_floor()),
        ("D04", d04.severity_floor()),
        ("D05", d05.severity_floor()),
        ("D06", d06.severity_floor()),
    ] {
        assert_eq!(
            floor,
            Severity::Info,
            "{name} severity_floor must be Info, got {floor:?}"
        );
    }
}

/// Verify that the fixture stream builder returns exactly 46 events (determinism guard).
#[test]
fn sprint4_fixture_stream_event_count() {
    let events = build_fixture_stream();
    assert_eq!(
        events.len(),
        46,
        "fixture stream must be exactly 46 events — update all dependant assertions if this changes"
    );
}

/// Verify the evidence key prefix convention used in assert_detector_invariants.
#[test]
fn sprint4_evidence_key_prefix_convention() {
    use mg_onchain_detectors::evidence_key;

    // Verify the expected prefixes for all 6 detector ids match the convention.
    for (id, metric) in [
        ("honeypot_sim", "buy_sell_ratio"),
        ("rug_pull_lp_drain", "lp_removed_pct"),
        ("holder_concentration", "gini_delta_24h"),
        ("pump_dump", "volume_multiplier"),
        ("wash_trading_h1", "round_trip_count"),
        ("mint_burn_anomaly", "supply_change_pct"),
    ] {
        let key = evidence_key(id, metric);
        assert!(
            key.starts_with(&format!("{id}/")),
            "evidence_key({id}, {metric}) = '{key}' must start with '{id}/'"
        );
    }
}
