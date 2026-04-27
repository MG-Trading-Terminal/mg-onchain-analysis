# Design 0018 — D11: Synchronized-Activity Clustering Detector (Sprint 14)

**Date:** 2026-04-24
**Status:** Draft — awaiting user sign-off on §11 decisions before implementation
**Author:** onchain-analyst agent
**Sprint:** 14 (B1 from Sprint 14 candidate tracks)
**ADR refs:**
- ADR 0001 §D5 — MVP detector set; Phase 3 graph algorithms; D11 is Phase 3 B-track
- ADR 0002 — Postgres-only storage; NUMERIC(39,0) for u128; string-bridged amounts
- ADR 0003 — self-sovereign infrastructure; pure-Rust algorithms; no 3rd-party SaaS in hot path
**Related designs:**
- `docs/designs/0008-detector-05-wash-trading.md` — D05 wash trading; Signal A covers per-wallet round-trips; D11 covers cross-wallet synchronization
- `docs/designs/0015-crates-graph-phase3.md` — Sprint 11 graph foundation; `address_labels`; `wallet_clusters`; `cluster_kind` CHECK constraint (already includes `'synchronized_activity'`)
- `docs/designs/0017-d05-signal-b-graph-cycles.md` — D05 Signal B cycle detection; structural template for graph-layer read path (Option D: transient in-memory from `transfers`/`swaps` tables)
- `docs/designs/0016-detector-09-bocpd-deployer-changepoint.md` — D09 BOCPD deployer changepoint; decisions pattern template; §11 sign-off format reference
- `docs/designs/0003-detector-trait.md` — Detector trait + DetectorContext

**Binding prior art in REFERENCES.md:**
- Mazza, Cresci et al. 2019 (RTbust, ACM WebSci 2019) — temporal DBSCAN clustering primary citation
- Mannocci, Mazza et al. 2024 (CIB Survey, arXiv:2408.01257) — definitional precision; pairwise temporal similarity formalization
- Arnold et al. 2024 (Temporal Motifs, Scientific Reports, arXiv:2402.09272) — on-chain primary citation; N_min threshold derivation
- Nizzoli, Tardelli et al. 2020 (Crypto Landscape, IEEE Access) — domain validation; >56% P&D Telegram bots; social-bot → on-chain bridge
- Liu et al. 2025 (arXiv:2505.09313) — Sybil temporal clustering feature (already in REFERENCES.md for D08)

---

## §1 Background: Coverage Gap D05/D08/D09 vs D11

### §1.1 What the existing Phase 3 detectors cover

The detector suite as of Sprint 13 addresses coordinated behavior through three orthogonal lenses:

**D05 Signal B (cycle detection):** Detects circular token flows — A→B→C→A patterns in the `transfers` graph within a 120-minute window. The signal is structural: it requires tokens to actually circulate through a ring of wallets. A pump coordinated by wallets that only BUY (never transfer between themselves) is invisible to D05 Signal B. A pre-launch accumulation phase where insiders independently buy the same token simultaneously — without token circulation — is also invisible.

**D08 Sybil (common-funder clustering):** Detects wallets that were funded from the same source and hold the same token. The signal is funding-graph based: wallets must share a common funder. Two wallets that independently acquired their trading capital but coordinate their actions are invisible to D08. Sophisticated actors routinely fragment across exchanges and mixing services specifically to defeat funding-graph clustering.

**D09 BOCPD (deployer changepoint):** Detects behavioral regime shifts in a single deployer's career arc — a deployer who suddenly changes their inter-launch cadence, LP lock percentage, or initial liquidity pattern. D09 is instanced per-deployer; it is entirely blind to cross-wallet coordination among buyers, not deployers. A botnet of buyer wallets targeting a token triggers no D09 signal.

### §1.2 The gap D11 fills

**D11 detects coordinated inauthentic buying activity by measuring near-simultaneous action timing across a cluster of wallets, regardless of whether those wallets:**
- transfer tokens among themselves (D05 prerequisite)
- share a common funder (D08 prerequisite)
- are associated with the deployer (D09 scope)

The signal is purely temporal: if N distinct wallets all execute buy swaps on the same token within a δ-second window, and the probability of this co-occurrence under a Poisson null model of independent random buying is below a threshold p, then coordinated behavior is the more parsimonious explanation.

This pattern captures:
- **Coordinated pump-starts:** botnet wallets executing synchronized first-buys to trigger price discovery and create the illusion of organic demand
- **Pre-P&D accumulation phase:** Karbalaii (2025) documented that 70% of pump events have an accumulation phase; if that accumulation is coordinated (multiple insiders buying simultaneously), D11 catches it even before the price spike that D04 requires
- **Airdrop-farming botnet launches:** wallets farming an airdrop by all interacting with a token contract in the same block window
- **Wash trading coordination without transfer rings:** multiple wallets buying simultaneously in a rotation that avoids direct A→B→C→A transfers

### §1.3 Reference precedent

The Mannocci et al. (2024) CIB Survey formalizes: "for each pair of actors (i, j), compute a temporal similarity score over a rolling window W; flag clusters of actors with high pairwise similarity as coordinated." RTbust (Mazza et al. 2019) operationalizes this with LSTM autoencoders + hierarchical DBSCAN on Twitter botnet retweet patterns, achieving F1 = 0.87 on 10M events. The structural argument transfers directly: individual wallet buys are innocuous; inter-wallet timing correlation is the group-level signal. Arnold et al. (2024) establishes empirically that 3-node temporal motifs in cryptocurrency transaction networks carry anomaly signal invisible in aggregate counts. Nizzoli et al. (2020) validates that the CIB social-bot methodology applies to on-chain crypto coordination specifically: >56% of P&D Telegram channel accounts are bots — the on-chain buying that follows Telegram pump signals is therefore predominantly coordinated.

---

## §2 Goals and Non-Goals

### §2.1 Goals

1. Detect clusters of N_min or more distinct wallets each executing a buy swap on the same token within a δ-second window, where the co-occurrence probability under a Poisson null model is below p_threshold.
2. Emit a continuous confidence score in [0.0, 1.0] — not a boolean. Confidence is proportional to cluster size, temporal tightness, and statistical significance (p-value).
3. Be deterministic: given identical DetectorContext inputs (same `swaps` and `transfers` table rows for the same token and time window), produce bit-identical output.
4. Operate read-only from existing `swaps` and/or `transfers` Postgres tables — no new migration unless a stateful persistence decision is made in §11.
5. Emit evidence bundles with all wallet addresses, timestamps, and cluster metrics sufficient for human review.
6. Integrate as a cadenced streaming detector slotted into the existing streaming scheduler (same pattern as D08, D01 cadenced path).

### §2.2 Non-Goals

1. Detection of coordinated social-media activity (off-chain) — D11 is on-chain only.
2. Detection of coordinated LP provisioning by legitimate market makers — this is a known false positive scenario addressed in §8.
3. Cross-token synchronization (wallets synchronizing buys across multiple tokens simultaneously) — deferred to Phase 5 scoring layer.
4. Consumer integration — standalone service only (SESSION-KICKOFF §21 boundary).
5. EVM chain support — Phase 4. D11 is Solana-first per ADR 0001 §D1.
6. Real-time per-block detection (sub-second latency) — D11 is cadenced, evaluating accumulated swap history over a lookback window. Real-time motif streaming is a future enhancement.

---

## §3 Algorithm

### §3.1 High-level pipeline

```
Input:  swaps (and optionally transfers) for token T over lookback window L
        Per-token rolling action rate λ (actions per second) from 7-day history

Step 1. Fetch raw events
        -- SQL query over `swaps` (+ optionally `transfers`) for token T
        -- window: [now - max_lookback_minutes, now)
        -- filter: buy-side events only (swap direction = buy)
        -- output: Vec<ActionEvent { wallet, block_time, tx_hash, amount_raw }>
        -- order: block_time ASC, tx_hash ASC (determinism)

Step 2. Bucketize into δ-second slots
        -- Divide the lookback window into T = L / δ discrete time buckets
        -- For each wallet w, produce a binary presence vector B_w ∈ {0,1}^T
        --   B_w[t] = 1 if wallet w had at least one action in bucket t, else 0
        -- Wallets with zero actions in the window are excluded (no row)

Step 3. Compute pairwise Jaccard similarity
        -- For each pair (i, j) of distinct wallets:
        --   J(i,j) = |B_i ∩ B_j| / |B_i ∪ B_j|
        --          = (number of buckets where both active) /
        --            (number of buckets where at least one active)
        -- J is bounded [0,1]; J=0 means no temporal overlap; J=1 means identical patterns
        -- Only compute pairs where |B_i ∪ B_j| >= min_union_buckets (default 1) to avoid
        --   0/0 division on wallets with no overlap

Step 4. DBSCAN clustering on Jaccard similarity matrix
        -- Input: pairwise distance matrix D where D(i,j) = 1 - J(i,j)
        -- Parameters: eps = 1 - jaccard_similarity_threshold (default eps = 0.30,
        --             i.e. J(i,j) >= 0.70 to be neighbors)
        --             min_samples = min_cluster_size (default 5)
        -- DBSCAN is deterministic given sorted input (wallets sorted ascending by
        --   their first-action block_time, ties broken by tx_hash ascending)
        -- Output: Vec<Cluster { wallets: Vec<String>, core_wallets: Vec<String> }>
        -- Noise points (cluster = -1) are discarded

Step 5. For each cluster C with |C| >= min_cluster_size:
        a. Compute temporal_tightness:
             -- For each wallet in C, find its earliest action block_time in the window
             -- temporal_spread = max(earliest_times) - min(earliest_times) in seconds
             -- temporal_tightness = 1.0 - (temporal_spread / delta_seconds)
             --   clamped to [0.0, 1.0]
             -- tightness = 1.0 → all wallets first-acted within the same bucket
             -- tightness = 0.0 → first actions spread across the full window

        b. Compute Poisson p-value:
             -- λ_token = per-token action rate (actions per second) from 7-day rolling window
             -- If no 7-day history: use λ_token = 0.0 (p-value = 1.0, no signal)
             -- Probability that one wallet acts within δ seconds:
             --   p_one = 1 - exp(-λ_token * delta_seconds)  [f64 probability]
             -- Probability that k = |C| wallets each independently act within δ seconds
             --   (each independently Poisson-distributed):
             --   p_joint = p_one ^ k
             -- This is the p-value under the null hypothesis of independent random buying

        c. Apply filter criteria (§5):
             -- Drop clusters where temporal_tightness < temporal_tightness_threshold
             -- Drop clusters where p_value > poisson_p_threshold
             -- Drop clusters where |C| < min_cluster_size

        d. Compute confidence score (§4)

        e. Emit AnomalyEvent with evidence bundle (§6.4)

Step 6. If multiple clusters found for token T: emit the highest-confidence event.
        -- Secondary clusters may be included in evidence bundle but do not generate
        --   additional top-level AnomalyEvents (avoids alert storm per token).
        -- Document all cluster metadata in evidence for human review.
```

### §3.2 Determinism invariants

- SQL query ordered by `block_time ASC, tx_hash ASC` — same rows always in same order.
- Wallet vertex IDs assigned in order of first appearance in the sorted event stream — deterministic.
- Jaccard computed over sorted wallet pairs (i < j by wallet ID assignment order).
- DBSCAN processes wallets in ascending order of wallet ID (first-action order). No random seed.
- p-value is a pure mathematical formula over f64; same inputs → same output.
- Confidence formula is deterministic (§4).

### §3.3 Pseudocode: DBSCAN implementation

DBSCAN is a 60-90 line algorithm. Hand-roll as a pure-Rust function in `crates/detectors/src/d11_synchronized_activity.rs`. Do not introduce a new crate dependency solely for DBSCAN.

```
// Inputs:
//   wallets: Vec<WalletId>   (sorted ascending by first-action order)
//   dist:    fn(i, j) -> f64  (1 - Jaccard; symmetric)
//   eps:     f64              (max distance for neighborhood; = 1 - jaccard_similarity_threshold)
//   min_samples: usize        (= min_cluster_size config)
//
// Output: Vec<i32>  (cluster label per wallet; -1 = noise)

fn dbscan(wallets: &[WalletId], dist: impl Fn(usize, usize) -> f64,
          eps: f64, min_samples: usize) -> Vec<i32> {

    let n = wallets.len();
    let mut labels = vec![-1_i32; n];
    let mut cluster_id: i32 = 0;

    // Precompute all pairwise distances (O(n^2), bounded by max wallets per token per window)
    // Safety: n is bounded by max_wallets_per_cluster_cap (§9, default 500)
    let neighbors = |i: usize| -> Vec<usize> {
        (0..n).filter(|&j| j != i && dist(i, j) <= eps).collect()
    };

    for i in 0..n {
        if labels[i] != -1 { continue; }  // already classified

        let nbrs = neighbors(i);
        if nbrs.len() + 1 < min_samples {
            // i is noise (may be absorbed into a cluster later as border point)
            continue;
        }

        // i is a core point; start a new cluster
        labels[i] = cluster_id;
        let mut seed_set: Vec<usize> = nbrs;
        let mut seed_idx = 0;

        while seed_idx < seed_set.len() {
            let j = seed_set[seed_idx];
            seed_idx += 1;

            if labels[j] == -1 {
                // j was noise; absorb as border point
                labels[j] = cluster_id;
            }
            if labels[j] != -1 && labels[j] != cluster_id {
                // j belongs to another cluster — do not merge (not single-linkage DBSCAN)
                continue;
            }
            if labels[j] == cluster_id { continue; }

            labels[j] = cluster_id;
            let j_nbrs = neighbors(j);
            if j_nbrs.len() + 1 >= min_samples {
                // j is also a core point; add its neighbors to the seed set
                for &k in &j_nbrs {
                    if labels[k] == -1 || labels[k] != cluster_id {
                        seed_set.push(k);
                    }
                }
            }
        }

        cluster_id += 1;
    }

    labels
}
```

Complexity: O(n^2) where n = number of distinct wallets acting on the token in the lookback window. With `max_wallets_per_cluster_cap = 500` (§9), worst case is 250,000 distance evaluations per token per evaluation cycle. At sub-microsecond per evaluation, this is ~0.25ms worst case. Acceptable.

---

## §4 Signal Math: Confidence Formula

### §4.1 Component scores

Three independent sub-signals combine into `conf_raw`:

**Sub-signal 1: Cluster size score (S_size)**

Derived from the observed cluster size relative to the minimum and a saturation point.

```
S_size = sigmoid((cluster_size - min_cluster_size) / cluster_size_scale)

where:
  cluster_size       = |C| (number of distinct wallets in the cluster)
  min_cluster_size   = config [synchronized_activity_v1].min_cluster_size (default 5)
  cluster_size_scale = 5.0  (sigmoid stretch; config [synchronized_activity_v1].cluster_size_scale)

sigmoid(x) = 1 / (1 + exp(-x))
```

At `cluster_size = min_cluster_size` (exactly at threshold): `S_size = sigmoid(0) = 0.50`
At `cluster_size = min_cluster_size + 5`: `S_size = sigmoid(1.0) ≈ 0.73`
At `cluster_size = min_cluster_size + 10`: `S_size = sigmoid(2.0) ≈ 0.88`
At `cluster_size = min_cluster_size + 20`: `S_size = sigmoid(4.0) ≈ 0.98`

Rationale: RTbust (Mazza et al. 2019) finds that cluster size is the strongest individual predictor of coordinated behavior. The sigmoid maps the integer cluster size to a smooth [0,1] confidence contribution. The scale of 5 was chosen so that a cluster of 10 wallets (min_cluster_size + 5) already produces S_size ≈ 0.73 — a meaningfully elevated signal — consistent with the Arnold et al. (2024) observation that 3-node motif counts are heavy-tailed with rapid falloff above 5.

**Sub-signal 2: Temporal tightness score (S_tight)**

```
S_tight = temporal_tightness  (already in [0.0, 1.0] from §3.1 Step 5a)
```

Direct linear use: tightness = 1.0 means all wallets acted in the same δ-second bucket (maximum synchrony). Tightness = 0.0 means the first actions were spread across the entire lookback window.

**Sub-signal 3: Statistical significance score (S_stat)**

```
p_value = p_one ^ cluster_size
        = (1 - exp(-lambda_token * delta_seconds)) ^ cluster_size

S_stat = 1.0 - p_value / poisson_p_threshold   if p_value <= poisson_p_threshold
       = 0.0                                      if p_value >  poisson_p_threshold

clamped to [0.0, 1.0]
```

At `p_value = poisson_p_threshold` (exactly at threshold): `S_stat = 0.0`
At `p_value = 0.0`: `S_stat = 1.0`
At `p_value = poisson_p_threshold / 2`: `S_stat = 0.5`

Rationale: The Mannocci et al. (2024) survey recommends a Poisson null model for temporal co-activity detection. The RTbust research note (research/sprint13-b-citations.md §"Statistical Framework Recommendation") derives: for k=5 wallets, λ=1 action/hour, δ=30s, p_joint ≈ 4×10^-10 — far below any threshold of interest. The mapping is linear from 0 to p_threshold to avoid treating borderline p-values as equally significant as p=0.

### §4.2 Combined confidence

```
conf_raw = (w_size * S_size + w_tight * S_tight + w_stat * S_stat)
           / (w_size + w_tight + w_stat)

where:
  w_size  = 0.40  (config: synchronized_activity_v1.weight_cluster_size)
  w_tight = 0.30  (config: synchronized_activity_v1.weight_temporal_tightness)
  w_stat  = 0.30  (config: synchronized_activity_v1.weight_statistical_significance)
```

Default weights: size (40%) > tightness (30%) = significance (30%). Cluster size is weighted highest because it is the most externally validated predictor (RTbust 2019, Arnold 2024). Tightness and significance are co-equal — both provide complementary evidence of actual coordination rather than coincidental overlap.

Weights are config-exposed (§9) to allow calibration against the labelled fixture corpus.

### §4.3 Severity ladder mapping

The confidence is capped and mapped to severity using the shared `severity_from_confidence` helper (gotcha #16):

```
conf_final = min(conf_raw, 0.90)   // hard cap; irreducible uncertainty about intent
                                   // 0.90 matches D08 Sybil signal cap

severity = severity_from_confidence(conf_final)
// Existing ladder (from crates/common):
//   >= 0.80 -> Critical
//   >= 0.60 -> High
//   >= 0.40 -> Medium
//   >= 0.20 -> Low
//   < 0.20  -> Informational
```

The 0.90 cap is justified by RTbust's 0.87 F1 on a clean labelled dataset: even the best-performing temporal synchronization classifier has irreducible false positive rate at 0.13 — mapping to full confidence = 1.0 would overstate certainty. The 0.90 cap leaves room for future ensemble amplification from co-firing D05 or D08 signals.

### §4.4 No f64 for money

All `amount_raw` values are `u128` parsed from `NUMERIC(39,0)` Postgres columns via String bridge. USD amounts are `rust_decimal::Decimal`. The p-value, S_size, S_tight, S_stat, conf_raw, and conf_final are all probabilities or normalized scores — they are f64 by type convention. No monetary amount is ever stored or computed as f64 (CLAUDE.md binding rule; ADR 0002).

---

## §5 Filter Criteria

Clusters that pass DBSCAN formation must also satisfy all of the following hard gates before confidence is computed. A cluster failing any gate is discarded silently (logged at TRACE level with reason).

### §5.1 Minimum cluster size

```
|C| >= min_cluster_size    (config: synchronized_activity_v1.min_cluster_size, default 5)
```

**Derivation:** Arnold et al. (2024) uses 3-node temporal motifs as the minimum structural unit. RTbust clusters with N >= 10 in the Twitter botnet corpus. The research recommendation (research/sprint13-b-citations.md §"Suggested Signal/Threshold Formulation") uses 5 as "a conservative midpoint for shitcoin context where genuine launch communities may have 3–4 simultaneous buyers." A shitcoin token launched organically on pump.fun may attract 3–4 simultaneous buyers from a trending Telegram post. The threshold N_min = 5 is conservatively above this noise floor. See §11 Decision 5 for the alternatives.

**Config key:** `synchronized_activity_v1.min_cluster_size`

### §5.2 Minimum temporal tightness

```
temporal_tightness >= temporal_tightness_threshold
                      (config: synchronized_activity_v1.temporal_tightness_threshold, default 0.50)
```

At `temporal_tightness = 0.50` with `delta_seconds = 30`: the cluster's earliest first-actions span at most 15 seconds. This eliminates clusters where wallets acted in the same general epoch (same hour) but not with the sub-minute precision that indicates coordination vs coincidence.

**Derivation:** No published threshold for this metric. Proposed as the midpoint of [0, 1.0]. Must be calibrated against positive fixtures (coordinated buys) and negative fixtures (organic community launches). Classified as `unverified-heuristic` until Sprint 14 fixture calibration.

**Config key:** `synchronized_activity_v1.temporal_tightness_threshold`

### §5.3 Statistical significance gate

```
p_value <= poisson_p_threshold
           (config: synchronized_activity_v1.poisson_p_threshold, default 1e-6)
```

**Derivation:** The research recommendation (research/sprint13-b-citations.md §"Suggested Signal/Threshold Formulation") derives p ≈ 4×10^-10 for k=5, λ=1/hour, δ=30s. 1e-6 is deliberately lenient relative to this example to accommodate tokens with higher organic activity rates (λ > 1/hour). At λ = 10/hour (600 buys/day), p_one = 1 - exp(-10/3600 * 30) ≈ 0.077; p_joint for k=5 is 0.077^5 ≈ 2.7×10^-5 — still well below 1e-6 at the threshold calibration point. The 1e-6 threshold fires only when the independent-action hypothesis is extremely improbable. **Note: if `lambda_token = 0.0` (no 7-day history), p_value is set to 1.0 and the cluster is discarded by this gate.** This prevents spurious fires during warmup for brand-new tokens.

**Config key:** `synchronized_activity_v1.poisson_p_threshold`

### §5.4 Warmup guard

```
7-day action history must exist with >= min_baseline_events events
(config: synchronized_activity_v1.min_baseline_events, default 10)
```

If the token has fewer than `min_baseline_events` actions in the 7-day window used to compute `lambda_token`, the Poisson baseline is unreliable and D11 emits no event. This matches the D04 `min_baseline_days = 3` guard pattern (REFERENCES.md D04 burst_concentration_threshold row).

**Config key:** `synchronized_activity_v1.min_baseline_events`

### §5.5 Target false positive rate

Given the base rate of coordinated buying events (estimated 3–5% of Solana tokens show coordination per Chainalysis 2025 P&D base rate ≈ 3.59%), and a target precision of >= 70% (acceptable for a consumer that rates by confidence):

```
FP rate target: < 5% at confidence >= 0.60 threshold
```

This is not achieved by thresholds alone; the calibration fixture corpus (§7, §12) is the verification mechanism. The 0.90 confidence cap and the p_value gate together are the primary FP suppressors.

---

## §6 Integration

### §6.1 Detector trait implementation

```rust
// crates/detectors/src/d11_synchronized_activity.rs

pub struct D11SynchronizedActivityDetector {
    config: SynchronizedActivityConfig,
    pg: Arc<PgPool>,
}

impl Detector for D11SynchronizedActivityDetector {
    fn detector_id(&self) -> &str {
        "synchronized_activity_v1"
    }

    async fn evaluate(&self, ctx: &DetectorContext) -> Result<Vec<AnomalyEvent>> {
        // ctx.token: &TokenMeta
        // ctx.observed_at: DateTime<Utc>  -- from block_time (gotcha #28; NEVER Utc::now())
        // ctx.chain: Chain

        // Step 1: Fetch events
        let events = fetch_action_events(
            &self.pg,
            ctx.chain.as_str(),
            &ctx.token.mint,
            ctx.observed_at - Duration::minutes(self.config.max_lookback_minutes as i64),
            ctx.observed_at,
            self.config.action_source_kinds.as_slice(),
            self.config.max_events_per_window,
        ).await?;

        if events.len() < self.config.min_cluster_size {
            return Ok(vec![]);  // Not enough events to form any cluster
        }

        // Step 2: Compute lambda from 7-day rolling window
        let lambda_token = compute_lambda(
            &self.pg,
            ctx.chain.as_str(),
            &ctx.token.mint,
            ctx.observed_at - Duration::days(7),
            ctx.observed_at,
            self.config.action_source_kinds.as_slice(),
        ).await?;

        if lambda_token == 0.0 || events_in_history < self.config.min_baseline_events {
            return Ok(vec![]);  // Warmup guard (§5.4)
        }

        // Steps 2-4: Bucketize → Jaccard → DBSCAN
        let buckets = bucketize_events(&events, self.config.delta_seconds);
        let wallets: Vec<String> = buckets.keys().cloned().collect();
        // wallets sorted by first action block_time ASC, tx_hash ASC (determinism)
        let jaccard_dist = |i: usize, j: usize| -> f64 {
            one_minus_jaccard(&buckets[&wallets[i]], &buckets[&wallets[j]])
        };
        let labels = dbscan(
            &wallets,
            jaccard_dist,
            1.0 - self.config.jaccard_similarity_threshold,
            self.config.min_cluster_size,
        );

        // Step 5: Evaluate clusters, compute confidence, emit events
        let clusters = extract_clusters(&wallets, &labels);
        let mut best_event: Option<AnomalyEvent> = None;

        for cluster in clusters {
            if cluster.wallet_count < self.config.min_cluster_size { continue; }

            let tightness = compute_temporal_tightness(&cluster, &events, self.config.delta_seconds);
            if tightness < self.config.temporal_tightness_threshold { continue; }

            let p_value = compute_poisson_p_value(
                lambda_token,
                self.config.delta_seconds as f64,
                cluster.wallet_count,
            );
            if p_value > self.config.poisson_p_threshold { continue; }

            // Established-protocol suppression (§6.3)
            if self.config.suppress_established_protocols
                && ctx.token.is_established_protocol
            {
                continue;
            }

            let conf = compute_confidence(&self.config, cluster.wallet_count, tightness, p_value);
            let evidence = build_evidence(&cluster, &events, tightness, p_value, conf);
            let event = AnomalyEvent {
                detector_id: self.detector_id().to_string(),
                token: ctx.token.mint.clone(),
                chain: ctx.chain.clone(),
                confidence: conf,
                severity: severity_from_confidence(conf),
                observed_at: ctx.observed_at,  // from block_time, never Utc::now()
                evidence,
            };

            best_event = Some(match best_event {
                None => event,
                Some(prev) if event.confidence > prev.confidence => event,
                Some(prev) => prev,
            });
        }

        Ok(best_event.into_iter().collect())
    }
}
```

### §6.2 What DetectorContext fields are needed

D11 requires the following fields from `DetectorContext` (all already present in the shared trait):
- `ctx.token.mint: String` — token mint address
- `ctx.token.is_established_protocol: bool` — for suppression gate (§6.3)
- `ctx.chain: Chain` — for Postgres query filtering
- `ctx.observed_at: DateTime<Utc>` — lookback window anchor; must come from block_time (gotcha #28)

No new fields are needed on `DetectorContext`. No new Postgres migrations are needed if the stateless evaluation decision (§11 Decision 6) is made.

### §6.3 Established-protocol suppression

**This is §11 Decision 7 — user must sign off.** The suppression flag `synchronized_activity_v1.suppress_established_protocols` controls whether D11 silences on tokens where `is_established_protocol = true`.

**Arguments for suppression (conservative):**
High-volume DEX pairs (SOL/USDC, SOL/USDT) will have many wallets executing buys simultaneously simply because market-making activity is inherently synchronized with price feeds. Suppressing avoids FP storms on established tokens.

**Arguments against suppression (aggressive):**
Sophisticated P&D operators deliberately target established tokens or launch tokens that initially appear established. Not suppressing catches dust-bot clusters and coordinated accumulation on any token type. D08 Sybil detection deliberately does not suppress on established protocols (design 0015 §6.2 + SESSION-KICKOFF gotcha #42).

See §11 Decision 7 for the full options and recommended default.

### §6.4 Evidence bundle

All evidence keys are prefixed by the detector ID per gotcha #9 (`synchronized_activity_v1/`):

```json
{
  "synchronized_activity_v1/cluster_size": 7,
  "synchronized_activity_v1/cluster_wallets": [
    "8aKx...abc1",
    "3rPq...xyz2",
    "... (up to 50 wallets, remainder count noted)"
  ],
  "synchronized_activity_v1/temporal_tightness": 0.82,
  "synchronized_activity_v1/temporal_spread_seconds": 5.4,
  "synchronized_activity_v1/poisson_p_value": 3.2e-8,
  "synchronized_activity_v1/lambda_token_per_second": 0.0028,
  "synchronized_activity_v1/window_start_block_time": "2026-04-24T10:00:00Z",
  "synchronized_activity_v1/window_end_block_time": "2026-04-24T10:00:30Z",
  "synchronized_activity_v1/delta_seconds": 30,
  "synchronized_activity_v1/action_source_kinds": ["swap_buy"],
  "synchronized_activity_v1/representative_tx_hashes": [
    "5xyz...aa01",
    "7abc...bb02",
    "... (one per wallet, up to 50)"
  ],
  "synchronized_activity_v1/total_cluster_volume_usd": "4250.00",
  "synchronized_activity_v1/mean_pairwise_jaccard": 0.84,
  "synchronized_activity_v1/secondary_cluster_count": 1,
  "synchronized_activity_v1/algorithm_version": "synchronized_activity_v1"
}
```

Note: `total_cluster_volume_usd` is `rust_decimal::Decimal` serialized as a string (CLAUDE.md no-f64 rule). The evidence includes a `secondary_cluster_count` field to note when multiple clusters were found but only the best was emitted as the top-level event.

### §6.5 Fixture shape

See §12 for full fixture JSON schema. Summary:
- `tests/fixtures/solana/positive/SYNTH_POS_D11_01_coordinated_buy_cluster.json` — synthetic 7-wallet synchronized buy cluster; 6-second temporal spread over 30-second window; p_value ≈ 1e-12
- `tests/fixtures/solana/negative/SYNTH_NEG_D11_01_organic_community_launch.json` — synthetic 8-wallet buy cluster with 45-second temporal spread (tightness < threshold) and λ calibrated so p_value > 1e-6
- `tests/fixtures/solana/negative/SYNTH_NEG_D11_02_market_maker_simultaneous_quotes.json` — 2 wallets (below N_min=5) buying simultaneously; DBSCAN produces singleton cluster, filtered at §5.1

### §6.6 Streaming scheduler integration

D11 integrates as a cadenced streaming detector in the same pattern as D08 (`docs/designs/0014-streaming-detector.md`):

```
Cadence: every cadence_seconds seconds per token
         (config: synchronized_activity_v1.cadence_seconds, default 120)
Trigger: streaming scheduler fires D11.evaluate(ctx) per tracked token
Hook class: None needed — D11 is not an IndexerHook; it reads from existing
            `swaps` and/or `transfers` tables populated by the existing indexer
```

D11 does NOT need a new `IndexerHook` implementation. The existing indexer already writes to `swaps` (populated by Raydium/Orca/Jupiter swap events) and `transfers` (SPL token transfers). D11 reads from those tables at evaluation time. This is the same read-only strategy used by D09's composite score computation.

---

## §7 Threshold Calibration

### §7.1 Window δ = 30 seconds

**Source:** research/sprint13-b-citations.md §"Suggested Signal/Threshold Formulation": "30 seconds (2 Solana slot-equivalents at ~400ms; chosen to be above single-slot MEV arb range but below human-reaction-time deliberate coordination)."

**Chain-specificity:** Solana slots are ~400ms. A 30-second window spans ~75 slots. This is large enough to encompass a coordinated wave of buy transactions submitted by a botnet in a single "fire" command. It is small enough to exclude wallets that happened to buy the same token in the same hour from organic interest. For EVM chains (Phase 4), a 30-second window spans 2–3 blocks on Ethereum (12s block time) — still appropriate for coordinated activity, possibly too short for Layer 2 chains with faster finality. The `delta_seconds` config key is chain-configurable in Phase 4.

**Alternative evaluated:** Arnold et al. (2024) temporal motif windows range from seconds to minutes. A longer window (e.g., 5 minutes) would capture slower coordinated accumulation but would also capture many more organic buyer pairs from popular tokens, increasing FP rate without a corresponding TP gain. A shorter window (e.g., 10 seconds) risks missing deliberately staggered botnets. 30 seconds is the recommended default.

### §7.2 Minimum cluster size N_min = 5

**Source:** Arnold et al. (2024) uses 3-node temporal motifs as the minimum; RTbust (Mazza et al. 2019) clusters with N >= 10 in their Twitter botnet corpus. The research recommendation (research/sprint13-b-citations.md) selects 5 as "a conservative midpoint for shitcoin context where genuine launch communities may have 3–4 simultaneous buyers."

**Base rate consideration:** Chainalysis (2025) reports 3.59% P&D base rate among all launched tokens. If we assume coordinated buys occur in 50% of P&D events and in 1% of legitimate launches, and N_min=5 eliminates 90% of organic 3–4 wallet coincidences: at N_min=5, the precision is approximately (0.5 × 0.0359) / (0.5 × 0.0359 + 0.1 × 0.9641) ≈ 16%. This is not high in absolute terms, but FP events at confidence < 0.60 are filtered by consumers. The p-value gate (§5.3) is the primary precision lever; N_min is a coarse pre-filter.

**Calibration target:** After Sprint 14 fixture corpus is built, re-run D11 with N_min ∈ {3, 4, 5, 6, 8} and measure TPR/FPR on positive/negative fixtures. Document the ROC curve in `research/sprint14-d11-calibration.md` (placeholder).

### §7.3 Poisson p-value threshold = 1e-6

**Source:** research/sprint13-b-citations.md §"Statistical Framework Recommendation": Poisson null derivation for k=5, λ=1/hour, δ=30s yields p ≈ 4×10^-10. The 1e-6 threshold is deliberately more lenient to accommodate tokens with λ up to ~10 actions/hour while still requiring extreme statistical improbability.

**Calibration:** The threshold is sensitive to `lambda_token`. For tokens with λ > 100 actions/hour (established high-volume pairs), even random activity produces clusters with p < 1e-6. This is why the established-protocol suppression (§6.3) or a token-tier-normalized lambda is necessary. The config key allows threshold adjustment after calibration.

### §7.4 Jaccard similarity threshold = 0.70

**Source:** Mannocci et al. (2024) survey of CIB detection methods recommends Jaccard-based temporal similarity with thresholds calibrated against platform-specific baselines. The RTbust (Mazza et al. 2019) equivalent parameter in DBSCAN epsilon space was calibrated against a 10M-event Twitter corpus. No published threshold exists for DEX buy synchronization specifically. 0.70 (meaning wallets must share 70% of their active time buckets) is proposed as the midpoint of the [0.5, 0.9] meaningful range. **This is an `unverified-heuristic` pending Sprint 14 calibration.**

**Config key:** `synchronized_activity_v1.jaccard_similarity_threshold`

### §7.5 Confidence weights

The weights w_size=0.40, w_tight=0.30, w_stat=0.30 are proposed defaults. RTbust (Mazza et al. 2019) ranks cluster size as the strongest predictor in their model, justifying the size weight primacy. The 30/30 split between tightness and significance reflects equal uncertainty about which sub-signal dominates in the on-chain domain. **Both weights are config-exposed for calibration.**

---

## §8 Evasion Analysis

### E-D11-1: Multi-window split (slowdown evasion)

**Attack:** Attacker splits the coordinated buy into multiple waves, each below the δ=30s window. Wave 1 at t=0, Wave 2 at t=45s, Wave 3 at t=90s. Each wave has fewer than N_min wallets.

**Effectiveness:** High. Each wave is individually below threshold. D11 fires only if a single wave crosses N_min within δ seconds.

**Mitigation:** Extend the evaluation to a second pass with a larger window δ_wide (e.g., 3 minutes), lower N_min_wide (e.g., 3 wallets per sub-window), and aggregate evidence across sub-windows. This is a Phase 5 enhancement. At MVP, this evasion is accepted as a known gap (DG-D11-1). Cross-correlation with D04 Signal A (volume spike) can provide partial compensation — a staggered coordinated buy still produces a volume spike detectable by D04.

**Residual risk:** Medium. Sophisticated attackers know to stagger; naive botnets do not.

### E-D11-2: Gradual ramp (slow accumulation)

**Attack:** Attacker uses a slow ramp: 1–2 bot wallets buy every 5 minutes over 2 hours, instead of a concentrated burst. No single 30-second window has more than N_min wallets.

**Effectiveness:** High. D11 with δ=30s cannot see gradual ramps. This is the same pattern that defeats D04's 1-hour window (see D04 evasion E-D04-9 in REFERENCES.md).

**Mitigation:** D09 BOCPD deployer changepoint may catch the deployer's behavioral shift if the deployer has history. D04 Signal A may catch the aggregate volume spike if it is large enough. D11 alone cannot catch slow ramps. Accept as known gap (DG-D11-2).

### E-D11-3: Cross-token spread

**Attack:** Attacker coordinates wallets to buy across multiple tokens simultaneously (one buy per wallet per token), so no single token sees N_min wallets in the window.

**Effectiveness:** High for D11 (token-scoped). Medium overall — if this is a pre-rug signal across multiple tokens, D09 (deployer behavioral shift) or D10 (launch audit) may catch the deployer.

**Mitigation:** Cross-token synchronized activity is a Phase 5 graph-layer signal. Not in D11 scope. Accept as known gap (DG-D11-3).

### E-D11-4: Noise injection (disguise coordination as coincidence)

**Attack:** Attacker has 5 coordinated wallets, each also executing random decoy buys in the same window on the same token from 20 additional unrelated wallets. The Jaccard similarity of the 5 coordinated wallets is diluted by the additional noise wallets, potentially breaking the DBSCAN cluster.

**Effectiveness:** Low-to-medium. The Jaccard metric is computed only between wallets that have actions in the window. If the 5 coordinated wallets all act in the same bucket, their pairwise Jaccard is 1.0 regardless of the noise wallets. The noise wallets have random bucket patterns and low Jaccard with each other and with the coordinated cluster — they are classified as DBSCAN noise points. The coordinated cluster remains intact.

**Residual risk:** Low if noise wallets have independent random timing; medium if the attacker engineers noise wallets to have moderate Jaccard with coordinated wallets (to dilute the cluster's DBSCAN core by absorbing core-wallet neighbors into a large mixed cluster). This is computationally expensive for the attacker and would itself be detectable as an abnormal number of simultaneous buyers.

### E-D11-5: Poisson baseline poisoning

**Attack:** Attacker makes the token's baseline action rate λ artificially high by running background buy wash traffic for 7+ days before the coordinated pump. With λ_token inflated, p_value for the real coordinated cluster exceeds the threshold.

**Effectiveness:** Medium. Requires sustained 7-day pre-operation. Inflating λ enough to suppress p_value below 1e-6 for a 5-wallet cluster requires λ_one > 0.50 per 30 seconds (one action every 30s on average). At 86,400 actions/day × 7 days = 604,800 wash buys — conspicuous and detected by D05 Signal A (per-wallet round-trips) before D11 fires.

**Mitigation:** If D05 Signal A fires during the baseline window, flag `lambda_contaminated = true` in the D11 evidence bundle and apply a lower effective lambda (conservative estimate). This is a Phase 3 cross-detector signal — not in D11 MVP scope but documented for scoring crate integration.

**Residual risk:** Medium for attackers willing to trade D05 exposure for D11 evasion.

### E-D11-6: Below-N_min precision timing

**Attack:** Attacker uses exactly N_min - 1 = 4 coordinated wallets. D11 never fires at any threshold.

**Effectiveness:** High for D11 specifically. Effectiveness overall: medium. The pump signal from 4 coordinated wallets is still detectable by D04 volume spike analysis if the 4 wallets trade significant volume. D11's N_min is a coarse filter; lowering N_min is the mitigation at the cost of FP rate increase.

**Mitigation:** Phase 5 scoring crate can combine D04 confidence with D11 near-miss evidence (e.g., a cluster of 4 wallets with tightness=0.95 and p_value=5e-6 is still useful signal even if D11 doesn't fire). D11's evidence bundle should include near-miss clusters (cluster size = N_min - 1) at TRACE log level for post-hoc analysis.

### E-D11-7: CEX withdrawal timing correlation (false positive source)

**Attack/false positive scenario:** Multiple retail buyers withdraw funds from the same CEX simultaneously (e.g., triggered by a public tweet) and immediately buy a token. This produces a synchronized burst of N >> N_min buyers within δ seconds — D11 fires on genuine organic demand.

**Effectiveness as false positive:** Medium-high for trending tokens. Solana has several DEX aggregators (Jupiter) that execute buys for many users simultaneously when a price alert fires. These produce genuine temporal clusters that are indistinguishable from coordinated botnets at the D11 signal level.

**Mitigation:** The `is_established_protocol` suppression (§6.3) partially addresses this for known high-volume tokens. For new tokens, there is no reliable way to distinguish CEX-triggered organic demand from botnet coordination at D11's observation level. The confidence cap at 0.90 and the requirement for human review in the evidence bundle are the primary mitigations. The D11 confidence of a legitimate CEX-triggered burst should typically be lower (temporal tightness near 1.0 but p_value only marginally below threshold if λ is high) than a coordinated botnet (tightness near 1.0, p_value << threshold). This is an empirical hypothesis requiring calibration.

---

## §9 Config Keys

All keys live under `[synchronized_activity_v1]` in `config/detectors.toml`. Every key must have a comment citing its derivation source.

```toml
[synchronized_activity_v1]

# Window width in seconds for time-bucket formation (Step 2 of §3.1).
# Cited: research/sprint13-b-citations.md §"Suggested Signal/Threshold Formulation":
# "30 seconds (2 Solana slot-equivalents at ~400ms; above single-slot MEV arb range;
#  below human-reaction-time coordination)."
# For Solana only; EVM chains may override this at Phase 4.
window_seconds = 30

# Minimum number of distinct wallets required in a DBSCAN cluster to fire.
# Cited: Arnold et al. 2024 (3-node motif minimum); RTbust 2019 (N>=10 in Twitter corpus);
# research/sprint13-b-citations.md midpoint recommendation = 5.
min_cluster_size = 5

# Poisson null model p-value threshold. Clusters with p_value > this are discarded.
# Cited: research/sprint13-b-citations.md §"Statistical Framework Recommendation":
# k=5, λ=1/hour, δ=30s → p ≈ 4e-10; 1e-6 is deliberately lenient for higher-λ tokens.
poisson_p_threshold = 1e-6

# Minimum temporal tightness score [0.0, 1.0] for a cluster to pass the filter.
# Cited: unverified-heuristic; midpoint of [0, 1.0]; calibrate against Sprint 14 fixture corpus.
# TODO: derive from calibration once POS/NEG fixture corpus reaches 20+ examples.
temporal_tightness_threshold = 0.50

# Jaccard similarity threshold for DBSCAN neighborhood (wallets with J >= this are neighbors).
# Cited: unverified-heuristic; midpoint of [0.5, 0.9] meaningful range per Mannocci et al. 2024.
# TODO: calibrate against Sprint 14 fixture corpus.
jaccard_similarity_threshold = 0.70

# Action types to include as events for D11 evaluation.
# Options: "swap_buy", "swap_sell", "transfer", "lp_add", "lp_remove"
# Decision §11-1 controls which types are active at MVP.
# Default: swap_buy only (highest signal-to-noise; lowest recall for LP events).
action_source_kinds = ["swap_buy"]

# Maximum lookback window in minutes for event fetching.
# Events older than this are not considered for cluster formation.
# Cited: research/sprint13-b-citations.md: 7-day rolling baseline window for λ;
# the lookback window is shorter (for cluster detection, not baseline).
max_lookback_minutes = 10

# Maximum number of raw events to fetch per evaluation (safety ceiling).
# Analogous to D05 Signal B max_transfers_per_window = 10,000.
# At max_lookback_minutes=10, tokens with >10,000 buys in 10 minutes are large-cap
# tokens better handled by established-protocol suppression.
max_events_per_window = 10000

# Maximum number of distinct wallets to include in DBSCAN (beyond this, the token
# has too many buyers for meaningful cluster detection and is likely large-cap).
# DBSCAN pairwise distance is O(n^2); 500 wallets = 250,000 comparisons per evaluation.
max_wallets_per_cluster_cap = 500

# Minimum number of historical events in the 7-day window required before the
# Poisson baseline is considered reliable (warmup guard, §5.4).
# Analogous to D04 min_baseline_days = 3 guard.
min_baseline_events = 10

# Cadence in seconds between D11 evaluations per tracked token.
# Analogous to D08 sybil_cluster_cadence_seconds.
cadence_seconds = 120

# Whether to suppress D11 events for established-protocol tokens.
# Decision §11-7. Default: false (aggressive; consistent with D08 non-suppression).
# Set to true to reduce FP rate on high-volume DEX pairs.
suppress_established_protocols = false

# Confidence formula weights (must sum to a positive number; relative weighting applies).
# Cited: RTbust 2019 (cluster size strongest predictor → highest weight).
weight_cluster_size = 0.40
weight_temporal_tightness = 0.30
weight_statistical_significance = 0.30

# Sigmoid stretch factor for cluster size sub-signal (S_size, §4.1).
# At cluster_size = min_cluster_size + cluster_size_scale, S_size ≈ 0.73.
cluster_size_scale = 5.0
```

---

## §10 Cross-Detector Coverage Matrix

This matrix shows which behavioral patterns each detector can and cannot catch, to justify D11's distinct role and identify gaps requiring multi-detector scoring.

| Coordinated Pattern | D05 Signal A | D05 Signal B (cycles) | D08 Sybil | D09 BOCPD | D11 Synchronized |
|---------------------|--------------|-----------------------|-----------|-----------|------------------|
| Per-wallet round-trip self-dealing | **Yes** (primary) | Partial (2-hop degenerate) | No | No | No (2 wallets < N_min) |
| Multi-wallet token circulation ring (wash ring) | No (different wallets) | **Yes** (primary) | No (unless funded together) | No | Partial (if ring wallets buy simultaneously first) |
| Common-funder sybil cluster holding same token | No | No | **Yes** (primary) | No | Partial (if cluster buys simultaneously) |
| Deployer behavioral shift (acceleration of launches) | No | No | No | **Yes** (primary) | No |
| Simultaneous buy burst by N distinct wallets | No | No | No | No | **Yes** (primary) |
| Pre-pump accumulation phase (slow, staggered) | No | No | Partial (if common funder) | Partial (if deployer change) | **No** (E-D11-2 gap) |
| Airdrop-farming botnet synchronized interactions | No | No | Partial (common funder) | No | **Yes** (if N_min wallets in window) |
| LP drain (rug pull) | No | No | No | **Yes** (prior_rug_rate feature) | No |
| Mint authority abuse | No | No | No | No | No (D06 primary) |
| CEX-triggered organic burst (false positive) | No | No | No | No | **FP risk** (E-D11-7) |

**Conclusion:** D11 fills a distinct gap for simultaneous multi-wallet coordination not covered by any existing detector. The closest overlap is with D08 (which may catch the same cluster if those wallets share a common funder) and D05 Signal B (which may catch the same wallets if they also form a transfer ring). Triple co-firing of D11 + D08 + D05 on the same token at the same time is a strong composite signal for the scoring crate.

---

## §11 Decisions Requiring Sign-Off

**Each decision below must be resolved before implementation begins (S14-2). Implementation proceeds only after user sign-off on all 8 decisions. The recommended default is marked; fallback options are listed if the recommended default is not accepted.**

---

### Decision 1: Action source kinds (recall vs. precision tradeoff)

**Question:** Which on-chain event types should D11 consider as "actions" for temporal clustering?

**Option A: Swap buy events only** (recommended default)
- Inputs: `swaps` table, `is_buy = true` filter
- Pros: highest signal-to-noise; buy swaps are the clearest signal for coordinated pump starts; lowest FP rate from legitimate LP activity
- Cons: misses coordinated LP deposits (which can front-run a liquidity event); misses coordinated token transfers (airdrop farming)
- Recall: catches coordinated pump-starts, pre-P&D accumulation; misses LP-coordination and transfer-farming

**Option B: Swaps (buy + sell) + token transfers**
- Inputs: `swaps` table (all sides) + `transfers` table
- Pros: catches coordinated sell-offs (distribution phase), airdrop farming, transfer-based coordination
- Cons: sell-side synchronization is common for legitimate stop-loss triggers (price drop → many simultaneous sells); transfer events include internal dex routing hops that inflate wallet count artificially; significantly higher FP rate
- Recall: higher; precision: lower

**Option C: Swaps + transfers + LP add/remove events**
- Inputs: `swaps` + `transfers` + `pool_events` (PoolEvent::Deposit and PoolEvent::Withdraw)
- Pros: catches coordinated LP drain (N wallets simultaneously removing LP — overlaps with D02 but provides a clustering signal D02 lacks)
- Cons: highest FP rate; legitimate AMM rebalancing fires frequently; requires joining `pool_events` in the bucket computation; implementation complexity increases significantly
- Recall: highest; precision: lowest

**Recommended default: Option A.** The MVP detector should maximize precision first. The research recommendation (research/sprint13-b-citations.md) explicitly proposes "buy swap events only" as the MVP input. Options B and C can be enabled via the `action_source_kinds` config key in Phase 5 without algorithm changes. The config array already accepts `"swap_sell"`, `"transfer"`, `"lp_add"`, `"lp_remove"` as valid values (§9).

**Fallback:** Option B with `action_source_kinds = ["swap_buy", "swap_sell"]` if the user wants to catch distribution-phase coordination in the MVP.

**User must sign off before implementation.**

---

### Decision 2: Window δ — fixed 30s vs. configurable vs. per-token-calibrated

**Question:** Should the time-bucket window δ be a fixed 30-second constant, a configurable global constant, or per-token-calibrated?

**Option A: Configurable global constant (default 30s)** (recommended)
- `window_seconds = 30` in config; same for all tokens on Solana
- Pros: simple; deterministic; consistent with the research recommendation; calibration is single-number tuning
- Cons: a 30-second window may be too short for slower coordinated strategies; tokens with higher organic activity may require a narrower window for the same p-value guarantee
- Implementation: trivial; already specified in §9

**Option B: Per-token-calibrated (window derived from token's inter-buy interval distribution)**
- δ = p25 of the token's inter-buy interval over the past 7 days, bounded to [5s, 120s]
- Pros: adapts to each token's trading pace; avoids false positives on high-frequency tokens; reduces false negatives on slow tokens
- Cons: requires computing a distribution statistic per token per evaluation; introduces a data dependency that can be manipulated (E-D11-5 variant); more complex; harder to explain/audit
- Implementation: additional SQL query for inter-buy interval distribution

**Option C: Multiple windows evaluated simultaneously (30s, 60s, 120s)**
- Run DBSCAN at three window sizes; take the maximum confidence
- Pros: catches coordinated activity at multiple time scales; reduces E-D11-1 gap
- Cons: 3× computational cost; risk of inflating confidence by always taking the maximum; harder to calibrate thresholds simultaneously
- Implementation: loop over window sizes in the evaluate function

**Recommended default: Option A (configurable global constant, 30 seconds).** The research recommendation is 30 seconds. Per-token calibration (Option B) adds complexity and attack surface that is not warranted at MVP. Multi-window (Option C) is the correct Phase 5 enhancement for E-D11-1 gap closure.

**Fallback:** Option A with a user-specified value other than 30 seconds (e.g., 60 seconds for more conservative detection).

**User must sign off before implementation.**

---

### Decision 3: Clustering algorithm — Jaccard+DBSCAN vs. temporal motifs vs. ensemble

**Question:** Which clustering algorithm should D11 use for identifying synchronized wallet groups?

**Option A: Jaccard-over-time-buckets + DBSCAN** (recommended)
- Described in §3.1; implemented in §3.3
- Pros: directly recommended by Mannocci et al. (2024) survey; well-understood; parameter-interpretable (eps = 1 - jaccard_threshold); deterministic; no new dependencies; hand-rollable in ~100 lines of Rust
- Cons: O(n^2) pairwise distance computation; does not explicitly model the temporal sequence (only bucket overlap); requires tuning of Jaccard threshold and DBSCAN epsilon
- Complexity: O(n^2 * T) where n = distinct wallets, T = number of buckets

**Option B: Pairwise temporal motif counting (Arnold 2024 approach)**
- For each pair of wallets, count 3-node temporal motifs (buyer_A → token ← buyer_B within δ seconds); wallets with motif count > motif_threshold form the cluster
- Pros: directly grounded in Arnold et al. (2024) on-chain evidence; conceptually simpler; fewer parameters (just the threshold)
- Cons: 3-node motifs miss larger coordination clusters (N=7 is not captured as a 7-node motif in the Arnold formalism without extension); requires defining "motif count threshold" without guidance from the paper (which is descriptive, not prescriptive)
- Implementation: ~80 lines; simpler than DBSCAN

**Option C: Both algorithms ensembled (highest-confidence output)**
- Run both A and B; emit the event with the higher confidence; include both confidence scores in the evidence bundle
- Pros: higher recall (each catches some cases the other misses); provides two independent evidence streams for human review
- Cons: 2× computational cost; complex to calibrate when the two outputs disagree; ensemble hyperparameter (how to combine) adds opacity

**Recommended default: Option A (Jaccard + DBSCAN).** The Mannocci et al. (2024) survey is the primary methodological citation and explicitly recommends this approach. Arnold et al. (2024) establishes that the on-chain signal exists but does not prescribe a classifier. The DBSCAN approach is more general (captures clusters of any size) and better-cited. Option B can be added as a secondary signal in Phase 5.

**Fallback:** Option B if the user prefers a simpler initial implementation. Option C if the user wants comprehensive coverage from day one at the cost of complexity.

**User must sign off before implementation.**

---

### Decision 4: Null model for p-value — closed-form Poisson vs. empirical rolling baseline vs. both

**Question:** How should the statistical significance of a cluster be assessed against the null hypothesis of independent random buying?

**Option A: Closed-form Poisson null model** (recommended)
- `p_value = (1 - exp(-lambda_token * delta_seconds)) ^ cluster_size`
- `lambda_token` estimated from 7-day rolling event count / window duration
- Pros: analytically grounded; Mannocci et al. (2024) explicitly recommends this framing; no additional storage or computation beyond a count query; fully transparent derivation
- Cons: Poisson assumes independence of buying events (not always valid — price momentum induces buying clustering even without coordination); requires 7-day warmup; lambda estimate is a single scalar, not a distribution

**Option B: Empirical per-token rolling baseline (permutation test)**
- Estimate the empirical distribution of "maximum cluster size in a random 30-second window" by drawing 1,000 permutations of the event timestamps within the lookback window
- p-value = fraction of permutations producing a cluster as large as the observed cluster
- Pros: makes no distributional assumptions; robust to non-Poisson buying patterns; automatically accounts for price-momentum clustering
- Cons: 1,000 permutations × pairwise DBSCAN each = prohibitive at O(1000 * n^2) per evaluation; requires storing no additional state but is computationally expensive at evaluation time; results depend on the random seed (non-deterministic unless seed is fixed and documented)

**Option C: Both — Poisson p-value as primary, empirical calibration as quarterly offline check**
- Hot path: Option A (Poisson)
- Offline calibration: run permutation test on historical data to verify that Poisson thresholds are calibrated correctly for each token tier
- Pros: best of both — production path is fast and deterministic; calibration path catches Poisson assumption violations
- Cons: operational complexity; calibration job must be scheduled separately

**Recommended default: Option A (closed-form Poisson) with the understanding that the 7-day warmup guard (§5.4, `min_baseline_events`) protects against unreliable lambda estimates.** The Poisson independence assumption is imperfect but sufficient for MVP: if price-momentum clustering creates false positives, the temporal_tightness and cluster_size thresholds provide compensating filters. Option C is the natural Phase 5 calibration enhancement.

**Fallback:** Option A with a more conservative `poisson_p_threshold` (e.g., 1e-8 instead of 1e-6) if the user is concerned about Poisson assumption violations in production.

**User must sign off before implementation.**

---

### Decision 5: Minimum cluster size N_min — 3 vs. 5 vs. token-size-adaptive

**Question:** What should the minimum number of distinct wallets in a synchronized cluster be to fire D11?

**Option A: N_min = 3 (Arnold 2024 minimum)**
- Minimum motivated by Arnold et al. (2024) temporal motif research (3-node motifs as the minimum meaningful structure)
- Pros: highest recall; catches small coordinated groups (e.g., a 3-person insider group buying simultaneously)
- Cons: highest FP rate; organic token launches frequently have 3 wallets buying in the same 30-second window from a single Telegram announcement; the base rate of "3 wallets buy simultaneously" is too high for this to be meaningful without a very tight p-value

**Option B: N_min = 5 (research recommendation midpoint)** (recommended)
- Motivated by research/sprint13-b-citations.md midpoint between Arnold (3) and RTbust (10+)
- Pros: conservatively above the noise floor of 3–4 simultaneous organic buyers; compatible with the Poisson p-value derivation in §7.2; matches the research recommendation explicitly
- Cons: misses small coordinated groups (E-D11-6 evasion); may be too conservative for early-phase detection

**Option C: Token-size-adaptive (N_min = max(3, floor(unique_holders / 100)))**
- For tokens with very few holders (< 300), N_min remains 3
- For tokens with 1,000 holders, N_min = 10; for 10,000 holders, N_min = 100
- Pros: adapts to token maturity; prevents FP on large tokens with high organic buyer counts; increases precision on large-cap tokens
- Cons: adds complexity to threshold derivation; requires knowing current holder count at evaluation time (additional query or DetectorContext field); the formula is an unverified heuristic with no published precedent

**Recommended default: Option B (N_min = 5) with the config key exposed for easy adjustment.** The research recommendation is explicit. Option A (N_min = 3) is available by changing one config value after calibration confirms FP rate is acceptable. Option C is a Phase 5 enhancement.

**Fallback:** Option A if the user wants maximum recall at MVP.

**User must sign off before implementation.**

---

### Decision 6: Storage — stateless recompute vs. V00014 materialized cluster snapshots

**Question:** Should D11 store detected cluster snapshots in Postgres for auditability and temporal tracking, or operate statelessly by recomputing from `swaps`/`transfers` on every evaluation?

**Option A: Stateless (recompute per evaluation)** (recommended)
- No new migration. D11 reads from `swaps` and/or `transfers` tables at evaluation time and computes clusters in-memory. AnomalyEvent is emitted and stored in the existing `anomaly_events` table as all other detectors.
- Pros: simplest implementation; no new schema; no state corruption risk; compatible with D05 Signal B's Option D (transient in-memory from `transfers`); consistent with D11's cadenced-detector model
- Cons: cluster membership is not persistent — if you want to see "which wallets were in the cluster that fired 3 hours ago," you must re-run D11 over the same window, which requires the `swaps`/`transfers` data still be within retention period; no ability to track cluster membership evolution over time

**Option B: V00014 state table (materialized cluster snapshots)**
- New migration V00014 (next available per SESSION-KICKOFF gotcha #31)
- Schema: `synchronized_activity_clusters (cluster_id UUID PK, chain TEXT, token TEXT, first_seen TIMESTAMPTZ, last_seen TIMESTAMPTZ, wallet_count INT, wallet_addresses JSONB, confidence DOUBLE PRECISION, temporal_tightness DOUBLE PRECISION, poisson_p_value DOUBLE PRECISION, delta_seconds INT, updated_at TIMESTAMPTZ)`
- Pros: full auditability of cluster history; enables tracking whether the same cluster reappears across multiple evaluation windows (escalating confidence); enables Phase 5 cross-cluster wallet identity resolution
- Cons: new table, migration, and upsert logic; increases implementation scope by ~30%; retention policy needed (90-day TTL recommended); worst-case JSONB `wallet_addresses` column can be large for 500-wallet clusters (cap at 100 wallet strings)

**Recommended default: Option A (stateless).** The AnomalyEvent evidence bundle already captures all cluster metadata at the time of firing (§6.4). Re-running D11 over the same historical window is possible as long as the `swaps`/`transfers` data is within retention. The clustering state needed for Phase 5 cross-window tracking is better implemented as a Phase 5 enhancement (V00014 at that point) once the need is validated. Keeping D11 stateless at MVP mirrors D05 Signal B's Option D choice.

**Fallback:** Option B if the user needs cluster membership history for external audit or wants to track recurring clusters across evaluation windows from day one.

**User must sign off before implementation. If Option B selected, V00014 migration is required and implementation scope increases by approximately 150 LOC.**

---

### Decision 7: Established-protocol suppression — suppress vs. not suppress

**Question:** Should D11 suppress events for tokens where `is_established_protocol = true`?

**Option A: Do NOT suppress on established protocols** (recommended)
- Consistent with D08 Sybil behavior (design 0015 §6.2; SESSION-KICKOFF gotcha #42)
- Pros: catches coordinated dust-bot activity on any token; catches cross-chain coordination targeting established token pairs; consistent internal policy
- Cons: high-volume established DEX pairs (SOL/USDC, SOL/USDT) will have many simultaneous buyers from MEV bots and market makers — these produce temporal clusters indistinguishable from coordinated pumps at the D11 level; FP rate elevated on established protocols

**Option B: Suppress on established protocols** (conservative)
- Consistent with D10 launch audit behavior (D10 IS suppressed on established protocols)
- Pros: eliminates FP storm on large-cap token pairs; reduces operational noise for consumers
- Cons: misses coordinated activity on established tokens; creates a known exploitable gap (attacker can camouflage token as established)

**Option C: Suppress AND log suppression at DEBUG level with evidence**
- Same as Option B but emits a low-confidence (< 0.20) informational event for suppressed clusters
- Pros: no data loss; consumer can inspect suppressed events at low threshold; consistent with the "false negatives are expensive" principle in CLAUDE.md
- Cons: additional implementation complexity; informational events may clutter the anomaly log

**Recommended default: Option A (do not suppress).** The rationale mirrors D08 Sybil (gotcha #42): established tokens can be Sybil-targeted and coordinated-buy-targeted. The FP risk on large-cap pairs is mitigated by the `max_wallets_per_cluster_cap` ceiling (§9, default 500) which causes D11 to short-circuit evaluation on tokens with too many buyers (these are almost certainly large-cap legitimate tokens). The `poisson_p_threshold` also provides protection: for high-λ established tokens, the p-value gate suppresses clusters that are statistically plausible under organic traffic.

**Fallback:** Option B with `suppress_established_protocols = true` in config if the user experiences FP storm on established pairs in production testing.

**User must sign off before implementation.**

---

### Decision 8: Graph integration — read-only from tables vs. new edge type vs. cluster label

**Question:** How should D11 integrate with the `crates/graph` layer? Should it write anything to the graph store?

**Option A: Read-only from `swaps`/`transfers` tables (no graph writes)** (recommended)
- D11 reads from existing event tables. No writes to `graph_edges`, `wallet_clusters`, `address_labels`, or `wallet_cluster_members`.
- Pros: consistent with D05 Signal B Option D (transient in-memory from `transfers`); no write-path complexity; no graph-edge accumulation; no new migration; simplest option
- Cons: cluster membership is not available for graph-layer queries (e.g., future Phase 5 detector that wants to know "has this wallet appeared in a synchronized cluster before?"); D08 Sybil cannot consume D11 cluster output directly

**Option B: Write `SynchronizedActivity` edges to `graph_edges`**
- When D11 detects a cluster, write one `graph_edges` row per wallet pair in the cluster with `edge_type = 'SynchronizedActivity'`, `confidence` from the cluster, and `block_height` of the cluster peak action
- Pros: enables future graph queries ("is this wallet in any synchronized cluster on any token?"); enables D08 to use D11 cluster membership as a feature
- Cons: dense writes for large clusters (N=50 wallets → 1,225 edge pairs per firing); introduces D11 as a writer to `graph_edges` which is currently written only by the indexer (separation of concerns violation); `graph_edges` PRIMARY KEY `(chain, from_address, to_address, edge_type, token, block_height)` would produce many rows per cluster

**Option C: Write `ClusterLabel` to `address_labels` via `GraphLabelStore`**
- When D11 detects a cluster, write one `address_labels` row per wallet with `label_type = 'SynchronizedBuyer'`, `confidence` from the cluster, and TTL = 168 hours (7-day default for cluster labels)
- Pros: consistent with how D08 writes `FundingSource` labels (design 0015 §3.2.2); lightweight (N rows, not N^2); enables downstream detectors to query "is this wallet labelled SynchronizedBuyer?"; clean separation (D11 uses GraphLabelStore API, not direct SQL)
- Cons: requires `GraphLabelStore` access in D11 (adds dependency: `crates/detectors` must import `crates/graph`'s label API — already in the dep chain per SESSION-KICKOFF gotcha #33); TTL-based labels expire and may miss recurring clusters

**Recommended default: Option A (read-only, no graph writes).** This mirrors D05 Signal B Option D and keeps D11 purely reactive. Graph label writes (Option C) are a clean Phase 3 enhancement once the value of the `SynchronizedBuyer` label for downstream detectors is confirmed. Dep chain is already correct (`detectors → graph`), so Option C is a low-friction upgrade. Option B (edge writes) introduces N^2 write density that is disproportionate to the value.

**Fallback:** Option C if the user wants D11 cluster membership available for graph queries from day one. Option C adds approximately 80 LOC and requires `GraphLabelStore` to be passed as a constructor argument to `D11SynchronizedActivityDetector`.

**User must sign off before implementation.**

---

## §12 Fixture Shape Specification

### §12.1 Positive fixture — coordinated buy cluster

**File:** `tests/fixtures/solana/positive/SYNTH_POS_D11_01_coordinated_buy_cluster.json`

**Scenario:** 7 wallets execute buy swaps on the same token within a 6-second window in a 30-second evaluation slot. The token has a 7-day rolling buy rate of λ = 1 action per minute (0.0167/s). Poisson p_value = (1 - exp(-0.0167 * 30))^7 = (0.394)^7 ≈ 1.5×10^-3 — with N_min=5 and δ=30s, this is above the p_threshold of 1e-6 at the naive rate.

Adjust for fixture: use a lower λ = 0.001/s (1 action per 17 minutes) to make p_joint more extreme. At λ=0.001, p_one = 1 - exp(-0.001*30) ≈ 0.030; p_joint for k=7: (0.030)^7 ≈ 2.2×10^-11. Expected confidence: S_size ≈ sigmoid((7-5)/5) ≈ 0.55; S_tight ≈ 0.80 (6s spread / 30s window = tightness 0.80); S_stat ≈ 1.0 - (2.2e-11/1e-6) ≈ 1.0. conf_raw ≈ (0.4*0.55 + 0.3*0.80 + 0.3*1.0) / 1.0 ≈ 0.76. Expected severity: High.

```json
{
  "fixture_id": "SYNTH_POS_D11_01",
  "description": "7 coordinated wallets buying the same token within 6s; p_value ≈ 2.2e-11",
  "chain": "Solana",
  "token_mint": "SYNTH_TOKEN_D11_POS_01",
  "is_established_protocol": false,
  "observed_at_block_time": "2026-04-24T10:00:30Z",
  "lambda_token_per_second": 0.001,
  "baseline_events_7d": 605,
  "swaps": [
    {
      "wallet": "WALLET_D11_P01_A",
      "tx_hash": "TX_D11_P01_A001",
      "block_time": "2026-04-24T10:00:04Z",
      "block_height": 100000,
      "is_buy": true,
      "amount_raw": "1000000000",
      "pool": "POOL_D11_POS_01"
    },
    {
      "wallet": "WALLET_D11_P01_B",
      "tx_hash": "TX_D11_P01_B001",
      "block_time": "2026-04-24T10:00:05Z",
      "block_height": 100002,
      "is_buy": true,
      "amount_raw": "2000000000",
      "pool": "POOL_D11_POS_01"
    },
    {
      "wallet": "WALLET_D11_P01_C",
      "tx_hash": "TX_D11_P01_C001",
      "block_time": "2026-04-24T10:00:05Z",
      "block_height": 100002,
      "is_buy": true,
      "amount_raw": "1500000000",
      "pool": "POOL_D11_POS_01"
    },
    {
      "wallet": "WALLET_D11_P01_D",
      "tx_hash": "TX_D11_P01_D001",
      "block_time": "2026-04-24T10:00:06Z",
      "block_height": 100004,
      "is_buy": true,
      "amount_raw": "3000000000",
      "pool": "POOL_D11_POS_01"
    },
    {
      "wallet": "WALLET_D11_P01_E",
      "tx_hash": "TX_D11_P01_E001",
      "block_time": "2026-04-24T10:00:07Z",
      "block_height": 100006,
      "is_buy": true,
      "amount_raw": "800000000",
      "pool": "POOL_D11_POS_01"
    },
    {
      "wallet": "WALLET_D11_P01_F",
      "tx_hash": "TX_D11_P01_F001",
      "block_time": "2026-04-24T10:00:09Z",
      "block_height": 100010,
      "is_buy": true,
      "amount_raw": "1200000000",
      "pool": "POOL_D11_POS_01"
    },
    {
      "wallet": "WALLET_D11_P01_G",
      "tx_hash": "TX_D11_P01_G001",
      "block_time": "2026-04-24T10:00:10Z",
      "block_height": 100012,
      "is_buy": true,
      "amount_raw": "900000000",
      "pool": "POOL_D11_POS_01"
    }
  ],
  "expected_output": {
    "detector_id": "synchronized_activity_v1",
    "cluster_size": 7,
    "temporal_tightness_min": 0.75,
    "p_value_max": 1e-9,
    "confidence_min": 0.70,
    "severity": "High",
    "fires": true
  }
}
```

### §12.2 Negative fixture 1 — organic community launch (spread too wide)

**File:** `tests/fixtures/solana/negative/SYNTH_NEG_D11_01_organic_community_launch.json`

**Scenario:** 8 wallets buy the same token within a 45-second window after a Telegram announcement. Temporal tightness = 1.0 - 45/30 → clamped to 0.0 (spread exceeds the window). Alternatively, with the lookback window of 10 minutes, the first buys are spread across 45 seconds — temporal_spread = 45s, temporal_tightness = 1.0 - (45/30) → clamped to 0.0, which is below the temporal_tightness_threshold of 0.50. D11 must NOT fire.

```json
{
  "fixture_id": "SYNTH_NEG_D11_01",
  "description": "8 wallets buying across 45s window; tightness below threshold; organic launch",
  "chain": "Solana",
  "token_mint": "SYNTH_TOKEN_D11_NEG_01",
  "is_established_protocol": false,
  "observed_at_block_time": "2026-04-24T11:00:00Z",
  "lambda_token_per_second": 0.002,
  "baseline_events_7d": 1200,
  "swaps": [
    {
      "wallet": "WALLET_D11_N01_A",
      "tx_hash": "TX_D11_N01_A001",
      "block_time": "2026-04-24T10:59:15Z",
      "block_height": 110000,
      "is_buy": true,
      "amount_raw": "500000000",
      "pool": "POOL_D11_NEG_01"
    },
    {
      "wallet": "WALLET_D11_N01_B",
      "tx_hash": "TX_D11_N01_B001",
      "block_time": "2026-04-24T10:59:23Z",
      "block_height": 110020,
      "is_buy": true,
      "amount_raw": "700000000",
      "pool": "POOL_D11_NEG_01"
    },
    {
      "wallet": "WALLET_D11_N01_C",
      "tx_hash": "TX_D11_N01_C001",
      "block_time": "2026-04-24T10:59:31Z",
      "block_height": 110040,
      "is_buy": true,
      "amount_raw": "300000000",
      "pool": "POOL_D11_NEG_01"
    },
    {
      "wallet": "WALLET_D11_N01_D",
      "tx_hash": "TX_D11_N01_D001",
      "block_time": "2026-04-24T10:59:38Z",
      "block_height": 110056,
      "is_buy": true,
      "amount_raw": "1000000000",
      "pool": "POOL_D11_NEG_01"
    },
    {
      "wallet": "WALLET_D11_N01_E",
      "tx_hash": "TX_D11_N01_E001",
      "block_time": "2026-04-24T10:59:44Z",
      "block_height": 110068,
      "is_buy": true,
      "amount_raw": "600000000",
      "pool": "POOL_D11_NEG_01"
    },
    {
      "wallet": "WALLET_D11_N01_F",
      "tx_hash": "TX_D11_N01_F001",
      "block_time": "2026-04-24T10:59:51Z",
      "block_height": 110082,
      "is_buy": true,
      "amount_raw": "400000000",
      "pool": "POOL_D11_NEG_01"
    },
    {
      "wallet": "WALLET_D11_N01_G",
      "tx_hash": "TX_D11_N01_G001",
      "block_time": "2026-04-24T10:59:55Z",
      "block_height": 110090,
      "is_buy": true,
      "amount_raw": "800000000",
      "pool": "POOL_D11_NEG_01"
    },
    {
      "wallet": "WALLET_D11_N01_H",
      "tx_hash": "TX_D11_N01_H001",
      "block_time": "2026-04-24T11:00:00Z",
      "block_height": 111000,
      "is_buy": true,
      "amount_raw": "900000000",
      "pool": "POOL_D11_NEG_01"
    }
  ],
  "expected_output": {
    "detector_id": "synchronized_activity_v1",
    "fires": false,
    "reason": "temporal_tightness below threshold (spread 45s > window 30s)"
  }
}
```

### §12.3 Negative fixture 2 — market maker pair (below N_min)

**File:** `tests/fixtures/solana/negative/SYNTH_NEG_D11_02_market_maker_simultaneous_quotes.json`

**Scenario:** 2 market maker wallets simultaneously buy the same token in the same block as part of a legitimate quoting strategy. Cluster size = 2 < N_min = 5. DBSCAN produces no cluster meeting the minimum-samples threshold. D11 must NOT fire.

```json
{
  "fixture_id": "SYNTH_NEG_D11_02",
  "description": "2 market maker wallets buying simultaneously; below N_min threshold",
  "chain": "Solana",
  "token_mint": "SYNTH_TOKEN_D11_NEG_02",
  "is_established_protocol": false,
  "observed_at_block_time": "2026-04-24T12:00:00Z",
  "lambda_token_per_second": 0.05,
  "baseline_events_7d": 30000,
  "swaps": [
    {
      "wallet": "WALLET_D11_N02_MM_A",
      "tx_hash": "TX_D11_N02_MM_A001",
      "block_time": "2026-04-24T11:59:58Z",
      "block_height": 120000,
      "is_buy": true,
      "amount_raw": "50000000000",
      "pool": "POOL_D11_NEG_02"
    },
    {
      "wallet": "WALLET_D11_N02_MM_B",
      "tx_hash": "TX_D11_N02_MM_B001",
      "block_time": "2026-04-24T11:59:58Z",
      "block_height": 120000,
      "is_buy": true,
      "amount_raw": "48000000000",
      "pool": "POOL_D11_NEG_02"
    }
  ],
  "expected_output": {
    "detector_id": "synchronized_activity_v1",
    "fires": false,
    "reason": "cluster_size 2 < min_cluster_size 5"
  }
}
```

### §12.4 Calibration targets

After Sprint 14 implementation, run D11 against the full positive/negative fixture corpus with the recommended default config:

| Metric | Target | Acceptance criterion |
|--------|--------|---------------------|
| TPR at confidence >= 0.60 | >= 0.70 | Positive fixtures: at least 7 of 10 must fire |
| FPR at confidence >= 0.60 | <= 0.10 | Negative fixtures: at most 1 of 10 must fire |
| Mean confidence on positives | >= 0.65 | Avoid systematic under-confidence on genuine clusters |
| Mean confidence on negatives | <= 0.30 | Avoid systematic over-confidence on noise |

These targets are consistent with the RTbust (Mazza et al. 2019) F1 = 0.87 anchor, discounted for the more challenging on-chain domain and smaller fixture corpus.

---

## §13 Design Gaps (Open Items)

These gaps are documented for future sprints and do not block Sprint 14 implementation.

**DG-D11-1: Multi-window slow ramp detection.** E-D11-1 and E-D11-2 are not addressed at MVP. A future Phase 5 enhancement should run D11 at δ ∈ {30s, 3m, 10m} simultaneously and aggregate evidence across windows with decreasing confidence weights for wider windows.

**DG-D11-2: Cross-token synchronized activity.** D11 is scoped to a single token per evaluation. Cross-token coordination (same wallets buying different tokens simultaneously) is a Phase 5 graph-layer signal requiring a cross-token join at the `swaps` level.

**DG-D11-3: Baseline contamination detection.** E-D11-5 (Poisson baseline poisoning) requires correlation with D05 Signal A during the 7-day baseline window. A Phase 3 cross-detector signal in the scoring crate can flag `lambda_contaminated` when D05 Signal A fires during the D11 baseline period.

**DG-D11-4: Per-chain window calibration (EVM Phase 4).** The 30-second window is calibrated for Solana's ~400ms slot time. EVM chains (12s Ethereum blocks, ~400ms Arbitrum blocks) need chain-specific window defaults when D11 is extended to Phase 4.

**DG-D11-5: Historical cluster membership tracking.** If Decision 6 remains Option A (stateless), there is no way to detect that the same cluster reappeared across multiple evaluation windows. This tracking is the primary value of Decision 6 Option B and should be revisited after 30-day production operation.

---

## §14 References

All citations are tracked in REFERENCES.md. The four D11-primary entries are:

| Citation | Role in D11 |
|----------|-------------|
| Mazza, Cresci et al. 2019 (RTbust, ACM WebSci 2019, arXiv:1902.04506) | Primary methodological anchor; DBSCAN on temporal patterns; F1=0.87 anchor for calibration targets |
| Mannocci, Mazza et al. 2024 (CIB Survey, arXiv:2408.01257) | Definitional precision for actors/actions/intent framework; Jaccard temporal similarity recommendation; Poisson null model framing |
| Arnold et al. 2024 (Temporal Motifs, Scientific Reports, arXiv:2402.09272) | On-chain primary citation; establishes temporal motif signal in crypto networks; N_min = 3 lower bound derivation |
| Nizzoli, Tardelli et al. 2020 (Crypto Landscape, IEEE Access, arXiv:2001.10289) | Domain validation; >56% P&D Telegram bots; confirms CIB methodology applies to on-chain coordination |
| research/sprint13-b-citations.md §Task 1 | Window δ=30s derivation; N_min=5 recommendation; Poisson framework formulation; config key naming |
| Chainalysis 2025 | P&D base rate 3.59%; base rate inputs for §5.5 FP rate target derivation |
| Liu et al. 2025 (arXiv:2505.09313) | Temporal clustering as Sybil feature (§3.2); supports on-chain temporal concentration as anomaly signal |
