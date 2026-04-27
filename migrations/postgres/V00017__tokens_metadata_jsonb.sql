-- V00017: tokens.metadata_jsonb — forward-only enrichment storage
--
-- Adds a JSONB column to the `tokens` table for flexible phase-5 enrichment data
-- that does not warrant dedicated columns (low cardinality, rarely queried by
-- relational predicates, evolving schema).
--
-- Schema convention (JSON object keys):
--   {
--     "graduation": <GraduationInfo>,  -- see crates/token-registry/src/graduation.rs
--     "lockers":    [<LockerHit>, ...]  -- LP locker transfer records
--   }
--
-- Design decisions:
--   1. JSONB NOT NULL DEFAULT '{}' — always present, never NULL; empty object when
--      no enrichment has occurred. This avoids NULL checks in all read paths.
--   2. GEN_RANDOM_UUID() is NOT used — this column is pure JSONB enrichment, not identity.
--   3. Two partial indexes for the two known access patterns:
--      - graduation time range scans (D10 launch-within-N-hours gate)
--      - locker presence check (D02 Signal B: locked LP reduces rug-pull confidence)
--   4. No jsonb_path_ops GIN index: access patterns are top-level key existence (?)
--      and single nested field extraction (->>). btree on expression is cheaper.
--
-- Reference: crates/token-registry/src/graduation.rs, crates/server/src/init/locker_watcher.rs
-- Sprint: 44 (Track 1)

ALTER TABLE tokens
    ADD COLUMN IF NOT EXISTS metadata_jsonb JSONB NOT NULL DEFAULT '{}'::jsonb;

-- Partial index for graduation time range queries.
-- Expression index on the ISO-8601 timestamp string inside the graduation object.
-- Partial (WHERE clause) ensures the index covers only rows that actually have
-- graduation data — token universe without graduation data pays no index overhead.
--
-- Query pattern: WHERE (metadata_jsonb -> 'graduation' ->> 'graduationTime') > $since
CREATE INDEX IF NOT EXISTS idx_tokens_graduation_time
    ON tokens ((metadata_jsonb -> 'graduation' ->> 'graduationTime'))
    WHERE metadata_jsonb ? 'graduation';

-- Partial index for locker presence.
-- Used by D02 Signal B: "does this token have any locked LP?"
-- Expression index on the array non-empty check is cheaper than a GIN scan.
--
-- Query pattern: WHERE metadata_jsonb ? 'lockers' (presence check)
CREATE INDEX IF NOT EXISTS idx_tokens_has_lockers
    ON tokens ((metadata_jsonb ? 'lockers'))
    WHERE metadata_jsonb ? 'lockers';

COMMENT ON COLUMN tokens.metadata_jsonb IS
    'Phase 5 forward-only enrichment storage (V00017, Sprint 44). '
    'Never NULL: defaults to empty object {}. '
    'Schema: { '
    '  graduation: GraduationInfo (crates/token-registry/src/graduation.rs), '
    '  lockers: [LockerHit, ...] (crates/server/src/init/locker_watcher.rs) '
    '}. '
    'Monetary amounts encoded as decimal strings per ADR 0002 (no f64). '
    'Use PgStore::upsert_graduation_info / upsert_locker_hit / fetch_graduation_info / fetch_lockers.';
