-- =============================================================================
-- V00006__auth_users.sql — Auth user store for the gateway
-- =============================================================================
-- Tables:
--   auth_users  — API credential store (Argon2id hashed passwords, scopes)
--   auth_tokens — JWT audit log (NOT a revocation list; stateless JWTs in MVP)
--
-- Design: ADR 0003 (self-sovereign — no Auth0/Clerk/Cognito).
--         Design 0011 §5.4.
-- =============================================================================

CREATE TABLE IF NOT EXISTS auth_users (
    id              BIGSERIAL       PRIMARY KEY,
    username        TEXT            NOT NULL UNIQUE,
    -- Argon2id hash — format: $argon2id$v=19$m=65536,t=3,p=4$<salt>$<hash>
    password_hash   TEXT            NOT NULL,
    scopes          TEXT[]          NOT NULL DEFAULT '{}',
    enabled         BOOLEAN         NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMPTZ     NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ     NOT NULL DEFAULT NOW(),
    last_login_at   TIMESTAMPTZ
);

-- GIN index for scopes array queries: WHERE 'admin' = ANY(scopes)
CREATE INDEX IF NOT EXISTS idx_auth_users_scopes ON auth_users USING GIN (scopes);

-- ---------------------------------------------------------------------------
-- auth_tokens — JWT audit log
-- ---------------------------------------------------------------------------
-- Stores issued JWTs for audit trail. NOT a revocation list (Phase 6 concern).
-- Cleanup: DELETE WHERE expires_at < NOW() - INTERVAL '7 days' (background job).
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS auth_tokens (
    jti             UUID            PRIMARY KEY,
    subject         TEXT            NOT NULL,
    issued_at       TIMESTAMPTZ     NOT NULL,
    expires_at      TIMESTAMPTZ     NOT NULL,
    scopes          TEXT[]          NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS auth_tokens_subject_idx ON auth_tokens (subject);
CREATE INDEX IF NOT EXISTS auth_tokens_expires_idx ON auth_tokens (expires_at);
