# Design 0006 — Detector D03: Holder Concentration Shift

**Date:** 2026-04-21
**Status:** Draft
**Author:** onchain-analyst agent
**ADR refs:**
- ADR 0001 §D4 — `AnomalyEvent { confidence, severity, evidence }` output contract
- ADR 0001 §D5 — MVP detector #3 (Holder Concentration Shift), priority S–M
- ADR 0001 §D7 — fixture corpus bootstrapping from RugCheck rugged=true / jup_verified
- ADR 0002 — Postgres-only storage; all queries in PostgreSQL dialect
- ADR 0003 — self-sovereign infrastructure; no 3rd-party runtime dependencies
**Trait ref:** `docs/designs/0003-detector-trait.md` — implements `Detector` trait, uses `DetectorContext`; LEFT JOIN to `holder_classifications` sidecar
**Query ref:** `docs/queries/d03_holder_concentration_shift.sql` — liquid-filtered concentration query (to be augmented per §3 Step 2)
**Probe ref:** `research/token-probes/wet-WETZjtp.md` — WET anchor: systematic FP source; vesting contract sidecar suppresses it
**Detector ID:** `holder_concentration`

---

## 1. Context

Anomalous holder concentration is both a leading indicator and a lagging indicator of token
fraud. In the leading role, a deployer who retains >80% of liquid supply after launch has
structural power to dump at any time. In the lagging role, a rapid concentration shift —
insiders re-accumulating from retail holders during a price run-up — precedes distribution
pumps and coordinated rugs.

Shoaei et al. 2026 (TM-RugPull) confirmed that scam tokens exhibit significantly higher
holder concentration and faster concentration variance than legitimate tokens across their
labelled dataset. Brown (2023) validated Gini coefficient as the canonical metric for
holder-distribution inequality and demonstrated that a Gini delta of material size reliably
distinguishes redistribution events from organic holder churn. SolRPDS (Alhaidari et al.
2025) identified concentration-related features (top-holder share, holder count delta) as
high-importance predictors in their Solana pool risk classifier.

The critical complication is **non-liquid supply**: vesting contracts, DEX pool accounts,
CEX hot wallets, and burn addresses hold tokens but are not retail or insider wallets. A
naive Gini or top-10% calculation over all holders systematically fires on legitimate tokens
with declared vesting allocations (Foundation 40% + Lab 25% = 65% of supply for WET, per
`research/token-probes/wet-WETZjtp.md`). The detector's core innovation is computing all
concentration metrics exclusively over **liquid holders**, with non-liquid wallets excluded
via a JOIN to the `holder_classifications` sidecar table.

This spec is the implementation contract for the P3-3 developer task. It supersedes the stub
fields in `crates/detectors/src/config.rs` (`ConcentrationConfig`) and the stub TOML section
in `config/detectors.toml` under `[holder_concentration.*]`. The developer implements
`crates/detectors/src/d03_concentration.rs` without modifying any frozen type in
`crates/common`.

---

## 2. Signal Taxonomy

D03 produces zero to three `AnomalyEvent`s from a single `evaluate()` call:

| Signal | When it fires | Confidence band | Severity range | Leading / Trailing |
|--------|--------------|-----------------|----------------|-------------------|
| 1 — Gini delta 24h | Liquid-filtered Gini increases ≥ `gini_delta_24h` over `delta_window_hours`; ≥ `min_liquid_holders` liquid wallets; prior snapshot exists | 0.50–0.80 | Medium–High | Trailing (requires two snapshots) |
| 2 — Top-10 pct delta 24h | Liquid-filtered top-10 share increases ≥ `top10_pct_delta_24h` over `delta_window_hours`; ≥ `min_liquid_holders` liquid wallets; prior snapshot exists | 0.50–0.75 | Medium–High | Trailing (requires two snapshots) |
| 3 — Absolute top-10 ceiling | Liquid-filtered top-10 share ≥ `absolute_top10_ceiling` at current snapshot | 0.65–0.85 | Medium–Critical | Leading (cold-start capable; fires on first snapshot) |
| Info — Cold start | Prior snapshot is absent (first evaluation) | 0.10 | Info | N/A |

The Info cold-start event is not a fired signal in the anomaly sense — it is an auditor-log
entry that records "detector ran, no delta possible yet." Consumers that filter by
`severity >= Low` will not surface this event.

Signals 1 and 2 can fire simultaneously from the same evaluation. Signal 3 can fire
simultaneously with Signals 1 and/or 2. All fired signals are returned in one
`Vec<AnomalyEvent>`. Each element carries its own `detector_id = "holder_concentration"`
and a distinct evidence key set identifying the signal that fired.

---

## 3. Algorithm

### 3.1 Entry point

```
FUNCTION evaluate(ctx: DetectorContext) -> Result<Vec<AnomalyEvent>, DetectorError>:

  cfg = ctx.config.holder_concentration
  now = ctx.window.end   // block-time sourced; deterministic; no wall-clock reads

  // Step 1: Fetch the current HolderSnapshot from holder_snapshots.
  //         The snapshot is the most recent one within 1 hour before ctx.window.end.
  snapshot_now = ctx.store.fetch_holder_snapshot_now(
    chain:       ctx.chain,
    token:       ctx.token,
    window_end:  now,
    tolerance:   Duration::hours(1)
  ).await

  IF snapshot_now is Err(SnapshotNotFound):
    // No snapshot at all — token not yet indexed, or holder table not populated.
    RETURN Err(DetectorError::MissingDependencyData {
      detector_id: "holder_concentration",
      token: ctx.token.canonical,
      reason: "holder_snapshots has no row for this token within 1h of window_end"
    })
    // DetectorError::is_retryable() = true; scheduler will retry after indexer populates.

  // Step 2: Compute liquid-filtered Gini and top10_pct from the current snapshot.
  //
  // IMPORTANT: snapshot_now.gini and snapshot_now.top10_pct (pre-computed in HolderSnapshot)
  //            are computed over ALL holders and MUST NOT be used for D03's primary signals.
  //            D03 requires liquid-only metrics. Execute the sidecar-JOIN SQL query instead.
  //
  // The query:
  //   SELECT
  //     COUNT(*) FILTER (WHERE hc.kind IS NULL OR hc.kind = 'Liquid')              AS liquid_count,
  //     COUNT(*) FILTER (WHERE hc.kind IS NOT NULL AND hc.kind != 'Liquid')        AS excluded_count,
  //     COUNT(*) FILTER (WHERE hc.kind IS NULL)                                    AS unclassified_count,
  //     COALESCE(
  //       SUM(hs.balance_raw) FILTER (WHERE hc.kind IS NULL OR hc.kind = 'Liquid'),
  //       0
  //     )                                                                           AS liquid_supply_raw,
  //     -- Top-10 liquid holders by balance
  //     (SELECT COALESCE(SUM(b.balance_raw), 0)
  //      FROM (
  //        SELECT hs2.balance_raw
  //        FROM holder_snapshots hs2
  //        LEFT JOIN holder_classifications hc2
  //          ON hc2.chain = hs2.chain AND hc2.address = hs2.holder
  //        WHERE hs2.chain = $1 AND hs2.token = $2 AND hs2.snapshot_id = $3
  //          AND (hc2.kind IS NULL OR hc2.kind = 'Liquid')
  //        ORDER BY hs2.balance_raw DESC
  //        LIMIT 10
  //      ) b
  //     )                                                                           AS top10_liquid_raw
  //   FROM holder_snapshots hs
  //   LEFT JOIN holder_classifications hc
  //     ON hc.chain = hs.chain AND hc.address = hs.holder
  //   WHERE hs.chain = $1 AND hs.token = $2 AND hs.snapshot_id = $3
  //
  // Parameters: $1=chain, $2=token, $3=snapshot_id (from snapshot_now.snapshot_id)
  //
  // The Gini coefficient is computed in Rust from the liquid holder balances array
  // (not in SQL), using O(N log N) sort + prefix-sum formula per Brown 2023.
  //
  // "kind IS NULL" means the address is absent from holder_classifications;
  // treat as Liquid (conservative: count it toward concentration, not away from it).

  liquid_result = ctx.store.execute_liquid_concentration_query(
    chain:       ctx.chain,
    token:       ctx.token,
    snapshot_id: snapshot_now.snapshot_id
  ).await

  IF liquid_result is Err(TransientQuery):
    RETURN Err(DetectorError::TransientQuery { ... })

  liquid_count         = liquid_result.liquid_count
  excluded_count       = liquid_result.excluded_count
  unclassified_count   = liquid_result.unclassified_count
  liquid_supply_raw    = liquid_result.liquid_supply_raw
  top10_liquid_raw     = liquid_result.top10_liquid_raw

  IF liquid_supply_raw == 0:
    // All holders are classified as non-liquid OR supply is zero.
    // Cannot compute meaningful metrics. Emit Info.
    RETURN Ok(vec![AnomalyEvent {
      detector_id: "holder_concentration",
      confidence:  0.05,
      severity:    Info,
      evidence: {
        "holder_concentration/liquid_count":    "0",
        "holder_concentration/excluded_count":  excluded_count,
        "holder_concentration/no_liquid_supply": "1"
      }
    }])

  top10_pct_now   = Decimal::from(top10_liquid_raw) / Decimal::from(liquid_supply_raw)

  // Fetch all liquid holder balances for Gini computation.
  liquid_balances = ctx.store.fetch_liquid_holder_balances(
    chain:       ctx.chain,
    token:       ctx.token,
    snapshot_id: snapshot_now.snapshot_id
  ).await
  // Returns Vec<u128> of balance_raw for holders where kind IS NULL OR kind = 'Liquid'.

  gini_now = compute_gini(liquid_balances)
  // O(N log N): sort ascending, compute Lorenz curve area, return 0.0..1.0 Decimal.
  // Returns 0.0 if liquid_balances.len() < 2.

  // Step 3: Lazy classify top-N unclassified addresses.
  //         Bounded to max_lazy_classifications to cap RPC/CPU cost per evaluation.
  //         Only applies to addresses in the top holders that are unclassified (kind IS NULL).
  //         This improves future evaluations by populating the sidecar for high-impact wallets.
  IF unclassified_count > 0:
    top_unclassified = ctx.store.fetch_top_unclassified_holders(
      chain:       ctx.chain,
      token:       ctx.token,
      snapshot_id: snapshot_now.snapshot_id,
      limit:       cfg.max_lazy_classifications.value  // default: 10
    ).await.unwrap_or_default()

    FOR each address IN top_unclassified:
      kind = ctx.registry.classify_holder(address, ctx.chain).await
        .unwrap_or(HolderKind::Liquid)
        // classify_holder falls back to Liquid on RPC failure (per token-registry contract).
      // Write-back is done by ctx.registry internally via upsert_classification.
      // The current evaluation does NOT re-query after classification —
      // classifications take effect on the NEXT evaluation cycle.
      // Rationale: re-querying would be non-deterministic (order of lazy classification
      // affects which new kinds are in scope). Accept the lag; the sidecar warms up
      // over successive evaluations.

  // Step 4: Fetch prior snapshot from holder_snapshots_history.
  //         Target time: now - delta_window_hours, tolerance: ±prior_snapshot_tolerance_hours.
  prior_target = now - Duration::hours(cfg.delta_window_hours.value)   // default: 24h
  tolerance    = Duration::hours(cfg.prior_snapshot_tolerance_hours.value)  // default: 2h

  snapshot_prior = ctx.store.fetch_holder_snapshot_history(
    chain:       ctx.chain,
    token:       ctx.token,
    target_time: prior_target,
    tolerance:   tolerance
  ).await
  // Returns the snapshot row closest to prior_target within ±tolerance.
  // Returns None if no row falls within the tolerance window.

  IF snapshot_prior is None:
    // Cold start or gap in history. Fire Info event and evaluate Signal 3 only
    // (absolute ceiling does not require a prior snapshot).
    cold_start_event = AnomalyEvent {
      detector_id: "holder_concentration",
      confidence:  0.10,
      severity:    Info,
      evidence: {
        "holder_concentration/cold_start":          "1",
        "holder_concentration/top10_pct_now":       top10_pct_now,
        "holder_concentration/gini_now":            gini_now,
        "holder_concentration/liquid_count":        liquid_count,
        "holder_concentration/excluded_count":      excluded_count,
        "holder_concentration/needs_classification_count": unclassified_count
      }
    }
    events = [cold_start_event]
    GOTO step_signal_3   // Check absolute ceiling even without prior snapshot.

  // Step 5: Compute liquid-filtered metrics for prior snapshot.
  //         Same SQL pattern as Step 2 but for snapshot_prior.snapshot_id.
  prior_liquid_result = ctx.store.execute_liquid_concentration_query(
    chain:       ctx.chain,
    token:       ctx.token,
    snapshot_id: snapshot_prior.snapshot_id
  ).await.unwrap_or_default()
  // On query failure, treat prior as unavailable (cold-start path).

  IF prior_liquid_result.liquid_supply_raw == 0:
    GOTO step_signal_3_only  // Cannot compute delta without prior liquid supply.

  top10_pct_prior = Decimal::from(prior_liquid_result.top10_liquid_raw)
                    / Decimal::from(prior_liquid_result.liquid_supply_raw)

  prior_liquid_balances = ctx.store.fetch_liquid_holder_balances(
    chain:       ctx.chain,
    token:       ctx.token,
    snapshot_id: snapshot_prior.snapshot_id
  ).await.unwrap_or_default()
  gini_prior = compute_gini(prior_liquid_balances)

  gini_delta    = gini_now - gini_prior      // positive = concentration increased
  top10_delta   = top10_pct_now - top10_pct_prior  // positive = top-10 share grew

  events = []

  // Step 6: Check minimum liquid holders guard.
  //         Gini is noisy for small populations; suppress delta signals below min_liquid_holders.
  IF liquid_count < cfg.min_liquid_holders.value:
    // Emit a dedicated Info event and skip delta signals (1, 2).
    events.push(AnomalyEvent {
      detector_id: "holder_concentration",
      confidence:  0.10,
      severity:    Info,
      evidence: {
        "holder_concentration/insufficient_liquid_holders": "1",
        "holder_concentration/liquid_count":       liquid_count,
        "holder_concentration/min_liquid_holders": cfg.min_liquid_holders.value,
        "holder_concentration/top10_pct_now":      top10_pct_now,
        "holder_concentration/gini_now":           gini_now
      }
    })
    GOTO step_signal_3  // Absolute ceiling still evaluated regardless of liquid_count.

  // Step 7: Signal 1 — Gini delta.
  IF gini_delta >= cfg.gini_delta_24h.value:
    // Formula: confidence_gini = min(1.0, 0.50 + (gini_delta - threshold) / 0.10 * 0.30)
    // Linear ramp: at threshold → 0.50; at threshold + 0.10 → 0.80; capped at 1.0.
    raw_excess  = gini_delta - cfg.gini_delta_24h.value  // non-negative
    conf_gini   = min(Decimal::ONE, Decimal::from("0.50")
                      + raw_excess / Decimal::from("0.10") * Decimal::from("0.30"))

    severity_1 = severity_from_confidence(conf_gini)

    events.push(AnomalyEvent {
      detector_id: "holder_concentration",
      confidence:  conf_gini,
      severity:    severity_1,
      evidence:    build_evidence_signal1(
        gini_delta, gini_now, gini_prior,
        top10_pct_now, top10_pct_prior, top10_delta,
        liquid_count, excluded_count, unclassified_count,
        snapshot_now, snapshot_prior
      )
    })

  // Step 8: Signal 2 — Top-10 delta.
  IF top10_delta >= cfg.top10_pct_delta_24h.value:
    // Formula: confidence_top10 = min(1.0, 0.50 + (top10_delta - threshold) / 0.10 * 0.25)
    // Linear ramp: at threshold → 0.50; at threshold + 0.10 → 0.75; capped at 1.0.
    raw_excess_t = top10_delta - cfg.top10_pct_delta_24h.value  // non-negative
    conf_top10   = min(Decimal::ONE, Decimal::from("0.50")
                       + raw_excess_t / Decimal::from("0.10") * Decimal::from("0.25"))

    severity_2 = severity_from_confidence(conf_top10)

    events.push(AnomalyEvent {
      detector_id: "holder_concentration",
      confidence:  conf_top10,
      severity:    severity_2,
      evidence:    build_evidence_signal2(
        top10_delta, top10_pct_now, top10_pct_prior,
        gini_now, gini_prior, gini_delta,
        liquid_count, excluded_count, unclassified_count,
        snapshot_now, snapshot_prior
      )
    })

  // Steps 7–8 fall through into Signal 3 check below.

step_signal_3:
  // Step 9: Signal 3 — Absolute top-10 ceiling (no prior snapshot required).
  IF top10_pct_now >= cfg.absolute_top10_ceiling.value:
    // Formula: confidence_abs = min(0.85, 0.65 + (top10_pct_now - ceiling) / 0.20 * 0.20)
    // Linear ramp: at ceiling → 0.65; at ceiling + 0.20 → 0.85; capped at 0.85.
    // Cap at 0.85 (not 1.0): static snapshot alone does not prove malicious intent.
    raw_excess_a = top10_pct_now - cfg.absolute_top10_ceiling.value  // non-negative
    conf_abs     = min(Decimal::from("0.85"), Decimal::from("0.65")
                       + raw_excess_a / Decimal::from("0.20") * Decimal::from("0.20"))

    severity_3 = severity_from_confidence(conf_abs)

    events.push(AnomalyEvent {
      detector_id: "holder_concentration",
      confidence:  conf_abs,
      severity:    severity_3,
      evidence:    build_evidence_signal3(
        top10_pct_now, liquid_count, excluded_count, unclassified_count,
        snapshot_now
      )
    })

  RETURN Ok(events)  // 0..3 signal events + optional Info events.
```

---

## 4. Confidence Composition and Severity Mapping

### Signal 1 — Gini delta confidence formula

```
gini_delta          ∈ [gini_delta_24h, ∞)
raw_excess          = gini_delta - gini_delta_24h
confidence_gini     = min(1.0,  0.50 + raw_excess / 0.10 × 0.30)
```

Calibration points (`gini_delta_24h = 0.05`):

| gini_delta | raw_excess | confidence_gini | Severity |
|-----------|-----------|----------------|---------|
| 0.05 (threshold) | 0.00 | 0.50 | Medium |
| 0.08 | 0.03 | 0.59 | Medium |
| 0.10 | 0.05 | 0.65 | High |
| 0.15 (threshold + 0.10) | 0.10 | **0.80** | High |
| 0.20 | 0.15 | 0.95 → capped at **1.0** | Critical |

The ramp rate (0.30 over 0.10 excess) is calibrated so that a Gini increase of 0.15 —
three times the detection threshold, representing a severe and rapid concentration event —
reaches 0.80, the High/Critical boundary.

### Signal 2 — Top-10 delta confidence formula

```
top10_delta         ∈ [top10_pct_delta_24h, ∞)
raw_excess          = top10_delta - top10_pct_delta_24h
confidence_top10    = min(1.0,  0.50 + raw_excess / 0.10 × 0.25)
```

Calibration points (`top10_pct_delta_24h = 0.10`):

| top10_delta | raw_excess | confidence_top10 | Severity |
|------------|-----------|-----------------|---------|
| 0.10 (threshold) | 0.00 | 0.50 | Medium |
| 0.15 | 0.05 | 0.625 | High |
| 0.20 (threshold + 0.10) | 0.10 | **0.75** | High |
| 0.30 | 0.20 | 1.0 → capped at **1.0** | Critical |

Signal 2 ramps more slowly than Signal 1 (0.25 vs 0.30 over 0.10 excess), reflecting that
a top-10 delta is a weaker individual signal than a Gini delta — a single large buyer
entering the top-10 can shift the metric without representing coordinated accumulation.

### Signal 3 — Absolute ceiling confidence formula

```
top10_pct_now       ∈ [absolute_top10_ceiling, ∞)
raw_excess          = top10_pct_now - absolute_top10_ceiling
confidence_abs      = min(0.85,  0.65 + raw_excess / 0.20 × 0.20)
```

Calibration points (`absolute_top10_ceiling = 0.80`):

| top10_pct_now | raw_excess | confidence_abs | Severity |
|--------------|-----------|---------------|---------|
| 0.80 (ceiling) | 0.00 | 0.65 | High |
| 0.90 | 0.10 | 0.75 | High |
| 1.00 (ceiling + 0.20) | 0.20 | **0.85** | Critical |

The cap at 0.85 reflects that a static concentration observation alone does not prove
malicious intent — the deployer may have legitimate vesting not yet classified in the
sidecar. The sidecar exclusion mechanism is the primary guard; the 0.85 cap is the
backstop for cases where exclusion is still warming up.

### Severity mapping

The existing `severity_from_confidence()` helper in `crates/detectors/src/signals.rs`:

| Confidence | Severity |
|-----------|---------|
| < 0.30 | Info |
| 0.30–0.50 | Low |
| 0.50–0.65 | Medium |
| 0.65–0.80 | High |
| ≥ 0.80 | Critical |

All three signals use this helper directly. The Info cold-start event and the
insufficient-liquid-holders Info event use `confidence = 0.10`, which maps to Info.

---

## 5. Threshold Table

All thresholds live in `config/detectors.toml` under `[holder_concentration.*]` and in
the `ConcentrationConfig` struct in `crates/detectors/src/config.rs`.

| Threshold | Config Key | Default Value | Rationale | Prior Art |
|-----------|-----------|--------------|-----------|-----------|
| Gini delta trigger | `gini_delta_24h` | **0.05** | Brown (2023): Ethereum Gini coefficient analysis; a 24-hour Gini increase of 0.05 represents a rapid concentration event inconsistent with organic holder churn. No published Solana-specific calibration exists; this is the working default from `research/02-detection-methodology.md` §10. Calibrate from labelled fixture corpus by Sprint 4. | Brown 2023 |
| Top-10 delta trigger | `top10_pct_delta_24h` | **0.10** | A 10 percentage-point increase in liquid top-10 share within 24 hours indicates rapid accumulation. Derived from the RugCheck DANGER breakpoints (>70% top-10 absolute) and the methodology survey. No published Solana-specific delta threshold; calibrate from SolRPDS / TM-RugPull labelled dataset. | TM-RugPull 2026, RugCheck DANGER tier |
| Absolute top-10 ceiling | `absolute_top10_ceiling` | **0.80** | Over liquid-only holders: 80% in top 10 means 10 wallets control 80% of the sellable float. Set higher than the RugCheck DANGER threshold (0.70 over all holders) because the sidecar exclusion already removes vesting and DEX pools; a post-exclusion 80% is a stronger signal than a pre-exclusion 70%. TM-RugPull (2026) confirms >80% concentration as a pre-collapse feature in their labelled scam dataset. | TM-RugPull 2026 |
| Delta window | `delta_window_hours` | **24** | The standard interval for 24-hour holder-distribution comparisons in academic literature (Brown 2023, TM-RugPull 2026). Shorter windows produce noise; longer windows miss rapid accumulation before a dump. | Brown 2023, TM-RugPull 2026 |
| Prior snapshot tolerance | `prior_snapshot_tolerance_hours` | **2** | Tolerance window when looking up the prior snapshot in `holder_snapshots_history`. The snapshot pipeline may lag by 1–2 hours under load. A ±2h tolerance is sufficient to find the closest 24h-ago snapshot without pulling in a significantly older one. Unverified-heuristic; no academic citation. | Operational heuristic |
| Min liquid holders | `min_liquid_holders` | **50** | Gini coefficient is statistically noisy for small populations (Brown 2023 derives Gini from populations of hundreds to thousands). Below 50 liquid holders, a single large purchase can shift the Gini by 0.10+ without representing a coordinated event. 50 is a pragmatic floor; calibrate from labelled corpus. | Brown 2023 §3 (population size caveat) |
| Max lazy classifications | `max_lazy_classifications` | **10** | Bounds the number of `ctx.registry.classify_holder()` RPC calls per evaluation to cap latency. Top-N holders above the sidecar join result are classified first. 10 is a conservative default for on-demand evaluation; increase to 50 for batch mode. Unverified-heuristic. | Operational heuristic |

### Threshold changes from architect stub values

| Threshold | Architect stub (detectors.toml) | This spec value | Reason |
|-----------|--------------------------------|-----------------|--------|
| `top10_pct_high_risk` (0.70) | Separate key in config | **Replaced by `absolute_top10_ceiling = 0.80`** | The 0.70 threshold was over ALL holders; D03's Signal 3 is over liquid-only holders. A post-exclusion 80% is the empirical equivalent. The key is renamed to reflect its role as the Signal 3 trigger, not a generic "high risk" label. |
| `top10_pct_elevated` (0.50) | Retained in old stub | **Dropped** | The 0.50 threshold was a coarse pre-signal gate from Phase 0. Signal 2 (delta) and Signal 3 (absolute ceiling) together cover the concentration space more precisely. A separate 0.50 gate adds complexity without improving accuracy. |
| `deployer_balance_max_pct` (0.15) | Separate key in config | **Dropped** | Subsumed by the liquid-Gini and liquid-top10 signals. If the deployer retains a material balance, it contributes to the liquid top-10 share and raises Signal 3. A separate deployer balance key requires identifying the deployer wallet explicitly (not always possible from `HolderSnapshot`); the liquid-top10 is a superset signal that catches deployer retention without needing deployer attribution. |

### New thresholds added beyond architect stub

- `delta_window_hours = 24` — explicit config key (was implicit in Phase 0 designs)
- `absolute_top10_ceiling = 0.80` — replaces the two-tier top10_pct_elevated / top10_pct_high_risk pair
- `min_liquid_holders = 50` — Gini reliability guard
- `max_lazy_classifications = 10` — RPC cost cap per evaluation
- `prior_snapshot_tolerance_hours = 2` — snapshot pipeline lag tolerance

---

## 6. Evidence Schema

All keys use the `holder_concentration/` prefix per the evidence_key convention in
`crates/detectors/src/lib.rs`. Values are `Decimal`, never `f64`, per `CLAUDE.md`.

### Signal 1 evidence keys (Gini delta)

| Key | Value type | Example | Meaning |
|-----|-----------|---------|---------|
| `holder_concentration/signal` | Decimal (enum: 1, 2, 3) | `"1"` | Which signal fired |
| `holder_concentration/gini_delta_24h` | Decimal | `"0.0820"` | Increase in liquid-filtered Gini over delta_window_hours |
| `holder_concentration/gini_now` | Decimal | `"0.7340"` | Liquid-filtered Gini at current snapshot |
| `holder_concentration/gini_24h_ago` | Decimal | `"0.6520"` | Liquid-filtered Gini at prior snapshot |
| `holder_concentration/top10_pct_now` | Decimal | `"0.7200"` | Liquid-filtered top-10 share at current snapshot |
| `holder_concentration/top10_pct_24h_ago` | Decimal | `"0.6100"` | Liquid-filtered top-10 share at prior snapshot |
| `holder_concentration/top10_pct_delta` | Decimal | `"0.1100"` | Change in liquid top-10 share |
| `holder_concentration/liquid_count` | Decimal | `"183"` | Liquid holder count at current snapshot |
| `holder_concentration/excluded_count` | Decimal | `"3"` | Holders excluded by sidecar (VestingContract + DexPool + CexHotWallet + BurnAddress) |
| `holder_concentration/needs_classification_count` | Decimal | `"7"` | Holders with no sidecar entry (treated as Liquid; will be lazy-classified) |
| `holder_concentration/snapshot_now_id` | Decimal | `"1042"` | snapshot_id of current snapshot (for audit) |
| `holder_concentration/snapshot_prior_id` | Decimal | `"891"` | snapshot_id of prior snapshot |

### Signal 2 evidence keys (Top-10 delta)

Same set as Signal 1 with `"holder_concentration/signal" = "2"`.

Signal 2 has identical evidence keys to Signal 1 because both deltas are computed from the
same snapshot pair. When both Signal 1 and Signal 2 fire from the same evaluation, the
consumer receives two events with identical evidence key values but different `confidence`
and the `signal` discriminator key.

### Signal 3 evidence keys (Absolute ceiling)

| Key | Value type | Example | Meaning |
|-----|-----------|---------|---------|
| `holder_concentration/signal` | Decimal | `"3"` | Signal 3 |
| `holder_concentration/top10_pct_now` | Decimal | `"0.8900"` | Liquid-filtered top-10 share |
| `holder_concentration/absolute_top10_ceiling` | Decimal | `"0.8000"` | Config ceiling for comparison |
| `holder_concentration/liquid_count` | Decimal | `"47"` | Liquid holder count |
| `holder_concentration/excluded_count` | Decimal | `"4"` | Non-liquid holders excluded |
| `holder_concentration/needs_classification_count` | Decimal | `"2"` | Unclassified holders |
| `holder_concentration/snapshot_now_id` | Decimal | `"1042"` | Snapshot for audit |

### Info event evidence keys (cold start, insufficient holders)

Cold start:
- `holder_concentration/cold_start = "1"`
- `holder_concentration/top10_pct_now` — Signal 3 can still fire; this provides context
- `holder_concentration/gini_now`
- `holder_concentration/liquid_count`, `holder_concentration/excluded_count`
- `holder_concentration/needs_classification_count`

Insufficient liquid holders:
- `holder_concentration/insufficient_liquid_holders = "1"`
- `holder_concentration/liquid_count`
- `holder_concentration/min_liquid_holders`

### Evidence.addresses

All events include the top-10 liquid holder addresses (up to 10) in `Evidence.addresses`.
This enables the consumer and human reviewer to directly inspect the concentrating wallets
without a secondary lookup.

### Evidence.tx_hashes

D03 is a snapshot-delta detector, not an event detector. Transaction hashes are not
included by default. Exception: if `top10_delta ≥ top10_pct_delta_24h` and the largest
single incoming transfer to a top-10 wallet in the window is identifiable from the
`transfers` table, include that transfer hash in `Evidence.tx_hashes` as contextual
evidence. This is a best-effort enrichment; absence of tx_hashes does not prevent the
event from firing.

---

## 7. Failure Modes

### 7.1 `holder_snapshots` has no row for this token

**Trigger:** Token not yet indexed, or snapshot pipeline has not processed this token.

**Action:** Return `Err(DetectorError::MissingDependencyData)` with
`reason = "holder_snapshots has no row"`. `is_retryable() = true`. Do NOT emit a
low-confidence event — absence of snapshot data is not evidence of safety or risk; it
is a pipeline gap.

---

### 7.2 `execute_liquid_concentration_query` transient failure

**Trigger:** Postgres connectivity blip or query timeout.

**Action:** Return `Err(DetectorError::TransientQuery)`. The full evaluation is abandoned.
Scheduler retries with exponential backoff. Signal 3 (absolute ceiling) is not partially
emitted — either the full evaluation succeeds or nothing is emitted, ensuring the consumer
does not act on partial state.

---

### 7.3 Prior snapshot absent (cold start)

**Trigger:** Token is being evaluated for the first time (no row in `holder_snapshots_history`
within `delta_window_hours ± prior_snapshot_tolerance_hours`).

**Action:** Emit the Info cold-start event (`confidence = 0.10, severity = Info`). Then
proceed to evaluate Signal 3 (absolute ceiling). The Info event documents that delta signals
(1, 2) were not evaluated because no prior snapshot existed, and provides the current
snapshot's concentration metrics for auditor review.

**Signal 3 still fires from the cold-start path.** This is the "cold-start capable" property
of Signal 3 — a token with 95% liquid top-10 concentration on its first snapshot is
dangerous regardless of whether a prior snapshot exists.

---

### 7.4 All holders classified as non-liquid

**Trigger:** Every holder address in the snapshot has a sidecar entry with kind in
{VestingContract, DexPool, CexHotWallet, BurnAddress}. `liquid_supply_raw == 0`.

**Action:** Emit `confidence = 0.05, severity = Info` with
`"holder_concentration/no_liquid_supply": "1"`. This state is legitimate for tokens that
have burned all supply or tokens in their vesting phase where all circulating supply is
locked. It is also a potential evasion (see §9). The Info event provides the auditor with
context; no false positive is generated.

---

### 7.5 `liquid_count < min_liquid_holders`

**Trigger:** Fewer than 50 liquid holders at current snapshot. Gini is unreliable at this
population size.

**Action:** Skip Signals 1 and 2. Emit the insufficient-liquid-holders Info event. Signal 3
still evaluates — even with a small number of liquid holders, absolute top-10 concentration
is a valid signal (it measures share, not count).

---

### 7.6 `gini_prior` or `top10_pct_prior` unavailable from prior snapshot query

**Trigger:** The prior snapshot exists but `execute_liquid_concentration_query` returns
`liquid_supply_raw == 0` for the prior snapshot (e.g., sidecar had no entries at that time,
so the prior computation would have included non-liquid holders under the current schema,
but the prior snapshot row itself is stale).

**Action:** Skip Signals 1 and 2 (no reliable delta). Evaluate Signal 3 only. The developer
must handle this path explicitly — do not propagate a `Decimal::NaN` or `Option::unwrap` on
the prior result.

---

### 7.7 Creator wallet excluded from liquid-holder set

**Trigger:** The deployer / creator wallet appears in the `top_holders` list and is also
classified in the `holder_classifications` sidecar as `CreatorWallet` (or is matched by
the creator address stored in `TokenMeta.creator`).

**Background:** In rug-pull scenarios the creator wallet often holds a large initial
allocation that is later dumped. Including this wallet in the *liquid* holder distribution
artificially inflates both Gini and top-10 concentration metrics — making the token look
more concentrated than it truly is from the perspective of *other* market participants.
Conversely, if the creator wallet is *not* present (already sold or transferred), the Gini
delta between the pre- and post-dump snapshots captures the correct concentration shift.

**Action:** The sidecar classification query (`holder_classifications`) handles this
transparently: rows with `classification_kind = 'CreatorWallet'` are excluded via the
`LEFT JOIN` filter, identical to the treatment of `VestingContract` and `DexPool` entries.
Implementors must ensure that creator addresses are populated into `holder_classifications`
during the enrichment step (`token-registry/src/enrich.rs`) and not merely stored in
`TokenMeta.creator`. A creator wallet that is *not* in `holder_classifications` will be
treated as a regular liquid holder — leading to inflated concentration metrics (false
positives) or masking concentration shifts (false negatives if the creator sells).

**Invariant:** After the sidecar join, `liquid_supply_raw` must never include the raw
balance of any address classified as `CreatorWallet`. If this invariant is violated, both
the absolute ceiling check (Signal 3) and the delta checks (Signals 1 and 2) will produce
systematically biased results for creator-heavy tokens (most new shitcoin launches).

**Test coverage:** The positive fixture `creator_wallet_concentration_positive.json` (Phase 3)
must include a creator wallet in the top-10 list and verify that its exclusion changes the
computed `top10_liquid_pct` by the expected fraction. This fixture is deferred to Phase 3
when the `holder_classifications` sidecar is implemented.

---

## 8. Fixture Corpus

Six fixtures in `research/fixtures/concentration/`. Developer writes unit tests in
`crates/detectors/src/d03_concentration.rs` and integration tests in
`tests/fixtures/concentration/` pointing at these files.

### Positive fixtures (at least one signal fires)

| File | Mint | Token | Signal(s) | Expected Confidence | Expected Severity | Notes |
|------|------|-------|-----------|--------------------|--------------------|-------|
| `FKXSS4N2HFpTw5wr2xyJBKAWRiWb4kpfGSYpK5aCRqyG.json` | `FKXSS4N2HFpTw5wr2xyJBKAWRiWb4kpfGSYpK5aCRqyG` | Rug-confirmed Solana scam token (RugCheck rugged=true; name withheld) | 1 + 2 | S1: ~0.62, S2: ~0.58 | Medium | 24h snapshot pair: top-10 pct went from 38% to 54% (delta=0.16), Gini went from 0.61 to 0.69 (delta=0.08). All top-10 wallets are Liquid in sidecar. Source: RugCheck rugged=true corpus, Phase 0 research. |
| `SYNTHETIC_liquid_concentrated_positive.json` | SYNTHETIC | — | 3 | 0.75 | High | Single snapshot: 6 Liquid wallets hold 90% of circulating supply. No prior snapshot. Signal 3 fires at cold-start path. `top10_pct_now = 0.90`, `liquid_count = 6`. Developer MUST replace with real fixture before Phase 3. |
| `SYNTHETIC_absolute_ceiling_90pct.json` | SYNTHETIC | — | 1 + 2 + 3 | S1: 0.74, S2: 0.65, S3: 0.75 | High | Prior snapshot (24h ago): top10_pct=0.65, gini=0.58. Current: top10_pct=0.90, gini=0.73. All three signals fire simultaneously. liquid_count=80. Tests full evaluation path with all signals active. |

### Negative fixtures (no signal fires, or only Info)

| File | Mint | Token | Signal | Max Expected Confidence | Notes |
|------|------|-------|--------|------------------------|-------|
| `WETZjtprkDMCcUxPi9PfWnowMRZkiGGHDb9rABuRZ2U.json` | `WETZjtprkDMCcUxPi9PfWnowMRZkiGGHDb9rABuRZ2U` | HumidiFi (WET) | None | 0.10 (Info only) | Top-3 holders at 77% are VestingContract in sidecar (Foundation 40% + Lab 25% of total supply via Jupiter Lock). After exclusion: liquid_count is very low → fires insufficient-liquid-holders Info. Does NOT fire Signals 1, 2, or 3. This is the primary driver for the sidecar design. Source: `research/token-probes/wet-WETZjtp.md`. |
| `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v.json` | `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v` | USDC | None | 0.10 (cold-start Info) | Reserve addresses classified as CexHotWallet or BurnAddress. Liquid holder distribution is well-diversified (tens of thousands of wallets). No concentration shift. Tests that a stable high-volume stablecoin suppresses all signals. |
| `EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm.json` | `EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm` | dogwifhat ($WIF) | None | 0.30 | Large mid-cap meme token with stable holder distribution: liquid_count > 50,000, top10_pct ~25% over liquid holders, Gini ~0.55 with minimal 24h delta. Tests that a well-distributed active token does not fire any signal. |

**Fixture file format:** Each JSON file contains two top-level objects:
- `"snapshot_now"` — the current `HolderSnapshot` state (from `holder_snapshots` + the
  computed liquid breakdown from the JOIN query)
- `"snapshot_prior"` — the prior `HolderSnapshot` state (from `holder_snapshots_history`),
  or `null` if testing the cold-start path
- `"sidecar_rows"` — an array of `holder_classifications` rows pre-populated for the test
- `"expected"` — the expected evaluation output: `{ signals_fired: [1,2,3], confidence: [...],
  severity: [...] }`

Developer populates each fixture's `sidecar_rows` array to simulate the sidecar state at
evaluation time. The unit test framework inserts these rows into the test database before
running the evaluation.

---

## 9. Known Evasions

Building on `docs/reviews/0002-d02-rug-pull-evasions.md` §E-D02-16, which identified
pre-drain holder dilution as an explicit D03 suppression tactic.

### E-D03-1 — Sidecar label laundering

**Attack:** Before launching the scam token, the deployer creates a wallet that mimics a
vesting contract pattern: it holds tokens behind a program account, has a locked-until date,
and matches the discriminator pattern used by Jupiter Lock or Fluxbeam. The deployer submits
it to the `holder_classifications` sidecar (either by directly calling `upsert_classification`
if accessible, or by triggering an RPC-based classification that produces
`kind = VestingContract`). The large insider balance is then excluded from liquid
calculations. Signal 3 does not fire.

**Signals defeated:** Signal 3 (absolute ceiling). Signal 1 and 2 may still fire on the
delta leading up to the drain.

**Mitigation (Phase 2):** `crates/token-registry/src/classify.rs` classifies vesting
contracts by checking known locker program IDs (whitelist). The deployer must match a
specific program ID, not just a discriminator. Additionally, `upsert_classification` has a
confidence guard (only overwrites if new confidence ≥ existing). A VestingContract
classification has confidence 0.90; an attacker who triggers the classification via a
convincing program structure reaches at most 0.90, which overwrites a prior Liquid (0.50)
classification. This is the known vulnerability from D02 E-D02-4 (fake locker) applied to
D03.

**Phase that fixes it:** Phase 3 upgrade-authority check for vesting program accounts
(identical to D02 E-D02-4 mitigation). Vesting programs whose upgrade authority is the
deployer cluster are not trusted.

---

### E-D03-2 — Slow distribution before dump (pre-drain holder dilution)

**Attack:** The deployer holds 80% of liquid supply. Starting 3 weeks before the drain, the
deployer distributes tokens to 200 fresh wallets (each receiving ~0.4% of supply), bringing
the top-10 liquid share below the `absolute_top10_ceiling`. The Gini also drops from 0.80 to
0.55. When the dump occurs (after the accumulation by retail triggered by the shill campaign),
D03 does not fire because concentration has been artificially reduced. D02 may fire if a pool
drain occurs, but the D03 suppression causes the combined anomaly score to understate risk.

**Signals defeated:** All three D03 signals. D02 Signal B (latent risk) is unaffected.

**Mitigation (Phase 2):** Cross-detector scoring rule: D02 Critical signals MUST NOT be
attenuated by D03 silence. The scoring crate implements this as a veto: if D02 fires with
severity ≥ High, the combined score is not reduced below High regardless of D03's output.
See `docs/reviews/0001-d01-honeypot-evasions.md` §E-D02-16 cross-reference.

**Phase that fixes it:** Phase 3 wallet clustering. The 200 fresh recipient wallets are
funded from the deployer cluster. A funded-by-deployer label on those wallets causes them
to be treated as `HolderKind::Liquid` with a deployer-affiliated flag, which the scoring
crate can use to raise the risk score even when raw concentration metrics appear healthy.

---

### E-D03-3 — All-vesting classification (classify-out entire supply)

**Attack:** Deployer creates a token where 100% of supply is nominally "locked" in a program
account. The classification ladder assigns all supply to `VestingContract`.
`liquid_supply_raw == 0`. D03 emits the Info event from §7.4 (no_liquid_supply) rather
than firing a signal. The bot-trader, filtering for `severity >= Low`, does not see any D03
alert.

**Signals defeated:** All signals. D03 is effectively blind.

**Mitigation (Phase 2):** The `no_liquid_supply` Info event must be elevated to Low or
Medium severity in the scoring crate when the token also has active trading volume (detected
by D04 or from the indexer's swap event count). A token with zero liquid supply AND active
trading is internally inconsistent — either the supply classification is wrong or the trading
is occurring through a mechanism not captured by `holder_snapshots`. The scoring crate raises
the combined score in this case.

**Phase that fixes it:** Phase 2 scoring rule (cross-detector consistency check). Phase 3
adds a dedicated "phantom supply" detector that fires when `liquid_supply_raw == 0 AND
swap_count_24h > 0`.

---

## 10. Design Gaps

Five areas where this spec cannot be fully definitive without implementation context:

### DG-D03-1 — `holder_snapshots_history` liquid-filtered fields

**Gap:** The `holder_snapshots_history` table stores `HolderSnapshot` rows with the
precomputed `gini: Option<Decimal>` and `top10_pct: Option<Decimal>` fields, which are
computed over ALL holders (not liquid-only). D03 requires liquid-filtered metrics for the
prior snapshot, computed by the same LEFT JOIN pattern as the current snapshot (Step 5
above).

This means Step 5 must execute the `execute_liquid_concentration_query` against the prior
`snapshot_id`. The query works on `holder_snapshots` rows tagged by `snapshot_id`; the
developer must confirm that `holder_snapshots_history` is structured as a partitioned view
or a separate table where historical rows retain the `(chain, token, holder, balance_raw,
snapshot_id)` columns needed for the JOIN. If historical rows are stored in aggregated form
(without per-holder rows), the prior-snapshot Gini and top10_pct cannot be computed from
the history table.

**Developer task:** Confirm the schema of `holder_snapshots_history` before implementing
Step 5. If per-holder rows are not retained in history, add them — the per-holder rows are
required for the liquid-filtered delta computation.

### DG-D03-2 — Gini computation precision for large holder sets

**Gap:** The Gini computation in Step 2 loads the full `liquid_balances` vector into Rust
memory. For a token like USDC with 500,000+ holders, this is a significant allocation.

**Developer task:** Implement a streaming Gini approximation for tokens where
`liquid_count > 10,000` (configurable). The approximation sorts and processes holders in
batches using a merge-sort approach. The exact streaming formula is described in Brown 2023
Appendix B. For Phase 2, a hard limit of `liquid_count ≤ 50,000` with a graceful fallback
to "gini unavailable" (skip Signal 1) is acceptable; add a TODO for Sprint 4 streaming
implementation.

### DG-D03-3 — `snapshot_id` field in `holder_snapshots`

**Gap:** The algorithm references `snapshot_now.snapshot_id` as a parameter to
`execute_liquid_concentration_query`. The frozen `HolderSnapshot` struct in
`crates/common/src/token.rs` does not have a `snapshot_id` field — it has `block: BlockRef`
and `block_time: DateTime<Utc>`. The Postgres schema may use `block.slot` or a separate
auto-increment `snapshot_id` column.

**Developer task:** The `ctx.store.fetch_holder_snapshot_now()` return type is a
storage-layer struct (not `HolderSnapshot` from `crates/common`) that includes the Postgres
row identifier. Define a `HolderSnapshotRow { snapshot_id: i64, ... common fields ... }`
in `crates/storage` to carry this identifier without modifying the frozen common type.

### DG-D03-4 — Lazy classification write-back cycle

**Gap:** Step 3 calls `ctx.registry.classify_holder()` for unclassified addresses. The
token-registry library's `classify_holder` method does NOT write back to the sidecar (per
`crates/token-registry/src/lib.rs` — only `upsert_classification` does). The algorithm
notes "write-back is done by ctx.registry internally," but this is inconsistent with the
current implementation.

**Developer task:** Confirm whether `classify_holder` performs upsert internally or whether
the detector must explicitly call `ctx.registry.upsert_classification(address, kind)` after
classification. If the latter, add the upsert call to the lazy-classification loop. The
current implementation behaviour is the authoritative source; this spec defers to it.

### DG-D03-5 — `ConcentrationConfig` struct field consolidation

**Gap:** The existing stub `ConcentrationConfig` in `crates/detectors/src/config.rs` has
fields `top10_pct_elevated`, `top10_pct_high_risk`, `gini_delta_24h`,
`top10_pct_delta_24h`, `deployer_balance_max_pct`. This spec:
(a) Drops `top10_pct_elevated`, `top10_pct_high_risk`, `deployer_balance_max_pct`.
(b) Adds `absolute_top10_ceiling`, `delta_window_hours`, `min_liquid_holders`,
    `max_lazy_classifications`, `prior_snapshot_tolerance_hours`.
(c) Retains `gini_delta_24h` and `top10_pct_delta_24h` with the same names.

The developer must update `ConcentrationConfig`, remove old fields, update
`config/detectors.toml`, and confirm `AllDetectorConfigs` deserializes without error.
`config.rs` is NOT frozen.

---

## 11. Developer Acceptance Checklist

Before marking P3-3 complete, the developer must verify:

### Config
- [ ] `cfg.holder_concentration.gini_delta_24h.value == 0.05`
- [ ] `cfg.holder_concentration.top10_pct_delta_24h.value == 0.10`
- [ ] `cfg.holder_concentration.absolute_top10_ceiling.value == 0.80`
- [ ] `cfg.holder_concentration.delta_window_hours.value == 24`
- [ ] `cfg.holder_concentration.min_liquid_holders.value == 50`
- [ ] `cfg.holder_concentration.max_lazy_classifications.value == 10`
- [ ] `cfg.holder_concentration.prior_snapshot_tolerance_hours.value == 2`
- [ ] Old fields `top10_pct_elevated`, `top10_pct_high_risk`, `deployer_balance_max_pct` are REMOVED from `ConcentrationConfig` and `config/detectors.toml`
- [ ] `load_detector_config("config/detectors.toml")` parses without error with all new fields

### Liquid filtering
- [ ] The detector NEVER uses `snapshot.gini` or `snapshot.top10_pct` (pre-computed ALL-holder fields from `HolderSnapshot`) for Signals 1, 2, or 3
- [ ] `execute_liquid_concentration_query` filters on `hc.kind IS NULL OR hc.kind = 'Liquid'` (NULL = unclassified = treated as Liquid)
- [ ] `excluded_count` counts only wallets with a non-NULL, non-Liquid kind in sidecar
- [ ] `needs_classification_count` counts wallets with `hc.kind IS NULL` (absent from sidecar)
- [ ] Evidence keys `holder_concentration/excluded_count` and `holder_concentration/needs_classification_count` are present on every emitted event

### Algorithm correctness
- [ ] `evaluate()` returns `Err(MissingDependencyData)` when `holder_snapshots` has no row within 1h of `window_end` (not an Info event, not `Ok([])`)
- [ ] Cold-start path (no prior snapshot): Info event emitted with `cold_start = "1"` AND Signal 3 still evaluated
- [ ] `liquid_count < min_liquid_holders`: Signals 1 and 2 suppressed; Signal 3 still evaluated; insufficient-liquid-holders Info emitted
- [ ] `liquid_supply_raw == 0`: Info event with `no_liquid_supply = "1"` returned; no signal events
- [ ] Signal 3 fires on cold-start path: `evaluate()` returns `[Info(cold_start), Signal3(0.65..0.85)]` when `top10_pct_now >= absolute_top10_ceiling` and no prior snapshot exists
- [ ] Lazy classification cap: at most `max_lazy_classifications` calls to `ctx.registry.classify_holder()` per evaluation
- [ ] Lazy classification does NOT affect the current evaluation's metrics (only warms the sidecar for next evaluation)
- [ ] `compute_gini(balances)` returns `Decimal::ZERO` when `balances.len() < 2`
- [ ] `compute_gini` uses `Decimal` arithmetic throughout (no `f64` intermediate)

### Confidence formulas
- [ ] `gini_delta = 0.05` (threshold) → `confidence_gini = 0.50` exactly
- [ ] `gini_delta = 0.15` → `confidence_gini = 0.80` exactly (ramp rate check)
- [ ] `top10_delta = 0.10` (threshold) → `confidence_top10 = 0.50` exactly
- [ ] `top10_delta = 0.20` → `confidence_top10 = 0.75` exactly
- [ ] `top10_pct_now = 0.80` (ceiling) → `confidence_abs = 0.65` exactly
- [ ] `top10_pct_now = 1.00` → `confidence_abs = 0.85` (capped, not 1.0)

### Evidence
- [ ] All required evidence keys present on every emitted AnomalyEvent (§6 key tables)
- [ ] All evidence keys use `holder_concentration/` prefix
- [ ] `holder_concentration/signal` discriminator key present: `"1"`, `"2"`, or `"3"` per fired signal
- [ ] `Evidence.addresses` contains the top-10 liquid holder addresses (up to 10)

### Tests
- [ ] Unit test: WET fixture (`WETZjtprkDMCcUxPi9PfWnowMRZkiGGHDb9rABuRZ2U.json`) → max confidence ≤ 0.10, no signal events (only Info)
- [ ] Unit test: USDC fixture → no signal events
- [ ] Unit test: $WIF fixture → no signal events, max confidence < 0.30
- [ ] Unit test: positive rugged fixture (FKXSS4N2HFpTw5wr2xyJBKAWRiWb4kpfGSYpK5aCRqyG.json) → Signal 1 OR Signal 2 fires, confidence ≥ 0.50
- [ ] Unit test: cold-start + Signal 3: `top10_pct_now = 0.90`, no prior snapshot → `[Info(cold_start), AnomalyEvent(signal=3, confidence=0.75)]`
- [ ] Unit test: `liquid_count = 40 < min_liquid_holders = 50` → `[Info(insufficient)]`; Signal 3 still evaluated if `top10_pct_now >= absolute_top10_ceiling`
- [ ] Unit test: `gini_delta = 0.08` AND `top10_delta = 0.11` → two events returned (Signal 1 + Signal 2)
- [ ] Integration test (Postgres test container): insert `holder_snapshots` + `holder_classifications` rows; call `evaluate()`; verify Gini and top10_pct computed from liquid-only rows
- [ ] Integration test: fixture `WETZjtprkDMCcUxPi9PfWnowMRZkiGGHDb9rABuRZ2U.json` with sidecar rows inserted → verify zero signal events

### Cross-references
- [ ] `REFERENCES.md` rows for D03/holder_concentration exist (populated in this sprint per §12)
- [ ] `config/detectors.toml` D03 stub replaced with full threshold keys (DG-D03-5)
- [ ] Scoring crate has rule: D02 Critical is not attenuated by D03 silence (evasion E-D03-2)

---

## 12. References

All primary references are in `REFERENCES.md`. Rows used by D03:

| Mechanism | Row in REFERENCES.md |
|-----------|---------------------|
| Gini methodology and delta detection | "Holder concentration — Brown 2023" |
| Concentration as rug-pull pre-signal | "Holder concentration — rug signal — TM-RugPull 2026" |
| Top-10 pct and holder features in Solana risk classifier | "Holder concentration — Solana features — SolRPDS 2025" |
| WET systematic FP derivation | `research/token-probes/wet-WETZjtp.md` §gap analysis |
| Pre-drain holder dilution as D03 suppression | "Holder dilution before rug" (existing D02 REFERENCES.md row) |

New rows added by this spec (max 3):

| Mechanism | Signal / Formula | Source | Used In | Verified Against |
|-----------|-----------------|--------|---------|-----------------|
| Holder concentration — Solana features | Top-10 holder share and holder count delta are high-importance features in Solana pool risk classifier trained on SolRPDS dataset; liquid-only filtering described as critical for accuracy | Alhaidari et al. 2025 (SolRPDS) Table 2 feature importance, https://arxiv.org/abs/2504.07132 | D03 Signal 3 `absolute_top10_ceiling` threshold calibration; `min_liquid_holders` guard rationale | Live fetch 2026-04-21 |
| Vesting contract misclassification (WET probe) | HumidiFi Token (WET): top-3 holders at 77% of supply are Foundation/Lab vesting contracts via Jupiter Lock; raw top-10 metric fires at confidence 0.55; after sidecar VestingContract exclusion, no signal fires — systematic FP source requiring sidecar LEFT JOIN design | `research/token-probes/wet-WETZjtp.md` §gap analysis (internal derivation, 2026-04-21) | D03 sidecar LEFT JOIN design; `absolute_top10_ceiling = 0.80` (post-exclusion calibration); negative fixture WETZjtprkDMCcUxPi9PfWnowMRZkiGGHDb9rABuRZ2U.json | Probe derivation 2026-04-21 |
| Gini minimum population caveat | Gini coefficient is unreliable for populations < ~50; single large purchase can shift Gini by 0.10+ in small-holder tokens; paper cites population sensitivity as a known limitation of Gini for financial inequality analysis | Brown 2023 §3 (population size caveat), https://eprint.iacr.org/2023/1493.pdf | D03 `min_liquid_holders = 50` guard; Signal 1 suppression when liquid_count < 50 | Cross-reference to existing entry |
