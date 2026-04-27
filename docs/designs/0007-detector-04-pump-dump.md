# Design 0007 — Detector D04: Pump & Dump (Volume/Price Spike)

**Date:** 2026-04-22
**Status:** Draft
**Author:** onchain-analyst agent
**ADR refs:**
- ADR 0001 §D4 — `AnomalyEvent { confidence, severity, evidence }` output contract
- ADR 0001 §D5 — MVP detector #4 (Pump & Dump), priority M
- ADR 0001 §D7 — fixture corpus bootstrapping from RugCheck rugged=true / jup_verified
- ADR 0002 — Postgres-only storage; all queries in PostgreSQL dialect
- ADR 0003 — self-sovereign infrastructure; no 3rd-party runtime dependencies
**Trait ref:** `docs/designs/0003-detector-trait.md` — implements `Detector` trait, uses `DetectorContext`;
  `fetch_rows` / `compute` split for testability
**Query ref:** `docs/queries/d04_pump_and_dump.sql` — Query 1 (spike detection) + Query 2 (insider sell-off)
**Probe ref:** `research/token-probes/rave-FeqiF7TE.md` — RAVE anchor: single-burst attack, zero-baseline gap
**Probe ref:** `research/token-probes/wet-WETZjtp.md` — WET anchor: below-threshold normal trading, established-protocol FP scenario
**Detector ID:** `pump_dump`

---

## 1. Context

Pump-and-dump is the highest-frequency fraud category in the Solana shitcoin ecosystem.
Chainalysis (2025) found that 3.59% of 2,063,519 tokens launched in 2024 meet pump-and-dump
criteria, with an average cycle duration of 6.23 days and 94% of cases rugged by the pool
deployer. The primary signal is an abnormal volume-and-price spike relative to the token's
own rolling baseline, followed within 24 hours by insider sell-off from deployer-linked wallets.

D04 has three design constraints that drive its dual-signal architecture:

1. **Zero-baseline gap (RAVE probe §4 Gap 2).** The d04_pump_and_dump.sql Query 1 guard
   (`WHERE b.median_volume_usd > 0`) returns empty when a token has been dormant for weeks
   and then activated in a single burst. The RAVE token demonstrated this precisely: 1h volume
   = 6h volume = 24h volume = $7.03M, prior baseline near zero — ratio mathematically undefined,
   detector silent. Signal B (burst concentration ratio) exists solely to close this gap.

2. **Established-protocol asymmetry.** Event-based volume spikes can happen on any token,
   including established protocols on news events. Suppressing Signal A on established protocols
   would mask real pumps. Signal C (insider sell-off amplifier) fires on state-based holder data
   and MUST be suppressed for established protocols because protocol treasuries selling tokens
   following a governance vote is not a dump.

3. **Phase 2 deployer_clusters table sparsity.** The `deployer_clusters` table is not fully
   populated until Phase 3 graph module ships. Signal C must degrade gracefully to a top-N
   holder proxy when deployer cluster data is absent.

This spec is the implementation contract for the P4-1 developer task. The developer implements
`crates/detectors/src/d04_pump_dump.rs` without modifying any frozen type in `crates/common`.

---

## 2. Signal Taxonomy

D04 produces zero to two `AnomalyEvent`s from a single `evaluate()` call:

| Signal | When it fires | Confidence band | Severity range | Event-based? |
|--------|--------------|-----------------|----------------|--------------|
| A — Volume + price spike over rolling baseline | 1h volume ≥ `volume_multiplier × 7d_daily_median` AND 1h price change ≥ `price_spike_pct`; baseline has ≥ `min_baseline_days` days | 0.60–0.95 | Medium–Critical | Yes — event-based |
| B — Burst concentration fallback | `median_volume_usd ≈ 0` OR baseline has < `min_baseline_days` days; `vol_1h / vol_24h ≥ burst_concentration_threshold`; `vol_1h ≥ min_burst_volume_usd` | 0.50–0.75 | Low–High | Yes — event-based |
| C — Insider sell-off amplifier | Signal A or B has fired; insider wallets sell ≥ `insider_sell_pct` within `post_pump_insider_window_hours`; `is_established_protocol` = false | additive +`insider_amplifier` to base, capped | Severity escalates by one level | Event-based (sell txs) + state-based (holder snapshot) |
| Info — Market cap above filter | `market_cap_usd > market_cap_filter_usd` | 0.05 | Info | N/A — design exclusion |
| Info — Insufficient data | Signal B criteria not met AND Signal A not applicable | 0.05 | Info | N/A — data gap |

Signals A and B are mutually exclusive: A fires when a valid baseline exists and the multiplier
threshold is breached; B fires when the baseline is invalid or absent. Signal C is a modifier
applied to whichever of A or B fires; it does not produce a standalone `AnomalyEvent` — instead
it modifies the confidence and evidence of the A or B event already in the output vector.

When market cap is above `market_cap_filter_usd`, `evaluate()` returns a single Info event and
exits without evaluating A, B, or C. This is by design (Bolz et al. 2024 §4.2: market-cap
filter <$60M reduces noise; high-cap tokens rarely exhibit manipulable pump patterns).

---

## 3. Algorithm

### 3.1 Entry Point

```
FUNCTION evaluate(ctx: DetectorContext) -> Result<Vec<AnomalyEvent>, DetectorError>:

  cfg = ctx.config.pump_dump
  meta = ctx.registry.enrich(ctx.token, ctx.chain).await
  IF meta is Err:
    RETURN Err(MissingDependencyData {
      detector_id: "pump_dump",
      token: ctx.token.canonical,
      reason: "TokenMeta not yet enriched"
    })

  // Market-cap filter (Bolz et al. 2024)
  IF meta.total_market_liquidity_usd * circulating_to_fdv_ratio > cfg.market_cap_filter_usd.value:
    // Use best available market cap proxy from TokenMeta; FDV is a conservative upper bound
    RETURN Ok(vec![make_info_event("market_cap_above_filter",
      metrics: { "pump_dump/market_cap_usd": market_cap_str })])

  // Step 1: compute 1h OHLCV and 7d baseline via Query 1
  spike_rows = fetch_spike(ctx, cfg).await
  //   Query 1 returns zero rows when:
  //   (a) median_volume_usd = 0 (WHERE guard) — zero-baseline case
  //   (b) thresholds not met
  //   (c) no swaps in window (pre-listing / delisted)

  baseline_days = count_baseline_days(ctx).await  // count days with non-zero volume in 7d window

  IF spike_rows is empty AND baseline_days == 0:
    // No swaps at all — token not yet indexed or delisted
    RETURN Err(MissingDependencyData {
      detector_id: "pump_dump",
      token: ctx.token.canonical,
      reason: "No swap rows found for token in 7-day window"
    })

  events = []

  IF spike_rows is non-empty:
    // Signal A fired: baseline valid and thresholds breached
    row = spike_rows[0]  // one row per (chain, token) — guaranteed by query structure
    signal_a_confidence = compute_signal_a_confidence(row, cfg)
    evidence_a = build_evidence_a(row, "spike_with_baseline")
    events.push(make_anomaly_event(signal_a_confidence, evidence_a))

  ELSE IF baseline_days < cfg.min_baseline_days.value OR baseline_days == 0:
    // Baseline insufficient — attempt Signal B fallback
    burst_row = fetch_burst_ratio(ctx).await
    //   Computes: vol_1h / vol_24h, vol_1h raw from swaps table
    IF burst_row.volume_1h_usd < cfg.min_burst_volume_usd.value:
      // Volume below dust filter — not meaningful
      RETURN Ok(vec![make_info_event("insufficient_data",
        metrics: { "pump_dump/signal": "insufficient_data",
                   "pump_dump/market_cap_usd": market_cap_str })])
    IF burst_row.burst_ratio >= cfg.burst_concentration_threshold.value:
      signal_b_confidence = compute_signal_b_confidence(burst_row, cfg)
      evidence_b = build_evidence_b(burst_row, "burst_fallback")
      events.push(make_anomaly_event(signal_b_confidence, evidence_b))
    ELSE:
      RETURN Ok(vec![make_info_event("insufficient_data",
        metrics: { "pump_dump/signal": "insufficient_data",
                   "pump_dump/market_cap_usd": market_cap_str })])

  ELSE:
    // Baseline valid but thresholds not breached — no event
    RETURN Ok(vec![])

  // Signal C: insider sell-off amplifier
  IF events is non-empty AND NOT is_established_protocol(meta):
    insider_result = fetch_insider_sells(ctx, cfg, spike_time).await
    IF insider_result.insider_sold_pct >= cfg.insider_sell_pct.value:
      apply_signal_c_amplifier(events[0], insider_result, cfg)
      // Updates evidence.metrics and confidence in-place; updates signal key
  ELSE IF events is non-empty AND is_established_protocol(meta):
    // Signal C suppressed — add audit key
    events[0].evidence.metrics["pump_dump/established_protocol_suppressed_signal_c"] = "1"

  RETURN Ok(events)
```

### 3.2 Signal A — Spike Detection Query Pattern

Signal A uses `docs/queries/d04_pump_and_dump.sql` Query 1. The query is written in PostgreSQL
dialect (ADR 0002). The `INNER JOIN` on `baseline_7d` means the query returns no rows when
`median_volume_usd = 0` (the zero-baseline guard `WHERE b.median_volume_usd > 0` AND the
`INNER JOIN` together ensure this). The developer wraps this in the `fetch_spike` function
following the `fetch_rows` / `compute` split pattern from `docs/designs/0003-detector-trait.md §mock.rs`.

The query's `median_volume_usd` column is labeled "median" but is computed as `AVG` of daily
totals over the 7-day window — a mean-as-median approximation. This is the query as written;
the developer MUST NOT silently substitute a true median computation without updating the
config key rationale and this spec. The approximation is acceptable for Phase 2: for the
threshold to be meaningful, the distribution of daily volumes needs to be roughly symmetric,
which holds for tokens with stable trading history. For tokens with highly skewed daily volumes,
the z-score column (`volume_z_score`) provides a more robust signal. Both columns are returned
and both contribute to the confidence formula.

**CTE wrapper requirement:** Postgres does not allow HAVING to reference window function results
computed in the SELECT list. The query as written does not use HAVING; the filtering is in the
outer WHERE clause via the INNER JOIN pattern. No CTE wrapper is required for D04's Query 1
beyond what is already present in the SQL file. If the developer adds a HAVING clause for
additional filtering, they MUST wrap in a subquery or CTE as established in the D02 pattern.

**Parameter binding (sqlx positional):**
- `$1` chain, `$2` token, `$3` window_start, `$4` window_end
- `$5` volume_multiplier (f64), `$6` price_spike_pct (f64)

### 3.3 Signal B — Burst Concentration Ratio Query

Signal B requires a simpler query not present in `d04_pump_and_dump.sql`. The developer adds
a `fetch_burst_ratio` function that executes:

```sql
-- Query B: burst concentration ratio (fallback for zero-baseline tokens)
-- Parameters: $1 chain, $2 token, $3 window_start_1h, $4 window_end_1h,
--             $5 window_start_24h ($3 - INTERVAL '24 hours')
WITH vol_1h AS (
    SELECT SUM(usd_value) AS volume_1h_usd
    FROM swaps
    WHERE chain = $1 AND token_out = $2
      AND block_time >= $3 AND block_time < $4
      AND usd_value > 0
),
vol_24h AS (
    SELECT SUM(usd_value) AS volume_24h_usd
    FROM swaps
    WHERE chain = $1 AND token_out = $2
      AND block_time >= $5 AND block_time < $4
      AND usd_value > 0
)
SELECT
    COALESCE(vol_1h.volume_1h_usd, 0)   AS volume_1h_usd,
    COALESCE(vol_24h.volume_24h_usd, 0) AS volume_24h_usd,
    CASE
        WHEN COALESCE(vol_24h.volume_24h_usd, 0) > 0
        THEN COALESCE(vol_1h.volume_1h_usd, 0) / vol_24h.volume_24h_usd
        ELSE 0.0
    END AS burst_ratio
FROM vol_1h CROSS JOIN vol_24h
```

This query MUST be added to `docs/queries/d04_pump_and_dump.sql` as Query B (Signal B
fallback). The developer adds it there and references it from `d04_pump_dump.rs`.

### 3.4 Signal C — Insider Sell-Off Fetch

Signal C uses `docs/queries/d04_pump_and_dump.sql` Query 2 (insider sell-off confirmation).
The caller passes `insider_addresses` from one of two sources, in priority order:

**Priority 1 (Phase 3+):** `deployer_clusters` Postgres table keyed on `(chain, token)`.
If the table has rows for this token, use them as the canonical insider address set.

**Priority 2 (Phase 2 degraded mode):** When `deployer_clusters` is empty for this token,
fall back to `meta.top_holders` filtered to holders with balance_raw ≥ 1% of `total_supply_raw`
and excluding known pool / vesting / CEX addresses via `holder_classifications` sidecar.
This is a weaker signal (top holders are not always insiders) but is the best available proxy
without graph clustering.

The developer records the source of insider addresses in evidence:
- `pump_dump/insider_source` = "deployer_clusters" or "top_holders_proxy"

When `deployer_clusters` is empty and `top_holders` is also empty (e.g. holder snapshot not
yet computed), Signal C produces no amplification — `evaluate()` returns the A or B event
without amplification and adds `pump_dump/insider_source = "unavailable"` to evidence.

---

## 4. Thresholds

All thresholds are config-driven via `config/detectors.toml` under `[pump_dump.*]`. The existing
stub keys from Sprint 2 are replaced/augmented here. Thresholds not previously in the stub are
NEW and must be added to the TOML and to `crates/detectors/src/config.rs` `PumpDumpConfig`.

| Config key | Default value | Rationale | Refs |
|---|---|---|---|
| `volume_multiplier` | **5.0** | Karbalaii (2025): ~70% of pump events have ≥5× volume vs baseline in the accumulation hour. Bolz et al. (2024): z-score ≥3 corresponds to approximately 5× baseline for normally distributed volume. Chainalysis (2025) corroborates multi-sigma spikes as the primary pump signal. | D04/pump_dump |
| `price_spike_pct` | **0.30** | Chainalysis (2025): 30% intra-hour price increase accompanies the majority of confirmed P&D events. La Morgia et al. (2021): RAVE probe measured +115% in 1h — 3.8× this threshold. Working default; recalibrate against Solana fixtures in Sprint 5. | D04/pump_dump |
| `min_baseline_days` | **3** | When fewer than 3 days of non-zero volume exist in the 7-day window, the average-as-median is not statistically meaningful (a single-day observation can dominate). RAVE probe §4 Gap 2 established this requirement. 3 is the minimum for a sample mean that meaningfully differs from a point estimate. | D04/pump_dump |
| `burst_concentration_threshold` | **0.90** | RAVE probe §4 Gap 2: volume_1h / volume_24h = 1.0 on the RAVE burst token. WET probe showed 0.0024 (normal trading). 0.90 cleanly discriminates the two cases. No academic citation; derived from probe analysis. Classified as unverified-heuristic; calibrate against labelled corpus in Sprint 5. | D04/pump_dump |
| `min_burst_volume_usd` | **5000.0** | Dust filter. Below $5,000 in 1h volume, the signal-to-noise ratio is dominated by thin-market tokens where a single retail trade can produce a 100% concentration ratio. No academic citation. Conservative floor; lower to $1,000 if Sprint 5 corpus shows meaningful pumps below $5K. | D04/pump_dump |
| `insider_sell_pct` | **0.40** | Research/02-detection-methodology.md §3: insider wallets selling ≥40% of their accumulated position within 24h confirms the dump phase. Chainalysis (2025): 94% of confirmed P&D cases involve deployer selling. 40% is a working default with no published Solana-specific calibration. | D04/pump_dump |
| `insider_amplifier` | **0.15** | Additive confidence boost when Signal C confirms insider sell-off. Calibrated so that a borderline Signal A (confidence = 0.60) elevated by +0.15 reaches Medium-High (0.75) — a meaningful severity escalation. Capped in the formula. No academic citation; derived from confidence-formula design. | D04/pump_dump |
| `post_pump_insider_window_hours` | **24** | Karbalaii (2025): ~70% of dump-phase sells complete within 1 hour of the pump peak; 24h is the outer bound to capture slower distributions. Chainalysis (2025) uses 24h window for insider-sell confirmation. | D04/pump_dump |
| `market_cap_filter_usd` | **60_000_000.0** | Bolz et al. (2024) §4.2: market-cap filter <$60M reduced noise in their CEX-based pump detector (top-5 accuracy improved from ~40% to 55.81%). At >$60M FDV, organic buy pressure from CEX listings, protocol announcements, and large-fund allocations creates volume spikes indistinguishable from coordinated pumps without order-book and off-chain data. | D04/pump_dump |

### 4.1 Threshold Divergences from Architect Stub (docs/designs/0003-detector-trait.md)

The architect stub (`PumpDumpConfig` in `config.rs`) contained four fields. Two NEW fields are
added in this spec; two existing fields are confirmed unchanged:

| Field | Stub value | This spec | Change | Citation |
|---|---|---|---|---|
| `price_spike_pct` | 0.30 | 0.30 | Unchanged | Chainalysis 2025 + Karbalaii 2025 |
| `volume_multiplier` | 5.0 | 5.0 | Unchanged | Karbalaii 2025 |
| `min_baseline_days` | 3 | 3 | Unchanged | RAVE probe §4 Gap 2 |
| `burst_concentration_ratio_threshold` | 0.90 | renamed → `burst_concentration_threshold`, same value | Rename for consistency with other detector naming | RAVE/WET probe |
| `insider_sell_pct` | 0.40 | 0.40 | Unchanged | Chainalysis 2025 |
| `insider_amplifier` | — | **0.15** | NEW | Formula design |
| `post_pump_insider_window_hours` | — | **24** | NEW | Karbalaii 2025 |
| `market_cap_filter_usd` | — | **60_000_000.0** | NEW | Bolz et al. 2024 |
| `min_burst_volume_usd` | — | **5000.0** | NEW | Dust filter; unverified-heuristic |

The `PumpDumpConfig` struct in `crates/detectors/src/config.rs` MUST be extended with the four
new fields. Field name change (`burst_concentration_ratio_threshold` → `burst_concentration_threshold`)
is a breaking config change; update `config/detectors.toml` key name accordingly.

---

## 5. Confidence Composition

### 5.1 Signal A Confidence Formula

```
// Inputs: row from Query 1
volume_ratio     = volume_1h_usd / median_volume_usd  // always >= volume_multiplier (query guard)
price_change     = price_spike_pct_1h                 // always >= price_spike_pct (query guard)
volume_z_score   = (volume_1h_usd - mean_volume_usd) / std_volume_usd  // from query; 0 if std=0

// Raw score — two components:
//   Volume excess above threshold: (ratio/multiplier - 1) * 0.5 weight
//   Price excess above threshold:  (price/spike_pct - 1) * 0.3 weight
// A volume_ratio of exactly volume_multiplier contributes 0; 2× multiplier contributes 0.5
// A price_change of exactly price_spike_pct contributes 0; 2× spike_pct contributes 0.3
raw = (volume_ratio / volume_multiplier - 1.0) * 0.5
    + (price_change / price_spike_pct - 1.0) * 0.3

// Sigmoid: 1 / (1 + exp(-raw))
// At raw=0: sigmoid=0.50; at raw=1: sigmoid=0.731; at raw=2: sigmoid=0.880
sigmoid_raw = 1.0 / (1.0 + exp(-raw))

// Clamp to [0.60, 0.95]
// Lower bound: 0.60 — a spike that barely clears both thresholds is still Medium severity
// Upper bound: 0.95 — reserve certainty for post-hoc confirmed cases
signal_a_confidence = clamp(sigmoid_raw, 0.60, 0.95)
```

The z-score is computed in the query but is NOT directly embedded in the confidence formula —
it is included in the evidence bundle as `pump_dump/volume_z_score` for human review and for
the `scoring/` crate's eventual ensemble. Rationale: the z-score requires a non-zero `std_volume_usd`
which may be zero when a token has had only one day of prior volume (all volume in one day =
zero standard deviation). Adding z-score to the formula without handling the zero-std case
would create a silent confidence distortion. Including it in evidence only is the safe choice
for Phase 2.

### 5.2 Signal B Confidence Formula

```
// Inputs: burst_ratio from Query B
// burst_ratio is always >= burst_concentration_threshold (caller ensures this)
// Linear interpolation: at threshold = 0.60 confidence; at threshold + 0.10 = 0.75 (cap)
signal_b_confidence = min(0.75, 0.50 + (burst_ratio - burst_concentration_threshold) / 0.10 * 0.25)
```

Worked examples:
- `burst_ratio = 0.90` (threshold): confidence = `0.50 + 0/0.10 * 0.25 = 0.50`. Clamped by
  `min(0.75, 0.50)` = 0.50. But note Signal B only fires when `burst_ratio ≥ threshold`, so
  this is the minimum Signal B confidence.
- `burst_ratio = 0.95`: confidence = `0.50 + 0.05/0.10 * 0.25 = 0.625`.
- `burst_ratio = 1.00` (RAVE case — all 24h volume in 1h): confidence = `0.50 + 0.10/0.10 * 0.25 = 0.75`.
- `burst_ratio > 1.00`: impossible (1h volume cannot exceed 24h volume); guard in `fetch_burst_ratio`.

**Why Signal B is capped at 0.75:** Without a validated baseline, we cannot distinguish between
a legitimate viral token with single-burst organic traffic and a coordinated pump. The 0.75 cap
reflects the irreducible uncertainty in the absence of historical data. Consumers wanting higher
certainty MUST wait for Signal C amplification or for baseline data to accumulate.

### 5.3 Signal C Amplifier

```
// Inputs: base_confidence from Signal A or B, insider_sold_pct, insider_amplifier config
amplified_confidence = base_confidence + cfg.insider_amplifier.value

// Cap varies by which signal is the base:
//   Signal A base: cap at 0.95 (same as Signal A upper bound)
//   Signal B base: cap at 0.85 (elevated above Signal B's 0.75 cap; insider sell confirms dump)
IF base_is_signal_a:
    final_confidence = min(amplified_confidence, 0.95)
    signal_label = "insider_amplified_spike"
ELSE:  // base_is_signal_b
    final_confidence = min(amplified_confidence, 0.85)
    signal_label = "insider_amplified_burst"
```

**Rationale for different caps:** Signal A with insider sell is the clearest possible pump-and-dump
confirmation: both volume/price spike AND insider selling observed. 0.95 is appropriate — the
only thing withholding full certainty (1.0) is the absence of a real-time confirmed transfer of
LP proceeds to an exchange. Signal B with insider sell is more ambiguous — we lack a baseline
to confirm the spike was anomalous relative to history, so 0.85 provides appropriate discounting.

---

## 6. Severity Mapping

D04 uses the project-standard `severity_from_confidence` function from `crates/common`. The
confidence → severity mapping is:

| Confidence range | Severity |
|---|---|
| [0.00, 0.10) | Info |
| [0.10, 0.40) | Low |
| [0.40, 0.65) | Medium |
| [0.65, 0.80) | High |
| [0.80, 1.00] | Critical |

Worked examples:
- Signal A at minimum (confidence 0.60): Medium. Appropriate — spikes that barely clear
  thresholds need human review, not immediate action.
- Signal A at `volume_ratio = 10×`, `price_change = 60%`: raw ≈ (2−1)×0.5 + (2−1)×0.3 = 0.8,
  sigmoid(0.8) ≈ 0.690, clamped to 0.690 → High.
- Signal A + Signal C amplified to 0.80: Critical. Bot-trader no-trade gate triggers.
- Signal B at minimum (0.50): Medium. Review warranted.
- Signal B at 1.00 burst ratio (0.75) + Signal C to 0.85 (capped): High.

**High/Critical boundary clarification (P5-0, action item #9):** `severity_from_confidence(0.80)`
returns `Severity::Critical` because the Critical band is `[0.80, 1.00]` — the lower bound is
inclusive. A confidence of 0.79 falls into the High band `[0.65, 0.80)` — the upper bound of
High is exclusive. This matches the `severity_from_confidence` helper semantics throughout the
project: boundaries are inclusive at the lower end, exclusive at the upper end. Calibration
consequence: `POS_054` fixture (`confidence=0.80`) is correctly Critical, not High. Any
consumer threshold set at "block if High or above" will block at confidence ≥ 0.65; "block if
Critical" will block at confidence ≥ 0.80.

---

## 7. Evidence Schema

All keys use the `pump_dump/` prefix per the project evidence convention
(`docs/designs/0003-detector-trait.md §4`). `Evidence::metrics` is `BTreeMap<String, Decimal>`;
string-valued fields use the `Evidence::notes` convention OR are encoded as
`Evidence::addresses` / `Evidence::tx_hashes`. For fields that are conceptually strings
(e.g. the signal label), use `Decimal` encoding: `"1"` = true flag, `"0"` = false, and
document the encoding in the field description below.

**Exception for signal and insider_source labels:** These are stored in `Evidence.notes` as a
structured prefix, not in `Evidence.metrics`, because `Decimal` cannot encode arbitrary strings.
The developer encodes these as: `notes = "signal=spike_with_baseline insider_source=deployer_clusters"`.
The consumer reads `notes` as space-separated `key=value` pairs. This is consistent with the
D02 notes pattern for the `latent_risk` flag.

| Evidence key | Type | Present when | Meaning |
|---|---|---|---|
| `pump_dump/volume_1h_usd` | Decimal string | Always when A or B fires | 1h volume in USD from query window |
| `pump_dump/baseline_7d_median_usd` | Decimal string | Always | Mean daily volume over 7d baseline; "0" if fallback engaged |
| `pump_dump/volume_multiplier_observed` | Decimal string | Signal A fires | `volume_1h / median_volume`; absent on Signal B |
| `pump_dump/price_change_pct_1h` | Decimal string (signed) | Signal A fires | `(price_now - price_start) / price_start`; signed positive for pump, negative for dump-leg catch |
| `pump_dump/volume_z_score` | Decimal string | Signal A fires | `(vol_1h - mean_vol) / std_vol`; 0 when std=0 |
| `pump_dump/burst_concentration_ratio` | Decimal string | Signal B fires | `vol_1h / vol_24h`; absent when Signal A fires |
| `pump_dump/baseline_days_available` | Decimal string | Always | Count of days with non-zero volume in 7-day window; 0 on Signal B |
| `pump_dump/insider_sold_pct` | Decimal string | Signal C applied | Fraction of insider holdings sold (0.0–1.0); absent if C not applied |
| `pump_dump/insider_source` | Notes (string) | Signal C applied | "deployer_clusters" or "top_holders_proxy" or "unavailable"; in `Evidence.notes` |
| `pump_dump/signal` | Notes (string) | Always | One of: "spike_with_baseline" / "burst_fallback" / "insider_amplified_spike" / "insider_amplified_burst" / "insufficient_data"; in `Evidence.notes` |
| `pump_dump/market_cap_usd` | Decimal string | Always | Best available market cap proxy (FDV as upper bound); for audit |
| `pump_dump/established_protocol_suppressed_signal_c` | Decimal string ("1") | Signal C suppressed | "1" if `is_established_protocol` = true suppressed Signal C; absent otherwise |

`Evidence.addresses`: Include insider wallet addresses when Signal C is applied. Include the
pool address(es) against which the spike was detected (from `swaps.pool`).

`Evidence.tx_hashes`: Include the tx hash of the largest sell transaction found in Signal C
Query 2 results. Include the tx hash of the largest 1h swap (buy side) from the spike window.

---

## 8. Failure Modes and Error Handling

| Condition | Behavior | Error variant |
|---|---|---|
| No swap rows for token in 7-day + 1h window | `Err(MissingDependencyData)` | `MissingDependencyData { reason: "No swap rows found" }` |
| `TokenMeta` not yet enriched | `Err(MissingDependencyData)` | `MissingDependencyData { reason: "TokenMeta not yet enriched" }` |
| `market_cap_usd > market_cap_filter_usd` | `Ok(vec![Info event])` with `signal="market_cap_above_filter"` | No error — expected exclusion |
| Baseline insufficient + burst below `min_burst_volume_usd` | `Ok(vec![Info event])` with `signal="insufficient_data"` | No error — coverage gap logged for calibration |
| Baseline insufficient + `burst_ratio < threshold` | `Ok(vec![Info event])` with `signal="insufficient_data"` | No error |
| Signal A or B fires but `deployer_clusters` empty AND `top_holders` empty | Signal A/B fires without C; evidence records `insider_source="unavailable"` | No error — graceful degradation |
| Postgres query transient failure | `Err(TransientQuery)` — retry with backoff | `TransientQuery` |
| `vol_24h = 0` in burst query (div-by-zero guard) | Query returns `burst_ratio = 0.0`; Signal B does not fire | No error |
| `amount_out_raw = 0` in price computation (div-by-zero guard) | Row excluded by `AND amount_out_raw > 0` in query WHERE clause | No error — already handled in SQL |

**Determinism invariant:** All SQL ORDER BY clauses MUST be fully deterministic:
- Query 1 `DISTINCT ON` clauses order by `(chain, token_out, block_time DESC/ASC)` — deterministic given identical data.
- Query 2 orders by `total_sold_raw DESC` — deterministic given identical data.
- Query B has no ORDER BY (scalar aggregate) — deterministic.

The developer MUST NOT call `Utc::now()` inside `evaluate()`. All timestamps come from
`ctx.window.start`, `ctx.window.end`, and block time fields from query results.

---

## 9. `is_established_protocol` Application

`crates::detectors::token_status::is_established_protocol(meta)` is applied **asymmetrically**:

| Signal | Applied? | Rationale |
|---|---|---|
| Signal A (volume/price spike) | **No — do not suppress** | Event-based: a real pump can happen on any token. Established protocols such as RAY or PYTH can be pumped by external actors. Suppressing Signal A would mask active manipulation. |
| Signal B (burst concentration fallback) | **No — do not suppress** | Event-based: same rationale as Signal A. |
| Signal C (insider sell-off amplifier) | **Yes — suppress entirely** | State-based + event-based hybrid: protocol treasuries or governance-mandated sells produce identical on-chain signatures to dump-phase insider sells. The `is_established_protocol` classifier identifies tokens where treasury sell is a documented operational behavior (RAY liquidity management, PYTH oracle operations, MPLX governance unlocks). Suppressing Signal C entirely — not dampening its coefficient — is the correct policy because: (1) even a dampened amplifier on a legitimate treasury sell would fire with Low severity and generate review noise; (2) the `established_protocol_suppressed_signal_c = "1"` evidence key preserves full auditability for post-hoc review; (3) Signal A still fires if a genuine pump occurs, so the consumer still receives a High or Critical alert — they just don't receive the insider-sell amplification. |

**Signal C suppression is total (not coefficient dampening)** because:
- Dampening (e.g. `insider_amplifier × 0.5`) would still elevate the confidence of a legitimate
  event by a partially-justified amount. The dampened signal carries an implicit false accusation.
- Total suppression with the audit key preserves the downstream human-review ability without
  generating noisy alerts on known-legitimate tokens.
- If a truly established protocol is attacked by a malicious insider, Signal A fires at the
  spike-only confidence level (0.60–0.95). This is an accepted information loss versus the
  alternative of false-positive amplification on treasuries.

**Empirical basis:** The four P3-4 corpus FPs (RAY, PYTH, TRUMP, MPLX) for D02 Signal B all
satisfy the established-protocol predicate or are pending resolution. The same tokens would
produce Signal C false positives when their treasuries execute programmatic sells into a
news-driven price spike. TRUMP (not suppressed by the current predicate) remains an outstanding
FP requiring a separate calibration task — Document this in Design Gaps §12.

---

## 10. `deployer_clusters` Graceful Degradation (Phase 2)

In Phase 2, the `deployer_clusters` table exists in the Postgres schema but is populated only
for tokens where the graph module (Phase 3) has run. For most tokens in Phase 2, it will be
empty. Signal C degrades through the following priority ladder:

```
FUNCTION resolve_insider_addresses(ctx, meta, cfg) -> InsiderSet:

  // Priority 1: deployer_clusters table (Phase 3+)
  cluster_addrs = query_deployer_clusters(ctx.store, ctx.chain, ctx.token).await
  IF cluster_addrs is non-empty:
    RETURN InsiderSet { addresses: cluster_addrs, source: "deployer_clusters" }

  // Priority 2: top_holders proxy (Phase 2 degraded mode)
  // Include holders with >= 1% of total supply, excluding known non-insider types
  liquid_holders = meta.top_holders
    .filter(|h| h.balance_raw >= meta.total_supply_raw / 100)
    .filter(|h| holder_is_not_excluded(h, ctx))  // excludes pool/vesting/CEX via holder_classifications
  IF liquid_holders is non-empty:
    RETURN InsiderSet { addresses: liquid_holders.map(|h| h.address), source: "top_holders_proxy" }

  // Priority 3: no insider data available
  RETURN InsiderSet { addresses: vec![], source: "unavailable" }

FUNCTION holder_is_not_excluded(holder, ctx) -> bool:
  // Check holder_classifications sidecar; default to NOT excluded if not classified yet
  classification = ctx.store.get_holder_classification(ctx.chain, holder.address).await
  RETURN classification.map(|c| c.kind != VestingContract && c.kind != DexPool && c.kind != CexWallet)
         .unwrap_or(true)  // unclassified = include (conservative — better to over-check)
```

The 1% supply floor for the proxy is a conservative filter: holders below 1% have insufficient
sell volume to produce a material dump-phase signal. Recalibrate this floor in Sprint 5 if the
fixture corpus shows material insider sells from sub-1% holders.

---

## 11. Fixture Corpus Specification

All fixtures live in `research/fixtures/pump_dump/`. JSON format following the pattern
established in `research/fixtures/honeypot/`, `research/fixtures/rug_pull/`, and
`research/fixtures/concentration/`.

### 11.1 Positive Fixtures (3)

**Positive 1 — RAVE live burst (Signal A + Signal B regression, live data)**
```
File: research/fixtures/pump_dump/POS_01_RAVE_FeqiF7TE.json
Mint: FeqiF7TEVmpYuuj4goD8WgmgyhFgFnZiWtw226wF4hNm
Source: RugCheck API + DEXScreener API live fetch 2026-04-21 (research/token-probes/rave-FeqiF7TE.md)
Signal: Signal A fires (price +115%, 7d baseline effectively zero → Signal B fallback also applies)
Expected confidence: Signal B path → 0.75 (burst_ratio = 1.00, formula: min(0.75, 0.50 + 0.10/0.10 * 0.25))
Expected severity: High
Signal C: Unavailable (deployer_clusters empty; top_holders_proxy shows 81.47% holder but
          Signal C window is 24h post-spike — probe data is at spike time, not post-spike)
Evidence:
  volume_1h_usd: "7032876"
  baseline_7d_median_usd: "0"
  burst_concentration_ratio: "1.00"
  baseline_days_available: "0"
  signal (notes): "burst_fallback"
  market_cap_usd: "68129"
Notes: This is the canonical RAVE-probe positive. Also serves as the primary regression test
  for the zero-baseline gap. The probe analyst assessed confidence 0.92 manually; the formula
  produces 0.75 because Signal B caps at 0.75 without Signal C. The difference (0.92 vs 0.75)
  is intentional: manual assessment incorporated Signal C intuition (81.47% single holder likely
  to sell), while the formula conservatively withholds the amplification until the sell is
  observed. This is the correct behavior: confidence 0.75 is still High severity and triggers
  consumer action.
```

**Positive 2 — RAVE-style synthetic dormant-then-burst (Signal B regression)**
```
File: research/fixtures/pump_dump/POS_02_SYNTHETIC_burst_fallback.json
Mint: SYNTH_POS_BURST_001 (synthetic: true)
Construction: Token with 90 days of zero volume followed by a single 1h burst of $50,000.
  volume_1h_usd = 50000, volume_24h_usd = 50000, burst_ratio = 1.00
  baseline_days_available = 0 (no non-zero days in 7d window)
  min_burst_volume_usd threshold = 5000 (cleared)
  deployer_clusters: empty (Phase 2 degraded mode)
  top_holders_proxy: wallet_A at 60% supply (proxy insider)
Signal: Signal B fires at 0.75
Signal C (proxy): If wallet_A sells 50%+ within 24h → amplify to min(0.75 + 0.15, 0.85) = 0.85 (High)
Expected confidence without C: 0.75 (High)
Expected confidence with C (proxy): 0.85 (High → nearly Critical)
Notes: Pure Signal B regression. Tests the Query B SQL path and the burst_ratio formula.
  Also tests Signal C degraded mode (top_holders_proxy, not deployer_clusters).
```

**Positive 3 — Synthetic pump with confirmed insider sell (Signal A + Signal C)**
```
File: research/fixtures/pump_dump/POS_03_SYNTHETIC_insider_sell.json
Mint: SYNTH_POS_INSIDER_001 (synthetic: true)
Construction:
  7-day baseline: 5 days with $1,000/day volume → median_volume_usd = $700 (avg over 7d incl. zeros)
  1h window: $8,500 volume → volume_ratio = 12.1× (>> 5.0 threshold)
  price_change_pct_1h: 0.45 (45%, >> 30% threshold)
  raw = (12.1/5 - 1) × 0.5 + (0.45/0.30 - 1) × 0.3 = (1.42 × 0.5) + (0.5 × 0.3) = 0.71 + 0.15 = 0.86
  sigmoid(0.86) ≈ 0.703; clamped to [0.60, 0.95] → 0.703
  baseline_days_available = 5 (>= min_baseline_days = 3)
Signal A confidence: 0.703 (High)
  Signal C: deployer wallet sells 65% of position within 24h (>> 40% threshold)
  amplified: min(0.703 + 0.15, 0.95) = 0.853 (Critical)
  signal label: "insider_amplified_spike"
Expected confidence: 0.853 (Critical)
Notes: Primary regression for Signal A + Signal C (full deployer_clusters path). Tests the
  full confidence formula with non-trivial inputs. Tests Signal C Query 2 path.
  Synthetic because no live token has been observed with a clean deployer_clusters entry + 
  Query 2 data available in Phase 2.
```

### 11.2 Negative Fixtures (3)

**Negative 1 — BONK daily active trading (high volume, no spike)**
```
File: research/fixtures/pump_dump/NEG_01_BONK_DezXAZ8z.json
Mint: DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263
Source: CoinMarketCap + Solana Explorer (live data; volume and price as of 2026-04-22)
Construction:
  7-day baseline: ~$20M–$50M daily DEX volume (BONK has high sustained trading)
  1h window volume: ~$1M–$3M (consistent with typical 1h slice of daily volume)
  volume_ratio: ~1.5–3× (well below 5× threshold)
  price_change_pct_1h: typically ±2–5% (well below 30% threshold)
Signal: Neither A nor B fires
Expected confidence: 0.0 (no event returned)
Notes: Primary regression test that the detector does NOT fire on a legitimately active token
  with high sustained volume. BONK has CEX listings on Binance, Coinbase, Kraken, Robinhood —
  its volume baseline is robust. The test validates that the rolling median correctly normalizes
  against a token's own history, not against a cross-token volume threshold.
  market_cap_usd >> $60M → market_cap_filter applies; emits Info event. This is expected and
  correct: BONK should not be evaluated for pump detection.
```

**Negative 2 — RAY / PYTH news-driven volume spike (Signal A fires, Signal C suppressed)**
```
File: research/fixtures/pump_dump/NEG_02_RAY_PYTH_established_protocol.json
Mint: 4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R (RAY) or 
      HZ1JovNiVvGrGNiiYvEozEVgZ58xaU3RKwX8eACQBCt3 (PYTH) — fixture uses RAY
Source: Synthetic construction based on WET-probe pattern (news-event volume spike)
Construction:
  7-day baseline: RAY has $10M–$50M daily volume → median_volume_usd ≈ $25M
  1h window: News event (e.g. Raydium governance vote) causes $200M in 1h → volume_ratio = 8×
  price_change_pct_1h: +35% (above 30% threshold)
  Signal A fires: volume_ratio = 8× >= 5.0; price_change = 0.35 >= 0.30
  Signal A confidence: raw = (8/5 - 1) × 0.5 + (0.35/0.30 - 1) × 0.3 = 0.3 + 0.05 = 0.35
                       sigmoid(0.35) ≈ 0.587; clamped to 0.60 (lower bound)
  is_established_protocol(RAY): RAY has jup_verified=false, rugcheck_score=56 → NOT suppressed
    (this is the known outstanding FP per token_status.rs module doc)
  Signal C: RAY treasury sells tokens post-spike (governance action)
    is_established_protocol = false for RAY → Signal C amplifier FIRES (this is the FP gap)
    evidence records established_protocol_suppressed_signal_c = absent (not suppressed)
Notes: This fixture documents the TRUMP/RAY outstanding FP gap (token_status.rs module doc).
  RAY does not satisfy either branch of is_established_protocol currently (jup_verified=false,
  not jup_strict). This fixture is a NEGATIVE in the sense that the expected correct behavior
  is NO Signal C amplification — but the current predicate does not suppress it for RAY.
  The fixture is labeled "expected_correct = false" with a TODO note indicating this is a
  known calibration gap requiring Sprint 5 resolution (extend the predicate to include RAY).
  PYTH (jup_verified=true, rugcheck_score=23 < 40) IS suppressed correctly — use PYTH as the
  "Signal A fires, Signal C suppressed" verification case within this fixture file.
```

**Negative 3 — USDC flat baseline, no signal**
```
File: research/fixtures/pump_dump/NEG_03_USDC_flat_baseline.json
Mint: EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v (USDC on Solana)
Source: Well-known stablecoin; parameters from public data
Construction:
  7-day baseline: USDC has enormous stable DEX volume ($500M–$2B daily on Solana DEX)
  1h window: $50M–$100M (proportional slice, no spike)
  price_change_pct_1h: +0.01% (stablecoin — near-zero price change)
  volume_ratio: ~1.0× (stable turnover)
  market_cap_usd: well above $60M → market_cap_filter applies
Signal: market_cap_filter fires → returns Info event; A, B, C not evaluated
Expected output: single Info event with signal="market_cap_above_filter"
Notes: Validates market_cap_filter path. Also validates that a stablecoin with
  near-zero price change does not fire the price spike check. Even if the market_cap_filter
  were disabled, price_change_pct_1h ≈ 0 << 0.30 → Signal A does not fire.
```

---

## 12. Known Evasions

### E-D04-1 — Slow Pump (Spike Spread Over 4h, Not 1h)

**Description:** Attacker spreads the pump over 4 hours: +8% per hour for 4 hours = +36%
total, but only +8% in any given 1-hour window. Volume is also distributed across hours.
Signal A's 1h window misses this.

**Signals defeated:** Signal A (price_change_pct_1h stays below 0.30 per window). Signal B
(volume is spread, burst_ratio stays below 0.90).

**Cost:** Low. The attacker simply coordinates the pump schedule. Slower pumps attract less
organic FOMO but also generate less D04 alert exposure.

**Detection:** Add a secondary evaluation at `slow_pump_window_hours = 4` with relaxed
thresholds: `price_spike_pct × 0.60` (18%) AND `volume_multiplier × 1.5` (7.5×) over the
4h window. The `slow_pump_window_hours` threshold pair requires a new config key and a
supplementary query. This is a **Phase 2 backlog item** — implement if Sprint 5 corpus
validation confirms this evasion pattern in labelled data. Document in config:
```toml
# [pump_dump.slow_pump_window_hours] — NOT YET IMPLEMENTED
# value = 4  # pending validation in Sprint 5
```

**Failure mode for mitigation:** Adding a 4h window increases false positives on tokens with
organic afternoon trading surges. The relaxed threshold pair must be calibrated carefully.

### E-D04-2 — Price-Only Pump (Volume from Wash Trading, Not Real Inflows)

**Description:** Attacker washes trade the pool to inflate volume (D05's domain) while
simultaneously accumulating with a small number of real buys that push price +30%+. Volume
is high but circulates between attacker wallets; real liquidity depth is barely touched.
Signal A fires because volume ratio is breached — but the "buyers" are all the attacker.

**Signals defeated:** None directly — Signal A fires correctly. However, the event is a
wash-trading event more than a pump-and-dump. The confidence assigned to Signal A may be
too high: volume generated by wash trades inflates the spike metric artificially.

**Detection:** Cross-reference with D05 wash-trading events on the same token in the same
window. If D05 fires simultaneously, the scoring layer should treat the D04 confidence as
"volume quality suspect" and adjust the combined risk score downward. This is a **cross-detector
scoring concern** handled by `crates/scoring/` (Phase 5), not by D04 itself. Document the
overlap in §13 Cross-Detector Relations.

### E-D04-3 — Pre-Pump Baseline Contamination

**Description:** Attacker buys slowly over the full 7-day baseline window (spreading 10% of
the pump volume uniformly across 7 days), then executes a large concentrated burst on day 8.
The contaminated baseline inflates the `median_volume_usd`, requiring a larger spike to breach
the `volume_multiplier` threshold. The attacker designs the burst to be exactly 5.1× the
contaminated baseline — barely clearing the threshold while having effectively pre-inflated the
baseline to require a larger real pump.

**Cost:** Medium-high. The attacker must deploy capital over 7 days in wash trades that
establish the baseline. This is expensive for high-liquidity tokens but cheap for micro-caps.

**Detection:** Karbalaii (2025) §3 describes the accumulation-phase heuristic: the baseline
window itself should be checked for anomalous volume patterns (persistent above-average daily
volume in a pre-existing dormant context). This requires tracking the pre-baseline state —
essentially a 14-day or 30-day window where the 7-day baseline is itself compared to the prior
7-day window. This is a **Phase 3 enhancement** (requires longer swap history retention and a
secondary baseline computation). Document in Design Gaps.

### E-D04-4 — Insider Address Rotation (Fresh Wallets Per Pump)

**Description:** After the first pump, all insider wallets from the original deployer cluster
are known. For subsequent pumps on the same token (or new tokens from the same operator),
the attacker rotates to freshly funded wallets. Signal C finds no known deployer cluster
addresses and falls back to the top_holders_proxy, which also does not identify the new wallets
(they buy quietly below the 1% supply floor).

**Signals defeated:** Signal C amplifier — insider sells are executed from fresh wallets not
in `deployer_clusters` or above the 1% proxy threshold.

**Cost:** Low (create fresh wallets per pump cycle). The economic cost is negligible; the
operational cost is automation of wallet generation, which is standard in the Solana bot
ecosystem.

**Detection:** Phase 3 graph clustering (wallet funding graph) connects fresh wallets to known
deployer clusters via shared SOL funding sources. Until Phase 3, Signal C operates in degraded
mode and this evasion succeeds. This is an accepted Phase 2 limitation; document in Design Gaps.

### E-D04-5 — Market-Cap Filter Bypass via Brief Supply Inflation

**Description:** Token is initially at $70M FDV (above the $60M filter). Deployer uses a
brief mint event to inflate circulating supply by 20%, temporarily deflating FDV below $60M
(same price × higher supply = lower FDV per token). During this window, D04 evaluates the
token. After evaluation, supply is burned back.

**Signals defeated:** market_cap_filter — detector evaluates a token it should exclude.

**Cost:** Medium. Requires active mint authority (which itself triggers D06). Cross-detector
link: if D06 fires simultaneously with D04, the scoring layer should treat the combined
signal as higher severity than either alone.

**Detection:** Cross-reference with D06 mint-anomaly events. If D04 fires on a token that
also has a recent D06 event (mint event within 1h of the pump window), flag the evaluation
in evidence as `pump_dump/concurrent_mint_anomaly = "1"`. This cross-pollination is a
**scoring layer concern** but the evidence key can be populated by D04 by checking the
`anomaly_events` table for recent D06 events on the same token.

### E-D04-6 — Cross-Pool Pump (Volume Concentrated in One Pool, Others Unaffected)

**Description:** A token has 5 Raydium/Orca pools. The attacker pumps only one pool —
concentrating volume and price impact in the smallest-liquidity pool (easiest to move price).
D04's Query 1 aggregates `SUM(usd_value)` across all pools for the token via `token_out = $2`.
This means the spike in the small pool is diluted by the normal volume in the other 4 pools.
The volume_ratio threshold is not breached at the token level even though the individual
pool shows a clear spike.

**Signals defeated:** Signal A — the token-level aggregation dilutes the per-pool spike.

**Cost:** Low. The attacker simply selects the most manipulable pool.

**Detection:** Add a per-pool variant of Signal A: in addition to the token-level aggregate,
evaluate each pool independently. If any single pool breaches both thresholds, fire at lower
confidence (`signal_a_confidence × 0.80`) with evidence key `pump_dump/per_pool_spike = "1"`.
This is a **Phase 2 backlog item** — add it when multi-pool signal architecture is resolved
(Open Question #4 in 0003-detector-trait.md). Until then, cross-pool pumps are a known
false-negative source.

### E-D04-7 — CEX-Listing Frontrun (Legitimate External Catalyst)

**Description:** A token receives a confirmed CEX listing announcement (Binance, Coinbase).
Organic buy pressure from traders frontrunning the listing causes +50% price in 1h and 10×
volume spike. This is not a pump-and-dump — it is legitimate market reaction to external news.
D04 Signal A fires because the thresholds are breached.

**Signals defeated:** None — Signal A fires correctly on the observable data. The false
positive is at the interpretation level, not the detection level.

**Cost:** Not an evasion in the malicious sense — this is a legitimate market event that
the detector misfires on.

**Mitigation:** D04 does not suppress this automatically. The evidence bundle includes
`pump_dump/market_cap_usd` and `pump_dump/signal = "spike_with_baseline"`. The scoring layer
and human reviewer must contextually evaluate the evidence. Future enhancement: consume
CEX listing announcement events from a static data source (news API, CEX API) as a suppression
signal. Defer to Phase 4 (self-sovereign constraint: no 3rd-party APIs in production). This
is documented as a known false positive in the calibration set.

**Known consequence:** This evasion shares the pattern with RAY/PYTH news-driven spikes
(Negative Fixture 2). The distinction between a legitimate listing pump and a coordinated
insider pump is not resolvable from on-chain data alone within the 1h window. Signal C
reduces but does not eliminate this FP: a legitimate listing pump does not trigger insider
sell-off within 24h (the team holds and benefits from appreciation), while a P&D does.

### E-D04-8 — Copy-Trading Automation (Bot Entry Used as Exit Liquidity)

**Description:** A real pump attracts copy-trading bots. The attacker uses the bots' automated
buy orders as exit liquidity — dumping into the bot-driven FOMO. D04 fires correctly on the
spike. However, the dump phase is executed through OTC or off-chain settlement between the
attacker and the copy-trading bot operators, leaving no on-chain insider sell signal.

**Signals defeated:** Signal C — the dump does not appear as on-chain sells from insider
addresses.

**Cost:** Medium. Requires coordination with or exploitation of bot operators. Some copy-trading
bots are fully automated and predictable; the attacker can exploit their entry timing without
explicit coordination.

**Detection:** Signal A fires at the spike. Signal C silence in this scenario means the
confidence stays in the 0.60–0.95 range without amplification — sufficient to trigger consumer
action. Full confirmation would require off-chain order-book data (OTC desks). This is an
accepted Phase 2 limitation.

---

## 13. Cross-Detector Relations

### D04 × D05 (Wash Trading)

**Overlap:** Some pumps are wash-traded to inflate volume before real dumps. D05 Heuristic 1
(same address round-trip within 25 slots) may fire on the same token and window as D04.

**Design rule:** When both D04 and D05 fire on the same token within the same hour, the
scoring layer MUST treat their combined confidence as higher than either alone (the signals
are complementary — volume is both abnormally large AND self-dealing). The evidence bundles
are independent; the scoring layer reads both.

**Evidence cross-pollination:** D04 does NOT directly query the D05 tables. Cross-reference is
handled by the `crates/scoring/` ensemble. Emit `pump_dump/concurrent_wash_suspected = "1"` if
the `anomaly_events` table contains a recent D05 event for the same token (within the current
evaluation window). This is a read from `anomaly_events` — a permissible cross-query because
it is reading historical output, not the concurrent computation.

### D04 × D06 (Mint/Burn Anomaly)

**Overlap:** Insider supply inflation before a pump (D06 territory) directly enables D04's pump
phase. If a deployer mints 20% additional supply into their own wallet immediately before the
pump window, D06 fires first. D04 fires on the subsequent volume spike.

**Design rule:** When D04 fires within 24h of a D06 event on the same token, both the D04
and D06 confidence values are elevated in the scoring layer. The evidence cross-pollination
key `pump_dump/concurrent_mint_anomaly = "1"` is set by D04 if a D06 event exists in
`anomaly_events` within the prior 24h.

**Market-cap filter bypass:** As described in E-D04-5, brief supply inflation followed by
burn can manipulate FDV below the market-cap filter. D04 checks `anomaly_events` for D06
events within the prior 1h before applying the market-cap filter — if a mint event is detected,
the market-cap filter is bypassed and evaluation proceeds with a warning evidence key.

### D04 × D03 (Holder Concentration Shift)

**Overlap:** Pump accumulation phase (insiders buying before the pump) shows up in D03 as
a concentration increase. Distribution phase (insiders selling after the pump) shows up as
a concentration decrease.

**Design rule:** D04 does NOT query D03 outputs. The overlap is handled by the scoring layer.
When both fire on the same token within 24h with complementary directionality (D03 shows
concentration increase during the baseline window preceding D04's pump window), the scorer
should weight this as a higher-confidence compound signal.

---

## 14. Developer Acceptance Checklist

- [ ] `cargo check -p mg-onchain-detectors` passes with no errors after adding D04 fields to
      `PumpDumpConfig` and extending `AllDetectorConfigs`.
- [ ] `cargo clippy -p mg-onchain-detectors --all-targets -- -D warnings` passes clean.
- [ ] `cargo test -p mg-onchain-detectors` passes including all D04 unit tests.
- [ ] `config/detectors.toml` includes all 9 `[pump_dump.*]` keys with non-empty `rationale`
      and at least one `refs` entry each. Config field `burst_concentration_ratio_threshold`
      is renamed to `burst_concentration_threshold`.
- [ ] `PumpDumpConfig` in `config.rs` has fields: `price_spike_pct`, `volume_multiplier`,
      `min_baseline_days`, `burst_concentration_threshold`, `min_burst_volume_usd`,
      `insider_sell_pct`, `insider_amplifier`, `post_pump_insider_window_hours`,
      `market_cap_filter_usd` — all as `Threshold<f64>` or `Threshold<u32>` as appropriate.
- [ ] The `compute_signal_a_confidence` function is a pure (non-async) function that takes
      `&SpikeRow` and `&PumpDumpConfig` and returns `Decimal`. Unit test: with
      `volume_ratio = 5.0, price_change = 0.30`, confidence = `clamp(sigmoid(0), 0.60, 0.95) = 0.60`.
- [ ] The `compute_signal_b_confidence` function is pure. Unit test: `burst_ratio = 1.00` →
      confidence = 0.75. Unit test: `burst_ratio = 0.90` → confidence = 0.50.
- [ ] `apply_signal_c_amplifier` correctly caps at 0.95 for Signal A base and 0.85 for
      Signal B base. Unit test: Signal A base 0.90 + 0.15 = capped to 0.95. Unit test:
      Signal B base 0.75 + 0.15 = capped to 0.85.
- [ ] `evaluate()` contains no call to `Utc::now()`. All timestamps derive from `ctx.window`.
- [ ] `Evidence::metrics` uses only `BTreeMap<String, Decimal>` (no `HashMap`).
- [ ] Query B (burst ratio SQL) is added to `docs/queries/d04_pump_and_dump.sql` with parameter
      comments matching the pattern of Query 1 and Query 2.
- [ ] `is_established_protocol(meta)` is called before applying Signal C. When it returns true,
      `established_protocol_suppressed_signal_c = "1"` is added to evidence and Signal C is NOT
      applied. Signal A or B confidence is returned unmodified.
- [ ] `resolve_insider_addresses` implements the two-level priority ladder (deployer_clusters
      first, top_holders_proxy second). The returned `InsiderSet.source` is included in evidence
      notes as `insider_source=<value>`.
- [ ] All three positive fixtures pass: POS_01 → Signal B, confidence ≥ 0.70; POS_02 →
      Signal B, confidence ≥ 0.70; POS_03 → Signal A + C, confidence ≥ 0.80.
- [ ] All three negative fixtures pass: NEG_01 → Info event (market_cap_filter); NEG_02 →
      Signal A fires for RAY (known gap, expected_correct = false documented); PYTH variant
      → Signal A fires, Signal C suppressed (established_protocol_suppressed_signal_c = "1");
      NEG_03 → Info event (market_cap_filter) or no event (no spike).
- [ ] REFERENCES.md has entries for all three new citations added in this spec
      (Bolz et al. 2024 market-cap filter section; burst_concentration_threshold probe-derived
      entry; insider_amplifier design-derived entry).
- [ ] `CHANGELOG.md` entry added: `Added: D04 pump_dump detector specification (0007)`.

---

## 15. Design Gaps

1. **Slow-pump window not implemented (E-D04-1 mitigation deferred).** The 4h slow-pump
   detection window (`slow_pump_window_hours`) is documented as a Phase 2 backlog item but
   not specified in this design. No Query 1 variant for a 4h window exists. Implement in
   Sprint 5 after fixture corpus validation confirms this evasion pattern is observed at
   meaningful frequency. Until then, pumps spread over >1h are a known false-negative source.

2. **Baseline contamination detection absent (E-D04-3 mitigation deferred to Phase 3).**
   The 7-day window baseline can be contaminated by attacker pre-pump wash trades. Detecting
   contamination requires a secondary 30-day baseline comparison (is the 7-day window itself
   anomalous relative to the prior 30 days?). This requires 30 days of swap history retention
   and a new query. Phase 3 enhancement; not blocking for Phase 2 MVP.

3. **RAY / TRUMP established-protocol FP gap unresolved.** RAY (jup_verified=false,
   rugcheck_score=56) and TRUMP (jup_verified=false, jup_strict=false, score=58) are not
   suppressed by the current `is_established_protocol` predicate. Signal C fires on their
   treasury sells. This is a known outstanding FP documented in `token_status.rs` and in the
   Sprint 3 calibration register. Resolving it requires either: (a) extending the predicate
   with a manual allowlist for these specific tokens, or (b) a third predicate branch based on
   on-chain provenance signals (e.g. age of token, number of verified exchange listings).
   Defer to Sprint 5 calibration task.

4. **Per-pool spike detection absent (E-D04-6 mitigation deferred).** Token-level aggregation
   of volume across all pools can miss targeted per-pool manipulation. Per-pool evaluation
   requires resolving Open Question #4 in `docs/designs/0003-detector-trait.md` (per-token vs
   per-pool invocation). Defer to when that architectural decision is made.

5. **Market cap proxy is FDV, not circulating market cap.** `TokenMeta.total_market_liquidity_usd`
   approximates market cap from DEX liquidity, not from true circulating supply × price. For
   tokens with large locked supplies (like WET: 23% circulating), the FDV significantly
   overestimates circulating market cap. The market-cap filter may incorrectly exclude tokens
   whose *circulating* market cap is below $60M but whose FDV is above it. A better proxy is
   `circulating_supply_raw × price_per_raw_unit`, available when `TokenMeta.circulating_supply_raw`
   is populated. The developer MUST prefer `circulating_supply_raw × price` when available and
   fall back to `total_market_liquidity_usd` when not. Document in evidence which proxy was used
   (`pump_dump/market_cap_source = "circulating"` or `"fdv_proxy"`).

---

## References

- Karbalaii (2025): https://arxiv.org/abs/2504.15790 — accumulation-phase structure; 70% of
  pump events concentrate volume within 1h; `volume_multiplier = 5.0`
- Bolz et al. (2024): https://arxiv.org/abs/2412.18848 — Z-score vs 30-day baseline;
  `market_cap_filter_usd = 60_000_000`; top-5 accuracy 55.81% at 20s pre-pump
- La Morgia et al. (2021): https://arxiv.org/abs/2105.00733 — F1 94.5% within 25s;
  order-book imbalance; `price_spike_pct` supporting reference
- Chainalysis (2025): https://www.chainalysis.com/blog/crypto-market-manipulation-wash-trading-pump-and-dump-2025/
  — 3.59% base rate; 94% deployer rug; `insider_sell_pct = 0.40`
- RAVE probe: `research/token-probes/rave-FeqiF7TE.md` — Signal B calibration; burst_ratio = 1.0
- WET probe: `research/token-probes/wet-WETZjtp.md` — Signal A BELOW THRESHOLD; negative case
- `docs/queries/d04_pump_and_dump.sql` — PostgreSQL-dialect queries (Query 1, 2; Query B added here)
- `crates/detectors/src/token_status.rs` — `is_established_protocol` predicate
- `docs/designs/0003-detector-trait.md` §Per-Detector Instance Metadata — D04 row, WET-fallback placeholder
- `docs/designs/0005-detector-02-rug-pull.md` §14 — established-protocol suppression precedent
- `research/fixtures/solana-corpus-phase1.md` — D02/D03 calibration register; D04 gap noted
