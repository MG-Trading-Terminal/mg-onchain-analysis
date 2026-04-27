-- V00008: Token-2022 marker extension flags
--
-- Adds two boolean columns for Token-2022 extensions that require special
-- detector handling:
--
--   non_transferable    (ext discriminator 9): marker extension, no payload.
--                       Structurally prevents any on-chain transfer.
--                       D01 S1 freeze-authority weight attenuated to 0.10.
--                       D05 returns InsufficientBaseline.
--
--   confidential_transfer (ext discriminator 4): ConfidentialTransferMint.
--                       Transfer amounts are ZK-encrypted on-chain (opaque
--                       ciphertexts).  D05 cannot operate — returns
--                       InsufficientBaseline, not silent BELOW.
--
-- Both columns default FALSE so existing rows are unaffected.

ALTER TABLE tokens
    ADD COLUMN IF NOT EXISTS non_transferable     BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN IF NOT EXISTS confidential_transfer BOOLEAN NOT NULL DEFAULT FALSE;

COMMENT ON COLUMN tokens.non_transferable     IS
    'Token-2022 NonTransferable extension (discriminator 9) detected on-chain.';
COMMENT ON COLUMN tokens.confidential_transfer IS
    'Token-2022 ConfidentialTransferMint extension (discriminator 4) detected on-chain.';
