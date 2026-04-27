-- =============================================================================
-- V00004__token_extensions.sql — Token-2022 extension sidecar columns
-- =============================================================================
-- Migration: sqlx migrate (V-prefix Flyway naming; runtime Migrator).
-- Apply: sqlx migrate run --database-url $DATABASE_URL
--
-- Purpose: Extends the `tokens` table with two nullable TEXT columns that
--   store Token-2022 extension data. These are populated by the enrichment
--   path in `crates/token-registry/src/enrich.rs` when a Token-2022 mint's
--   TLV extension bytes are decoded.
--
-- Pre-authorised in P2-5 task brief (DG2):
--   "Storage: add V00004__token_extensions.sql — two TEXT columns on tokens
--    table (permanent_delegate, transfer_hook_program). Update PgStore::upsert_token."
--
-- Column semantics:
--   permanent_delegate   — Base58 address of the PermanentDelegate authority, if any.
--                          Token-2022 extension discriminator: 12 (0x0C).
--                          Reference: https://github.com/solana-program/token-2022/blob/main/program/src/extension/mod.rs
--                          When set, this authority can transfer or burn any holder's
--                          tokens without consent — a major scam vector (S3 signal in D01).
--
--   transfer_hook_program — Base58 address of the TransferHook program, if any.
--                           Token-2022 extension discriminator: 14 (0x0E).
--                           Reference: same as above.
--                           When set, this program is invoked on every token transfer
--                           and can revert the tx (S4 signal in D01).
--
-- Both columns are NULL for:
--   - Standard SPL tokens (no Token-2022 extensions).
--   - EVM tokens (Phase 4).
--   - Token-2022 tokens not yet enriched (Phase 3+ when TLV decoder ships).
--
-- Design note: additive, nullable columns — SemVer-safe for all consumers.
-- =============================================================================

ALTER TABLE tokens
    ADD COLUMN IF NOT EXISTS permanent_delegate    TEXT DEFAULT NULL,
    ADD COLUMN IF NOT EXISTS transfer_hook_program TEXT DEFAULT NULL;

COMMENT ON COLUMN tokens.permanent_delegate IS
    'Token-2022 PermanentDelegate extension authority address (Base58). NULL for standard SPL or unenriched tokens.';

COMMENT ON COLUMN tokens.transfer_hook_program IS
    'Token-2022 TransferHook extension program address (Base58). NULL for standard SPL or unenriched tokens.';

-- Index for fast lookup of tokens with these extensions (used by monitoring and fixture bootstrapping).
CREATE INDEX IF NOT EXISTS idx_tokens_permanent_delegate
    ON tokens (permanent_delegate)
    WHERE permanent_delegate IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_tokens_transfer_hook_program
    ON tokens (transfer_hook_program)
    WHERE transfer_hook_program IS NOT NULL;
