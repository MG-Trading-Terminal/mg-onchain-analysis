-- =============================================================================
-- V00019__oak_technique_id.sql — Add OAK technique ID to anomaly_events
-- =============================================================================
-- Each anomaly event can optionally carry an OAK Technique ID (e.g. "OAK-T1.006")
-- that maps the detector's finding to the OAK threat taxonomy.
-- =============================================================================

ALTER TABLE anomaly_events
    ADD COLUMN IF NOT EXISTS oak_technique_id TEXT;
