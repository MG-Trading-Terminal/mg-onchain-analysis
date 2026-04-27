-- =============================================================================
-- V00005__anomaly_events_id.sql — Add surrogate id to anomaly_events
-- =============================================================================
-- The gateway's GET /v1/anomaly_events cursor uses (observed_at, id) for stable
-- keyset pagination. This migration adds a BIGSERIAL id column to the partitioned
-- parent table.
--
-- IMPORTANT: Adding a column to a partitioned table in Postgres 16 propagates
-- automatically to all child partitions.
-- =============================================================================

ALTER TABLE anomaly_events
    ADD COLUMN IF NOT EXISTS id BIGSERIAL;

-- Index to support cursor-based keyset pagination:
-- WHERE (observed_at, id) < ($cursor_ts, $cursor_id)
-- ORDER BY observed_at DESC, id DESC
CREATE INDEX IF NOT EXISTS idx_anomaly_events_cursor
    ON anomaly_events (observed_at DESC, id DESC);
