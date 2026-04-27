# Design 0022 — Smart-Money Labelling MVP (Stages 1 + 3, Sprint 22)

**Date:** 2026-04-25
**Status:** Draft — awaiting user sign-off on §11 decisions before implementation
**Author:** onchain-analyst agent
**Sprint:** 22 (S22-1 from SESSION-KICKOFF.md Option C — Sprint 13 B-track investment)
**ADR refs:**
- ADR 0001 §D5 — Phase 3 smart-money labelling deferred pending citations
- ADR 0002 — Postgres-only storage; NUMERIC(39,0) for u128; string-bridged amounts
- ADR 0003 — self-sovereign infrastructure; no Nansen API / Arkham API / Chainalysis
  wallet-intelligence in production hot path
- ADR 0005 Decision 2 — `Detector::supported_chains()` override pattern
**Related designs:**
- `docs/designs/0015-crates-graph-phase3.md` — Sprint 11 graph foundation; `GraphLabelStore`;
  `LabelType::SmartMoney` already declared in `crates/graph/src/labels.rs`
- `docs/designs/0003-detector-trait.md` — Detector trait + DetectorContext
- `docs/designs/0016-detector-09-bocpd-deployer-changepoint.md` — D09IndexerHook pattern;
  background-task-via-coordinator architecture reference
- `docs/designs/0017-d05-signal-b-graph-cycles.md` — Option D pattern: reads from tables
  directly, no new write path beyond the label store
- `docs/designs/0018-detector-11-synchronized-activity.md` — D11 stateless read pattern
- `docs/designs/0021-detector-13-sandwich-mev.md` — most recent detector spec; evidence
  prefix convention, fixture shape, suppression policy

**Binding prior art in REFERENCES.md:**
- Barras, Scaillet & Wermers 2010 (JoF 65(1)) — FDR skill/luck separation (Stage 2, blocked)
- Fantazzini & Xiao 2023 (Econometrics 11(3)) — informed-early-buyer timing window
- Fu, Feng, Wu & Xu 2025 (Perseus, arXiv:2503.01686) — cross-event recurrence threshold
- Easley, López de Prado & O'Hara 2012 (VPIN, RFS 25(5)) — informed-flow microstructure
- Nansen — secondary / market-color only; NOT the primary criterion

---

## §1 Background

### §1.1 Sprint 13 B-track investment

Sprint 13 opened two research tracks. Track A shipped the Tarjan SCC cycle detector (D05
Signal B replacement, design 0017). Track B produced `research/sprint13-b-citations.md` —
a full citation sweep for smart-money labelling and synchronized-activity clustering.
The citation sweep resolved the primary blocker (Nansen marketing reference was insufficient
for REFERENCES.md bar): Barras et al. 2010 (Journal of Finance), Fantazzini & Xiao 2023
(Econometrics), Perseus 2025 (UCL arXiv), and VPIN 2012 (Review of Financial Studies)
together form a defensible academic foundation.

Sprints 14-21 executed other tracks (EVM detectors D11-D13, server binary, Phase 5 USD
enrichment). Sprint 22 closes the B-track investment by shipping the smart-money labelling
MVP.

### §1.2 Why smart-money labelling matters

Smart-money labels are a cross-detector amplifier. A wallet with an established "smart money"
label entering a token is a strong prior that the token is worth watching — this is the
signal Perseus 2025 operationalizes at scale (438 confirmed masterminds, $3.2T in artificial
volume traced). Without these labels:
- D04 Pump & Dump cannot distinguish informed accumulation from organic demand
- D08 Sybil has no way to know whether a cluster contains sophisticated actors
- D05 Wash Trading cannot exclude known-smart-money round-trips from its PnL statistics
  (E-SM-2 evasion, §8 below)

The labelling pipeline is therefore infrastructure, not just a standalone signal.

### §1.3 Three-stage pipeline: why MVP ships Stages 1 + 3 only

The Sprint 13 framework (research/sprint13-b-citations.md §Task 2) defines three stages:

**Stage 1 — Realized PnL corpus:** Compute realized PnL per completed round-trip (entry +
exit on same token) per wallet. Aggregate per-wallet metrics: total realized PnL USD,
win rate, mean holding time, round-trip count.

**Stage 2 — FDR alpha separation (Barras et al. 2010):** Apply Benjamini-Hochberg FDR to
per-wallet alpha t-statistics computed over the Stage 1 corpus. Requires a minimum of
10 round-trips per wallet for t-statistic power (Barras et al. explicit requirement). At
Sprint 22, the indexer has not yet run in production — no live corpus exists. Stage 2 is
**data-blocked** until the live indexer has been running for at least 30 days and the corpus
reaches a meaningful population (target: ≥ 1,000 wallets with ≥ 10 round-trips).

**Stage 3 — Timing features (Fantazzini 2023 / Perseus 2025):** Pre-event timing advantage,
cross-event recurrence, sell-before-peak. These are computable from existing `swaps` and
`pool_events` tables without a PnL corpus of any particular depth. They provide an
immediate signal even with sparse history.

**MVP scope: Stages 1 + 3.** Stage 2 is annotated with `SPEC-NOTE: Stage 2 FDR
data-blocked; activate via smart_money_fdr_enabled = true when 30-day corpus ready.`

### §1.4 Self-sovereign constraint

ADR 0003 bans Nansen API, Arkham API, Chainalysis wallet-intelligence, or any third-party
labelling service in the production hot path. All labels must be derived from self-hosted
on-chain data. This design complies: the only data sources are the `swaps` and `pool_events`
tables populated by the self-hosted Yellowstone gRPC adapter.

---

## §2 Goals and Non-Goals

### §2.1 Goals

1. For each wallet with ≥ `min_round_trips` completed round-trips, compute a realized PnL
   corpus row (Stage 1) stored in `wallet_pnl_corpus` (V00016 migration, if Decision 4 picks
   materialized storage).
2. For each wallet passing Stage 1 PnL thresholds, compute timing features (Stage 3): median
   pre-event entry lead, cross-event recurrence count, sell-before-peak rate.
3. Emit `LabelType::SmartMoney` labels to `address_labels` for wallets meeting tier criteria
   (§5 Filters; §7 Tier criteria).
4. Annotate every label evidence with `"calibration": "heuristic, not FDR-controlled"` until
   Stage 2 unblocks (mandatory annotation per session brief).
5. Be deterministic: given identical `swaps` and `pool_events` table state, produce
   bit-identical label outputs.
6. Use `rust_decimal` for all monetary quantities; no `f64` for prices, PnL, or amounts.
7. Consume `TokenPriceProvider` (Sprint 21) for USD-denominated PnL computations.

### §2.2 Non-Goals

1. **Stage 2 FDR separation.** Not shipped. SPEC-NOTE placeholder in config.
2. **Zero-day smart-money detection.** A wallet with no round-trip history cannot be labelled
   here — this pipeline is retrospective. A new sophisticated actor is invisible until they
   have accumulated ≥ min_round_trips history.
3. **Social-graph features.** Telegram wallet correlation, Discord wallet correlation — off-chain
   data, excluded by ADR 0003 and project scope.
4. **Consumer integration.** Standalone service only (SESSION-KICKOFF gotcha #21).
5. **EVM chain support in MVP.** Solana-first per ADR 0001 §D1. EVM support is Phase 4 —
   this pipeline runs wherever the chain adapter and swap tables have data.
6. **Historical backfill.** The corpus populates as the indexer runs forward. No retroactive
   re-ingestion of pre-Sprint-22 swap history.
7. **Auto-promoting labels upstream.** Labels are written to `address_labels`; consumers
   read via `GraphLabelStore::addresses_with_label`. No push to bot-trader or custody.

---

## §3 Algorithm

### §3.1 Stage 1 — Realized PnL Corpus

**Definition of a round-trip:** a buy swap on token T followed by at least one sell swap on
token T by the same wallet, where the sell timestamp is after the buy timestamp, and the
matching is done FIFO (first-in first-out across multiple buys/sells). Partial sells produce
a partial round-trip; the closed portion is a completed round-trip.

**PnL for a single round-trip (all arithmetic in `rust_decimal`):**

```
let entry_qty: Decimal = buy_amount_tokens (decimal-adjusted, not raw)
let entry_price_usd: Decimal = TokenPriceProvider::get_token_price_usd(chain, token, buy_block_time)
let exit_qty: Decimal = min(sell_amount_tokens, entry_qty)  // FIFO partial-close
let exit_price_usd: Decimal = TokenPriceProvider::get_token_price_usd(chain, token, sell_block_time)

let pnl_usd: Decimal = (exit_price_usd - entry_price_usd) * exit_qty
let holding_time_secs: i64 = sell_block_time - buy_block_time  // both from block_time
```

**SPEC-NOTE:** When `TokenPriceProvider` returns `None` (new token, no price data), the
round-trip is recorded but `pnl_usd` is `NULL` in `wallet_pnl_corpus`. Aggregate PnL metrics
are computed over non-NULL round-trips only. Wallets where all round-trips have NULL PnL are
excluded from Stage 1 until price data populates.

**Per-wallet aggregate (one row in `wallet_pnl_corpus`):**

```
total_pnl_usd = SUM(pnl_usd) over completed round-trips with non-NULL pnl
win_rate = COUNT(pnl_usd > 0) / COUNT(pnl_usd IS NOT NULL)
mean_holding_time_secs = AVG(holding_time_secs) over completed round-trips
round_trip_count = COUNT(*) over all completed round-trips (including NULL-pnl)
non_null_pnl_count = COUNT(pnl_usd IS NOT NULL)
last_updated = MAX(sell_block_time)
```

**SQL projection (simplified; Decision 5 determines trigger):**

```sql
-- Corpus query (run per wallet or in batch, see Decision 5)
WITH buys AS (
    SELECT wallet, token, block_time AS buy_time,
           amount_out AS token_qty, -- tokens received
           ROW_NUMBER() OVER (PARTITION BY wallet, token ORDER BY block_time) AS seq
    FROM swaps
    WHERE chain = $1 AND token = $2 AND side = 'buy'
      AND block_time BETWEEN $lookback_start AND $lookback_end
),
sells AS (
    SELECT wallet, token, block_time AS sell_time,
           amount_in AS token_qty, -- tokens sold
           ROW_NUMBER() OVER (PARTITION BY wallet, token ORDER BY block_time) AS seq
    FROM swaps
    WHERE chain = $1 AND token = $2 AND side = 'sell'
      AND block_time BETWEEN $lookback_start AND $lookback_end
),
matched AS (
    -- FIFO matching: buy seq i pairs with sell seq i
    SELECT b.wallet, b.token,
           b.buy_time, s.sell_time,
           LEAST(b.token_qty, s.token_qty) AS closed_qty,
           b.seq
    FROM buys b
    JOIN sells s ON s.wallet = b.wallet AND s.token = b.token AND s.seq = b.seq
)
SELECT wallet, token,
       COUNT(*)                              AS round_trip_count,
       SUM(closed_qty * (exit_price - entry_price)) AS total_pnl_usd, -- prices from price_provider
       AVG(EXTRACT(EPOCH FROM (sell_time - buy_time))) AS mean_holding_time_secs,
       MAX(sell_time)                        AS last_updated
FROM matched
GROUP BY wallet, token
```

Note: price lookup cannot be expressed purely in SQL because `TokenPriceProvider` is a Rust
trait. The actual implementation fetches buy/sell rows in Rust, iterates the FIFO pairs, and
calls `price_provider.get_token_price_usd()` per pair. The SQL above illustrates the
projection shape; the Rust pseudocode below is authoritative.

**Rust pseudocode (Stage 1 core):**

```rust
async fn compute_wallet_pnl(
    wallet: &str,
    token: &str,
    swaps: &[SwapRow],         // pre-fetched from `swaps` table, ordered by block_time
    price_provider: &dyn TokenPriceProvider,
    chain: Chain,
) -> WalletPnlRow {
    let mut buys: VecDeque<OpenPosition> = VecDeque::new();
    let mut round_trips: Vec<RoundTrip> = Vec::new();

    for swap in swaps.iter().filter(|s| s.wallet == wallet && s.token == token) {
        match swap.side {
            Side::Buy => {
                let price = price_provider.get_token_price_usd(chain, token, swap.block_time).await;
                buys.push_back(OpenPosition { qty: swap.token_qty_decimal(), price, block_time: swap.block_time });
            }
            Side::Sell => {
                let exit_price = price_provider.get_token_price_usd(chain, token, swap.block_time).await;
                let mut remaining_sell = swap.token_qty_decimal();
                while remaining_sell > Decimal::ZERO {
                    let Some(mut pos) = buys.pop_front() else { break };
                    let closed = remaining_sell.min(pos.qty);
                    let pnl_usd = match (pos.price, exit_price) {
                        (Some(ep), Some(xp)) => Some((xp - ep) * closed),
                        _ => None,
                    };
                    round_trips.push(RoundTrip { closed_qty: closed, pnl_usd, holding_secs: (swap.block_time - pos.block_time).num_seconds() });
                    remaining_sell -= closed;
                    if pos.qty > closed {
                        pos.qty -= closed;
                        buys.push_front(pos); // remainder stays
                    }
                }
            }
        }
    }

    WalletPnlRow {
        wallet: wallet.to_string(),
        token: token.to_string(),
        round_trip_count: round_trips.len() as i64,
        non_null_pnl_count: round_trips.iter().filter(|r| r.pnl_usd.is_some()).count() as i64,
        total_pnl_usd: round_trips.iter().filter_map(|r| r.pnl_usd).fold(Decimal::ZERO, |a, b| a + b),
        win_rate: {
            let non_null: Vec<_> = round_trips.iter().filter_map(|r| r.pnl_usd).collect();
            if non_null.is_empty() { None }
            else {
                let wins = non_null.iter().filter(|&&p| p > Decimal::ZERO).count();
                Some(Decimal::from(wins) / Decimal::from(non_null.len()))
            }
        },
        mean_holding_time_secs: if round_trips.is_empty() { None }
            else { Some(Decimal::from(round_trips.iter().map(|r| r.holding_secs).sum::<i64>()) / Decimal::from(round_trips.len())) },
        last_updated: round_trips.iter().map(|r| r.sell_block_time).max().unwrap_or_default(),
    }
}
```

### §3.2 Stage 3 — Timing Features

Stage 3 consumes the pump event index (D04 anomaly events from `anomaly_events` table where
`detector_id = 'pump_dump_v1'` and `confidence >= pump_event_min_confidence`) as the set of
"known pump events." For each known pump event on token T:

**Feature A — Pre-event timing lead:**

```
event_peak_time = block_time of the AnomalyEvent emission from D04
wallet_entry_time = MIN(block_time) for swaps WHERE wallet = W AND token = T AND side = 'buy'
                    AND block_time BETWEEN (event_peak_time - pre_event_window_secs) AND event_peak_time
timing_lead_secs = event_peak_time - wallet_entry_time
    (NULL if wallet has no buy in the pre-event window)
```

The `pre_event_window_secs` default is 3600 (60 minutes). This is anchored in Fantazzini &
Xiao 2023: "pre-announcement buyers statistically distinct from post-announcement buyers
within a 60-minute window."

**Feature B — Cross-event recurrence:**

```
recurrence_count = COUNT(DISTINCT pump_event_id)
    WHERE wallet W has timing_lead_secs IS NOT NULL
    over the past recurrence_lookback_days
```

Perseus 2025 establishes that mastermind wallets appear in pre-event windows across ≥ 3
distinct events. A single event "smart" entry is luck. Three events is 0.001 under
independence (assuming 10% base rate of a random wallet entering the correct window).

**Feature C — Sell-before-peak:**

```
for each pump event E on token T where wallet W has a buy in the pre-event window:
    peak_price_time = block_time of MAX(price_usd) within event window
    wallet_exit_time = MIN(block_time) for swaps WHERE wallet = W AND token = T AND side = 'sell'
                       AND block_time <= (peak_price_time + post_peak_grace_secs)
    sell_before_peak = (wallet_exit_time < peak_price_time)
```

`sell_before_peak_rate = COUNT(sell_before_peak = true) / COUNT(events with an exit)`

Perseus 2025 documents that masterminds consistently exit before the price peak. A retail
participant who follows the pump typically exits after the peak (during distribution).

**Per-wallet timing summary (aggregated across all known pump events):**

```rust
struct WalletTimingFeatures {
    wallet: String,
    chain: String,
    recurrence_count: i64,           // COUNT of distinct pump events with pre-event entry
    median_timing_lead_secs: Option<Decimal>,  // median of timing_lead_secs over events
    timing_lead_percentile_rank: Option<Decimal>, // rank vs all wallets in those events (0.0–1.0)
    sell_before_peak_rate: Option<Decimal>,    // fraction of exited events sold before peak
    events_evaluated: i64,           // total pump events considered
}
```

### §3.3 Label Assignment

After computing Stage 1 (PnL corpus) and Stage 3 (timing features), the labeller classifies
each wallet into a tier and writes a `LabelType::SmartMoney` label.

**Tier assignment (Decision 3 thresholds; see §7 for derivation):**

```rust
fn assign_tier(pnl: &WalletPnlRow, timing: Option<&WalletTimingFeatures>, cfg: &SmartMoneyCfg) -> Option<SmartMoneyTier> {
    // Tier 1: strong realized PnL + timing alpha confirmed
    if pnl.non_null_pnl_count >= cfg.min_round_trips_for_tier1
        && pnl.win_rate.unwrap_or_default() >= cfg.tier1_win_rate_floor
        && pnl.total_pnl_usd >= cfg.tier1_pnl_floor_usd
        && timing.map_or(false, |t| t.recurrence_count >= cfg.min_event_recurrence_tier1)
        && timing.map_or(false, |t| t.timing_lead_percentile_rank.unwrap_or_default() >= cfg.timing_lead_percentile_threshold)
    {
        return Some(SmartMoneyTier::Tier1);
    }
    // Tier 2: strong PnL OR 2+ event recurrence (either criterion, not both required)
    if pnl.non_null_pnl_count >= cfg.min_round_trips
        && (pnl.total_pnl_usd >= cfg.tier2_pnl_floor_usd
            || timing.map_or(false, |t| t.recurrence_count >= cfg.min_event_recurrence_tier2))
    {
        return Some(SmartMoneyTier::Tier2);
    }
    // Tier 3: sufficient round-trips + positive realized PnL (directional edge only)
    if pnl.round_trip_count >= cfg.min_round_trips
        && pnl.total_pnl_usd > Decimal::ZERO
    {
        return Some(SmartMoneyTier::Tier3);
    }
    None
}
```

---

## §4 Label Confidence Math

**MANDATORY annotation:** All confidence values emitted by this pipeline carry the annotation
`"calibration": "heuristic, not FDR-controlled"` in the evidence JSON until Stage 2 unblocks.
This is required in both the label evidence payload and the config doc comment.

### §4.1 Tier → base confidence

| Tier | Base confidence | Rationale |
|------|----------------|-----------|
| Tier 1 | 0.70 | PnL + timing recurrence simultaneously satisfied; heuristic floor |
| Tier 2 | 0.50 | Single criterion (PnL OR recurrence); more uncertain |
| Tier 3 | 0.30 | Positive PnL only; weakest signal; useful for downstream filtering |

### §4.2 Bonus adjustments

All bonuses additive; final value clamped to [0.0, 0.90].

Cap at 0.90 (not 0.95) reflects that without FDR correction (Stage 2), a systematic lucky
streak in a short corpus period cannot be ruled out. FDR-corrected labels in Stage 2 will
lift the cap to 0.95.

**Bonuses:**

```
sell_before_peak_bonus = 0.10 if sell_before_peak_rate >= 0.70
    // Perseus 2025: consistent sell-before-peak is the strongest distinguisher of
    // masterminds from lucky participants
recurrence_bonus = MIN(0.10, (recurrence_count - min_required) * 0.03)
    // +0.03 per additional confirmed event beyond minimum; capped at 0.10
holding_time_bonus = 0.05 if mean_holding_time_secs BETWEEN 300 AND 86400
    // Short-but-not-zero holds (5 min to 24h) indicate tactical trading rather
    // than airdrop farming (hours-old wallets) or bagholding
pnl_scale_bonus = MIN(0.05, total_pnl_usd.log10() * 0.02 - 0.02) if total_pnl_usd >= 1000
    // Larger absolute PnL lifts confidence marginally; bounded contribution
```

**Final:**

```rust
let conf_raw = base + sell_before_peak_bonus + recurrence_bonus + holding_time_bonus + pnl_scale_bonus;
let confidence = conf_raw.clamp(Decimal::ZERO, Decimal::from_str("0.90").unwrap());
```

### §4.3 Stage 2 FDR confidence (future)

When `smart_money_fdr_enabled = true`, Stage 2 replaces the heuristic base with a
calibrated Barras et al. (2010) alpha t-statistic mapped through a sigmoid:

```
// Stage 2 (not shipped in Sprint 22)
let t_stat = wallet_alpha / wallet_alpha_se;  // from Barras et al. §3
let fdr_confidence = sigmoid((t_stat - fdr_t_threshold) / fdr_scale);
// bonuses from §4.2 still apply; cap raised to 0.95
// annotation updated to: "calibration": "FDR-controlled (Barras 2010)"
```

---

## §5 Filters

### §5.1 Minimum round-trips

`min_round_trips = 10` (primary; recommended default).

**Derivation:** Barras et al. 2010 explicitly notes that alpha t-statistics are unreliable
for fund managers with fewer than 10 return observations. At fewer than 10 round-trips, the
win-rate estimate has standard error of 0.5/sqrt(10) ≈ 0.16, meaning a wallet with a true
win-rate of 0.5 routinely appears at 0.66 or 0.34. Labels derived from fewer than 10
round-trips carry substantial luck component that Stage 2 FDR would reject.

The MVP ships `min_round_trips = 10` as the default. A configurable floor of 5 is available
for operators who accept higher noise in sparse-corpus periods:

```toml
[smart_money_v1]
min_round_trips = 10          # Barras 2010 power requirement; lower to 5 with explicit noise acceptance
```

**When `non_null_pnl_count < min_round_trips` but `round_trip_count >= min_round_trips`:**
Wallet is NOT labelled (insufficient price data). SPEC-NOTE logged per wallet; no label
written. This prevents NULL-contaminated PnL rows from producing spurious labels.

### §5.2 Minimum PnL by tier

```toml
tier1_pnl_floor_usd = "10000"   # $10K realized PnL; meaningful absolute alpha
tier2_pnl_floor_usd = "1000"    # $1K; directional but smaller edge
```

No floor for Tier 3 beyond `total_pnl_usd > 0`. Tier 3 is a candidate pool, not a label
with authority.

**USD denominations are `rust_decimal`; stored as `NUMERIC(20,4)` in V00016.**

### §5.3 Known-good allowlist (CEX hot wallets, DEX programs, vesting)

Addresses carrying `LabelType::KnownExchange` or `LabelType::KnownDex` from `address_labels`
are **excluded** from smart-money labelling. A Binance hot wallet that earns positive PnL on
every trade is not "smart money" — it is an exchange operator.

**Exclusion query:**

```sql
SELECT address FROM address_labels
WHERE chain = $1
  AND label_type IN ('KnownExchange', 'KnownDex', 'KnownBurn')
  AND (expires_at IS NULL OR expires_at > now())
```

The exclusion check runs at corpus-computation time, not at label-write time. Wallets in the
exclusion set are filtered before the FIFO PnL computation starts.

### §5.4 Minimum win-rate floor for Tier 1

`tier1_win_rate_floor = 0.55` (55% win rate on closed round-trips).

A 55% win rate is substantially above the 50% breakeven, but not so high that it could only
result from market-making on liquid tokens (which have near-100% win rates on thin spreads).
No academic citation for this exact value; classified `unverified-heuristic` in config
comment. The Stage 2 FDR procedure replaces this floor with a statistically grounded test.

### §5.5 Suppression

Smart-money is a **positive label**, not a negative anomaly. The `is_established_protocol`
suppression logic used by D04/D06 is **not applied** here. An EOA that has demonstrated
timing alpha on established-protocol tokens (BONK, WIF, JUP, RAY) is still smart money.
Suppressing the label on established protocols would remove the most credible evidence of
genuine skill (since established tokens have deeper markets where "luck" is harder to sustain).

---

## §6 Integration

### §6.1 Decision 1: Integration model recommendation

**Recommendation: pipeline-as-Background-Task via periodic batch job, NOT Detector trait.**

**Rationale:**

The `Detector` trait (design 0003) is designed for per-event evaluation: `fn evaluate(&ctx)
-> Vec<AnomalyEvent>`. Smart-money labelling is a retrospective batch computation that:
1. Requires scanning the entire `swaps` table for each wallet (not a single token's event
   stream at a single block height)
2. Produces `AddressLabel` rows (not `AnomalyEvent` rows) as its primary output
3. Is triggered by the passage of time and accumulation of swap history, not by a specific
   on-chain event

This mismatch is the same that motivated D09's `D09IndexerHook` for pool-initialize events
(design 0016 §6), but more extreme. D09 still fits the per-event Indexer hook because it
reacts to each `PoolEvent::Initialize`. The smart-money pipeline has no natural per-event
trigger — it is a population-level computation.

D08 is the closest existing analogue that writes `AddressLabel` rows via `GraphLabelStore`.
D08 is a cadenced streaming detector (not a per-event hook) that runs on a schedule against
accumulated holder data. The smart-money pipeline is similar in character but longer-running:
D08 completes in seconds; the smart-money pipeline at scale may take minutes to hours.

**Architecture:** a `SmartMoneyLabeller` struct with a `run_batch()` async method, spawned
by the `MultiChainCoordinator` as a background task with a configurable interval. This
matches the generic Coordinator model (ADR 0005 Decision 1) without requiring a new
architectural pattern.

```rust
/// Background task spawned by MultiChainCoordinator.
/// NOT a Detector trait implementation.
pub struct SmartMoneyLabeller {
    pg_pool: PgPool,
    label_store: Arc<dyn GraphLabelStore>,
    price_provider: Arc<dyn TokenPriceProvider>,
    cfg: SmartMoneyConfig,
}

impl SmartMoneyLabeller {
    /// Entry point called by the Coordinator on each batch interval.
    pub async fn run_batch(&self, chain: Chain) -> anyhow::Result<SmartMoneyBatchStats> {
        // 1. Fetch active wallets (any swap in the last corpus_lookback_days)
        // 2. For each wallet: compute WalletPnlRow (Stage 1)
        //    - Only wallets with round_trip_count >= min_round_trips proceed
        // 3. Compute WalletTimingFeatures (Stage 3) for Stage-1-passing wallets
        //    using pump events from anomaly_events WHERE detector_id = 'pump_dump_v1'
        // 4. Assign tiers → confidence scores
        // 5. Write labels via label_store.upsert_labels()
        // Return stats: wallets_evaluated, labels_written, labels_refreshed
    }
}
```

### §6.2 Event consumption

**Inputs (read-only):**
- `swaps` table: buy/sell swaps with `(chain, wallet, token, side, amount_in, amount_out, block_time)`.
  Filtering: `chain = $1 AND block_time >= NOW() - INTERVAL '$corpus_lookback_days days'`.
- `anomaly_events` table: D04 pump events as the "known pump event" index for Stage 3.
  Filtering: `detector_id = 'pump_dump_v1' AND confidence >= pump_event_min_confidence`.
- `address_labels` table: `KnownExchange`/`KnownDex`/`KnownBurn` exclusion list.

**Output:**
- `address_labels` table: `LabelType::SmartMoney` rows written via `GraphLabelStore::upsert_labels()`.
- `wallet_pnl_corpus` table (V00016, if Decision 4 selects materialized storage): one row
  per `(chain, wallet, token)` with PnL corpus metrics.

### §6.3 Label write pattern (mirroring D08)

```rust
let label = AddressLabel {
    chain: chain.to_string(),
    address: wallet.clone(),
    label_type: LabelType::SmartMoney,
    confidence: confidence.to_f64().unwrap(),  // f64 is correct: probability, not money
    evidence: serde_json::json!({
        "smart_money/tier":                     tier_str,           // "tier1" / "tier2" / "tier3"
        "smart_money/total_pnl_usd":            pnl_row.total_pnl_usd.to_string(),
        "smart_money/win_rate":                 pnl_row.win_rate.map(|v| v.to_string()),
        "smart_money/round_trip_count":         pnl_row.round_trip_count,
        "smart_money/recurrence_count":         timing.recurrence_count,
        "smart_money/median_timing_lead_secs":  timing.median_timing_lead_secs.map(|v| v.to_string()),
        "smart_money/sell_before_peak_rate":    timing.sell_before_peak_rate.map(|v| v.to_string()),
        "smart_money/timing_lead_percentile":   timing.timing_lead_percentile_rank.map(|v| v.to_string()),
        "smart_money/mean_holding_time_secs":   pnl_row.mean_holding_time_secs.map(|v| v.to_string()),
        "smart_money/per_token_pnl":            per_token_pnl_map,  // cross-token evidence (Decision 9)
        "calibration":                          "heuristic, not FDR-controlled",
        "stage2_blocked_reason":                "live corpus < 30 days; activate via smart_money_fdr_enabled = true",
    }),
    issued_at: last_batch_run_time,  // wall-clock OK for background job (not indexer path)
    expires_at: Some(last_batch_run_time + Duration::hours(cfg.label_ttl_hours as i64)),
    source: "smart_money_labeller_v1".to_string(),
};
```

**Evidence prefix: `smart_money/` per gotcha #9.**

### §6.4 TTL and refresh

Labels have TTL = `label_ttl_hours` (default: 720h / 30 days). The batch job runs every
`batch_interval_hours` (default: 6h). On each run, upsert logic updates confidence and
evidence only when the incoming confidence ≥ existing label confidence OR the label has
expired. This prevents a noisy low-confidence run (sparse corpus period) from overwriting
a well-established Tier 1 label.

### §6.5 Wiring in `crates/server/src/init/`

The `SmartMoneyLabeller` is constructed in `init::detectors` (or a new
`init::labellers` sub-module if the developer prefers to separate it from the anomaly
detector registry). The `build_all_detectors` function already receives `label_store`
and `price_provider` as parameters — the same arguments needed here. The Coordinator
spawns the `run_batch()` loop as a `tokio::spawn` task with a `tokio::time::interval`.

---

## §7 Threshold Calibration

### §7.1 Tier 1 thresholds

| Parameter | Value | Source |
|-----------|-------|--------|
| `min_round_trips_for_tier1` | 10 | Barras et al. 2010: t-statistic power minimum |
| `tier1_win_rate_floor` | 0.55 | Unverified-heuristic; Stage 2 FDR replaces |
| `tier1_pnl_floor_usd` | $10,000 | Nansen secondary (market-color); no academic anchor; Stage 2 to validate |
| `min_event_recurrence_tier1` | 3 | Perseus 2025: 438 masterminds all appeared ≥ 3× in pre-event windows |
| `timing_lead_percentile_threshold` | 0.90 | Sprint 13 framework recommendation: top-10% earliest entries; conservative cutoff to avoid FP on lucky early retail |

**Perseus 2025 recurrence threshold derivation:**
Under independence with 10% base rate of any wallet entering a 60-min pre-event window by
chance: P(3+ events) = (0.10)^3 = 0.001. At 1M monitored wallets this produces ~1,000
false positives — acceptable for a heuristic label that is downstream-filtered. At
recurrence = 2: ~10,000 FPs. At recurrence = 1: ~100,000 FPs (unusable). Threshold 3 is
the minimum defensible value; 4+ would further reduce FPs but also cuts recall on real
masterminds.

### §7.2 Tier 2 thresholds

| Parameter | Value | Source |
|-----------|-------|--------|
| `min_round_trips` | 10 | Same as Tier 1 minimum |
| `tier2_pnl_floor_usd` | $1,000 | Heuristic; meaningful directional edge without requiring top-tier returns |
| `min_event_recurrence_tier2` | 2 | Heuristic lower bound; more permissive than Tier 1 |

### §7.3 Tier 3 thresholds

| Parameter | Value | Source |
|-----------|-------|--------|
| `min_round_trips` | 10 | Barras 2010 minimum applies equally |
| `total_pnl_usd > 0` | Positive | Weakest possible filter; any net gain |

**Tier 3 justification:** a candidate pool for future FDR analysis. Tier 3 wallets with
≥ 10 round-trips and positive PnL are the population Barras et al. 2010 Stage 2 will
analyze. Labelling them at Tier 3 (confidence 0.30) makes them discoverable by consumers
without claiming skill.

### §7.4 Timing lead percentile (90th)

The Sprint 13 framework recommendation is the 90th percentile of pre-event entry time within
the wallet population participating in the same pump events. This is a **relative** threshold
(not absolute minutes) calibrated against the observed cohort for each event, then aggregated
as a median percentile rank across all events for the wallet. A wallet that is in the top 10%
earliest across multiple events is unlikely to be coincidentally early.

Fantazzini & Xiao 2023 validate that pre-event buyers are statistically distinguishable from
the full buyer population within the 60-minute window — the timing lead percentile is this
paper's feature operationalized as a continuous label input.

### §7.5 Stage 2 FDR parameters (SPEC-NOTE: not activated)

When `smart_money_fdr_enabled = true` (post-30-day corpus):

| Parameter | Value | Source |
|-----------|-------|--------|
| FDR q threshold | 0.10 | Sprint 13 framework; more permissive than standard 0.05 because downstream use is filtering not hypothesis publication |
| Min round-trips for FDR | 10 | Barras 2010 explicit requirement |
| Alpha benchmark | cohort-matched (same token set, same period) | Barras 2010 §3 cross-sectional benchmark |

---

## §8 Evasion Analysis

### E-SM-1: Wallet split (fragmenting PnL across addresses)

**Attack:** A sophisticated actor splits their trading across N wallets, each accumulating
fewer than `min_round_trips` round-trips or smaller PnL than the tier floors. No single
wallet reaches Tier 1 threshold.

**Detection coverage:** D08 Sybil (common-funder clustering) may group the wallets into a
cluster if they share a funder. Smart-money labels on the cluster's aggregate PnL (rather
than per-wallet) is a Phase 5 enhancement. At MVP, this evasion is acknowledged but not
closed. Cross-reference: D08 confidence amplification when a Sybil cluster has cumulative
smart-money signals is listed in §10 as a future integration.

**Cost of evasion:** Splitting across N wallets multiplies gas costs by N, requires N
separate fund flows (detectable by D08), and reduces each wallet's PnL signal. At N = 5,
each wallet needs $2,000 PnL to achieve what one wallet does at $10,000. Non-trivial
coordination cost.

### E-SM-2: Fake PnL via wash trading

**Attack:** An actor self-deals between two controlled wallets to inflate the "smart money"
wallet's PnL. Wallet A buys token T; wallet B sells the same token at a higher (self-set)
price in an internal trade or via a thin pool. Wallet A shows large realized PnL.

**Detection coverage:** D05 Wash Trading (Signal A: same-address round-trips; Signal B: cycle
detection) should catch this if the wash trades flow through the pool. Cross-token wash is
harder to detect.

**Critical mitigation:** The smart-money labeller MUST exclude wallets with active
`wash_trading_v1` `AnomalyEvent` entries (confidence ≥ 0.70) from label computation. Add a
filter: before computing WalletPnlRow, check `anomaly_events WHERE detector_id =
'wash_trading_v1' AND confidence >= 0.70 AND block_time >= NOW() - INTERVAL '30 days'`. This
cross-detector dependency is explicit in §10.

### E-SM-3: Borrowed PnL (token gifts / airdrops)

**Attack:** An actor receives a large token airdrop as a "gift" (zero cost basis), then sells
at market price. Realized PnL is large but reflects no skill — just free tokens.

**Mitigation:** Filter out `amount_in = 0` buys (buys with no cost, i.e., token credits not
preceded by a swap). In the FIFO pairing, an entry with `entry_price = 0` or `entry_qty`
credited via transfer (not a swap) should be tagged as `airdrop_entry = true` and excluded
from PnL computation. This requires joining the `swaps` table with `transfers` to detect
non-swap token credits to the wallet.

**In MVP:** Partial mitigation only. Stage 1 computes PnL over `swaps` table entries only
(swaps have non-zero `amount_in`). Pure airdrops do not appear in the `swaps` table. Mixed
airdrop-and-sell (receive via airdrop, sell via swap) will show zero cost-basis on the sell
side and inflate PnL. The FIFO algorithm assigns any unmatched sell to `airdrop_entry = true`
if there is no corresponding buy swap. SPEC-NOTE: full airdrop-entry filtering is a Sprint
23 enhancement.

### E-SM-4: Time-limited smart money (flash edge)

**Attack:** An actor earns genuine timing alpha during a specific market regime (e.g.,
Solana memecoin mania, November–January) then loses edge. The label persists at Tier 1
due to historical PnL even though the wallet is no longer "smart."

**Mitigation:** TTL = 720h (30 days). Labels expire and must be re-earned. The `corpus_lookback_days`
(default: 90 days) controls the evidence window. If a wallet had a great Q4 but underperforms
Q1, confidence on the new run drops and the label either degrades or is not renewed (upsert
logic: new confidence must be >= existing confidence to overwrite).

---

## §9 Config Keys

All keys under `[smart_money_v1]` in `config/detectors.toml`. Every key must have a
REFERENCES.md-linked rationale comment.

```toml
[smart_money_v1]
# ---- Stage 1: PnL corpus ----

# Minimum completed round-trips for a wallet to enter the corpus.
# Barras et al. 2010 JoF 65(1): below 10, alpha t-statistic has insufficient power.
# Lower to 5 only if accepting higher heuristic noise in sparse-corpus periods.
# CALIBRATION: "heuristic, not FDR-controlled" until smart_money_fdr_enabled = true
min_round_trips = 10

# Number of days of swap history to include in the corpus window.
corpus_lookback_days = 90

# Minimum non-null-priced round-trips required before computing PnL metrics.
# If non_null_pnl_count < min_round_trips, the wallet is skipped (no label written).
min_priced_round_trips = 10

# ---- Tier criteria ----

# Tier 1: strong PnL + timing alpha (heuristic; Stage 2 FDR replaces)
tier1_pnl_floor_usd = "10000"           # USD; rust_decimal string encoding
tier1_win_rate_floor = "0.55"           # unverified-heuristic; Stage 2 FDR replaces
min_round_trips_for_tier1 = 10          # same as min_round_trips

# Tier 2: PnL OR recurrence (one of two criteria)
tier2_pnl_floor_usd = "1000"            # USD; heuristic

# ---- Stage 3: Timing features ----

# Pre-event window for "informed early buyer" classification (seconds).
# Fantazzini & Xiao 2023 Econometrics 11(3): 60-minute pre-announcement window.
pre_event_window_secs = 3600            # 60 minutes

# Minimum number of distinct pump events where a wallet appeared in the pre-event window.
# Tier 1: Perseus 2025 arXiv:2503.01686: all 438 masterminds recurred >= 3 times.
# Tier 2: heuristic lower bound.
min_event_recurrence_tier1 = 3
min_event_recurrence_tier2 = 2

# Lookback for pump event recurrence counting (days).
recurrence_lookback_days = 90

# Timing lead percentile threshold for Tier 1: top-10% earliest entries.
# Sprint 13 framework recommendation; Fantazzini & Xiao 2023 operationalization.
timing_lead_percentile_threshold = "0.90"   # 90th percentile

# Minimum D04 confidence for a pump event to be included in the Stage 3 event index.
pump_event_min_confidence = "0.60"

# Post-peak grace period for sell-before-peak classification (seconds).
# Wallets that exit within this window of the peak are counted as "at peak" not "after peak."
post_peak_grace_secs = 300              # 5 minutes

# ---- Stage 2 FDR (NOT ACTIVATED in Sprint 22) ----

# SPEC-NOTE: Stage 2 FDR is data-blocked. Requires >= 30-day live indexer corpus.
# When corpus is ready, set to true and provide fdr_q_threshold.
# Citation: Barras, Scaillet & Wermers 2010 JoF DOI 10.1111/j.1540-6261.2009.01527.x
smart_money_fdr_enabled = false

# FDR q-value threshold (Benjamini-Hochberg procedure).
# Barras et al. use q = 0.05 for publications; 0.10 used here because downstream use
# is ranking/filtering, not hypothesis testing.
fdr_q_threshold = "0.10"

# Minimum round-trips for FDR t-statistic to be meaningful.
# Barras et al. 2010 explicit requirement (same as general min_round_trips).
min_round_trips_fdr = 10

# ---- Infrastructure ----

# Batch interval (hours). Background job runs this often.
batch_interval_hours = 6

# Label TTL (hours). Labels expire and must be re-earned on each batch.
label_ttl_hours = 720                   # 30 days

# Wallets with active wash_trading_v1 anomaly events above this confidence are excluded.
# Cross-detector evasion guard (E-SM-2 in design 0022 §8).
wash_trading_exclusion_confidence = "0.70"
```

---

## §10 Cross-Detector Coverage

### §10.1 D08 Sybil consuming smart-money labels

**Current state:** D08 does not consume smart-money labels. The label store write in D08
is one-directional (D08 → `Sybil` labels).

**Proposed integration (Phase 5):** In `D08SybilDetector::evaluate`, after computing Signal A
overlap, fetch `SmartMoney` labels for cluster members. If the cluster contains ≥ 1 Tier 1
smart-money wallet, the cluster is more likely a deliberate operation (not a naive launch
botnet). Confidence amplifier: add 0.05 to Signal B output when Tier 1 smart-money is
present in the cluster.

**Implementation note:** `D08SybilDetector` already accepts `Arc<dyn GraphLabelStore>`.
Adding a label lookup is a single method call, no new dependency.

### §10.2 D04 Pump & Dump amplification

**Current state:** D04 Signal C (insider sell amplifier) fires when addresses in
`deployer_clusters` are selling during the price spike. This is a deployer-identity check,
not a smart-money label check.

**Proposed integration (Phase 5):** Augment Signal C to also amplify when smart-money
(Tier 1 or Tier 2) wallets are buying in the pre-event window. Rationale: if wallets with
established timing alpha are accumulating before the detected spike, the spike is more likely
a coordinated pump (not organic demand).

**Risk of feedback loop:** D04 anomaly events are used as the Stage 3 "known pump event"
index. D04 confidence feeding back into smart-money confidence, which feeds back into D04
amplification, is a potential stability issue. Mitigation: break the cycle by using a
time-delayed smart-money label (labels only apply to events that occur AFTER the label's
`issued_at`).

### §10.3 D05 Wash Trading exclusion of smart-money PnL

**Current state:** D05 Signal A (same-address round-trips) correctly fires on self-dealing.
D05 Signal B (cycle detection) fires on ring trades. Neither currently distinguishes
smart-money round-trips from wash-trading round-trips.

**Required cross-detector check:** The smart-money labeller (E-SM-2) must exclude wallets
with active `wash_trading_v1` anomaly events. Conversely, D05 Signal A should NOT automatically
discount a round-trip just because the wallet has a smart-money label — smart money may also
wash trade. The exclusion is unidirectional: wash-trading suspicion blocks smart-money labels;
smart-money labels do not suppress wash-trading signals.

### §10.4 D09 BOCPD Deployer Changepoint

No direct dependency. Smart-money labels are about buyer wallets; D09 tracks deployer
behavior. Indirect: if smart-money wallets consistently appear on tokens deployed by the
same deployer, this is a correlated signal (D09's deployer composite score + smart-money
co-occurrence) that could be surfaced at the scoring layer. Phase 5 work.

---

## §11 Decisions Requiring Sign-Off

### Decision 1: Integration model
**Options:**
- A) Pipeline-as-Detector (Detector trait impl, returns AnomalyEvent, label write as side-effect — like D08)
- B) Pipeline-as-Background-Task (periodic batch job, no Detector trait, Coordinator-spawned) ← **RECOMMENDED**
- C) Pipeline-as-Background-Task-via-Indexer-Hook (D09IndexerHook analog, fires per PoolEvent::Initialize)

**Recommendation: B.** See §6.1. The computation is population-level and time-triggered, not
per-event. D08 writes labels from a Detector but only because it naturally slots into the
cadenced streaming scheduler; the smart-money pipeline is heavier and needs explicit batch
control.

**Trade-off:** Option B does not surface in the `anomaly_events` table (no `AnomalyEvent`
emitted). Consumers must query `address_labels` directly. Option A would emit a low-severity
`Info` event alongside the label, making the labelling activity visible in the event stream
without adding analytical value. If event-stream visibility is desired, a thin adapter can
emit a synthetic Info event after each batch.

### Decision 2: Label schema
**Options:**
- A) Single `LabelType::SmartMoney` with tier encoded in evidence JSON ← **RECOMMENDED**
- B) `LabelType::SmartMoney(SmartMoneyTier)` — new enum variant with inner tier data

**Recommendation: A.** `LabelType::SmartMoney` is already declared in
`crates/graph/src/labels.rs` (Sprint 11 forward-declaration). Adding a tuple variant would
change the enum from a unit-variant type to a mixed type, breaking the `as_db_str` /
`from_db_str` round-trip (which currently uses fixed string constants). The `TEXT` column in
`address_labels` would need to encode both the type and the tier ("SmartMoneyTier1",
"SmartMoneyTier2", "SmartMoneyTier3") — which is a new string mapping not covered by the
existing `from_db_str` implementation. Option A stores tier in evidence JSON, which is
already a freeform `serde_json::Value`. No schema change required.

**Change required (crates/graph only):** None for the label schema itself. A new
`SmartMoneyTier` enum in the `smart_money` module (`crates/detectors` or a new
`crates/labellers`) is the correct home for the Rust-side tier logic; it does not enter
`LabelType`.

### Decision 3: Tier criteria thresholds
**See §7 for full derivation.** Summary of recommended defaults:

| Tier | PnL floor | Win rate | Recurrence | Timing percentile |
|------|-----------|----------|------------|-------------------|
| Tier 1 | $10,000 | 55% | ≥ 3 events | top 10% |
| Tier 2 | $1,000 | none | ≥ 2 events OR PnL floor | top 10% optional |
| Tier 3 | > $0 | none | none | none |

All thresholds configurable under `[smart_money_v1]`. Annotated `heuristic, not FDR-controlled`.

### Decision 4: Storage tier for wallet_pnl_corpus
**Options:**
- A) Materialized `wallet_pnl_corpus` table (V00016 migration) ← **RECOMMENDED**
- B) Computed-on-demand from `swaps` (no new table)

**Recommendation: A.** At 30-day corpus depth, the active wallet population on a high-volume
Solana token set is estimated at 100K–1M wallets. For each batch run, computing FIFO PnL
from scratch over the full `swaps` table for all wallets would require scanning millions of
rows per run (every 6 hours). A materialized `wallet_pnl_corpus` table with incremental
updates (only wallets with new swap activity since the last batch update their row) reduces
per-run query cost by 10–100× at the cost of one additional table.

**The materialized approach mirrors Sprint 21 `PgTokenPriceProvider`:** store derived results
in Postgres, invalidate on schedule. The difference is the batch scope (per-wallet-token vs
per-token-price).

**Migration V00016 is the next available number** (confirmed from SESSION-KICKOFF gotcha #31).

### Decision 5: Trigger model
**Options:**
- A) Realtime per-swap incremental (every processed swap updates the affected wallet's corpus row)
- B) Periodic batch job every 6 hours ← **RECOMMENDED**
- C) Hybrid (realtime corpus updates + periodic label re-scoring)

**Recommendation: B.** Realtime per-swap incremental (Option A) adds a synchronous write to
the hot indexing path for every swap event. At Solana mainnet throughput, this is a 30K/sec
write amplifier that would bottleneck the indexer. The smart-money labelling use case is
retrospective by nature — a label assigned 6 hours ago is as actionable as one assigned 6
minutes ago. Option B at 6-hour intervals gives 4 batch runs per day with manageable
incremental update scope. Option C (hybrid) adds implementation complexity without a clear
latency requirement — skip for MVP.

### Decision 6: Min round-trips
**Recommendation: 10 (configurable floor: 5).**
See §5.1. Barras 2010 is explicit. Ship 10 as default. Operators can lower to 5 if they
understand the noise trade-off. Do not lower below 5 without an explicit data-calibrated
justification.

### Decision 7: Timing-lead percentile
**Recommendation: 90th percentile (top-10% earliest entries), as proposed by Sprint 13
framework.**
This is not re-opened for sign-off — it is baked into the config key
`timing_lead_percentile_threshold = "0.90"`. The decision is whether to maintain the Sprint
13 recommendation or change it. **Maintain 90th percentile.**

Rationale: the percentile is relative to the cohort of wallets that participated in the same
pump events. A 90th percentile cutoff selects 1 in 10 wallets — at a recurrence requirement
of ≥ 3 events, the combined probability under independence is (0.10)^3 = 0.001. This is
the statistical anchor that makes the threshold defensible without FDR.

### Decision 8: Stage 2 FDR activation mechanism
**Options:**
- A) Config flag `smart_money_fdr_enabled = false` (default) → operator flips to `true`
  after validating corpus ← **RECOMMENDED**
- B) Auto-enable based on corpus age (detect when 30 days of data have accumulated)

**Recommendation: A.** Auto-enable (Option B) is fragile: corpus age can be measured, but
corpus quality (sufficient wallet population with ≥ 10 round-trips) cannot be detected
automatically without running the FDR procedure itself. A false auto-enable produces
miscalibrated FDR labels. Explicit operator sign-off (config flag) is the correct gate —
the operator runs the Stage 2 FDR procedure, validates TPR/FPR on the labelled fixture set,
then flips the flag.

### Decision 9: Cross-token vs per-token corpus
**Recommendation: aggregate cross-token; per-token PnL in evidence payload.**

Rationale from research/sprint13-b-citations.md §Task 2: "a wallet may be smart on
memecoins but lucky on bluechips." The aggregate `total_pnl_usd` and `win_rate` are
cross-token (all tokens the wallet traded). The `per_token_pnl` field in the evidence
JSON carries a map of `{token_mint: pnl_usd}` for the top-10 tokens by absolute PnL,
allowing downstream consumers to identify token-specific edge.

The `wallet_pnl_corpus` table stores cross-token aggregates per row. A secondary table or
JSONB column holds per-token breakdown. The label evidence is the consumer-facing surface;
per-token detail is in the JSON.

### Decision 10: Suppression
**Recommendation: NO suppression of smart-money labels on established protocols.**
See §5.5. Smart-money labelling is a positive label for EOAs with demonstrated alpha. An
EOA with timing alpha on JUP, RAY, or BONK is strong positive evidence of genuine skill,
not a false positive to suppress. Suppression would remove the most credible evidence.

---

## §12 Migration Spec — V00016 `wallet_pnl_corpus`

**Applies only if Decision 4 selects materialized storage (recommended).**

**File:** `migrations/postgres/V00016__wallet_pnl_corpus.sql`

```sql
-- V00016: wallet_pnl_corpus
-- Materialized PnL corpus for the smart-money labelling pipeline (Sprint 22).
-- One row per (chain, wallet, token) representing the aggregate PnL metrics
-- computed from FIFO-matched buy/sell round-trips in the `swaps` table.
--
-- Design: docs/designs/0022-smart-money-labelling-mvp.md §12
-- Decision 4: materialized storage preferred over computed-on-demand at 100K+ wallet scale.

CREATE TABLE IF NOT EXISTS wallet_pnl_corpus (
    id                      BIGSERIAL               NOT NULL,
    chain                   TEXT                    NOT NULL,
    wallet                  TEXT                    NOT NULL,
    token                   TEXT                    NOT NULL,  -- token mint / contract address
    round_trip_count        BIGINT                  NOT NULL DEFAULT 0,
    non_null_pnl_count      BIGINT                  NOT NULL DEFAULT 0,
    -- All monetary values stored as NUMERIC per ADR 0002; no FLOAT.
    total_pnl_usd           NUMERIC(20, 4),          -- NULL when non_null_pnl_count = 0
    win_rate                NUMERIC(6, 5),            -- [0.00000, 1.00000]; NULL when no priced round-trips
    mean_holding_time_secs  NUMERIC(12, 2),           -- NULL when round_trip_count = 0
    sell_before_peak_rate   NUMERIC(6, 5),            -- [0.00000, 1.00000]; NULL when no pump events evaluated
    recurrence_count        BIGINT                  NOT NULL DEFAULT 0,
    median_timing_lead_secs NUMERIC(12, 2),           -- NULL when recurrence_count = 0
    timing_lead_pct_rank    NUMERIC(6, 5),            -- [0.00000, 1.00000]; NULL when recurrence_count = 0
    per_token_pnl           JSONB,                   -- {token_mint: "pnl_usd_string"} top-10 tokens (Decision 9)
    first_trade_at          TIMESTAMPTZ,
    last_updated            TIMESTAMPTZ             NOT NULL,
    batch_run_id            UUID                    NOT NULL, -- identifies which batch computed this row
    CONSTRAINT pk_wallet_pnl_corpus PRIMARY KEY (id)
);

-- Unique index for upsert path: one row per (chain, wallet, token).
-- Covers both the upsert ON CONFLICT target and point lookups.
CREATE UNIQUE INDEX IF NOT EXISTS uq_wallet_pnl_corpus_wallet_token
    ON wallet_pnl_corpus (chain, wallet, token);

-- Index for batch-level queries: fetch all wallets with new activity since last run.
-- Used by the SmartMoneyLabeller incremental update strategy.
CREATE INDEX IF NOT EXISTS idx_wallet_pnl_corpus_last_updated
    ON wallet_pnl_corpus (chain, last_updated DESC);

-- Index for label-quality queries: find all wallets above a PnL threshold for audit.
CREATE INDEX IF NOT EXISTS idx_wallet_pnl_corpus_pnl
    ON wallet_pnl_corpus (chain, total_pnl_usd DESC NULLS LAST)
    WHERE total_pnl_usd IS NOT NULL;

COMMENT ON TABLE wallet_pnl_corpus IS
    'Materialized realized-PnL corpus for smart-money labelling (design 0022, Sprint 22). '
    'One row per (chain, wallet, token). Updated every batch_interval_hours by SmartMoneyLabeller. '
    'No f64 monetary columns; all NUMERIC per ADR 0002.';

COMMENT ON COLUMN wallet_pnl_corpus.total_pnl_usd IS
    'Sum of (exit_price - entry_price) * closed_qty over FIFO-matched round-trips with non-NULL price data. '
    'CALIBRATION: heuristic, not FDR-controlled (Barras 2010 Stage 2 pending corpus).';

COMMENT ON COLUMN wallet_pnl_corpus.win_rate IS
    'Fraction of priced round-trips with positive PnL. '
    'Tier 1 floor: 0.55 (unverified-heuristic; see config/detectors.toml [smart_money_v1]).';
```

**Partition strategy:** Monthly partitioning is not applied to `wallet_pnl_corpus` at MVP.
The table is not a time-series append; it is a mutable aggregate (one row per wallet-token
pair, updated in place). Partitioning by `chain` would help at multi-chain scale (Phase 4)
but is unnecessary for Solana-only at Sprint 22. Add partition key when Phase 4 adds EVM
chains. The `last_updated` BRIN-like index supports time-based incremental queries without
a partition key.

---

## §13 Fixture Specification

### §13.1 Positive fixtures

**POS_SM_01 — Tier 1 mastermind wallet (synthetic):**
- Wallet: `SM_POS_01_tier1_mastermind` (synthetic Base58 address)
- History: 15 completed round-trips on 5 tokens
- Total PnL: $45,000 (30% win rate — wait, 55%+ required: configure 10/15 winning)
- Timing recurrence: appeared in 4 pump events pre-announcement window
- Timing lead percentile rank: 0.92 (top 8%)
- Sell-before-peak rate: 0.75
- Expected label: SmartMoney, Tier 1, confidence ≈ 0.80

**POS_SM_02 — Tier 2 high-PnL wallet (synthetic):**
- Wallet: `SM_POS_02_tier2_pnl`
- History: 12 completed round-trips
- Total PnL: $5,000 (meets Tier 2 $1,000 floor but not Tier 1 $10,000)
- Recurrence: 1 pump event (does not meet Tier 1 recurrence = 3)
- Expected label: SmartMoney, Tier 2, confidence ≈ 0.55

**POS_SM_03 — Tier 3 directional (synthetic):**
- Wallet: `SM_POS_03_tier3`
- History: 11 completed round-trips, total PnL $200
- Expected label: SmartMoney, Tier 3, confidence ≈ 0.30

### §13.2 Negative fixtures

**NEG_SM_01 — CEX hot wallet (excluded by KnownExchange label):**
- Wallet: Binance hot wallet address (from `token-registry/data/*.json`)
- History: Thousands of round-trips, massive PnL
- Expected outcome: NO label written (allowlist exclusion at §5.3)

**NEG_SM_02 — Wash trader (excluded by E-SM-2 cross-detector check):**
- Wallet: `SM_NEG_02_wash_trader`
- Active `wash_trading_v1` AnomalyEvent with confidence 0.80
- History: positive PnL from self-dealing
- Expected outcome: NO label written (wash-trading exclusion at §9)

**NEG_SM_03 — Insufficient round-trips:**
- Wallet: `SM_NEG_03_few_trades`
- History: 7 completed round-trips (below min_round_trips = 10)
- Expected outcome: NO label written

**NEG_SM_04 — Negative PnL wallet:**
- Wallet: `SM_NEG_04_bag_holder`
- History: 15 completed round-trips, total PnL = -$3,000
- Expected outcome: NO label written (Tier 3 requires PnL > 0; Tier 1/2 PnL floors not met)

---

## §14 References

All citations backed by REFERENCES.md entries:

- Barras, Scaillet & Wermers 2010 — FDR skill/luck separation. Stage 2 (blocked).
  `min_round_trips = 10` derivation; `fdr_q_threshold = 0.10` rationale.
- Fantazzini & Xiao 2023 — informed-early-buyer 60-min window. `pre_event_window_secs = 3600`.
  Timing lead feature operationalization.
- Fu, Feng, Wu & Xu 2025 (Perseus) — cross-event recurrence ≥ 3. `min_event_recurrence_tier1 = 3`.
  438 confirmed masterminds as positive fixture inspiration.
- Easley, López de Prado & O'Hara 2012 (VPIN) — informed-flow microstructure. Secondary
  support for timing-lead features as "informed flow" proxy. Not directly implemented in MVP
  but informs the theoretical frame.
- Nansen — secondary / market-color. `tier1_pnl_floor_usd = "10000"` calibration anchor
  (industry-standard smart-money PnL floors reported as $10K-$100K threshold for "whale
  smart money" designation). Superseded by Barras 2010 as primary statistical authority.
- research/sprint13-b-citations.md — Sprint 13 framework recommendation. Authoritative input
  for this spec.
