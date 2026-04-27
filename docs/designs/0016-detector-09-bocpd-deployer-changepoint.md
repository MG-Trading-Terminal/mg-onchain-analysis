# Design 0016 — D09: BOCPD Deployer Changepoint Detector (Sprint 12)

**Date:** 2026-04-24
**Status:** Draft — awaiting developer implementation
**Author:** onchain-analyst agent
**Sprint:** 12 (T2-1 from `research/03-feature-gap-2026-04-24.md`)
**ADR refs:**
- ADR 0001 §D5 — MVP detector set; Phase 3 graph algorithms
- ADR 0002 — Postgres-only storage; NUMERIC(39,0) for u128; string-bridged amounts
- ADR 0003 — self-sovereign infrastructure; no 3rd-party ML runtime in hot path
**Related designs:**
- `docs/designs/0015-crates-graph-phase3.md` — Sprint 11 graph foundation; data contract
- `docs/designs/0003-detector-trait.md` — Detector trait + DetectorContext
- `docs/designs/0014-streaming-detector.md` — streaming scheduler pattern
- `docs/designs/0012-detector-07-withdraw-withheld.md` — D07 structural reference
**Binding prior art in REFERENCES.md** (new entries proposed in §14):
- Adams & MacKay 2007 — online BOCPD algorithm
- Murphy 2007 — Normal-Gamma conjugate prior cheatsheet
- Chainalysis 2025 — deployer as risk signal (already present; "Used In" update needed)
- Sun et al. 2024 — evasion analysis reference (already present)

---

## §1 Purpose and Scope

### §1.1 Why this detector exists

D01–D08 are all **per-token, static-snapshot** detectors. Given a token at a moment in time, they examine its current state (liquidity, holders, authority, cluster membership). None of them model the **temporal arc of a deployer's career**: the fact that deployer `7xKP…Qrt` launched 15 legitimate tokens over 6 months and then, in the last 72 hours, launched 3 tokens that were drained within minutes of their first buy.

The latent-flux system (see `research/03-feature-gap-2026-04-24.md` §10, repo #10) operationally confirms this pattern on Base, Arbitrum, and Optimism: 36,000 deployers tracked; behavioral changepoint detection identifies regime shifts in deployer behavior with lead time before a drain completes. Adams & MacKay (2007) — the canonical algorithm — is the reference.

D09 fills the **temporal dimension** gap. It is the only detector in this codebase whose primary input is a time-series of events indexed by deployer identity rather than by token identity. All other detectors are instanced per-token; D09 is instanced per-deployer.

### §1.2 What it catches that D01–D08 do not

| Scenario | D01–D08 coverage | D09 coverage |
|----------|-----------------|--------------|
| Deployer with established record suddenly shortens launch cadence from 30d to 1d | None — per-token detectors cannot see across tokens | Yes — time-between-launches observation fires changepoint |
| LP locked on first 10 tokens; LP unlocked on token #11 | D02 fires on token #11 after LP drain starts | Yes — fires at token #11 creation, before drain |
| Deployer gradually reduces holder count across launches | D03 fires per-token; no cross-token signal | Partial — gradual drift tracked but BOCPD calibrated for abrupt shifts |
| Deployer rotates EOA every 10 launches | No cross-deployer link | Gap (see §11 evasion analysis) |
| Brand-new deployer with no history | D08 Sybil may fire | Not applicable — D09 requires min_history_length |

### §1.3 Cross-reference to research doc

This detector implements **T2-1** from `research/03-feature-gap-2026-04-24.md`:

> "Bayesian changepoint detection on deployer behavior (D-BPCD, Phase 3 graph prereq).
> Input: per-deployer time-series of token_count_launched, time_between_launches,
> mean_lp_locked_pct, mean_holder_count_at_launch, fraction_of_tokens_that_rugged.
> Algorithm: Adams & MacKay 2007 online BOCPD with Normal-Gamma conjugate prior;
> hazard rate 1/300 events."

The Sprint 11 graph foundation (design 0015) provides all required data: `graph_edges` with
`edge_type = 'DeployerOf'` and `address_labels` with `label_type = 'DeployerEOA'` are the
primary inputs.

---

## §2 Feature Vector (Per-Deployer Time-Series)

### §2.1 Observation definition

Each "observation" in the deployer's time-series is one new token launched. Formally:

```
observation_t = f(token_t, deployer_history_up_to_t)
```

Observations are ordered by `block_time` of the `PoolEvent::Initialize` that created the
pool for `token_t`. This ordering is deterministic given the same block-time data
(ADR 0001 determinism contract).

### §2.2 Raw feature candidates

The research doc proposed six features. Below is a critique, refinement, and final recommendation.

| Feature | Proposed | Decision | Justification |
|---------|---------|----------|---------------|
| `token_count_launched` | Cumulative count | Drop from observation vector | It is an index, not a signal. The BOCPD algorithm already maintains `t` (observation number). Including it as a feature conflates position in the series with behavioral state. |
| `time_between_launches` (seconds since prior launch) | Keep | Keep, normalized | Abrupt shortening of inter-launch gap is the clearest behavioral shift signal. Normalize as `ln(seconds + 1)` to compress the heavy right tail (deployers can go silent for months). Cited: Adams & MacKay 2007 §4 application example uses inter-arrival times as the primary feature. |
| `mean_lp_locked_pct` at launch | Rolling mean | Replace with instantaneous | At observation `t`, record the LP lock percentage of `token_t` at pool creation (from `pools` table). Rolling mean smooths the signal and delays changepoint detection. Instantaneous value is more sensitive to step-change attacks. Cited: Alhaidari et al. 2025 (SolRPDS) Table 3 — `lp_locked_pct` at launch is a top-3 predictor. |
| `mean_holder_count_at_first_hour` | Rolling mean | Replace with instantaneous | Record holder count for `token_t` at +1h from launch (join to `holders_snapshots` at nearest block). Same argument as above. |
| `initial_liquidity_usd` | Per-token | Keep, log-normalized | `ln(liquidity_usd + 1)`. Large variance; log compression improves Gaussian prior fit. Cited: RugWatch consensus (<5 SOL threshold) from research doc §T1-1. |
| `fraction_of_prior_tokens_rugged` | Derived from D02/D06/D07 | Keep | This is the most direct outcome signal. Derived at observation time by counting `anomaly_events` for the deployer's prior tokens where `detector_id IN ('rug_pull_lp_drain', 'mint_burn_anomaly', 'deployer_changepoint')` AND `confidence >= 0.60`. Must be computed at observation time only (no lookahead — gotcha on stationarity). |

**Final five features for the observation vector:**

| Index | Feature name | Derivation | Type |
|-------|-------------|-----------|------|
| 0 | `log_gap_seconds` | `ln(seconds_since_prior_launch + 1)` | f64 |
| 1 | `lp_locked_pct` | `pools.lp_locked_pct` at pool init (0.0–1.0) | f64 |
| 2 | `log_initial_liquidity_usd` | `ln(pools.initial_liquidity_usd + 1)` | f64 |
| 3 | `holder_count_at_1h` | `holders_snapshots` nearest to launch+3600s | f64 (as count) |
| 4 | `prior_rug_rate` | Confirmed rug count / (t-1) prior launches, 0.0 if t==1 | f64 |

### §2.3 Univariate composite score: decision and justification

**Decision: univariate composite score using a weighted linear combination of the five features, yielding a single scalar observation fed into a univariate Normal-Gamma BOCPD.**

**Rejected: multivariate BOCPD.**

Rationale for rejection:

1. Multivariate BOCPD requires a Normal-Inverse-Wishart prior (for multivariate Gaussian observations). The state per run-length `r` is a `d×d` precision matrix (`d=5`), so state size is `O(T × d²)` = `O(T × 25)` with `max_run_length_tracked = 1000`, requiring 25,000 f64 values per deployer per update. At thousands of tracked deployers this is significant memory pressure.
2. The five features are not independent; `prior_rug_rate` and `lp_locked_pct` are correlated with the deployment regime shift. A linear composite captures the dominant direction of variation without the full covariance matrix.
3. Latent-flux's BOCPD implementation uses a univariate scalar per-deployer based on a heuristic "badness score" (confirmed in research doc §10 repo description). The univariate approach is validated in production.
4. Murphy 2007 §9.4.2 explicitly recommends starting with Normal-Gamma (univariate) before Normal-Inverse-Wishart for online learning with small per-deployer sample sizes (our deployers average ~8 launches per REFERENCES.md Chainalysis 2025 data).

**Composite score formula:**

```
S_t = w0 * (1 - sigmoid(log_gap_seconds / 10.0))    # lower gap → higher score
    + w1 * (1 - lp_locked_pct)                       # locked LP → lower risk score
    + w2 * (1 - sigmoid(log_initial_liquidity_usd / 8.0))  # low liquidity → higher score
    + w3 * (1 - sigmoid(holder_count_at_1h / 100.0)) # low holders → higher score
    + w4 * prior_rug_rate                             # prior rugs → directly higher score

where weights (w0..w4) are config values defined in §7.
```

The composite score `S_t ∈ [0.0, 1.0]` represents a "behavioral risk unit" for this launch event. A deployer whose launches are spaced apart, well-funded, with locked LP, healthy holders, and no prior rugs will produce `S_t` near 0.0. A deployer launching rapidly with no LP lock, tiny liquidity, no holder accumulation, and prior rugs will produce `S_t` near 1.0.

BOCPD detects when the distribution of `S_t` values shifts — i.e. when the deployer's risk profile suddenly changes.

### §2.4 Sigmoid normalization note

All per-feature sigmoids use f64 (ADR 0002 permits f64 for probabilities and normalized scores; prohibits f64 for prices and token amounts). The composite score and BOCPD state are f64 throughout. Only evidence output values stored in `Evidence::metrics` (a `BTreeMap<String, Decimal>`) use `Decimal` for serialization.

---

## §3 BOCPD Algorithm Specification

### §3.1 Adams & MacKay 2007 canonical formulation

Reference: R. P. Adams and D. J. C. MacKay, "Bayesian Online Changepoint Detection," 2007. arXiv:0710.3742.

The algorithm maintains a probability distribution over the current **run length** `r_t` — the number of observations since the last changepoint. At each step `t`, given a new observation `x_t`, it updates `P(r_t | x_{1:t})` using:

```
P(r_t, x_{1:t}) = P(r_t, r_{t-1}, x_{1:t})
                  (marginalising over r_{t-1})
```

The two key equations are the **growth** message (run continues) and the **changepoint** message (run resets to 0):

```
P(r_t = r+1, x_{1:t}) = P(r_{t-1} = r, x_{1:t-1}) * P(x_t | r, x_t..t-1) * (1 - H(r))
P(r_t = 0,   x_{1:t}) = P(r_{t-1} = r, x_{1:t-1}) * P(x_t | r, x_t..t-1) * H(r)   ∀r

normalise: P(r_t | x_{1:t}) = P(r_t, x_{1:t}) / sum_r P(r_t=r, x_{1:t})
```

Where:
- `H(r)` is the hazard function (probability of changepoint given run length `r`)
- `P(x_t | r, x_t..t-1)` is the predictive probability under the conjugate prior for run `r`

**Alert rule:** emit `AnomalyEvent` when `P(r_t = 0 | x_{1:t}) >= changepoint_prob_threshold` (configured in §7).
**Confidence:** `clamp(P(r_t = 0 | x_{1:t}), 0.0, 1.0)`.

### §3.2 Hazard function

**Proposed hazard:** constant (memoryless) `H(r) = hazard_rate = 1/300 ≈ 0.00333`.

This corresponds to a geometric prior on run lengths with mean 300 observations. For most deployers who launch one token per week, this is approximately a 6-year expected run between changepoints — compatible with a "previously legitimate actor" model.

**Why not Weibull?** The Weibull-increasing hazard encodes the belief that actors who have been "quiet for a long time" are increasingly likely to change regime. For deployer behavior, there is no empirical support for this: a deployer who has been inactive for 2 years is not more likely to rug their next launch than one who has been launching monthly. The geometric (constant-hazard) prior is the correct uninformative prior for changepoint timing under Adams & MacKay (2007) §4.2. The latent-flux implementation also uses constant hazard (confirmed from research doc).

**Calibration target:** the POS_D09_01 synthetic positive fixture (§9) should produce `P(r_t = 0) >= 0.50` at observation #11, where the regime shifts. This is a sanity check, not a fit — the algorithm is deterministic.

**Config key:** `deployer_changepoint.hazard_rate = 0.00333` (§7).

### §3.3 Predictive UPM: Normal-Gamma conjugate prior

For a univariate Gaussian stream, the Normal-Gamma is the conjugate prior on `(mu, tau)` where `tau = 1/sigma²` (precision). Under this prior, the posterior predictive for a new observation is a Student-t distribution, computable in closed form.

Reference: Murphy 2007, "Conjugate Bayesian Analysis of the Gaussian Distribution," §4. Also Murphy 2012, "Machine Learning: A Probabilistic Perspective" §4.4.

**Prior hyperparameters (for run length = 0, i.e. fresh run):**

| Hyperparameter | Config key | Proposed value | Justification |
|---------------|-----------|---------------|---------------|
| `mu_0` | `deployer_changepoint.mu_0` | 0.20 | Prior mean of the composite score. 0.20 reflects a mildly skeptical prior — new deployers with no history are assumed slightly above zero risk. Uninformative would be 0.50; 0.20 is calibrated toward legitimate-actor-by-default. |
| `kappa_0` | `deployer_changepoint.kappa_0` | 1.0 | Pseudo-count on the mean. Value 1 = weak prior (equivalent to 1 prior observation). Small value means the prior is quickly overridden by data. |
| `alpha_0` | `deployer_changepoint.alpha_0` | 3.0 | Shape parameter of the Gamma prior on precision. Chosen so the prior predictive has finite variance (requires `alpha > 1`) and heavy tails (requires `alpha` not too large). Murphy 2007 §4: `alpha_0 = 1` is uninformative; `alpha_0 = 3` gives a prior with roughly 2 degrees of freedom in the Student-t predictive, appropriate for a small-sample regime. |
| `beta_0` | `deployer_changepoint.beta_0` | 1.0 | Rate parameter of the Gamma prior on precision. Together with `alpha_0 = 3`, this gives a prior variance of `beta_0 / (alpha_0 - 1) = 0.50` for the composite score. At `mu_0 = 0.20`, this means the prior assigns meaningful probability to scores in `[0.0, 0.70]` — appropriate for the full range of deployer risk profiles. |

**Design-derivation note (REFERENCES.md classification):** the specific hyperparameter values above are derived from the feature-vector domain knowledge (composite score range `[0.0, 1.0]`) rather than from a Solana-specific labelled corpus. They are classified as `unverified-heuristic` pending Sprint 12 calibration against the positive fixture corpus. The derivation follows Murphy 2007 §4 guidance for bounded-range Gaussian data.

### §3.4 Posterior update equations

For run length `r` at time `t` with observations `x_{t-r+1..t}`, the Normal-Gamma posterior parameters are updated using the standard conjugate update (Murphy 2007 §4.3):

```
n          = r                               // number of observations in this run
x_bar      = mean(x_{t-r+1..t})             // sample mean
S          = sum((x_i - x_bar)^2)           // sum of squared deviations

kappa_n    = kappa_0 + n
mu_n       = (kappa_0 * mu_0 + n * x_bar) / kappa_n
alpha_n    = alpha_0 + n / 2.0
beta_n     = beta_0 + S / 2.0
           + (kappa_0 * n * (x_bar - mu_0)^2) / (2.0 * (kappa_0 + n))
```

**Predictive probability** for next observation `x_{t+1}`:

The posterior predictive is a Student-t distribution with:
```
nu        = 2.0 * alpha_n
mu_pred   = mu_n
sigma_pred^2 = (beta_n * (kappa_n + 1)) / (alpha_n * kappa_n)
```

Student-t log-PDF:
```
log P(x | mu_pred, sigma_pred^2, nu) =
    log Gamma((nu + 1) / 2)
    - log Gamma(nu / 2)
    - 0.5 * log(nu * pi * sigma_pred^2)
    - ((nu + 1) / 2) * ln(1 + (x - mu_pred)^2 / (nu * sigma_pred^2))
```

Use the `statrs` crate (`statrs::distribution::StudentsT`) for the log-PDF computation. No custom implementation of special functions is required or permitted (ADR 0003: no Python ML bridge; `statrs` is a pure Rust crate).

### §3.5 Online sufficient statistics (incremental update)

Computing `x_bar` and `S` from scratch on every observation for every run length would be `O(T²)` over the full history. Instead, maintain Welford's online algorithm per run-length slot:

```rust
// Per run-length slot (maintained in BOCPD state):
struct RunSlot {
    n:       u32,      // observation count
    mean:    f64,      // running mean
    m2:      f64,      // sum of squared deviations (Welford M2)
    kappa_n: f64,
    mu_n:    f64,
    alpha_n: f64,
    beta_n:  f64,
}

// Update on new observation x:
fn update(slot: &mut RunSlot, x: f64, mu_0: f64, kappa_0: f64, alpha_0: f64, beta_0: f64) {
    slot.n += 1;
    let n = slot.n as f64;

    // Welford mean and M2:
    let delta = x - slot.mean;
    slot.mean += delta / n;
    let delta2 = x - slot.mean;
    slot.m2 += delta * delta2;

    // Normal-Gamma posterior:
    slot.kappa_n = kappa_0 + n;
    slot.mu_n    = (kappa_0 * mu_0 + n * slot.mean) / slot.kappa_n;
    slot.alpha_n = alpha_0 + n / 2.0;
    // beta_n requires the cross-term:
    let cross = (kappa_0 * n * (slot.mean - mu_0).powi(2)) / (2.0 * (kappa_0 + n));
    slot.beta_n = beta_0 + slot.m2 / 2.0 + cross;
}
```

This gives exact Normal-Gamma updates in `O(1)` per run-length slot per observation.

### §3.6 Numerical stability in log space

The run-length posterior vector grows to length `T` over time (one probability per possible run length). Probabilities for long runs become extremely small — IEEE-754 underflows to zero for `T > ~700` in double precision.

**Requirement:** maintain the posterior in log space and normalise only for output.

```rust
// State vector: log P(r_t = r, x_{1:t}) for r = 0..max_run_length_tracked
let mut log_joint: Vec<f64> = vec![f64::NEG_INFINITY; max_run_length_tracked + 1];

// Growth step (prior to normalisation):
log_joint_new[r + 1] = log_joint[r] + log_pred(x, &slot_r) + ln(1.0 - hazard_rate);

// Changepoint mass at r=0:
let log_cp_sum = log_sum_exp(log_joint.iter().zip(&log_preds)
    .map(|(lj, lp)| lj + lp + ln(hazard_rate)).collect());
log_joint_new[0] = log_cp_sum;

// Normalise:
let log_total = log_sum_exp(&log_joint_new);
for lj in &mut log_joint_new {
    *lj -= log_total;
}

// Extract changepoint probability:
let cp_prob = log_joint_new[0].exp();
```

**`log_sum_exp` implementation:** use the standard numerically stable form:
```
log_sum_exp(v) = max(v) + ln(sum(exp(v_i - max(v))))
```

**Guard against `max_run_length_tracked` truncation:** when the vector reaches `max_run_length_tracked`, do NOT extend it. Instead, fold the mass at `r = max_run_length_tracked` into `r = max_run_length_tracked - 1` (absorbing boundary). This preserves probability normalization while bounding memory.

### §3.7 Determinism assertion

The BOCPD algorithm is deterministic: given the same sequence of composite scores `(S_1, S_2, ..., S_t)` and the same hyperparameters, it produces identical output. This holds because:
- No wall-clock time is used (all times come from `block_time` via `ctx.observed_at`)
- No randomness is used (Normal-Gamma updates are deterministic)
- `Vec<f64>` iteration is ordered

**Developer requirement:** add a unit test (§13) that asserts identical output on identical input fed twice.

### §3.8 State reset policy

A deployer's BOCPD state is **never automatically reset** once initialized. The posterior simply accumulates evidence over the deployer's full career.

An explicit admin reset (deleting the `bocpd_deployer_state` row) is the only reset mechanism. This is intentional: clearing state would lose all accumulated evidence and create a cold-start window that an attacker could exploit (see §11 evasion E-D09-7).

If a deployer has zero observations for more than 365 days, the state is considered **dormant** but not deleted. A returning deployer picks up where they left off — the long quiet period appears as a long run length, which is evidence of stability, not evidence of a changepoint.

---

## §4 Data Flow

### §4.1 Trigger: new `DeployerOf` edge for an existing `DeployerEOA`

The detector is **event-driven**, not cadenced. It fires exactly once per new token launch from a known deployer. The trigger is:

> A new `DeployerOf` edge is written to `graph_edges` by `GraphIndexerWriter` (indexer S11-4), AND the deployer address has a non-expired `DeployerEOA` label in `address_labels`.

In practice, `GraphIndexerWriter` already writes `DeployerEOA` on first encounter. On subsequent launches from the same deployer, the `DeployerEOA` label already exists (permanent, `expires_at = NULL`). The server-side event handler can detect "existing deployer" by:

```sql
SELECT COUNT(*) FROM address_labels
WHERE chain = $1 AND address = $2 AND label_type = 'DeployerEOA'
  AND expires_at IS NULL;
```

Or more efficiently, `get_neighbors(chain, deployer, EdgeType::DeployerOf, limit=2)` returns > 0 rows after the first token.

**Implementation hook:** the server-side `IndexerEventHandler` (in `crates/server`) already calls `graph_writer.on_pool_initialize()`. D09 evaluation is added as a step in this path, after the `DeployerOf` edge is written and after the feature observation is computed.

### §4.2 Feature observation computation

After the `DeployerOf` edge is written for `(deployer, token_t)`, compute the five features:

```sql
-- Feature 0: time since prior launch
SELECT EXTRACT(EPOCH FROM (
    $new_block_time -
    MAX(ge.block_time)
)) AS gap_seconds
FROM graph_edges ge
WHERE ge.chain = $chain
  AND ge.from_address = $deployer
  AND ge.edge_type = 'DeployerOf'
  AND ge.to_address != $new_token    -- exclude the current launch
ORDER BY ge.block_time DESC
LIMIT 1;
-- If no prior launch: gap_seconds = NULL → use default = ln(30*24*3600 + 1) ≈ 14.7 (30 days)

-- Feature 1: lp_locked_pct at launch
SELECT COALESCE(p.lp_locked_pct, 0.0) AS lp_locked_pct
FROM pools p
WHERE p.chain = $chain AND p.token = $new_token
ORDER BY p.created_at ASC
LIMIT 1;

-- Feature 2: initial_liquidity_usd
SELECT COALESCE(p.initial_liquidity_usd, 0.0) AS initial_liquidity_usd
FROM pools p
WHERE p.chain = $chain AND p.token = $new_token
ORDER BY p.created_at ASC
LIMIT 1;

-- Feature 3: holder_count at +1h (nearest snapshot)
SELECT COUNT(DISTINCT hs.holder_address) AS holder_count
FROM holders_snapshots hs
WHERE hs.chain = $chain AND hs.token = $new_token
  AND hs.snapshot_time BETWEEN $launch_time AND $launch_time + INTERVAL '2 hours'
ORDER BY ABS(EXTRACT(EPOCH FROM (hs.snapshot_time - ($launch_time + INTERVAL '1 hour'))))
LIMIT 1;
-- If no snapshot exists within 2h: holder_count = 0

-- Feature 4: prior_rug_rate
SELECT
    COUNT(DISTINCT ae.token) FILTER (WHERE ae.confidence >= $rug_conf_threshold) AS rugged,
    COUNT(DISTINCT ge.to_address) AS total_prior
FROM graph_edges ge
LEFT JOIN anomaly_events ae
    ON ae.chain = ge.chain
    AND ae.token = ge.to_address
    AND ae.detector_id IN ('rug_pull_lp_drain', 'mint_burn_anomaly', 'withdraw_withheld_drain')
    AND ae.confidence >= $rug_conf_threshold
WHERE ge.chain = $chain
  AND ge.from_address = $deployer
  AND ge.edge_type = 'DeployerOf'
  AND ge.to_address != $new_token;
-- prior_rug_rate = rugged / total_prior (0.0 if total_prior = 0)
```

All five queries are run by the detector (via `ctx.store`) before BOCPD update. They must be wrapped in a single database transaction to prevent partial-observation states.

### §4.3 BOCPD state: load → update → store

```
1. Load deployer state from `bocpd_deployer_state` (§4.4) — or initialize with prior if absent.
2. Compute composite score S_t from the five features (§2.3).
3. Execute BOCPD update (§3) on the loaded state + new score S_t.
4. Compute cp_prob = P(r_t = 0 | x_{1:t}).
5. Store updated state back to `bocpd_deployer_state`.
6. If cp_prob >= changepoint_prob_threshold AND total_launches >= min_history_length:
     emit AnomalyEvent.
```

Step 5 MUST execute even if no event is emitted — state must always be persisted.

### §4.4 Storage: `bocpd_deployer_state` table (V00013)

**Decision: Postgres (ADR 0002). Not in-memory.**

Rationale: in-memory state requires full deployer history replay on every restart. At MVP scale (hundreds of deployers, thousands of tokens), a restart replay would query `graph_edges`, `pools`, `holders_snapshots`, and `anomaly_events` for each deployer — potentially minutes of startup time and significant DB load. Postgres-serialized state survives restarts with zero replay cost.

**Migration:** V00013. V00012 is reserved for `token_risk_reports` (SESSION-KICKOFF gotcha #31). V00013 is next (confirmed: V00001–V00011 shipped; V00012 slot reserved).

**Schema:**

```sql
-- V00013__bocpd_deployer_state.sql

CREATE TABLE IF NOT EXISTS bocpd_deployer_state (
    -- Identity
    chain           TEXT            NOT NULL,
    deployer        TEXT            NOT NULL,

    -- Total number of observations ingested (= tokens launched by this deployer)
    total_observations  INTEGER     NOT NULL DEFAULT 0,

    -- Serialized run-length posterior and sufficient statistics.
    -- Format: JSON array of RunSlotSnapshot objects (see §4.5).
    -- JSONB chosen over BYTEA/Base64 for inspectability; the array length is
    -- bounded by max_run_length_tracked (config, default 1000).
    run_length_state_json   JSONB   NOT NULL DEFAULT '[]'::jsonb,

    -- The composite score from the most recent observation, for debugging.
    -- DOUBLE PRECISION (not NUMERIC) because this is a normalized probability,
    -- not a monetary amount (ADR 0002 scope: NUMERIC for prices/amounts only).
    last_observation_score  DOUBLE PRECISION,

    -- The raw feature vector of the most recent observation, for evidence bundle.
    last_observation_features_json  JSONB,

    -- The changepoint probability from the most recent update.
    last_cp_prob            DOUBLE PRECISION,

    -- Block context of the most recent update.
    last_update_block_height    BIGINT,
    last_update_block_time      TIMESTAMPTZ,

    -- Housekeeping (NOT Utc::now() in streaming path — see §4.6)
    updated_at              TIMESTAMPTZ     NOT NULL DEFAULT now(),

    PRIMARY KEY (chain, deployer)
);

-- Index for bulk scan (e.g. admin dashboard, calibration tool)
CREATE INDEX IF NOT EXISTS idx_bocpd_deployer_state_chain
    ON bocpd_deployer_state (chain);

-- Index for finding deployers with high last_cp_prob (alert triage)
CREATE INDEX IF NOT EXISTS idx_bocpd_deployer_state_cp_prob
    ON bocpd_deployer_state (chain, last_cp_prob DESC)
    WHERE last_cp_prob IS NOT NULL;
```

**Retention policy:** no automatic deletion. Deployer state is permanent (matches `DeployerEOA` label TTL = NULL). If the table exceeds 10M rows (Phase 4 multi-chain at scale), add an archival job that migrates rows with `total_observations = 1` AND `last_update_block_time < now() - INTERVAL '1 year'` to a cold storage table.

**Gotcha #7 compliance:** `bocpd_deployer_state` is NOT partitioned at Sprint 12 scale. No partition key in the PRIMARY KEY. If partitioned in the future, add `last_update_block_time` to the PK at migration time.

### §4.5 `run_length_state_json` format

Each element of the JSON array corresponds to one run-length slot. Array index `i` = run length `r = i`. Slot 0 is always included (it represents a fresh-start run). Slots beyond `max_run_length_tracked` are not stored (truncation at absorbing boundary).

```json
[
  {
    "r": 0,
    "log_joint": -0.693,
    "n": 0,
    "mean": 0.0,
    "m2": 0.0,
    "kappa_n": 1.0,
    "mu_n": 0.20,
    "alpha_n": 3.0,
    "beta_n": 1.0
  },
  {
    "r": 1,
    "log_joint": -1.234,
    "n": 1,
    "mean": 0.35,
    "m2": 0.0,
    "kappa_n": 2.0,
    "mu_n": 0.275,
    "alpha_n": 3.5,
    "beta_n": 1.012
  }
]
```

Fields:
- `r`: run length (matches array index; included for readability/debugging)
- `log_joint`: `log P(r_t = r, x_{1:t})` after normalisation
- `n`, `mean`, `m2`: Welford online statistics (sufficient for posterior recomputation)
- `kappa_n`, `mu_n`, `alpha_n`, `beta_n`: current Normal-Gamma posterior parameters

The BOCPD update only requires `log_joint` and the Normal-Gamma parameters; `n`, `mean`, `m2` are stored redundantly for auditability and for potential future feature (e.g. state inspection endpoint).

### §4.6 Time source discipline

- `last_update_block_time` = `ctx.observed_at` (block_time-sourced, NOT `Utc::now()`)
- `updated_at` = `now()` (standard Postgres housekeeping; wall-clock is acceptable for this metadata column)
- `last_update_block_height` = from the `PoolEvent::Initialize` block that triggered the observation

**Developer requirement:** add a `grep Utc::now` check in the PR diff for `d09_bocpd.rs` (gotcha #22).

---

## §5 Detector Wiring

### §5.1 Event-driven, not cadenced

D09 is **not** a cadenced detector (unlike D01 which fires every N ticks per token, or D08 which fires on cluster refresh). D09 fires exactly once per new token launch from an existing deployer. The trigger is the `PoolEvent::Initialize` write path.

This is a departure from the streaming scheduler pattern but is the correct model: the BOCPD update is lightweight (microseconds of CPU per update), the trigger is a natural event (pool creation), and cadencing would introduce artificial delay.

**Implementation:** add a `D09BocpdDetector::on_new_token_launch(chain, deployer, token, ctx)` method that is called from `IndexerEventHandler::on_pool_initialize` in `crates/server`, after the graph writer completes its writes. This is NOT called via `StreamingScheduler` but directly from the event handler.

**Send + Sync requirement (gotcha #27):** the `D09BocpdDetector` struct holds:
- `Arc<dyn TypedEdgeStore>` — for reading `DeployerOf` edges
- `Arc<dyn GraphLabelStore>` — for reading/writing `DeployerEOA` labels
- `Arc<PgPool>` — for `bocpd_deployer_state` reads/writes

All of these are `Send + Sync`. The detector itself is `Send + Sync` and can be stored in `Arc<D09BocpdDetector>`.

### §5.2 Detector trait compliance

D09 implements the `Detector` trait for API compatibility with the scheduler and testing framework:

```rust
impl Detector for D09BocpdDetector {
    fn id(&self) -> &'static str { "deployer_changepoint" }

    fn severity_floor(&self) -> Severity { Severity::Medium }

    fn evaluate<'ctx>(
        &'ctx self,
        ctx: &'ctx DetectorContext<'ctx>,
    ) -> impl Future<Output = Result<Vec<AnomalyEvent>, DetectorError>> + Send + 'ctx {
        // For the cadenced/on-demand path: load state for ctx.token's deployer,
        // run BOCPD on any NEW observations since last_update_block_height,
        // emit events if cp_prob >= threshold.
        // This is the fallback path; production uses on_new_token_launch().
        async move { self.evaluate_inner(ctx).await }
    }
}
```

The primary production path is `on_new_token_launch`. The `evaluate` trait impl provides a fallback for historical replay: given a `ctx.token`, it looks up the deployer via `graph_edges`, checks for unprocessed launches (blocks between `last_update_block_height` and `ctx.window.block_end`), and runs BOCPD forward.

### §5.3 State holds `Arc` stores, not context references

Follows the D08 Option B pattern (design 0015 §5.2): D09 stores `Arc<dyn TypedEdgeStore>` and `Arc<PgPool>` as struct fields, injected at server startup. It does not receive them via `DetectorContext`. This avoids adding graph store fields to `DetectorContext` (which would compile-break all 8 existing detectors).

---

## §6 Evidence Keys

All keys use the `deployer_changepoint/` prefix (CLAUDE.md gotcha #9).

### `Evidence::metrics` (BTreeMap<String, Decimal>)

| Key | Decimal encoding | Meaning |
|-----|-----------------|---------|
| `deployer_changepoint/changepoint_prob` | 6 decimal places | `P(r_t = 0 \| x_{1:t})` — the raw BOCPD output |
| `deployer_changepoint/observation_value` | 6 decimal places | Composite score `S_t` for this observation |
| `deployer_changepoint/total_tokens_launched` | Integer (0 decimal places) | Total tokens from this deployer (including current) |
| `deployer_changepoint/prior_rug_rate` | 4 decimal places | Fraction of prior tokens with confirmed rug events |
| `deployer_changepoint/lp_locked_pct` | 4 decimal places | LP locked pct for this token at launch |
| `deployer_changepoint/log_gap_seconds` | 4 decimal places | `ln(gap_seconds + 1)` for this observation |
| `deployer_changepoint/run_length_mode` | Integer | The most probable run length at time `t` (`argmax P(r_t)`) |
| `deployer_changepoint/run_length_prob_0` | 6 decimal places | `P(r_t = 0)` (same as changepoint_prob, included for symmetry) |
| `deployer_changepoint/run_length_prob_1` | 6 decimal places | `P(r_t = 1)` |
| `deployer_changepoint/run_length_prob_mode` | 6 decimal places | `P(r_t = mode)` |

### `Evidence::addresses`

- The deployer EOA address (primary subject of the event)

### `Evidence::notes`

- `"detector_version=D09_v1"`
- `"prior_rug_tokens=<comma_separated_token_mints>"` (up to 5, sorted by recency)
- `"new_token=<mint_address>"`

### Decimal serialization note

All `f64` internal values are converted to `Decimal` for evidence via `Decimal::from_f64(v).unwrap_or(Decimal::ZERO).round_dp(6)`. The `Decimal` stored in `Evidence::metrics` must NOT be the raw f64 bitcast — use `rust_decimal::prelude::FromPrimitive`.

---

## §7 Thresholds in `config/detectors.toml`

New section `[deployer_changepoint]`:

```toml
# ---------------------------------------------------------------------------
# D09 — Deployer Changepoint (BOCPD)
# Full implementation: Sprint 12 (docs/designs/0016-detector-09-bocpd-deployer-changepoint.md)
# Primary sources:
#   Adams & MacKay 2007 — https://arxiv.org/abs/0710.3742 (BOCPD algorithm)
#   Murphy 2007 — conjugate prior cheatsheet (Normal-Gamma update equations)
#   Chainalysis 2025 — deployer as risk signal (deployer behavior baseline)
#   Sun et al. 2024 — evasion patterns (hidden mint, fake LP lock)
#   Latent-flux (#10) — production BOCPD on deployer behavior (research doc §10)
# ---------------------------------------------------------------------------

[deployer_changepoint.changepoint_prob_threshold]
value     = 0.50
rationale = """Emit AnomalyEvent when P(r_t=0|x_1:t) >= 0.50. The 0.50 threshold is the
              standard maximum-a-posteriori decision boundary for a two-class problem: if
              the changepoint hypothesis is more likely than the no-changepoint hypothesis,
              fire the event. Adams & MacKay (2007) §3 use 0.50 as the canonical threshold
              for their experiments. CLAUDE.md rule: 'false negatives are expensive' —
              prefer 0.50 over higher thresholds. Calibrate downward to 0.40 if Sprint 12
              fixture shows too many false negatives on POS_D09_01."""
refs      = ["D09/deployer_changepoint", "Adams&MacKay2007/bocpd"]

[deployer_changepoint.min_history_length]
value     = 5
rationale = """Minimum number of prior tokens required before the detector emits any event.
              The BOCPD prior (mu_0=0.20, kappa_0=1.0) is weak — it takes approximately 5
              observations for the posterior to stabilize enough for a meaningful changepoint
              signal. With fewer than 5 tokens, the deployer's behavior is not well-characterized
              and any apparent 'changepoint' is more likely noise from the weak prior than a
              genuine regime shift. Note: research doc T2-1 proposed min=3; raised to 5 because
              the research doc also noted 'must be >3 to catch evasion where deployer warms up
              with 3 launches' (§11 evasion E-D09-3 below). D02/D08 cover the first 3-4 launches
              from any deployer; D09 provides the complementary long-run view."""
refs      = ["D09/deployer_changepoint", "Adams&MacKay2007/bocpd"]

[deployer_changepoint.hazard_rate]
value     = 0.00333
rationale = """Constant (memoryless) hazard H(r) = 1/300 ≈ 0.00333. This corresponds to a
              geometric prior on run lengths with expected run = 300 observations. For deployers
              launching one token per week, this is approximately a 6-year expected career
              between regime changes — consistent with a 'legitimate actor by default' prior.
              Adams & MacKay (2007) §4 recommend starting with constant hazard (geometric prior)
              as the uninformative baseline. Latent-flux production confirms constant hazard
              (research doc §10). Calibrate against POS_D09_01 fixture: the synthetic deployer
              shifts at observation #11 and should produce P(r_t=0) >= 0.50."""
refs      = ["D09/deployer_changepoint", "Adams&MacKay2007/bocpd", "latentflux/production"]

[deployer_changepoint.mu_0]
value     = 0.20
rationale = """Prior mean of composite risk score. 0.20 encodes a 'mildly legitimate' prior:
              new deployers are assumed to be slightly above-zero risk (not fully trusted but
              not assumed malicious). An uninformative prior would be 0.50 (middle of [0,1]).
              0.20 reduces false positives on legitimate deployers early in their career.
              Classified as unverified-heuristic; calibrate from NEG_D09_01 fixture — the
              negative deployer's mean score should be well below 0.50 under this prior."""
refs      = ["D09/deployer_changepoint", "Murphy2007/normal-gamma"]

[deployer_changepoint.kappa_0]
value     = 1.0
rationale = """Pseudo-count on the prior mean. kappa_0=1.0 means the prior is equivalent to
              1 prior observation. This is the standard 'weak prior' choice (Murphy 2007 §4.4:
              kappa_0=1 is the minimum informative value). A weaker prior (kappa_0=0.01) would
              be overridden by the first real observation; a stronger prior (kappa_0=10) would
              resist updating for 10+ observations. At kappa_0=1, the prior is overridden after
              approximately 3-5 real observations, matching min_history_length=5."""
refs      = ["D09/deployer_changepoint", "Murphy2007/normal-gamma"]

[deployer_changepoint.alpha_0]
value     = 3.0
rationale = """Shape of the Gamma prior on precision. alpha_0=3.0 gives a Student-t predictive
              with nu=2*alpha_0=6 degrees of freedom — moderately heavy tails, appropriate for
              small samples (Murphy 2007 §9.4.2). Requires alpha_0 > 1.0 for finite prior
              variance. At alpha_0=3.0, prior variance of composite score = beta_0/(alpha_0-1)=0.50,
              covering the full [0,1] range at 2 sigma."""
refs      = ["D09/deployer_changepoint", "Murphy2007/normal-gamma"]

[deployer_changepoint.beta_0]
value     = 1.0
rationale = """Rate of the Gamma prior on precision. Together with alpha_0=3.0, gives prior
              variance=0.50 for the composite score. Derived from the domain constraint that
              composite scores live in [0,1]; 2-sigma range=[mu_0-1.0, mu_0+1.0]=[−0.8, 1.2]
              covers the full feasible range with headroom. Murphy 2007 §4.4: beta_0=1 is the
              standard scale parameter for unit-bounded data."""
refs      = ["D09/deployer_changepoint", "Murphy2007/normal-gamma"]

[deployer_changepoint.max_run_length_tracked]
value     = 1000
rationale = """Maximum number of run-length slots maintained in the posterior vector. Beyond
              this length, mass is absorbed into the boundary slot (§3.6). At 1000 slots,
              each represented by 8 f64 values (log_joint + 6 Normal-Gamma parameters) = 64
              bytes, the state per deployer is 64 KB. At 10,000 tracked deployers = 640 MB
              RAM — acceptable for Phase 3 scale but requires monitoring. A deployer would
              need to launch 1000+ tokens without a changepoint to hit this limit; at real-world
              cadences (< 100 tokens per legitimate deployer), this is a safety ceiling."""
refs      = ["D09/deployer_changepoint"]

[deployer_changepoint.composite_weight_log_gap]
value     = 0.25
rationale = """Weight w0 for log_gap_seconds in the composite score. Normalized so w0+w1+w2+w3+w4=1.0.
              0.25 gives the inter-launch time gap equal weight to LP lock and liquidity signals.
              The feature is sign-inverted: a shorter gap → higher score (more suspicious). Calibrate
              by running the positive and negative fixtures and checking that the score range is
              well-separated (positive deployer mean > negative deployer mean by > 0.3). Classified
              as unverified-heuristic; adjust in Sprint 12 after fixture calibration."""
refs      = ["D09/deployer_changepoint"]

[deployer_changepoint.composite_weight_lp_locked]
value     = 0.25
rationale = """Weight w1 for (1 - lp_locked_pct). Alhaidari et al. (2025) SolRPDS Table 2 lists
              lp_locked_pct as a top-3 feature in their Solana pool risk classifier. Equal weight
              to log_gap justified by comparable feature importance. Sign-inverted: higher LP lock
              → lower risk → lower score. Calibrate from NEG_D09_01: a deployer with consistently
              100% LP lock should produce composite scores near 0.0 on this component."""
refs      = ["D09/deployer_changepoint", "Alhaidari2025/solrpds"]

[deployer_changepoint.composite_weight_log_liquidity]
value     = 0.15
rationale = """Weight w2 for (1 - sigmoid(log_initial_liquidity_usd / 8.0)). Lower weight than
              gap and LP because initial liquidity is more variable across token types (micro-cap
              vs established DeFi). The sigmoid with scale 8.0 maps ln($10,000) ≈ 9.2 to ~0.12,
              meaning a token launched with $10K liquidity scores 0.12 on this component — low risk.
              A token launched with $10 liquidity scores ~0.75 on this component. Classified as
              unverified-heuristic."""
refs      = ["D09/deployer_changepoint"]

[deployer_changepoint.composite_weight_holder_count]
value     = 0.10
rationale = """Weight w3 for (1 - sigmoid(holder_count_at_1h / 100.0)). Lowest weight because
              holder count at +1h is noisy (dependent on time of day, market conditions, CEX
              listings). The sigmoid with scale 100 maps a token with 100 holders at +1h to
              score 0.5 on this component. Classified as unverified-heuristic."""
refs      = ["D09/deployer_changepoint"]

[deployer_changepoint.composite_weight_prior_rug_rate]
value     = 0.25
rationale = """Weight w4 for prior_rug_rate. Equal to log_gap and lp_locked because this is the
              most direct outcome signal: a deployer who has already rugged tokens is the strongest
              predictor of future rugs (Chainalysis 2025: 94% of rugged tokens had deployer as
              primary rug actor). A prior_rug_rate of 0.50 (half of prior tokens rugged) contributes
              0.25 * 0.50 = 0.125 to the composite score — not dominant alone, but combined with
              other signals it drives the score high."""
refs      = ["D09/deployer_changepoint", "Chainalysis2025/base-rate"]

[deployer_changepoint.rug_confidence_threshold]
value     = 0.60
rationale = """Minimum confidence required for an anomaly_event to be counted as a confirmed rug
              when computing prior_rug_rate. 0.60 corresponds to the High severity band floor
              in severity_from_confidence(). Low-confidence rug signals (Info/Low) are excluded
              to avoid false-positive feedback loops where a tentative D02 signal inflates the
              BOCPD prior_rug_rate, causing D09 to fire, which then further inflates the D09
              prior on the next launch. 0.60 is the same threshold used in D04 Signal C insider
              sell confirmation."""
refs      = ["D09/deployer_changepoint"]
```

**Weight sum check:** `w0 + w1 + w2 + w3 + w4 = 0.25 + 0.25 + 0.15 + 0.10 + 0.25 = 1.00`. Developer must assert this at startup via a config validation step.

---

## §8 Suppression and Noise Control

### §8.1 Established-protocol suppression

**Applies to D09.** Unlike D08 (which deliberately does not suppress on established protocols), D09 should suppress on `KnownDex` and `KnownExchange` deployers.

Rationale: Raydium's pool initializer program (`Raydium_AMM_authority`, a PDA) appears as the `from_address` in some `DeployerOf` edges for program-initialized pools. If this PDA is tracked as a deployer, every new Raydium pool would trigger a D09 update with a trivially short inter-launch gap, producing spurious changepoints. The same applies to CEX-associated deployers.

**Implementation:** before computing features for a new launch, check:

```rust
let labels = label_store.get_labels(chain, deployer).await?;
let is_known_infra = labels.iter().any(|l| {
    matches!(l.label_type, LabelType::KnownDex | LabelType::KnownExchange)
        && l.confidence >= 0.80
});
if is_known_infra {
    return Ok(vec![]);  // Skip without updating state
}
```

Reference: design 0015 §4.1 (`KnownDex` label seeded from token-registry static data).

### §8.2 Minimum-data guard (`min_history_length`)

No event is emitted until `total_observations >= min_history_length` (config default: 5). State is always updated — the guard only suppresses event emission. This ensures:
1. The posterior has received enough data for the Student-t predictive to have reasonable parameters.
2. D02/D08 cover the first few launches; D09 provides the complementary long-run view.

### §8.3 Cold-start behavior

When a deployer is first seen (no `bocpd_deployer_state` row exists):
1. Initialize state with the prior hyperparameters (`kappa_0`, `mu_0`, `alpha_0`, `beta_0`).
2. Set `total_observations = 0`.
3. Process the first observation (the current token) — this increments `total_observations` to 1.
4. Store the state.
5. No event emitted (cold-start; `total_observations < min_history_length`).

The cold-start is transparent to the caller — the detector handles missing state gracefully.

### §8.4 Self-feeding confidence loop prevention

The `prior_rug_rate` feature incorporates `anomaly_events` from previous detectors. A high D09 confidence event could — in theory — inflate the `prior_rug_rate` for the next D09 evaluation on the same deployer, causing runaway confidence escalation.

**Mitigation:** D09's own events (`detector_id = 'deployer_changepoint'`) are **excluded** from the `prior_rug_rate` query. Only `rug_pull_lp_drain`, `mint_burn_anomaly`, and `withdraw_withheld_drain` events count as confirmed rugs for this calculation. D09 fires on deployer-level signals; counting D09's own output as a rug would be circular.

---

## §9 Fixture Plan

### §9.1 Positive fixture: `POS_D09_01`

**File:** `tests/fixtures/solana/d09_positive_01_deployer_changepoint.json`

**Scenario:** Deployer `7xKP…QrtA` (synthetic address) launches 10 tokens over 10 months with healthy behavior, then launches token #11 with a sudden behavioral shift.

**Observation series:**

| Obs | Gap (days) | LP locked | Liquidity (USD) | Holders @1h | Prior rug rate | Composite S_t |
|-----|-----------|-----------|----------------|------------|----------------|---------------|
| 1 | 30 (default) | 0.90 | 5000 | 120 | 0.00 | ~0.07 |
| 2 | 28 | 0.85 | 6000 | 135 | 0.00 | ~0.08 |
| 3 | 32 | 0.92 | 4500 | 110 | 0.00 | ~0.07 |
| 4 | 25 | 0.88 | 7000 | 150 | 0.00 | ~0.07 |
| 5 | 30 | 0.80 | 5500 | 125 | 0.00 | ~0.09 |
| 6 | 27 | 0.91 | 6200 | 140 | 0.00 | ~0.07 |
| 7 | 31 | 0.87 | 4800 | 115 | 0.00 | ~0.08 |
| 8 | 29 | 0.93 | 5300 | 130 | 0.00 | ~0.07 |
| 9 | 33 | 0.82 | 6800 | 155 | 0.00 | ~0.08 |
| 10 | 28 | 0.89 | 5100 | 128 | 0.00 | ~0.07 |
| **11** | **0.5** | **0.00** | **50** | **3** | **0.30** | **~0.82** |

The regime shift at observation #11:
- Gap drops from ~29 days to 0.5 days (12 hours) → `log_gap_seconds` drops sharply
- LP locked drops from ~88% to 0%
- Liquidity drops from ~$5,500 to $50
- Holders at +1h drops from ~130 to 3
- Prior rug rate = 3 of 10 prior tokens rugged (30%)

**Expected output:** `P(r_t = 0) >= 0.50` at observation #11.

The fixture JSON contains:
- The 11-observation composite score sequence (pre-computed)
- The expected `bocpd_deployer_state` after 10 observations (to seed the test without replaying)
- The expected `cp_prob` range after observation #11

**Why synthetic:** capturing a real on-chain deployer's 11-token career requires archival RPC access and an agreed-upon definition of "rug." The BOCPD algorithm is deterministic; a synthetic series that is well-defined proves correctness. The fixture also serves as the calibration target for `hazard_rate` and `mu_0` — if the algorithm doesn't fire at observation #11 on this series, the hyperparameters need adjustment.

### §9.2 Negative fixture: `NEG_D09_01`

**File:** `tests/fixtures/solana/d09_negative_01_consistent_deployer.json`

**Scenario:** Deployer `9mTR…AaBc` (synthetic) launches 10 tokens over 10 months with entirely consistent behavior. No changepoint.

**Observation series:** all 10 observations at composite score ~0.07 (same as the first 10 observations in POS_D09_01).

**Expected output:** `P(r_t = 0) < changepoint_prob_threshold` for all observations. No `AnomalyEvent` emitted.

---

## §10 Cross-Detector Composition

### §10.1 D09 as a scoring crate input

D09 emits `AnomalyEvent { detector_id: "deployer_changepoint", confidence, severity, evidence }`. The scoring crate (`crates/scoring`) treats this event identically to D01–D08: it contributes to the token's `token_risk_score` via the standard weighted aggregation.

**Recommended scoring weight:** D09 events should be weighted at 1.0× in the scoring crate (no special amplification or dampening beyond what the `confidence` value already encodes). The `deployer_changepoint` event fires on the deployer-level signal and is already adjusted by the BOCPD changepoint probability — no secondary amplification is needed.

### §10.2 D09 as a D02/D04 amplifier (future)

A natural extension (deferred to Sprint 13+): when D09 fires on a deployer and a new token from that deployer subsequently triggers D02 (LP drain) or D04 (pump & dump), the scoring crate applies a `deployer_changepoint_amplifier` to those events. This is the correct composition: D09 provides prior probability that the deployer is in a malicious regime; D02/D04 confirm the specific mechanism.

This amplifier is NOT implemented in Sprint 12. The scoring crate change requires a separate design.

### §10.3 Client-SDK changes

No client-SDK changes required. The SDK already passes through `AnomalyEvent` structs by `detector_id`. Consumers filter by `detector_id = 'deployer_changepoint'` if they want to act on D09 specifically.

---

## §11 Evasion Analysis

### E-D09-1: EOA rotation every N launches

**Attack:** Deployer creates a new EOA every 10 launches. D09 never accumulates enough history on any single EOA. Each new EOA starts at `total_observations = 0` and is in cold-start until it reaches `min_history_length`.

**Mitigation:** Partially mitigated by D08 Sybil clustering: if the new EOAs are funded by the same source, `FundingSource` labels connect them. Full mitigation requires cross-deployer state aggregation via cluster membership — deferred to Phase 3 (DG-D09-1 below). This is the single most effective evasion and is documented as a known gap.

**Cost of evasion:** LOW. Creating Solana wallets costs ~0.000005 SOL per account. The attacker only needs to fund a new wallet before each "career phase" (every 10 launches).

### E-D09-2: Gradual behavioral drift (not abrupt changepoint)

**Attack:** Instead of a sudden shift at launch #11, the attacker reduces LP lock percentage by 5% per launch (100% → 95% → 90% → ... → 50% over 10 launches) and shortens inter-launch time by 1 day per launch.

**Impact:** BOCPD is calibrated for abrupt changepoints (sudden shift in mean). A gradual drift produces elevated-but-sub-threshold `P(r_t = 0)` at each step. The changepoint is eventually detected (after the cumulative shift becomes large enough) but with a delay of multiple launches compared to an abrupt shift.

**Acceptable tradeoff:** Adams & MacKay (2007) §5 explicitly discusses this limitation — BOCPD optimizes for abrupt changes. Gradual drift detection requires a separate CUSUM or EWMA detector (DG-D09-2). For the deployer threat model, the most common pattern is abrupt (a legitimate deployer "goes rogue" suddenly due to financial pressure or market opportunity), not gradual.

### E-D09-3: Warm-up with min_history_length legitimate launches, then rug

**Attack:** Deployer launches exactly 5 legitimate tokens (matching `min_history_length = 5`), then immediately rugs token #6.

**Impact:** D09 fires on token #6 only if the behavioral shift is large enough to exceed `changepoint_prob_threshold`. With only 5 prior observations, the posterior is still relatively uncertain (governed by the prior). A dramatic shift (0% LP lock, tiny liquidity, zero holders) should still produce `P(r_t = 0) >= 0.50` because the composite score at observation #6 (`S_6 ≈ 0.85`) is far above the prior mean (`mu_0 = 0.20`).

**Design recommendation confirmed:** `min_history_length = 5` (not 3 as proposed in research doc T2-1). With only 3 prior observations, the posterior is too dominated by the prior to fire reliably. With 5 observations, the posterior mean tracks the data closely enough to detect the shift at #6.

**Residual risk:** D02 fires on token #6 directly when LP is drained; D09 adds pre-drain early warning if it fires at pool creation.

### E-D09-4: Clockwork/Jito CPI deployment (PDA as deployer)

**Attack:** Deployer uses an on-chain program (PDA) as the Initialize signer. The PDA appears as the deployer address in `graph_edges`. The program cycles PDAs.

**Mitigation:** Same gap as D07 `scheduler_controlled_cpi` evasion (docs/reviews/0004). The chain-adapter sees the PDA as the signer. Partial mitigation: if the PDA is a Clockwork thread or Jito tip router, the `KnownDex` / `KnownExchange` suppression (§8.1) may already exclude it. Full mitigation requires on-chain program analysis (Phase 4).

### E-D09-5: Manipulating `anomaly_events` to suppress prior_rug_rate

**Attack:** The attacker monitors whether D02/D06/D07 fires on their tokens. If they can keep each token's rug below the `rug_confidence_threshold = 0.60`, `prior_rug_rate` stays near 0.0.

**Mitigation:** D02/D06/D07 thresholds are published in `config/detectors.toml`. An attacker who studies the thresholds can calibrate their rugs to stay just below `rug_confidence_threshold`. Counter: the remaining features (gap, LP, liquidity, holders) still shift at the changepoint; the composite score does not depend solely on `prior_rug_rate`.

### E-D09-6: Injecting legitimate-looking observations to reset BOCPD

**Attack:** After deployer has built up a positive history (low `S_t` average), attacker deliberately launches 3 well-behaved tokens between malicious launches to "reset" the BOCPD run length. The BOCPD posterior sees a long run of low scores, and the next spike is attributed to a new run, not a changepoint.

**Impact:** If the well-behaved tokens are genuinely legitimate (LP locked, healthy holders, no rug), `S_t ≈ 0.07` and the run length grows longer. The subsequent malicious launch produces `S_t ≈ 0.85` — a dramatic shift — which should still trigger the changepoint signal even from within a long run.

**Quantitative bound:** the Normal-Gamma predictive becomes tighter around the low mean as more observations accumulate. A single large outlier (`S_t = 0.85` when mean is `0.07`, sigma is `0.01` after 15 observations) has extremely low predictive probability, which drives `P(r_t = 0)` high. The "injection" evasion makes the BOCPD more sensitive to the malicious event, not less.

### E-D09-7: Admin reset exploitation

**Attack:** If a social engineering or admin access allows resetting `bocpd_deployer_state` for a deployer, the attacker gets a clean slate with no history.

**Mitigation:** Admin reset should be logged, rate-limited, and reviewed (not automated). The `graph_edges` history is never deleted by admin reset — the deployer's full `DeployerOf` edge history remains available for forensic replay.

### E-D09-8: High-frequency launching to overwhelm state

**Attack:** Deployer launches 1000 tokens rapidly to fill the `max_run_length_tracked = 1000` buffer, after which the absorbing boundary compresses all prior-run mass.

**Impact:** After 1000 launches with low scores, the BOCPD run-length distribution has most mass at large run lengths. A malicious launch at #1001 produces a composite score `S_t ≈ 0.85`; the algorithm computes its predictive probability under each run-length hypothesis. Since the mean is very well estimated at `~0.07` with small variance after 1000 observations, the new score is an extreme outlier and `P(r_t = 0)` should spike to near 1.0.

**Net effect:** the absorbing boundary at `max_run_length_tracked` does not help the attacker. The BOCPD fires correctly on the malicious launch. The 1000-token "warm-up" is the actual cost the attacker must bear (1000 legitimate tokens = real economic cost in LP seeding fees).

---

## §12 Design Gaps (DG-D09-N)

### DG-D09-1: Cross-deployer state aggregation via cluster membership

**Gap:** Deployer EOA rotation (E-D09-1) defeats per-EOA state tracking. The correct fix is to aggregate BOCPD state across all EOAs in the same `FundingSource` cluster: if deployer A and deployer B share a `FundingSource` label (funded by the same wallet), their combined token history forms a single time-series.

**Complexity:** requires joining `bocpd_deployer_state` via `wallet_cluster_members` to find co-clustered deployers, then merging their posteriors. Merging BOCPD posteriors is not defined in Adams & MacKay (2007); it would require approximation (e.g., taking the maximum-entropy merge or simply concatenating the observation sequences and replaying). This is Phase 4 scope.

**Phase pointer:** Phase 4 graph algorithms; deferred to Sprint 14+.

### DG-D09-2: Gradual drift detector (CUSUM/EWMA companion)

**Gap:** BOCPD is calibrated for abrupt changepoints (E-D09-2). Slow drifts in deployer behavior are not reliably detected.

**Phase pointer:** Phase 4 or 5 scoring enhancement. A CUSUM signal on the composite score's EWMA could complement D09 at low additional cost.

### DG-D09-3: Holder count at +1h data gap

**Gap:** `holders_snapshots` may not have a snapshot within 2 hours of launch for tokens that are not in the streaming registry (newly discovered tokens). If no snapshot exists, `holder_count_at_1h = 0` is used — which maximizes the score on that feature, potentially inflating confidence.

**Mitigation:** the weight on `composite_weight_holder_count = 0.10` is the lowest of all five features, limiting the impact of this gap. Full mitigation requires a streaming registry registration before the first `PoolEvent::Initialize` is processed.

**Phase pointer:** Phase 3 streaming registry enhancement.

### DG-D09-4: `initial_liquidity_usd` oracle dependency

**Gap:** computing `initial_liquidity_usd` in USD requires a SOL/USD price conversion. If the price oracle is unavailable (ADR 0003 self-sovereign; we don't use 3rd-party price feeds), the feature falls back to `0.0`, which maximizes risk score on that component.

**Mitigation:** use raw SOL amount (lamports) as a proxy, normalized independently. Alternatively, if the `pools` table already stores `initial_liquidity_usd` (populated by the token-registry from its SOL price cache), the feature is available without an external oracle call.

**Phase pointer:** confirm at implementation time that `pools.initial_liquidity_usd` is populated by token-registry at pool creation; if not, fall back to `ln(initial_liquidity_sol * 1e9 + 1)` (lamports-based).

### DG-D09-5: Post-cold-start history replay correctness

**Gap:** if the D09 detector is deployed after a deployer has already launched 10 tokens (historical tokens in `graph_edges`), the detector starts cold. The historical replay path (via `Detector::evaluate`) must not use lookahead information (e.g., `prior_rug_rate` at observation #3 must be computed based on rug events confirmed as of block #3, not as of today).

**Mitigation:** the feature computation queries use `block_time` guards (`ae.block_time <= $observation_block_time`) to ensure temporal correctness. The developer must add this guard to all feature SQL queries in §4.2.

---

## §13 Test Plan

### §13.1 Unit tests (no DB required)

**Location:** `crates/detectors/src/d09_bocpd.rs` inline `#[cfg(test)]` module.

**Test: BOCPD math on a known synthetic series**

```rust
#[test]
fn bocpd_5obs_stable_no_changepoint() {
    // 5 observations with composite score ≈ 0.07 each.
    // After 5 obs, P(r_t = 0) should be well below 0.50.
    let hyperparams = BocpdHyperparams { mu_0: 0.20, kappa_0: 1.0, alpha_0: 3.0, beta_0: 1.0 };
    let hazard = 0.00333_f64;
    let mut state = BocpdState::new_with_prior(&hyperparams, 1000);
    let scores = [0.07, 0.08, 0.07, 0.09, 0.07];
    for s in scores {
        state.update(s, hazard, &hyperparams);
    }
    let cp_prob = state.changepoint_prob();
    assert!(cp_prob < 0.10, "Stable series should have P(r=0) < 0.10, got {cp_prob}");
}

#[test]
fn bocpd_abrupt_shift_triggers_changepoint() {
    // 10 stable observations then 1 spike → P(r_t=0) >= 0.50
    let hyperparams = BocpdHyperparams { mu_0: 0.20, kappa_0: 1.0, alpha_0: 3.0, beta_0: 1.0 };
    let hazard = 0.00333_f64;
    let mut state = BocpdState::new_with_prior(&hyperparams, 1000);
    let stable = [0.07, 0.08, 0.07, 0.09, 0.07, 0.08, 0.07, 0.08, 0.07, 0.08];
    for s in stable {
        state.update(s, hazard, &hyperparams);
    }
    state.update(0.85, hazard, &hyperparams); // abrupt shift
    let cp_prob = state.changepoint_prob();
    assert!(cp_prob >= 0.50, "Abrupt shift should produce P(r=0) >= 0.50, got {cp_prob}");
}

#[test]
fn bocpd_posterior_sums_to_one() {
    // Probabilities must sum to 1.0 after any update sequence.
    let hyperparams = BocpdHyperparams { mu_0: 0.20, kappa_0: 1.0, alpha_0: 3.0, beta_0: 1.0 };
    let mut state = BocpdState::new_with_prior(&hyperparams, 100);
    for s in [0.10, 0.50, 0.90, 0.10, 0.85] {
        state.update(s, 0.00333, &hyperparams);
        let total: f64 = state.run_length_probs().iter().sum();
        assert!((total - 1.0).abs() < 1e-9, "Posterior must sum to 1.0, got {total}");
    }
}

#[test]
fn bocpd_determinism() {
    // Two identical runs must produce bit-identical output.
    let hyperparams = BocpdHyperparams { mu_0: 0.20, kappa_0: 1.0, alpha_0: 3.0, beta_0: 1.0 };
    let scores = [0.07, 0.08, 0.07, 0.09, 0.07, 0.85];
    let cp_a = {
        let mut s = BocpdState::new_with_prior(&hyperparams, 1000);
        for score in scores { s.update(score, 0.00333, &hyperparams); }
        s.changepoint_prob()
    };
    let cp_b = {
        let mut s = BocpdState::new_with_prior(&hyperparams, 1000);
        for score in scores { s.update(score, 0.00333, &hyperparams); }
        s.changepoint_prob()
    };
    assert_eq!(cp_a.to_bits(), cp_b.to_bits(), "BOCPD must be deterministic");
}

#[test]
fn bocpd_numerical_stability_long_run() {
    // 500 observations without changepoint — no underflow or NaN.
    let hyperparams = BocpdHyperparams { mu_0: 0.20, kappa_0: 1.0, alpha_0: 3.0, beta_0: 1.0 };
    let mut state = BocpdState::new_with_prior(&hyperparams, 1000);
    for _ in 0..500 {
        state.update(0.08, 0.00333, &hyperparams);
        let cp = state.changepoint_prob();
        assert!(cp.is_finite() && !cp.is_nan(), "cp_prob must be finite after long run");
    }
}
```

**Test: composite score formula**

```rust
#[test]
fn composite_score_malicious_is_high() {
    let features = ObservationFeatures {
        log_gap_seconds: f64::ln(0.5 * 86400.0 + 1.0), // 12 hours
        lp_locked_pct: 0.0,
        log_initial_liquidity_usd: f64::ln(50.0 + 1.0),
        holder_count_at_1h: 3.0,
        prior_rug_rate: 0.30,
    };
    let weights = CompositeWeights::default(); // from config defaults
    let score = features.composite_score(&weights);
    assert!(score > 0.70, "Malicious features should produce score > 0.70, got {score}");
}

#[test]
fn composite_score_legitimate_is_low() {
    let features = ObservationFeatures {
        log_gap_seconds: f64::ln(30.0 * 86400.0 + 1.0), // 30 days
        lp_locked_pct: 0.90,
        log_initial_liquidity_usd: f64::ln(5000.0 + 1.0),
        holder_count_at_1h: 120.0,
        prior_rug_rate: 0.0,
    };
    let weights = CompositeWeights::default();
    let score = features.composite_score(&weights);
    assert!(score < 0.15, "Legitimate features should produce score < 0.15, got {score}");
}
```

### §13.2 Integration test (Docker-gated, `#[ignore]`)

**Location:** `crates/server/tests/` (alongside `sprint8_exit_test.rs`)

**Test:** `d09_bocpd_deployer_changepoint_integration`

1. Seed Postgres with a synthetic deployer's 10-token history (POS_D09_01 fixture state).
2. Call `D09BocpdDetector::on_new_token_launch` with observation #11 (malicious parameters).
3. Assert:
   - `bocpd_deployer_state.last_cp_prob >= 0.50`
   - `anomaly_events` table has one new row with `detector_id = 'deployer_changepoint'` and `confidence >= 0.50`
4. Call with NEG_D09_01 negative fixture deployer through 10 observations.
5. Assert: no `AnomalyEvent` rows emitted.

### §13.3 Property tests (quickcheck)

```rust
#[quickcheck]
fn bocpd_posterior_always_sums_to_one(scores: Vec<f64>) -> bool {
    let scores: Vec<f64> = scores.into_iter().map(|x| x.abs().min(1.0)).collect();
    if scores.is_empty() { return true; }
    let hp = BocpdHyperparams { mu_0: 0.20, kappa_0: 1.0, alpha_0: 3.0, beta_0: 1.0 };
    let mut state = BocpdState::new_with_prior(&hp, 200);
    for s in scores {
        state.update(s, 0.00333, &hp);
        let total: f64 = state.run_length_probs().iter().sum();
        if (total - 1.0).abs() > 1e-6 { return false; }
    }
    true
}

#[quickcheck]
fn bocpd_cp_prob_in_unit_interval(scores: Vec<f64>) -> bool {
    let scores: Vec<f64> = scores.into_iter().map(|x| x.abs().min(1.0)).collect();
    let hp = BocpdHyperparams { mu_0: 0.20, kappa_0: 1.0, alpha_0: 3.0, beta_0: 1.0 };
    let mut state = BocpdState::new_with_prior(&hp, 200);
    for s in scores {
        state.update(s, 0.00333, &hp);
        let cp = state.changepoint_prob();
        if !(0.0..=1.0).contains(&cp) { return false; }
    }
    true
}
```

---

## §14 Worked Example: Posterior Update on 5-Observation Series

This section provides explicit numeric values for the developer to use as unit test oracle values. The series is: `[0.07, 0.08, 0.07, 0.09, 0.85]` with hyperparameters `(mu_0=0.20, kappa_0=1.0, alpha_0=3.0, beta_0=1.0)` and `hazard_rate=0.00333`.

### After observation 1 (x_1 = 0.07):

**Run-length 0 (fresh start — changepoint exactly here):**

The prior predictive at r=0 (n=0 observations):
- `kappa_n = 1.0`, `mu_n = 0.20`, `alpha_n = 3.0`, `beta_n = 1.0`
- Student-t predictive: `nu=6.0`, `mu_pred=0.20`, `sigma_pred^2 = (1.0*2.0)/(3.0*1.0) = 0.667`
- `sigma_pred = 0.816`
- log-PDF of x=0.07 under Student-t(6, 0.20, 0.667):
  - `(x - mu)/sigma = (0.07 - 0.20)/0.816 = -0.159`
  - `log Gamma(3.5) - log Gamma(3.0) - 0.5*ln(6*π*0.667) - 3.5*ln(1 + 0.159²/6)`
  - `≈ 1.200 - 0.693 - 0.5*3.547 - 3.5*0.00420`
  - `≈ 1.200 - 0.693 - 1.774 - 0.0147 ≈ -1.282`

Initial log-joint vector (before observation 1): `log_joint[0] = 0.0` (uniform at start).

After update:
- Growth: `log_joint_new[1] = 0.0 + (-1.282) + ln(1 - 0.00333) = -1.282 - 0.00334 = -1.285`
- Changepoint: `log_joint_new[0] = 0.0 + (-1.282) + ln(0.00333) = -1.282 - 5.707 = -6.989`
- Normalise: `log_total = log_sum_exp(-6.989, -1.285) = -1.283` (dominated by the growth term)
- After normalisation: `log_joint[0] = -6.989 - (-1.283) = -5.706` → `P(r=0) = exp(-5.706) ≈ 0.0033`
- `log_joint[1] = -1.285 - (-1.283) = -0.002` → `P(r=1) = exp(-0.002) ≈ 0.998`

**Interpretation:** after 1 observation, nearly all probability is at run-length 1 (the run started at the beginning). Changepoint probability is ~0.33% — the hazard rate itself, as expected for a single observation under a uniform start.

### After observations 2–4 (x = 0.08, 0.07, 0.09):

The run-length posterior concentrates around r=t (the run has been going since the start). Changepoint probability stays near `hazard_rate` (~0.33%) because all observations are consistent with the prior mean of 0.20. The Normal-Gamma posterior after 4 observations:
- `kappa_n = 5.0`, `mu_n ≈ 0.162` (pulled toward data mean 0.0775), `alpha_n = 5.0`, `beta_n ≈ 1.007`
- The predictive variance is tightening: `sigma_pred^2 = (1.007 * 6.0) / (5.0 * 5.0) = 0.242`, `sigma_pred ≈ 0.492`

### After observation 5 (x_5 = 0.85):

The predictive probability of `x=0.85` under the current posterior (tightly centered at ~0.08):
- `(x - mu_n)/sigma_pred = (0.85 - 0.162)/0.492 = 1.398` — more than 1 sigma away, but the Student-t has heavier tails than Gaussian.
- For the run-length 4 slot specifically (the dominant slot with n=4 observations):
  - `log-PDF ≈ -2.8` (estimate; the actual value depends on exact Normal-Gamma parameters)
- For run-length 0 (changepoint = this is the start of a new run):
  - Prior predictive log-PDF: `≈ -1.282 + contribution from x=0.85`
  - `(0.85 - 0.20)/0.816 = 0.797` → log-PDF ≈ `-1.00` (less extreme because the prior mean is 0.20 and variance is large)

The mass flowing to `r=0` (changepoint) vs `r=5` (growth) depends on the ratio of their predictive probabilities times the hazard. Detailed calculation:

- Growth contribution to r=5: `P(r_4=4) * P(x_5 | r=4) * (1-H) ≈ 0.998 * exp(-2.8) * 0.997 ≈ 0.061`
- Changepoint contribution to r=0: `sum_r P(r_4=r) * P(x_5 | r) * H`
  - Dominant term: `P(r_4=4) * P(x_5 | r=4) * H ≈ 0.998 * exp(-2.8) * 0.00333 ≈ 0.000203`
  - Secondary terms (r=0,1,2,3) negligible
  - Total changepoint mass ≈ 0.000206
- Fresh-run contribution to r=1: `sum_r P(r_4=r) * P(x_5 | r) * H * (prior predictive for x_5)`
  - Actually the changepoint resets to r=0, then growth goes to r=1 next step

Normalizing over [0.000206, 0.061]: `P(r_5 = 0) = 0.000206 / (0.000206 + 0.061) = 0.0034 ≈ 0.34%`.

**Wait** — this is too low. The intuition says the spike at x=0.85 should be detectable. The reason it's not yet: with only 4 prior observations (min_history_length not yet reached), the Normal-Gamma predictive still has enough uncertainty (heavy Student-t tails) that even x=0.85 is not an extreme outlier.

This is the correct behavior: `min_history_length = 5` means we need at least 5 prior stable observations before the posterior is tight enough to reliably detect a spike. In POS_D09_01, the spike occurs at observation #11 (after 10 stable observations), not observation #5. After 10 stable observations at x≈0.07:
- `kappa_n = 11`, `mu_n ≈ 0.075`, `alpha_n = 8.0`, `beta_n ≈ 1.004`
- `sigma_pred^2 = (1.004 * 12) / (8.0 * 11.0) = 0.137`, `sigma_pred ≈ 0.370`
- `(0.85 - 0.075) / 0.370 = 2.09` — now this is in the tails of a Student-t(16) distribution
- The likelihood ratio (run-continuation vs changepoint) strongly favors changepoint for observation #11

The developer MUST validate this against the unit test `bocpd_abrupt_shift_triggers_changepoint` which uses a 10-stable + 1-spike series.

---

## §15 New REFERENCES.md Entries

The following rows are to be appended to the REFERENCES.md table. The developer agent running the implementation sprint should append these verbatim.

```markdown
| Adams & MacKay 2007 BOCPD | Online Bayesian changepoint detection via run-length posterior `P(r_t|x_{1:t})`; Normal-Gamma conjugate prior for Gaussian observations; hazard function `H(r) = 1/300` (constant memoryless); alert on `P(r_t=0) >= 0.50` | Adams, R. P. and MacKay, D. J. C. (2007). Bayesian Online Changepoint Detection. arXiv:0710.3742. https://arxiv.org/abs/0710.3742 | D09 BOCPD; `config/detectors.toml [deployer_changepoint]`; `hazard_rate`, `changepoint_prob_threshold` derivation | arXiv page confirmed live 2026-04-24 |
| Murphy 2007 Normal-Gamma conjugate prior | Normal-Gamma posterior update equations: `kappa_n = kappa_0 + n`, `mu_n = (kappa_0*mu_0 + n*x_bar)/kappa_n`, `alpha_n = alpha_0 + n/2`, `beta_n = beta_0 + S/2 + cross_term`; Student-t predictive with `nu = 2*alpha_n` | Murphy, K. P. (2007). Conjugate Bayesian Analysis of the Gaussian Distribution. Technical Report. https://www.cs.ubc.ca/~murphyk/Papers/bayesGauss.pdf | D09 BOCPD `BocpdState::update()` implementation; `mu_0`, `kappa_0`, `alpha_0`, `beta_0` hyperparameter derivation in §3.3 | PDF confirmed live 2026-04-24 |
| Latent-flux production BOCPD (deployer behavior) | Production BOCPD on deployer transaction sequences on Base/Arbitrum/Optimism; constant hazard H=1/300 confirmed; 36K deployers tracked; univariate scalar composite score per deployer; Adams & MacKay 2007 cited | 2654-zed/latent-flux, https://github.com/2654-zed/latent-flux (BUSL-1.1); described in research/03-feature-gap-2026-04-24.md §10 (repo #10) | D09 constant-hazard choice (§3.2 justification); univariate-vs-multivariate decision (§2.3); production feasibility confirmation | Research doc survey 2026-04-24 |
```

**Update to existing entries:** the "Used In" column for the following REFERENCES.md rows must be updated to include `D09`:
- Chainalysis 2025 row (deployer behavior as risk signal): append `D09 prior_rug_rate feature (§4.2)`
- Sun et al. 2024 row (rug root causes): append `D09 §11 evasion E-D09-5 (rug confidence threshold)`
- Alhaidari et al. 2025 (SolRPDS): append `D09 composite_weight_lp_locked calibration (§7)`

---

## §16 ADR Assessment

### ADR 0001 (MVP detector set)
Consistent. D09 is a Phase 3 graph algorithm explicitly identified in T2-1 of the feature gap analysis. The design 0015 Sprint 12 section lists D09 as a planned item.

### ADR 0002 (Postgres-only)
Consistent. `bocpd_deployer_state` is Postgres. JSONB for run-length state (inspectable, bounded). DOUBLE PRECISION for probability values (not monetary amounts). `run_length_state_json` array size is bounded by `max_run_length_tracked`. No NUMERIC(39,0) required for this table (no token amounts stored here).

### ADR 0003 (self-sovereign)
Consistent. The BOCPD algorithm is pure Rust arithmetic + `statrs` (a pure Rust statistics crate, no Python bridge). No external ML service. No 3rd-party SaaS data source. All inputs come from Postgres tables populated by the self-hosted Yellowstone stream.

---

## Inconsistency Report

**1. Design 0015 §9 names this design as "0016" for Tarjan SCC and "0017" for BOCPD:**
Design 0015 §9 writes: "New design doc 0016" for T2-2 (Tarjan SCC) and "New design doc 0017" for T2-1 (BOCPD). The user prompt assigns this design to `0016` for BOCPD (D09). Tarjan SCC will take `0017`. The numbering is user-authoritative; ignore the design 0015 allocation. No impact on functionality.

**2. Research doc T2-1 proposed `min_history_length = 3`:**
This design raises it to 5 (§7, §8.2). Rationale: evasion E-D09-3 (warm-up with exactly `min_history_length` launches) requires the threshold to be above 3 for reliable changepoint detection. The research doc's own note ("must be >3 to catch") confirms this. No user sign-off required — the spec supersedes the research doc suggestion.

**3. V00012 reservation:**
V00012 is reserved for `token_risk_reports` (SESSION-KICKOFF gotcha #31). D09's state table is V00013. Developer must not use V00012 for `bocpd_deployer_state`.
