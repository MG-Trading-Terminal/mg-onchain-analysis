-- =============================================================================
-- V00012__token_risk_reports.sql  —  Durable token risk scoring persistence
-- =============================================================================
-- Migration tool: sqlx migrate (sqlx-cli).
-- Apply: `sqlx migrate run --database-url $DATABASE_URL`
--        or via `StorageConfig.migrations_auto_apply = true` at service startup.
--
-- Design context: SESSION-KICKOFF gotchas #30 + #31.
-- Sprint 8 spec deviation: scoring was in-memory RiskCache only.
-- Sprint 12 (Option C): V00012 adds the durable persistence layer.
--
-- Consumer: `crates/server/src/streaming/worker.rs` calls
--   `PgTokenRiskReportStore::upsert_token_risk_report` after each scoring tick
--   (when `streaming.persistence.token_risk_reports_enabled = true`).
-- Gateway: `crates/gateway/src/cache.rs` RiskCache remains the HOT cache.
--   This table is the COLD durable record (auditing, crash recovery, backfill).
--   The gateway does NOT fall back to this table at V00012; that is a follow-up.
--
-- Partitioning decision (gotcha #7 forward-compat):
--   token_risk_reports is NOT partitioned at V00012 scale.
--   Row count is bounded: one row per (chain, token, window_end) per scheduler
--   cycle. At MVP scale (~1000 tracked tokens × ~1 report/hour = ~8760 rows/token/year)
--   the total stays well under 10M rows for multiple years. No partition needed.
--   Scale trigger: if rows exceed 10M (e.g. Phase 4 multi-chain × high-frequency
--   scheduler), add monthly partitioning on window_end. At that point, window_end
--   MUST be included in every unique constraint (gotcha #7 is already satisfied
--   by the PK including window_end).
--
-- Retention policy (not enforced here):
--   Proposed 90-day retention: rows where window_end < now() - INTERVAL '90 days'
--   are eligible for deletion. A background eviction job (Sprint 13+) should run:
--     DELETE FROM token_risk_reports WHERE window_end < now() - INTERVAL '90 days';
--   This is NOT implemented in V00012 to keep the migration focused.
--   Tracked as follow-up in ROADMAP.md.
--
-- Gotcha #6 (UNIQUE dedup): the PRIMARY KEY (chain, token, window_end) is the
--   dedup constraint. Upserts use ON CONFLICT (chain, token, window_end) DO UPDATE.
--   Multiple scheduler runs for the same (chain, token, window_end) tuple are safe:
--   the latest run overwrites (idempotent re-score picks up late events).
-- =============================================================================

CREATE TABLE IF NOT EXISTS token_risk_reports (
    -- -------------------------------------------------------------------------
    -- Identity
    -- -------------------------------------------------------------------------

    -- Chain identifier (e.g. 'solana', 'ethereum'). TEXT, not enum.
    chain            TEXT        NOT NULL,

    -- Token mint address in chain-canonical form.
    -- Solana: Base58. Ethereum: checksummed hex. Normalised at trait boundary.
    token            TEXT        NOT NULL,

    -- Detection window start timestamp (from TokenRiskReport::window.0).
    -- Indexed implicitly via the PK on window_end; range queries use computed_at.
    window_start     TIMESTAMPTZ NOT NULL,

    -- Detection window end timestamp (from TokenRiskReport::window.1).
    -- Serves as the stable dedup identifier for a scheduler cycle.
    window_end       TIMESTAMPTZ NOT NULL,

    -- Wall-clock time the report was produced.
    -- Source of truth for "latest report" ordering (not window_end).
    -- computed_at is allowed to differ between re-runs of the same window_end.
    computed_at      TIMESTAMPTZ NOT NULL,

    -- -------------------------------------------------------------------------
    -- Top-level scores
    -- -------------------------------------------------------------------------

    -- Overall risk score after attenuation multipliers. Range [0.0, 1.0].
    -- DOUBLE PRECISION is correct: Confidence is a probability, not a money amount.
    -- See CLAUDE.md §no-f64 rule — applies to prices/amounts/supplies, not probabilities.
    overall_score    DOUBLE PRECISION NOT NULL
                         CHECK (overall_score >= 0.0 AND overall_score <= 1.0),

    -- Pre-attenuation score. Range [0.0, 1.0].
    base_score       DOUBLE PRECISION NOT NULL
                         CHECK (base_score >= 0.0 AND base_score <= 1.0),

    -- -------------------------------------------------------------------------
    -- Severity
    -- -------------------------------------------------------------------------

    -- Worst-case severity across all fired events.
    -- Stored as TEXT with application-level enforcement via Severity enum.
    -- TEXT (not Postgres CHECK enum) to avoid migration cost for new severity levels.
    -- Valid values: 'Info', 'Low', 'Medium', 'High', 'Critical'.
    overall_severity TEXT        NOT NULL,

    -- -------------------------------------------------------------------------
    -- JSONB columns for nested structs
    -- -------------------------------------------------------------------------

    -- BTreeMap<String, DetectorScore>: per-detector breakdown.
    -- BTreeMap guarantees deterministic alphabetical key ordering — JSONB stores are
    -- deterministic across upserts given identical input (no HashMap nondeterminism).
    per_detector     JSONB       NOT NULL,

    -- Vec<EvidenceHighlight>: top-N evidence entries ranked by severity + confidence.
    -- Ordered deterministically by the scoring engine (rank_top_evidence).
    top_evidence     JSONB       NOT NULL,

    -- SignalCounts: { fired, inconclusive, suppressed_info } event counts.
    signal_counts    JSONB       NOT NULL,

    -- CoverageReport: { detectors_run, detectors_skipped, coverage_completeness }.
    coverage         JSONB       NOT NULL,

    -- ScoringConfig snapshot: exact config used to produce this report.
    -- Stored for reproducibility auditing (consumers can replay with this config).
    config_snapshot  JSONB       NOT NULL,

    -- -------------------------------------------------------------------------
    -- Housekeeping
    -- -------------------------------------------------------------------------

    -- Last updated timestamp (server-side). Use for "row was inserted/updated at".
    -- Distinct from computed_at (scoring clock) — updated_at is the storage clock.
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- -------------------------------------------------------------------------
    -- Dedup: one report per (chain, token, window_end). Latest wins on conflict.
    -- -------------------------------------------------------------------------
    PRIMARY KEY (chain, token, window_end)
);

-- -------------------------------------------------------------------------
-- Indexes
-- -------------------------------------------------------------------------

-- "Latest reports by compute time" — scheduler monitoring, admin dashboards.
-- DESC ordering: most recent compute at the front of the index.
CREATE INDEX IF NOT EXISTS idx_token_risk_reports_computed_at
    ON token_risk_reports (computed_at DESC);

-- "Medium+ risk tokens on a given chain" — consumer hot query.
-- Partial index: only rows where overall_score >= 0.4 (Medium threshold).
-- The partial predicate keeps the index small and avoids scanning low-risk noise.
-- Source: spec consumer guidance (overall_score >= 0.4 = medium risk / review).
CREATE INDEX IF NOT EXISTS idx_token_risk_reports_overall_score
    ON token_risk_reports (chain, overall_score DESC)
    WHERE overall_score >= 0.4;
