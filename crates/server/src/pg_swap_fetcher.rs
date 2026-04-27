//! `PgSwapFetcher` — production `SwapFetcher` implementation backed by `sqlx::PgPool`.
//!
//! # Why here (not in crates/graph or crates/storage)?
//!
//! `SwapFetcher` is defined in `crates/graph::smart_money`. The production
//! implementation needs to query the `swaps` and `anomaly_events` tables, which
//! are owned by `crates/storage`. Placing it here (crates/server) keeps the
//! dependency direction clean:
//!
//! ```text
//! server → graph → storage → common   (existing direction)
//! server → storage                     (existing direct dep)
//! ```
//!
//! Adding SQL queries to `crates/graph` would make graph a production SQL crate,
//! which conflicts with the design intent (graph should be schema-agnostic for
//! testability). Placing in `crates/server` is consistent with how other
//! production-SQL adapters are wired (e.g. `PgD10AnomalyEventSink` in `main.rs`).
//!
//! # Amount encoding
//!
//! `swaps.amount_out` (for buys) and `swaps.amount_in` (for sells) are NUMERIC
//! columns. They are read as TEXT and parsed to Decimal (string-bridge per ADR 0002).
//! `decimals` from the `tokens` table is used to divide raw amounts to decimal-adjusted.
//!
//! For MVP (Sprint 22), if `decimals` is unavailable (token not in registry), the
//! raw amount is used as-is with a SPEC-NOTE. This is consistent with the S21 sprint
//! "decimals defaults" open item (gotcha #87).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::str::FromStr;
use tracing::instrument;

use mg_onchain_common::chain::Chain;
use mg_onchain_graph::smart_money::{PumpEvent, SwapFetcher, SwapRow, SwapSide};

/// Production `SwapFetcher` backed by a shared `sqlx::PgPool`.
pub struct PgSwapFetcher {
    pool: sqlx::PgPool,
}

impl PgSwapFetcher {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SwapFetcher for PgSwapFetcher {
    #[instrument(skip(self), fields(%chain, wallet, %since, %until))]
    async fn fetch_swaps_for_wallet(
        &self,
        chain: Chain,
        wallet: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> anyhow::Result<Vec<SwapRow>> {
        use sqlx::Row as _;

        // SPEC-NOTE (gotcha #87): decimals defaults to 9 (Solana SPL) or 18 (EVM).
        // Exact decimals fetching from the `tokens` table is a Sprint 23 enhancement.
        let default_decimals: u32 = match chain {
            Chain::Ethereum | Chain::Bsc | Chain::Base | Chain::Arbitrum | Chain::Polygon => 18,
            _ => 9, // Solana SPL default
        };
        let divisor = Decimal::from(10u64.saturating_pow(default_decimals));

        let rows = sqlx::query(
            r#"
            SELECT
                sender AS wallet,
                token_out AS token,
                'buy'::TEXT AS side,
                amount_out::TEXT AS token_qty_raw,
                block_time,
                block_height,
                tx_hash
            FROM swaps
            WHERE chain = $1
              AND sender = $2
              AND block_time >= $3
              AND block_time <= $4
              AND side = 'buy'

            UNION ALL

            SELECT
                sender AS wallet,
                token_in AS token,
                'sell'::TEXT AS side,
                amount_in::TEXT AS token_qty_raw,
                block_time,
                block_height,
                tx_hash
            FROM swaps
            WHERE chain = $1
              AND sender = $2
              AND block_time >= $3
              AND block_time <= $4
              AND side = 'sell'

            ORDER BY block_time ASC, tx_hash ASC
            LIMIT 10000
            "#,
        )
        .bind(chain.as_str())
        .bind(wallet)
        .bind(since)
        .bind(until)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("fetch_swaps_for_wallet DB error: {e}"))?;

        let mut result = Vec::with_capacity(rows.len());
        for row in &rows {
            let wallet_addr: String = row.try_get("wallet")?;
            let token: String = row.try_get("token")?;
            let side_str: String = row.try_get("side")?;
            let qty_raw_str: String = row.try_get("token_qty_raw")?;
            let block_time: DateTime<Utc> = row.try_get("block_time")?;
            let block_height: i64 = row.try_get("block_height")?;
            let tx_hash: String = row.try_get("tx_hash")?;

            let side = if side_str == "buy" {
                SwapSide::Buy
            } else {
                SwapSide::Sell
            };

            // String-bridge: parse NUMERIC raw amount, then decimal-adjust.
            let raw = Decimal::from_str(&qty_raw_str)
                .unwrap_or(Decimal::ZERO);
            let token_qty = if divisor.is_zero() { raw } else { raw / divisor };

            result.push(SwapRow {
                wallet: wallet_addr,
                token,
                side,
                token_qty,
                block_time,
                block_height,
                tx_hash,
            });
        }

        Ok(result)
    }

    #[instrument(skip(self), fields(%chain, %since, %until))]
    async fn fetch_active_wallets(
        &self,
        chain: Chain,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"
            SELECT DISTINCT sender AS wallet
            FROM swaps
            WHERE chain = $1
              AND block_time >= $2
              AND block_time <= $3
            ORDER BY wallet
            LIMIT 100000
            "#,
        )
        .bind(chain.as_str())
        .bind(since)
        .bind(until)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("fetch_active_wallets DB error: {e}"))?;

        Ok(rows.into_iter().map(|(w,)| w).collect())
    }

    #[instrument(skip(self), fields(%chain, %since, min_confidence))]
    async fn fetch_pump_events(
        &self,
        chain: Chain,
        since: DateTime<Utc>,
        min_confidence: f64,
    ) -> anyhow::Result<Vec<PumpEvent>> {
        use sqlx::Row as _;

        let rows = sqlx::query(
            r#"
            SELECT token, observed_at AS event_peak_time, confidence
            FROM anomaly_events
            WHERE chain = $1
              AND detector_id = 'pump_dump_v1'
              AND confidence >= $2
              AND observed_at >= $3
            ORDER BY observed_at ASC
            LIMIT 10000
            "#,
        )
        .bind(chain.as_str())
        .bind(min_confidence)
        .bind(since)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("fetch_pump_events DB error: {e}"))?;

        let mut events = Vec::with_capacity(rows.len());
        for row in &rows {
            let token: String = row.try_get("token")?;
            let event_peak_time: DateTime<Utc> = row.try_get("event_peak_time")?;
            let confidence: f64 = row.try_get("confidence")?;
            events.push(PumpEvent {
                token,
                event_peak_time,
                confidence,
            });
        }

        Ok(events)
    }

    #[instrument(skip(self), fields(%chain))]
    async fn fetch_excluded_wallets(&self, chain: Chain) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"
            SELECT DISTINCT address
            FROM address_labels
            WHERE chain = $1
              AND label_type IN ('KnownExchange', 'KnownDex', 'KnownBurn')
              AND (expires_at IS NULL OR expires_at > now())
            ORDER BY address
            "#,
        )
        .bind(chain.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("fetch_excluded_wallets DB error: {e}"))?;

        Ok(rows.into_iter().map(|(a,)| a).collect())
    }

    #[instrument(skip(self), fields(%chain, exclusion_confidence))]
    async fn fetch_wash_trading_excluded(
        &self,
        chain: Chain,
        exclusion_confidence: f64,
    ) -> anyhow::Result<Vec<String>> {
        // Fetch distinct wallets (token field serves as "sender" in the swaps table context).
        // In anomaly_events, the `token` column is the mint — not the wallet. We need
        // to join with swaps to get wallets. For MVP, use a simplified approach:
        // fetch all tokens flagged as wash trading, then exclude any wallet that traded
        // those tokens within the last 30 days.
        //
        // SPEC-NOTE: A full implementation would cross-reference with the wash_trading_v1
        // evidence JSON to extract specific wallet addresses. For Sprint 22, we exclude
        // wallets that had ANY swap on a wash-trading-flagged token.
        // TODO(sprint-23): parse wash_trading_v1 evidence["wash_trading_h1/round_trip_senders"]
        // to get exact wallet addresses for finer-grained exclusion.
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"
            SELECT DISTINCT s.sender
            FROM swaps s
            INNER JOIN anomaly_events ae
                ON ae.chain = s.chain
               AND ae.token = s.token_out
               AND ae.detector_id = 'wash_trading_v1'
               AND ae.confidence >= $2
               AND ae.observed_at >= now() - INTERVAL '30 days'
            WHERE s.chain = $1
            ORDER BY s.sender
            LIMIT 10000
            "#,
        )
        .bind(chain.as_str())
        .bind(exclusion_confidence)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("fetch_wash_trading_excluded DB error: {e}"))?;

        Ok(rows.into_iter().map(|(a,)| a).collect())
    }
}
