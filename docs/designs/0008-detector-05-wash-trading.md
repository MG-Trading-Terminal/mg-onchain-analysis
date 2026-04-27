# Design 0008 — Detector D05: Wash Trading Heuristic 1

**Date:** 2026-04-21
**Status:** Draft
**Author:** onchain-analyst agent
**ADR refs:**
- ADR 0001 §D4 — `AnomalyEvent { confidence, severity, evidence }` output contract
- ADR 0001 §D5 — MVP detector #5 (Wash Trading), priority M
- ADR 0001 §D7 — fixture corpus bootstrapping from RugCheck rugged=true / jup_verified
- ADR 0002 — Postgres-only storage; all queries in PostgreSQL dialect
- ADR 0003 — self-sovereign infrastructure; no 3rd-party runtime dependencies
**Trait ref:** `docs/designs/0003-detector-trait.md` — implements `Detector` trait, uses `DetectorContext`; `fetch_rows` / `compute` split for testability
**Query ref:** `docs/queries/d05_wash_trading_h1.sql` — Signal A round-trip self-join pattern (PostgreSQL dialect)
**Probe refs:**
- `research/fixtures/wash_trading/POS_01_synth_single_wallet.json` — positive anchor: 5 round-trips, single wallet
- `research/fixtures/wash_trading/NEG_01_BONK.json` — negative anchor: high-volume legitimate trading
**Detector ID:** `wash_trading_h1`

---

## 1. Context

Wash trading — the execution of buy and sell orders against oneself or a colluding counterparty
to artificially inflate trading volume — was responsible for $704M of fabricated DEX volume in
2024 (0.035% of total DEX volume), per Chainalysis (2025). Victor & Weintraud (2021) found that
>30% of tokens traded on IDEX and EtherDelta exhibited wash-trading patterns, with $159M in wash
volume documented using legal-definition criteria. The market-maker consumer is the primary
beneficiary of this detector: fabricated volume causes MM algorithms to missize inventory and
spread.

The core academic signal (Chainalysis Heuristic 1) is a same-address round-trip: buy and sell
in the same pool within a short block window, with near-zero net volume difference, repeated
enough times to establish a pattern. For Ethereum (12-second blocks), the Chainalysis window of
25 blocks ≈ 5 minutes. For Solana (400ms slots), 25 slots ≈ 10 seconds — a dramatically tighter
window. This tightness is an asset: legitimate market makers rarely close a position within 10
seconds on a single DEX pool; MEV arb bots close within the same slot (not caught by this
detector, by design). The 10-second window is the detection sweet spot for Solana wash traders
who exploit low fees to cycle through positions rapidly.

This spec covers three signals:

- **Signal A (H1 pattern):** Primary. Per-wallet, per-pool round-trip detection — same address
  buys then sells within `block_window_slots`, volume diff < `volume_diff_pct`, repeated ≥
  `min_repetitions` times.
- **Signal B (cluster wash):** Latent-risk proxy. N wallets whose net flows to each other
  approximately cancel within a pool window, indicating coordinated volume fabrication without
  self-dealing.
- **Signal C (volume inflation amplifier):** Contextual. When Signal A or B fires, the ratio
  of wash volume to total pool volume upgrades severity if ≥ `severity_amplifier_ratio`.

This spec is the implementation contract for the P4-2 developer task. The developer implements
`crates/detectors/src/d05_wash_trading.rs` without modifying any frozen type in `crates/common`.

---

## 2. Signal Taxonomy

D05 produces zero to two `AnomalyEvent`s from a single `evaluate()` call:

| Signal | When it fires | Confidence band | Severity range | Anomaly category |
|--------|--------------|-----------------|----------------|-----------------|
| A — H1 round-trip pattern | Same sender, same pool: buy→sell within `block_window_slots`, `vol_diff < volume_diff_pct`, count ≥ `min_repetitions` | 0.60–0.95 | Medium–Critical | Wash trading |
| B — Cluster reciprocal flow | ≥ `min_cluster_size` senders in pool with net flows summing to ≈ zero within ± `cluster_balance_tolerance_pct`, total cluster volume ≥ `min_cluster_volume_usd` | 0.50–0.60 | Low–Medium | Wash trading (Sybil proxy) |
| C — Volume inflation amplifier | A or B has fired; `wash_volume_ratio ≥ severity_amplifier_ratio` | Additive to severity band only | Severity upgrades one band | Evidence amplifier (not standalone) |
| Info — Insufficient data | No swaps in window for pool | 0.02 | Info | Data gap |
| Info — Pool too thin | Pool USD value < `min_pool_usd_for_h1` | 0.02 | Info | Design exclusion |

Signals A and B are independent and may fire simultaneously from the same evaluate() call,
each producing a separate `AnomalyEvent` element in the output `Vec`. Signal C is not a
standalone event — it modifies the `Severity` of whichever Signal A or B event fired first
(highest-confidence event in the output vector). Signal A is suppressed for established
protocols (see §6). Signal B is not suppressed.

---

## 3. Algorithm

### 3.1 Entry Point

```
FUNCTION evaluate(ctx: DetectorContext) -> Result<Vec<AnomalyEvent>, DetectorError>:

  cfg = ctx.config.wash_trading_h1
  meta = ctx.registry.enrich(ctx.token, ctx.chain).await
  IF meta is Err:
    RETURN Err(MissingDependencyData {
      detector_id: "wash_trading_h1",
      token: ctx.token.canonical,
      reason: "TokenMeta not yet enriched"
    })

  // Pool dust filter
  IF meta.total_market_liquidity_usd < cfg.min_pool_usd_for_h1.value:
    RETURN Ok(vec![make_info_event("insufficient_liquidity",
      metrics: {
        "wash_trading_h1/pool_usd": meta.total_market_liquidity_usd.to_string(),
        "wash_trading_h1/min_pool_usd": cfg.min_pool_usd_for_h1.value.to_string()
      })])

  events = []

  // --- Signal A ---
  established = is_established_protocol(&meta)
  IF NOT established:
    signal_a_rows = fetch_signal_a(ctx, cfg).await
    FOR row IN signal_a_rows:
      confidence_a = compute_signal_a_confidence(row, cfg)
      evidence_a = build_evidence_a(row)
      events.push(make_anomaly_event("wash_trading_h1", confidence_a, evidence_a))
  ELSE:
    // Add suppression audit key to any future events for this token
    suppressed_signal_a = true

  // --- Signal B ---
  // NOT gated by is_established_protocol — Sybil cluster wash on established
  // protocols is still coordinated manipulation even if Signal A is suppressed.
  sender_rows = fetch_pool_senders(ctx, cfg).await

  IF sender_rows.len() >= cfg.min_cluster_size.value:
    cluster_result = compute_cluster_flows(sender_rows, cfg)
    IF cluster_result.cluster_found:
      confidence_b = compute_signal_b_confidence(cluster_result, cfg)
      evidence_b = build_evidence_b(cluster_result, suppressed_signal_a)
      events.push(make_anomaly_event("wash_trading_h1", confidence_b, evidence_b))

  IF events.is_empty():
    // No swaps at all → MissingDependencyData upstream; if we get here, pool had
    // activity but thresholds were not breached.
    RETURN Ok(vec![])

  // --- Signal C (amplifier on the highest-confidence event) ---
  wash_vol_usd = sum_wash_volumes(events)
  total_pool_vol_usd = fetch_total_pool_volume(ctx, cfg).await
  IF total_pool_vol_usd > Decimal::ZERO:
    wash_ratio = wash_vol_usd / total_pool_vol_usd
    IF wash_ratio >= cfg.severity_amplifier_ratio.value:
      apply_signal_c_amplifier(events[0], wash_ratio, total_pool_vol_usd)

  IF suppressed_signal_a AND events.is_empty():
    // Established protocol, no Signal B fired — emit audit Info
    RETURN Ok(vec![make_info_event("established_protocol_signal_a_suppressed",
      metrics: { "wash_trading_h1/established_protocol_suppressed_signal_a": "1" })])

  RETURN Ok(events)
```

---

### 3.2 Signal A — H1 Round-Trip Detection

Signal A uses `docs/queries/d05_wash_trading_h1.sql`. The query is written in PostgreSQL
dialect (ADR 0002). The developer wraps this in a `fetch_signal_a` function following the
`fetch_rows` / `compute` split pattern from `docs/designs/0003-detector-trait.md §mock.rs`.

The query's self-join pattern:

```sql
WITH buys AS (
    SELECT sender, pool, block_height, tx_hash, amount_out_raw AS token_amount, usd_value
    FROM swaps
    WHERE chain = $1 AND token_out = $2
      AND block_time >= $3 AND block_time < $4
),
sells AS (
    SELECT sender, pool, block_height, tx_hash, amount_in_raw AS token_amount, usd_value
    FROM swaps
    WHERE chain = $1 AND token_in = $2
      AND block_time >= $3 AND block_time < $4
),
round_trips AS (
    SELECT b.sender, b.pool, b.tx_hash AS buy_tx, s.tx_hash AS sell_tx,
           b.block_height AS buy_block, s.block_height AS sell_block,
           ABS(b.token_amount::DOUBLE PRECISION - s.token_amount::DOUBLE PRECISION)
               / GREATEST(b.token_amount::DOUBLE PRECISION, s.token_amount::DOUBLE PRECISION)
                                               AS volume_diff_pct,
           COALESCE(b.usd_value, Decimal::ZERO) AS buy_usd
    FROM buys b
    INNER JOIN sells s
        ON b.sender = s.sender AND b.pool = s.pool
       AND s.block_height > b.block_height
       AND s.block_height - b.block_height <= $7   -- block_window_slots
    WHERE ABS(...) / GREATEST(...) <= $5            -- volume_diff_pct threshold
)
SELECT sender, pool,
       COUNT(*)               AS round_trip_count,
       AVG(volume_diff_pct)   AS avg_volume_diff_pct,
       MIN(buy_block)         AS first_seen_block,
       MAX(sell_block)        AS last_seen_block,
       MIN(buy_tx)            AS first_buy_tx,
       MAX(sell_tx)           AS last_sell_tx,
       SUM(buy_usd)           AS wash_volume_usd
FROM round_trips
GROUP BY sender, pool
HAVING COUNT(*) >= $6         -- min_repetitions
ORDER BY round_trip_count DESC;
```

Full canonical query lives in `docs/queries/d05_wash_trading_h1.sql`. The developer MUST NOT
inline the query in Rust; use `include_str!()` or a const pointing to the query file, matching
the D02/D04 storage-method pattern.

**Parameter binding (sqlx positional):**
- `$1` chain (TEXT), `$2` token (TEXT), `$3` window_start (TIMESTAMPTZ),
  `$4` window_end (TIMESTAMPTZ), `$5` volume_diff_pct (DOUBLE PRECISION),
  `$6` min_repetitions (INT), `$7` block_window_slots (BIGINT)

**Row-level filtering note:** The query returns one row per `(sender, pool)` pair. A single
evaluate() call for a token with 3 pools may return up to 3N rows where N is the distinct
senders per pool. The caller aggregates by `pool` for Signal C (total pool volume) but reports
each `(sender, pool)` pair as a separate evidence entry within a single AnomalyEvent.

**Sell-before-buy (sell→buy) pairs:** The query as written requires `s.block_height > b.block_height`
— buy must precede sell. Wash traders occasionally reverse this (sell then buy to establish a
short-then-cover pattern). Add a symmetric query with buy and sell CTEs swapped and the condition
`b.block_height > s.block_height AND b.block_height - s.block_height <= $7`. Union the results,
deduplicate on `(buy_tx, sell_tx)` to prevent double-counting if both directions match. Label
this sub-variant in evidence as `wash_trading_h1/direction = "sell_first"` vs `"buy_first"`.

---

### 3.3 Signal B — Cluster Reciprocal Flow Detection

Signal B is a cheap Phase 2 proxy for the Sybil/multi-wallet wash pattern that Heuristic 1
cannot detect (different wallet buys, different wallet sells, net flow ≈ 0 across the group).

**Algorithm:**

```
FUNCTION compute_cluster_flows(sender_rows, cfg) -> ClusterResult:

  // sender_rows: list of (sender, net_token_flow_in, net_token_flow_out, volume_usd)
  // net_token_flow_in = SUM(amount_out_raw WHERE token_out = tracked_token)
  // net_token_flow_out = SUM(amount_in_raw WHERE token_in = tracked_token)
  // net_flow = net_token_flow_in - net_token_flow_out (positive = net buyer)

  // Cap to top_senders_cap to bound O(N^2) complexity (see §13 DG5)
  top_senders = sender_rows
    .sort_by(|a, b| b.volume_usd.cmp(a.volume_usd))
    .take(cfg.top_senders_cap.value)  // default: 50

  // For each pair (A, B) in top_senders:
  //   Check if A's net flow ≈ -B's net flow within tolerance
  //   i.e., one is net buyer and the other is a net seller of approximately the same amount
  //   Tolerance: |net_flow_A + net_flow_B| / max(|net_flow_A|, |net_flow_B|) <= cluster_balance_tolerance_pct

  clusters = []
  visited = {}

  FOR i = 0 TO top_senders.len() - 1:
    IF top_senders[i].sender IN visited: CONTINUE
    cluster = [top_senders[i]]
    cluster_net = top_senders[i].net_flow

    FOR j = i+1 TO top_senders.len() - 1:
      IF top_senders[j].sender IN visited: CONTINUE
      combined_net = cluster_net + top_senders[j].net_flow
      combined_magnitude = MAX(ABS(cluster_net), ABS(top_senders[j].net_flow))
      IF combined_magnitude > 0:
        imbalance = ABS(combined_net) / combined_magnitude
        IF imbalance <= cfg.cluster_balance_tolerance_pct.value:
          cluster.push(top_senders[j])
          cluster_net = combined_net

    IF cluster.len() >= cfg.min_cluster_size.value:
      cluster_volume = cluster.iter().map(|s| s.volume_usd).sum()
      IF cluster_volume >= cfg.min_cluster_volume_usd.value:
        clusters.push(Cluster {
          wallets: cluster.iter().map(|s| s.sender).collect(),
          volume_usd: cluster_volume,
          balance_deviation_pct: ABS(cluster_net) / cluster.iter().map(|s| ABS(s.net_flow)).sum()
        })
        cluster.iter().for_each(|s| visited.insert(s.sender))

  IF clusters.is_empty():
    RETURN ClusterResult { cluster_found: false }
  
  // Report the largest cluster by volume
  best = clusters.sort_by_desc(|c| c.volume_usd).first()
  RETURN ClusterResult {
    cluster_found: true,
    cluster_wallets: best.wallets,
    cluster_volume_usd: best.volume_usd,
    cluster_balance_deviation_pct: best.balance_deviation_pct
  }
```

**SQL for sender_rows (`fetch_pool_senders`):**

```sql
-- Parameters: $1 chain, $2 token, $3 window_start, $4 window_end
SELECT
    sender,
    SUM(CASE WHEN token_out = $2 THEN amount_out_raw::DOUBLE PRECISION ELSE 0 END)
        AS net_token_in,
    SUM(CASE WHEN token_in = $2 THEN amount_in_raw::DOUBLE PRECISION ELSE 0 END)
        AS net_token_out,
    SUM(COALESCE(usd_value, 0))
        AS volume_usd
FROM swaps
WHERE chain = $1
  AND (token_in = $2 OR token_out = $2)
  AND block_time >= $3
  AND block_time <  $4
GROUP BY sender
HAVING SUM(COALESCE(usd_value, 0)) > 0
ORDER BY volume_usd DESC
LIMIT $5;  -- top_senders_cap
```

The `LIMIT $5` is `top_senders_cap` (default 50) and is the key to bounding complexity;
see §13 DG5 for the O(N^2) analysis.

---

### 3.4 Signal C — Volume Inflation Amplifier

Signal C fires when Signal A or B has already fired and the wash-trade volume represents a
material fraction of total pool volume. It upgrades severity by one band; it does not change
confidence.

```
FUNCTION apply_signal_c_amplifier(event, wash_ratio, total_pool_vol_usd):
  // Upgrade severity by one band
  new_severity = match event.severity:
    Info     => Low
    Low      => Medium
    Medium   => High
    High     => Critical
    Critical => Critical  // already at ceiling

  event.severity = new_severity
  event.evidence.metrics["wash_trading_h1/wash_volume_ratio"] = wash_ratio.to_string()
  event.evidence.metrics["wash_trading_h1/total_pool_volume_usd"] = total_pool_vol_usd.to_string()
  event.evidence.metrics["wash_trading_h1/signal_c_amplifier_applied"] = "1"
```

**Total pool volume query:**

```sql
SELECT COALESCE(SUM(usd_value), 0) AS total_volume_usd
FROM swaps
WHERE chain = $1
  AND (token_in = $2 OR token_out = $2)
  AND block_time >= $3
  AND block_time < $4;
```

---

## 4. `is_established_protocol` Application Decision

**Signal A: SUPPRESS on established protocols.**

Rationale: A legitimate market maker running on an established protocol (e.g. Raydium's own
treasury MM bot providing liquidity for RAY, or a Jupiter-verified token's professional MM)
will exhibit H1-like round-trip patterns by design — they close positions quickly and repeatedly
to maintain a target spread. Suppressing Signal A for established protocols follows the
asymmetric suppression contract documented in `crates/detectors/src/token_status.rs`:
"Apply ONLY to state-based / latent-risk signals. Do NOT apply to event-based signals that
confirm actual attacks."

In D05, Signal A is a pattern signal (repeated round-trips) rather than a confirmed attack
event. The repetition alone does not prove malicious intent for a token with `jup_strict=true`
or `jup_verified && score < 40`. For those tokens, a professional MM is the most parsimonious
explanation for H1-pattern activity.

When Signal A is suppressed, the detector emits an evidence key
`wash_trading_h1/established_protocol_suppressed_signal_a = "1"` for auditor visibility.

**Signal B: DO NOT SUPPRESS on established protocols.**

Rationale: Sybil clusters with zero-sum net flows represent coordinated volume inflation that
is suspicious regardless of the token's provenance. A cluster of 5+ wallets funded by a common
ancestor running coordinated buys and sells on an established-protocol token is not a
legitimate MM activity pattern — legitimate MMs use a small number of professional wallet
addresses, not a Sybil cluster. Signal B on an established protocol warrants investigation.

This asymmetry mirrors the D04 established-protocol suppression pattern: Signal C (insider
sell-off amplifier for P&D) is suppressed for established protocols; Signal A (spike over
baseline) is not. The principle is identical: event-based or relationship-based signals that
require a pattern of coordinated behaviour across multiple wallets are not suppressed, even
for established protocols.

**Reference:** `crates/detectors/src/token_status.rs` module-level doc § "Asymmetric suppression
contract"; `docs/designs/0005-detector-02-rug-pull.md` §14 for rationale origin.

---

## 5. Threshold Table

| Config Key | Default Value | Rationale | Prior Art |
|------------|--------------|-----------|-----------|
| `wash_trading_h1.block_window_slots` | **25** | Chainalysis 2025 Heuristic 1 canonical value. Solana recalibration: 25 slots ≈ 10s (400ms/slot); legitimate MMs rarely round-trip within 10s on a single pool. Tighter than EVM (25 blocks ≈ 5min) but appropriate for Solana's sub-second slot times. Recalibrate if confirmed Solana wash cases use >25 slot windows. | Chainalysis 2025 |
| `wash_trading_h1.volume_diff_pct` | **0.01** | 1% volume difference threshold. At 0.01, buy and sell volumes must be within 1% of each other — economically equivalent to self-dealing once fees are subtracted. Chainalysis 2025 calibrated this on EVM data; Solana constant-product AMM fees (0.25–1.0%) mean true self-trades will clear under 1% diff after fee accounting. | Chainalysis 2025; Victor & Weintraud 2021 (near-zero-imbalance criterion) |
| `wash_trading_h1.min_repetitions` | **3** | Minimum qualifying round-trip pairs to fire Signal A. One round-trip could be an experimental trade or a MEV failure. Two could be a coincidence. Three establishes a deliberate pattern. Chainalysis 2025 calibrated threshold; $704M detected at this value. | Chainalysis 2025 |
| `wash_trading_h1.min_cluster_size` | **3** | Minimum wallets in a Signal B cluster. Two-wallet circular trades are too similar to normal back-and-forth arbitrage to carry meaningful confidence at this phase. Three wallets with approximately zero-sum flows require a level of coordination that is harder to explain as coincidence. | Design derivation; unverified-heuristic |
| `wash_trading_h1.cluster_balance_tolerance_pct` | **0.05** | 5% net-flow imbalance tolerance for Signal B cluster detection. If 3 wallets' combined net flow is within 5% of zero, the pattern is consistent with coordinated volume fabrication. 5% accounts for price slippage between buy and sell legs; goes beyond what random trading would produce across 3+ wallets. | Design derivation; analogous to Chainalysis H2 buy_sell_imbalance_max (5%) |
| `wash_trading_h1.min_cluster_volume_usd` | **5000.0** | Dust filter for Signal B. A cluster of 3 wallets trading $200 each is noise; $5,000 USD total is the threshold at which the manipulation is economically meaningful. | Design derivation; unverified-heuristic |
| `wash_trading_h1.severity_amplifier_ratio` | **0.30** | 30% of total pool volume composed of wash trades triggers Signal C severity upgrade. Below 30%, the absolute wash volume matters but the pool retains some genuine price-discovery function. Above 30%, the pool's volume signal is materially compromised. | Design derivation; Victor & Weintraud 2021 found >30% wash-trading prevalence on IDEX/EtherDelta as a threshold for market integrity concern |
| `wash_trading_h1.detection_window_hours` | **24** | Standard 24-hour observation window, matching D04 and consistent with Chainalysis 2025 annual-volume aggregation methodology. Shorter windows (1h) produce high false-positive rates from transient arbitrage bursts; longer windows (7d) are too slow for bot-trader-2-0 alert latency requirements. | Chainalysis 2025; D04 window consistency |
| `wash_trading_h1.min_pool_usd_for_h1` | **10000.0** | Pools below $10,000 USD liquidity have negligible price-discovery value; wash trading in a dead pool produces false positives without impacting consumers. Dust filter consistent with D02's `min_pool_usd = 1500` (different: D02 guards for drain risk; here we guard for signal meaningfulness at scale). | Design derivation; unverified-heuristic |
| `wash_trading_h1.top_senders_cap` | **50** | Maximum distinct senders evaluated by Signal B cluster algorithm. Caps Signal B complexity at O(50^2) = 2500 pair comparisons per pool per evaluation. For pools with >50 active senders, the top-50 by volume represent the dominant flow concentration. See §13 DG5 for full complexity analysis. | Design derivation; complexity bound |
| `wash_trading_h1.min_wash_volume_usd` | **500.0** | Minimum total wash volume USD for Signal A confidence formula denominator. Prevents log(0) and stabilises the formula for tiny-volume round trips. | Design derivation; unverified-heuristic |

### Threshold deviations from architect defaults

| Threshold | Architect value | This spec value | Reason |
|-----------|----------------|-----------------|--------|
| `block_window` (now `block_window_slots`) | 25 | **Retained 25** | Chainalysis canonical value. Renamed to `block_window_slots` to make Solana-specificity explicit. |
| `volume_diff_pct` | 0.01 | **Retained 0.01** | Chainalysis calibration confirmed. |
| `min_repetitions` | 3 | **Retained 3** | Chainalysis calibration confirmed. |
| `min_funded_addresses` | 5 | **Renamed, lowered to 3** | `min_cluster_size` in Signal B. Architect stub targeted Chainalysis H2 (funding-graph based); Signal B is a different, cheaper proxy. 3-wallet clusters are the minimum meaningful coordinated pattern without a funding graph. |
| `buy_sell_imbalance_max` | 0.05 | **Retained 0.05** | Renamed `cluster_balance_tolerance_pct`; applied to Signal B rather than H2. |

New keys beyond architect stub: `min_cluster_volume_usd`, `severity_amplifier_ratio`,
`detection_window_hours`, `min_pool_usd_for_h1`, `top_senders_cap`, `min_wash_volume_usd`.

---

## 6. Confidence Composition

### Signal A formula

```
FUNCTION compute_signal_a_confidence(row, cfg) -> f64:
  repetitions = row.round_trip_count as f64
  wash_volume_usd = row.wash_volume_usd  // Decimal
  min_wash = cfg.min_wash_volume_usd.value  // default 500.0

  // Base confidence: scales with repetition count above min_repetitions threshold
  // At min_repetitions=3: 0.60. Each additional rep adds 0.05, saturating at 7+.
  rep_term = (repetitions - 3.0) * 0.05

  // Volume term: log-scaled contribution from wash trade USD value
  volume_ratio = max(wash_volume_usd.to_f64(), min_wash) / min_wash
  vol_term = volume_ratio.ln() * 0.10

  raw = 0.60 + rep_term + vol_term
  RETURN min(0.95, raw)
```

**Calibration anchors:**
- `repetitions=3, wash_vol=$500` (minimum case): `0.60 + 0 + 0.10*ln(1) = 0.60`. Correct: minimum-trigger case at baseline confidence.
- `repetitions=7, wash_vol=$50K`: `0.60 + 0.20 + 0.10*ln(100) = 0.60 + 0.20 + 0.461 = 1.26` → capped 0.95. Correct: saturates for repeated high-volume wash.
- `repetitions=5, wash_vol=$5K`: `0.60 + 0.10 + 0.10*ln(10) = 0.60 + 0.10 + 0.230 = 0.93`. High but not max.

**Saturation:** The formula saturates at 0.95 for 7+ repetitions with any wash volume above
$500 due to the log scale growing slowly after that point. The 0.95 cap is intentional — D05
does not emit 1.0 confidence without a confirmed simulation failure (D01's domain).

### Signal B formula

```
FUNCTION compute_signal_b_confidence(cluster_result, cfg) -> f64:
  cluster_size = cluster_result.cluster_wallets.len() as f64
  min_size = cfg.min_cluster_size.value as f64  // 3

  // Base 0.50 + linear scale by cluster size above minimum
  raw = 0.50 + ((cluster_size - min_size) / 10.0) * 0.10
  RETURN min(0.60, raw)
```

**Calibration anchors:**
- `cluster_size=3` (minimum): `0.50 + 0 = 0.50`. Correct: minimum-trigger case.
- `cluster_size=8`: `0.50 + 0.05 = 0.55`.
- `cluster_size=13+`: `0.50 + 0.10 = 0.60` (capped). Correct: Signal B cannot exceed 0.60 — it is a cheap proxy, not a confirmed pattern.

**Why cap at 0.60:** Signal B detects reciprocal flow patterns, not confirmed self-dealing.
The 0.60 ceiling reflects epistemic uncertainty: the flows could be coincidence or legitimate
arbitrage routing. True Sybil cluster confirmation requires Phase 3 graph analysis (funding
provenance, same-source address detection). The cap communicates this to the scoring crate.

### Signal C amplifier

Signal C does not modify confidence — only severity. See §3.4 algorithm above.

---

## 7. Severity Mapping

Standard `severity_from_confidence()` applied to Signal A:

| Confidence | Severity |
|-----------|----------|
| < 0.30 | Info |
| 0.30–0.49 | Low |
| 0.50–0.64 | Medium |
| 0.65–0.79 | High |
| ≥ 0.80 | Critical |

Signal A confidence range [0.60, 0.95] maps to Medium–Critical.

Signal B is capped at 0.60 → maps to Medium. Signal B never produces High or Critical before
Signal C amplification.

Signal C applies post-`severity_from_confidence()` upgrade:

```
FUNCTION apply_signal_c_amplifier(event, wash_ratio, total_vol):
  IF wash_ratio < cfg.severity_amplifier_ratio.value: RETURN  // no change
  event.severity = upgrade_severity_one_band(event.severity)
  // add evidence keys: wash_volume_ratio, total_pool_volume_usd, signal_c_amplifier_applied
```

Therefore Signal B + Signal C can produce Medium → High (if ratio ≥ 0.30).

---

## 8. Evidence Schema

All keys use `wash_trading_h1/` prefix per `evidence_key()` convention.

### Required keys (MUST be present on every emitted AnomalyEvent)

| Key | Value type | Example | Meaning |
|-----|-----------|---------|---------|
| `wash_trading_h1/signal` | String | `"signal_a"` or `"signal_b"` | Which signal fired |
| `wash_trading_h1/pool` | String | `"7XawhbbxtsRcQA8KTkHT9f9nc6d69UwqCDh6U5EEbEmX"` | Pool address |
| `wash_trading_h1/detection_window_hours` | Decimal | `"24"` | Window used |
| `wash_trading_h1/block_window_slots` | Decimal | `"25"` | Slot window (Signal A only) |
| `wash_trading_h1/established_protocol_suppressed_signal_a` | Decimal (0 or 1) | `"0"` | 1 = Signal A was suppressed |

### Signal A specific keys (present when signal = "signal_a")

| Key | Value type | Example | Meaning |
|-----|-----------|---------|---------|
| `wash_trading_h1/wallet` | String | `"3XYZ..."` | Offending sender address |
| `wash_trading_h1/repetition_count` | Decimal | `"5"` | Number of qualifying round-trip pairs |
| `wash_trading_h1/avg_volume_diff_pct` | Decimal | `"0.0032"` | Avg |buy - sell| / max(buy,sell) across pairs |
| `wash_trading_h1/wash_volume_usd` | Decimal | `"12500.00"` | Sum of buy USD value across qualifying pairs |
| `wash_trading_h1/first_round_trip_tx` | String | `"3abc..."` | Buy tx hash of the first qualifying pair |
| `wash_trading_h1/last_round_trip_tx` | String | `"9xyz..."` | Sell tx hash of the last qualifying pair |
| `wash_trading_h1/direction` | String | `"buy_first"` or `"sell_first"` | Which leg came first |

### Signal B specific keys (present when signal = "signal_b")

| Key | Value type | Example | Meaning |
|-----|-----------|---------|---------|
| `wash_trading_h1/cluster_wallets` | String (JSON array) | `'["A...","B...","C..."]'` | Cluster member addresses |
| `wash_trading_h1/cluster_size` | Decimal | `"3"` | Number of wallets in cluster |
| `wash_trading_h1/cluster_volume_usd` | Decimal | `"52000.00"` | Total volume across cluster members |
| `wash_trading_h1/cluster_balance_deviation_pct` | Decimal | `"0.023"` | Net-flow imbalance ratio across cluster |

### Signal C keys (present when amplifier applied)

| Key | Value type | Example | Meaning |
|-----|-----------|---------|---------|
| `wash_trading_h1/wash_volume_ratio` | Decimal | `"0.43"` | wash_vol / total_pool_vol |
| `wash_trading_h1/total_pool_volume_usd` | Decimal | `"29000.00"` | Total pool volume in window |
| `wash_trading_h1/signal_c_amplifier_applied` | Decimal (0 or 1) | `"1"` | 1 = severity upgraded by Signal C |

### `Evidence.addresses` population

- Offending sender address (Signal A) or cluster wallet list (Signal B), up to 10 addresses.
- Pool address.

### `Evidence.tx_hashes` population

- `first_round_trip_tx` (the buy tx of the first qualifying pair, Signal A only).
- `last_round_trip_tx` (the sell tx of the last qualifying pair, Signal A only).

### `Evidence.notes` format

Human-readable summary for auditors. Example (Signal A fire):
```
"ALERT: wash_trading_h1 Signal A — wallet 3XYZ... executed 5 round-trips in pool 7XawhB... \
 within 25 slots (10s window), avg volume diff 0.32%, total wash volume $12,500. \
 Confidence 0.78 / High."
```
Example (Signal B fire):
```
"ALERT: wash_trading_h1 Signal B — cluster of 3 wallets [A..., B..., C...] in pool 7XawhB... \
 net flow deviation 2.3%, total cluster volume $52,000. Confidence 0.50 / Medium."
```
Example (clean):
```
"wash_trading_h1: no qualifying round-trips in 24h window. Pool $45K USD liquidity. \
 Established-protocol suppression applied to Signal A."
```

---

## 9. Failure Modes

### 9.1 No swaps in window

**Trigger:** `swaps` table returns zero rows for `(chain, token, window_start, window_end)`.

**Action:** Return `Err(DetectorError::MissingDependencyData { reason: "no swap rows found" })`.
The scheduler retries after the indexer populates new swaps. Do NOT emit a false Info event
suggesting the pool is clean — no data means no conclusion.

---

### 9.2 Pool below `min_pool_usd_for_h1`

**Trigger:** `meta.total_market_liquidity_usd < cfg.min_pool_usd_for_h1.value` (default $10,000).

**Action:** Return a single `Info` severity event with confidence 0.02 and evidence key
`wash_trading_h1/insufficient_liquidity = "1"`. The pool is too thin to produce meaningful
wash-trading signal; high false-positive rates dominate. Do not evaluate Signal A or B.

---

### 9.3 Signal A query times out

**Trigger:** The self-join query exceeds the configured query timeout (from `DetectorContext`).

**Action:** Return `Err(DetectorError::StorageError { ... })`. Log at WARN level with the
query parameters. The scheduler retries with exponential backoff. Do not emit a partial result.

---

### 9.4 Signal B `fetch_pool_senders` returns < `min_cluster_size` senders

**Trigger:** Fewer than `min_cluster_size` (3) distinct senders in the pool in the window.

**Action:** Skip Signal B silently. A pool with 1–2 senders cannot form a cluster. This is a
legitimate state for newly listed or thin tokens.

---

### 9.5 `usd_value` is NULL for swaps

**Trigger:** The indexer has not yet populated price data for some swap rows; `usd_value = None`
in `Swap` struct.

**Action:** Signal A uses `token_amount` (raw units) for volume-diff computation, which does
not require USD conversion. Signal C volume ratio computation uses `COALESCE(usd_value, 0)`,
which will undercount. Add evidence key `wash_trading_h1/missing_usd_value_count = N` where N
is the count of null-price swap rows in the window. If more than 50% of rows have null USD,
skip Signal C (volume ratio is unreliable) and add
`wash_trading_h1/signal_c_skipped_missing_price = "1"`.

---

### 9.6 Established protocol with active Signal B

**Trigger:** `is_established_protocol(meta) = true` AND Signal B fires.

**Action:** Do NOT suppress Signal B. Emit the Signal B event with the evidence key
`wash_trading_h1/established_protocol_suppressed_signal_a = "1"`. The established-protocol
flag is informational for auditors reviewing the event; it does not gate Signal B.

---

## 10. Fixture Specification

Files in `research/fixtures/wash_trading/`. Developer writes integration tests in
`tests/fixtures/wash_trading/` pointing at these files.

### Positive Fixtures (detector FIRES)

| File | Type | Setup | Expected Signal | Expected Confidence | Notes |
|------|------|-------|-----------------|--------------------:|-------|
| `POS_01_synth_single_wallet.json` | Synthetic | 1 wallet, 5 buy→sell round-trips, avg vol diff 0.003, 10-slot gap, $12K wash vol | Signal A fires | 0.78–0.85 | Primary Signal A anchor; tests repetition + volume formula |
| `POS_02_synth_cluster.json` | Synthetic | 3 wallets, net flows ≈ zero (±2%), total cluster vol $52K, no individual wallet reaches min_repetitions | Signal B fires | 0.50–0.55 | Signal B only; Signal A silent; tests cluster algorithm |
| `POS_03_synth_high_volume_wash.json` | Synthetic | 1 wallet, 7 round-trips, $90K wash vol, wash/pool ratio 0.45 (≥ severity_amplifier_ratio 0.30) | Signal A fires + Signal C amplifies | Signal A 0.95; severity upgrade Medium→High or High→Critical | Tests Signal C severity uplift |

### Negative Fixtures (detector BELOW threshold)

| File | Type | Token | Expected Result | Rationale |
|------|------|-------|-----------------|-----------|
| `NEG_01_BONK.json` | Live (DezXAZ8z) | BONK | No fire | High distributed trading volume; no single wallet with ≥ 3 round-trips in 25 slots; jup_verified, score < 40 → even if Signal A patternmatches, suppressed |
| `NEG_02_RAY.json` | Live (4k3Dyjzv) | RAY | Signal A suppressed (established protocol); Signal B no fire | `jup_verified=false, score=56` — RAY does not satisfy `is_established_protocol` per token_status.rs tests, BUT serves as a test of the boundary condition: Signal A fires for RAY but is documented as a calibration gap, parallel to D02/D04 RAY FP debt. Expected: Signal A fires with confidence ≤ 0.65, NOT suppressed. |
| `NEG_03_USDC.json` | Live (EPjFWdd5) | USDC | No fire (pool below min_pool_usd_for_h1 on Solana) | USDC Solana pool is a canonical stable reference; insufficient trading volume on native chain to trigger wash patterns |

**Note on NEG_02 (RAY):** RAY does not pass `is_established_protocol()` per the current
implementation (jup_verified=false, score=56, not jup_strict). This is a known calibration
gap identical to the D02/D04 RAY outstanding FP. The NEG_02 fixture should carry
`"calibration_flag": true` and `"expected_correct": false` — meaning the detector fires on RAY
but this is a known acceptable FP pending a separate P4-0 calibration task for D05. The fixture
serves to document the gap, not to assert a passing test.

**Fixture format:** JSON schema matching the D04 pump_dump fixture format in
`research/fixtures/pump_dump/`. Each fixture carries:
- `"synthetic": true/false`
- `"calibration_flag": true/false`
- `"expected_correct": true/false`
- `"expected_signal": "signal_a" | "signal_b" | "none"`
- `"expected_confidence_min": float`
- `"expected_confidence_max": float`
- Swap rows with `sender`, `pool`, `token_in`, `token_out`, `amount_in_raw`, `amount_out_raw`,
  `block_height`, `block_time`, `usd_value`, `tx_hash`.

---

## 11. Known Evasions

### E-D05-1 — Sybil Wallet Rotation (< min_repetitions per wallet)

**Attack:** Instead of one wallet doing 5 round-trips, attacker uses 5 wallets each doing
1 round-trip. Each wallet has repetition_count=1 < min_repetitions=3, so Signal A never fires.

**Signals defeated:** Signal A (repetition count guard).

**Signals that partially catch it:** Signal B (if the 5 wallets' net flows cancel within the
cluster tolerance, Signal B fires). However, Signal B has no per-wallet repetition guard — a
single round-trip per wallet is enough to be included in a cluster.

**Attacker cost:** Low. Wallet creation on Solana costs ~0.002 SOL each. Rotation requires
small coordination overhead.

**Detection path:** Phase 3 graph module (funding provenance, same-funder cluster detection
per Liu et al. 2025). Phase 2 Signal B is a weak proxy.

---

### E-D05-2 — Expanded Block Window (>25 slots between buy and sell)

**Attack:** Attacker waits 30–60 slots between buy and sell legs. Each pair exceeds
`block_window_slots=25` and is therefore not matched by the self-join.

**Signals defeated:** Signal A (block gap filter `s.block_height - b.block_height <= 25`).

**Signals that partially catch it:** Signal B (net flow still approximately cancels). D04
Signal A if the wash trading inflates price.

**Attacker cost:** Low. Trivially automatable — just add a time delay between buy and sell.

**Calibration note:** The 25-slot window is the Chainalysis canonical value; an empirical
Solana recalibration may determine that 50 or 100 slots is more appropriate. The config
makes this adjustable. The evasion window for Solana (100 slots ≈ 40s) is still tight enough
to be distinguishable from legitimate trading behavior, which typically operates on timescales
of minutes to hours.

---

### E-D05-3 — Volume Difference Just Above Threshold (>1% vol diff per pair)

**Attack:** Attacker ensures each buy-sell pair has a volume diff slightly above 1% (e.g.
1.05%). Each individual pair fails the `volume_diff_pct` filter. The attacker still creates
artificial volume but avoids the pattern detector.

**Signals defeated:** Signal A (volume diff filter).

**Signals that partially catch it:** Signal B (net flows over the window still approximately
cancel). D04 Signal A if price is moved.

**Attacker cost:** Minimal — adjust trade sizes by ~1–2% each.

**Mitigation path:** Raise `volume_diff_pct` threshold slightly (e.g. 0.02) to reduce this
attack surface. But raising it increases false-positive rate from legitimate MM trades that
have small residual imbalances from fee deductions. Trade-off: current 0.01 is calibrated
from Chainalysis 2025; adjust with corpus evidence only.

---

### E-D05-4 — Multi-Hop Round Trip via Intermediate Wallet

**Attack:** Wallet A buys. Wallet A sends tokens to Wallet B. Wallet B sells. The net flow
from the pool's perspective is: token out to A, token in from B — zero sum. But A and B have
different `sender` values in the `swaps` table, so Signal A's self-join on `b.sender = s.sender`
does not match.

**Signals defeated:** Signal A (sender equality requirement).

**Signals that partially catch it:** Signal B (A and B in the same pool with approximately
zero-sum flows). If B receives tokens from A via an SPL Transfer (not a pool swap), B's
buy-side contribution in the swaps table may be missing, causing Signal B to undercount.

**Detection path:** Phase 3 graph work — trace token flows through transfer edges, not just
pool swaps. Multi-hop wash detection requires following `Transfer` events between the pool
events.

---

### E-D05-5 — Cross-Pool Round Trip (Different Pools, Same Net Effect)

**Attack:** Attacker buys token in Pool X and sells in Pool Y (arbitrage-like). From Pool X's
perspective, there is only a buy. From Pool Y's perspective, there is only a sell. The
per-pool Signal A query never sees a buy-sell pair from the same sender in the same pool.

**Signals defeated:** Signal A (same-pool requirement `b.pool = s.pool`).

**Is this wash trading or arbitrage?** Cross-pool round trips are economically identical to
arbitrage — they benefit from price differences across pools. This evasion exploits a genuine
ambiguity. For Phase 2, cross-pool round trips are classified as potential arbitrage (not wash
trading) and are correctly excluded from Signal A. Phase 3 may introduce a cross-pool wash
variant with additional criteria (e.g., both pools have the same deployer, or the round trip
is unprofitable at execution prices).

**Attacker cost:** Low (but requires two pools to be active). For thin tokens with one
primary pool, this evasion is not available.

---

### E-D05-6 — Jupiter-Routed Wash (Aggregator Obscures Sender)

**Attack:** The attacker routes both buy and sell through Jupiter aggregator. Depending on
how the chain adapter resolves the `sender` field in the `Swap` struct, the sender may be
the Jupiter router program ID rather than the economic sender's wallet.

**Signals defeated:** Signal A (sender field is the aggregator, not the attacker wallet;
self-join never matches).

**Mitigation (current):** `crates/common/src/event.rs` documents that `sender` MUST be the
"economic sender (the wallet paying), not the aggregator router." The chain adapter is
responsible for tracing through aggregator hops. If this trace is implemented correctly,
Signal A is not defeated. If the adapter traces incorrectly, Signal A is silently blind to
Jupiter-routed wash trades.

**Detection path:** Verify chain adapter Jupiter sender resolution in integration tests.
Add a test fixture with Jupiter-routed swaps and verify sender = ultimate EOA.

---

### E-D05-7 — Minimal Cluster Size Attack (Exactly 2 Wallets)

**Attack:** Two wallets execute mirror trades: Wallet A buys, Wallet B sells (and vice versa
repeatedly). Signal B requires `min_cluster_size = 3`, so a two-wallet cluster below the
threshold is invisible to Signal B. Signal A also fails because no single wallet has ≥ 3
round-trips against itself.

**Signals defeated:** Signal A, Signal B.

**Residual catch:** If A and B are funded from a common source, Phase 3 graph clustering may
catch the link. D04 Signal A may fire if the total volume spike is large enough.

**Mitigation:** Lower `min_cluster_size` to 2. Risk: two-wallet coincidental matching
dramatically increases Signal B false-positive rate. Victor & Weintraud (2021) found that
two-party circular trades were the most common wash pattern but also the hardest to distinguish
from normal arbitrage at this phase. Retain 3 for Phase 2; consider 2 in Phase 3 with graph
confirmation.

---

### E-D05-8 — Real MM Mimicry on Unlisted Protocols

**Attack:** Attacker deploys a professional-looking but unlisted MM wallet that genuinely
market-makes (quotes bids and asks) to obscure wash-trading intent. The MM produces H1-like
round-trip patterns with volume diff < 1%. The token is not established (`jup_strict=false`,
`jup_verified=false`) so Signal A is not suppressed.

**Signals defeated:** None — Signal A fires correctly. But this creates a false-positive
category where the "attack" is indistinguishable from a legitimate MM.

**Mitigation:** This is the canonical false-positive scenario (see §12 Failure Modes FP1).
Mitigation requires a known-MM address list (out of scope for Phase 2) or a net-position
delta check over a longer window. A legitimate MM carries net inventory over time; a wash
trader has near-zero net position over any window. Phase 3 enhancement: add a 7-day net
position check — if net position delta < 0.1% of trading volume, the wallet is likely wash
trading, not market making.

---

### E-D05-9 — Fee-on-Transfer Token Disguise

**Attack:** On a token with a 0.5% transfer fee, every sell automatically has volume_diff ≈
1.0% below the buy amount (due to fee deducted on transfer to pool). The attacker calibrates
buy and sell amounts so that after the fee, the volume diff appears to be 1.05% — just above
the `volume_diff_pct = 0.01` threshold. Signal A is evaded despite the round-trip being
economically unprofitable for any honest actor.

**Signals defeated:** Signal A (volume diff inflated past threshold by transfer fee).

**Signals that partially catch it:** D01 Signal S2 (transfer fee above threshold).

**Mitigation:** Add a `fee_adjusted_volume_diff` computation in Signal A: if the token has
a `transfer_fee.fee_bps > 0`, subtract the expected fee deduction from the sell amount before
computing the diff. This is a Phase 3 enhancement; Phase 2 Signal A is blind to this case.

---

### E-D05-10 — Baseline Contamination of D04 (Cross-Detector Interaction)

**Attack:** Attacker wash-trades below D05 Heuristic 1 thresholds for 7 days to inflate the
D04 Signal A 7-day rolling baseline. The fake volume pumps up the `median_volume_usd`, which
makes the real pump appear proportionally smaller (volume ratio < 5×). D04 Signal A fails to
fire during the actual pump even though the absolute volume spike is significant.

**This evasion exploits D05's blind spots to compromise D04.** It is documented in D04
evasion E-D04-21 (cross-reference to REFERENCES.md). Signal B in D04 (burst concentration)
partially catches the pump since it uses absolute burst ratio, not the baseline-adjusted ratio.

**Detection path:** D03 concentration shift detector may catch the insider accumulation phase.
Phase 3 cross-detector scoring: if D04 Signal A baseline is suspected to be contaminated (D05
fires on the same token's 7-day window history), apply a decontaminated baseline.

---

## 12. Known False-Positive Scenarios

### FP1 — Legitimate Market Maker on Non-Established Protocol

**Description:** A professional MM bot on a new token (not yet jup_verified) legitimately
quotes bids and asks, closing positions rapidly (within the 25-slot window) to maintain spread.
The MM executes > 3 round-trips per 24h evaluation window with volume diff < 1%.

**Signal A fires correctly but is a false positive.** The MM is providing liquidity, not
fabricating volume.

**Detection frequency:** High — any token with professional MM support will produce this
pattern in the first weeks before jup_verified status is established.

**Mitigation:** The `is_established_protocol` suppression covers the subset of tokens that
are already jup_strict or jup_verified+low-score. For newly listed tokens with professional MMs
but no Jupiter verification yet, Signal A is a true false positive. Consumers should treat
D05 Signal A events on very new tokens with lower weight until the token ages. Phase 3
mitigation: add a `net_position_delta_7d` check — an MM with near-zero net position over 7
days is likely wash trading or very active legitimate MM; use `known_mm_addresses` list from
`token-registry` to suppress known MM wallets.

---

### FP2 — Arbitrage Bot with Fast Round-Trips

**Description:** An arb bot identifies a price discrepancy between pools and executes a
round-trip: buy from one pool address, sell to the same pool address (via AMM mechanics) in
a near-zero block gap. The `sender` field is the arb bot's wallet; both legs are in the same
pool within 25 slots; volume diff may be < 1% after fees.

**Key distinguishing property:** Arbitrage is profitable at execution prices — the buy price
is strictly lower than the sell price (or vice versa). Wash trades are economically neutral
(the attacker neither gains nor loses from the round-trip itself, only from volume fabrication
benefits). However, detecting profitability requires knowing the execution prices precisely,
which is not captured in the `Swap` struct's raw amounts.

**Mitigation (Phase 2):** Guard against same-slot (block_height delta = 0) pairs, since
true arbitrage often closes within the same transaction or adjacent instructions. Require
`s.block_height > b.block_height` (strictly greater, not equal) — already in the query.
Require a minimum block gap of 1 slot to exclude same-transaction arb.

**Note:** The existing query (`s.block_height > b.block_height`) already excludes same-block
arb. Block-gap = 1 (adjacent slots, ~400ms) may still catch some legitimate flash-arb bots.
Phase 3 refinement: add a minimum gap config `min_block_gap_slots = 2` (default 1) to
provide a configuration knob.

---

## 13. Design Gaps

Five areas requiring developer resolution before implementation:

### DG5-1 — Signal B Cluster Algorithm Complexity

**Gap:** The cluster detection algorithm described in §3.3 is O(N^2) in the number of distinct
senders after the `top_senders_cap` cut. With `top_senders_cap = 50`, the complexity is
`50 * 49 / 2 = 1225` pair comparisons per pool per evaluation — fully tractable.

**For pools with thousands of senders:** Without the cap, the naive pairwise algorithm is
O(N^2). For a pool with 10,000 distinct senders in the window, the comparison count is 50M —
not feasible per evaluation. The `top_senders_cap = 50` is the Phase 2 bound. It is
correctly chosen: wash traders need to concentrate volume in their wallets to produce visible
noise; the top-50 by volume captures the meaningful actors.

**Phase 2 recommendation:** Enforce `top_senders_cap = 50` strictly. Document in code comment.

**Phase 3 enhancement:** Replace the pairwise scan with a bucketing approach: sort senders
by net_flow, then scan adjacent pairs (buy-heavy wallets paired with sell-heavy wallets).
This reduces to O(N log N) for sorting. Graph-based cluster detection (Phase 3 graph crate)
eliminates the need for this algorithm entirely by providing funding-provenance clusters.

**Verdict:** O(N^2) after the cap → O(50^2) = 2500 comparisons. Feasible. Cap is mandatory.

---

### DG5-2 — sell-before-buy Direction Coverage

**Gap:** The reference query (`d05_wash_trading_h1.sql`) only detects buy-first, sell-second
round trips. Wash traders who sell-first (to establish a short, then cover) are not detected.

**Resolution options:**
- (a) Add a symmetric query with buy and sell CTEs swapped and UNION the results.
- (b) Accept the one-direction limitation for Phase 2 (sell-first patterns are less common
  on Solana AMMs where most retail interaction is buy-first).

**Recommendation:** Option (a). The symmetric query adds minimal complexity (copy the CTE,
swap buys/sells, UNION ALL, deduplicate on `(buy_tx, sell_tx)` pair). The signal direction
is recorded in evidence as `direction = "sell_first" | "buy_first"`. Cost: doubles the
range-join scan but both CTEs are indexed on `(chain, token, block_time)` so the incremental
cost is low.

---

### DG5-3 — Wash Volume USD Computation When usd_value is NULL

**Gap:** Signal A's confidence formula uses `wash_volume_usd` (see §6). If `usd_value` is
NULL for the buy leg of a round-trip pair, the formula falls back to `min_wash_volume_usd`
(denominator floor). This underestimates confidence for high-volume wash trading on tokens
that lack price data.

**Resolution:** Use `GREATEST(COALESCE(b.usd_value, 0), sell_equivalent_usd)` where
`sell_equivalent_usd` uses the sell leg's `usd_value`. If both are NULL, use
`min_wash_volume_usd` floor with evidence key `wash_trading_h1/missing_usd_value_count`.
The developer must handle this in the `fetch_signal_a` result mapping.

---

### DG5-4 — Multi-Pool Token Handling

**Gap:** A token with 3 active pools may have the same wallet wash-trading on all three
pools independently. The query is scoped per-pool (the `b.pool = s.pool` join condition).
Evaluate per pool or aggregate?

**Resolution:** Evaluate Signal A per pool (consistent with D02's per-market evaluation
pattern). Return one `AnomalyEvent` per (wallet, pool) pair that fires. The consumer
(scoring crate) takes the worst-case event per pool. Add evidence key
`wash_trading_h1/pool_count` with the number of pools evaluated for this token in the
window, so the consumer can detect cross-pool wash patterns.

**Note:** If the same wallet fires on 2 of 3 pools, this is strong evidence of coordinated
wash. Phase 3 enhancement: if the same sender fires on ≥ 2 pools for the same token, upgrade
confidence by +0.10 (cross-pool wash pattern).

---

### DG5-5 — Deterministic Window vs. Block-Time Alignment

**Gap:** The `detection_window_hours = 24` config means `window_start = window_end - 24h`.
On Solana with ~400ms slots, a 24-hour window covers approximately 216,000 slots. For a
token with continuous trading, the `buys` and `sells` CTEs may return tens of thousands of
rows before the self-join. Query performance at this scale is unvalidated.

**Resolution:** Add an execution plan test (EXPLAIN ANALYZE) in CI using a realistic row
count for a high-volume token (e.g., BONK in a 24h window). If the query exceeds the
`ctx.store` timeout, add a secondary filter: `AND usd_value >= $8` (minimum swap USD value)
to reduce low-value noise rows. The `min_swap_usd_filter` config key (default 0.0 — no filter)
provides a knob. Document the performance baseline in a comment in `d05_wash_trading.rs`.

---

## 14. Cross-Detector Interactions

### With D04 (Pump & Dump)

Wash trading is a direct precursor to pump-and-dump schemes. Wash trades inflate the 7-day
rolling volume baseline used by D04 Signal A, potentially requiring the real pump to exceed
a higher multiplier before firing. Conversely, if D05 fires on a token and D04's baseline is
suspected to be contaminated by that wash volume, the scoring crate should note the cross-detector
correlation.

Specific cross-reference: D04 evasion E-D04-10 / E-D04-21 ("Pre-pump baseline contamination
via D05-evading wash trades") in `REFERENCES.md`. When D05 Signal A fires on a token, the
scoring crate should flag D04's baseline as potentially contaminated for that token and apply
a corrected baseline (D04 volume excluding the wash-trade period) if the Phase 3 decontamination
feature is available.

### With D02 (Rug Pull / LP Drain)

Rug-pull orchestrators sometimes inflate token volume via wash trading to attract genuine
retail buyers before the LP drain, creating the appearance of a healthy trading token. D05
Signal A firing on a token 24–48 hours before a D02 Signal A drain event is a meaningful
co-occurrence. The scoring crate should weight the combined D02+D05 signal higher than either
alone.

---

## 15. Developer Acceptance Checklist

Before marking P4-2 complete, the developer must verify:

### Implementation

- [ ] `WashTradingH1Detector` is implemented in `crates/detectors/src/d05_wash_trading.rs`
  per this spec. The stub (if any) is fully replaced.
- [ ] `Detector::evaluate()` executes Signal A (via `fetch_signal_a`) and Signal B (via
  `fetch_pool_senders` + `compute_cluster_flows`) per §3.
- [ ] Signal A is gated by `is_established_protocol(&meta)` — suppressed when true.
- [ ] Signal B is NOT gated by `is_established_protocol` — always evaluated.
- [ ] Signal C is computed and applied as a severity upgrade, not a confidence change.
- [ ] `fetch_signal_a` uses `include_str!()` or equivalent to load `d05_wash_trading_h1.sql`;
  the SQL is NOT inlined in the Rust source.
- [ ] The sell-before-buy symmetric query variant is implemented per DG5-2 resolution.
- [ ] Query parameters are bound positionally via sqlx; no string interpolation.
- [ ] `compute()` and `fetch_rows()` are split for testability (pure compute function tested
  independently, matching the D02/D04 pattern from design 0003).
- [ ] No `Utc::now()` calls inside `evaluate()` or any called function — all time comes
  from `ctx.window`. This is a CRITICAL constraint per D04 review finding C1.

### Config

- [ ] All threshold keys from §5 are present in `config/detectors.toml` under
  `[wash_trading_h1.*]` with `value`, `rationale`, and `refs` fields.
- [ ] New keys added: `min_cluster_volume_usd`, `severity_amplifier_ratio`,
  `detection_window_hours`, `min_pool_usd_for_h1`, `top_senders_cap`, `min_wash_volume_usd`.
- [ ] `WashTradingH1Config` struct in `crates/detectors/src/config.rs` is updated with all
  new fields as `Threshold<T>` wrappers.
- [ ] Config load test: all keys present in `config/detectors.toml` and deserialize without
  error.

### Evidence

- [ ] All required evidence keys from §8 are present on every emitted event.
- [ ] Signal A evidence keys are present only for Signal A events.
- [ ] Signal B evidence keys are present only for Signal B events.
- [ ] Signal C keys are present when and only when the amplifier is applied.
- [ ] `established_protocol_suppressed_signal_a` evidence key is set to "1" when Signal A
  is suppressed, "0" otherwise, on every event.
- [ ] `Evidence.addresses` includes sender (Signal A) or cluster wallets up to 10 (Signal B)
  plus the pool address.
- [ ] `Evidence.tx_hashes` includes `first_round_trip_tx` and `last_round_trip_tx` (Signal A).

### Tests

- [ ] Unit test for `compute_signal_a_confidence()`: repetitions=3, wash_vol=$500 → 0.60.
- [ ] Unit test for `compute_signal_a_confidence()`: repetitions=7, wash_vol=$50K → 0.95 (capped).
- [ ] Unit test for `compute_signal_a_confidence()`: repetitions=5, wash_vol=$5K → ≈ 0.93.
- [ ] Unit test for `compute_signal_b_confidence()`: cluster_size=3 → 0.50.
- [ ] Unit test for `compute_signal_b_confidence()`: cluster_size=13 → 0.60 (capped).
- [ ] Unit test for `compute_cluster_flows()`: 3 wallets, net flows +100/-60/-40 → cluster
  found, deviation ≈ 0.0.
- [ ] Unit test for `compute_cluster_flows()`: 2 wallets → cluster NOT found (< min_cluster_size).
- [ ] Unit test for Signal C amplifier: severity Medium + wash_ratio=0.35 → severity High.
- [ ] Unit test for Signal C amplifier: severity Medium + wash_ratio=0.29 → severity unchanged.
- [ ] Unit test for established-protocol suppression: `is_established_protocol=true` →
  Signal A does NOT fire; Signal B still evaluates.
- [ ] Integration test (Postgres test container): insert 5 round-trip swap pairs for one
  sender within 25 block window, vol diff < 1%; run Signal A query; verify round_trip_count=5.
- [ ] Integration test: insert BONK-like distributed swaps (many unique senders, no
  repetitions per sender); verify Signal A returns zero rows.
- [ ] Fixture test: run detector against `POS_01_synth_single_wallet.json`; assert
  confidence ∈ [0.78, 0.85] and severity ≥ Medium.
- [ ] Fixture test: run detector against `NEG_01_BONK.json`; assert no Signal A or B fire.

### Cross-references

- [ ] `REFERENCES.md` entries for Chainalysis 2025 (D05 row) and Victor & Weintraud 2021
  (D05 row) are already present. Confirm they are correct before merge; add 3 new entries
  per §16 below.
- [ ] `config/detectors.toml` `[wash_trading_h1.*]` section fully populated (replaces stub).
- [ ] `docs/designs/0003-detector-trait.md` does not require changes — D05 follows
  existing patterns exactly.

---

## 16. REFERENCES.md Additions

Three new entries to add (existing Chainalysis 2025 and Victor & Weintraud 2021 entries
already cover the primary citations):

| Mechanism | Signal / Formula | Source | Used In | Verified Against |
|-----------|-----------------|--------|---------|-----------------|
| Wash trading cluster flow balance (Signal B proxy) | N wallets with net token flows summing to ≈ zero within ±5% tolerance; cluster volume ≥ $5K; proxy for Phase 3 Sybil cluster detection | Design derivation (D05 spec §3.3); analogous to Chainalysis H2 buy_sell_imbalance_max (5%); Victor & Weintraud 2021 circular-trade pattern | D05 Signal B `cluster_balance_tolerance_pct = 0.05`; `min_cluster_volume_usd = 5000` | Design derivation 2026-04-21; unverified-heuristic |
| Wash trading volume inflation ratio (Signal C severity amplifier) | Wash trade volume / total pool volume ≥ 30% degrades pool's price-discovery function materially; severity upgrade justified above this ratio | Victor & Weintraud 2021: >30% wash prevalence on IDEX/EtherDelta cited as market integrity threshold; design derivation for D05 severity amplifier | D05 `severity_amplifier_ratio = 0.30`; Signal C severity upgrade logic | Live fetch 2026-04-21; design derivation |
| Solana 25-slot wash-trading window recalibration | Chainalysis H1 25-block window (ETH, ~5min) recalibrated to 25-slot window (Solana, ~10s); legitimate MMs rarely round-trip within 10s on a single pool; MEV arb closes within the same slot (excluded by >0 block gap guard); tight window is a detection asset not a liability | Research/02-detection-methodology.md §4 Wash Trading gap note; Solana slot timing docs (400ms/slot); design derivation D05 §1 | D05 `block_window_slots = 25`; Signal A query `s.block_height - b.block_height <= 25` guard | Design derivation 2026-04-21; requires empirical Solana wash-case recalibration in Sprint 5 |

---

## References (sources used in this spec)

1. Chainalysis (2025) — Crypto Crime Report / Market Manipulation: wash trading Heuristic 1
   (same address, 25 blocks, <1% vol diff, ≥3 reps); $704M detected on DEX 2024.
   https://www.chainalysis.com/blog/crypto-market-manipulation-wash-trading-pump-and-dump-2025/

2. Victor & Weintraud (2021) — Detecting and Quantifying Wash Trading on Decentralized
   Cryptocurrency Exchanges: $159M wash volume; >30% of IDEX/EtherDelta tokens showed patterns;
   legal-definition near-zero-imbalance criterion.
   https://arxiv.org/abs/2102.07001

3. Liu et al. (2025) — Sybil detection framework: subgraph features + LightGBM; >0.90
   precision/recall on 193K addresses. Informs Phase 3 graph-based cluster detection that
   Signal B proxies for in Phase 2.
   https://arxiv.org/abs/2505.09313

4. `docs/reviews/0003-d04-pump-dump-evasions.md` — evasion patterns E-D04-10 and E-D04-21
   document the D04 baseline contamination interaction with D05 wash trading.

5. `research/02-detection-methodology.md` §4 Wash Trading — primary methodology reference
   for Signal A and Signal B design.
