-- D07 Withdraw-Withheld Drain Detector — three queries: W1, W2, W3.
--
-- What it computes:
--   W1: Fetch all WithdrawWithheld* instruction events for a mint within
--       the detection window. Used to evaluate Signal A (active extraction).
--
--   W2: Fetch SetAuthority(WithdrawWithheldTokens) instruction history for
--       a mint within the authority rotation lookback window. Used to evaluate
--       Signal B (authority rotation alert).
--
--   W3: Aggregate cumulative extraction metrics (sum of raw amounts, sum of
--       USD amounts, event count) from W1 rows. Implemented as a CTE over W1
--       to avoid a second table scan.
--
-- Storage dependency:
--   All queries operate against `token2022_instructions` (Migration V00007).
--   This table is populated by the chain-adapter Token-2022 instruction decoder.
--   If the table is empty for the queried (chain, mint) combination, D07 returns
--   DetectorError::MissingDependencyData — not a query failure.
--
-- Research sources:
--   E-D02-11: docs/reviews/0002-d02-rug-pull-evasions.md §E-D02-11
--     Token-2022 withdraw_withheld as non-LP drain path; original gap analysis.
--   D06 §10 coverage matrix: docs/designs/0009-detector-06-mint-burn.md §10
--     Formal confirmation that D06 does not cover withdraw_withheld extraction.
--   Solana Token-2022 extension docs:
--     https://spl.solana.com/token-2022/extensions#transfer-fees
--   Sun et al. 2024 "Hidden Fee" (category 7/34):
--     https://arxiv.org/abs/2403.16082
--   SPL Token-2022 instruction discriminators (byte offsets 27-29):
--     https://github.com/solana-labs/solana-program-library/blob/master/token/program-2022/src/instruction.rs
--
-- PostgreSQL dialect (ADR 0002). No ClickHouse equivalent — token2022_instructions
-- is a Postgres-only table (low event volume; not time-series analytics).
--
-- Index used by all three queries:
--   idx_t22_instructions_chain_mint_time ON token2022_instructions(chain, mint, block_time)
--
-- ============================================================================
-- QUERY W1 — Fetch WithdrawWithheld* extraction events in detection window
-- ============================================================================
--
-- Parameter mapping (sqlx positional):
--   $1  chain          TEXT        — e.g. 'solana'
--   $2  mint           TEXT        — token mint address (Base58 / checksummed hex)
--   $3  window_start   TIMESTAMPTZ — ctx.window.start (detection_window_hours before window_end)
--   $4  window_end     TIMESTAMPTZ — ctx.window.end
--
-- Returns one row per WithdrawWithheld* instruction within the window.
-- instruction_kind IN ('withdraw_withheld_from_accounts', 'withdraw_withheld_from_mint')
-- HarvestWithheldTokensToMint rows are excluded: they are permissionless and do not
-- represent attacker-controlled extraction.
--
-- Caller (d07_withdraw_withheld.rs) checks row count vs min_extraction_events
-- and calls Query W3 for the aggregated USD total.

SELECT
    id,
    tx_hash,
    block_height,
    block_time,
    instruction_kind,
    authority,
    destination,
    amount_raw,
    amount_usd
FROM token2022_instructions
WHERE chain             = $1
  AND mint              = $2
  AND block_time       >= $3
  AND block_time       <  $4
  AND instruction_kind IN (
        'withdraw_withheld_from_accounts',
        'withdraw_withheld_from_mint'
      )
ORDER BY block_time ASC;

-- ============================================================================
-- QUERY W2 — Fetch withdraw_withheld authority rotation history
-- ============================================================================
--
-- Parameter mapping (sqlx positional):
--   $1  chain            TEXT        — e.g. 'solana'
--   $2  mint             TEXT        — token mint address
--   $3  lookback_start   TIMESTAMPTZ — window_end - authority_rotation_window_days
--   $4  window_end       TIMESTAMPTZ — ctx.window.end
--
-- Returns all SetAuthority(WithdrawWithheldTokens) rows within the lookback window.
-- The LEFT JOIN against wallet_funding_events provides the new authority's first
-- SOL receipt timestamp for the fresh-wallet check. If wallet_funding_events does
-- not exist or has no row for the new authority, new_authority_first_sol_time = NULL.
--
-- Caller (d07_withdraw_withheld.rs) uses:
--   - prev_authority + rotation block_time + new_authority to compute tenure
--   - new_authority_first_sol_time to compute fresh_wallet_funding delta
--   - withheld_at_rotation_usd (if populated by indexer) for Signal B bonus

SELECT
    ti.tx_hash                                          AS rotation_tx_hash,
    ti.block_height                                     AS rotation_block_height,
    ti.block_time                                       AS rotation_block_time,
    ti.prev_authority,
    ti.new_authority,
    -- Amount of fees accumulated in the mint's withheld_amount at the time of
    -- rotation. Populated by the indexer from the mint account pre-execution state.
    -- NULL if the indexer did not capture this (acceptable for MVP).
    ti.amount_usd                                       AS withheld_at_rotation_usd,
    -- First SOL receipt time for the new authority wallet.
    -- NULL if wallet_funding_events table is absent or has no record.
    wf.first_sol_time                                   AS new_authority_first_sol_time
FROM token2022_instructions ti
LEFT JOIN LATERAL (
    -- wallet_funding_events: tracks the first SOL receipt for each wallet.
    -- Table: (wallet TEXT, chain TEXT, first_sol_time TIMESTAMPTZ)
    -- If this table does not exist, the entire query fails at parse time.
    -- The developer MUST create a stub for wallet_funding_events in V00007 or a
    -- companion migration, even if it is initially empty.
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
ORDER BY ti.block_time ASC;

-- ============================================================================
-- QUERY W3 — Aggregate cumulative extraction metrics (CTE over W1)
-- ============================================================================
--
-- Parameter mapping (sqlx positional): identical to W1 ($1–$4).
--
-- Returns a single row with:
--   event_count       — total count of WithdrawWithheld* instructions in window
--   cumulative_raw    — SUM of amount_raw (raw token units); NULL if no rows
--   cumulative_usd    — SUM of amount_usd (USD decimal); NULL if all amount_usd = NULL
--
-- NULL cumulative_usd indicates the indexer had no price data for the extraction
-- events. The caller must fall back to event_count-only evaluation in this case
-- and emit cumulative_withdrawn_usd = "0" with evidence note "price_data_unavailable".
--
-- This query is a CTE re-implementation of W1's filter, not a separate table scan.
-- In production the ORM layer may combine W1 + W3 into a single query with a
-- CTE to avoid two identical index lookups; the separation here is for clarity.

WITH extraction_events AS (
    SELECT
        amount_raw,
        amount_usd
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
    COUNT(*)            AS event_count,
    SUM(amount_raw)     AS cumulative_raw,
    -- SUM returns NULL when all amount_usd are NULL (price unavailable).
    -- Caller checks for NULL and falls back to event-count-only gate.
    SUM(amount_usd)     AS cumulative_usd
FROM extraction_events;

-- ============================================================================
-- SUPPLEMENTARY: pool_volume_usd for established-protocol ratio check
-- ============================================================================
--
-- This query is NOT a named detector query (W1/W2/W3) — it is an auxiliary
-- lookup used only when is_established_protocol(meta) = true AND cumulative_usd
-- is available. It computes the total pool swap volume in USD for the token
-- within the detection window, sourced from the `swaps` table (ClickHouse in
-- production; Postgres for testing).
--
-- Parameter mapping (sqlx positional):
--   $1  chain          TEXT
--   $2  token          TEXT        — token mint address (matches swaps.token_in or token_out)
--   $3  window_start   TIMESTAMPTZ
--   $4  window_end     TIMESTAMPTZ
--
-- NOTE: This query targets the `swaps` table which may be in ClickHouse in
-- production (ADR 0002 Postgres-only for metadata; ClickHouse for time-series
-- events). The developer MUST reconcile this with the storage tier selection:
-- if swaps are in Postgres for MVP, use this query directly; if in ClickHouse,
-- the detector must make a separate ClickHouse call or accept NULL pool_volume
-- (skipping the established-protocol ratio check per failure mode §13).
--
-- For MVP, the developer may stub this query to return NULL (0 pool volume)
-- and skip the ratio check entirely, documenting it as DG-D07-2 workaround.

SELECT
    COALESCE(SUM(usd_value), 0.0)  AS pool_volume_usd
FROM swaps
WHERE chain      = $1
  AND (token_in  = $2 OR token_out = $2)
  AND block_time >= $3
  AND block_time <  $4;
