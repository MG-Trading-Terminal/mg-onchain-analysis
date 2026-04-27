-- =============================================================================
-- V00010__streaming_emitted_by.sql — Add provenance tag to anomaly_events
-- =============================================================================
-- Migration tool: sqlx migrate (sqlx-cli).
-- Apply: `sqlx migrate run --database-url $DATABASE_URL`
--        or via `StorageConfig.migrations_auto_apply = true` at service startup.
--
-- Purpose:
--   Distinguishes streaming-scheduler-emitted events from API-request-emitted
--   events.  Placement decision (design 0014 §2.5 option (c)):
--   schema-only addition — no Rust type change — smallest blast radius.
--   crates/common is FROZEN; AnomalyEvent is not touched.
--
-- Values:
--   'api_request'          — emitted by POST /v1/tokens/analyze (on-demand)
--   'streaming_scheduler'  — emitted by DetectorScheduler workers (Phase 2+)
--
-- The DEFAULT 'api_request' covers all existing rows without a backfill.
--
-- IMPORTANT: anomaly_events is a partitioned table (by chain, per V00002).
-- Postgres 16 propagates ADD COLUMN automatically to all child partitions.
-- =============================================================================

ALTER TABLE anomaly_events
    ADD COLUMN IF NOT EXISTS emitted_by TEXT NOT NULL DEFAULT 'api_request';

CREATE INDEX IF NOT EXISTS ix_anomaly_events_emitted_by
    ON anomaly_events (emitted_by);
