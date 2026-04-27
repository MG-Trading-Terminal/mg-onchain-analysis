-- =============================================================================
-- V00007__token2022_instructions.sql — Token-2022 instruction event store
-- =============================================================================
-- Tables:
--   token2022_instructions  — decoded WithdrawWithheld*, HarvestWithheld,
--                             and SetAuthority(WithdrawWithheldTokens) instructions
--   wallet_funding_events   — first SOL receipt per wallet (Signal B fresh-wallet check)
--
-- Design: docs/designs/0012-detector-07-withdraw-withheld.md §11
-- ADR 0002: Postgres-only. No ClickHouse equivalent — instruction events are
--           low-volume (one row per instruction, not per token account transfer).
-- =============================================================================

CREATE TABLE IF NOT EXISTS token2022_instructions (
    id               BIGSERIAL       PRIMARY KEY,
    chain            TEXT            NOT NULL,
    mint             TEXT            NOT NULL,
    tx_hash          TEXT            NOT NULL,
    block_height     BIGINT          NOT NULL,
    block_time       TIMESTAMPTZ     NOT NULL,
    -- 'withdraw_withheld_from_accounts' | 'withdraw_withheld_from_mint'
    -- | 'harvest_withheld_to_mint' | 'set_authority_withdraw_withheld'
    instruction_kind TEXT            NOT NULL CHECK (instruction_kind IN (
        'withdraw_withheld_from_accounts',
        'withdraw_withheld_from_mint',
        'harvest_withheld_to_mint',
        'set_authority_withdraw_withheld'
    )),
    -- signer for withdraw/set_authority; NULL for harvest (permissionless)
    authority        TEXT,
    -- destination token account for withdraw instructions; NULL otherwise
    destination      TEXT,
    -- token units extracted; NULL for set_authority instructions
    amount_raw       NUMERIC,
    -- USD value at block_time from indexer price feed; NULL if no price
    amount_usd       NUMERIC,
    -- populated for set_authority_withdraw_withheld: new authority pubkey
    -- or NULL if authority was revoked
    new_authority    TEXT,
    -- previous authority pubkey; populated for set_authority_withdraw_withheld
    prev_authority   TEXT,
    -- instruction index within the transaction (outer_idx * 1000 + inner_idx for CPI)
    log_index        INT             NOT NULL,
    ingested_at      TIMESTAMPTZ     NOT NULL DEFAULT now(),
    CONSTRAINT token2022_instructions_uniq UNIQUE (chain, tx_hash, log_index)
);

-- Primary query index: W1, W2, W3 all filter on (chain, mint, block_time)
CREATE INDEX IF NOT EXISTS idx_t22_instructions_chain_mint_time
    ON token2022_instructions (chain, mint, block_time);

-- Secondary index for kind-based scans (monitoring dashboards, admin queries)
CREATE INDEX IF NOT EXISTS idx_t22_instructions_kind
    ON token2022_instructions (instruction_kind, block_time);

-- ---------------------------------------------------------------------------
-- wallet_funding_events — first SOL receipt tracking for Signal B fresh-wallet check
-- ---------------------------------------------------------------------------
-- Populated by the indexer when it observes a wallet's first SOL receipt.
-- A wallet that received its first SOL within fresh_wallet_funding_hours before
-- being set as withdraw_withheld_authority is classified as a disposable key
-- (per docs/designs/0012-detector-07-withdraw-withheld.md §6 Signal B).
--
-- If the indexer has not populated this table, D07 Signal B fires at base
-- confidence (0.40) without the fresh_wallet_bonus.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS wallet_funding_events (
    chain            TEXT            NOT NULL,
    wallet           TEXT            NOT NULL,
    first_sol_time   TIMESTAMPTZ     NOT NULL,
    first_sol_tx     TEXT,
    PRIMARY KEY (chain, wallet)
);
