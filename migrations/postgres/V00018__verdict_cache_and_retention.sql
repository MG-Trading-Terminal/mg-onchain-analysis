-- V00018: verdict_cache + drop never-populated bulk event tables
--
-- Sprint 26 (T26-5) under ADR 0007 (Pull-Based Query Engine).
-- Adds the on-demand verdict cache used by the indexer + gateway under the new
-- pull-based model. Drops bulk event tables created in V00002 under the old
-- continuous-streaming model: `transfers`, `swaps`, `pool_events`,
-- `holder_snapshots_history`. Each drop is guarded with a row-count check that
-- fails loudly if the table contains production data — in that case the drop
-- is deferred to V00019 with explicit operator sign-off (design 0028 §11.4).
--
-- Tables retained (NOT dropped here) include `holder_snapshots` (current-state
-- per-token holder rows used by D03), `permit2_events` (D12), `mev_events`
-- (D13), `token2022_instructions` (D07), `wallet_funding_events`,
-- `anomaly_events`, all metadata/graph/scoring tables, and the
-- `wallet_pnl_corpus` materialised aggregate.

-- ---------------------------------------------------------------------------
-- verdict_cache
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS verdict_cache (
    chain          TEXT          NOT NULL,
    token_address  TEXT          NOT NULL,
    detector_id    TEXT          NOT NULL,
    confidence     NUMERIC(5, 4) NOT NULL CHECK (confidence >= 0 AND confidence <= 1),
    severity       TEXT          NOT NULL CHECK (severity IN ('NONE', 'LOW', 'MEDIUM', 'HIGH', 'CRITICAL')),
    evidence       JSONB         NOT NULL,
    cached_at      TIMESTAMPTZ   NOT NULL DEFAULT now(),
    expires_at     TIMESTAMPTZ   NOT NULL,
    PRIMARY KEY (chain, token_address, detector_id)
);

CREATE INDEX IF NOT EXISTS idx_verdict_cache_expires_at
    ON verdict_cache (expires_at);

CREATE INDEX IF NOT EXISTS idx_verdict_cache_token
    ON verdict_cache (chain, token_address);

COMMENT ON TABLE verdict_cache IS
    'Per-detector cached verdicts under ADR 0007 pull-based query engine. '
    'Hourly purge of expired rows by background task. TTL is computed at upsert '
    'from config/detectors.toml [verdict_cache.ttl_minutes].';

-- ---------------------------------------------------------------------------
-- Drop never-populated bulk event tables (continuous-streaming relics)
--
-- Each block guards against production data: SELECT COUNT(*) before DROP.
-- If the table contains ANY rows, the migration aborts loudly so the operator
-- can move to V00019 with explicit sign-off.
-- ---------------------------------------------------------------------------

DO $$
DECLARE n BIGINT;
BEGIN
    SELECT COUNT(*) INTO n FROM transfers;
    IF n > 0 THEN
        RAISE EXCEPTION 'V00018: refusing to drop transfers — % rows present. Manual cleanup required (V00019 with sign-off).', n;
    END IF;
    DROP TABLE transfers CASCADE;
END $$;

DO $$
DECLARE n BIGINT;
BEGIN
    SELECT COUNT(*) INTO n FROM swaps;
    IF n > 0 THEN
        RAISE EXCEPTION 'V00018: refusing to drop swaps — % rows present.', n;
    END IF;
    DROP TABLE swaps CASCADE;
END $$;

DO $$
DECLARE n BIGINT;
BEGIN
    SELECT COUNT(*) INTO n FROM pool_events;
    IF n > 0 THEN
        RAISE EXCEPTION 'V00018: refusing to drop pool_events — % rows present.', n;
    END IF;
    DROP TABLE pool_events CASCADE;
END $$;

DO $$
DECLARE n BIGINT;
BEGIN
    SELECT COUNT(*) INTO n FROM holder_snapshots_history;
    IF n > 0 THEN
        RAISE EXCEPTION 'V00018: refusing to drop holder_snapshots_history — % rows present.', n;
    END IF;
    DROP TABLE holder_snapshots_history CASCADE;
END $$;
