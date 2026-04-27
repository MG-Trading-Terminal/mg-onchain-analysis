# Design 0010 — `crates/scoring`: Token Risk Score Aggregation

**Date:** 2026-04-21
**Status:** Draft
**Author:** architect agent
**Sprint:** 5 (P5-1)
**ADR refs:**
- ADR 0001 §D4 — `AnomalyEvent { confidence, severity, evidence }` output contract
- ADR 0001 §D8 — three delivery modes (in-process crate, REST, WebSocket streaming)
- ADR 0002 — Postgres-only storage; all hot state in Postgres
- ADR 0003 — self-sovereign infrastructure; no 3rd-party runtime dependencies in hot path
**Design refs:**
- `docs/designs/0001-crates-common-types.md` — `AnomalyEvent`, `Severity`, `Confidence`, `Evidence`
- `docs/designs/0003-detector-trait.md` — `Detector` trait, `DetectorContext`
- `docs/designs/0004` through `0009` — D01–D06 specs; confidence bands and severity mappings
- `docs/reviews/0001-0003*.md` — adversarial reviews; DG4 jup_verified attenuation now lands here
**Calibration anchors:**
- `research/token-probes/rave-FeqiF7TE.md` — RAVE: 0.83 / Critical
- `research/token-probes/wet-WETZjtp.md` — WET: 0.31 / Medium

---

## 1. Context and Scope

### Why a separate `crates/scoring` crate

Detectors emit `AnomalyEvent` values independently. Each detector has its own confidence model, severity scale, and evidence bundle. No single detector knows about the others. A consumer who receives six separate `AnomalyEvent`s for a single token still needs to answer: "Is this token worth trading? How dangerous is it overall?"

`crates/scoring` is the aggregation layer that takes a `Vec<AnomalyEvent>` for a single `(chain, token, time_window)` tuple and returns a single `TokenRiskReport` — a consumer-friendly summary with one overall score, one severity band, per-detector breakdowns, the highest-impact evidence entries, coverage metadata, and the configuration used. This crate has no I/O dependencies of its own: it is a pure computation over already-fetched data.

### What this crate does NOT do

- Issue database queries. Callers (e.g. `crates/gateway`, `crates/server`) fetch `AnomalyEvent` rows from Postgres and pass them in.
- Emit new `AnomalyEvent` values. Scoring produces `TokenRiskReport` only.
- Modify detector thresholds. Scoring never changes how detectors fire.
- Implement streaming. Callers re-invoke scoring per WS update tick; the crate has no async runtime interaction.
- Touch `crates/common` types. `AnomalyEvent`, `Severity`, `Confidence` are frozen.

---

## 2. Input / Output Types

### Input

```
fn score(
    events:  &[AnomalyEvent],   // all events for (chain, token) in window, any order
    meta:    &TokenMeta,         // current token state from token-registry
    config:  &ScoringConfig,     // loaded from config/scoring.toml
    now:     DateTime<Utc>,      // wall-clock moment (only used for computed_at field)
    window:  (DateTime<Utc>, DateTime<Utc>),  // the query window; matches events
) -> TokenRiskReport
```

`events` may be empty (no detector fired in the window — valid; coverage report explains why).
`now` is the single wall-clock read; it does not affect any computed score fields.
All other output is a deterministic function of `events`, `meta`, `config`, and `window`.

### Output: `TokenRiskReport`

```rust
pub struct TokenRiskReport {
    /// Token address and chain.
    pub token: Address,
    pub chain: Chain,

    /// Time window this report covers.
    pub window: (DateTime<Utc>, DateTime<Utc>),

    /// Wall-clock time the report was produced. Only non-deterministic field.
    pub computed_at: DateTime<Utc>,

    /// Overall risk score ∈ [0.0, 1.0]. After attenuation.
    /// Use for quick threshold filtering.
    pub overall_score: Confidence,

    /// Pre-attenuation score (before jup/established/age multipliers).
    /// Useful for debugging and consumer overrides.
    pub base_score: Confidence,

    /// Worst-case severity across all fired events.
    /// Derived from AnomalyEvent.severity; not affected by attenuation.
    pub overall_severity: Severity,

    /// Per-detector breakdown. Keys are detector_id strings.
    /// BTreeMap for deterministic ordering.
    pub per_detector: BTreeMap<String, DetectorScore>,

    /// ≤ config.evidence_highlight_count highest-impact evidence entries.
    pub top_evidence: Vec<EvidenceHighlight>,

    /// Signal counts across all events in window.
    pub signal_counts: SignalCounts,

    /// Which detectors ran, which were skipped, and why.
    pub coverage: CoverageReport,

    /// The ScoringConfig used to produce this report.
    /// Stored for reproducibility auditing.
    pub config_snapshot: ScoringConfig,
}
```

### Supporting output types

```rust
pub struct DetectorScore {
    pub detector_id: String,

    /// Number of AnomalyEvent entries from this detector in the window.
    pub fired_events: u32,

    /// Events from this detector that were suppressed at the detector level
    /// (is_established_protocol suppressed them before reaching scoring).
    /// Count sourced from evidence metrics: detectors should emit a suppressed
    /// count in their evidence bundle when they suppress latent signals.
    pub suppressed_events: u32,

    /// Events where confidence < inconclusive_floor (config: default 0.30).
    pub inconclusive_events: u32,

    /// Max confidence among fired events (decay not applied — raw value).
    pub max_confidence: Confidence,

    /// Time-decay-weighted average confidence across fired events.
    /// Zero if no events fired.
    pub weighted_confidence: Confidence,

    /// Max severity among fired events.
    pub severity: Severity,

    /// Top 3 evidence key-value pairs by severity+confidence rank.
    /// Format: (metric_key, metric_value_string).
    pub evidence_summary: Vec<(String, String)>,
}

pub struct EvidenceHighlight {
    pub detector_id: String,
    pub severity: Severity,
    pub confidence: Confidence,  // raw, no decay
    pub key: String,             // evidence metric key, e.g. "rug_pull_lp_drain/lp_removed_pct"
    pub value: String,           // string-encoded Decimal value
    pub note: Option<String>,    // AnomalyEvent.evidence.notes[0] if present
}

pub struct SignalCounts {
    pub fired: u32,          // events with confidence >= config.inconclusive_floor
    pub inconclusive: u32,   // events with confidence < config.inconclusive_floor
    pub suppressed_info: u32, // events at Severity::Info (detector's own floor)
}

pub struct CoverageReport {
    pub detectors_run: Vec<String>,    // detector_ids that produced ≥1 AnomalyEvent in window
    pub detectors_skipped: Vec<SkipReason>,
}

pub struct SkipReason {
    pub detector_id: String,
    pub reason: String,  // e.g. "no swap events in window", "insufficient holder snapshots"
}
```

`SkipReason` entries are populated by the caller (gateway/server), not by scoring itself, since only the caller knows which detectors were actually invoked. The caller passes `skip_reasons: Vec<SkipReason>` as an additional parameter and scoring includes them verbatim in `CoverageReport`. This keeps scoring itself free of scheduling knowledge.

Full function signature with skips:

```
fn score(
    events:       &[AnomalyEvent],
    meta:         &TokenMeta,
    config:       &ScoringConfig,
    skip_reasons: &[SkipReason],
    now:          DateTime<Utc>,
    window:       (DateTime<Utc>, DateTime<Utc>),
) -> TokenRiskReport
```

---

## 3. Aggregation Strategy

### Candidates considered

Four aggregation strategies were evaluated:

**Option 1 — Severity max + confidence of that event.** Simplest. Reports the worst event found. Discards all other signal. Misses cumulative risk: three independent Medium events never aggregate above the worst Medium. Rejected.

**Option 2 — Weighted sum with severity multipliers.** Each event contributes `detector_weight × confidence × decay`. Detector importance weights are fixed per-detector constants (sum=1.0). Score is a convex combination of confidences, weighted by how important that detector's signal type is relative to the risk decision. Naturally handles cumulative risk: more fired detectors produce higher aggregated scores. Denominator is 1.0 (weights already sum to 1.0), so score ∈ [0, 1] without normalization. Selected.

**Option 3 — Max severity banding + weighted confidence within that band.** Attempts to use the severity ladder as the primary consumer signal. In calibration: when two detectors fire at Critical (as in RAVE), the top-band average confidence with cross-band boost produces a score of 1.0 (clamped) — overshoots the calibrated 0.83 anchor. Rejected in favour of Option 2.

**Option 4 — Bayesian ensemble / probabilistic.** Requires per-detector precision/recall calibration. Phase 2 corpus (100 positives, 99 negatives) is sufficient for TPR/FPR estimation but not for the per-class likelihood ratios needed for a proper Bayesian combiner. Deferred to Phase 6 when corpus grows to ≥1,000 labelled examples.

### Adopted: Option 2 with probe-derived fixed detector importance weights

The RAVE and WET probe reports each independently derived the same aggregation formula:

```
base_score = Σ (w_i × c_i × d_i)
             i = D01..D06
```

Where:
- `w_i` = detector importance weight (fixed config; must sum to 1.0)
- `c_i` = `AnomalyEvent.confidence.value()` for detector i (0.0 if no event fired in window)
- `d_i` = time decay factor for event-based detectors; 1.0 for state-based

When multiple events exist for the same detector in the window (rare but possible — e.g. D05 may fire on multiple pool addresses), use the maximum-confidence event for that detector. The `DetectorScore.weighted_confidence` field captures the time-decay-weighted average for the breakdown, but only `max_confidence` feeds the aggregation formula.

**Calibration result:** With the probe-derived weights and age-1h events, the formula produces:
- RAVE: 0.827 (rounds to 0.83) — matches target
- WET: 0.308 (rounds to 0.31) — matches target

### Detector importance weights

Derived from the RAVE and WET probe aggregation sections. These reflect the relative importance of each detector TYPE to the overall risk decision — not the severity of any individual event output.

| Detector | `detector_id` | Importance Weight | Rationale |
|----------|---------------|-------------------|-----------|
| D03 Holder Concentration | `holder_concentration` | 0.35 | Strongest pre-collapse signal (TM-RugPull 2026); fires on structural fraud state regardless of timing; highest raw confidence in RAVE (0.95) and WET (0.55) — empirically most predictive |
| D04 Pump & Dump | `pump_dump` | 0.35 | Highest-frequency fraud category (Chainalysis 2025: 3.59% of tokens); when it fires, it fires at high confidence (RAVE: 0.92); equal weight to D03 per probe calibration |
| D02 Rug Pull | `rug_pull_lp_drain` | 0.20 | Structural precursor for most costly consumer outcome; latent signal (Signal B) leads the drain event; weight reduced from D03/D04 because it fires at lower confidence when latent |
| D05 Wash Trading | `wash_trading_h1` | 0.07 | Supportive signal; frequently inconclusive (WET: 0.25, RAVE: 0.45); important to MM consumer but lower base-rate than manipulation signals |
| D01 Honeypot | `honeypot_sim` | 0.015 | When it fires at high confidence (simulation confirmed), it is Critical — but the weight is low because it fires cleanly or not at all; a confirmed honeypot at 0.95 confidence contributes 0.014 to overall score, relying on the Critical severity escalating consumer action rather than score elevation |
| D06 Mint/Burn | `mint_burn_anomaly` | 0.015 | Typically fires at low confidence for latent state; high-confidence events are rare but Critical; same reasoning as D01 |

Sum: 0.35 + 0.35 + 0.20 + 0.07 + 0.015 + 0.015 = 1.0

These weights are configurable via `config/scoring.toml` under `[detector_weights]`. Changing them requires re-running the calibration test suite (see §12). The default values above are the P5-1 calibrated baseline.

### Handling D01 / D06 at Critical

The low importance weights for D01 and D06 mean a confirmed honeypot (D01 confidence=0.95, severity=Critical) contributes only 0.014 to `base_score`. This is intentional: the `overall_severity` field (which reflects the worst severity seen, regardless of score) escalates to `Critical`, which is the correct consumer signal. Consumers MUST check `overall_severity` for action thresholds, not only `overall_score`. The score drives ranking and filtering across a portfolio; the severity drives per-token action policy.

---

## 4. Time Decay

### Rationale

Event-based detector signals (D04 pump/dump volume spike, D05 wash trading round trips) are tied to specific on-chain activity that becomes less relevant as time passes. A pump that occurred 120 hours ago is less actionable than one from 6 hours ago. State-based signals (D01 freeze authority, D02 latent LP state, D03 holder snapshot, D06 mint authority) reflect the CURRENT on-chain state of the token — they do not decay because the risk is present NOW regardless of when it was first detected.

### Decay classification

| Detector | Signal type | Decays? |
|----------|-------------|---------|
| D01 S1–S4 (freeze, fee, delegate, hook) | State-based | No |
| D01 S5 (buy/sell ratio) | Event-derived from rolling count | Yes |
| D02 Signal A (LP drain event) | Event-based | Yes |
| D02 Signal B (latent LP state) | State-based | No |
| D03 (all signals) | State-based (snapshot) | No |
| D04 Signal A/B (volume/price spike) | Event-based | Yes |
| D04 Signal C (insider sell-off) | Hybrid: event-based sell txs | Yes |
| D05 Signal A (H1 round trips) | Event-based | Yes |
| D05 Signal B (cluster wash) | State-based proxy | No |
| D06 Signal A (mint authority static) | State-based | No |
| D06 Signal B/C (mint/burn events) | Event-based | Yes |

In practice, the detector that fired is the source of truth on signal type. The scoring crate cannot inspect the signal type from an `AnomalyEvent` directly; it uses the detector ID as a proxy. If a detector emits a mixed event (both state-based and event-based components), it SHOULD emit two separate `AnomalyEvent` values with distinct `detector_id` values (e.g. `"rug_pull_lp_drain_event"` and `"rug_pull_lp_drain_latent"`). Scoring applies decay based on a per-detector-id decay classification table in `ScoringConfig`.

### Decay formula

```
decay(age_hours, half_life_hours) = exp(- age_hours × ln(2) / half_life_hours)
```

At `age = 0h`: decay = 1.0.
At `age = half_life_hours`: decay = 0.5.
At `age = 2 × half_life_hours`: decay = 0.25.

Default `half_life_hours = 72` (3 days). Rationale: Chainalysis (2025) reports average pump-and-dump cycle duration of 6.23 days; half of that is ~3 days, meaning an event that is 3 days old is at half-weight — still meaningful but no longer dominant. LROO (2026) documents that >95% of rugged tokens complete the drain within 1–3 days; at half_life=72h, a drain event from 3 days ago retains 50% weight, which is appropriate.

`age_hours = (window.end - event.observed_at).num_seconds() / 3600.0`

where `event.observed_at` is the block-time timestamp (per `AnomalyEvent.observed_at` contract — not wall clock).

### State-based signals and decay

State-based signals always use `decay = 1.0`. The `observed_at` timestamp for a state-based event reflects when the state was last read, not when it became dangerous. An active freeze authority is equally dangerous whether we read it 1 hour or 60 hours ago; the risk is present NOW.

---

## 5. Attenuation Layer

After computing `base_score`, the scoring engine applies a series of multiplicative attenuation factors. These represent non-detector context signals that reduce the prior probability that a token is malicious, independent of the detector outputs.

### Design principle

Attenuation is a SECOND PASS on top of per-detector `is_established_protocol` suppression. The detector-level suppression eliminates specific false-positive signals at the source. The attenuation layer reduces the final score for tokens where the overall base-rate of fraud is lower, even if individual detectors still fire at non-trivial confidence. Consumers can opt out of individual attenuations via config flags.

The attenuation order matters because all factors are multiplied together. They are applied in the order listed below.

### Attenuation factors

#### A1 — `jup_strict_multiplier` (default: **0.30**)

Applied when `meta.verification.jup_strict == true`.

Jupiter's strict list is curated by active human review and requires social proof of legitimacy. A token cannot appear on the strict list before being rugged (inclusion lag of weeks to months). This is the strongest single non-detector attenuator. Default 0.30 means a jup_strict token's score is capped at 30% of the base score.

Carried from DG4 in `docs/designs/0004-detector-01-honeypot.md` §6 where it was flagged as a "Phase 5 scoring crate concern." Now formally resolved here.

#### A2 — `jup_verified_multiplier` (default: **0.60**)

Applied when `meta.verification.jup_verified == true` AND `jup_strict == false`.

If `jup_strict == true`, A1 has already applied; A2 would compound incorrectly. Only one of A1, A2 fires per token.

Jupiter verified (non-strict) is a weaker signal: self-reported metadata + automated checks. Legitimate use for this attenuator: USDC, wSOL, and other widely-traded verified tokens should not score high even if a structural signal fires on their large LP positions. Default 0.60 means verified tokens retain 60% of base score.

#### A3 — `established_protocol_multiplier` (default: **0.50**)

Applied when `crates/detectors::token_status::is_established_protocol(meta) == true`.

This is the same predicate used in the detectors for latent-signal suppression (per `docs/designs/0003-detector-trait.md` §Established-protocol suppression pattern and `docs/designs/0005-detector-02-rug-pull.md` §14). In scoring, it acts as a belt-and-suspenders safety net: even if event-based signals (which are NOT suppressed at the detector level) still fire on an established protocol, the overall score is halved.

Example: RAY (Raydium governance token) fires D02 Signal B at the detector level before suppression kicks in. Scoring applies A3 to further reduce the score to 50% of whatever base remains after detector-level suppression has already reduced the signal count.

Default 0.50. Rationale: LROO (2026) confirms >95% rug events are non-established protocols. The base-rate of fraud for established protocols is at least an order of magnitude lower; a 50% score reduction is a conservative representation of that prior.

Note on interaction with A1/A2: most established protocols also have `jup_strict` or `jup_verified`. Only the highest-precedence attenuator that applied (A1 or A2) is compounded with A3. The guard is: if `jup_strict` applied A1 AND `is_established_protocol` is also true, apply BOTH A1 and A3 because they are independent facts about the token.

#### A4 — `token_age_multiplier` (default: **disabled, value 1.0**)

Applied as a piecewise function of `meta.detected_at` (token age in days).

Default is disabled (multiplier = 1.0 at all ages) because calibration shows that WET at 137 days already scores correctly at 0.31 WITHOUT age discounting. Applying a non-trivial age discount would push WET below 0.27 — below the calibrated target. The multiplier is included in config for consumer opt-in.

When a consumer enables it, the recommended piecewise function is:

```
f(age_days) = young_multiplier            if age < young_cutoff_days
            = lerp(young_multiplier,
                   mature_multiplier,
                   (age - young_cutoff) / (mature_cutoff - young_cutoff))
                                           if young_cutoff <= age < mature_cutoff
            = mature_multiplier            if age >= mature_cutoff
```

Default params (when enabled):
- `young_cutoff_days = 30` (token younger than 30 days: no discount)
- `mature_cutoff_days = 365`
- `young_multiplier = 1.0` (no change for young tokens)
- `mature_multiplier = 0.75` (25% discount for tokens > 365 days)

The young_multiplier = 1.0 for tokens under 30 days is critical: new tokens are the HIGHEST risk; discounting them would be dangerous. The mature discount reflects that a token which has survived a year of trading without rugging has a substantially lower rug-base-rate.

`meta.detected_at = None` (unknown age) → multiplier = 1.0 (no discount; unknown age does not earn the benefit of maturity).

### Attenuation application order and formula

```
overall_score = base_score × A1_factor × A3_factor × A4_factor
```

Where:
- `A1_factor = jup_strict_multiplier`   if `jup_strict == true`, else 1.0
- `A2_factor = jup_verified_multiplier` if `jup_verified && !jup_strict`, else 1.0
- The actual formula uses `max(A1_factor, A2_factor)` — NOT both simultaneously for the jup factors — to avoid doubling:

```
jup_factor    = jup_strict_multiplier   if jup_strict
              = jup_verified_multiplier if jup_verified (and not strict)
              = 1.0                     otherwise

overall_score = base_score × jup_factor × A3_factor × A4_factor
```

All factors are in (0, 1]; overall_score is clamped to [0, 1].

### Attenuation transparency

The `TokenRiskReport` includes both `base_score` (pre-attenuation) and `overall_score` (post-attenuation) so consumers can see what the attenuators did. A consumer with different trust in Jupiter verification can apply their own post-hoc correction.

---

## 6. Per-Detector Score Computation

For each known detector ID in `ScoringConfig.detector_weights`:

1. Collect all `AnomalyEvent` entries for this detector from the input slice.
2. If none: `fired_events = 0`, `weighted_confidence = Confidence::ZERO`, `severity = Severity::Info`.
3. If one or more:
   a. Compute `effective_confidence(e) = e.confidence.value() × decay_factor(e)`.
   b. `max_confidence = max(e.confidence)` over raw values (no decay — for human display).
   c. `weighted_confidence = Σ(effective_confidence) / count` (simple average of decay-adjusted values).
   d. `severity = max(e.severity)` over all events (Severity implements Ord).
   e. `fired_events = count_where(effective_confidence >= config.inconclusive_floor)`.
   f. `inconclusive_events = count_where(effective_confidence < config.inconclusive_floor)`.
4. For `aggregation_confidence` (fed into the global formula): use the single highest-effective-confidence event for this detector, NOT the average. This matches the probe's implicit per-detector representation of "best evidence for this detector type."
5. For `evidence_summary`: collect top 3 `(key, value)` pairs from `event.evidence.metrics` ranked by `(event.severity ordinal DESC, event.confidence DESC)`.

### Suppressed event count

Detectors that implement `is_established_protocol` suppression emit a metric in their evidence bundle: `"<detector_id>/suppressed_count"` when they suppress latent signals. The `DetectorScore.suppressed_events` field reads this metric key from the evidence of the highest-confidence event for this detector. If the key is absent, `suppressed_events = 0`.

---

## 7. Top Evidence Ranking

`top_evidence` selects up to `config.evidence_highlight_count` (default 5) entries across ALL events from ALL detectors.

### Ranking algorithm

1. Collect all `(key, Decimal_value)` pairs from `evidence.metrics` across every `AnomalyEvent` in the window.
2. Assign a sort key to each pair: `(severity_ordinal DESC, confidence DESC, key ASC)`.
3. Deduplicate by `(detector_id, key)`: keep only the highest-ranked occurrence of each unique (detector, key) pair.
4. Take the top N entries.

### EvidenceHighlight construction

```
EvidenceHighlight {
    detector_id: event.detector_id,
    severity:    event.severity,
    confidence:  event.confidence,   // raw, no decay
    key:         metric_key,
    value:       metric_value.to_string(),
    note:        event.evidence.notes.first().cloned(),
}
```

### Determinism

Step 2 uses a stable sort over `(severity_ordinal DESC, confidence DESC, key ASC)`. The `key ASC` tiebreaker ensures identical output given identical inputs. No HashMap iteration is used in this algorithm; all collections are sorted before selection.

---

## 8. Coverage Report

The `CoverageReport` answers: "Which detectors had the opportunity to fire, and which were skipped?"

```
CoverageReport {
    detectors_run:     Vec<String>,       // detector_ids that produced ≥1 AnomalyEvent in window
    detectors_skipped: Vec<SkipReason>,   // passed in by caller; see §2
}
```

`detectors_run` is derived entirely from the input `events` slice: `events.iter().map(|e| e.detector_id.clone()).collect::<BTreeSet<_>>().into_iter().collect()`.

The caller constructs `skip_reasons` based on the scheduling logic in `crates/server` or `crates/gateway`. Example skip reasons:

- `"no swap events in window (required for D04/D05)"`
- `"insufficient holder snapshots: only 1 snapshot, need ≥2 for delta signals"`
- `"D01 simulation disabled (config: simulation_enabled=false)"`
- `"pool below min_pool_usd threshold ($450 < $1,500)"`

This design gives consumers the critical distinction between "this detector fired zero events because the signal is clean" vs "this detector was never run because the required data was absent." The second case is a data gap that reduces trust in the overall score.

### Coverage completeness score (informational)

The number of known detectors that either ran OR had a deliberate skip reason, divided by the total known detector count, gives a coverage completeness percentage. This is informational only (not part of `overall_score`). Include in `CoverageReport`:

```
pub coverage_completeness: f32,   // 0.0–1.0; 1.0 = all 6 detectors covered
```

---

## 9. Configuration

### Location

`config/scoring.toml`. Loaded at startup by `crates/server` via `ScoringConfig::from_toml`. Follows the `{ value, rationale, refs }` three-key convention established in `config/detectors.toml`.

### `ScoringConfig` shape

```rust
pub struct ScoringConfig {
    /// Per-detector importance weights for the aggregation formula.
    /// Must sum to 1.0 (enforced at deserialization time).
    pub detector_weights: DetectorWeights,

    /// Exponential decay half-life in hours for event-based signals.
    /// State-based signals always use decay = 1.0 (ignore this value).
    pub decay_half_life_hours: f64,

    /// Detector IDs classified as "state-based" (no decay).
    /// Any detector_id not in this list is treated as event-based (decays).
    pub state_based_detectors: Vec<String>,

    /// Multiplier for jup_strict tokens.
    pub jup_strict_multiplier: f64,

    /// Multiplier for jup_verified (non-strict) tokens.
    pub jup_verified_multiplier: f64,

    /// Multiplier for tokens where is_established_protocol() returns true.
    pub established_protocol_multiplier: f64,

    /// Token age attenuation parameters.
    /// Set young_multiplier = mature_multiplier = 1.0 to disable.
    pub token_age: TokenAgeAttenuationConfig,

    /// Confidence floor below which an event is classified as "inconclusive."
    pub inconclusive_floor: f64,

    /// Maximum number of entries in top_evidence.
    pub evidence_highlight_count: usize,
}

pub struct DetectorWeights {
    pub honeypot_sim:         f64,
    pub rug_pull_lp_drain:    f64,
    pub holder_concentration: f64,
    pub pump_dump:            f64,
    pub wash_trading_h1:      f64,
    pub mint_burn_anomaly:    f64,
}

pub struct TokenAgeAttenuationConfig {
    pub young_cutoff_days:  u32,   // age < this → young_multiplier
    pub mature_cutoff_days: u32,   // age >= this → mature_multiplier
    pub young_multiplier:   f64,   // 1.0 = no discount for young tokens
    pub mature_multiplier:  f64,   // 1.0 = disabled; <1.0 = age discount
}
```

### `config/scoring.toml` default content

```toml
# config/scoring.toml
#
# Scoring aggregation configuration for crates/scoring.
#
# Format: every numeric parameter is a TOML table with:
#   value     — the numeric value
#   rationale — human-readable justification
#   refs      — REFERENCES.md entry IDs
#
# ADR refs: ADR 0001 §D4 (confidence+severity contract), ADR 0001 §D8 (three delivery modes)
# Design ref: docs/designs/0010-scoring.md

# ---------------------------------------------------------------------------
# Detector importance weights — sum MUST equal 1.0
# Calibrated against RAVE probe (0.83/Critical) and WET probe (0.31/Medium)
# Source: research/token-probes/rave-FeqiF7TE.md §3 and wet-WETZjtp.md §3
# ---------------------------------------------------------------------------

[detector_weights.holder_concentration]
value     = 0.35
rationale = """Strongest pre-collapse signal per TM-RugPull 2026 and Brown 2023. RAVE
              probe fired at 0.95 (Critical); WET at 0.55 (Medium). Empirically the most
              predictive detector in the Phase 2 corpus across positive categories. Equal
              weight with pump_dump per probe calibration derivation."""
refs      = ["SCORING/detector_weights"]

[detector_weights.pump_dump]
value     = 0.35
rationale = """Highest-frequency fraud category (Chainalysis 2025: 3.59% of tokens).
              Fires at high confidence when activated (RAVE: 0.92 Critical). Equal weight
              with holder_concentration per probe calibration. Together these two dominate
              the score because they represent the two most financially costly false-negative
              modes for bot-trader-2-0."""
refs      = ["SCORING/detector_weights"]

[detector_weights.rug_pull_lp_drain]
value     = 0.20
rationale = """Structural precursor for the most costly consumer outcome (LP drain).
              Lower weight than D03/D04 because latent Signal B fires at moderate confidence
              (RAVE: 0.72; WET: 0.28); the 0.20 weight reflects this middle-tier importance.
              Source: RAVE probe §3 weighting derivation."""
refs      = ["SCORING/detector_weights"]

[detector_weights.wash_trading_h1]
value     = 0.07
rationale = """Supportive signal; frequently inconclusive without tx-level data (RAVE: 0.45;
              WET: 0.25). Critical for market-maker consumer but lower base-rate importance
              for overall risk scoring. Source: RAVE probe §3 weighting derivation."""
refs      = ["SCORING/detector_weights"]

[detector_weights.honeypot_sim]
value     = 0.015
rationale = """When confirmed, honeypot is Critical severity — consumer action is driven by
              the severity field, not the score contribution. Low weight reflects that
              honeypot fires cleanly (high confidence) or not at all; overall_severity
              escalation is the primary consumer signal for this detector."""
refs      = ["SCORING/detector_weights"]

[detector_weights.mint_burn_anomaly]
value     = 0.015
rationale = """Typically fires at low confidence for latent state (mint authority present);
              high-confidence events are rare but Critical. Same reasoning as honeypot_sim:
              overall_severity escalation is the primary consumer signal."""
refs      = ["SCORING/detector_weights"]

# ---------------------------------------------------------------------------
# Time decay
# ---------------------------------------------------------------------------

[decay_half_life_hours]
value     = 72.0
rationale = """Chainalysis 2025 reports average pump-and-dump cycle duration 6.23 days.
              Half of that (3 days = 72 hours) means an event 3 days old retains 50%
              weight — still meaningful but no longer dominant. LROO 2026 confirms >95% of
              rugged tokens complete the drain within 1–3 days; a drain event at 72h retains
              50% weight, appropriate for ongoing risk monitoring."""
refs      = ["SCORING/decay"]

# Detectors whose signals are state-based and do not decay.
# Any detector_id NOT in this list is treated as event-based (decay applies).
state_based_detectors = [
    "honeypot_sim_static",       # D01 S1-S4: freeze/fee/delegate/hook (structural)
    "rug_pull_lp_drain_latent",  # D02 Signal B: LP state (structural)
    "holder_concentration",      # D03: all signals are snapshot-based
    "mint_burn_anomaly_static",  # D06 Signal A: mint authority present (structural)
]
# Note: "honeypot_sim" (with simulation S5/S6) IS event-based and decays.
# Detectors that emit both state and event signals should use separate detector_ids.

# ---------------------------------------------------------------------------
# Attenuation
# ---------------------------------------------------------------------------

[jup_strict_multiplier]
value     = 0.30
rationale = """Jupiter strict list requires active human curation + social proof.
              A scam token cannot appear before rugging (months of inclusion lag).
              0.30 = 70% reduction; even if D02/D03 fire on a jup_strict token (e.g.
              USDC with active freeze authority or USDT with managed LP), the overall
              score stays below 0.30 — below any reasonable alert threshold.
              Derived from DG4 in docs/designs/0004-detector-01-honeypot.md §6."""
refs      = ["SCORING/attenuation"]

[jup_verified_multiplier]
value     = 0.60
rationale = """Jupiter verified (non-strict) is a weaker single signal than strict listing.
              0.60 = 40% reduction. Calibration: WET is NOT jup_verified in the probe,
              so this factor did not apply to the WET calibration anchor. Value chosen
              to ensure a legitimate verified token with one D03 Medium firing (e.g. wSOL
              with pool concentration) scores < 0.40 — below action thresholds.
              Derived from DG4 in docs/designs/0004-detector-01-honeypot.md §6."""
refs      = ["SCORING/attenuation"]

[established_protocol_multiplier]
value     = 0.50
rationale = """LROO 2026 confirms >95% of rug events are non-established protocols.
              0.50 = 50% reduction. Belt-and-suspenders on top of detector-level
              is_established_protocol suppression (token_status.rs). Established protocols
              still emit event-based signals (D04 can fire on news-driven spikes for RAY);
              this multiplier reduces their score without silencing the signal.
              Source: docs/designs/0005-detector-02-rug-pull.md §14 design rationale."""
refs      = ["SCORING/attenuation"]

# Token age attenuation — disabled by default (both multipliers = 1.0)
[token_age]
young_cutoff_days  = 30
mature_cutoff_days = 365

[token_age.young_multiplier]
value     = 1.0
rationale = """Tokens under 30 days are highest risk (new launches are primary scam
              category in Chainalysis 2025 data). No discount applied to new tokens.
              DO NOT lower this value without corpus evidence."""
refs      = ["SCORING/attenuation"]

[token_age.mature_multiplier]
value     = 1.0
rationale = """Disabled by default. Set to 0.75 to apply a 25% discount for tokens
              older than 365 days. Disabled because calibration shows WET at 137 days
              already scores correctly at 0.31 without age discount; applying discount
              would push it to 0.27, below the calibrated target.
              Enable only after corpus reaches ≥500 labelled tokens with age data."""
refs      = ["SCORING/attenuation"]

# ---------------------------------------------------------------------------
# Miscellaneous
# ---------------------------------------------------------------------------

[inconclusive_floor]
value     = 0.30
rationale = """Events with confidence < 0.30 are classified as inconclusive in
              SignalCounts. 0.30 is the midpoint between noise (< 0.20) and a weak
              positive signal. RAVE wash_trading confidence (0.45) is above this
              floor (correctly counted as fired); WET wash_trading (0.25) is below
              (correctly counted as inconclusive). Calibrated against both probes."""
refs      = ["SCORING/misc"]

[evidence_highlight_count]
value     = 5
rationale = """Maximum 5 evidence highlights is consistent with human attention span
              for triage decisions. The top-evidence section is for human review,
              not machine consumption. 5 matches the RugCheck risk display convention
              (top risks shown in UI). Configurable up to 10 for exchange compliance
              workflows that need full audit trail."""
refs      = ["SCORING/misc"]
```

---

## 10. Calibration Anchors

### Anchor 1 — RAVE (`FeqiF7TEVmpYuuj4goD8WgmgyhFgFnZiWtw226wF4hNm`)

**Probe verdict:** 0.83 / Critical

**Expected scoring crate output** (all events age ~1h, no attenuation applies — no jup flags, not established protocol):

| Detector | Confidence | Severity | Decays? | Decay@1h | Contribution |
|----------|-----------|---------|---------|----------|--------------|
| D01 honeypot_sim | 0.03 | Info | No | 1.000 | 0.015 × 0.03 × 1.0 = 0.00045 |
| D02 rug_pull_lp_drain_latent | 0.72 | High | No | 1.000 | 0.20 × 0.72 × 1.0 = 0.14400 |
| D03 holder_concentration | 0.95 | Critical | No | 1.000 | 0.35 × 0.95 × 1.0 = 0.33250 |
| D04 pump_dump | 0.92 | Critical | Yes | 0.9903 | 0.35 × 0.92 × 0.9903 = 0.31891 |
| D05 wash_trading_h1 | 0.45 | Medium | Yes | 0.9903 | 0.07 × 0.45 × 0.9903 = 0.03119 |
| D06 mint_burn_anomaly | 0.02 | Info | No | 1.000 | 0.015 × 0.02 × 1.0 = 0.00030 |

`base_score = 0.00045 + 0.14400 + 0.33250 + 0.31891 + 0.03119 + 0.00030 = 0.8274`
`overall_score = 0.8274 × 1.0 × 1.0 × 1.0 = 0.827` → rounds to **0.83** ✓
`overall_severity = Critical` ✓ (D03 and D04 both fire Critical)

**Acceptance tolerance:** `overall_score ∈ [0.80, 0.86]`, `overall_severity == Critical`.

### Anchor 2 — WET (`WETZjtprkDMCcUxPi9PfWnowMRZkiGGHDb9rABuRZ2U`)

**Probe verdict:** 0.31 / Medium

**Expected scoring crate output** (all events age ~1h, no attenuation — no jup flags confirmed, not established protocol):

| Detector | Confidence | Severity | Decays? | Decay@1h | Contribution |
|----------|-----------|---------|---------|----------|--------------|
| D01 honeypot_sim | 0.02 | Info | No | 1.000 | 0.015 × 0.02 × 1.0 = 0.00030 |
| D02 rug_pull_lp_drain_latent | 0.28 | Low | No | 1.000 | 0.20 × 0.28 × 1.0 = 0.05600 |
| D03 holder_concentration | 0.55 | Medium | No | 1.000 | 0.35 × 0.55 × 1.0 = 0.19250 |
| D04 pump_dump | 0.12 | Info | Yes | 0.9903 | 0.35 × 0.12 × 0.9903 = 0.04159 |
| D05 wash_trading_h1 | 0.25 | Info | Yes | 0.9903 | 0.07 × 0.25 × 0.9903 = 0.01733 |
| D06 mint_burn_anomaly | 0.02 | Info | No | 1.000 | 0.015 × 0.02 × 1.0 = 0.00030 |

`base_score = 0.00030 + 0.05600 + 0.19250 + 0.04159 + 0.01733 + 0.00030 = 0.3080`
`overall_score = 0.3080 × 1.0 × 1.0 × 1.0 = 0.308` → rounds to **0.31** ✓
`overall_severity = Medium` ✓ (D03 fires Medium, no Critical events)

**Acceptance tolerance:** `overall_score ∈ [0.27, 0.35]`, `overall_severity == Medium`.

### Decay sensitivity at 1h vs 0h

Decay at age=1h with half_life=72h:
`exp(-1 × ln(2) / 72) = exp(-0.00963) ≈ 0.9904`

At 1h, event-based signals retain 99% of their weight. The RAVE and WET probes represent near-real-time detection; 1h age is the standard calibration input.

### Why probe weights reproduce the probe scores

The probe authors independently derived per-detector weights by considering: (a) which detectors are most empirically grounded (strongest citations), (b) which false-negative mode is most costly per consumer. The resulting weights [0.35, 0.35, 0.20, 0.07, 0.015, 0.015] happen to be derivable from the principle that D03 and D04 together dominate risk assessment (combined weight 0.70 = same as their combined share of pump-and-dump + concentration research citations).

This is not a coincidence: the probes were the calibration data, so the scoring design that reproduces them is the correct baseline. Future calibration against the 200-fixture corpus (100 positive + 99 negative from `research/fixtures/solana-corpus-phase2.md`) should NOT change the weights unless there is a statistically significant finding.

---

## 11. Testability and Determinism

### Pure function contract

`ScoringEngine::score()` is a pure function with no I/O, no global state, no random numbers, and no wall-clock reads except `now` (which only populates `computed_at`). Given identical `(events, meta, config, skip_reasons, now, window)`, the function MUST return identical `TokenRiskReport` byte-for-byte.

### No HashMap in any scoring path

All collections that contribute to output use `BTreeMap` or sorted `Vec`. `BTreeMap` is already enforced in `Evidence::metrics` by the frozen `common` types. In scoring code: any intermediate `HashMap` for grouping events by detector must be replaced with a sorted-insertion pattern or collected into a `BTreeMap` before use.

### Ordering guarantees required of inputs

`events` may arrive in any order from the Postgres query. Scoring MUST sort events before processing: `events.sort_by_key(|e| (&e.detector_id, e.observed_at))`. This sort must be stable to preserve relative ordering of events with identical `(detector_id, observed_at)`.

### Decimal for evidence metrics

`EvidenceHighlight.value` is a string-encoded `Decimal`, never an `f64`. The `Confidence` type's `f64` inner value is only used for arithmetic in the aggregation formula, with results assigned to new `Confidence` values via `Confidence::new(v.clamp(0.0, 1.0)).unwrap()`. No intermediate floating-point result is stored in a stable output field.

### Test structure

The developer implements the following test modules in `crates/scoring/src/`:

1. **Calibration anchor tests:** Two tests, one per probe. Input: hand-constructed `Vec<AnomalyEvent>` matching the probe tables in §10. Assert: `overall_score.value()` within tolerance, `overall_severity` matches. These are regression-guard tests — they pin the calibrated formula and prevent silent drift.

2. **Decay function unit tests:** Test `decay(0.0, 72.0) == 1.0`, `decay(72.0, 72.0) ≈ 0.5`, `decay(144.0, 72.0) ≈ 0.25`. Pure math; no AnomalyEvent dependency.

3. **Attenuation stack tests:** Test each attenuator in isolation and combinations. Verify: `jup_strict → overall_score = base × 0.30`, `established → overall_score = base × 0.50`, `jup_strict && established → base × 0.30 × 0.50`.

4. **Evidence ranking test:** Construct 8 events with known severity+confidence values; assert that `top_evidence` returns the top 5 in the correct deterministic order.

5. **Empty events test:** `events = []` → `overall_score = 0.0`, `overall_severity = Info`, `per_detector` has all detectors with `fired_events = 0`.

6. **Determinism test:** Call `score()` twice with identical inputs (different `now`); assert all fields except `computed_at` are identical byte-for-byte.

7. **BTreeMap ordering test:** Construct events whose detector_ids are not in alphabetical order; assert `per_detector` keys are sorted alphabetically.

8. **Weight sum validation test:** `ScoringConfig::from_toml()` returns `Err` if detector weights do not sum to 1.0 within tolerance (1e-6). Test: `[0.35, 0.35, 0.20, 0.07, 0.015, 0.015]` sums to 1.0 and passes; `[0.35, 0.35, 0.20, 0.07, 0.015, 0.020]` sums to 1.005 and fails.

### No live database or network in tests

Tests use hand-constructed `AnomalyEvent` values and hand-constructed `TokenMeta` values. No `DetectorContext`, no `PgStore`, no Yellowstone connection. All dependency on external infrastructure is in the detector crates, not in `crates/scoring`.

---

## 12. Streaming Shape

### Decision: scoring remains stateless; WS gateway re-invokes per update tick

The gateway WS handler maintains a subscription map of `(chain, token) → Vec<WsSubscriber>`. When a new `AnomalyEvent` arrives from the indexer for token T:

1. The gateway fetches recent events for T from Postgres (last 24h window, configurable).
2. The gateway calls `ScoringEngine::score()` with the refreshed event slice.
3. The resulting `TokenRiskReport` is serialized and dispatched to all subscribers for T.

This means scoring is called every time a new event arrives for a tracked token. At steady-state (a few events per minute per token), this is cheap: scoring is O(n) in the number of events in the window, which is small. A token generating 100 events in 24h invokes scoring at most 100 times/day — negligible.

### Why not incremental scoring

Incremental scoring (update the score without re-running from scratch) would require state — the previous window's aggregation intermediate values. This would couple the scoring crate to the gateway state machine, breaking the pure-function property and making determinism testing harder. The cost of recomputing from scratch is low enough that purity wins.

### Cache layer in gateway

The gateway SHOULD implement a short-lived in-memory cache: `(chain, token) → (TokenRiskReport, computed_at)` with TTL = `min(decay_half_life_hours / 10, 30_seconds)`. Cache invalidation on new `AnomalyEvent` arrival. This prevents duplicate scoring invocations when multiple events arrive in a burst (e.g. D02 + D03 both fire within the same second).

The cache TTL is NOT in `ScoringConfig` — it is a gateway concern. Scoring itself has no TTL concept.

---

## 13. Crate Layout

```
crates/scoring/
  Cargo.toml
  src/
    lib.rs          # pub use ScoringEngine, TokenRiskReport, ScoringConfig, ...
    engine.rs       # ScoringEngine::score() — the primary public function
    aggregation.rs  # base_score computation: detector weight × confidence × decay
    decay.rs        # decay() function + state_based_detectors classification
    attenuation.rs  # jup/established/age multiplier stack
    evidence.rs     # top_evidence ranking and EvidenceHighlight construction
    coverage.rs     # CoverageReport assembly
    config.rs       # ScoringConfig, DetectorWeights, TokenAgeAttenuationConfig
    types.rs        # TokenRiskReport, DetectorScore, EvidenceHighlight, SignalCounts
```

`crates/scoring` depends on:
- `crates/common` (AnomalyEvent, Severity, Confidence, Evidence, TokenMeta)
- `crates/detectors` (is_established_protocol only; imported as a pure function)

`crates/scoring` does NOT depend on:
- `crates/storage` (no I/O)
- `crates/gateway` (no transport)
- `crates/chain-adapter` (no chain logic)

This is the correct dependency direction: scoring is below gateway and above common, parallel to detectors.

---

## 14. Developer Acceptance Checklist

The developer task for P5-1 is complete when all of the following pass:

- [ ] `cargo test -p onchain-scoring` passes with zero failures.
- [ ] RAVE calibration anchor test: `overall_score ∈ [0.80, 0.86]`, `overall_severity == Critical`.
- [ ] WET calibration anchor test: `overall_score ∈ [0.27, 0.35]`, `overall_severity == Medium`.
- [ ] Decay unit test: `decay(72.0, 72.0)` within 1e-6 of 0.5.
- [ ] Attenuation stack: `jup_strict_multiplier = 0.30` reduces RAVE base_score to ≤ 0.26.
- [ ] Empty events: `overall_score == 0.0`, `overall_severity == Severity::Info`.
- [ ] Determinism test: two identical calls return identical output (excluding `computed_at`).
- [ ] BTreeMap ordering: `per_detector` keys are in alphabetical order.
- [ ] Weight validation: `ScoringConfig` with weights summing to 1.005 fails deserialization.
- [ ] `ScoringEngine::score()` has zero external I/O (no `tokio::spawn`, no `sqlx`, no `reqwest`).
- [ ] No `HashMap` in any path from `score()` inputs to `TokenRiskReport` fields.
- [ ] `config/scoring.toml` file committed with all required `{ value, rationale, refs }` tables.
- [ ] `crates/scoring/Cargo.toml` added to workspace `[members]` in root `Cargo.toml`.
- [ ] `crates/scoring` does not import `crates/storage`, `crates/gateway`, or `crates/chain-adapter`.
- [ ] `TokenRiskReport` serializes cleanly for all three delivery modes: in-process (no-alloc clone), REST (serde JSON roundtrip), WS (same JSON).

---

## 15. Open Questions

**OQ1 — Multi-event aggregation for the same detector (use max or average?)**

The design specifies using the max-confidence event per detector for the global aggregation formula. The reasoning: when D05 fires on 3 separate pool addresses, using the max represents "the worst case we found for this token on this detector," which is the appropriate input for a risk score. Using the average would undercount the risk when one pool is clean and one has strong wash trading. However, if the intent is "overall probability of wash trading somewhere on this token," the average might be more correct. The current spec chooses max for conservative (higher-score) behavior. OPEN: is there a concrete consumer scenario where averaging is preferable?

**OQ2 — `SkipReason` is caller-injected: is there a risk of incomplete coverage reporting?**

The design relies on the caller (gateway/server) to pass accurate skip reasons. If a detector was scheduled but crashed (Err return from `evaluate()`), the caller needs to distinguish "skipped by scheduler" from "errored out" and include the error case in skip reasons. The current design does not specify how `DetectorError` variants map to `SkipReason` strings. OPEN: should `crates/scoring` define `SkipReason` variants as an enum (not free-form strings) for type safety?

**OQ3 — `overall_severity` uses worst-case across ALL events, including Info-level noise**

If a token has one `Info` event and one `Critical` event, `overall_severity = Critical`. This is correct because the `Critical` is the actionable finding. However, if a token has ONLY `Info` events (all detectors fired at Info confidence), `overall_severity = Info`, which is appropriate. The edge case: a token with many `Info`-level events and one `Low`-level event reports `overall_severity = Low`. Is this the right consumer signal, or should `overall_severity` be computed only over events above `inconclusive_floor`? OPEN: define whether `overall_severity` includes or excludes inconclusive events.

**OQ4 — Configuration of `state_based_detectors` list creates an implicit contract with detector IDs**

The list of state-based detector IDs in `scoring.toml` must stay in sync with the actual `detector_id` constants in `crates/detectors`. If a detector changes its ID or a new detector ships with state-based signals under a different ID, scoring will incorrectly apply decay to a non-decaying signal. OPEN: should detector IDs be defined as constants in `crates/common` (or a shared `crates/detector-ids` micro-crate) to give the compiler a chance to catch mismatches?

**OQ5 — How does the `overall_score` threshold map to consumer action levels?**

The scoring design produces a `Confidence` scalar but does not prescribe action thresholds. The four consumers have different action policies:

- bot-trader: threshold for "no trade" action
- custody: threshold for "reject deposit" action
- market maker: threshold for "no quote" action
- exchange: threshold for "review before listing" action

These thresholds are consumer-specific and should live in each consumer's config, NOT in `ScoringConfig`. OPEN: document in the gateway OpenAPI spec a recommended threshold table (e.g. `> 0.7` = High risk / block, `0.4–0.7` = Medium risk / review, `< 0.4` = Low risk / proceed) for consumer guidance, without hardcoding in the scoring crate.

---

## 16. Design Gaps

**DG1 — Scoring does not combine events across time windows**

The current design scores a fixed window (typically last 24h). A token that had a pump 30 hours ago AND a new concentration spike now will score the pump at ~74% weight (age 30h at half_life=72h) and the concentration at 100%. There is no mechanism for "historical pattern awareness" — e.g. "this token pumped three times in the past week." Cumulative pattern scoring is Phase 6.

**DG2 — No cross-token calibration (absolute vs relative scoring)**

The base score is calibrated against RAVE (high-risk) and WET (medium-risk) as absolute anchors. If a large number of new tokens all fire at exactly the same confidence values, they will all receive the same score regardless of whether they are collectively more or less risky than the historical baseline. The scoring formula has no knowledge of the population distribution of scores. Cross-token ranking (percentile rank within the current token pool) is not implemented. This is a limitation for exchange consumers who want "top N riskiest tokens this hour" rather than absolute thresholds.

**DG3 — `is_established_protocol` attenuation is applied globally, not per-consumer**

The `established_protocol_multiplier` in `ScoringConfig` is a single value applied to all consumers. But the bot-trader and the exchange have different risk tolerances for established protocols: the exchange may want to know if even RAY shows unusual activity, while the bot-trader correctly treats RAY as low-risk. Consumer-specific attenuation overrides are not in scope for P5-1 but belong in the gateway/consumer SDK layer for Phase 6.

**DG4 — No calibration test coverage against the full 200-fixture corpus**

The calibration anchors use two probes. The 200-fixture corpus (`research/fixtures/solana-corpus-phase2.md`) has 100 positive + 99 negative fixtures with per-detector verdicts. A calibration sweep across the corpus would produce FPR and TPR for different `overall_score` thresholds. This sweep is Phase 6 work; the scoring design is calibrated only at two hand-verified anchors for P5-1.

**DG5 — No handling of future detector D07 (Phase 3)**

`DetectorWeights` is a struct with six named fields. Adding D07 (brand impersonation or sybil detection) requires both a struct field addition and a `config/scoring.toml` entry. The sum-to-1.0 constraint means existing weights must be re-derived when a new detector ships. This is a planned breaking change; it should be documented in `ROADMAP.md` as "Phase 3: update ScoringConfig for D07" with a note that the calibration anchor tests must be re-run.

---

*End of design. References: `research/token-probes/rave-FeqiF7TE.md` (RAVE probe), `research/token-probes/wet-WETZjtp.md` (WET probe), ADR 0001 §D4/D8, ADR 0002, ADR 0003, docs/designs/0004–0009 (D01–D06 specs), crates/common/src/anomaly.rs, crates/detectors/src/token_status.rs.*
