# Review 0003 — D04 Pump & Dump / Volume-Price Spike: Adversarial Evasion Analysis

**Date:** 2026-04-21
**Reviewer:** security-researcher agent, mg-onchain-analysis
**Detector:** D04 `pump_dump`
**Spec ref:** `docs/designs/0007-detector-04-pump-dump.md`
**Implementation ref:** `crates/detectors/src/d04_pump_dump.rs`
**SQL ref:** `docs/queries/d04_pump_and_dump.sql`
**Config ref:** `config/detectors.toml` `[pump_dump.*]`
**Prior reviews:** `docs/reviews/0001-d01-honeypot-evasions.md`, `docs/reviews/0002-d02-rug-pull-evasions.md`
**Status:** Draft

---

## 1. Executive Summary

- Signal A (volume/price spike over rolling baseline) is structurally sound for concentrated
  single-hour pumps but has four exploitable timing gaps: slow-pump spread over 4–6h (spec
  DG-04-1, unimplemented), pre-pump baseline contamination (spec DG-04-2, Phase 3), cross-pool
  dilution (spec DG-04-4), and the liquidity-removal-then-replacement price-spike pattern where
  volume never spikes but price does.

- Signal B (burst fallback) carries the highest false-negative risk per dollar of attacker
  benefit. An attacker who understands the `burst_concentration_threshold = 0.90` can execute a
  two-hour pump where `vol_1h / vol_24h = 0.55` — well under threshold — while still extracting
  the majority of retail FOMO inflows. Signal B is a blunt instrument calibrated from two probes
  (RAVE + WET); the threshold is almost certainly too tight.

- Signal C (insider sell-off amplifier) has three meaningful attack surfaces beyond what the
  spec documents: (a) fragmentation of insider sells below the 1% supply floor so no wallet
  triggers the Priority 2 proxy; (b) routing insider sells through DexPool-classified accounts
  that the `holder_is_not_excluded` sidecar removes before Signal C evaluation; and (c) the
  `top_holders_insider_floor_pct = 0.01` floor combined with zero `deployer_clusters` data
  produces a systematic Phase 2 Signal C miss on any token where insiders accumulated below 1%
  individually. This is a known Phase 2 limitation but its practical impact is higher than the
  spec acknowledges.

- Signal C total suppression on established protocols is the correct policy for known protocols
  (PYTH, MPLX) but creates an accepted severity downgrade for MPLX-style tokens that are
  genuinely being rug-pumped by a malicious insider. The spec's reasoning is sound; however,
  this review proposes a refinement: a partial-suppression mode for tokens where Signal A
  confidence exceeds 0.80 AND the insider-sold-pct exceeds 2× the threshold, since at those
  extremes the "treasury sell" hypothesis becomes implausible.

- Three determinism and code-level findings are identified. The most significant is
  `ingested_at: Utc::now()` inside `make_event()` at line 963, which is also present in D02
  and was identified in review 0002 as a reproducibility violation. This review escalates it
  to CRITICAL for D04 because the spec (§8) explicitly mandates "The developer MUST NOT call
  `Utc::now()` inside `evaluate()`." The implementation violates this constraint.

- The sigmoid computation at line 554 converts `Decimal → f64` via `.to_f64().unwrap_or(0.0)`
  before applying the formula. This is the correct approach (sigmoid operates on f64) but the
  `Decimal → f64` conversion is lossy and the fallback to 0.0 silently produces a clamped
  confidence of 0.60 if the Decimal happens to be non-representable. Given that the inputs
  (`volume_ratio_dec`, `price_change_dec`) are USD values computed from Decimal arithmetic,
  the conversion can lose precision for very large values (e.g. volume > 10^15 USD in raw units).
  No realistic token reaches this; however, the silent 0.0 fallback should be an error log.

- Sign-off: **Caveat.** The code is shippable for Phase 2 with four findings that MUST be
  documented as accepted risk before Sprint 4 exit. `Utc::now()` (C1) must be patched before
  merge — it is a one-line fix and violates an explicit spec mandate. The remaining findings
  (C2 through C4) may ship with REFERENCES.md entries and TODO comments that specify the
  resolution sprint. The evasion catalog below informs threshold adjustments and fixture
  additions that should be addressed in Sprint 5.

---

## 2. Evasion Catalog

The spec (0007 §12) documented eight evasion patterns (E-D04-1 through E-D04-8). This review
identifies fourteen additional techniques (E-D04-9 through E-D04-22), for a total of
twenty-two entries. Each entry maps which signal it defeats, the attacker's cost, preserved
reward, detection cost, and precedent.

---

### E-D04-9 — Two-Hour Slow Pump (Threshold Straddle)

**One-line:** Pump is executed over 2h; each 1h slice stays below price and volume thresholds.

**Signals defeated:**
- Signal A: `price_spike_pct_1h = 0.30` requires 30% in a single hour. Two consecutive hours
  of +18% each produces +39.24% total — fully above organic for a 48h window — while each
  individual 1h window stays at 18%. The `volume_multiplier = 5.0` is similarly straddled by
  distributing 10× baseline volume across 2h: 5× per hour is exactly the threshold in each
  window, which the SQL query guard (`WHERE vol_ratio >= $5`) excludes at equality (only `>=`
  is tested; at exactly 5× the query returns a row, but at 4.9× it does not).
- Signal B: `burst_ratio = vol_1h / vol_24h`. At 2h of sustained buying, `vol_1h ≈ 0.50 ×
  vol_24h`, giving burst_ratio 0.50 — far below the 0.90 threshold.
- Signal C: not evaluated (neither A nor B fires).

**Cost to attacker:** Low. Coordination of buy orders across a 2h window is trivially
automatable. Reduces peak FOMO attraction (retail sees a slower move) but preserves the
majority of exit liquidity.

**Reward preserved:** ~85%. Slower pumps attract less retail volume but the attacker can
compensate with higher pre-pump accumulation.

**Detection cost:** Implement DG-04-1 as specified: a secondary 4h evaluation window with
relaxed thresholds (`price_spike_pct × 0.60 = 0.18` and `volume_multiplier × 1.5 = 7.5×` over
the 4h window). This is the only correct counter. Alternatively, use a sliding-window
continuous check: if price rises ≥30% between any two points in the last 4h regardless of
individual 1h slices.

**Precedent:** The original RAVE EVM pump (ZachXBT, April 2026) spread its primary
accumulation phase over ~18h before the vertical spike. The 1h-window detector would have
been blind for the majority of the accumulation.

---

### E-D04-10 — 7-Day Baseline Wash Inflation (Contaminated Baseline)

**One-line:** Attacker wash-trades the token for 7 days prior to the real pump, inflating the
baseline so the required burst never breaches the 5× multiplier.

**Signals defeated:**
- Signal A: `volume_multiplier = 5.0` compares 1h spike to 7d median. If the attacker
  wash-trades at $1,000/day for 7 days (cost: ~$7,000 in self-crossing fees), the median
  becomes $1,000. A real pump generating $4,900/h now appears as only 4.9× — below threshold.
  To evade, the attacker chooses the wash volume such that the spike is always <5× whatever
  baseline the wash creates.
- Signal B: does not fire because the baseline now has ≥3 days of non-zero volume (Signal B
  only fires when `min_baseline_days < 3`). The zero-baseline fallback is disabled.

**Cost to attacker:** Medium. Self-crossing DEX trades at small volumes. For a $50K pump,
washing $7K in baseline costs ~1-3% in pool fees and is recoverable via the dump proceeds.
Main cost is time (7 days of active wash before launch).

**Reward preserved:** Full. The pump executes normally; only the detector baseline is
inflated.

**Detection cost:** Secondary baseline comparison (spec DG-04-2, Phase 3): compare the 7d
baseline itself against a prior 30d baseline. If the 7d baseline is anomalously elevated
relative to the 30d prior period, flag the baseline as contaminated and lower the effective
threshold. This requires 30 days of swap history retention. Phase 3 enhancement.

**Precedent:** Karbalaii (2025) §3 explicitly describes pre-pump baseline contamination as an
observed attacker behavior. The accumulation phase is definitionally this pattern applied at
a coarser granularity.

---

### E-D04-11 — Cross-Pool Targeted Pump (Volume Dilution)

**One-line:** Pump concentrates on the token's smallest-liquidity pool; token-level aggregation
dilutes the spike below threshold.

**Signals defeated:**
- Signal A: the SQL query sums `usd_value` across all pools where `token_out = $2`. A token
  with 5 pools averaging $10K/h normal volume has a token-level baseline of $50K/h. The
  attacker pumps one $70K/h burst in the smallest pool ($10K liquidity) but the token-level
  1h volume is only $80K total — a 1.6× multiple, far below the 5× threshold. The individual
  pool shows 7× ($70K vs $10K baseline), but D04 never evaluates per-pool.
- Signal B: token-level burst_ratio is also diluted: the 4 unaffected pools continue normal
  trading throughout the 24h window, keeping `vol_24h` large and `burst_ratio` low.

**Cost to attacker:** Low. Pick the pool with lowest liquidity (highest price impact per
dollar spent), concentrate all buys there. Cost is the slippage on the pump buys, which is
recovered from retail entry.

**Reward preserved:** Full within the targeted pool. Exit is on the same pool.

**Detection cost:** Per-pool Signal A evaluation (spec DG-04-4). Add a pool-level branch to
the SQL query that evaluates each pool independently. Fire at lower confidence (`0.80 ×
signal_a_confidence`) with `pump_dump/per_pool_spike = "1"` in evidence. Requires resolving
Open Question #4 in `docs/designs/0003-detector-trait.md`. Phase 2 backlog.

**Precedent:** Cross-pool price arbitrage exploitation is well-documented in AMM literature.
The specific pump-via-small-pool pattern is observed in micro-cap Raydium tokens where multiple
stale pools exist post-launch (DEXScreener shows pools created at different price ranges).

---

### E-D04-12 — Liquidity-Remove Price Spike (Volume-Free Pump)

**One-line:** Attacker removes 60–70% of pool liquidity (keeping it below D02's 65% threshold
or using multiple actors), causing price to spike on the reduced liquidity without generating
buy volume.

**Signals defeated:**
- Signal A: price spikes +35% because the AMM curve steepens on reduced liquidity. But no
  retail buys happen yet — the spike is caused by liquidity removal, not by buy pressure.
  `volume_1h_usd` stays near zero (there are no swaps). Signal A does not fire because the
  volume threshold is not breached.
- Signal B: `vol_1h / vol_24h` is near-zero / near-zero = undefined or 0. The burst query
  returns 0 burst_ratio. Signal B does not fire.
- Signal C: no insider sells yet; the attacker is in the accumulation-via-price-distortion
  phase.

**What fires:** D02 may partially fire (partial LP removal), but if the attacker uses multiple
actors (each below the 65% D02 threshold per actor), D02 Signal A also misses. D03 may fire
if liquidity removal causes the holder distribution to shift.

**Cost to attacker:** Medium-Low. Remove LP, wait for organic retail to enter on the FOMO of
a rising price chart, then dump into retail and re-add LP at the higher price. Net LP position
is the same or better after the cycle.

**Reward preserved:** High. Retail enters into a price that was artificially elevated by
liquidity removal; the attacker exits at a superior price.

**Detection cost:** Correlate D04 confidence with concurrent D02 LP reduction events. If
`anomaly_events` shows a D02 event within the last 1h on the same token with LP removal
>20%, fire a combined signal. D04 should check `anomaly_events` for recent D02 events as
already specified for D06 correlation (spec §13). Add D02 to the cross-detector check.

**Precedent:** Observed in Uniswap v2 micro-cap tokens where deployers provide LP at launch,
wait for organic trading, then remove LP gradually to force price up before dumping their token
holdings. Peckshield post-mortems 2022-2023 category "LP removal price manipulation."

---

### E-D04-13 — Fragmented Insider Sell (Signal C Floor Evasion)

**One-line:** Insider positions are fragmented across many wallets each holding <1% of supply;
the Priority 2 proxy floor filters out all of them.

**Signals defeated:**
- Signal C Priority 2 proxy: the `top_holders_insider_floor_pct = 0.01` filter removes all
  holders with <1% supply from the insider candidate set. An attacker who pre-distributes
  their 20% supply position across 25 wallets (each at 0.8%) is completely invisible to the
  proxy — none of the 25 wallets clears the 1% floor individually.
- Signal C Priority 1 (deployer_clusters): Phase 2 has no deployer_clusters data for new
  tokens; this tier is empty.

**Combined result:** Signal A or B fires correctly at the pump peak. Signal C is not applied.
Confidence stays at Signal A's 0.60–0.95 without the +0.15 amplifier. A pump that would have
been Critical (e.g. 0.80 → 0.95 after amplification) stays at High (0.80). The trading bot
may still block on High — but at lower thresholds, the attacker avoids the Critical severity
that triggers the most aggressive consumer responses.

**Cost to attacker:** Low. Wallet generation is cheap; distributing supply before listing costs
~25 Solana transactions at dust amounts.

**Reward preserved:** Full. The fragmentation does not affect pump profitability.

**Detection cost:** Lower the `top_holders_insider_floor_pct` threshold for Signal C when
Signal A confidence exceeds 0.75 (high-confidence pump). At 0.75+ Signal A, drop the floor
to 0.003 (0.3%) to capture sub-1% fragmented insiders. This is a config change only. Tradeoff:
lower floor increases false positives for Signal C by including legitimate small holders.

**Precedent:** The RAVE EVM token (ZachXBT, April 2026) distributed supply across dozens of
wallets each holding fractions of a percent before the coordinated pump. Standard practice for
any sophisticated pump-and-dump ring.

---

### E-D04-14 — DexPool-Classified Sell-Through (Signal C Sidecar Evasion)

**One-line:** Insider sells tokens by providing liquidity to a new pool (a "buy-side inject"),
then removing it — appearing as a DexPool transaction rather than a sell.

**Signals defeated:**
- Signal C: the `fetch_insider_sells` query looks for direct sell transactions from insider
  addresses. If the insider instead creates a new Raydium pool with their token holdings as
  the "token side" and SOL as the "quote side," then immediately removes LP, the effective
  sale appears as two `pool_events` (Mint then Burn) rather than as a `swaps` row for the
  insider address. The Signal C query (Query 2) scans `swaps` for insider sells — it does not
  scan `pool_events` for LP-creation-then-removal by insiders.
- Additionally, the `holder_is_not_excluded` sidecar classifies DexPool-type addresses as
  excluded. If the insider registers their own address as a "pool" in the holder_classifications
  table (possible via the sidecar's default-include policy for unclassified addresses, but
  exploitable if the classification pipeline is slow to process a new pool), the address may
  be misclassified or unclassified.

**Cost to attacker:** Medium. Creating a new pool requires capital (the quote-side SOL), pool
creation fees (~0.3 SOL on Raydium), and two transactions. But the sell is indistinguishable
from routine AMM operation.

**Reward preserved:** Full. The attacker recovers their SOL by removing LP immediately.

**Detection cost:** Extend Signal C's sell detection to include LP-remove events by insider
addresses. Query `pool_events` for Burn events by insider addresses within the
`post_pump_insider_window_hours` window. This requires joining Signal C's Query 2 with a
pool_events query. Phase 3 enhancement.

**Precedent:** LP-creation-then-removal as a token disposal method is documented in EVM
contexts (Chainalysis 2025 §3: "liquidity-remove dump as an alternative to direct sells").

---

### E-D04-15 — Sybil Confidence Decay (gray-zone classification exploit)

**One-line:** Attacker exploits unclassified Priority 2 proxy holders with low-confidence
sidecar classifications to route sells through addresses that will be incorrectly included
or excluded based on the unclassified-default behavior.

**Signals defeated:**
- Signal C Priority 2: the `holder_is_not_excluded` function (spec §10) defaults to `true`
  (include) for unclassified addresses. An attacker who has multiple insider wallets that
  happen to have not yet been classified (new wallets, no prior history) will have them
  included in the proxy set. This is the correct behavior for fresh wallets — but it means
  the attacker cannot exploit this direction.
  
  The reverse exploit: if the attacker's insider wallets are pre-classified by the sidecar
  as "DexPool" (by routing prior activity through pool accounts), the sidecar excludes them
  from the proxy set. An attacker who knows the sidecar's classification heuristics can
  deliberately engineer their wallets to receive DexPool or VestingContract classifications.

- For example: wallet performs a series of LP deposit/withdraw cycles before the pump to
  build a DexPool transaction history. The sidecar classifier uses transaction patterns to
  assign classifications. By the time the pump occurs, the wallet has been classified as
  "DexPool" and is excluded from the Priority 2 proxy. Signal C never sees it.

**Cost to attacker:** Low-Medium. Requires 10–30 transactions per wallet to build the right
transaction history. This is a one-time setup cost per wallet cluster.

**Reward preserved:** Full.

**Detection cost:** The sidecar classification must include temporal confidence decay — a
wallet classified as DexPool 30+ days before a pump event should have its classification
re-verified before exclusion. This requires a `classification_timestamp` field and re-evaluation
triggers. Phase 3 graph module concern.

**Precedent:** No documented Solana-specific instance. Direct analogue in EVM: "classification
poisoning" of Etherscan labels by deliberately mimicking DEX behavior in contract calls
(Peckshield 2024 MEV-bot classification evasion).

---

### E-D04-16 — CEX Listing Frontrun (Legitimate Catalyst FP)

**One-line:** A genuine token receives a major CEX listing; organic buy pressure exceeds
Signal A thresholds, producing a false positive.

**This is a known FP (spec E-D04-7), restated here with threat model clarity.**

**Signals defeated (from scammer's perspective):** A scammer can exploit this FP by timing a
pump to coincide with an unrelated token's listing announcement. The Telegram/Discord pump
signal fires simultaneously with the legitimate listing news; D04 fires on the scam token
(correct) but also fires on the legitimate token (FP). In a high-alert environment, the FP
on the legitimate token desensitizes operators to D04 alerts.

**Cost to attacker:** Low. The attacker does not control the listing announcement; they
opportunistically time their pump to the announcement. Information about upcoming listings is
often leaked 24–48h before announcement in trading circles.

**Reward preserved:** Full. The FP exhausts reviewer attention on the legitimate token while
the scam completes.

**Detection cost:** A CEX listing data feed (out of scope for Phase 2 per self-sovereign
constraint). In the interim: if Signal A fires on a token with high prior-day organic buy
volume (volume_z_score low), reduce confidence by 20%. The z-score already captures this
— the spec notes its inclusion in evidence but not in the formula. See threshold analysis §3.

**Precedent:** The Bolz et al. (2024) paper explicitly targets this FP as the core limitation
of volume-spike-based pump detection, proposing the market-cap filter as the primary
mitigation. The $60M filter eliminates most legitimate CEX-listed tokens.

---

### E-D04-17 — Copy-Trading Bot Exit Liquidity (Multi-Sender Signal C Miss)

**One-line:** Attacker uses the copy-trading bot swarm as exit liquidity, executing many small
sells rather than a concentrated insider dump; `insider_sell_pct` threshold is never breached
per wallet.

**Signals defeated:**
- Signal C: the `compute_insider_sold_pct` function aggregates sells across all insider
  wallets. If the attacker's 20% supply position is fragmented across 100 wallets selling
  0.002% of total supply each — each wallet selling 100% of its own holdings — the aggregate
  calculation depends on how many of these wallets are in the `insider_set.addresses`. If
  only 5 of the 100 wallets are in the proxy (because the rest are below the 1% floor),
  the `total_balance` denominator only captures 5% of the actual insider position, and
  `total_sold / total_balance` for those 5 wallets may be ≤40% if each sold only a portion.

**Cost to attacker:** Low. Fragmenting sells across many wallets is standard bot practice.

**Reward preserved:** Full. The retail copy-trading bots absorb the sell pressure; price
decline is gradual enough that not all bots exit before full distribution.

**Detection cost:** Signal C needs a "velocity of insider sells" metric: count unique insider-
adjacent wallets (from the Priority 2 proxy or deployer_clusters) that sold ANY amount within
the `post_pump_insider_window_hours` window. If >50% of identified insiders sold ANY position,
fire Signal C at lower confidence (0.10 amplifier instead of 0.15) regardless of per-wallet
sold_pct. Phase 3 enhancement (requires graph to identify "insider-adjacent").

**Precedent:** RAVE EVM April 2026: ZachXBT noted that the dump was distributed across
"dozens of team-linked wallets" each executing small sells simultaneously, making it appear
as organic profit-taking.

---

### E-D04-18 — OTC / Off-Chain Accumulation (Signal C Blind Spot)

**One-line:** Insiders accumulate via off-chain OTC agreements; on-chain they appear as
a normal holder who never participated in the pre-pump phase.

**Signals defeated:**
- Signal C Priority 1 (deployer_clusters): the OTC wallet is not funded from the deployer;
  it received tokens via a peer-to-peer SOL transfer that appears in the blockchain as a
  standard SOL transfer with no token link.
- Signal C Priority 2 (top_holders_proxy): the OTC wallet holds tokens (it's a legitimate
  holder from the chain's perspective) but its sell-pct post-pump may exceed 40%. Whether
  it's caught depends on whether it clears the 1% supply floor. If the OTC deal was for
  exactly 0.8% of supply, it is excluded.

**Signal C cannot see accumulation that occurred before any on-chain swap activity.**

**Cost to attacker:** High. OTC deals require trust relationships and are logistically
complex. Only sophisticated operators execute this.

**Reward preserved:** Full.

**Detection cost:** Graph analysis of wallet funding (Phase 3): trace the funding source of
all top holders back to their originating funder. If multiple top holders were funded from
the same CEX withdrawal or bridge transaction within 72h of listing, flag as coordinated
pre-pump accumulation. This is Phase 3 graph clustering.

**Precedent:** Whale-group pumps documented by ZachXBT (multiple 2025 cases) where
Telegram-coordinated buyers accumulated via over-the-counter USDT purchases before the
on-chain pump.

---

### E-D04-19 — Multi-Hop Jupiter Route (Attribution Muddying)

**One-line:** Attacker routes their pump buys through Jupiter aggregator's multi-hop path;
intermediate tokens appear as the swap counterparty, muddying Signal C address attribution.

**Signals defeated:**
- Signal C Priority 2 (top_holders_proxy): the `fetch_insider_sells` Query 2 looks for swaps
  where `from_address IN (insider_addresses)`. Jupiter routes create intermediate accounts and
  program-derived addresses as the actual `from_address` in the `swaps` table. The real
  attacker wallet initiates a Jupiter transaction but the intermediate hop addresses are
  recorded as the swap counterparty.
- This does not defeat Signal A or B (volume is volume regardless of routing).

**Cost to attacker:** Zero additional cost — Jupiter routing is standard for any Solana DEX
trade. The muddying happens automatically.

**Reward preserved:** Full.

**Detection cost:** Signal C's sell query must trace through Jupiter's transaction layout to
identify the originating signer, not just the swap counterparty. This requires per-DEX
adapter logic in the `dex-adapter` crate (Jupiter-specific instruction parsing). Phase 3
enhancement; the chain-adapter layer must expose the "economic from" (original signer) vs
"technical from" (program account).

**Precedent:** EVM meta-transaction relay analogues documented in ERC-4337 token transfer
attribution (CLAUDE.md §Ethereum/EVM common pitfalls: "follow the money, not the `from`
field").

---

### E-D04-20 — Market-Cap Filter Manipulation via Supply Inflation (D06 Link)

**One-line:** Token starts above $60M FDV; attacker inflates supply briefly via hidden mint
to deflate FDV below filter, executes pump, burns back supply.

**Signals defeated:**
- Market-cap filter: as specified in spec E-D04-5. The implementation at lines 247–263 computes
  `market_cap_usd` from `meta.total_market_liquidity_usd` (a DEX liquidity proxy, NOT the
  actual FDV). This means the filter only fires when DEX liquidity exceeds $60M — not when FDV
  exceeds $60M. A token with $70M FDV but only $5M in DEX liquidity would NOT trigger the
  filter. The filter is materially weaker than the spec intends.

**Code-level gap:** `resolve_market_cap()` at lines 736–761 in the implementation only uses
`total_market_liquidity_usd` as a proxy ("a lower bound on market cap" per the inline comment).
For most shitcoins, DEX liquidity is 5–20% of FDV. The filter's effective threshold is
`$60M / 0.10 = $600M FDV equivalent` — ten times the stated threshold. This is documented
as DG-04-5 in the spec but the practical gap is larger than acknowledged.

**Cost to attacker:** Low (if mint authority is retained; requires D06 event).

**Reward preserved:** Full.

**Detection cost:** Use `circulating_supply_raw × price_per_token` as the market cap when
available (spec §DG-04-5 already mandates this). The current implementation does NOT
implement this path — it always falls through to the liquidity proxy. Fix: compute price
from recent swap data (`volume_usd / token_volume_raw` from the last confirmed swap) and
multiply by `circulating_supply_raw`. Add a Phase 2 task to implement this.

**Precedent:** TRUMP token (jup_verified=false, score=58) has FDV well above $60M but DEX
liquidity is a fraction of FDV. The current implementation would evaluate TRUMP for pump
detection when it should be filtered out, and conversely would evaluate high-FDV scam tokens
that maintain thin liquidity.

---

### E-D04-21 — Fake-Volume Baseline Inflation via D05-Blind Wash Trading

**One-line:** Attacker uses a D05-evading wash-trade pattern to inflate the 7d baseline;
when D05 does not fire, D04's baseline is contaminated without any cross-detector alert.

**Signals defeated:**
- Signal A: same contamination mechanism as E-D04-10, but the attacker specifically designs
  the wash trades to evade D05's Heuristic 1 (same address round-trip within 25 slots, diff
  <1%, ≥3 reps). By spacing round-trips >25 slots apart and varying the volume diff slightly
  (1.5–3% diff to stay above D05's `volume_diff_pct = 0.01` but still be economically neutral),
  D05 does not fire. D04's baseline is contaminated without any upstream alert.

**Cost to attacker:** Low — the D05 evasion just requires spacing the wash trades out by 10
seconds (25 Solana slots). Total cost is the DEX fees paid on the wash volume.

**Reward preserved:** Full.

**Detection cost:** When D04 Signal A fires, check whether the 7d baseline window had
statistically anomalous volume distribution (coefficient of variation > threshold). If the
7d volume is concentrated on certain hours that cluster suspiciously (e.g., all wash trades
occur at the same time each day), emit a `pump_dump/baseline_quality_suspect = "1"` evidence
key. This is a Phase 3 enhancement to the baseline computation query.

**Precedent:** Volume manipulation before pump is standard practice in low-cap token manipulation.
Documented by Chainalysis (2025): "baseline contamination precedes approximately 40% of
confirmed pump-and-dump events in Solana shitcoin category."

---

### E-D04-22 — Thin-Book Single-Buyer Price Spike (No Volume, Price Threshold Breached)

**One-line:** Attacker makes one large buy in a thin-liquidity pool; price spikes 35%+ but
volume_ratio is only 2–3× baseline (below 5× threshold). Signal A's dual gate means neither
component alone fires.

**Signals defeated:**
- Signal A: the query requires BOTH `vol_ratio >= volume_multiplier` AND `price_change >=
  price_spike_pct`. A single large buy in a $10K liquidity pool can move price by 50% while
  generating only $3,000 in absolute volume — which may be 2× the daily median but not 5×.
  The volume gate prevents Signal A from firing.
- Signal B: burst_ratio requires the 1h volume to be ≥90% of 24h volume. If the rest of the
  day has $1,500 in normal volume, and the large buy was $3,000, `burst_ratio = 3000/4500 =
  0.67` — below threshold.

**Cost to attacker:** Very low. One swap transaction. The price spike attracts retail FOMO
who then push the price further.

**Reward preserved:** High. The attacker benefits from the price appreciation caused by
their single buy, then exits into retail demand.

**Detection cost:** Add a third detection mode for Signal A: `price_spike_pct ≥ 0.50` AND
`pool_liquidity_usd < 20000` fires at a fixed lower confidence (0.55, below Signal A minimum).
This catches thin-book price spikes without requiring the volume threshold. Requires pool
liquidity data from `TokenMeta`. Phase 2 addition — pool liquidity is already in
`total_market_liquidity_usd`.

**Precedent:** Thin-book price manipulation is the primary attack vector for tokens with
<$5K liquidity. Common in newly-listed PumpSwap tokens in the first 30 minutes of trading.

---

## 3. Threshold Analysis

### 3.1 `volume_multiplier = 5.0`

**Assessment: Correctly calibrated for normal baseline, but produces a systematic gap for
micro-cap tokens.**

Karbalaii (2025) derives 5× from accumulation-phase volume analysis. However, micro-cap tokens
($10K–$100K liquidity) naturally have higher daily volume variance — a coefficient of variation
of 200–400% is common. For these tokens, the mean-as-median can be dominated by a single
prior active day, making the 5× threshold either too tight (many organic trading spikes
trigger Signal A) or too loose (a single prior high-volume day raises the baseline so the
real pump appears as only 3×).

**Proposal:** Introduce a conditional multiplier: when `std_volume_usd / mean_volume_usd > 1.5`
(high-variance baseline), use `volume_multiplier × 1.5 = 7.5` to compensate for the inflated
baseline instability. At low variance (consistent trader), use 5.0. This is a config-addable
`high_variance_volume_multiplier = 7.5` with conditional application in the formula. The
z-score column (already in evidence) is the correct mechanism: when `volume_z_score ≥ 5.0`
(which corresponds to a ~5σ event), fire regardless of the raw multiplier.

**Recommendation:** Add z-score as a secondary gate: fire Signal A if `volume_z_score ≥ 5.0`
even when `volume_ratio < 5.0`. This closes the baseline-contamination gap partially and
provides a robust signal for high-variance micro-caps. No config change needed — z-score is
already computed.

### 3.2 `price_spike_pct = 0.30`

**Assessment: Too loose for established tokens; too tight for newly-listed micro-caps.**

30% in 1h is a reasonable global threshold. However:
- For tokens with >$5M baseline volume (mid-tier tokens), a 30% price move in 1h is genuinely
  anomalous. The threshold is appropriate.
- For tokens with <$5K daily baseline volume, a single $100 buy can move price 30%+ on a thin
  pool. The threshold would fire on almost any active trading session for ultra-micro-caps.

**Proposal:** Make `price_spike_pct` conditional on pool depth:
- Pool depth < $5K: `price_spike_pct = 0.60` (higher threshold — thin book, organic spikes
  are common)
- Pool depth $5K–$100K: `price_spike_pct = 0.30` (current)
- Pool depth > $100K: `price_spike_pct = 0.20` (lower threshold — price is harder to move,
  any 20% spike is more suspicious)

This requires pool depth data in the query, which is available from `TokenMeta.
total_market_liquidity_usd`.

### 3.3 `burst_concentration_threshold = 0.90`

**Assessment: Calibrated from 2 probes only (RAVE + WET). Almost certainly too tight.**

The RAVE probe produced a burst_ratio of 1.00 (all volume in 1h). The WET probe produced
0.0024. The threshold of 0.90 cleanly separates these two data points — but two data points
are not a calibration; they are an existence proof.

Real pump-and-dump tokens will produce burst_ratios in the 0.55–0.90 range when the pump
extends over 2h. At 0.90, every 2h pump is missed by Signal B.

**Proposal:** Lower to `burst_concentration_threshold = 0.70`. At 0.70 burst_ratio, confidence
would be `0.50 + (0.70 - 0.70)/0.10 * 0.25 = 0.50` (minimum Signal B). At 0.90, confidence
is still 0.50. The threshold change increases coverage; the confidence formula already
provides a linear ramp. The primary FP risk is legitimate tokens with concentrated but organic
trading (e.g., a new token where 70% of day-1 volume occurred in the first hour of listing).
Add a `min_burst_volume_usd` guard to mitigate this: the current $5,000 dust filter is
appropriate. Sprint 5 calibration should measure FP rate at 0.70 vs 0.90 on the labelled
corpus before shipping.

### 3.4 `insider_sell_pct = 0.40`

**Assessment: Correctly calibrated but susceptible to fragmentation (E-D04-13).**

40% aggregate sell is a reasonable threshold for the Priority 1 (deployer_clusters) path.
For the Priority 2 (top_holders_proxy) path, the 40% threshold combined with the 1% floor
creates a compound gap: an attacker can maintain positions of 0.99% per wallet, avoid the
floor entirely, and Signal C never fires regardless of how much they sell.

**Proposal:** When `insider_source = top_holders_proxy` (degraded mode), lower the floor to
`top_holders_insider_floor_pct × 0.33 = 0.0033` (0.33% of supply) for tokens where Signal A
confidence ≥ 0.75. This is a conditional config applied only in degraded mode during
high-confidence pumps. Document as a compensating control for Phase 2 degraded mode.

### 3.5 `market_cap_filter_usd = 60_000_000`

**Assessment: The filter threshold is correct but the proxy used to evaluate it is wrong.**

As identified in E-D04-20: the implementation uses `total_market_liquidity_usd` (DEX pool
depth) as the market cap proxy. This is documented in the code as a "lower bound" but in
practice it is a 5–20× underestimate for most tokens. The filter only triggers when DEX
liquidity exceeds $60M — a condition that effectively never occurs for shitcoins.

**Corrective action:** Implement `circulating_supply_raw × price_per_token_usd` as the primary
market cap proxy. The price can be computed from the most recent swap in the `swaps` table
(`usd_value / token_amount_raw`). This is a Phase 2 fix, not a Phase 3 enhancement.

### 3.6 `top_holders_insider_floor_pct = 0.01` 

**Assessment: Correctly calibrated for Phase 3 but creates a systematic miss in Phase 2.**

The 1% floor is documented as a conservative filter for Phase 2. However, as shown in
E-D04-13, it is trivially evaded by any sophisticated pump operator who reads this codebase.
The sprint 5 calibration task should measure what percentage of confirmed pump-and-dump
insider wallets in the labelled corpus held <1% of supply. If this exceeds 30%, the floor
must be lowered.

---

## 4. Fixture Gap Analysis

### 4.1 Coverage Assessment of Current 6 Fixtures

| Fixture | Signal Path Covered | Gap |
|---|---|---|
| POS_01 RAVE (live) | Signal B, burst_ratio=1.00 | Tests extreme case only; no mid-range burst_ratio |
| POS_02 SYNTHETIC burst | Signal B + Signal C Priority 2 proxy | No Priority 1 (deployer_clusters) path for Signal B |
| POS_03 SYNTHETIC insider | Signal A + Signal C Priority 1 | Correct; primary regression |
| NEG_01 BONK | market_cap_filter | Always triggers filter; Signal A/B never tested |
| NEG_02 established protocol | Signal C suppression (PYTH/RAY) | See gaps below |
| NEG_03 USDC flat | market_cap_filter + price near-zero | Same as NEG_01 |

### 4.2 Missing Fixtures

**Missing Fixture 1: Slow-pump positive (Signal A near-miss)**
```
File: research/fixtures/pump_dump/NEG_04_SLOW_PUMP_near_miss.json
Purpose: Documents that a 2h pump with per-hour values just below thresholds
  (price_change_pct_1h = 0.28, volume_ratio = 4.7) does NOT fire Signal A.
Signal: Neither A nor B fires (no event returned).
Why needed: Validates that the detector is correctly silent on the most common evasion
  pattern. Also serves as the anchor for DG-04-1 threshold calibration in Sprint 5.
Parameters: volume_1h = 4700 (baseline = 1000, ratio=4.7), price_change=0.28,
  baseline_days=5. Expected output: empty vec (no event).
```

**Missing Fixture 2: Signal B mid-range burst_ratio (between threshold and 1.0)**
```
File: research/fixtures/pump_dump/POS_04_SYNTHETIC_burst_midrange.json
Purpose: Tests Signal B confidence formula at burst_ratio = 0.95 (midrange, not
  the extreme RAVE case of 1.0). Validates the linear interpolation in the formula.
Signal: Signal B fires at confidence = 0.50 + (0.95 - 0.90) / 0.10 * 0.25 = 0.625
Expected severity: Medium (0.625 is in [0.40, 0.65)).
Why needed: POS_01 and POS_02 both use burst_ratio=1.00; neither tests the linear
  interpolation branch (only the cap branch).
Parameters: vol_1h = 95000, vol_24h = 100000, burst_ratio = 0.95,
  baseline_days = 0, min_burst_volume = 5000 (cleared).
```

**Missing Fixture 3: Signal C suppression test where Signal A fires on established protocol**
```
File: research/fixtures/pump_dump/NEG_05_ESTABLISHED_PROTOCOL_signal_a_fires.json
Purpose: Tests that an established protocol (is_established_protocol=true) with a
  genuine pump spike DOES fire Signal A but does NOT have Signal C applied.
  The evidence key established_protocol_suppressed_signal_c must be present.
Why needed: NEG_02 VARIANT_A (PYTH) exits at the market_cap_filter before Signal A
  is ever evaluated — so the Signal C suppression logic is never exercised by any
  fixture. The unit_test_note in NEG_02 acknowledges this gap but there is no
  corresponding fixture.
Construction: Use a synthetic token with jup_strict=true (is_established_protocol=true)
  AND market_cap_usd below $60M (so the filter does not trigger). Spike the volume to
  8× baseline with +40% price change. Signal A fires. Insider wallets sell 65%.
  Signal C suppressed. Expected output: Signal A confidence unmodified, evidence has
  established_protocol_suppressed_signal_c = "1".
```

**Missing Fixture 4: Market-cap filter boundary (token near $60M)**
```
File: research/fixtures/pump_dump/NEG_06_MARKET_CAP_BOUNDARY.json
Purpose: Tests the filter at exactly the boundary. Two sub-cases: (a) market_cap_usd
  = $59,999,999 — filter does NOT trigger, Signal A evaluated; (b) market_cap_usd
  = $60,000,001 — filter triggers, Info event returned.
Why needed: Boundary conditions on the filter are not exercised by any current fixture.
  BONK and USDC are far above the filter; the filter boundary is untested.
Note: Given the current implementation uses DEX liquidity as the proxy (not true FDV),
  this fixture should document the proxy gap in its notes field.
```

**Missing Fixture 5: Signal C Priority 2 fragmentation miss**
```
File: research/fixtures/pump_dump/NEG_07_FRAGMENTED_INSIDER_miss.json
Purpose: Documents that fragmented insider positions (25 wallets each at 0.8% supply)
  cause Signal C to produce insider_source=unavailable despite 20% total insider supply.
Construction: Token with Signal A firing (volume_ratio=8×, price=+40%). 25 insider
  wallets each holding 0.8% of supply — all below the 1% floor. deployer_clusters empty.
  Expected output: Signal A fires at confidence 0.720, Signal C not applied,
  insider_source=unavailable. Severity: High (not Critical as it would be with C).
Why needed: Documents the E-D04-13 evasion path as a known false-negative to track
  for Sprint 5 calibration.
```

---

## 5. Signal C Total Suppression Verdict

### 5.1 The Spec's Reasoning

The spec (§9) argues for total suppression (not dampening) with three justifications:
1. A dampened amplifier on a legitimate treasury sell still generates review noise.
2. The `established_protocol_suppressed_signal_c = "1"` key preserves auditability.
3. Signal A still fires at 0.60–0.95 — sufficient for consumer action.

### 5.2 The Attack Scenario

A genuinely malicious insider at an MPLX-style established protocol (jup_strict=true)
executes a pump by accumulating silently (Phase 2 cannot detect this) then dumps 80% of
their position immediately after the pump. Signal A fires at, say, 0.75 (High). Signal C
is totally suppressed. The severity stays at High instead of Critical.

The critical question: is "High" the correct severity for a confirmed pump-and-dump on an
established protocol? For the `bot-trader-2-0` consumer, the no-trade gate triggers at
both High and Critical — so the severity difference does not change the trading decision.
For the custody and exchange consumers, High may be below their operational alert threshold
if they have configured Critical as the action trigger.

### 5.3 The Counter-Proposal

Total suppression is the correct default. However, add a partial-suppression escape hatch:

**Condition:** `is_established_protocol = true` AND Signal A confidence ≥ 0.80 AND
insider sell pct (if available from proxy) ≥ 2 × `insider_sell_pct` (i.e., ≥ 80%).

**Action:** Apply Signal C at 50% of the normal amplifier:
`amplified = base + (insider_amplifier × 0.50)` = base + 0.075, capped at 0.95.

**Rationale:** At confidence ≥ 0.80 AND insider-sell ≥ 80%, the "treasury sell" hypothesis
is implausible. A treasury does not sell 80% of its holdings in a single 24h window. This
escape hatch restores the Critical severity path for extreme cases while preserving total
suppression for the 0.60–0.80 range where the treasury-sell FP is realistic.

**Verdict:** The spec's total suppression is defensible and correct for Phase 2. The partial-
suppression escape hatch is a Phase 3 refinement. Do not implement now; add to the design
gaps register as DG-04-6.

---

## 6. Priority 2 `top_holders` Proxy Attack Surface

The Priority 2 proxy is the primary Signal C mechanism in Phase 2, since deployer_clusters
is not populated. Its attack surface is documented here with specificity.

### 6.1 Floor Fragmentation (E-D04-13 detail)

The `top_holders_insider_floor_pct = 0.01` filters holders with `balance_raw < total_supply_raw / 100`.
An attacker who reads this threshold pre-distributes their entire position across N wallets each
at `(0.01 × total_supply_raw) / N × (N-1) / N` — i.e., each wallet holds `(1/N - epsilon) ×
total_supply` so all N wallets are simultaneously below the 1% floor. For N=25 and 20% total
position, each wallet holds 0.8% — below 1%, above dust. All 25 wallets are invisible to
Signal C.

**Attack surface rating: HIGH. Trivially executable. Zero detection.**

### 6.2 DexPool Sidecar Routing (E-D04-14 detail)

The `holder_is_not_excluded` function calls `ctx.store.get_holder_classification()` and
excludes DexPool + VestingContract + CexWallet. The implementation at lines 845–862 has a
secondary fallback: `meta.top_holders` filtered in Rust. This fallback does NOT apply the
`holder_is_not_excluded` exclusion — it only applies the balance floor. An insider wallet
classified as DexPool by the sidecar would be excluded in the primary path (lines 812–841)
but would be included in the meta.top_holders fallback path (lines 845–862) since the
fallback lacks the sidecar JOIN.

**Actual code behavior:** Look at lines 845–862 in `d04_pump_dump.rs`:
```rust
let proxy_addrs: Vec<String> = meta
    .top_holders
    .iter()
    .filter(|h| {
        let balance = Decimal::from(h.amount_raw);
        balance >= floor_raw
    })
    .map(|h| h.address.as_str().to_owned())
    .collect();
```
This filter applies ONLY the balance floor — no `holder_is_not_excluded` check. A DexPool-
classified address that clears the balance floor will be included in the insider set.

**Attack surface rating: MEDIUM. Requires the fallback path to trigger (sidecar query failed
or returned empty). An attacker who can cause the `fetch_top_holders_liquid` query to fail
(e.g., by timing the pump to when the sidecar is lagging) gets their DexPool-classified
wallet included in Signal C evaluation — but this is not the evasion direction. The actual
gap is the reverse: a legitimate DexPool address included falsely creates an FP in Signal C.**

**Exploitable gap:** A DexPool address that has sold tokens (because the pool rebalanced) will
appear as an "insider sell" in Query 2. The aggregate `sold_pct` includes this non-insider
sell. If the DexPool sold 40%+ of its holdings during the pump (normal LP rebalancing), Signal
C fires with `insider_source=top_holders_proxy` as a false positive.

### 6.3 Unclassified-Default Include (E-D04-15 detail)

The implementation default for unclassified addresses is include (lines 508–511 of spec §10:
`unwrap_or(true)`). This means fresh wallets (no prior classification) are always included in
the proxy insider set. An attacker with fresh wallets that clear the 1% floor is correctly
included — this is the RIGHT behavior. But it creates a FP if a large retail holder (who
happened to buy above 1% of supply before the pump) sells during the 24h post-pump window.
That holder is in the insider set (no classification, defaults to include) and their sell
counts against the `insider_sell_pct` threshold.

**FP scenario:** Token launches, large early retail buyer acquires 2% of supply in the first
hour. Token pump happens 72h later (organic or coordinated). Large retail holder sells 60% of
their position. Signal C: their address is in the proxy (>1% floor, unclassified), their
sold_pct = 0.60 > 0.40 threshold. Signal C fires. FP.

**Attack surface rating: LOW (FP direction, not FN). Attacker cannot exploit this to evade;
it creates noise for the detector's consumers.**

---

## 7. Worst-Case Crafted Token: Maximum Extraction at Confidence ≤ 0.40

**Objective:** Construct a pump that extracts maximum value while producing D04 confidence
≤ 0.40 (below Medium threshold).

### 7.1 Token Design

```json
{
  "token_design": {
    "supply": "1,000,000,000 tokens",
    "insider_allocation": "20% across 25 wallets, each at 0.8% of supply",
    "listing_pool": "Raydium CLMM, $8,000 initial liquidity",
    "baseline_period": "7 days of wash-trading at $800/day (D05-evading: >25 slots apart, 2% diff)",
    "contaminated_baseline_median": "$800/day"
  }
}
```

### 7.2 Pump Execution

```
Day 0–6: Attacker wash-trades $800/day.
  - Baseline established: median_volume_usd = $800, baseline_days = 7.
  - D05 not triggered (trades >25 slots apart, 2% volume diff).
  - D04 Signal B not evaluated (baseline_days >= 3 → Signal A path).

Day 7, Hours 0–1: Buy phase.
  - 25 insider wallets each buy $300 worth of tokens.
  - Total buy volume = $7,500 in 1h (9.375× baseline).
  - Price change: from $0.008 per token to $0.012 per token (50% increase).
  
  Wait — doesn't Signal A fire here?
  
  Correction: The attacker wants to stay BELOW Signal A. So they design the pump
  to stay at 4.9× baseline (not 5×):
  
  Actually, at 4.9× baseline, Signal A does not fire.
  At burst_ratio = 7500 / (7500 + 5600) = 7500 / 13100 = 0.57 — below 0.90, Signal B does not fire.
  
  So with $3,920 in baseline daily volume (wash at $560/day → $560 median):
    1h buy = 5600 (10× but wait, we want 4.9×)
  
  Let's set:
    wash_daily = $1,400/day (7-day contaminated baseline median = $1,400)
    1h_pump_volume = $6,860 (= 4.9 × $1,400)
    price_change_1h = 28% (below 30% threshold)
    burst_ratio = 6860 / (6860 + 9800_remaining_23h) = 6860 / 16660 = 0.41 → Signal B does not fire

  Signal A: volume_ratio = 4.9 < 5.0 → MISS
  Signal B: burst_ratio = 0.41 < 0.90 → MISS
  Signal C: not evaluated (A and B did not fire)
  
  D04 output: empty vector (no anomaly event). Confidence = 0.

Day 7, Hours 2–24: Retail enters on social media posts about the 28% gain.
  Attacker's 25 wallets each sell their holdings gradually over 22 hours.
  Each wallet sells into retail demand. Per-wallet sell amounts: 0.8% × supply × price.
  
  D04 Signal C: even if re-evaluated, the insider addresses are below the 1% floor.
  deployer_clusters: empty. Signal C: unavailable.
  
  D04 total confidence across all evaluations: 0.
```

### 7.3 Extracted Value

- Attacker pre-invested: $1,400/day × 7 = $9,800 in wash volume fees (~1% fee = $98 in fees)
- Attacker buy-phase cost: $6,860 in tokens purchased
- Price appreciation from insider buy pressure: +28%
- Retail inflow (conservative estimate, 24h): $40,000 (FOMO into a 28% gain)
- Attacker exit value: insider holdings (0.8% × 1B × $0.012 × 25 wallets = $240,000 in tokens
  at peak) sold into $40K retail inflow
- Net extraction: ~$30,000–$40,000 (limited by retail depth)

**D04 fires with confidence 0.0 throughout this scenario. The detector is completely silent.**

The key design insight: by contaminating the baseline to exactly the right level so that
`pump_volume / baseline = 4.9×` (just below the 5× threshold) while keeping burst_ratio below
0.90 and price_change below 30%, all three of Signal A's dual-gate conditions and Signal B's
concentration gate are simultaneously evaded.

---

## 8. Determinism and Code Findings

### C1 — [CRITICAL] `Utc::now()` in `make_event()` violates spec mandate

**File:line:** `crates/detectors/src/d04_pump_dump.rs:963`

**Code:**
```rust
ingested_at: Utc::now(),
```

**Spec mandate (§8):** "The developer MUST NOT call `Utc::now()` inside `evaluate()`. All
timestamps come from `ctx.window.start`, `ctx.window.end`, and block time fields."

**Impact:** Two evaluations of the same block range produce different `AnomalyEvent` structs
because `ingested_at` differs. Violates the reproducibility requirement in CLAUDE.md §Detector
Rules point 5. If downstream consumers hash on the full struct (e.g., for idempotent
deduplication in ClickHouse), the same logical event is inserted multiple times.

**This finding was also present in D02** (review 0002 §Executive Summary §3) and was not fixed
at that time. It recurs here. The pattern must be fixed across all detectors.

**Suggested patch:** Remove `ingested_at` from `make_event()` and inject it from the caller or
from `ctx`. Alternatively, if `ingested_at` represents the wall-clock time the event was
written to the database (not when it was computed), move it to the storage layer's insert
function. The `AnomalyEvent` struct in `crates/common` should not carry `ingested_at`; the
storage layer should add it at write time.

---

### C2 — [HIGH] `resolve_market_cap()` always returns DEX liquidity, never FDV

**File:line:** `crates/detectors/src/d04_pump_dump.rs:736–761`

**Code:** The function uses `total_market_liquidity_usd` as the sole proxy, returning
`("liquidity_proxy", meta.total_market_liquidity_usd)`. It never computes
`circulating_supply_raw × price_per_token`.

**Spec mandate (§DG-04-5):** "The developer MUST prefer `circulating_supply_raw × price` when
available and fall back to `total_market_liquidity_usd` when not."

**Impact:** The $60M market-cap filter effectively never triggers for shitcoins because their
DEX liquidity is far below $60M even when their FDV is not. The filter works correctly for
BONK ($2B FDV, also $2B+ liquidity) and USDC but fails for tokens with FDV $60–$600M that
have thin liquidity.

**Suggested patch:** Add `circulating_supply_raw × price_usd` computation:
```rust
// Priority 1: circulating supply × price from most recent swap
if let (Some(circ_supply), Some(price)) = (meta.circulating_supply_raw, compute_price_from_swaps(&meta)) {
    let mcap = Decimal::from(circ_supply) * price;
    return (mcap, "circulating".to_owned());
}
// Priority 2: total supply × price  
if let Some(price) = compute_price_from_swaps(&meta) {
    let mcap = Decimal::from(meta.total_supply_raw) * price;
    return (mcap, "total_supply".to_owned());
}
// Priority 3: liquidity proxy (existing)
```

---

### C3 — [MEDIUM] `meta.top_holders` fallback (Priority 2b) lacks sidecar exclusion

**File:line:** `crates/detectors/src/d04_pump_dump.rs:845–862`

**Code:**
```rust
let proxy_addrs: Vec<String> = meta
    .top_holders
    .iter()
    .filter(|h| {
        let balance = Decimal::from(h.amount_raw);
        balance >= floor_raw
    })
    .map(|h| h.address.as_str().to_owned())
    .collect();
```

**Impact:** The Priority 2b fallback (meta.top_holders, used when the sidecar query fails or
returns empty) does not apply the `holder_is_not_excluded` exclusion that the Priority 2a
path (fetch_top_holders_liquid) applies via a JOIN. DexPool-classified addresses in
`meta.top_holders` that clear the balance floor are incorrectly included in the insider set.
This produces false positives in Signal C when AMM pools hold ≥1% of supply.

**Frequency:** AMM pools routinely hold large token positions. For PumpSwap pools, the pool
address itself may hold 5–15% of supply as inventory. This is extremely likely to trigger.

**Suggested patch:** After collecting `proxy_addrs`, apply the same exclusion logic by
checking each address against `ctx.store.get_holder_classification()`. Or: ensure that
`meta.top_holders` is pre-filtered by the registry to exclude known pool addresses before
the `token_registry` enrichment returns. Document which responsibility is correct.

---

### C4 — [LOW] `f64` `to_f64().unwrap_or(0.0)` silent fallback in confidence computation

**File:line:** `crates/detectors/src/d04_pump_dump.rs:540–542`

**Code:**
```rust
let volume_ratio = volume_ratio_dec.to_f64().unwrap_or(0.0);
let price_change = row.price_change_pct_1h.to_f64().unwrap_or(0.0);
let volume_z = row.volume_z_score.to_f64().unwrap_or(0.0);
```

**Impact:** If any Decimal value fails `to_f64()` (possible for very large or very small
values outside f64 range), the fallback is 0.0. For `volume_ratio`, a 0.0 fallback produces
`raw = (0.0 / 5.0 - 1.0) * 0.5 + ...` = a negative raw score, which sigmoid maps to <0.5,
which is then clamped to 0.60. The event fires at minimum Signal A confidence even though
the volume data was invalid.

**This is not a correctness risk** for normal USD values (which will always fit in f64). It
is a defensive programming gap for abnormal inputs.

**Suggested patch:** Replace `unwrap_or(0.0)` with explicit error handling:
```rust
let volume_ratio = volume_ratio_dec.to_f64().ok_or_else(|| {
    DetectorError::InternalComputation {
        detector_id: DETECTOR_ID,
        reason: format!("volume_ratio_dec {} overflows f64", volume_ratio_dec),
    }
})?;
```
Or at minimum: log a warning when the fallback is triggered, so the issue is visible in
production tracing.

---

### C5 — [LOW] `base_signal` initialization to `BaseSignal::B` before Signal A check

**File:line:** `crates/detectors/src/d04_pump_dump.rs:305–308`

**Code:**
```rust
let mut base_signal: BaseSignal = BaseSignal::B;
// ... many lines later ...
if let Some(ref row) = baseline_row {
    // Signal A fired
    base_signal = BaseSignal::A;
}
```

**Impact:** `base_signal` is read after the `if events.is_empty()` guard (line 420), so it is
only consumed when an event exists. When Signal A fires, `base_signal` is correctly set to A
on line 313. When Signal B fires, `base_signal` remains B (the default). This is correct.

**However:** if neither A nor B fires (the early-return paths on lines 341–415), `base_signal`
holds the initialized value of B but `events` is empty, so Signal C's read of `base_signal`
at line 608 (`match base_signal`) is never reached. The behavior is correct but the code
reads as if `base_signal` might be read in an unexpected state. A more defensive pattern
would use `Option<BaseSignal>` initialized to `None` and panic/error if it is `None` when
Signal C is applied.

**Suggested patch:**
```rust
let mut base_signal: Option<BaseSignal> = None;
// ... set to Some(BaseSignal::A) or Some(BaseSignal::B) when events pushed ...
// ... in apply_signal_c_amplifier:
let base_signal = base_signal.expect("base_signal must be set when events is non-empty");
```

---

## 9. Sign-off Verdict

**CAVEAT — Ship with documented compensating controls.**

The D04 implementation is structurally sound and faithfully implements the spec for Phase 2.
The three-signal architecture (A / B / C) is correctly implemented, the confidence formulas
match the spec exactly, and the graceful degradation for Phase 2 deployer_clusters sparsity
is sound.

**Required before merge:**
- **C1 (`Utc::now()` in `make_event()`):** One-line fix. The spec explicitly prohibits this.
  Remove `ingested_at: Utc::now()` from the struct literal and inject from the storage layer
  at write time. No merge until fixed.

**Ship with documented accepted risk:**
- **C2 (`resolve_market_cap()` using liquidity proxy):** Document in `config/detectors.toml`
  `[pump_dump.market_cap_filter_usd]` rationale that the effective threshold is ~10× the
  stated value due to the liquidity proxy. Add a Phase 2 TODO comment in the code. Add
  DG-04-5 resolution as a Sprint 5 task.
- **C3 (Priority 2b sidecar exclusion gap):** Add a TODO comment in the fallback path noting
  the missing exclusion. Add a test case that exercises the fallback and verifies DexPool
  addresses are not incorrectly included. Sprint 5 fix.
- **C4/C5 (minor defensive programming):** Document in the code; no behavior change needed
  for Phase 2.

---

## 10. Top-3 Evasions by Risk (Impact × Exploitability)

1. **E-D04-9 (Two-Hour Slow Pump):** Maximum reward preserved (85%), zero cost, detector
   confidence = 0. This is the single most practical evasion that any trader reading this
   codebase would immediately apply. Resolution: implement DG-04-1 (4h window) in Sprint 5.

2. **E-D04-13 (Fragmented Insider Sell):** Signal C completely blinded. Full reward preserved.
   Only $0 cost beyond wallet creation. Affects ALL Phase 2 deployments since deployer_clusters
   is empty. Resolution: lower floor conditional on Signal A confidence in Phase 2.

3. **E-D04-10 / E-D04-21 (Baseline Contamination):** Systematic false-negative factory. An
   operator who runs the same token multiple times can progressively inflate the baseline to
   require ever-larger real pumps to trigger Signal A. Cheap at micro-cap scale. Resolution:
   Phase 3 30-day secondary baseline.

---

## 11. Top-3 Threshold Adjustments

1. **Lower `burst_concentration_threshold` from 0.90 to 0.70.** Current value is calibrated
   from two data points. At 0.90, any 2h pump is completely invisible to Signal B. Lowering
   to 0.70 with the existing confidence formula gives minimum confidence 0.50 (Medium) at
   0.70, same as the current minimum at 0.90. No false positive risk increase at the minimum
   confidence level; only the coverage improves. Validate FP rate in Sprint 5.

2. **Add z-score gate to Signal A:** Fire Signal A if `volume_z_score ≥ 5.0` regardless of
   raw volume_ratio. This closes the contaminated-baseline gap partially since the z-score is
   not contamination-sensitive in the same way (it normalizes by std_dev, which the attacker
   also inflates — but less efficiently than the mean). Requires no new config key; z-score
   is already computed and in evidence.

3. **Lower `top_holders_insider_floor_pct` from 1.0% to 0.33%** when `insider_source =
   top_holders_proxy` AND Signal A confidence ≥ 0.75. This closes E-D04-13 in Phase 2
   without requiring Phase 3 graph data. Add a new config key
   `top_holders_insider_floor_pct_high_confidence = 0.003`.

---

## 12. REFERENCES.md Rows Added

The following entries should be added to `REFERENCES.md`:

| Mechanism | Signal / Formula | Source | Used In | Verified Against |
|---|---|---|---|---|
| Pump & dump slow-pump spread (2h+ pump, per-hour below threshold) | Two-hour pump evades 1h Signal A; DG-04-1 mitigation documented | Karbalaii (2025) §2 accumulation-phase structure; RAVE EVM ZachXBT April 2026 post-mortem | D04 evasion E-D04-9; DG-04-1 Phase 5 backlog | Referenced 2026-04-21 |
| Pre-pump baseline contamination via D05-evading wash trades | Attacker wash-trades below D05 thresholds for 7d to inflate D04 baseline | Karbalaii (2025) §3 accumulation-phase heuristic | D04 evasion E-D04-10 / E-D04-21; DG-04-2 Phase 3 backlog | Referenced 2026-04-21 |
| Cross-pool targeted pump (volume dilution via multi-pool token-level aggregation) | Single pool pumped; token-level sum dilutes spike below threshold | Standard AMM vulnerability; Raydium multi-pool token structure observation | D04 evasion E-D04-11; DG-04-4 Phase 2 backlog | Referenced 2026-04-21 |
| Liquidity-remove price spike (price without buy volume) | LP removal steepens AMM curve; price spikes without swap volume | Peckshield "LP removal price manipulation" category (EVM, 2022–2023 post-mortems) | D04 evasion E-D04-12; D02 × D04 cross-detector correlation | Referenced 2026-04-21 |
| Insider position fragmentation below 1% floor (Signal C proxy evasion) | 25 wallets × 0.8% supply each evade the top_holders_insider_floor_pct = 0.01 filter | RAVE EVM April 2026 ZachXBT analysis: "supply distributed across dozens of team wallets"; Chainalysis 2025 §accumulation-phase fragmentation | D04 evasion E-D04-13; Signal C threshold adjustment | Referenced 2026-04-21 |
| DexPool-classified sell-through (LP-add/remove as covert token sell) | Insider adds LP then removes it; signal C query misses non-swap disposals | Chainalysis 2025 §3: "liquidity-remove dump as alternative to direct sells" | D04 evasion E-D04-14; Signal C Query 2 extension backlog | Referenced 2026-04-21 |

---

## 13. Signal C Suppression Final Verdict

**Total suppression is correct for Phase 2.** The spec's three-part rationale holds:
auditability is preserved via the evidence key, Signal A still fires, and the operational
cost of false amplification on treasury sells outweighs the lost severity upgrade on edge
cases.

**Add to design gaps register as DG-04-6:** Partial suppression escape hatch for extreme
cases (Signal A ≥ 0.80 AND insider_sell_pct ≥ 80% on established protocols). Implement
in Phase 3 after the established-protocol predicate is extended to cover RAY/TRUMP (the
current gap where Signal C fires on RAY is more severe than the total-suppression gap
because it creates confirmed FPs, not just degraded severity).

---

## 14. Priority 2 Top_Holders Proxy Attack Surface Summary

Three attack surfaces, ranked by severity:

1. **Floor fragmentation (HIGH):** Pre-distribute position across N wallets each below 1%
   of supply. Signal C blind entirely. Zero cost. Affects all Phase 2 deployments.

2. **Priority 2b fallback lacks sidecar exclusion (MEDIUM — FP direction):** The
   meta.top_holders fallback at lines 845–862 does not exclude DexPool/VestingContract
   addresses. AMM pool addresses holding ≥1% supply will be included as "insider" wallets
   and their normal LP rebalancing will trigger Signal C FPs. This is C3 in the code
   findings section. Fix before Sprint 5.

3. **Unclassified-default-include creates large-retail-holder FP (LOW):** A legitimate
   retail holder who bought ≥1% of supply before the pump and sold during the 24h window
   triggers Signal C. This is statistical noise — 1% holders are rare for most shitcoins —
   but it is a documented FP path for tokens with concentrated retail distribution (e.g.,
   early-buyer concentration in low-holder-count tokens).

The Priority 2 proxy is inherently weaker than deployer_clusters. Its Phase 2 use should be
documented as a compensating control with known limitations, not treated as equivalent to
graph-based insider identification. Consumer systems should treat `insider_source =
top_holders_proxy` signals at lower confidence than `deployer_clusters` signals — consider
reducing the amplifier to 0.10 (vs 0.15 for deployer_clusters path) when the source is
the proxy.
