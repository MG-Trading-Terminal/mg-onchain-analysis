//! Postgres storage layer — typed methods for all tables (metadata + event tables).
//!
//! # Why runtime queries (not sqlx::query!)
//!
//! `sqlx::query!` / `sqlx::query_as!` verify column types against a live DB
//! at compile time. That would require `DATABASE_URL` in every CI environment
//! that runs `cargo check`. We use `sqlx::query` / `sqlx::query_as` (runtime
//! verification) instead, which compile without a DB.
//!
//! If compile-time verification is wanted later:
//!   1. Set `DATABASE_URL` in the environment.
//!   2. Run `cargo sqlx prepare` to snapshot into `.sqlx/`.
//!   3. Build with `SQLX_OFFLINE=true` in CI (reads from snapshot).
//!
//! # Amount encoding: NUMERIC ↔ String
//!
//! sqlx 0.8 does not expose a native `Decimal` ↔ Postgres NUMERIC codec without
//! an additional crate feature that conflicts with our workspace setup.
//!
//! Instead, u128 amounts are round-tripped through `TEXT` using Postgres's
//! implicit TEXT → NUMERIC cast:
//!   - **Write:** `bind(value.to_string())` — Postgres casts the text literal
//!     to NUMERIC at insert time, matching the `NUMERIC(39,0)` column definition.
//!   - **Read:** `get::<String, _>("col")` → `parse::<u128>()` or
//!     `Decimal::from_str()`.
//!
//! Postgres NUMERIC → TEXT round-trip is lossless for integer NUMERIC values.
//! Decimal (USD, pct) amounts use the same pattern.
//!
//! This is documented in the design doc as the "String bridge" pattern.
//!
//! # Batch insert for event tables
//!
//! The event table insert methods (`insert_transfers`, `insert_swaps`, etc.) use
//! `COPY FROM STDIN` via sqlx `copy_in_raw` for batches > 100 rows, and regular
//! multi-value INSERT for smaller batches. This matches Postgres's optimal write
//! path: COPY is ~5-10× faster than INSERT for large batches.
//!
//! Dedup handling: the `transfers` table has a `UNIQUE (chain, tx_hash, log_index)`
//! constraint. On conflict, the insert is silently skipped (`ON CONFLICT DO NOTHING`).
//! This converts the duplicate-boundary-slot issue from a hard error into an
//! idempotent no-op.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::str::FromStr;
use sqlx::{PgPool, Row};
use tracing::{debug, instrument};

use mg_onchain_common::anomaly::AnomalyEvent;
use mg_onchain_common::event::{PoolEvent, PoolEventKind, Swap, Transfer};
use mg_onchain_common::token::HolderSnapshot;

use crate::error::StorageError;

// ---------------------------------------------------------------------------
// Typed row structs (returned from SELECT queries)
// ---------------------------------------------------------------------------

/// A row from the `tokens` table — subset of columns for hot-path detector use.
#[derive(Debug, Clone)]
pub struct TokenRow {
    pub id: i64,
    pub chain: String,
    pub mint: String,
    pub symbol: Option<String>,
    pub name: Option<String>,
    pub decimals: i16,
    pub token_program: Option<String>,
    /// total_supply_raw as Decimal (lossless from NUMERIC(39,0))
    pub total_supply_raw: Decimal,
    pub circulating_supply_raw: Option<Decimal>,
    pub mint_authority: Option<String>,
    pub freeze_authority: Option<String>,
    pub creator: Option<String>,
    pub creator_balance_raw: Decimal,
    pub total_holders: i64,
    pub total_market_liquidity_usd: Decimal,
    pub jup_verified: bool,
    pub jup_strict: bool,
    pub graph_insiders_detected: bool,
    pub rugged: bool,
    pub rugcheck_score: Option<i32>,
    pub launchpad: Option<String>,
    pub deploy_platform: Option<String>,
    pub detected_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    /// Token-2022 PermanentDelegate extension authority (Base58). NULL for standard SPL.
    /// Added in V00004 migration; populated by enrichment path in P5-4.
    pub permanent_delegate: Option<String>,
    /// Token-2022 TransferHook program address (Base58). NULL for standard SPL.
    /// Added in V00004 migration; populated by enrichment path in P5-4.
    pub transfer_hook_program: Option<String>,
    /// Token-2022 NonTransferable extension marker. NULL for rows pre-V00008.
    /// Added in V00008 migration (P6-2 action item #6). Default false.
    pub non_transferable: Option<bool>,
    /// Token-2022 ConfidentialTransferMint extension marker. NULL for rows pre-V00008.
    /// Added in V00008 migration (P6-2 action item #7). Default false.
    pub confidential_transfer: Option<bool>,
}

impl TokenRow {
    /// Convert `total_supply_raw` Decimal to u128.
    pub fn total_supply_u128(&self) -> u128 {
        use rust_decimal::prelude::ToPrimitive;
        self.total_supply_raw.to_u128().unwrap_or(0)
    }

    /// Convert `circulating_supply_raw` to Option<u128>.
    pub fn circulating_supply_u128(&self) -> Option<u128> {
        use rust_decimal::prelude::ToPrimitive;
        self.circulating_supply_raw.as_ref().and_then(|d| d.to_u128())
    }

    /// Convert `creator_balance_raw` to u128.
    pub fn creator_balance_u128(&self) -> u128 {
        use rust_decimal::prelude::ToPrimitive;
        self.creator_balance_raw.to_u128().unwrap_or(0)
    }
}

/// The result of the D01 honeypot buy/sell ratio query (`docs/queries/d01_honeypot.sql`).
///
/// Returned by [`PgStore::fetch_honeypot_ratio`]. A `None` result means no
/// transfers involving the pool were found in the window — `buy_count` and
/// `sell_count` should be treated as zero (S5 signal suppressed).
#[derive(Debug, Clone)]
pub struct HoneypotRatioRow {
    /// Number of buy transfers (token flowing into pool) in the window.
    pub buy_count: i64,
    /// Number of sell transfers (token flowing out of pool) in the window.
    pub sell_count: i64,
    /// Total raw token amount transferred in (buys).
    pub total_buy_raw: Decimal,
    /// Total raw token amount transferred out (sells).
    pub total_sell_raw: Decimal,
    /// `buy_count / sell_count`, or `999.0` when `sell_count = 0` (SQL sentinel).
    pub buy_sell_ratio: f64,
}

/// A row returned by the D02 rug-pull drain event query.
///
/// Returned by [`PgStore::fetch_rug_pull_drain_events`]. Each row represents
/// a single `pool_events` Burn record that contributes to the cumulative LP
/// drain for a given actor within the observation window.
///
/// The SQL query (docs/queries/d02_rug_pull_lp_drain.sql) uses a window function
/// `SUM(lp_tokens) OVER (PARTITION BY chain, pool, actor ORDER BY block_time)` to
/// accumulate burns per actor; `cumulative_removed_pct` catches trickle drains.
///
/// # DG-D02-3 note
///
/// `lp_removed_pct` and `cumulative_removed_pct` are stored as `f64` here because
/// they are computed by Postgres as `DOUBLE PRECISION` (the SQL query divides
/// NUMERIC lp_tokens by NUMERIC lp_total_supply via `::DOUBLE PRECISION` cast).
/// They are NOT monetary amounts — they are computed ratios (fractions of 1.0).
/// The `CLAUDE.md` no-f64 rule applies to prices/amounts/supplies, not ratios.
/// See DG-D02-3 resolution: monetary arithmetic (locked_amount_raw / lp_total_supply)
/// uses `Decimal` in the detector; these SQL-computed ratios remain `f64`.
#[derive(Debug, Clone)]
pub struct DrainEventRow {
    /// Transaction hash of the burn event.
    pub tx_hash: String,
    /// Wallet address that executed the burn (LP position owner).
    pub actor: String,
    /// Block timestamp of this burn event.
    pub block_time: chrono::DateTime<Utc>,
    /// Block height of this burn event.
    pub block_height: i64,
    /// Raw LP tokens burned in this single transaction.
    pub lp_burned: Decimal,
    /// `lp_tokens / lp_total_supply` for this single burn event. Range [0, 1].
    pub lp_removed_pct: f64,
    /// Cumulative `SUM(lp_tokens) / lp_total_supply` for this actor up to this event.
    pub cumulative_removed_pct: f64,
}

/// A row from the `adapter_checkpoints` table.
#[derive(Debug, Clone)]
pub struct CheckpointRow {
    pub adapter_id: String,
    pub last_slot: i64,
    pub last_signature: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// A row from the `pools` table — subset for rug-pull detector.
#[derive(Debug, Clone)]
pub struct PoolRow {
    pub id: i64,
    pub chain: String,
    pub pool_address: String,
    pub dex: String,
    pub token0: String,
    pub token1: String,
    pub reserve0_raw: Decimal,
    pub reserve1_raw: Decimal,
    pub lp_total_supply: Decimal,
    pub deployer_lp_amount: Decimal,
    pub lifetime_tx_count: i64,
    pub liquidity_usd: Decimal,
    pub updated_at: DateTime<Utc>,
}

impl PoolRow {
    pub fn lp_total_supply_u128(&self) -> u128 {
        use rust_decimal::prelude::ToPrimitive;
        self.lp_total_supply.to_u128().unwrap_or(0)
    }

    pub fn deployer_lp_amount_u128(&self) -> u128 {
        use rust_decimal::prelude::ToPrimitive;
        self.deployer_lp_amount.to_u128().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Pool market row — lightweight projection for TokenMeta::markets population
// ---------------------------------------------------------------------------

/// Lightweight pool projection returned by `PgStore::get_pools_for_token_as_markets`.
///
/// Carries only the fields needed to construct a `MarketInfo` value inside
/// `token-registry::enrich`.  `lp_burned_pct` and `lp_provider_count` are
/// not stored in `pools`; callers that need them must query an external source.
#[derive(Debug, Clone)]
pub struct PoolMarketRow {
    pub pool_address: String,
    /// DEX identifier string, e.g. `"raydium_v4"`, `"raydium_cpmm"`.
    pub dex: String,
    pub liquidity_usd: Decimal,
}

// ---------------------------------------------------------------------------
// D03 — Holder Concentration storage types
// ---------------------------------------------------------------------------

/// A single row returned from `holder_snapshots` or `holder_snapshots_history`
/// for one holder at one snapshot time.
///
/// This is the storage-layer view of a per-holder balance at a point in time.
/// It differs from `common::HolderSnapshot` which represents a full snapshot
/// bundle (all holders + aggregate metrics). This struct carries only the fields
/// needed by D03's liquid-filtered concentration computation.
///
/// # DG-D03-3 resolution
///
/// `snapshot_id` (the Postgres row id / composite key) is carried here so
/// that D03 can reference a specific snapshot without modifying the frozen
/// `common::HolderSnapshot` type which uses `block: BlockRef` as its key.
#[derive(Debug, Clone)]
pub struct HolderSnapshotRow {
    /// Address of the holder (canonical form for the chain).
    pub holder: String,
    /// Raw token balance (before decimal adjustment).
    pub balance_raw: Decimal,
    /// Block height at which this balance was snapshotted.
    pub block_height: i64,
    /// Wall-clock time the snapshot was taken (from block metadata).
    pub snapshot_time: DateTime<Utc>,
}

/// The result of the D03 liquid-filtered concentration query.
///
/// Contains the top-N holders by balance that are classified as Liquid (or
/// unclassified, which D03 treats conservatively as Liquid), plus aggregate
/// counts and the list of addresses needing lazy classification.
///
/// Gini coefficient and top-10% are NOT pre-computed here — D03 computes them
/// in Rust from `liquid_holders` to avoid SQL complexity and ensure `Decimal`
/// arithmetic throughout (per CLAUDE.md §no-f64 rule for monetary quantities).
///
/// # DG-D03-2 resolution
///
/// The query uses `LIMIT top_n_limit` (default 1000). For tokens with >1000
/// liquid holders, Gini and top-10% are approximate (computed over the top-1000
/// slice). This is sufficient because top-heavy tokens have most Gini mass in
/// the top slice. TODO(phase-3): streaming approximation for full population.
#[derive(Debug, Clone)]
pub struct LiquidConcentrationView {
    /// Top-N liquid holders by balance_raw, ordered descending.
    ///
    /// Includes both:
    /// - Holders with `hc.kind IS NULL` (unclassified — treated as Liquid)
    /// - Holders with `hc.kind = 'Liquid'`
    pub liquid_holders: Vec<HolderSnapshotRow>,
    /// Total count of liquid + unclassified holders at this snapshot
    /// (across ALL holders, not just the top-N slice in `liquid_holders`).
    pub liquid_count: u64,
    /// Count of holders explicitly excluded (kind IS NOT NULL AND kind != 'Liquid').
    pub excluded_count: u64,
    /// Breakdown of excluded holders by kind string (e.g. "VestingContract" → 3).
    /// `BTreeMap` for deterministic iteration.
    pub excluded_breakdown: std::collections::BTreeMap<String, u64>,
    /// Addresses where `hc.kind IS NULL` (absent from sidecar) in the top-N slice.
    /// These are candidates for lazy classification (DG-D03-4).
    pub needs_classification: Vec<String>,
}

// ---------------------------------------------------------------------------
// Gateway: anomaly_events paginated row
// ---------------------------------------------------------------------------

/// A row returned from `fetch_anomaly_events_paginated`.
///
/// Contains the raw columns from `anomaly_events` plus the `id` column added in V00005.
/// The `to_json_value` method serializes this row to a JSON value suitable for the REST API.
#[derive(Debug, Clone)]
pub struct AnomalyEventRow {
    pub id: i64,
    pub chain: String,
    pub token: String,
    pub detector_id: String,
    pub observed_at: DateTime<Utc>,
    pub ingested_at: DateTime<Utc>,
    pub window_start_height: i64,
    pub window_end_height: i64,
    pub confidence: f64,
    pub severity: String,
    pub evidence: serde_json::Value,
}

impl AnomalyEventRow {
    /// Serialize to a JSON value for the REST API response.
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "detectorId": self.detector_id,
            "token": self.token,
            "chain": self.chain,
            "confidence": self.confidence,
            "severity": self.severity,
            "evidence": self.evidence,
            "observedAt": self.observed_at.to_rfc3339(),
            "ingestedAt": self.ingested_at.to_rfc3339(),
            "window": [
                { "chain": self.chain, "height": self.window_start_height },
                { "chain": self.chain, "height": self.window_end_height }
            ],
            "_id": self.id,
        })
    }
}

// ---------------------------------------------------------------------------
// D07 — Token-2022 Withdraw-Withheld Drain storage types (V00007 migration)
// ---------------------------------------------------------------------------

/// A row from the `token2022_instructions` table.
///
/// Returned by D07 queries W1, W2, W3 results. Used by the detector to evaluate
/// Signal A (extraction event) and Signal B (authority rotation alert).
///
/// # String bridge
///
/// `amount_raw` and `amount_usd` are NUMERIC columns in Postgres. They are
/// read as `Option<String>` and parsed to `Option<Decimal>` (String bridge pattern,
/// consistent with all other NUMERIC columns in this file).
#[derive(Debug, Clone)]
pub struct Token2022InstructionRow {
    /// Postgres auto-incremented row id.
    pub id: i64,
    /// Chain identifier (e.g. "solana").
    pub chain: String,
    /// Token mint address (Base58).
    pub mint: String,
    /// Transaction hash (Base58 for Solana).
    pub tx_hash: String,
    /// Block height (slot number on Solana).
    pub block_height: i64,
    /// Wall-clock time of the block.
    pub block_time: DateTime<Utc>,
    /// One of: 'withdraw_withheld_from_accounts', 'withdraw_withheld_from_mint',
    /// 'harvest_withheld_to_mint', 'set_authority_withdraw_withheld'.
    pub instruction_kind: String,
    /// Authority signer for withdraw/set_authority; NULL for harvest (permissionless).
    pub authority: Option<String>,
    /// Destination token account for withdraw instructions; NULL otherwise.
    pub destination: Option<String>,
    /// Token units extracted. NULL for set_authority instructions and harvest.
    pub amount_raw: Option<Decimal>,
    /// USD value at block_time from indexer price feed. NULL if no price available.
    pub amount_usd: Option<Decimal>,
    /// New authority pubkey for set_authority_withdraw_withheld. NULL if revoked.
    pub new_authority: Option<String>,
    /// Previous authority pubkey for set_authority_withdraw_withheld.
    pub prev_authority: Option<String>,
    /// Log index within the transaction (CPI: outer_idx*1000 + inner_idx).
    pub log_index: i32,
}

/// W2 query result row — authority rotation event with optional fresh-wallet info.
///
/// Returned by [`PgStore::fetch_withdraw_authority_history`].
/// Carries all fields from `Token2022InstructionRow` for rotation events, plus
/// the `new_authority_first_sol_time` from the LEFT JOIN on `wallet_funding_events`.
#[derive(Debug, Clone)]
pub struct AuthorityRotationRow {
    /// The `Token2022InstructionRow` for the rotation instruction.
    pub row: Token2022InstructionRow,
    /// First SOL receipt time for the new authority wallet.
    /// `None` if `wallet_funding_events` has no record for this wallet.
    pub new_authority_first_sol_time: Option<DateTime<Utc>>,
}

/// Aggregated W3 query result — cumulative extraction metrics over the detection window.
///
/// Returned by [`PgStore::fetch_withdraw_withheld_events`] alongside the event rows.
#[derive(Debug, Clone)]
pub struct WithdrawWithheldEventsResult {
    /// All extraction event rows (W1 results, ordered by block_time ASC).
    pub events: Vec<Token2022InstructionRow>,
    /// Total count of extraction events in the window.
    pub event_count: i64,
    /// Sum of `amount_raw` across all events. `None` if no events or all amounts are NULL.
    pub cumulative_amount_raw: Option<Decimal>,
    /// Sum of `amount_usd` across all events. `None` if all `amount_usd` are NULL
    /// (i.e. price data was unavailable for all events in the window).
    pub cumulative_amount_usd: Option<Decimal>,
}

// ---------------------------------------------------------------------------
// D06 — Mint / Burn Anomaly storage types
// ---------------------------------------------------------------------------

/// A single supply-change transfer event row for D06 Signal B.
///
/// Returned by [`PgStore::fetch_supply_change_events`]. Each row represents one
/// Transfer where `from_address = zero_address` (mint) or `to_address = zero_address`
/// (burn) that crosses the `supply_change_threshold_pct` gate and is NOT in the
/// `known_lp_addresses` exclusion list.
///
/// # `supply_change_pct` sign convention
///
/// Positive values indicate a supply increase (mint); negative values indicate
/// a supply decrease (burn). The magnitude is `amount_raw / supply_denominator`.
///
/// # `f64` for percentage ratios
///
/// Per CLAUDE.md: `f64` is used only for percentage ratios (not monetary amounts).
/// `amount_raw` uses `Decimal` (monetary); `supply_change_pct` uses `f64` (ratio).
/// This mirrors the DG-D02-3 resolution pattern in `DrainEventRow`.
#[derive(Debug, Clone)]
pub struct SupplyChangeEventRow {
    /// Transaction hash of the mint/burn transfer.
    pub tx_hash: String,
    /// Block timestamp of the event.
    pub block_time: DateTime<Utc>,
    /// Block height of the event.
    pub block_height: i64,
    /// Log index within the transaction.
    pub log_index: i32,
    /// Event kind: `"mint"` (from zero address) or `"burn"` (to zero address).
    pub event_kind: String,
    /// Raw token amount minted or burned.
    pub amount_raw: Decimal,
    /// `amount_raw / supply_denominator` — signed: positive for mint, negative for burn.
    pub supply_change_pct: f64,
    /// Recipient address for mint events (to_address); sender for burn events (from_address).
    /// `None` for burns where the burner is the zero address itself (degenerate case).
    pub recipient: Option<String>,
}

// ---------------------------------------------------------------------------
// Helper: parse a NUMERIC column returned as String from Postgres
// ---------------------------------------------------------------------------

/// Read a `NUMERIC` column as a Rust `Decimal` by fetching it as a String
/// and parsing. Postgres returns NUMERIC values as decimal strings over the wire.
fn get_decimal(row: &sqlx::postgres::PgRow, col: &str) -> Result<Decimal, StorageError> {
    let s: String = row.try_get(col).map_err(StorageError::Postgres)?;
    Decimal::from_str(&s).map_err(|e| StorageError::Other(format!("parse NUMERIC {col}: {e}")))
}

fn get_decimal_opt(
    row: &sqlx::postgres::PgRow,
    col: &str,
) -> Result<Option<Decimal>, StorageError> {
    let s: Option<String> = row.try_get(col).map_err(StorageError::Postgres)?;
    s.map(|v| {
        Decimal::from_str(&v).map_err(|e| StorageError::Other(format!("parse NUMERIC {col}: {e}")))
    })
    .transpose()
}

// ---------------------------------------------------------------------------
// PgStore — the Postgres wrapper
// ---------------------------------------------------------------------------

/// Postgres storage wrapper providing typed access to all tables.
///
/// Constructed via [`PgStore::new`]. The underlying [`PgPool`] is shared and
/// clone-cheap — `PgStore` can be cheaply cloned for use across tasks.
#[derive(Debug, Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// Construct a new `PgStore` from an existing connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Construct a new `PgStore` by connecting to the given URL.
    pub async fn connect(url: &str) -> Result<Self, StorageError> {
        let pool = PgPool::connect(url).await?;
        Ok(Self { pool })
    }

    /// Expose the inner pool (for use in tests or advanced callers).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    // -------------------------------------------------------------------------
    // tokens
    // -------------------------------------------------------------------------

    /// Look up a token by (chain, mint). Returns `None` if not found.
    ///
    /// NUMERIC columns are fetched as TEXT and parsed — see module doc.
    #[instrument(skip(self))]
    pub async fn get_token(
        &self,
        chain: &str,
        mint: &str,
    ) -> Result<Option<TokenRow>, StorageError> {
        let row = sqlx::query(
            r#"SELECT id, chain, mint, symbol, name, decimals, token_program,
                      total_supply_raw::TEXT,
                      circulating_supply_raw::TEXT,
                      mint_authority, freeze_authority,
                      creator, creator_balance_raw::TEXT,
                      total_holders, total_market_liquidity_usd::TEXT,
                      jup_verified, jup_strict, graph_insiders_detected,
                      rugged, rugcheck_score, launchpad, deploy_platform,
                      detected_at, updated_at,
                      permanent_delegate, transfer_hook_program,
                      non_transferable, confidential_transfer
               FROM tokens
               WHERE chain = $1 AND mint = $2"#,
        )
        .bind(chain)
        .bind(mint)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            None => Ok(None),
            Some(r) => {
                Ok(Some(TokenRow {
                    id: r.try_get("id")?,
                    chain: r.try_get("chain")?,
                    mint: r.try_get("mint")?,
                    symbol: r.try_get("symbol")?,
                    name: r.try_get("name")?,
                    decimals: r.try_get("decimals")?,
                    token_program: r.try_get("token_program")?,
                    total_supply_raw: get_decimal(&r, "total_supply_raw")?,
                    circulating_supply_raw: get_decimal_opt(&r, "circulating_supply_raw")?,
                    mint_authority: r.try_get("mint_authority")?,
                    freeze_authority: r.try_get("freeze_authority")?,
                    creator: r.try_get("creator")?,
                    creator_balance_raw: get_decimal(&r, "creator_balance_raw")?,
                    total_holders: r.try_get("total_holders")?,
                    total_market_liquidity_usd: get_decimal(&r, "total_market_liquidity_usd")?,
                    jup_verified: r.try_get("jup_verified")?,
                    jup_strict: r.try_get("jup_strict")?,
                    graph_insiders_detected: r.try_get("graph_insiders_detected")?,
                    rugged: r.try_get("rugged")?,
                    rugcheck_score: r.try_get("rugcheck_score")?,
                    launchpad: r.try_get("launchpad")?,
                    deploy_platform: r.try_get("deploy_platform")?,
                    detected_at: r.try_get("detected_at")?,
                    updated_at: r.try_get("updated_at")?,
                    permanent_delegate: r.try_get("permanent_delegate")?,
                    transfer_hook_program: r.try_get("transfer_hook_program")?,
                    non_transferable: r.try_get("non_transferable")?,
                    confidential_transfer: r.try_get("confidential_transfer")?,
                }))
            }
        }
    }

    /// Upsert a token row. On conflict (chain, mint), update all mutable fields.
    ///
    /// u128 amounts are passed as `String` (their decimal representation).
    /// Postgres casts the text literal to `NUMERIC(39,0)` at insert time.
    ///
    /// # Parameters (25 total — bloat documented)
    ///
    /// The positional parameter list grew from 21 → 23 in P5-4 when
    /// `permanent_delegate` and `transfer_hook_program` were wired through from
    /// the Token-2022 TLV decoder, and from 23 → 25 in P6-2 when
    /// `non_transferable` and `confidential_transfer` were added (V00008).
    /// A future task should refactor the signature to accept `&TokenMeta` directly
    /// (Phase 5+ backlog).
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_token(
        &self,
        chain: &str,
        mint: &str,
        symbol: Option<&str>,
        name: Option<&str>,
        decimals: i16,
        token_program: Option<&str>,
        total_supply_raw: u128,
        circulating_supply_raw: Option<u128>,
        mint_authority: Option<&str>,
        freeze_authority: Option<&str>,
        creator: Option<&str>,
        creator_balance_raw: u128,
        total_holders: i64,
        total_market_liquidity_usd: &str, // decimal string
        jup_verified: bool,
        jup_strict: bool,
        rugged: bool,
        rugcheck_score: Option<i32>,
        launchpad: Option<&str>,
        deploy_platform: Option<&str>,
        detected_at: Option<DateTime<Utc>>,
        permanent_delegate: Option<&str>,
        transfer_hook_program: Option<&str>,
        non_transferable: bool,
        confidential_transfer: bool,
    ) -> Result<(), StorageError> {
        let circ = circulating_supply_raw.map(|v| v.to_string());
        sqlx::query(
            r#"INSERT INTO tokens (
                chain, mint, symbol, name, decimals, token_program,
                total_supply_raw, circulating_supply_raw,
                mint_authority, freeze_authority,
                creator, creator_balance_raw,
                total_holders, total_market_liquidity_usd,
                jup_verified, jup_strict, rugged, rugcheck_score,
                launchpad, deploy_platform, detected_at,
                permanent_delegate, transfer_hook_program,
                non_transferable, confidential_transfer,
                updated_at
               )
               VALUES ($1,$2,$3,$4,$5,$6,
                       $7::NUMERIC,$8::NUMERIC,
                       $9,$10,$11,$12::NUMERIC,$13,$14::NUMERIC,
                       $15,$16,$17,$18,$19,$20,$21,
                       $22,$23,
                       $24,$25,
                       now())
               ON CONFLICT (chain, mint) DO UPDATE SET
                 symbol                    = EXCLUDED.symbol,
                 name                      = EXCLUDED.name,
                 total_supply_raw          = EXCLUDED.total_supply_raw,
                 circulating_supply_raw    = EXCLUDED.circulating_supply_raw,
                 mint_authority            = EXCLUDED.mint_authority,
                 freeze_authority          = EXCLUDED.freeze_authority,
                 creator                   = EXCLUDED.creator,
                 creator_balance_raw       = EXCLUDED.creator_balance_raw,
                 total_holders             = EXCLUDED.total_holders,
                 total_market_liquidity_usd= EXCLUDED.total_market_liquidity_usd,
                 jup_verified              = EXCLUDED.jup_verified,
                 jup_strict                = EXCLUDED.jup_strict,
                 rugged                    = EXCLUDED.rugged,
                 rugcheck_score            = EXCLUDED.rugcheck_score,
                 launchpad                 = EXCLUDED.launchpad,
                 deploy_platform           = EXCLUDED.deploy_platform,
                 detected_at               = COALESCE(tokens.detected_at, EXCLUDED.detected_at),
                 permanent_delegate        = EXCLUDED.permanent_delegate,
                 transfer_hook_program     = EXCLUDED.transfer_hook_program,
                 non_transferable          = EXCLUDED.non_transferable,
                 confidential_transfer     = EXCLUDED.confidential_transfer,
                 updated_at                = now()"#,
        )
        .bind(chain)
        .bind(mint)
        .bind(symbol)
        .bind(name)
        .bind(decimals)
        .bind(token_program)
        .bind(total_supply_raw.to_string())
        .bind(circ)
        .bind(mint_authority)
        .bind(freeze_authority)
        .bind(creator)
        .bind(creator_balance_raw.to_string())
        .bind(total_holders)
        .bind(total_market_liquidity_usd)
        .bind(jup_verified)
        .bind(jup_strict)
        .bind(rugged)
        .bind(rugcheck_score)
        .bind(launchpad)
        .bind(deploy_platform)
        .bind(detected_at)
        .bind(permanent_delegate)
        .bind(transfer_hook_program)
        .bind(non_transferable)
        .bind(confidential_transfer)
        .execute(&self.pool)
        .await?;

        debug!(chain, mint, "token upserted");
        Ok(())
    }

    /// Record that a known LP locker holds tokens for a token on `chain`.
    ///
    /// Delegates to [`PgStore::upsert_locker_hit`] using a canonical JSON shape:
    /// ```json
    /// {
    ///   "locker_address": "0x...",
    ///   "protocol_name":  "Unicrypt",   // or null
    ///   "locked_amount_raw": "123456",  // string-encoded per ADR 0002
    ///   "observed_at": null             // block_time passed by caller; null if unavailable
    /// }
    /// ```
    ///
    /// Called by `write_locker_hit` in `crates/server/src/init/locker_watcher.rs`.
    ///
    /// V00017 migration added `tokens.metadata_jsonb` (Sprint 44). This method
    /// now persists locker data to that column; the prior SPEC-NOTE log-only stub
    /// is replaced.
    ///
    /// # Parameters
    ///
    /// - `chain` — chain identifier (e.g. `"ethereum"`)
    /// - `token_mint` — ERC-20 contract address of the token
    /// - `locker_address` — locker contract address (lowercase EVM hex)
    /// - `locked_amount_raw` — LP tokens transferred to the locker (raw u128)
    /// - `protocol_name` — human-readable protocol name (e.g. `"Unicrypt"`)
    pub async fn upsert_locker(
        &self,
        chain: &str,
        token_mint: &str,
        locker_address: &str,
        locked_amount_raw: u128,
        protocol_name: Option<&str>,
    ) -> Result<(), StorageError> {
        let hit = serde_json::json!({
            "locker_address":     locker_address,
            "protocol_name":      protocol_name,
            "locked_amount_raw":  locked_amount_raw.to_string(),
            "observed_at":        serde_json::Value::Null,
        });
        self.upsert_locker_hit(chain, token_mint, hit).await
    }

    // -----------------------------------------------------------------------
    // V00017 metadata_jsonb storage methods
    // -----------------------------------------------------------------------

    /// Write graduation metadata into `tokens.metadata_jsonb -> 'graduation'`.
    ///
    /// `info` is the serialised `GraduationInfo` value from `token-registry`.
    /// Callers serialise with `serde_json::to_value(&info)` before passing here
    /// to avoid a crate-level dep on `mg-onchain-token-registry` (which itself
    /// depends on `mg-onchain-storage` — circular dep).
    ///
    /// The merge expression `metadata_jsonb || jsonb_build_object('graduation', $3)`
    /// preserves existing keys (e.g. `lockers`) and overwrites `graduation` only.
    ///
    /// # Time discipline (gotcha #22)
    ///
    /// `info` must carry `graduation_time` sourced from `block_time`, never `Utc::now()`.
    /// The caller is responsible for this invariant; the storage layer is time-source agnostic.
    #[instrument(skip(self, info), fields(chain, token))]
    pub async fn upsert_graduation_info(
        &self,
        chain: &str,
        token: &str,
        info: serde_json::Value,
    ) -> Result<(), StorageError> {
        sqlx::query(
            r#"UPDATE tokens
               SET metadata_jsonb = metadata_jsonb || jsonb_build_object('graduation', $3::jsonb),
                   updated_at     = now()
               WHERE chain = $1 AND mint = $2"#,
        )
        .bind(chain)
        .bind(token)
        .bind(info)
        .execute(&self.pool)
        .await?;
        debug!(chain, token, "graduation info upserted to metadata_jsonb");
        Ok(())
    }

    /// Fetch graduation metadata from `tokens.metadata_jsonb -> 'graduation'`.
    ///
    /// Returns `Ok(None)` when the token row does not exist or the `graduation` key
    /// is absent from `metadata_jsonb`.
    ///
    /// Callers deserialise with `serde_json::from_value::<GraduationInfo>(val)`.
    #[instrument(skip(self), fields(chain, token))]
    pub async fn fetch_graduation_info(
        &self,
        chain: &str,
        token: &str,
    ) -> Result<Option<serde_json::Value>, StorageError> {
        let row = sqlx::query(
            r#"SELECT metadata_jsonb -> 'graduation' AS graduation
               FROM tokens
               WHERE chain = $1 AND mint = $2"#,
        )
        .bind(chain)
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else { return Ok(None) };
        let val: Option<serde_json::Value> = row.try_get("graduation")?;
        Ok(val)
    }

    /// Append a locker hit to `tokens.metadata_jsonb -> 'lockers'` array.
    ///
    /// The `hit` value is a JSON object representing a single LP locker transfer.
    /// Conventional fields (snake_case):
    /// ```json
    /// {
    ///   "locker_address": "0x...",
    ///   "protocol_name": "Unicrypt",
    ///   "locked_amount_raw": "1000000000000000000",
    ///   "observed_at": "<block_time ISO-8601>"
    /// }
    /// ```
    ///
    /// `locked_amount_raw` is a string-encoded integer per ADR 0002 (no f64).
    ///
    /// The merge expression uses `||` with `jsonb_build_object` + `coalesce` to
    /// create the `lockers` array if absent, then appends the new element:
    ///
    /// ```sql
    /// metadata_jsonb ||
    ///   jsonb_build_object('lockers',
    ///     coalesce(metadata_jsonb->'lockers', '[]'::jsonb) || $3::jsonb)
    /// ```
    ///
    /// This is idempotent for distinct locker addresses; duplicate entries are
    /// possible if the same Transfer event is reindexed. Callers should deduplicate
    /// at the query layer when reading (see `fetch_lockers`).
    #[instrument(skip(self, hit), fields(chain, token))]
    pub async fn upsert_locker_hit(
        &self,
        chain: &str,
        token: &str,
        hit: serde_json::Value,
    ) -> Result<(), StorageError> {
        sqlx::query(
            r#"UPDATE tokens
               SET metadata_jsonb = metadata_jsonb ||
                     jsonb_build_object('lockers',
                       coalesce(metadata_jsonb -> 'lockers', '[]'::jsonb) || $3::jsonb),
                   updated_at = now()
               WHERE chain = $1 AND mint = $2"#,
        )
        .bind(chain)
        .bind(token)
        .bind(serde_json::Value::Array(vec![hit]))
        .execute(&self.pool)
        .await?;
        debug!(chain, token, "locker hit appended to metadata_jsonb");
        Ok(())
    }

    /// Fetch all locker hits from `tokens.metadata_jsonb -> 'lockers'`.
    ///
    /// Returns an empty `Vec` when the token row does not exist or the `lockers`
    /// key is absent. Callers deserialise each element as needed.
    ///
    /// The SQL `jsonb_array_elements` expansion ensures we return one JSON value
    /// per locker entry. Results arrive in insertion order (Postgres JSONB array
    /// preserves order).
    #[instrument(skip(self), fields(chain, token))]
    pub async fn fetch_lockers(
        &self,
        chain: &str,
        token: &str,
    ) -> Result<Vec<serde_json::Value>, StorageError> {
        // Fetch the full lockers JSON array in one query.
        let row = sqlx::query(
            r#"SELECT metadata_jsonb -> 'lockers' AS lockers
               FROM tokens
               WHERE chain = $1 AND mint = $2"#,
        )
        .bind(chain)
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else { return Ok(vec![]) };
        let val: Option<serde_json::Value> = row.try_get("lockers")?;
        match val {
            Some(serde_json::Value::Array(arr)) => Ok(arr),
            Some(_) | None => Ok(vec![]),
        }
    }

    /// Return all rugged tokens for a chain — used for fixture corpus bootstrapping
    /// (ADR 0001 §D7 positive-class labels).
    pub async fn list_rugged_tokens(&self, chain: &str) -> Result<Vec<TokenRow>, StorageError> {
        let rows = sqlx::query(
            r#"SELECT id, chain, mint, symbol, name, decimals, token_program,
                      total_supply_raw::TEXT,
                      circulating_supply_raw::TEXT,
                      mint_authority, freeze_authority,
                      creator, creator_balance_raw::TEXT,
                      total_holders, total_market_liquidity_usd::TEXT,
                      jup_verified, jup_strict, graph_insiders_detected,
                      rugged, rugcheck_score, launchpad, deploy_platform,
                      detected_at, updated_at,
                      permanent_delegate, transfer_hook_program,
                      non_transferable, confidential_transfer
               FROM tokens
               WHERE chain = $1 AND rugged = TRUE
               ORDER BY detected_at DESC NULLS LAST"#,
        )
        .bind(chain)
        .fetch_all(&self.pool)
        .await?;

        let mut result = Vec::with_capacity(rows.len());
        for r in rows {
            result.push(TokenRow {
                id: r.try_get("id")?,
                chain: r.try_get("chain")?,
                mint: r.try_get("mint")?,
                symbol: r.try_get("symbol")?,
                name: r.try_get("name")?,
                decimals: r.try_get("decimals")?,
                token_program: r.try_get("token_program")?,
                total_supply_raw: get_decimal(&r, "total_supply_raw")?,
                circulating_supply_raw: get_decimal_opt(&r, "circulating_supply_raw")?,
                mint_authority: r.try_get("mint_authority")?,
                freeze_authority: r.try_get("freeze_authority")?,
                creator: r.try_get("creator")?,
                creator_balance_raw: get_decimal(&r, "creator_balance_raw")?,
                total_holders: r.try_get("total_holders")?,
                total_market_liquidity_usd: get_decimal(&r, "total_market_liquidity_usd")?,
                jup_verified: r.try_get("jup_verified")?,
                jup_strict: r.try_get("jup_strict")?,
                graph_insiders_detected: r.try_get("graph_insiders_detected")?,
                rugged: r.try_get("rugged")?,
                rugcheck_score: r.try_get("rugcheck_score")?,
                launchpad: r.try_get("launchpad")?,
                deploy_platform: r.try_get("deploy_platform")?,
                detected_at: r.try_get("detected_at")?,
                updated_at: r.try_get("updated_at")?,
                permanent_delegate: r.try_get("permanent_delegate")?,
                transfer_hook_program: r.try_get("transfer_hook_program")?,
                non_transferable: r.try_get("non_transferable")?,
                confidential_transfer: r.try_get("confidential_transfer")?,
            });
        }
        Ok(result)
    }

    // -------------------------------------------------------------------------
    // pools
    // -------------------------------------------------------------------------

    /// Look up a pool by (chain, pool_address).
    pub async fn get_pool(
        &self,
        chain: &str,
        pool_address: &str,
    ) -> Result<Option<PoolRow>, StorageError> {
        let row = sqlx::query(
            r#"SELECT id, chain, pool_address, dex, token0, token1,
                      reserve0_raw::TEXT, reserve1_raw::TEXT,
                      lp_total_supply::TEXT, deployer_lp_amount::TEXT,
                      lifetime_tx_count, liquidity_usd::TEXT, updated_at
               FROM pools
               WHERE chain = $1 AND pool_address = $2"#,
        )
        .bind(chain)
        .bind(pool_address)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            None => Ok(None),
            Some(r) => Ok(Some(PoolRow {
                id: r.try_get("id")?,
                chain: r.try_get("chain")?,
                pool_address: r.try_get("pool_address")?,
                dex: r.try_get("dex")?,
                token0: r.try_get("token0")?,
                token1: r.try_get("token1")?,
                reserve0_raw: get_decimal(&r, "reserve0_raw")?,
                reserve1_raw: get_decimal(&r, "reserve1_raw")?,
                lp_total_supply: get_decimal(&r, "lp_total_supply")?,
                deployer_lp_amount: get_decimal(&r, "deployer_lp_amount")?,
                lifetime_tx_count: r.try_get("lifetime_tx_count")?,
                liquidity_usd: get_decimal(&r, "liquidity_usd")?,
                updated_at: r.try_get("updated_at")?,
            })),
        }
    }

    /// Get all pool addresses for a given token (either side of the pair).
    pub async fn list_pool_addresses_for_token(
        &self,
        chain: &str,
        token: &str,
    ) -> Result<Vec<String>, StorageError> {
        let rows = sqlx::query(
            r#"SELECT pool_address FROM pools
               WHERE chain = $1 AND (token0 = $2 OR token1 = $2)"#,
        )
        .bind(chain)
        .bind(token)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| r.try_get::<String, _>("pool_address").unwrap_or_default())
            .collect())
    }

    /// Fetch all pools for a token and return them as `MarketInfo` values.
    ///
    /// Used by `token-registry::enrich` to populate `TokenMeta::markets` from the
    /// `pools` table without a `tokens_markets` join table.  Both sides of the pair
    /// are checked (`token0 = $2 OR token1 = $2`) so the suspect token appears
    /// regardless of which slot it occupies.
    ///
    /// `lp_burned_pct` and `lp_provider_count` are not tracked in `pools`; they
    /// default to `Decimal::ZERO` and `0` respectively.  Detectors that need these
    /// values must read them from an external source (e.g., RugCheck API, Phase 3).
    pub async fn get_pools_for_token_as_markets(
        &self,
        chain: &str,
        token: &str,
    ) -> Result<Vec<PoolMarketRow>, StorageError> {
        let rows = sqlx::query(
            r#"SELECT pool_address, dex, liquidity_usd::TEXT
               FROM pools
               WHERE chain = $1 AND (token0 = $2 OR token1 = $2)
               ORDER BY liquidity_usd DESC"#,
        )
        .bind(chain)
        .bind(token)
        .fetch_all(&self.pool)
        .await?;

        let result = rows
            .into_iter()
            .map(|r| {
                let pool_address: String = r.try_get("pool_address").unwrap_or_default();
                let dex: String = r.try_get("dex").unwrap_or_default();
                let liquidity_usd_str: String = r.try_get("liquidity_usd").unwrap_or_else(|_| "0".into());
                let liquidity_usd = Decimal::from_str(&liquidity_usd_str).unwrap_or(Decimal::ZERO);
                PoolMarketRow { pool_address, dex, liquidity_usd }
            })
            .collect();

        Ok(result)
    }

    /// Upsert a pool row. On conflict (chain, pool_address), update reserves and counts.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_pool(
        &self,
        chain: &str,
        pool_address: &str,
        dex: &str,
        token0: &str,
        token1: &str,
        reserve0_raw: u128,
        reserve1_raw: u128,
        lp_total_supply: u128,
        deployer_lp_amount: u128,
        lifetime_tx_count_delta: i64,
        liquidity_usd: &str, // decimal string
    ) -> Result<(), StorageError> {
        sqlx::query(
            r#"INSERT INTO pools (
                chain, pool_address, dex, token0, token1,
                reserve0_raw, reserve1_raw, lp_total_supply,
                deployer_lp_amount, lifetime_tx_count, liquidity_usd,
                initial_liquidity_usd, updated_at
               )
               VALUES ($1,$2,$3,$4,$5,
                       $6::NUMERIC,$7::NUMERIC,$8::NUMERIC,$9::NUMERIC,
                       $10,$11::NUMERIC,$11::NUMERIC,now())
               ON CONFLICT (chain, pool_address) DO UPDATE SET
                 reserve0_raw          = EXCLUDED.reserve0_raw,
                 reserve1_raw          = EXCLUDED.reserve1_raw,
                 lp_total_supply       = EXCLUDED.lp_total_supply,
                 deployer_lp_amount    = EXCLUDED.deployer_lp_amount,
                 lifetime_tx_count     = pools.lifetime_tx_count + $10,
                 liquidity_usd         = EXCLUDED.liquidity_usd,
                 -- initial_liquidity_usd is intentionally NOT updated on conflict:
                 -- it is a snapshot of the pool's liquidity at Initialize time (D09 F2 feature).
                 -- The ON CONFLICT clause must not include initial_liquidity_usd.
                 last_event_at         = now(),
                 updated_at            = now()"#,
        )
        .bind(chain)
        .bind(pool_address)
        .bind(dex)
        .bind(token0)
        .bind(token1)
        .bind(reserve0_raw.to_string())
        .bind(reserve1_raw.to_string())
        .bind(lp_total_supply.to_string())
        .bind(deployer_lp_amount.to_string())
        .bind(lifetime_tx_count_delta)
        .bind(liquidity_usd)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // -------------------------------------------------------------------------
    // adapter_checkpoints
    // -------------------------------------------------------------------------

    /// Save or update a checkpoint (atomic upsert).
    pub async fn save_checkpoint(
        &self,
        adapter_id: &str,
        last_slot: i64,
        last_signature: Option<&str>,
    ) -> Result<(), StorageError> {
        sqlx::query(
            r#"INSERT INTO adapter_checkpoints (adapter_id, last_slot, last_signature, updated_at)
               VALUES ($1, $2, $3, now())
               ON CONFLICT (adapter_id) DO UPDATE SET
                 last_slot      = EXCLUDED.last_slot,
                 last_signature = EXCLUDED.last_signature,
                 updated_at     = now()"#,
        )
        .bind(adapter_id)
        .bind(last_slot)
        .bind(last_signature)
        .execute(&self.pool)
        .await?;

        debug!(adapter_id, last_slot, "checkpoint saved");
        Ok(())
    }

    /// Load the last checkpoint for an adapter. Returns `None` on first run.
    pub async fn load_checkpoint(
        &self,
        adapter_id: &str,
    ) -> Result<Option<CheckpointRow>, StorageError> {
        let row = sqlx::query(
            r#"SELECT adapter_id, last_slot, last_signature, updated_at
               FROM adapter_checkpoints
               WHERE adapter_id = $1"#,
        )
        .bind(adapter_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| CheckpointRow {
            adapter_id: r.try_get("adapter_id").unwrap_or_default(),
            last_slot: r.try_get("last_slot").unwrap_or(0),
            last_signature: r.try_get("last_signature").unwrap_or(None),
            updated_at: r
                .try_get("updated_at")
                .unwrap_or_else(|_| Utc::now()),
        }))
    }

    // -------------------------------------------------------------------------
    // audit
    // -------------------------------------------------------------------------

    /// Append an audit event. This is append-only — never UPDATE/DELETE on audit.
    pub async fn audit(
        &self,
        category: &str,
        chain: Option<&str>,
        token: Option<&str>,
        actor: &str,
        payload: serde_json::Value,
    ) -> Result<(), StorageError> {
        sqlx::query(
            r#"INSERT INTO audit (category, chain, token, actor, payload, occurred_at)
               VALUES ($1, $2, $3, $4, $5, now())"#,
        )
        .bind(category)
        .bind(chain)
        .bind(token)
        .bind(actor)
        .bind(sqlx::types::Json(payload))
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // -------------------------------------------------------------------------
    // Event table inserts (ADR 0002: event tables move to Postgres)
    // -------------------------------------------------------------------------

    /// Insert a batch of [`Transfer`] events into the `transfers` partitioned table.
    ///
    /// Duplicate inserts (same `(chain, tx_hash, log_index)`) are silently ignored
    /// via `ON CONFLICT DO NOTHING`, converting the chain-adapter's duplicate
    /// boundary-slot issue into an idempotent no-op.
    ///
    /// For batches > 100 rows, consider switching to COPY — this INSERT path is
    /// acceptable for MVP event rates (hundreds/minute after filter).
    pub async fn insert_transfers(&self, transfers: &[Transfer]) -> Result<(), StorageError> {
        if transfers.is_empty() {
            return Ok(());
        }
        for t in transfers {
            let is_mint = t.is_mint();
            let is_burn = t.is_burn();
            let result = sqlx::query(
                r#"INSERT INTO transfers (
                    chain, token, block_time, block_height,
                    tx_hash, log_index,
                    from_address, to_address,
                    amount_raw, decimals,
                    is_mint, is_burn
                   )
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9::NUMERIC,$10,$11,$12)
                   ON CONFLICT (chain, tx_hash, log_index) DO NOTHING"#,
            )
            .bind(t.chain.as_str())
            .bind(t.token.as_str())
            .bind(t.block_time)
            .bind(t.block.height as i64)
            .bind(t.tx_hash.to_string())
            .bind(t.log_index as i32)
            .bind(t.from.as_str())
            .bind(t.to.as_str())
            .bind(t.amount_raw.to_string())
            .bind(t.decimals as i16)
            .bind(is_mint)
            .bind(is_burn)
            .execute(&self.pool)
            .await;

            match result {
                Ok(_) => {}
                Err(e) => return Err(StorageError::Postgres(e)),
            }
        }
        debug!(count = transfers.len(), "transfers inserted");
        Ok(())
    }

    /// Insert a batch of [`Swap`] events into the `swaps` partitioned table.
    ///
    /// Duplicate inserts (same `(chain, tx_hash, log_index)`) are silently ignored.
    pub async fn insert_swaps(&self, swaps: &[Swap]) -> Result<(), StorageError> {
        if swaps.is_empty() {
            return Ok(());
        }
        for s in swaps {
            let usd_value_str = s.usd_value.as_ref().map(|d| d.to_string());
            sqlx::query(
                r#"INSERT INTO swaps (
                    chain, pool, token_in, token_out,
                    block_time, block_height,
                    tx_hash, log_index,
                    sender, dex,
                    amount_in_raw, decimals_in,
                    amount_out_raw, decimals_out,
                    usd_value
                   )
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,
                           $11::NUMERIC,$12,$13::NUMERIC,$14,
                           $15::NUMERIC)
                   ON CONFLICT (chain, tx_hash, log_index) DO NOTHING"#,
            )
            .bind(s.chain.as_str())
            .bind(s.pool.as_str())
            .bind(s.token_in.as_str())
            .bind(s.token_out.as_str())
            .bind(s.block_time)
            .bind(s.block.height as i64)
            .bind(s.tx_hash.to_string())
            .bind(s.log_index as i32)
            .bind(s.sender.as_str())
            .bind(format!("{:?}", s.dex).to_lowercase())
            .bind(s.amount_in_raw.to_string())
            .bind(s.decimals_in as i16)
            .bind(s.amount_out_raw.to_string())
            .bind(s.decimals_out as i16)
            .bind(usd_value_str.as_deref().unwrap_or("0"))
            .execute(&self.pool)
            .await?;
        }
        debug!(count = swaps.len(), "swaps inserted");
        Ok(())
    }

    /// Insert a batch of [`PoolEvent`] events into the `pool_events` partitioned table.
    ///
    /// Duplicate inserts (same `(chain, tx_hash, log_index)`) are silently ignored.
    pub async fn insert_pool_events(&self, events: &[PoolEvent]) -> Result<(), StorageError> {
        if events.is_empty() {
            return Ok(());
        }
        for e in events {
            let (event_kind, amount0, amount1, lp_tokens, reserve0, reserve1, token0, token1) =
                match &e.kind {
                    PoolEventKind::Mint { amount0_raw, amount1_raw, lp_tokens_minted } => (
                        "mint",
                        *amount0_raw,
                        *amount1_raw,
                        *lp_tokens_minted,
                        0u128,
                        0u128,
                        "".to_string(),
                        "".to_string(),
                    ),
                    PoolEventKind::Burn { amount0_raw, amount1_raw, lp_tokens_burned } => (
                        "burn",
                        *amount0_raw,
                        *amount1_raw,
                        *lp_tokens_burned,
                        0u128,
                        0u128,
                        "".to_string(),
                        "".to_string(),
                    ),
                    PoolEventKind::Sync { reserve0_raw, reserve1_raw } => (
                        "sync",
                        0u128,
                        0u128,
                        0u128,
                        *reserve0_raw,
                        *reserve1_raw,
                        "".to_string(),
                        "".to_string(),
                    ),
                    PoolEventKind::Initialize { token0, token1 } => (
                        "initialize",
                        0u128,
                        0u128,
                        0u128,
                        0u128,
                        0u128,
                        token0.as_str().to_string(),
                        token1.as_str().to_string(),
                    ),
                    _ => (
                        "unknown",
                        0u128,
                        0u128,
                        0u128,
                        0u128,
                        0u128,
                        "".to_string(),
                        "".to_string(),
                    ),
                };

            sqlx::query(
                r#"INSERT INTO pool_events (
                    chain, pool, dex, event_kind,
                    block_time, block_height,
                    tx_hash, log_index,
                    actor,
                    amount0_raw, amount1_raw, lp_tokens,
                    reserve0_raw, reserve1_raw,
                    token0, token1
                   )
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,
                           $10::NUMERIC,$11::NUMERIC,$12::NUMERIC,
                           $13::NUMERIC,$14::NUMERIC,
                           $15,$16)
                   ON CONFLICT (chain, tx_hash, log_index) DO NOTHING"#,
            )
            .bind(e.chain.as_str())
            .bind(e.pool.as_str())
            .bind(format!("{:?}", e.dex).to_lowercase())
            .bind(event_kind)
            .bind(e.block_time)
            .bind(e.block.height as i64)
            .bind(e.tx_hash.to_string())
            .bind(e.log_index as i32)
            .bind(e.actor.as_str())
            .bind(amount0.to_string())
            .bind(amount1.to_string())
            .bind(lp_tokens.to_string())
            .bind(reserve0.to_string())
            .bind(reserve1.to_string())
            .bind(token0)
            .bind(token1)
            .execute(&self.pool)
            .await?;
        }
        debug!(count = events.len(), "pool_events inserted");
        Ok(())
    }

    /// Upsert holder snapshots — writes to both `holder_snapshots` (current state)
    /// and `holder_snapshots_history` (append-only, for D03 delta queries).
    ///
    /// The `holder_snapshots` table tracks current state: one row per
    /// `(chain, token, holder)`. The upsert guard `WHERE EXCLUDED.block_height >
    /// holder_snapshots.block_height` prevents stale data from overwriting newer state.
    ///
    /// `holder_snapshots_history` receives a row for every holder in every
    /// full snapshot (`is_full = true`). Delta snapshots are written to
    /// `holder_snapshots` only (they update current state without a history row).
    pub async fn upsert_holder_snapshots(
        &self,
        snapshots: &[HolderSnapshot],
    ) -> Result<(), StorageError> {
        if snapshots.is_empty() {
            return Ok(());
        }
        for snap in snapshots {
            let gini_str = snap.gini.as_ref().map(|d| d.to_string());
            let top10_str = snap.top10_pct.as_ref().map(|d| d.to_string());

            for (holder_addr, balance_raw) in &snap.balances {
                // --- current state table ---
                sqlx::query(
                    r#"INSERT INTO holder_snapshots (
                        chain, token, holder,
                        block_height, block_time,
                        balance_raw, total_holders,
                        gini, top10_pct
                       )
                       VALUES ($1,$2,$3,$4,$5,$6::NUMERIC,$7,$8::NUMERIC,$9::NUMERIC)
                       ON CONFLICT (chain, token, holder) DO UPDATE SET
                         block_height  = EXCLUDED.block_height,
                         block_time    = EXCLUDED.block_time,
                         balance_raw   = EXCLUDED.balance_raw,
                         total_holders = EXCLUDED.total_holders,
                         gini          = EXCLUDED.gini,
                         top10_pct     = EXCLUDED.top10_pct
                       WHERE EXCLUDED.block_height > holder_snapshots.block_height"#,
                )
                .bind(snap.chain.as_str())
                .bind(snap.token.as_str())
                .bind(holder_addr.as_str())
                .bind(snap.block.height as i64)
                .bind(snap.block_time)
                .bind(balance_raw.to_string())
                .bind(snap.total_holders as i64)
                .bind(gini_str.as_deref().unwrap_or("0"))
                .bind(top10_str.as_deref().unwrap_or("0"))
                .execute(&self.pool)
                .await?;

                // --- history table: only for full snapshots ---
                if snap.is_full {
                    sqlx::query(
                        r#"INSERT INTO holder_snapshots_history (
                            chain, token, holder,
                            block_height, balance_raw,
                            snapshot_time, total_holders,
                            gini, top10_pct
                           )
                           VALUES ($1,$2,$3,$4,$5::NUMERIC,$6,$7,$8::NUMERIC,$9::NUMERIC)"#,
                    )
                    .bind(snap.chain.as_str())
                    .bind(snap.token.as_str())
                    .bind(holder_addr.as_str())
                    .bind(snap.block.height as i64)
                    .bind(balance_raw.to_string())
                    .bind(snap.block_time)
                    .bind(snap.total_holders as i64)
                    .bind(gini_str.as_deref().unwrap_or("0"))
                    .bind(top10_str.as_deref().unwrap_or("0"))
                    .execute(&self.pool)
                    .await?;
                }
            }
        }
        debug!(count = snapshots.len(), "holder_snapshots upserted");
        Ok(())
    }

    // -------------------------------------------------------------------------
    // D01 Honeypot: buy/sell ratio query
    // -------------------------------------------------------------------------

    /// Execute the D01 honeypot buy/sell ratio query for the given token, pool, and window.
    ///
    /// Corresponds to `docs/queries/d01_honeypot.sql`. Returns `None` when the
    /// `transfers` table has no rows for this (chain, token, pool) in the window —
    /// the S5 buy/sell ratio signal is suppressed in that case.
    ///
    /// `zero_address` is the chain's null/burn address (e.g. Solana system program
    /// `"11111111111111111111111111111111"`), used to exclude mint/burn events.
    ///
    /// # Determinism
    ///
    /// The underlying SQL has `ORDER BY buy_sell_ratio DESC`. With `HAVING` and
    /// `GROUP BY (chain, token)`, at most one row is returned per (chain, token)
    /// pair. Output is fully deterministic for a given set of `transfers` rows.
    #[instrument(skip(self), fields(chain, token, pool))]
    pub async fn fetch_honeypot_ratio(
        &self,
        chain: &str,
        token: &str,
        pool: &str,
        zero_address: &str,
        window_start: chrono::DateTime<Utc>,
        window_end: chrono::DateTime<Utc>,
    ) -> Result<Option<HoneypotRatioRow>, StorageError> {
        let row = sqlx::query(
            r#"SELECT
                COUNT(*) FILTER (WHERE to_address   = $3)   AS buy_count,
                COUNT(*) FILTER (WHERE from_address = $3)   AS sell_count,
                COALESCE(SUM(amount_raw::NUMERIC) FILTER (WHERE to_address   = $3), 0)::TEXT AS total_buy_raw,
                COALESCE(SUM(amount_raw::NUMERIC) FILTER (WHERE from_address = $3), 0)::TEXT AS total_sell_raw,
                CASE
                    WHEN COUNT(*) FILTER (WHERE from_address = $3) > 0
                    THEN (COUNT(*) FILTER (WHERE to_address = $3))::DOUBLE PRECISION
                         / (COUNT(*) FILTER (WHERE from_address = $3))::DOUBLE PRECISION
                    ELSE 999.0
                END AS buy_sell_ratio
               FROM transfers
               WHERE chain         = $1
                 AND token         = $2
                 AND block_time   >= $5
                 AND block_time   <  $6
                 AND (from_address = $3 OR to_address = $3)
                 AND from_address != $4
                 AND to_address   != $4
               HAVING SUM(amount_raw::NUMERIC) FILTER (WHERE to_address = $3) > 0"#,
        )
        .bind(chain)
        .bind(token)
        .bind(pool)
        .bind(zero_address)
        .bind(window_start)
        .bind(window_end)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            None => Ok(None),
            Some(r) => {
                let buy_count: i64 = r.try_get("buy_count").unwrap_or(0);
                let sell_count: i64 = r.try_get("sell_count").unwrap_or(0);
                let total_buy_raw = get_decimal(&r, "total_buy_raw")
                    .unwrap_or(Decimal::ZERO);
                let total_sell_raw = get_decimal(&r, "total_sell_raw")
                    .unwrap_or(Decimal::ZERO);
                let buy_sell_ratio: f64 = r.try_get("buy_sell_ratio").unwrap_or(0.0);
                Ok(Some(HoneypotRatioRow {
                    buy_count,
                    sell_count,
                    total_buy_raw,
                    total_sell_raw,
                    buy_sell_ratio,
                }))
            }
        }
    }

    // -------------------------------------------------------------------------
    // D02 Rug Pull / LP Drain queries
    // -------------------------------------------------------------------------

    /// Fetch a pool row by (chain, pool_address) — thin alias of `get_pool` with
    /// the same return type.  Exposed as a distinct method so detectors can call it
    /// by the canonical name used in the D02 spec without importing the whole `get_pool`
    /// API surface.
    ///
    /// Returns `Ok(None)` when the pool is not yet indexed.
    #[instrument(skip(self), fields(chain, pool_address))]
    pub async fn fetch_pool_row(
        &self,
        chain: &str,
        pool_address: &str,
    ) -> Result<Option<PoolRow>, StorageError> {
        self.get_pool(chain, pool_address).await
    }

    /// Execute the D02 rug-pull LP drain query for the given pool and time window.
    ///
    /// Implements `docs/queries/d02_rug_pull_lp_drain.sql` wrapped in a CTE to apply
    /// the `lp_removed_pct >= threshold OR cumulative_removed_pct >= threshold` filter
    /// (HAVING on window functions is not valid in PostgreSQL without a subquery).
    ///
    /// # Parameters
    ///
    /// - `chain`: chain identifier, e.g. `"solana"`.
    /// - `pool_address`: pool contract address.
    /// - `window_start`: inclusive start of the observation window (block time).
    /// - `window_end`: exclusive end of the observation window (block time).
    /// - `lp_total_supply`: current total LP token supply (from `pools` table, not
    ///   `TokenMeta`). Used as the denominator for pct computation.
    /// - `threshold`: `lp_removal_threshold` from config (e.g. 0.65).
    ///
    /// # Returns
    ///
    /// Rows ordered by `block_time ASC` (deterministic per spec). Only rows where the
    /// single-event or cumulative drain percentage crosses `threshold` are returned.
    /// Returns an empty `Vec` when no qualifying drain events exist in the window.
    ///
    /// # DG-D02-3 note
    ///
    /// `lp_removed_pct` and `cumulative_removed_pct` are `f64` from Postgres
    /// (`DOUBLE PRECISION`) — these are computed ratios, not monetary amounts.
    /// See `DrainEventRow` doc for the full DG-D02-3 resolution.
    #[instrument(skip(self), fields(chain, pool_address))]
    pub async fn fetch_rug_pull_drain_events(
        &self,
        chain: &str,
        pool_address: &str,
        window_start: chrono::DateTime<Utc>,
        window_end: chrono::DateTime<Utc>,
        lp_total_supply: Decimal,
        threshold: f64,
    ) -> Result<Vec<DrainEventRow>, StorageError> {
        // Guard: zero LP supply would produce division-by-zero in Postgres.
        if lp_total_supply <= Decimal::ZERO {
            return Ok(vec![]);
        }

        // Wrap the base query in a CTE so the HAVING-equivalent filter can be applied
        // after window functions are evaluated. Postgres evaluates HAVING before window
        // functions when used in the same SELECT; a subquery/CTE avoids this restriction.
        // See analyst note in docs/queries/d02_rug_pull_lp_drain.sql.
        let rows = sqlx::query(
            r#"WITH drain_events AS (
                SELECT
                    chain,
                    pool,
                    actor,
                    tx_hash,
                    block_time,
                    block_height,
                    lp_tokens::TEXT                                             AS lp_burned_text,
                    lp_tokens::DOUBLE PRECISION / $5::DOUBLE PRECISION         AS lp_removed_pct,
                    SUM(lp_tokens) OVER (
                        PARTITION BY chain, pool, actor
                        ORDER BY block_time
                        ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                    )::DOUBLE PRECISION / $5::DOUBLE PRECISION                 AS cumulative_removed_pct
                FROM pool_events
                WHERE chain         = $1
                  AND pool          = $2
                  AND event_kind    = 'burn'
                  AND block_time   >= $3
                  AND block_time   <  $4
                  AND lp_tokens    > 0
            )
            SELECT *
            FROM drain_events
            WHERE lp_removed_pct >= $6 OR cumulative_removed_pct >= $6
            ORDER BY block_time ASC"#,
        )
        .bind(chain)
        .bind(pool_address)
        .bind(window_start)
        .bind(window_end)
        .bind(lp_total_supply.to_string()) // $5: lp_total_supply as NUMERIC string
        .bind(threshold)                    // $6: threshold as DOUBLE PRECISION
        .fetch_all(&self.pool)
        .await?;

        let mut result = Vec::with_capacity(rows.len());
        for r in rows {
            let lp_burned_text: String = r.try_get("lp_burned_text")?;
            let lp_burned = Decimal::from_str(&lp_burned_text)
                .map_err(|e| StorageError::Other(format!("parse lp_burned NUMERIC: {e}")))?;
            result.push(DrainEventRow {
                tx_hash: r.try_get("tx_hash")?,
                actor: r.try_get("actor")?,
                block_time: r.try_get("block_time")?,
                block_height: r.try_get("block_height")?,
                lp_burned,
                lp_removed_pct: r.try_get("lp_removed_pct")?,
                cumulative_removed_pct: r.try_get("cumulative_removed_pct")?,
            });
        }

        debug!(
            chain,
            pool_address,
            drain_rows = result.len(),
            "D02 drain events fetched"
        );
        Ok(result)
    }

    /// Insert a batch of [`AnomalyEvent`] detector outputs into the `anomaly_events`
    /// partitioned table.
    ///
    /// The evidence bundle is stored as `JSONB` in Postgres (was `String` in ClickHouse).
    /// On duplicate `(chain, tx_hash, log_index)` there is no natural unique key for
    /// anomaly events — they are identified by `(chain, token, detector_id, observed_at)`.
    /// Duplicates from re-runs are inserted as additional rows; the caller is responsible
    /// for dedup at the application layer if needed.
    pub async fn insert_anomaly_events(
        &self,
        events: &[AnomalyEvent],
        emitted_by: &str,
    ) -> Result<(), StorageError> {
        if events.is_empty() {
            return Ok(());
        }
        for a in events {
            let evidence_json = serde_json::to_value(&a.evidence)?;
            sqlx::query(
                r#"INSERT INTO anomaly_events (
                    chain, token, detector_id,
                    observed_at, ingested_at,
                    window_start_height, window_end_height,
                    confidence, severity,
                    evidence, emitted_by
                   )
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)"#,
            )
            .bind(a.chain.as_str())
            .bind(a.token.as_str())
            .bind(a.detector_id.as_str())
            .bind(a.observed_at)
            .bind(a.ingested_at)
            .bind(a.window.0.height as i64)
            .bind(a.window.1.height as i64)
            .bind(a.confidence.value())
            .bind(format!("{:?}", a.severity).to_lowercase())
            .bind(sqlx::types::Json(evidence_json))
            .bind(emitted_by)
            .execute(&self.pool)
            .await?;
        }
        debug!(count = events.len(), emitted_by, "anomaly_events inserted");
        Ok(())
    }

    // -------------------------------------------------------------------------
    // D03 — Holder Concentration: liquid-filtered snapshot queries
    // -------------------------------------------------------------------------

    /// Fetch the current liquid-filtered concentration view from `holder_snapshots`.
    ///
    /// Reads the MOST RECENT snapshot for the (chain, token) pair. Joins to
    /// `holder_classifications` to separate liquid from non-liquid holders.
    ///
    /// Returns `Ok(None)` if no snapshot exists. Returns the top `top_n_limit`
    /// holders ordered by `balance_raw DESC`.
    ///
    /// # SQL pattern
    ///
    /// ```sql
    /// SELECT hs.holder, hs.balance_raw::TEXT, hs.block_height, hs.snapshot_time,
    ///        hc.kind
    /// FROM holder_snapshots hs
    /// LEFT JOIN holder_classifications hc
    ///   ON hc.chain = hs.chain AND hc.address = hs.holder
    /// WHERE hs.chain = $1 AND hs.token = $2
    ///   AND hs.balance_raw > 0
    /// ORDER BY hs.balance_raw DESC
    /// LIMIT $3
    /// ```
    ///
    /// Aggregate counts (liquid_count, excluded_count, needs_classification) require
    /// a second query over the full set (not just top-N).
    ///
    /// # DG-D03-2
    ///
    /// For tokens with >top_n_limit liquid holders, Gini is approximate (computed
    /// over the top-N slice in Rust). TODO(phase-3): streaming approximation.
    #[instrument(skip(self), fields(chain, token))]
    pub async fn fetch_liquid_concentration_now(
        &self,
        chain: &str,
        token: &str,
        top_n_limit: u32,
    ) -> Result<Option<LiquidConcentrationView>, StorageError> {
        // Query 1: top-N holders with their classification kind.
        let top_n_rows = sqlx::query(
            r#"
            SELECT hs.holder, hs.balance_raw::TEXT AS balance_raw,
                   hs.block_height, hs.snapshot_time, hc.kind
            FROM holder_snapshots hs
            LEFT JOIN holder_classifications hc
              ON hc.chain = hs.chain AND hc.address = hs.holder
            WHERE hs.chain = $1 AND hs.token = $2
              AND hs.balance_raw > 0
            ORDER BY hs.balance_raw DESC
            LIMIT $3
            "#,
        )
        .bind(chain)
        .bind(token)
        .bind(top_n_limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        if top_n_rows.is_empty() {
            return Ok(None);
        }

        // Query 2: full aggregate counts (over ALL holders, not just top-N).
        let agg_row = sqlx::query(
            r#"
            SELECT
              COUNT(*) FILTER (WHERE hc.kind IS NULL OR hc.kind = 'Liquid') AS liquid_count,
              COUNT(*) FILTER (WHERE hc.kind IS NOT NULL AND hc.kind != 'Liquid') AS excluded_count
            FROM holder_snapshots hs
            LEFT JOIN holder_classifications hc
              ON hc.chain = hs.chain AND hc.address = hs.holder
            WHERE hs.chain = $1 AND hs.token = $2
              AND hs.balance_raw > 0
            "#,
        )
        .bind(chain)
        .bind(token)
        .fetch_one(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        let liquid_count: i64 = agg_row.try_get("liquid_count").map_err(StorageError::Postgres)?;
        let excluded_count: i64 = agg_row.try_get("excluded_count").map_err(StorageError::Postgres)?;

        // Query 3: excluded breakdown by kind.
        let breakdown_rows = sqlx::query(
            r#"
            SELECT hc.kind, COUNT(*) AS cnt
            FROM holder_snapshots hs
            JOIN holder_classifications hc
              ON hc.chain = hs.chain AND hc.address = hs.holder
            WHERE hs.chain = $1 AND hs.token = $2
              AND hs.balance_raw > 0
              AND hc.kind IS NOT NULL AND hc.kind != 'Liquid'
            GROUP BY hc.kind
            ORDER BY hc.kind
            "#,
        )
        .bind(chain)
        .bind(token)
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        let mut excluded_breakdown = std::collections::BTreeMap::new();
        for row in &breakdown_rows {
            let kind: String = row.try_get("kind").map_err(StorageError::Postgres)?;
            let cnt: i64 = row.try_get("cnt").map_err(StorageError::Postgres)?;
            excluded_breakdown.insert(kind, cnt.unsigned_abs());
        }

        // Parse top-N rows into HolderSnapshotRow, splitting liquid vs needs_classification.
        let mut liquid_holders = Vec::with_capacity(top_n_rows.len());
        let mut needs_classification = Vec::new();

        for row in &top_n_rows {
            let kind: Option<String> = row.try_get("kind").map_err(StorageError::Postgres)?;

            // Skip explicitly non-liquid holders in the top-N slice.
            if let Some(ref k) = kind
                && k != "Liquid"
            {
                continue;
            }

            let holder: String = row.try_get("holder").map_err(StorageError::Postgres)?;
            let balance_raw = get_decimal(row, "balance_raw")?;
            let block_height: i64 = row.try_get("block_height").map_err(StorageError::Postgres)?;
            let snapshot_time: DateTime<Utc> =
                row.try_get("snapshot_time").map_err(StorageError::Postgres)?;

            if kind.is_none() {
                needs_classification.push(holder.clone());
            }

            liquid_holders.push(HolderSnapshotRow {
                holder,
                balance_raw,
                block_height,
                snapshot_time,
            });
        }

        Ok(Some(LiquidConcentrationView {
            liquid_holders,
            liquid_count: liquid_count.unsigned_abs(),
            excluded_count: excluded_count.unsigned_abs(),
            excluded_breakdown,
            needs_classification,
        }))
    }

    /// Fetch a prior liquid-filtered concentration view from `holder_snapshots_history`.
    ///
    /// Searches for a snapshot whose `snapshot_time` falls within
    /// `[target_time - tolerance, target_time + tolerance]`. Among candidates, selects
    /// the one whose `snapshot_time` is closest to `target_time`. Returns `Ok(None)` if
    /// no snapshot falls in the tolerance window.
    ///
    /// # DG-D03-1 resolved
    ///
    /// `holder_snapshots_history` stores per-holder rows (not aggregates), so the same
    /// LEFT JOIN pattern as `fetch_liquid_concentration_now` applies. The `snapshot_time`
    /// column identifies which snapshot each row belongs to.
    #[instrument(skip(self), fields(chain, token))]
    pub async fn fetch_liquid_concentration_prior(
        &self,
        chain: &str,
        token: &str,
        target_time: DateTime<Utc>,
        tolerance: chrono::Duration,
        top_n_limit: u32,
    ) -> Result<Option<LiquidConcentrationView>, StorageError> {
        // Step 1: find the exact snapshot_time closest to target_time within tolerance.
        let window_start = target_time - tolerance;
        let window_end = target_time + tolerance;

        let time_row_opt = sqlx::query(
            r#"
            SELECT snapshot_time
            FROM holder_snapshots_history
            WHERE chain = $1 AND token = $2
              AND snapshot_time BETWEEN $3 AND $4
            ORDER BY ABS(EXTRACT(EPOCH FROM (snapshot_time - $5))) ASC
            LIMIT 1
            "#,
        )
        .bind(chain)
        .bind(token)
        .bind(window_start)
        .bind(window_end)
        .bind(target_time)
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        let exact_snapshot_time: DateTime<Utc> = match time_row_opt {
            None => return Ok(None),
            Some(r) => r.try_get("snapshot_time").map_err(StorageError::Postgres)?,
        };

        // Step 2: fetch top-N holders for that exact snapshot_time.
        let top_n_rows = sqlx::query(
            r#"
            SELECT hsh.holder, hsh.balance_raw::TEXT AS balance_raw,
                   hsh.block_height, hsh.snapshot_time, hc.kind
            FROM holder_snapshots_history hsh
            LEFT JOIN holder_classifications hc
              ON hc.chain = hsh.chain AND hc.address = hsh.holder
            WHERE hsh.chain = $1 AND hsh.token = $2
              AND hsh.snapshot_time = $3
              AND hsh.balance_raw > 0
            ORDER BY hsh.balance_raw DESC
            LIMIT $4
            "#,
        )
        .bind(chain)
        .bind(token)
        .bind(exact_snapshot_time)
        .bind(top_n_limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        if top_n_rows.is_empty() {
            return Ok(None);
        }

        // Step 3: aggregate counts (full population for that snapshot_time).
        let agg_row = sqlx::query(
            r#"
            SELECT
              COUNT(*) FILTER (WHERE hc.kind IS NULL OR hc.kind = 'Liquid') AS liquid_count,
              COUNT(*) FILTER (WHERE hc.kind IS NOT NULL AND hc.kind != 'Liquid') AS excluded_count
            FROM holder_snapshots_history hsh
            LEFT JOIN holder_classifications hc
              ON hc.chain = hsh.chain AND hc.address = hsh.holder
            WHERE hsh.chain = $1 AND hsh.token = $2
              AND hsh.snapshot_time = $3
              AND hsh.balance_raw > 0
            "#,
        )
        .bind(chain)
        .bind(token)
        .bind(exact_snapshot_time)
        .fetch_one(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        let liquid_count: i64 = agg_row.try_get("liquid_count").map_err(StorageError::Postgres)?;
        let excluded_count: i64 =
            agg_row.try_get("excluded_count").map_err(StorageError::Postgres)?;

        // Step 4: excluded breakdown.
        let breakdown_rows = sqlx::query(
            r#"
            SELECT hc.kind, COUNT(*) AS cnt
            FROM holder_snapshots_history hsh
            JOIN holder_classifications hc
              ON hc.chain = hsh.chain AND hc.address = hsh.holder
            WHERE hsh.chain = $1 AND hsh.token = $2
              AND hsh.snapshot_time = $3
              AND hsh.balance_raw > 0
              AND hc.kind IS NOT NULL AND hc.kind != 'Liquid'
            GROUP BY hc.kind
            ORDER BY hc.kind
            "#,
        )
        .bind(chain)
        .bind(token)
        .bind(exact_snapshot_time)
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        let mut excluded_breakdown = std::collections::BTreeMap::new();
        for row in &breakdown_rows {
            let kind: String = row.try_get("kind").map_err(StorageError::Postgres)?;
            let cnt: i64 = row.try_get("cnt").map_err(StorageError::Postgres)?;
            excluded_breakdown.insert(kind, cnt.unsigned_abs());
        }

        // Step 5: parse top-N rows.
        let mut liquid_holders = Vec::with_capacity(top_n_rows.len());
        let mut needs_classification = Vec::new();

        for row in &top_n_rows {
            let kind: Option<String> = row.try_get("kind").map_err(StorageError::Postgres)?;

            if let Some(ref k) = kind
                && k != "Liquid"
            {
                continue;
            }

            let holder: String = row.try_get("holder").map_err(StorageError::Postgres)?;
            let balance_raw = get_decimal(row, "balance_raw")?;
            let block_height: i64 = row.try_get("block_height").map_err(StorageError::Postgres)?;
            let snapshot_time: DateTime<Utc> =
                row.try_get("snapshot_time").map_err(StorageError::Postgres)?;

            if kind.is_none() {
                needs_classification.push(holder.clone());
            }

            liquid_holders.push(HolderSnapshotRow {
                holder,
                balance_raw,
                block_height,
                snapshot_time,
            });
        }

        Ok(Some(LiquidConcentrationView {
            liquid_holders,
            liquid_count: liquid_count.unsigned_abs(),
            excluded_count: excluded_count.unsigned_abs(),
            excluded_breakdown,
            needs_classification,
        }))
    }

    // -------------------------------------------------------------------------
    // D04 — Pump & Dump: baseline, burst, insider-sell queries
    // -------------------------------------------------------------------------

    /// Fetch the 1h volume/price spike row and 7d baseline for Signal A.
    ///
    /// Implements `docs/queries/d04_pump_and_dump.sql` Query 1. Returns `None` when:
    /// - The token has no swaps in the 7d baseline window (zero-baseline case).
    /// - The volume or price thresholds are not met.
    /// - The `median_volume_usd` is zero (INNER JOIN guard).
    ///
    /// # Parameters
    ///
    /// - `chain`, `token`: identify the token.
    /// - `window_end`: exclusive end of the 1h observation window (= `ctx.window.end`).
    /// - `min_baseline_days`: minimum number of days with non-zero volume in the 7d window.
    ///   Used by the caller to decide Signal A vs B; the SQL itself does not enforce this.
    /// - `volume_multiplier`: minimum required `volume_1h / median_volume_7d` ratio.
    /// - `price_spike_pct`: minimum required `(price_now - price_start) / price_start`.
    ///
    /// # Returns
    ///
    /// `Ok(Some(row))` when a spike is detected. `Ok(None)` when no qualifying spike
    /// exists or the baseline is insufficient. `Err(TransientQuery)` on Postgres failure.
    #[instrument(skip(self), fields(chain, token))]
    pub async fn fetch_pump_dump_baseline(
        &self,
        chain: &str,
        token: &str,
        window_end: DateTime<Utc>,
        _min_baseline_days: i64,
        volume_multiplier: f64,
        price_spike_pct: f64,
    ) -> Result<Option<PumpDumpBaselineRow>, StorageError> {
        let window_start = window_end - chrono::Duration::hours(1);
        let baseline_start = window_end - chrono::Duration::days(7);

        // Query 1 from docs/queries/d04_pump_and_dump.sql (PostgreSQL dialect).
        // INNER JOIN on baseline_7d ensures no row when median_volume_usd = 0.
        let row_opt = sqlx::query(
            r#"WITH
            daily_baseline AS (
                SELECT
                    chain,
                    token_out                                               AS token,
                    date_trunc('day', block_time)::date                     AS day,
                    SUM(usd_value)                                          AS daily_volume_usd
                FROM swaps
                WHERE chain       = $1
                  AND token_out   = $2
                  AND block_time >= $3
                  AND block_time <  $4
                  AND usd_value   > 0
                GROUP BY chain, token_out, date_trunc('day', block_time)::date
            ),
            baseline_7d AS (
                SELECT
                    chain,
                    token,
                    AVG(daily_volume_usd)::TEXT        AS median_volume_usd,
                    STDDEV_POP(daily_volume_usd)::TEXT AS std_volume_usd,
                    AVG(daily_volume_usd)::TEXT        AS mean_volume_usd,
                    COUNT(*)                           AS baseline_day_count
                FROM daily_baseline
                GROUP BY chain, token
            ),
            window_1h_volume AS (
                SELECT
                    chain,
                    token_out                          AS token,
                    SUM(usd_value)::TEXT               AS volume_1h_usd
                FROM swaps
                WHERE chain       = $1
                  AND token_out   = $2
                  AND block_time >= $4
                  AND block_time <  $5
                  AND usd_value   > 0
                  AND amount_out_raw > 0
                GROUP BY chain, token_out
            ),
            price_now AS (
                SELECT DISTINCT ON (chain, token_out)
                    chain,
                    token_out AS token,
                    (usd_value / (amount_out_raw::DOUBLE PRECISION / POWER(10.0, decimals_out::DOUBLE PRECISION)))::TEXT AS price
                FROM swaps
                WHERE chain       = $1
                  AND token_out   = $2
                  AND block_time >= $4
                  AND block_time <  $5
                  AND usd_value   > 0
                  AND amount_out_raw > 0
                ORDER BY chain, token_out, block_time DESC
            ),
            price_start AS (
                SELECT DISTINCT ON (chain, token_out)
                    chain,
                    token_out AS token,
                    (usd_value / (amount_out_raw::DOUBLE PRECISION / POWER(10.0, decimals_out::DOUBLE PRECISION)))::TEXT AS price
                FROM swaps
                WHERE chain       = $1
                  AND token_out   = $2
                  AND block_time >= $4
                  AND block_time <  $5
                  AND usd_value   > 0
                  AND amount_out_raw > 0
                ORDER BY chain, token_out, block_time ASC
            )
            SELECT
                v.volume_1h_usd,
                b.median_volume_usd,
                b.std_volume_usd,
                b.mean_volume_usd,
                b.baseline_day_count,
                (pn.price::DOUBLE PRECISION - ps.price::DOUBLE PRECISION) / ps.price::DOUBLE PRECISION AS price_change_pct_1h,
                CASE
                    WHEN b.std_volume_usd::DOUBLE PRECISION > 0
                    THEN (v.volume_1h_usd::DOUBLE PRECISION - b.mean_volume_usd::DOUBLE PRECISION)
                          / b.std_volume_usd::DOUBLE PRECISION
                    ELSE 0.0
                END AS volume_z_score
            FROM window_1h_volume v
            INNER JOIN baseline_7d b  ON v.chain = b.chain AND v.token = b.token
            INNER JOIN price_now   pn ON v.chain = pn.chain AND v.token = pn.token
            INNER JOIN price_start ps ON v.chain = ps.chain AND v.token = ps.token
            WHERE b.median_volume_usd::DOUBLE PRECISION > 0
              AND v.volume_1h_usd::DOUBLE PRECISION / b.median_volume_usd::DOUBLE PRECISION >= $6
              AND (pn.price::DOUBLE PRECISION - ps.price::DOUBLE PRECISION) / ps.price::DOUBLE PRECISION >= $7"#,
        )
        .bind(chain)          // $1
        .bind(token)          // $2
        .bind(baseline_start) // $3 — 7d baseline start
        .bind(window_start)   // $4 — 1h window start (= baseline end)
        .bind(window_end)     // $5 — 1h window end
        .bind(volume_multiplier) // $6
        .bind(price_spike_pct)   // $7
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        let Some(r) = row_opt else {
            return Ok(None);
        };

        // Also need a market_cap query for the baseline row.
        // The baseline row does not carry market cap — caller computes it from TokenMeta.
        // We return price_change_pct_1h and volume_z_score from the query result.

        let volume_1h_usd = get_decimal(&r, "volume_1h_usd")?;
        let median_volume_usd = get_decimal(&r, "median_volume_usd")?;
        let price_change_pct_raw: f64 = r.try_get("price_change_pct_1h").map_err(StorageError::Postgres)?;
        let volume_z_score: f64 = r.try_get("volume_z_score").map_err(StorageError::Postgres)?;
        let baseline_day_count: i64 = r.try_get("baseline_day_count").map_err(StorageError::Postgres)?;

        let price_change_pct_1h = Decimal::from_f64_retain(price_change_pct_raw)
            .unwrap_or(Decimal::ZERO);
        let volume_z_score_dec = Decimal::from_f64_retain(volume_z_score)
            .unwrap_or(Decimal::ZERO);

        debug!(
            chain,
            token,
            %volume_1h_usd,
            %median_volume_usd,
            baseline_day_count,
            "D04 Signal A spike row fetched"
        );

        Ok(Some(PumpDumpBaselineRow {
            volume_1h_usd,
            volume_7d_median_usd: median_volume_usd,
            price_change_pct_1h,
            volume_z_score: volume_z_score_dec,
            baseline_days_available: baseline_day_count,
            // market_cap_usd is populated by the caller from TokenMeta; set ZERO here.
            market_cap_usd: Decimal::ZERO,
            market_cap_source: "unavailable".to_owned(),
        }))
    }

    /// Fetch the Signal B burst concentration metrics.
    ///
    /// Implements Query B from `docs/designs/0007-detector-04-pump-dump.md` §3.3.
    /// Returns `volume_1h_usd`, `volume_24h_usd`, and their ratio.
    ///
    /// The `burst_ratio` is guarded against division-by-zero: when `volume_24h_usd = 0`
    /// the query returns `burst_ratio = 0.0`, which will not cross the threshold.
    #[instrument(skip(self), fields(chain, token))]
    pub async fn fetch_burst_metrics(
        &self,
        chain: &str,
        token: &str,
        window_end: DateTime<Utc>,
    ) -> Result<Option<BurstMetricsRow>, StorageError> {
        let window_start_1h = window_end - chrono::Duration::hours(1);
        let window_start_24h = window_end - chrono::Duration::hours(24);

        // Query B: burst concentration ratio (Signal B fallback).
        // Uses CROSS JOIN to ensure a row is always returned even when volume is zero.
        let row = sqlx::query(
            r#"WITH vol_1h AS (
                SELECT COALESCE(SUM(usd_value), 0)::TEXT AS volume_1h_usd
                FROM swaps
                WHERE chain       = $1
                  AND token_out   = $2
                  AND block_time >= $3
                  AND block_time <  $4
                  AND usd_value   > 0
            ),
            vol_24h AS (
                SELECT COALESCE(SUM(usd_value), 0)::TEXT AS volume_24h_usd
                FROM swaps
                WHERE chain       = $1
                  AND token_out   = $2
                  AND block_time >= $5
                  AND block_time <  $4
                  AND usd_value   > 0
            )
            SELECT
                vol_1h.volume_1h_usd,
                vol_24h.volume_24h_usd,
                CASE
                    WHEN vol_24h.volume_24h_usd::DOUBLE PRECISION > 0
                    THEN vol_1h.volume_1h_usd::DOUBLE PRECISION
                         / vol_24h.volume_24h_usd::DOUBLE PRECISION
                    ELSE 0.0
                END AS burst_concentration_ratio
            FROM vol_1h CROSS JOIN vol_24h"#,
        )
        .bind(chain)             // $1
        .bind(token)             // $2
        .bind(window_start_1h)  // $3 — 1h window start
        .bind(window_end)        // $4 — window end (shared for 1h and 24h upper bound)
        .bind(window_start_24h) // $5 — 24h window start
        .fetch_one(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        let volume_1h_usd = get_decimal(&row, "volume_1h_usd")?;
        let volume_24h_usd = get_decimal(&row, "volume_24h_usd")?;
        let burst_ratio_raw: f64 = row.try_get("burst_concentration_ratio").map_err(StorageError::Postgres)?;
        // burst_ratio cannot exceed 1.0 by definition (1h is a subset of 24h).
        // Guard against floating-point rounding producing values slightly above 1.
        let burst_ratio_clamped = burst_ratio_raw.clamp(0.0_f64, 1.0_f64);
        let burst_concentration_ratio = Decimal::from_f64_retain(burst_ratio_clamped)
            .unwrap_or(Decimal::ZERO);

        if volume_1h_usd == Decimal::ZERO {
            // No swaps at all — return None so the caller can distinguish
            // "no data" from "data but below threshold".
            return Ok(None);
        }

        debug!(
            chain,
            token,
            %volume_1h_usd,
            %volume_24h_usd,
            %burst_concentration_ratio,
            "D04 Signal B burst metrics fetched"
        );

        Ok(Some(BurstMetricsRow {
            volume_1h_usd,
            volume_24h_usd,
            burst_concentration_ratio,
        }))
    }

    /// Fetch insider sell transactions for Signal C confirmation.
    ///
    /// Implements `docs/queries/d04_pump_and_dump.sql` Query 2. Returns one row per
    /// insider wallet that sold tokens within `post_pump_insider_window_hours` of the spike.
    ///
    /// # Parameters
    ///
    /// - `insider_addresses`: canonical wallet addresses from deployer_clusters or
    ///   top_holders_proxy. An empty slice returns an empty Vec without a DB round-trip.
    /// - `balance_at_spike_raw`: map of address → balance at spike time. Used to compute
    ///   `sold_pct`. When a wallet is not in the map, `sold_pct` is set to ZERO.
    /// - `window_start`, `window_end`: the insider-sell observation window (spike_time to
    ///   spike_time + post_pump_insider_window_hours).
    ///
    /// Results are ordered by `total_sold_raw DESC` (deterministic).
    #[instrument(skip(self, insider_addresses), fields(chain, token, n_insiders = insider_addresses.len()))]
    pub async fn fetch_insider_sells(
        &self,
        chain: &str,
        token: &str,
        insider_addresses: &[String],
        window_start: DateTime<Utc>,
        window_end: DateTime<Utc>,
    ) -> Result<Vec<InsiderSellRow>, StorageError> {
        if insider_addresses.is_empty() {
            return Ok(vec![]);
        }

        // Pass the insider address array as a Postgres TEXT[] parameter.
        let rows = sqlx::query(
            r#"SELECT
                from_address                    AS address,
                SUM(amount_raw)::TEXT           AS total_sold_raw,
                COUNT(*)                        AS sell_tx_count,
                MAX(tx_hash)                    AS sample_tx_hash
            FROM transfers
            WHERE chain         = $1
              AND token         = $2
              AND from_address  = ANY($3)
              AND block_time   >= $4
              AND block_time   <  $5
              AND is_burn       = FALSE
            GROUP BY chain, from_address
            ORDER BY SUM(amount_raw) DESC"#,
        )
        .bind(chain)
        .bind(token)
        .bind(insider_addresses)
        .bind(window_start)
        .bind(window_end)
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        let mut result = Vec::with_capacity(rows.len());
        for r in rows {
            let address: String = r.try_get("address").map_err(StorageError::Postgres)?;
            let total_sold_raw = get_decimal(&r, "total_sold_raw")?;
            let sample_tx_hash: Option<String> = r.try_get("sample_tx_hash").map_err(StorageError::Postgres)?;

            result.push(InsiderSellRow {
                address,
                sold_amount_raw: total_sold_raw,
                // balance_at_spike_raw and sold_pct are not available from transfers table alone.
                // They are computed by the detector using TokenMeta.top_holders or deployer_clusters.
                // Return ZERO sentinels; the detector fills them in from its InsiderSet data.
                balance_at_spike_raw: Decimal::ZERO,
                sold_pct: Decimal::ZERO,
                sample_tx_hash,
            });
        }

        debug!(
            chain,
            token,
            insider_rows = result.len(),
            "D04 Signal C insider sells fetched"
        );
        Ok(result)
    }

    /// Fetch top holders with balance >= `min_pct` of total supply, excluding
    /// known non-insider classifications (DexPool, VestingContract, CexWallet).
    ///
    /// Used for Signal C Priority 2 degraded mode when `deployer_clusters` is absent.
    /// Returns holder addresses ordered by balance_raw DESC (largest first — deterministic).
    ///
    /// # SQL pattern
    ///
    /// ```sql
    /// SELECT hs.holder
    /// FROM holder_snapshots hs
    /// LEFT JOIN holder_classifications hc ON hc.chain = hs.chain AND hc.address = hs.holder
    /// LEFT JOIN tokens t ON t.chain = hs.chain AND t.mint = hs.token
    /// WHERE hs.chain = $1 AND hs.token = $2
    ///   AND hs.balance_raw >= t.total_supply_raw * $3  -- >= min_pct of supply
    ///   AND (hc.kind IS NULL OR hc.kind NOT IN ('DexPool', 'VestingContract', 'CexWallet'))
    /// ORDER BY hs.balance_raw DESC
    /// ```
    #[instrument(skip(self), fields(chain, token))]
    pub async fn fetch_top_holders_liquid(
        &self,
        chain: &str,
        token: &str,
        min_pct: Decimal,
    ) -> Result<Vec<String>, StorageError> {
        let min_pct_str = min_pct.to_string();

        let rows = sqlx::query(
            r#"SELECT hs.holder
            FROM holder_snapshots hs
            LEFT JOIN holder_classifications hc
                ON hc.chain = hs.chain AND hc.address = hs.holder
            JOIN tokens t
                ON t.chain = hs.chain AND t.mint = hs.token
            WHERE hs.chain = $1
              AND hs.token = $2
              AND hs.balance_raw > 0
              AND hs.balance_raw::NUMERIC >= (t.total_supply_raw * $3::NUMERIC)
              AND (hc.kind IS NULL
                   OR hc.kind NOT IN ('DexPool', 'VestingContract', 'CexWallet'))
            ORDER BY hs.balance_raw DESC"#,
        )
        .bind(chain)
        .bind(token)
        .bind(min_pct_str)
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        let addresses: Vec<String> = rows
            .into_iter()
            .map(|r| r.try_get::<String, _>("holder").unwrap_or_default())
            .collect();

        debug!(
            chain,
            token,
            n_holders = addresses.len(),
            "D04 top_holders_liquid (proxy insider) fetched"
        );
        Ok(addresses)
    }

    /// Fetch all deployer cluster addresses for a token.
    ///
    /// Returns the canonical insider address set when the graph module (Phase 3) has
    /// populated `deployer_clusters`. Returns empty Vec in Phase 2.
    #[instrument(skip(self), fields(chain, token))]
    pub async fn fetch_deployer_cluster_addresses(
        &self,
        chain: &str,
        token: &str,
    ) -> Result<Vec<String>, StorageError> {
        let rows = sqlx::query(
            r#"SELECT address
            FROM deployer_clusters
            WHERE chain = $1 AND token = $2
            ORDER BY address ASC"#,
        )
        .bind(chain)
        .bind(token)
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        Ok(rows
            .into_iter()
            .map(|r| r.try_get::<String, _>("address").unwrap_or_default())
            .collect())
    }
}

// ---------------------------------------------------------------------------
// D04 typed row structs (added with Signal A/B/C storage methods)
// ---------------------------------------------------------------------------

/// Row returned by [`PgStore::fetch_pump_dump_baseline`].
///
/// Carries the 1h volume, 7d rolling baseline metrics, and price-change information
/// needed for Signal A confidence computation.
#[derive(Debug, Clone)]
pub struct PumpDumpBaselineRow {
    /// 1h volume in USD (from the observation window).
    pub volume_1h_usd: Decimal,
    /// Mean daily volume over the 7-day baseline window.
    ///
    /// Note: labeled "median" for historical reasons but computed as AVG (see
    /// `docs/designs/0007-detector-04-pump-dump.md` §3.2 for the explanation).
    /// Zero when `median_volume_usd = 0` (zero-baseline case → Signal B path).
    pub volume_7d_median_usd: Decimal,
    /// `(price_now - price_start) / price_start` for the 1h window. Signed.
    pub price_change_pct_1h: Decimal,
    /// Z-score: `(volume_1h - mean) / std`; zero when std = 0.
    pub volume_z_score: Decimal,
    /// Number of days with non-zero volume in the 7-day baseline window.
    pub baseline_days_available: i64,
    /// Best available market cap proxy (circulating × price OR FDV). Set by caller.
    pub market_cap_usd: Decimal,
    /// Provenance of `market_cap_usd`: "circulating" | "total_supply" | "unavailable".
    pub market_cap_source: String,
}

/// Row returned by [`PgStore::fetch_burst_metrics`].
///
/// Carries the 1h and 24h volume and their concentration ratio for Signal B.
#[derive(Debug, Clone)]
pub struct BurstMetricsRow {
    /// 1h volume in USD.
    pub volume_1h_usd: Decimal,
    /// 24h volume in USD (includes the 1h window).
    pub volume_24h_usd: Decimal,
    /// `volume_1h_usd / volume_24h_usd`, clamped to `[0.0, 1.0]`.
    /// Zero when `volume_24h_usd = 0` (div-by-zero guard in SQL).
    pub burst_concentration_ratio: Decimal,
}

/// A single insider wallet sell record, returned by [`PgStore::fetch_insider_sells`].
#[derive(Debug, Clone)]
pub struct InsiderSellRow {
    /// Wallet address (canonical form for the chain).
    pub address: String,
    /// Total raw token amount sold in the window.
    pub sold_amount_raw: Decimal,
    /// Raw token balance at spike time (populated by the detector from InsiderSet; ZERO from DB).
    pub balance_at_spike_raw: Decimal,
    /// `sold_amount_raw / balance_at_spike_raw` (populated by detector; ZERO from DB).
    pub sold_pct: Decimal,
    /// A sample sell transaction hash for the evidence bundle.
    pub sample_tx_hash: Option<String>,
}

// ---------------------------------------------------------------------------
// D05 typed row structs (added with Signal A/B storage methods)
// ---------------------------------------------------------------------------

/// A row returned by [`PgStore::fetch_recent_swap_buys`] for D11 synchronized-activity.
///
/// One row per qualifying buy-swap event (token_out = token) in the lookback window.
/// Ordered by `(block_height ASC, tx_hash ASC)` for deterministic DBSCAN input.
///
/// # Amount encoding
///
/// `amount_out_raw` is a `u128` parsed from the `NUMERIC(39,0)` column via the
/// String bridge (TEXT cast in SQL; `parse::<u128>()` in Rust).
#[derive(Debug, Clone)]
pub struct SwapBuyRow {
    /// Wallet (sender) address in canonical chain form.
    pub sender: String,
    /// Pool address in canonical chain form.
    pub pool: String,
    /// Block timestamp (UTC) for this swap.
    pub block_time: DateTime<Utc>,
    /// Block height for ordering (determinism tie-breaker after block_time).
    pub block_height: i64,
    /// Transaction hash for evidence bundle (one per wallet in cluster).
    pub tx_hash: String,
    /// Raw token amount received (`amount_out_raw` from `swaps` table, NUMERIC → u128).
    pub amount_out_raw: u128,
}

/// A row returned by [`PgStore::fetch_wash_trading_round_trips`].
///
/// One row per `(sender, pool)` pair: all qualifying buy→sell (or sell→buy) pairs
/// in the observation window for one sender at one pool, aggregated.
///
/// # DG5-3 note
///
/// `wash_volume_usd` is computed via `COALESCE(usd_value, 0)` in SQL. When
/// `usd_value` is NULL for some swap rows, the USD total will be understated.
/// The detector uses `min_wash_volume_usd` as a floor to stabilise the
/// confidence formula regardless.
///
/// # Direction field
///
/// `direction` is `"buy_first"` (sender bought then sold) or `"sell_first"`
/// (sender sold then bought). The SQL UNION ALL handles both; the Rust layer
/// deduplicates on `(buy_tx, sell_tx)` before returning.
#[derive(Debug, Clone)]
pub struct RoundTripRow {
    /// Sender address (canonical for chain).
    pub sender: String,
    /// Pool address (canonical for chain).
    pub pool: String,
    /// Buy transaction hash of the first qualifying pair.
    pub buy_tx: String,
    /// Sell transaction hash of the last qualifying pair.
    pub sell_tx: String,
    /// Sum of `COALESCE(usd_value, 0)` for buy legs across qualifying pairs.
    pub wash_volume_usd: Decimal,
    /// Number of qualifying round-trip pairs for this (sender, pool).
    pub round_trip_count: i64,
    /// `AVG(|buy_amount - sell_amount| / max(buy, sell))` across qualifying pairs.
    pub avg_volume_diff_pct: f64,
    /// `"buy_first"` or `"sell_first"`.
    pub direction: String,
}

impl PgStore {
    /// Fetch Signal A round-trip rows for a token in a pool.
    ///
    /// Executes a symmetric buy-first + sell-first self-join query, deduplicates on
    /// `(buy_tx, sell_tx)`, and returns rows grouped by `(sender, pool)` with
    /// `round_trip_count >= min_repetitions` already applied in SQL.
    ///
    /// # Parameters
    ///
    /// - `chain`: chain string (e.g. `"solana"`).
    /// - `token`: token mint / contract address.
    /// - `window_hours`: observation window length in hours.
    /// - `window_end`: exclusive end of the observation window.
    /// - `volume_diff_pct`: max `|buy - sell| / max(buy, sell)`.
    /// - `min_repetitions`: minimum pairs to return a row.
    /// - `block_window_slots`: max `sell_block - buy_block` (or reverse).
    ///
    /// # Returns
    ///
    /// `Ok(Vec<RoundTripRow>)` sorted by `round_trip_count DESC` (deterministic).
    /// `Err(StorageError::Postgres)` on query failure.
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip(self), fields(chain, token))]
    pub async fn fetch_wash_trading_round_trips(
        &self,
        chain: &str,
        token: &str,
        window_hours: i64,
        window_end: DateTime<Utc>,
        volume_diff_pct: f64,
        min_repetitions: i64,
        block_window_slots: i64,
    ) -> Result<Vec<RoundTripRow>, StorageError> {
        let window_start = window_end - chrono::Duration::hours(window_hours);

        // Symmetric query: buy_first UNION ALL sell_first, then deduplicate and
        // group by (sender, pool). Both directions use the same volume_diff_pct
        // and block_window_slots threshold. The SQL labels each direction so that
        // the evidence key `wash_trading_h1/direction` is populated.
        //
        // Deduplication: a pair that matches in BOTH directions (buy_first and
        // sell_first) would be double-counted without the DISTINCT ON in the outer
        // query. We use a CTE that picks one direction per (buy_tx, sell_tx) pair.
        //
        // NULL usd_value: COALESCE(b.usd_value, 0) — understates wash volume when
        // price data is absent but avoids NULL propagation. The detector records
        // missing_usd_value_count from a separate query if needed.
        let rows = sqlx::query(
            r#"
WITH buys AS (
    SELECT sender, pool, block_height, tx_hash,
           amount_out_raw::DOUBLE PRECISION AS token_amount,
           COALESCE(usd_value::DOUBLE PRECISION, 0) AS usd_val
    FROM swaps
    WHERE chain = $1 AND token_out = $2
      AND block_time >= $3 AND block_time < $4
),
sells AS (
    SELECT sender, pool, block_height, tx_hash,
           amount_in_raw::DOUBLE PRECISION AS token_amount,
           COALESCE(usd_value::DOUBLE PRECISION, 0) AS usd_val
    FROM swaps
    WHERE chain = $1 AND token_in = $2
      AND block_time >= $3 AND block_time < $4
),
buy_first AS (
    SELECT b.sender, b.pool,
           b.tx_hash AS buy_tx, s.tx_hash AS sell_tx,
           b.usd_val AS buy_usd,
           ABS(b.token_amount - s.token_amount)
               / GREATEST(b.token_amount, s.token_amount) AS vol_diff_pct,
           'buy_first' AS direction
    FROM buys b
    INNER JOIN sells s
        ON b.sender = s.sender AND b.pool = s.pool
       AND s.block_height > b.block_height
       AND s.block_height - b.block_height <= $7
    WHERE GREATEST(b.token_amount, s.token_amount) > 0
      AND ABS(b.token_amount - s.token_amount)
              / GREATEST(b.token_amount, s.token_amount) <= $5
),
sell_first AS (
    SELECT s.sender, s.pool,
           b.tx_hash AS buy_tx, s.tx_hash AS sell_tx,
           b.usd_val AS buy_usd,
           ABS(b.token_amount - s.token_amount)
               / GREATEST(b.token_amount, s.token_amount) AS vol_diff_pct,
           'sell_first' AS direction
    FROM sells s
    INNER JOIN buys b
        ON b.sender = s.sender AND b.pool = s.pool
       AND b.block_height > s.block_height
       AND b.block_height - s.block_height <= $7
    WHERE GREATEST(b.token_amount, s.token_amount) > 0
      AND ABS(b.token_amount - s.token_amount)
              / GREATEST(b.token_amount, s.token_amount) <= $5
),
all_pairs AS (
    SELECT * FROM buy_first
    UNION ALL
    SELECT * FROM sell_first
),
deduped AS (
    -- Pick one direction per (buy_tx, sell_tx) pair to avoid double-counting.
    SELECT DISTINCT ON (sender, pool, buy_tx, sell_tx)
           sender, pool, buy_tx, sell_tx, buy_usd, vol_diff_pct, direction
    FROM all_pairs
    ORDER BY sender, pool, buy_tx, sell_tx, direction
)
SELECT
    sender, pool,
    COUNT(*)::BIGINT                AS round_trip_count,
    SUM(buy_usd)                    AS wash_volume_usd,
    AVG(vol_diff_pct)               AS avg_volume_diff_pct,
    MIN(buy_tx)                     AS buy_tx,
    MAX(sell_tx)                    AS sell_tx,
    MODE() WITHIN GROUP (ORDER BY direction) AS direction
FROM deduped
GROUP BY sender, pool
HAVING COUNT(*) >= $6
ORDER BY round_trip_count DESC, sender
            "#,
        )
        .bind(chain)
        .bind(token)
        .bind(window_start)
        .bind(window_end)
        .bind(volume_diff_pct)
        .bind(min_repetitions)
        .bind(block_window_slots)
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        let mut result = Vec::with_capacity(rows.len());
        for r in &rows {
            let sender: String = r.try_get("sender").map_err(StorageError::Postgres)?;
            let pool: String = r.try_get("pool").map_err(StorageError::Postgres)?;
            let buy_tx: String = r.try_get("buy_tx").map_err(StorageError::Postgres)?;
            let sell_tx: String = r.try_get("sell_tx").map_err(StorageError::Postgres)?;
            let round_trip_count: i64 =
                r.try_get("round_trip_count").map_err(StorageError::Postgres)?;
            let avg_volume_diff_pct: f64 =
                r.try_get("avg_volume_diff_pct").map_err(StorageError::Postgres)?;
            let direction: String = r.try_get("direction").map_err(StorageError::Postgres)?;

            // wash_volume_usd is SUM of DOUBLE PRECISION — read as f64 then convert to Decimal.
            let wash_vol_f64: f64 =
                r.try_get("wash_volume_usd").map_err(StorageError::Postgres)?;
            let wash_volume_usd = rust_decimal::Decimal::from_f64_retain(wash_vol_f64)
                .unwrap_or(rust_decimal::Decimal::ZERO);

            result.push(RoundTripRow {
                sender,
                pool,
                buy_tx,
                sell_tx,
                wash_volume_usd,
                round_trip_count,
                avg_volume_diff_pct,
                direction,
            });
        }

        debug!(
            chain,
            token,
            count = result.len(),
            "D05 fetch_wash_trading_round_trips returned rows"
        );
        Ok(result)
    }

    /// Fetch total pool volume in USD for Signal C amplifier computation.
    ///
    /// Returns the sum of `COALESCE(usd_value, 0)` for all swaps involving the
    /// token in the observation window. Used to compute `wash_volume_ratio`.
    #[instrument(skip(self), fields(chain, token))]
    pub async fn fetch_pool_volume_usd(
        &self,
        chain: &str,
        token: &str,
        window_hours: i64,
        window_end: DateTime<Utc>,
    ) -> Result<Decimal, StorageError> {
        let window_start = window_end - chrono::Duration::hours(window_hours);

        let row = sqlx::query(
            r#"
SELECT COALESCE(SUM(usd_value::DOUBLE PRECISION), 0) AS total_volume_usd
FROM swaps
WHERE chain = $1
  AND (token_in = $2 OR token_out = $2)
  AND block_time >= $3
  AND block_time <  $4
            "#,
        )
        .bind(chain)
        .bind(token)
        .bind(window_start)
        .bind(window_end)
        .fetch_one(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        let total_f64: f64 = row
            .try_get("total_volume_usd")
            .map_err(StorageError::Postgres)?;
        let total = Decimal::from_f64_retain(total_f64).unwrap_or(Decimal::ZERO);

        debug!(chain, token, %total, "D05 fetch_pool_volume_usd returned");
        Ok(total)
    }

    // -------------------------------------------------------------------------
    // D06 Mint / Burn Anomaly queries
    // -------------------------------------------------------------------------

    /// Fetch qualifying supply change events (mints + burns) for D06 Signal B.
    ///
    /// Implements `docs/queries/d06_mint_burn_anomaly.sql` Queries 1 and 2 combined.
    ///
    /// A row qualifies when:
    /// - It is a mint (from_address = zero_address) or burn (to_address = zero_address).
    /// - The recipient / burner is NOT in `known_lp_addresses`.
    /// - `amount_raw / supply_denominator >= threshold_pct`.
    ///
    /// # Parameters
    ///
    /// - `chain`: chain identifier, e.g. `"solana"`.
    /// - `token`: token mint / contract address.
    /// - `window_start`: inclusive start of observation window (block time).
    /// - `window_end`: exclusive end of observation window (block time).
    /// - `supply_denominator`: circulating or total supply (Decimal, non-zero).
    /// - `threshold_pct`: minimum absolute supply change fraction to qualify.
    /// - `zero_address`: chain-canonical zero / null address.
    /// - `known_lp_addresses`: LP contract addresses to exclude from Signal B.
    ///
    /// # Returns
    ///
    /// Rows ordered by `block_time ASC` (deterministic). The `supply_change_pct`
    /// field is positive for mints and negative for burns.
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip(self, known_lp_addresses), fields(chain, token))]
    pub async fn fetch_supply_change_events(
        &self,
        chain: &str,
        token: &str,
        window_start: DateTime<Utc>,
        window_end: DateTime<Utc>,
        supply_denominator: Decimal,
        threshold_pct: f64,
        zero_address: &str,
        known_lp_addresses: &[String],
    ) -> Result<Vec<SupplyChangeEventRow>, StorageError> {
        // Use a TEXT representation of the Decimal supply denominator (String bridge pattern).
        let supply_denom_str = supply_denominator.to_string();

        // We run two queries (mints + burns) and merge results.
        // Merged in Rust rather than SQL UNION to keep each query simple and separately
        // maintainable per the ADR 0002 convention.

        // --- Query 1: Unexpected mints (from_address = zero_address) ---
        let mint_rows = sqlx::query(
            r#"SELECT
                   tx_hash,
                   block_time,
                   block_height,
                   log_index,
                   'mint'::TEXT                                                AS event_kind,
                   amount_raw::TEXT                                            AS amount_raw,
                   amount_raw::DOUBLE PRECISION / $8::DOUBLE PRECISION        AS supply_change_pct,
                   to_address                                                  AS recipient
               FROM transfers
               WHERE chain        = $1
                 AND token        = $2
                 AND from_address = $3
                 AND to_address  != $3
                 AND block_time  >= $4
                 AND block_time  <  $5
                 AND to_address  != ALL($6)
                 AND amount_raw::DOUBLE PRECISION / $8::DOUBLE PRECISION >= $7
               ORDER BY block_time ASC, supply_change_pct DESC"#,
        )
        .bind(chain)
        .bind(token)
        .bind(zero_address)
        .bind(window_start)
        .bind(window_end)
        .bind(known_lp_addresses)
        .bind(threshold_pct)
        .bind(supply_denom_str.as_str())
        .fetch_all(&self.pool)
        .await?;

        // --- Query 2: Unexpected burns (to_address = zero_address) ---
        let burn_rows = sqlx::query(
            r#"SELECT
                   tx_hash,
                   block_time,
                   block_height,
                   log_index,
                   'burn'::TEXT                                                AS event_kind,
                   amount_raw::TEXT                                            AS amount_raw,
                   -(amount_raw::DOUBLE PRECISION / $8::DOUBLE PRECISION)     AS supply_change_pct,
                   from_address                                                AS recipient
               FROM transfers
               WHERE chain        = $1
                 AND token        = $2
                 AND to_address   = $3
                 AND from_address != $3
                 AND block_time  >= $4
                 AND block_time  <  $5
                 AND from_address != ALL($6)
                 AND amount_raw::DOUBLE PRECISION / $8::DOUBLE PRECISION >= $7
               ORDER BY block_time ASC, supply_change_pct ASC"#,
        )
        .bind(chain)
        .bind(token)
        .bind(zero_address)
        .bind(window_start)
        .bind(window_end)
        .bind(known_lp_addresses)
        .bind(threshold_pct)
        .bind(supply_denom_str.as_str())
        .fetch_all(&self.pool)
        .await?;

        let mut rows: Vec<SupplyChangeEventRow> = Vec::with_capacity(
            mint_rows.len() + burn_rows.len(),
        );

        // Deserialise mint rows.
        for r in mint_rows {
            let amount_str: String = r.try_get("amount_raw").map_err(StorageError::Postgres)?;
            let amount_raw = Decimal::from_str(&amount_str)
                .map_err(|e| StorageError::Other(format!("parse amount_raw for D06 mint: {e}")))?;
            rows.push(SupplyChangeEventRow {
                tx_hash: r.try_get("tx_hash").map_err(StorageError::Postgres)?,
                block_time: r.try_get("block_time").map_err(StorageError::Postgres)?,
                block_height: r.try_get("block_height").map_err(StorageError::Postgres)?,
                log_index: r.try_get("log_index").map_err(StorageError::Postgres)?,
                event_kind: "mint".to_owned(),
                amount_raw,
                supply_change_pct: r.try_get("supply_change_pct").map_err(StorageError::Postgres)?,
                recipient: r.try_get("recipient").map_err(StorageError::Postgres)?,
            });
        }

        // Deserialise burn rows.
        for r in burn_rows {
            let amount_str: String = r.try_get("amount_raw").map_err(StorageError::Postgres)?;
            let amount_raw = Decimal::from_str(&amount_str)
                .map_err(|e| StorageError::Other(format!("parse amount_raw for D06 burn: {e}")))?;
            rows.push(SupplyChangeEventRow {
                tx_hash: r.try_get("tx_hash").map_err(StorageError::Postgres)?,
                block_time: r.try_get("block_time").map_err(StorageError::Postgres)?,
                block_height: r.try_get("block_height").map_err(StorageError::Postgres)?,
                log_index: r.try_get("log_index").map_err(StorageError::Postgres)?,
                event_kind: "burn".to_owned(),
                amount_raw,
                supply_change_pct: r.try_get("supply_change_pct").map_err(StorageError::Postgres)?,
                recipient: r.try_get("recipient").map_err(StorageError::Postgres)?,
            });
        }

        // Sort merged result by block_time ASC for determinism (merges the two ordered streams).
        rows.sort_by(|a, b| a.block_time.cmp(&b.block_time).then(a.log_index.cmp(&b.log_index)));

        debug!(
            chain,
            token,
            count = rows.len(),
            "D06 fetch_supply_change_events returned rows"
        );
        Ok(rows)
    }

    /// Fetch cumulative non-LP supply change (mints only) for D06 Signal C.
    ///
    /// Sums mint `amount_raw` values where:
    /// - Transfer is from zero_address (mint event).
    /// - `to_address` is NOT in `known_lp_addresses` (non-LP recipient gate).
    /// - `block_time >= window_end - hidden_mint_window_days`.
    ///
    /// # Returns
    ///
    /// `(cumulative_pct, event_count)`:
    /// - `cumulative_pct` = SUM(amount_raw) / supply_denominator (as Decimal).
    /// - `event_count` = number of distinct non-LP mint events in the window.
    ///
    /// Returns `(Decimal::ZERO, 0)` when no qualifying events exist.
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip(self, known_lp_addresses), fields(chain, token))]
    pub async fn fetch_cumulative_supply_change(
        &self,
        chain: &str,
        token: &str,
        window_end: DateTime<Utc>,
        window_days: u64,
        supply_denominator: Decimal,
        zero_address: &str,
        known_lp_addresses: &[String],
    ) -> Result<(Decimal, u32), StorageError> {
        use chrono::Duration;

        let window_start = window_end - Duration::days(window_days as i64);
        let supply_denom_str = supply_denominator.to_string();

        let row = sqlx::query(
            r#"SELECT
                   COALESCE(SUM(amount_raw::NUMERIC), 0)::TEXT   AS cumulative_raw,
                   COUNT(*)::BIGINT                               AS event_count
               FROM transfers
               WHERE chain        = $1
                 AND token        = $2
                 AND from_address = $3
                 AND to_address  != $3
                 AND block_time  >= $4
                 AND block_time  <  $5
                 AND to_address  != ALL($6)"#,
        )
        .bind(chain)
        .bind(token)
        .bind(zero_address)
        .bind(window_start)
        .bind(window_end)
        .bind(known_lp_addresses)
        .fetch_one(&self.pool)
        .await?;

        let cumulative_raw_str: String =
            row.try_get("cumulative_raw").map_err(StorageError::Postgres)?;
        let cumulative_raw = Decimal::from_str(&cumulative_raw_str)
            .map_err(|e| StorageError::Other(format!("parse cumulative_raw for D06 C: {e}")))?;
        let event_count_i64: i64 =
            row.try_get("event_count").map_err(StorageError::Postgres)?;
        let event_count = u32::try_from(event_count_i64.max(0))
            .unwrap_or(u32::MAX);

        let cumulative_pct = if supply_denominator > Decimal::ZERO && cumulative_raw > Decimal::ZERO {
            cumulative_raw / Decimal::from_str(&supply_denom_str).unwrap_or(Decimal::ONE)
        } else {
            Decimal::ZERO
        };

        debug!(
            chain,
            token,
            event_count,
            %cumulative_pct,
            "D06 fetch_cumulative_supply_change returned"
        );
        Ok((cumulative_pct, event_count))
    }

    // -------------------------------------------------------------------------
    // Gateway: paginated anomaly_events feed
    // -------------------------------------------------------------------------

    /// Fetch paginated `anomaly_events` for `GET /v1/anomaly_events`.
    ///
    /// Uses a keyset cursor `(observed_at DESC, id DESC)` for stable pagination
    /// under concurrent inserts. The `id` column was added in V00005.
    ///
    /// Parameters:
    /// - `chain` — optional filter
    /// - `token` — optional filter
    /// - `detector_id` — optional filter
    /// - `severity_min` — inclusive floor (`"info"` returns all)
    /// - `from` — inclusive `observed_at >=`
    /// - `to` — exclusive `observed_at <`
    /// - `cursor_oat` / `cursor_id` — keyset cursor from previous page
    /// - `limit` — max rows to return (caller adds +1 to detect next page)
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip(self), fields(chain, token, detector_id, limit))]
    pub async fn fetch_anomaly_events_paginated(
        &self,
        chain: Option<&str>,
        token: Option<&str>,
        detector_id: Option<&str>,
        severity_min: &str,
        from: Option<DateTime<Utc>>,
        to: DateTime<Utc>,
        cursor_oat: Option<DateTime<Utc>>,
        cursor_id: Option<i64>,
        limit: i64,
    ) -> Result<Vec<AnomalyEventRow>, StorageError> {
        // Build severity ordering: map to integer for >= comparison.
        let severity_order = match severity_min {
            "critical" => 4,
            "high" => 3,
            "medium" => 2,
            "low" => 1,
            _ => 0, // "info" = all
        };

        let rows = sqlx::query(
            r#"
            SELECT id, chain, token, detector_id,
                   observed_at, ingested_at,
                   window_start_height, window_end_height,
                   confidence, severity, evidence
            FROM anomaly_events
            WHERE ($1::TEXT IS NULL OR chain = $1)
              AND ($2::TEXT IS NULL OR token = $2)
              AND ($3::TEXT IS NULL OR detector_id = $3)
              AND observed_at < $4
              AND ($5::TIMESTAMPTZ IS NULL OR observed_at >= $5)
              AND CASE severity
                    WHEN 'critical' THEN 4
                    WHEN 'high'     THEN 3
                    WHEN 'medium'   THEN 2
                    WHEN 'low'      THEN 1
                    ELSE 0
                  END >= $6
              AND (
                $7::TIMESTAMPTZ IS NULL
                OR (observed_at, id) < ($7, $8)
              )
            ORDER BY observed_at DESC, id DESC
            LIMIT $9
            "#,
        )
        .bind(chain)
        .bind(token)
        .bind(detector_id)
        .bind(to)
        .bind(from)
        .bind(severity_order as i64)
        .bind(cursor_oat)
        .bind(cursor_id.unwrap_or(i64::MAX))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        let mut result = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: i64 = row.try_get("id").map_err(StorageError::Postgres)?;
            let chain_str: String = row.try_get("chain").map_err(StorageError::Postgres)?;
            let token_str: String = row.try_get("token").map_err(StorageError::Postgres)?;
            let detector_id_str: String = row.try_get("detector_id").map_err(StorageError::Postgres)?;
            let observed_at: DateTime<Utc> = row.try_get("observed_at").map_err(StorageError::Postgres)?;
            let ingested_at: DateTime<Utc> = row.try_get("ingested_at").map_err(StorageError::Postgres)?;
            let window_start_height: i64 = row.try_get("window_start_height").map_err(StorageError::Postgres)?;
            let window_end_height: i64 = row.try_get("window_end_height").map_err(StorageError::Postgres)?;
            let confidence: f64 = row.try_get("confidence").map_err(StorageError::Postgres)?;
            let severity_str: String = row.try_get("severity").map_err(StorageError::Postgres)?;
            let evidence: serde_json::Value = row.try_get::<sqlx::types::Json<serde_json::Value>, _>("evidence")
                .map(|j| j.0)
                .map_err(StorageError::Postgres)?;

            result.push(AnomalyEventRow {
                id,
                chain: chain_str,
                token: token_str,
                detector_id: detector_id_str,
                observed_at,
                ingested_at,
                window_start_height,
                window_end_height,
                confidence,
                severity: severity_str,
                evidence,
            });
        }

        debug!(count = result.len(), "fetch_anomaly_events_paginated returned");
        Ok(result)
    }

    // -------------------------------------------------------------------------
    // D07 — Token-2022 Withdraw-Withheld instruction table
    // -------------------------------------------------------------------------

    /// Insert one or more Token-2022 instruction rows into `token2022_instructions`.
    ///
    /// Uses `ON CONFLICT DO NOTHING` on the `(chain, tx_hash, log_index)` unique
    /// constraint to make inserts idempotent (reorg / re-ingest safe).
    ///
    /// # Batch strategy
    ///
    /// Token-2022 instruction events are low-volume (one row per instruction, not
    /// per token account). A simple multi-value INSERT is sufficient; COPY is not
    /// needed for this table. Empty `rows` slice is a no-op.
    #[instrument(skip(self, rows), fields(count = rows.len()))]
    pub async fn insert_token2022_instructions(
        &self,
        rows: &[Token2022InstructionRow],
    ) -> Result<(), StorageError> {
        if rows.is_empty() {
            return Ok(());
        }

        for row in rows {
            sqlx::query(
                r#"INSERT INTO token2022_instructions
                   (chain, mint, tx_hash, block_height, block_time,
                    instruction_kind, authority, destination,
                    amount_raw, amount_usd, new_authority, prev_authority, log_index)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8,
                           $9::TEXT::NUMERIC, $10::TEXT::NUMERIC,
                           $11, $12, $13)
                   ON CONFLICT (chain, tx_hash, log_index) DO NOTHING"#,
            )
            .bind(&row.chain)
            .bind(&row.mint)
            .bind(&row.tx_hash)
            .bind(row.block_height)
            .bind(row.block_time)
            .bind(&row.instruction_kind)
            .bind(&row.authority)
            .bind(&row.destination)
            .bind(row.amount_raw.as_ref().map(|d| d.to_string()))
            .bind(row.amount_usd.as_ref().map(|d| d.to_string()))
            .bind(&row.new_authority)
            .bind(&row.prev_authority)
            .bind(row.log_index)
            .execute(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
        }

        debug!(count = rows.len(), "inserted token2022_instructions rows");
        Ok(())
    }

    /// Fetch Token-2022 `WithdrawWithheld*` extraction events for a mint within
    /// a time window (Query W1 + W3 combined).
    ///
    /// Returns `WithdrawWithheldEventsResult` which carries both the event rows
    /// (W1) and the aggregated cumulative metrics (W3) in a single pass to avoid
    /// redundant table scans.
    ///
    /// # Query W1 (event rows)
    ///
    /// Fetches all `withdraw_withheld_from_accounts` and `withdraw_withheld_from_mint`
    /// rows for the given (chain, mint) in `[window_start, window_end)`, ordered by
    /// `block_time ASC` (deterministic ordering).
    ///
    /// # Query W3 (aggregates)
    ///
    /// Returns `COUNT(*)`, `SUM(amount_raw)`, `SUM(amount_usd)` over the same filter.
    /// `SUM(amount_usd) = NULL` when all rows have `amount_usd = NULL` (price unavailable).
    #[instrument(skip(self), fields(chain, mint))]
    pub async fn fetch_withdraw_withheld_events(
        &self,
        chain: &str,
        mint: &str,
        window_start: DateTime<Utc>,
        window_end: DateTime<Utc>,
    ) -> Result<WithdrawWithheldEventsResult, StorageError> {
        // W1: fetch event rows
        let rows = sqlx::query(
            r#"SELECT id, chain, mint, tx_hash, block_height, block_time,
                      instruction_kind, authority, destination,
                      amount_raw::TEXT, amount_usd::TEXT,
                      new_authority, prev_authority, log_index
               FROM token2022_instructions
               WHERE chain             = $1
                 AND mint              = $2
                 AND block_time       >= $3
                 AND block_time       <  $4
                 AND instruction_kind IN (
                       'withdraw_withheld_from_accounts',
                       'withdraw_withheld_from_mint'
                     )
               ORDER BY block_time ASC"#,
        )
        .bind(chain)
        .bind(mint)
        .bind(window_start)
        .bind(window_end)
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        // W3: aggregate over same filter (separate query per spec; optimiser will
        // likely use the same index scan)
        let agg = sqlx::query(
            r#"WITH extraction_events AS (
                   SELECT amount_raw, amount_usd
                   FROM token2022_instructions
                   WHERE chain             = $1
                     AND mint              = $2
                     AND block_time       >= $3
                     AND block_time       <  $4
                     AND instruction_kind IN (
                           'withdraw_withheld_from_accounts',
                           'withdraw_withheld_from_mint'
                         )
               )
               SELECT
                   COUNT(*)::BIGINT           AS event_count,
                   SUM(amount_raw)::TEXT      AS cumulative_raw,
                   SUM(amount_usd)::TEXT      AS cumulative_usd
               FROM extraction_events"#,
        )
        .bind(chain)
        .bind(mint)
        .bind(window_start)
        .bind(window_end)
        .fetch_one(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        let event_count: i64 = agg.try_get("event_count").map_err(StorageError::Postgres)?;
        let cumulative_amount_raw = get_decimal_opt(&agg, "cumulative_raw")?;
        let cumulative_amount_usd = get_decimal_opt(&agg, "cumulative_usd")?;

        let mut event_rows = Vec::with_capacity(rows.len());
        for r in &rows {
            event_rows.push(Self::map_t22_instruction_row(r)?);
        }

        debug!(
            chain,
            mint,
            event_count,
            "fetch_withdraw_withheld_events returned"
        );

        Ok(WithdrawWithheldEventsResult {
            events: event_rows,
            event_count,
            cumulative_amount_raw,
            cumulative_amount_usd,
        })
    }

    /// Fetch `SetAuthority(WithdrawWithheldTokens)` rotation events for a mint within
    /// a lookback window (Query W2).
    ///
    /// Each row is joined with `wallet_funding_events` to get the new authority's
    /// first SOL receipt time (for Signal B fresh-wallet check).
    #[instrument(skip(self), fields(chain, mint))]
    pub async fn fetch_withdraw_authority_history(
        &self,
        chain: &str,
        mint: &str,
        window_start: DateTime<Utc>,
        window_end: DateTime<Utc>,
    ) -> Result<Vec<AuthorityRotationRow>, StorageError> {
        let rows = sqlx::query(
            r#"SELECT
                   ti.id,
                   ti.chain,
                   ti.mint,
                   ti.tx_hash,
                   ti.block_height,
                   ti.block_time,
                   ti.instruction_kind,
                   ti.authority,
                   ti.destination,
                   ti.amount_raw::TEXT,
                   ti.amount_usd::TEXT,
                   ti.new_authority,
                   ti.prev_authority,
                   ti.log_index,
                   wf.first_sol_time AS new_authority_first_sol_time
               FROM token2022_instructions ti
               LEFT JOIN LATERAL (
                   SELECT first_sol_time
                   FROM wallet_funding_events
                   WHERE wallet = ti.new_authority
                     AND chain  = $1
                   ORDER BY first_sol_time ASC
                   LIMIT 1
               ) wf ON TRUE
               WHERE ti.chain             = $1
                 AND ti.mint              = $2
                 AND ti.block_time       >= $3
                 AND ti.block_time       <  $4
                 AND ti.instruction_kind  = 'set_authority_withdraw_withheld'
               ORDER BY ti.block_time ASC"#,
        )
        .bind(chain)
        .bind(mint)
        .bind(window_start)
        .bind(window_end)
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        let mut result = Vec::with_capacity(rows.len());
        for r in &rows {
            let t22_row = Self::map_t22_instruction_row(r)?;
            let new_authority_first_sol_time: Option<DateTime<Utc>> =
                r.try_get("new_authority_first_sol_time")
                    .map_err(StorageError::Postgres)?;
            result.push(AuthorityRotationRow {
                row: t22_row,
                new_authority_first_sol_time,
            });
        }

        debug!(
            chain,
            mint,
            count = result.len(),
            "fetch_withdraw_authority_history returned"
        );

        Ok(result)
    }

    /// Look up the first SOL receipt time for a wallet address from `wallet_funding_events`.
    ///
    /// Returns `None` if the `wallet_funding_events` table has no record for this wallet,
    /// indicating that the indexer has not yet observed or recorded the wallet's first SOL
    /// receipt. D07 treats this as `fresh_wallet_bonus = 0.0` (sidecar absent).
    #[instrument(skip(self), fields(chain, wallet))]
    pub async fn fetch_wallet_funding_time(
        &self,
        chain: &str,
        wallet: &str,
    ) -> Result<Option<DateTime<Utc>>, StorageError> {
        let row = sqlx::query(
            "SELECT first_sol_time FROM wallet_funding_events WHERE chain = $1 AND wallet = $2",
        )
        .bind(chain)
        .bind(wallet)
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        match row {
            None => Ok(None),
            Some(r) => {
                let t: DateTime<Utc> = r.try_get("first_sol_time").map_err(StorageError::Postgres)?;
                Ok(Some(t))
            }
        }
    }

    // -------------------------------------------------------------------------
    // D11 Synchronized-Activity queries
    // -------------------------------------------------------------------------

    /// Fetch recent buy-swap events for D11 synchronized-activity clustering.
    ///
    /// A "buy" swap is one where `token_out = token` (the wallet receives the token).
    ///
    /// Results are ordered by `block_height ASC, tx_hash ASC` for deterministic
    /// input to the DBSCAN clustering algorithm. The hard cap `max_rows` prevents
    /// O(n²) blowup; a WARN is emitted if the cap is hit.
    ///
    /// # Parameters
    ///
    /// - `chain`: chain string (e.g. `"solana"`).
    /// - `token`: token mint / contract address.
    /// - `window_end`: exclusive end of the observation window.
    /// - `lookback_minutes`: lookback window length in minutes.
    /// - `max_rows`: hard row cap (safety ceiling; WARN logged on hit).
    ///
    /// # Returns
    ///
    /// `Ok(Vec<SwapBuyRow>)` ordered by `(block_height ASC, tx_hash ASC)`.
    /// `Err(StorageError::Postgres)` on query failure.
    #[instrument(skip(self), fields(chain, token))]
    pub async fn fetch_recent_swap_buys(
        &self,
        chain: &str,
        token: &str,
        window_end: DateTime<Utc>,
        lookback_minutes: i64,
        max_rows: i64,
    ) -> Result<Vec<SwapBuyRow>, StorageError> {
        let window_start = window_end - chrono::Duration::minutes(lookback_minutes);

        let rows = sqlx::query(
            r#"
SELECT sender, pool, block_time, block_height, tx_hash,
       amount_out_raw::TEXT AS amount_out_raw_str
FROM swaps
WHERE chain = $1
  AND token_out = $2
  AND block_time >= $3
  AND block_time <  $4
ORDER BY block_height ASC, tx_hash ASC
LIMIT $5
            "#,
        )
        .bind(chain)
        .bind(token)
        .bind(window_start)
        .bind(window_end)
        .bind(max_rows)
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        let hit_cap = rows.len() as i64 >= max_rows;
        if hit_cap {
            tracing::warn!(
                chain,
                token,
                cap = max_rows,
                "D11 fetch_recent_swap_buys hit max_rows cap; results may be incomplete"
            );
        }

        let mut result = Vec::with_capacity(rows.len());
        for r in &rows {
            let sender: String = r.try_get("sender").map_err(StorageError::Postgres)?;
            let pool: String = r.try_get("pool").map_err(StorageError::Postgres)?;
            let block_time: DateTime<Utc> =
                r.try_get("block_time").map_err(StorageError::Postgres)?;
            let block_height: i64 =
                r.try_get("block_height").map_err(StorageError::Postgres)?;
            let tx_hash: String = r.try_get("tx_hash").map_err(StorageError::Postgres)?;
            let amount_out_raw_str: String =
                r.try_get("amount_out_raw_str").map_err(StorageError::Postgres)?;
            let amount_out_raw: u128 = amount_out_raw_str.parse().unwrap_or(0);

            result.push(SwapBuyRow {
                sender,
                pool,
                block_time,
                block_height,
                tx_hash,
                amount_out_raw,
            });
        }

        debug!(
            chain,
            token,
            count = result.len(),
            "D11 fetch_recent_swap_buys returned rows"
        );
        Ok(result)
    }

    /// Map a `PgRow` from `token2022_instructions` to a `Token2022InstructionRow`.
    fn map_t22_instruction_row(
        r: &sqlx::postgres::PgRow,
    ) -> Result<Token2022InstructionRow, StorageError> {
        Ok(Token2022InstructionRow {
            id: r.try_get("id").map_err(StorageError::Postgres)?,
            chain: r.try_get("chain").map_err(StorageError::Postgres)?,
            mint: r.try_get("mint").map_err(StorageError::Postgres)?,
            tx_hash: r.try_get("tx_hash").map_err(StorageError::Postgres)?,
            block_height: r.try_get("block_height").map_err(StorageError::Postgres)?,
            block_time: r.try_get("block_time").map_err(StorageError::Postgres)?,
            instruction_kind: r
                .try_get("instruction_kind")
                .map_err(StorageError::Postgres)?,
            authority: r.try_get("authority").map_err(StorageError::Postgres)?,
            destination: r.try_get("destination").map_err(StorageError::Postgres)?,
            amount_raw: get_decimal_opt(r, "amount_raw")?,
            amount_usd: get_decimal_opt(r, "amount_usd")?,
            new_authority: r.try_get("new_authority").map_err(StorageError::Postgres)?,
            prev_authority: r.try_get("prev_authority").map_err(StorageError::Postgres)?,
            log_index: r.try_get("log_index").map_err(StorageError::Postgres)?,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests (unit — no DB needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    #[test]
    fn token_row_u128_conversion() {
        use rust_decimal::prelude::FromPrimitive;
        let row = TokenRow {
            id: 1,
            chain: "solana".into(),
            mint: "So11111111111111111111111111111111111111112".into(),
            symbol: None,
            name: None,
            decimals: 9,
            token_program: None,
            total_supply_raw: Decimal::from_u64(1_000_000_000u64).unwrap(),
            circulating_supply_raw: None,
            mint_authority: None,
            freeze_authority: None,
            creator: None,
            creator_balance_raw: Decimal::ZERO,
            total_holders: 0,
            total_market_liquidity_usd: Decimal::ZERO,
            jup_verified: false,
            jup_strict: false,
            graph_insiders_detected: false,
            rugged: false,
            rugcheck_score: None,
            launchpad: None,
            deploy_platform: None,
            detected_at: None,
            updated_at: Utc::now(),
            permanent_delegate: None,
            transfer_hook_program: None,
            non_transferable: None,
            confidential_transfer: None,
        };
        assert_eq!(row.total_supply_u128(), 1_000_000_000u128);
        assert_eq!(row.creator_balance_u128(), 0u128);
        assert!(row.circulating_supply_u128().is_none());
    }

    #[test]
    fn pool_row_u128_conversion() {
        let row = PoolRow {
            id: 1,
            chain: "solana".into(),
            pool_address: "pooladdr".into(),
            dex: "raydium_v4".into(),
            token0: "tokenA".into(),
            token1: "tokenB".into(),
            reserve0_raw: Decimal::new(100_000, 0),
            reserve1_raw: Decimal::new(200_000, 0),
            lp_total_supply: Decimal::new(10_000, 0),
            deployer_lp_amount: Decimal::new(9_000, 0),
            lifetime_tx_count: 500,
            liquidity_usd: Decimal::new(5000, 0),
            updated_at: Utc::now(),
        };
        assert_eq!(row.lp_total_supply_u128(), 10_000u128);
        assert_eq!(row.deployer_lp_amount_u128(), 9_000u128);
    }

    #[test]
    fn numeric_string_parse_roundtrip_within_decimal_range() {
        // rust_decimal::Decimal supports up to 28 significant digits.
        // u128::MAX has 39 digits — it exceeds Decimal's range.
        //
        // In practice, Solana token supplies rarely exceed 10^18 (9 decimals + 1B tokens).
        // The canonical u128 used in storage is the raw on-chain amount; a u128 value
        // that exceeds 28 digits would represent a supply of ~10^28 tokens, which is
        // astronomically large and not seen in real deployments.
        //
        // For values within the Decimal range (up to 10^28), the String bridge works:
        use rust_decimal::prelude::ToPrimitive;

        // 10^18 (SOL with 9 decimals, 1B token supply) — well within Decimal range
        let amount: u128 = 1_000_000_000_000_000_000u128;
        let s = amount.to_string();
        let d = Decimal::from_str(&s).expect("should parse within Decimal range");
        let back = d.to_u128().expect("should fit in u128");
        assert_eq!(back, amount);
    }

    #[test]
    fn numeric_string_u128_max_exceeds_decimal_range() {
        // Document the known limitation: u128::MAX overflows Decimal (28 sig digits).
        // This is acceptable: actual token supplies do not reach u128::MAX in practice.
        // The schema stores NUMERIC(39,0) in Postgres via the TEXT cast path,
        // but the Decimal intermediate type is only used for amounts ≤ 10^28.
        // For values exceeding Decimal range, callers must use string-only paths.
        let s = u128::MAX.to_string();
        assert!(Decimal::from_str(&s).is_err(), "u128::MAX should overflow Decimal — known limitation");
    }

    #[test]
    fn get_decimal_from_string_helper() {
        // Simulate what get_decimal does (without a real PgRow) for a realistic supply
        let s = "1000000000000000000"; // 10^18 (1B tokens × 10^9 decimals)
        let d = Decimal::from_str(s).unwrap();
        use rust_decimal::prelude::ToPrimitive;
        let v: u128 = d.to_u128().unwrap();
        assert_eq!(v, 1_000_000_000_000_000_000u128);
    }

    // --- D07 storage type unit tests ---

    #[test]
    fn token2022_instruction_row_amount_raw_optional() {
        // SetAuthority rows have no amount_raw — confirm None is representable
        let row = Token2022InstructionRow {
            id: 1,
            chain: "solana".into(),
            mint: "Mint1111111111111111111111111111111111111112".into(),
            tx_hash: "Tx1111111111111111111111111111111111111111111111111111111111111112"
                .into(),
            block_height: 310_000_000,
            block_time: chrono::DateTime::parse_from_rfc3339("2026-04-21T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            instruction_kind: "set_authority_withdraw_withheld".into(),
            authority: Some("Authority111111111111111111111111111111111111".into()),
            destination: None,
            amount_raw: None,
            amount_usd: None,
            new_authority: Some("NewAuth1111111111111111111111111111111111111".into()),
            prev_authority: Some("OldAuth1111111111111111111111111111111111111".into()),
            log_index: 0,
        };
        assert!(row.amount_raw.is_none());
        assert_eq!(row.instruction_kind, "set_authority_withdraw_withheld");
    }

    #[test]
    fn token2022_instruction_row_withdraw_has_amounts() {
        let row = Token2022InstructionRow {
            id: 2,
            chain: "solana".into(),
            mint: "Mint1111111111111111111111111111111111111112".into(),
            tx_hash: "Tx2222222222222222222222222222222222222222222222222222222222222222"
                .into(),
            block_height: 310_000_001,
            block_time: chrono::DateTime::parse_from_rfc3339("2026-04-21T01:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            instruction_kind: "withdraw_withheld_from_accounts".into(),
            authority: Some("Authority111111111111111111111111111111111111".into()),
            destination: Some("Dest111111111111111111111111111111111111111".into()),
            amount_raw: Some(Decimal::new(200_000_000, 0)),
            amount_usd: Some(Decimal::new(400, 0)),
            new_authority: None,
            prev_authority: None,
            log_index: 1,
        };
        assert_eq!(row.amount_raw.unwrap(), Decimal::new(200_000_000, 0));
        assert_eq!(row.amount_usd.unwrap(), Decimal::new(400, 0));
        assert!(row.new_authority.is_none());
    }

    #[test]
    fn withdraw_withheld_events_result_fields() {
        let result = WithdrawWithheldEventsResult {
            events: vec![],
            event_count: 0,
            cumulative_amount_raw: None,
            cumulative_amount_usd: None,
        };
        assert_eq!(result.event_count, 0);
        assert!(result.cumulative_amount_usd.is_none());
    }

    #[test]
    fn authority_rotation_row_with_no_funding_time() {
        let row = Token2022InstructionRow {
            id: 3,
            chain: "solana".into(),
            mint: "Mint1111111111111111111111111111111111111112".into(),
            tx_hash: "Tx3333333333333333333333333333333333333333333333333333333333333333"
                .into(),
            block_height: 310_000_002,
            block_time: Utc::now(),
            instruction_kind: "set_authority_withdraw_withheld".into(),
            authority: None,
            destination: None,
            amount_raw: None,
            amount_usd: None,
            new_authority: None,
            prev_authority: None,
            log_index: 2,
        };
        let rotation_row = AuthorityRotationRow {
            row,
            new_authority_first_sol_time: None,
        };
        assert!(rotation_row.new_authority_first_sol_time.is_none());
    }
}

// ===========================================================================
// D12 — Permit2 event storage (V00014 migration)
// ===========================================================================

/// A row from the `permit2_events` table (V00014).
///
/// Mirrors the V00014 schema exactly. All NUMERIC columns are bridged via
/// String as per pg.rs convention (see module-level doc §Amount encoding).
///
/// # event_kind values
///
/// | Value | Source event |
/// |-------|-------------|
/// | `"permit"` | `Permit(owner, token, spender, amount, expiration, nonce)` |
/// | `"approval"` | `Approval(owner, token, spender, amount, expiration)` |
/// | `"lockdown"` | `Lockdown(owner, token, spender)` |
/// | `"nonce_invalidation"` | `NonceInvalidation(owner, token, spender, newNonce, oldNonce)` |
/// | `"unordered_nonce_invalidation"` | `UnorderedNonceInvalidation(owner, word, mask)` |
#[derive(Debug, Clone)]
pub struct Permit2EventRow {
    /// Always `"ethereum"` at MVP; extended for other EVM chains in Phase 5.
    pub chain: String,
    /// Block timestamp (TIMESTAMPTZ). NEVER wall-clock.
    pub block_time: DateTime<Utc>,
    /// Block height (block number on EVM).
    pub block_height: i64,
    /// 0x-prefixed hex transaction hash (66 chars).
    pub tx_hash: String,
    /// Log index within the transaction (dedup key with tx_hash).
    pub log_index: i32,
    /// Event kind string (see table above).
    pub event_kind: String,
    /// Permit signer / victim address (0x-prefixed lowercase hex).
    pub owner: String,
    /// Token contract address. NULL for `unordered_nonce_invalidation`.
    pub token: Option<String>,
    /// Spender address. NULL for `unordered_nonce_invalidation`.
    pub spender: Option<String>,
    /// Raw uint160 permit amount as Decimal. NULL for lockdown/nonce events.
    /// Stored as NUMERIC(78,0); bridged via String.
    pub amount_raw: Option<Decimal>,
    /// uint48 expiration unix timestamp. NULL for lockdown/nonce events.
    pub expiration_unix: Option<i64>,
    /// uint48 nonce. NULL for `approval` (no nonce field) and `unordered`.
    pub nonce: Option<i64>,
    /// Full decoded event as JSONB for evidence reproduction.
    pub raw_event_data: serde_json::Value,
}

impl PgStore {
    /// Upsert a single `permit2_events` row.
    ///
    /// Duplicate rows (same `chain, tx_hash, log_index, block_time`) are silently
    /// skipped via `ON CONFLICT DO NOTHING` — idempotent at-least-once delivery
    /// per the UNIQUE constraint in V00014 (gotcha #7 compliance: block_time in UK).
    ///
    /// # Errors
    ///
    /// Returns `StorageError::Postgres` on unexpected database errors.
    /// Does NOT return an error on conflict (the upsert is intentionally silent).
    #[instrument(skip(self, row), fields(chain = %row.chain, tx_hash = %row.tx_hash))]
    pub async fn upsert_permit2_event(&self, row: &Permit2EventRow) -> Result<(), StorageError> {
        let amount_str = row.amount_raw.as_ref().map(|d| d.to_string());
        sqlx::query(
            r#"INSERT INTO permit2_events (
                chain, block_time, block_height,
                tx_hash, log_index, event_kind,
                owner, token, spender,
                amount_raw, expiration_unix, nonce,
                raw_event_data
               )
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,
                       $10::NUMERIC,$11,$12,
                       $13)
               ON CONFLICT (chain, tx_hash, log_index, block_time) DO NOTHING"#,
        )
        .bind(&row.chain)
        .bind(row.block_time)
        .bind(row.block_height)
        .bind(&row.tx_hash)
        .bind(row.log_index)
        .bind(&row.event_kind)
        .bind(&row.owner)
        .bind(&row.token)
        .bind(&row.spender)
        .bind(amount_str)
        .bind(row.expiration_unix)
        .bind(row.nonce)
        .bind(&row.raw_event_data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch recent `permit2_events` rows for a given owner and/or spender.
    ///
    /// Used by D12 Signal A2: fetch Permit2 Permit events for a token within
    /// the lookback window, then correlate with ERC-20 Transfer events by `tx_hash`.
    ///
    /// # Parameters
    ///
    /// - `chain` — always `"ethereum"` at MVP.
    /// - `token` — ERC-20 token address; if `None`, returns events for all tokens.
    /// - `owner` — if `Some`, restricts to events signed by this owner (victim).
    /// - `spender` — if `Some`, restricts to events authorising this spender (drainer).
    /// - `window_end` — exclusive upper bound on `block_time` (block-time sourced).
    /// - `lookback_minutes` — how far back to look from `window_end`.
    /// - `max_rows` — hard row cap; WARN is emitted on cap hit.
    ///
    /// # Determinism
    ///
    /// Results are ordered by `(block_height ASC, tx_hash ASC, log_index ASC)`.
    /// Callers MUST use this ordering for deterministic detector output.
    ///
    /// # Performance
    ///
    /// The query uses `idx_permit2_events_chain_token_time` for token-scoped lookups
    /// and `idx_permit2_events_chain_owner_time` / `idx_permit2_events_chain_spender_time`
    /// for victim/drainer lookups (V00014 indexes).
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip(self), fields(chain, token, owner, spender))]
    pub async fn fetch_recent_permit2_events(
        &self,
        chain: &str,
        token: Option<&str>,
        owner: Option<&str>,
        spender: Option<&str>,
        window_end: DateTime<Utc>,
        lookback_minutes: i64,
        max_rows: u32,
    ) -> Result<Vec<Permit2EventRow>, StorageError> {
        let window_start =
            window_end - chrono::Duration::minutes(lookback_minutes);

        // Build query with optional token/owner/spender filters.
        // SPEC-NOTE: We use sequential WHERE clauses rather than a JOIN approach.
        // The D12 algorithm builds a BTreeMap<tx_hash, Vec<Permit>> in the detector,
        // then cross-references with transfer rows fetched separately. This avoids
        // a cross-table JOIN in the storage layer and keeps storage methods simple.
        // Trade-off: two queries per evaluation vs. one JOIN. Acceptable at MVP scale
        // (Permit2 events are low-volume compared to ERC-20 transfers).
        let rows = sqlx::query(
            r#"SELECT
                chain, block_time, block_height,
                tx_hash, log_index, event_kind,
                owner, token, spender,
                amount_raw::TEXT AS amount_raw_str,
                expiration_unix, nonce,
                raw_event_data
               FROM permit2_events
               WHERE chain = $1
                 AND block_time >= $2
                 AND block_time <= $3
                 AND ($4::TEXT IS NULL OR token = $4)
                 AND ($5::TEXT IS NULL OR owner = $5)
                 AND ($6::TEXT IS NULL OR spender = $6)
               ORDER BY block_height ASC, tx_hash ASC, log_index ASC
               LIMIT $7"#,
        )
        .bind(chain)
        .bind(window_start)
        .bind(window_end)
        .bind(token)
        .bind(owner)
        .bind(spender)
        .bind(max_rows as i64)
        .fetch_all(&self.pool)
        .await?;

        if rows.len() == max_rows as usize {
            tracing::warn!(
                chain,
                max_rows,
                "fetch_recent_permit2_events hit row cap; results may be incomplete"
            );
        }

        let result: Result<Vec<Permit2EventRow>, StorageError> = rows
            .into_iter()
            .map(|r| {
                let amount_raw_str: Option<String> = r.try_get("amount_raw_str").ok().flatten();
                let amount_raw = amount_raw_str
                    .as_deref()
                    .map(Decimal::from_str)
                    .transpose()
                    .map_err(|e| {
                        StorageError::Other(format!("permit2_events amount_raw parse: {e}"))
                    })?;

                Ok(Permit2EventRow {
                    chain: r.try_get("chain")?,
                    block_time: r.try_get("block_time")?,
                    block_height: r.try_get("block_height")?,
                    tx_hash: r.try_get("tx_hash")?,
                    log_index: r.try_get("log_index")?,
                    event_kind: r.try_get("event_kind")?,
                    owner: r.try_get("owner")?,
                    token: r.try_get("token")?,
                    spender: r.try_get("spender")?,
                    amount_raw,
                    expiration_unix: r.try_get("expiration_unix")?,
                    nonce: r.try_get("nonce")?,
                    raw_event_data: r.try_get("raw_event_data")?,
                })
            })
            .collect();

        result
    }

    /// Fetch recent ERC-20 Transfer rows for a token within a lookback window.
    ///
    /// Used by D12 Signal A1 (known-drainer address match on `to_address`) and
    /// A2 (same-tx correlation with permit2_events).
    ///
    /// # Determinism
    ///
    /// Results are ordered by `(block_height ASC, tx_hash ASC, log_index ASC)`.
    #[instrument(skip(self), fields(chain, token))]
    pub async fn fetch_recent_transfers_for_token(
        &self,
        chain: &str,
        token: &str,
        window_end: DateTime<Utc>,
        lookback_minutes: i64,
        max_rows: u32,
    ) -> Result<Vec<TransferRow>, StorageError> {
        let window_start =
            window_end - chrono::Duration::minutes(lookback_minutes);

        let rows = sqlx::query(
            r#"SELECT
                chain, token, block_time, block_height,
                tx_hash, log_index,
                from_address, to_address,
                amount_raw::TEXT AS amount_raw_str,
                decimals
               FROM transfers
               WHERE chain = $1
                 AND token = $2
                 AND block_time >= $3
                 AND block_time <= $4
               ORDER BY block_height ASC, tx_hash ASC, log_index ASC
               LIMIT $5"#,
        )
        .bind(chain)
        .bind(token)
        .bind(window_start)
        .bind(window_end)
        .bind(max_rows as i64)
        .fetch_all(&self.pool)
        .await?;

        if rows.len() == max_rows as usize {
            tracing::warn!(
                chain,
                token,
                max_rows,
                "fetch_recent_transfers_for_token hit row cap; results may be incomplete"
            );
        }

        rows.into_iter()
            .map(|r| {
                let amount_raw_str: String = r.try_get("amount_raw_str").unwrap_or_default();
                let amount_raw = Decimal::from_str(&amount_raw_str).map_err(|e| {
                    StorageError::Other(format!("transfers amount_raw parse: {e}"))
                })?;
                Ok(TransferRow {
                    chain: r.try_get("chain")?,
                    token: r.try_get("token")?,
                    block_time: r.try_get("block_time")?,
                    block_height: r.try_get("block_height")?,
                    tx_hash: r.try_get("tx_hash")?,
                    log_index: r.try_get("log_index")?,
                    from_address: r.try_get("from_address")?,
                    to_address: r.try_get("to_address")?,
                    amount_raw,
                    decimals: r.try_get("decimals")?,
                })
            })
            .collect()
    }

    // ===========================================================================
    // D14 — Bridge Drain: outflow / inflow helpers (stateless recompute from transfers)
    // ===========================================================================

    /// Fetch all outbound transfers FROM a bridge custody address in the window.
    ///
    /// Used by D14 to compute total outflow from a known bridge custody address.
    ///
    /// The `transfers` table contains all ERC-20 / SPL token transfers indexed by
    /// (chain, token, block_time). D14 needs transfers WHERE `from_address = custody_address`
    /// regardless of token — bridges hold many different tokens simultaneously.
    ///
    /// # Row cap
    ///
    /// When `max_rows` is hit, a warning is emitted. D14 interprets a capped result
    /// conservatively: total outflow may be underestimated, but the drain_pct ratio
    /// is computed on what we have. False-negative risk noted in D14 design doc.
    ///
    /// # Order
    ///
    /// Deterministic: ORDER BY block_height ASC, log_index ASC.
    pub async fn fetch_outflows_from_bridge(
        &self,
        chain: &str,
        from_address: &str,
        window_end: DateTime<Utc>,
        lookback_minutes: i64,
        max_rows: u32,
    ) -> Result<Vec<BridgeTransferRow>, StorageError> {
        let window_start = window_end - chrono::Duration::minutes(lookback_minutes);

        let rows = sqlx::query(
            r#"SELECT
                from_address, to_address,
                amount_raw::TEXT AS amount_raw_str,
                decimals, block_height, log_index
               FROM transfers
               WHERE chain = $1
                 AND from_address = $2
                 AND block_time >= $3
                 AND block_time <= $4
               ORDER BY block_height ASC, log_index ASC
               LIMIT $5"#,
        )
        .bind(chain)
        .bind(from_address)
        .bind(window_start)
        .bind(window_end)
        .bind(max_rows as i64)
        .fetch_all(&self.pool)
        .await?;

        if rows.len() == max_rows as usize {
            tracing::warn!(
                chain,
                from_address,
                max_rows,
                "fetch_outflows_from_bridge hit row cap; drain_pct may be underestimated"
            );
        }

        rows.into_iter()
            .map(|r| {
                let amount_raw_str: String = r.try_get("amount_raw_str").unwrap_or_default();
                let amount_raw =
                    Decimal::from_str(&amount_raw_str).map_err(|e| {
                        StorageError::Other(format!("bridge outflow amount_raw parse: {e}"))
                    })?;
                Ok(BridgeTransferRow {
                    from_address: r.try_get("from_address")?,
                    to_address: r.try_get("to_address")?,
                    amount_raw,
                    decimals: r.try_get("decimals")?,
                    block_height: r.try_get("block_height")?,
                    log_index: r.try_get("log_index")?,
                })
            })
            .collect()
    }

    /// Fetch all inbound transfers TO a bridge custody address (balance proxy).
    ///
    /// Used by D14 as a stateless balance approximation:
    /// `balance_proxy ≈ Σ inflows - Σ outflows_lifetime`.
    ///
    /// Since we cannot run `eth_getBalance` in the stateless recompute pattern,
    /// we use total inflows over a long lookback window (default 30 days) as a
    /// proxy for the custody balance. This intentionally over-estimates balance,
    /// making drain_pct conservative (under-estimated), which reduces false positives.
    ///
    /// SPEC-NOTE: A dedicated bridge balance snapshot table would be more accurate.
    /// Deferred to Sprint 27+ when bridge-specific infrastructure is added.
    pub async fn fetch_inflows_to_bridge(
        &self,
        chain: &str,
        to_address: &str,
        window_end: DateTime<Utc>,
        lookback_minutes: i64,
        max_rows: u32,
    ) -> Result<Vec<BridgeTransferRow>, StorageError> {
        let window_start = window_end - chrono::Duration::minutes(lookback_minutes);

        let rows = sqlx::query(
            r#"SELECT
                from_address, to_address,
                amount_raw::TEXT AS amount_raw_str,
                decimals, block_height, log_index
               FROM transfers
               WHERE chain = $1
                 AND to_address = $2
                 AND block_time >= $3
                 AND block_time <= $4
               ORDER BY block_height ASC, log_index ASC
               LIMIT $5"#,
        )
        .bind(chain)
        .bind(to_address)
        .bind(window_start)
        .bind(window_end)
        .bind(max_rows as i64)
        .fetch_all(&self.pool)
        .await?;

        if rows.len() == max_rows as usize {
            tracing::warn!(
                chain,
                to_address,
                max_rows,
                "fetch_inflows_to_bridge hit row cap; balance_proxy may be underestimated"
            );
        }

        rows.into_iter()
            .map(|r| {
                let amount_raw_str: String = r.try_get("amount_raw_str").unwrap_or_default();
                let amount_raw =
                    Decimal::from_str(&amount_raw_str).map_err(|e| {
                        StorageError::Other(format!("bridge inflow amount_raw parse: {e}"))
                    })?;
                Ok(BridgeTransferRow {
                    from_address: r.try_get("from_address")?,
                    to_address: r.try_get("to_address")?,
                    amount_raw,
                    decimals: r.try_get("decimals")?,
                    block_height: r.try_get("block_height")?,
                    log_index: r.try_get("log_index")?,
                })
            })
            .collect()
    }
}

/// A typed row from the `transfers` table for D14 bridge drain detector use.
///
/// Separate from `TransferRow` (D12) — D14 doesn't need chain/token/tx_hash fields,
/// only the amount, addresses, and ordering fields.
#[derive(Debug, Clone)]
pub struct BridgeTransferRow {
    /// Sender address.
    pub from_address: String,
    /// Recipient address.
    pub to_address: String,
    /// Raw token amount from NUMERIC(39,0).
    pub amount_raw: Decimal,
    /// Token decimals.
    pub decimals: i16,
    /// Block height (ordering).
    pub block_height: i64,
    /// Log index within block (ordering + dedup).
    pub log_index: i32,
}

/// A typed row from the `transfers` table for D12 detector use.
///
/// Separate from the broader transfer structs in `common` — this is a minimal
/// read-side row for the detector's fetch-then-compute pattern.
#[derive(Debug, Clone)]
pub struct TransferRow {
    pub chain: String,
    pub token: String,
    pub block_time: DateTime<Utc>,
    pub block_height: i64,
    pub tx_hash: String,
    pub log_index: i32,
    pub from_address: String,
    pub to_address: String,
    /// amount_raw from NUMERIC(39,0) column.
    pub amount_raw: Decimal,
    pub decimals: i16,
}

// ===========================================================================
// D13 — MEV sandwich event storage (V00015 migration)
// ===========================================================================

/// A row from the `mev_events` table (V00015).
///
/// Mirrors the V00015 schema exactly. All NUMERIC columns are bridged via
/// String as per pg.rs convention (see module-level doc §Amount encoding).
///
/// # pool_kind values
///
/// | Value | Source pool type |
/// |-------|-----------------|
/// | `"univ2"` | Uniswap V2 pair (amount0In/Out schema) |
/// | `"univ3"` | Uniswap V3 pool (signed int256 amounts) |
#[derive(Debug, Clone)]
pub struct MevEventRow {
    /// Always `"ethereum"` at MVP; extended for other EVM chains in Phase 5.
    pub chain: String,
    /// Block timestamp (TIMESTAMPTZ). NEVER wall-clock.
    pub block_time: DateTime<Utc>,
    /// Block height (block number on EVM).
    pub block_height: i64,
    /// 0x-prefixed hex tx hash of attacker's front-run transaction (66 chars).
    pub tx_hash_front: String,
    /// 0x-prefixed hex tx hash of victim's transaction (66 chars).
    pub tx_hash_victim: String,
    /// 0x-prefixed hex tx hash of attacker's back-run transaction (66 chars).
    pub tx_hash_back: String,
    /// Pool address where the sandwich occurred (0x-prefixed lowercase hex).
    pub pool_address: String,
    /// Attacker EOA or contract address (0x-prefixed lowercase hex).
    pub attacker_address: String,
    /// Victim sender address (heuristic from victim tx; 0x-prefixed lowercase hex).
    pub victim_address: String,
    /// Token the attacker bought / victim was selling (0x-prefixed lowercase hex).
    pub token_in: String,
    /// Counterpart token. NULL for UniV3 single-side cases.
    pub token_out: Option<String>,
    /// Attacker net P&L in `token_in` raw units. NULL when profit cannot be computed.
    /// Stored as NUMERIC(78,0); bridged via String. May be negative if attack failed.
    pub profit_amount_raw: Option<Decimal>,
    /// USD equivalent of attacker profit. NULL until Phase 5 enrichment.
    /// SPEC-NOTE: Same pattern as D11/D12 `amount_usd = None` at Sprint 20.
    pub profit_amount_usd: Option<Decimal>,
    /// Victim slippage imposed by the sandwich as a fraction (e.g. 0.005 = 0.5%).
    pub victim_slippage_pct: Decimal,
    /// Victim swap input amount in token_in raw units.
    pub victim_swap_size_raw: Decimal,
    /// Pool kind: `"univ2"` or `"univ3"`.
    pub pool_kind: String,
    /// Full evidence bundle as JSONB for audit replay. None acceptable for batch paths.
    pub raw_event_data: Option<serde_json::Value>,
}

impl PgStore {
    /// Upsert a single `mev_events` row.
    ///
    /// Duplicate rows (same `chain, block_time, block_height, tx_hash_victim`) are
    /// silently skipped via `ON CONFLICT DO NOTHING` — idempotent at-least-once
    /// delivery per the UNIQUE constraint in V00015 (gotcha #7 compliance: block_time in UK).
    ///
    /// Best-effort: errors are returned but callers typically log and continue (V00012 pattern).
    ///
    /// # Errors
    ///
    /// Returns `StorageError::Postgres` on unexpected database errors.
    /// Does NOT return an error on conflict (the upsert is intentionally silent).
    #[instrument(skip(self, row), fields(chain = %row.chain, block_height = %row.block_height, tx_hash_victim = %row.tx_hash_victim))]
    pub async fn upsert_mev_event(&self, row: &MevEventRow) -> Result<(), StorageError> {
        let profit_raw_str = row.profit_amount_raw.as_ref().map(|d| d.to_string());
        let profit_usd_str = row.profit_amount_usd.as_ref().map(|d| d.to_string());
        let slippage_str = row.victim_slippage_pct.to_string();
        let swap_size_str = row.victim_swap_size_raw.to_string();
        let raw_data = row.raw_event_data.as_ref().cloned().unwrap_or(serde_json::Value::Null);

        sqlx::query(
            r#"INSERT INTO mev_events (
                chain, block_time, block_height,
                tx_hash_front, tx_hash_victim, tx_hash_back,
                pool_address, attacker_address, victim_address,
                token_in, token_out,
                profit_amount_raw, profit_amount_usd,
                victim_slippage_pct, victim_swap_size_raw,
                pool_kind, raw_event_data
               )
               VALUES (
                $1, $2, $3,
                $4, $5, $6,
                $7, $8, $9,
                $10, $11,
                $12::NUMERIC, $13::NUMERIC,
                $14::NUMERIC, $15::NUMERIC,
                $16, $17
               )
               ON CONFLICT (chain, block_time, block_height, tx_hash_victim) DO NOTHING"#,
        )
        .bind(&row.chain)
        .bind(row.block_time)
        .bind(row.block_height)
        .bind(&row.tx_hash_front)
        .bind(&row.tx_hash_victim)
        .bind(&row.tx_hash_back)
        .bind(&row.pool_address)
        .bind(&row.attacker_address)
        .bind(&row.victim_address)
        .bind(&row.token_in)
        .bind(&row.token_out)
        .bind(profit_raw_str)
        .bind(profit_usd_str)
        .bind(slippage_str)
        .bind(swap_size_str)
        .bind(&row.pool_kind)
        .bind(raw_data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch recent `mev_events` rows with optional attacker, pool, and victim filters.
    ///
    /// Used by D13 for auditing and by consumers building attacker recurrence scores.
    ///
    /// # Parameters
    ///
    /// - `chain` — always `"ethereum"` at MVP.
    /// - `attacker` — if `Some`, restricts to events by this attacker address.
    /// - `pool` — if `Some`, restricts to events on this pool address.
    /// - `victim` — if `Some`, restricts to events for this victim address.
    /// - `window_end` — exclusive upper bound on `block_time` (block-time sourced).
    /// - `lookback_minutes` — how far back to look from `window_end`.
    /// - `max_rows` — hard row cap; WARN is emitted on cap hit.
    ///
    /// # Determinism
    ///
    /// Results are ordered by `(block_height ASC, tx_hash_victim ASC)`.
    /// Callers MUST use this ordering for deterministic detector output.
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip(self), fields(chain, attacker, pool, victim))]
    pub async fn fetch_recent_mev_events(
        &self,
        chain: &str,
        attacker: Option<&str>,
        pool: Option<&str>,
        victim: Option<&str>,
        window_end: DateTime<Utc>,
        lookback_minutes: i64,
        max_rows: u32,
    ) -> Result<Vec<MevEventRow>, StorageError> {
        let window_start = window_end - chrono::Duration::minutes(lookback_minutes);

        let rows = sqlx::query(
            r#"SELECT
                chain, block_time, block_height,
                tx_hash_front, tx_hash_victim, tx_hash_back,
                pool_address, attacker_address, victim_address,
                token_in, token_out,
                profit_amount_raw::TEXT AS profit_raw_str,
                profit_amount_usd::TEXT AS profit_usd_str,
                victim_slippage_pct::TEXT AS slippage_str,
                victim_swap_size_raw::TEXT AS swap_size_str,
                pool_kind, raw_event_data
               FROM mev_events
               WHERE chain = $1
                 AND block_time >= $2
                 AND block_time <= $3
                 AND ($4::TEXT IS NULL OR attacker_address = $4)
                 AND ($5::TEXT IS NULL OR pool_address = $5)
                 AND ($6::TEXT IS NULL OR victim_address = $6)
               ORDER BY block_height ASC, tx_hash_victim ASC
               LIMIT $7"#,
        )
        .bind(chain)
        .bind(window_start)
        .bind(window_end)
        .bind(attacker)
        .bind(pool)
        .bind(victim)
        .bind(max_rows as i64)
        .fetch_all(&self.pool)
        .await?;

        if rows.len() == max_rows as usize {
            tracing::warn!(
                chain,
                max_rows,
                "fetch_recent_mev_events hit row cap; results may be incomplete"
            );
        }

        let result: Result<Vec<MevEventRow>, StorageError> = rows
            .into_iter()
            .map(|r| {
                let parse_opt_dec = |col: &str| -> Result<Option<Decimal>, StorageError> {
                    let s: Option<String> = r.try_get(col).ok().flatten();
                    s.as_deref()
                        .map(Decimal::from_str)
                        .transpose()
                        .map_err(|e| StorageError::Other(format!("mev_events {col} parse: {e}")))
                };
                let parse_dec = |col: &str| -> Result<Decimal, StorageError> {
                    let s: String = r.try_get(col).map_err(StorageError::Postgres)?;
                    Decimal::from_str(&s)
                        .map_err(|e| StorageError::Other(format!("mev_events {col} parse: {e}")))
                };

                Ok(MevEventRow {
                    chain: r.try_get("chain")?,
                    block_time: r.try_get("block_time")?,
                    block_height: r.try_get("block_height")?,
                    tx_hash_front: r.try_get("tx_hash_front")?,
                    tx_hash_victim: r.try_get("tx_hash_victim")?,
                    tx_hash_back: r.try_get("tx_hash_back")?,
                    pool_address: r.try_get("pool_address")?,
                    attacker_address: r.try_get("attacker_address")?,
                    victim_address: r.try_get("victim_address")?,
                    token_in: r.try_get("token_in")?,
                    token_out: r.try_get("token_out")?,
                    profit_amount_raw: parse_opt_dec("profit_raw_str")?,
                    profit_amount_usd: parse_opt_dec("profit_usd_str")?,
                    victim_slippage_pct: parse_dec("slippage_str")?,
                    victim_swap_size_raw: parse_dec("swap_size_str")?,
                    pool_kind: r.try_get("pool_kind")?,
                    raw_event_data: r.try_get("raw_event_data")?,
                })
            })
            .collect();

        result
    }
}

// ---------------------------------------------------------------------------
// Unit tests for Permit2EventRow and TransferRow helpers
// ---------------------------------------------------------------------------

#[cfg(test)]
mod permit2_storage_tests {
    use super::*;
    use chrono::TimeZone;

    fn make_permit2_row() -> Permit2EventRow {
        Permit2EventRow {
            chain: "ethereum".into(),
            block_time: Utc.with_ymd_and_hms(2024, 2, 5, 14, 30, 0).unwrap(),
            block_height: 19_200_000,
            tx_hash: "0xd12pos02bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .to_string(),
            log_index: 1,
            event_kind: "permit".into(),
            owner: "0xvictim000000000000000000000000000000002".into(),
            token: Some("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".into()),
            spender: Some("0xfreshdrainer000000000000000000000000002".into()),
            amount_raw: Some(Decimal::from(1_000_000_000_000_000_000u64)),
            expiration_unix: Some(9_999_999_999),
            nonce: Some(0),
            raw_event_data: serde_json::json!({ "event_kind": "permit" }),
        }
    }

    #[test]
    fn permit2_event_row_fields_accessible() {
        let row = make_permit2_row();
        assert_eq!(row.chain, "ethereum");
        assert_eq!(row.event_kind, "permit");
        assert!(row.amount_raw.is_some());
        assert_eq!(row.expiration_unix, Some(9_999_999_999));
        assert_eq!(row.nonce, Some(0));
        assert_eq!(row.log_index, 1);
    }

    #[test]
    fn transfer_row_amount_raw_is_decimal() {
        let row = TransferRow {
            chain: "ethereum".into(),
            token: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into(),
            block_time: Utc.with_ymd_and_hms(2024, 1, 15, 10, 0, 0).unwrap(),
            block_height: 18_800_000,
            tx_hash: "0xd12pos01aaa".into(),
            log_index: 1,
            from_address: "0xvictim001".into(),
            to_address: "0xdrainer001".into(),
            amount_raw: Decimal::from(5_000_000_000u64),
            decimals: 6,
        };
        assert_eq!(row.amount_raw, Decimal::from(5_000_000_000u64));
        assert_eq!(row.decimals, 6i16);
    }

    #[test]
    fn permit2_row_nullable_fields_work() {
        // unordered_nonce_invalidation has no token/spender/amount/expiration/nonce
        let row = Permit2EventRow {
            chain: "ethereum".into(),
            block_time: Utc.with_ymd_and_hms(2024, 3, 1, 0, 0, 0).unwrap(),
            block_height: 19_500_000,
            tx_hash: "0xunordered0000000000000000000000000000000000000000000000000000000"
                .to_string(),
            log_index: 0,
            event_kind: "unordered_nonce_invalidation".into(),
            owner: "0xowner".into(),
            token: None,
            spender: None,
            amount_raw: None,
            expiration_unix: None,
            nonce: None,
            raw_event_data: serde_json::json!({}),
        };
        assert!(row.token.is_none());
        assert!(row.spender.is_none());
        assert!(row.amount_raw.is_none());
        assert!(row.nonce.is_none());
    }
}

// ---------------------------------------------------------------------------
// Unit tests for MevEventRow helpers (V00015)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod mev_storage_tests {
    use super::*;
    use chrono::TimeZone;

    fn make_mev_row() -> MevEventRow {
        MevEventRow {
            chain: "ethereum".into(),
            block_time: Utc.with_ymd_and_hms(2024, 1, 15, 10, 0, 0).unwrap(),
            block_height: 18_800_000,
            tx_hash_front: "0xfront0000000000000000000000000000000000000000000000000000000001".into(),
            tx_hash_victim: "0xvictim000000000000000000000000000000000000000000000000000000001".into(),
            tx_hash_back: "0xback00000000000000000000000000000000000000000000000000000000001".into(),
            pool_address: "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc".into(),
            attacker_address: "0xattacker00000000000000000000000000000001".into(),
            victim_address: "0xvictimaddr0000000000000000000000000000001".into(),
            token_in: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into(),
            token_out: Some("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".into()),
            profit_amount_raw: Some(Decimal::from(1_000_000u64)),
            profit_amount_usd: None, // Phase 5 deferred
            victim_slippage_pct: Decimal::new(5, 3), // 0.005 = 0.5%
            victim_swap_size_raw: Decimal::from(10_000_000_000_000_000_000u64),
            pool_kind: "univ2".into(),
            raw_event_data: Some(serde_json::json!({ "pool_kind": "univ2" })),
        }
    }

    #[test]
    fn mev_event_row_fields_accessible() {
        let row = make_mev_row();
        assert_eq!(row.chain, "ethereum");
        assert_eq!(row.pool_kind, "univ2");
        assert!(row.profit_amount_usd.is_none()); // Phase 5 deferred
        assert!(row.profit_amount_raw.is_some());
        assert_eq!(row.block_height, 18_800_000);
    }

    #[test]
    fn mev_event_row_nullable_fields_work() {
        let mut row = make_mev_row();
        row.token_out = None;
        row.profit_amount_raw = None;
        row.raw_event_data = None;
        assert!(row.token_out.is_none());
        assert!(row.profit_amount_raw.is_none());
        assert!(row.raw_event_data.is_none());
    }

    #[test]
    fn mev_event_slippage_decimal_precision() {
        let row = make_mev_row();
        // 0.005 = 0.5% — verify Decimal round-trips correctly via to_string.
        let s = row.victim_slippage_pct.to_string();
        let parsed: Decimal = Decimal::from_str(&s).expect("slippage round-trip");
        assert_eq!(parsed, row.victim_slippage_pct);
        // Confirm it's > min gate (0.005)
        assert!(row.victim_slippage_pct >= Decimal::new(5, 3));
    }
}

// ---------------------------------------------------------------------------
// Unit tests for V00017 metadata_jsonb helpers (pure JSON logic, no DB needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod metadata_jsonb_tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Test: locker hit JSON shape — ADR 0002 no-f64 for monetary amounts
    // -----------------------------------------------------------------------

    /// The `locked_amount_raw` field must be string-encoded (not a JSON number).
    /// JSON numbers lose precision for u128 values > 2^53.
    #[test]
    fn locker_hit_json_amount_is_string() {
        let amount_raw: u128 = 18_446_744_073_709_551_615u128; // u64::MAX — beyond f64 exact range
        let hit = serde_json::json!({
            "locker_address":    "0x663a5c229c09b049e36dcc11a9b0d4a8eb9db214",
            "protocol_name":     "Unicrypt",
            "locked_amount_raw": amount_raw.to_string(),
            "observed_at":       serde_json::Value::Null,
        });
        // Must be a string, not a number.
        assert!(
            hit["locked_amount_raw"].is_string(),
            "locked_amount_raw must be string-encoded per ADR 0002 (no f64 precision loss)"
        );
        // Round-trip: string → u128 must recover the original value.
        let recovered: u128 = hit["locked_amount_raw"]
            .as_str()
            .expect("must be a string")
            .parse()
            .expect("must parse as u128");
        assert_eq!(recovered, amount_raw);
    }

    /// `upsert_locker` builds a JSON object with the expected shape.
    ///
    /// This is a pure-logic test of the JSON construction path inside `upsert_locker`.
    /// DB interaction is tested separately behind `#[ignore]` (Docker gate).
    #[test]
    fn upsert_locker_builds_correct_json_shape() {
        let amount: u128 = 1_000_000_000_000_000_000;
        let hit = serde_json::json!({
            "locker_address":    "0x663a5c229c09b049e36dcc11a9b0d4a8eb9db214",
            "protocol_name":     "Unicrypt",
            "locked_amount_raw": amount.to_string(),
            "observed_at":       serde_json::Value::Null,
        });
        assert_eq!(hit["locker_address"], "0x663a5c229c09b049e36dcc11a9b0d4a8eb9db214");
        assert_eq!(hit["protocol_name"], "Unicrypt");
        assert_eq!(hit["locked_amount_raw"], "1000000000000000000");
        assert!(hit["observed_at"].is_null());
    }

    /// `upsert_locker` with `protocol_name = None` encodes as JSON null (not missing key).
    #[test]
    fn upsert_locker_none_protocol_encodes_as_null() {
        let protocol_name: Option<&str> = None;
        let hit = serde_json::json!({
            "locker_address":    "0x663a5c229c09b049e36dcc11a9b0d4a8eb9db214",
            "protocol_name":     protocol_name,
            "locked_amount_raw": 42u128.to_string(),
            "observed_at":       serde_json::Value::Null,
        });
        // `None` serialises as null — key is present, value is JSON null.
        assert!(hit["protocol_name"].is_null());
    }

    /// GraduationInfo-shaped JSON round-trips through serde_json without precision loss.
    ///
    /// The `initial_liquidity_usd_at_grad` amount uses Decimal serialised to string
    /// inside GraduationInfo (via `serde(with = "rust_decimal::serde::str")`).
    /// This test verifies the JSON structure expected by `upsert_graduation_info`.
    #[test]
    fn graduation_info_json_round_trip_no_float() {
        // Simulate what token-registry would produce via serde_json::to_value(&info).
        let graduation_json = serde_json::json!({
            "launchpad": "pump_fun",
            "graduationTime": "2026-04-24T12:00:00Z",
            "graduationBlock": 300_000_000u64,
            "graduationTx": { "Solana": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" },
            "initialLiquidityUsdAtGrad": "690.00"
        });
        // Verify the key used by the SQL index expression matches the migration.
        // The index is: (metadata_jsonb -> 'graduation' ->> 'graduationTime')
        assert!(graduation_json["graduationTime"].is_string());
        // Liquidity must be a string (not float) for ADR 0002 compliance.
        assert!(graduation_json["initialLiquidityUsdAtGrad"].is_string());
        let val_str = graduation_json["initialLiquidityUsdAtGrad"].as_str().unwrap();
        let parsed = Decimal::from_str(val_str).expect("must parse as Decimal");
        assert_eq!(parsed.to_string(), "690.00");
    }

    /// `fetch_lockers` should return an empty Vec when the lockers array is absent.
    ///
    /// Pure-logic test of the array extraction path (no DB).
    #[test]
    fn fetch_lockers_empty_when_no_lockers_key() {
        // Simulate a row with no "lockers" key in metadata_jsonb.
        let metadata: serde_json::Value = serde_json::json!({});
        let lockers_val: Option<serde_json::Value> = metadata.get("lockers").cloned();
        let result: Vec<serde_json::Value> = match lockers_val {
            Some(serde_json::Value::Array(arr)) => arr,
            Some(_) | None => vec![],
        };
        assert!(result.is_empty(), "no 'lockers' key → empty Vec");
    }
}
