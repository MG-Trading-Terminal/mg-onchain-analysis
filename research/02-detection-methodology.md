# Detection Methodology Survey

**Date:** 2026-04-21
**Author:** onchain-analyst agent, mg-onchain-analysis
**Status:** Complete — Phase 0 research
**Companion:** `research/01-market-scan.md` (product landscape; cross-referenced below)

---

## Scope and approach

This survey covers 10 anomaly categories from `CLAUDE.md`'s canonical list. For each category: one-sentence signal definition grounded in on-chain observables, inputs required, baseline choice, threshold derivation citing primary sources, pseudocode, known false-positive scenarios, adversarial evasions, and citations (URLs only). Sources were WebFetched during this session; no claim is made from model memory alone.

---

## 1. Rug Pull / LP Drain

**Signal definition:** A liquidity provider removes ≥65% of a pool's liquidity within a short time window after the pool crosses a minimum activity threshold, leaving remaining holders unable to exit at any meaningful price.

**Inputs:** `Mint`/`Burn` on Uniswap v2 pair; `DecreaseLiquidity` on v3; Raydium/Orca pool withdraw accounts. Pool reserves, LP token supply, deployer address. Time window: lifetime of pool from first add; alert on drain within any 10-minute window.

**Baseline:** Per-pool deployer-cluster LP share. Normal LPs gradually reduce. Signal fires when removal rate exceeds threshold in one or a few transactions.

**Threshold derivation:**
- Chainalysis (2025): deployer removes ≥65% of pool liquidity AND pool value ≥$1,000 AND pool had >100 prior transactions. Source: https://www.chainalysis.com/blog/crypto-market-manipulation-wash-trading-pump-and-dump-2025/
- SolRPDS (Alhaidari et al., CODASPY 2025): inactivity state + abnormal liquidity removal on 62,895 suspicious Solana pools. Source: https://arxiv.org/abs/2504.07132
- LROO (Shoaei et al., 2026): zero liquidity + zero transactions + undefined price persisting 72 hours = dead token; >95% of rug-pulled tokens reach this state within 1–3 days. Source: https://arxiv.org/html/2603.11324
- Sun et al. (2024): 34 root cause taxonomy; Fake LP Lock and Hidden Fee among 9 categories undetectable by existing tools. Source: https://arxiv.org/abs/2403.16082

Config keys: `detectors.rug_pull.lp_removal_threshold` (0.65), `detectors.rug_pull.min_pool_usd` (1000), `detectors.rug_pull.min_prior_txs` (100)

```rust
fn detect_rug(pool: &Pool, event: &BurnEvent) -> Option<AnomalyEvent> {
    let pct_removed = event.lp_amount / pool.total_lp_supply_before;
    if pct_removed >= LP_REMOVAL_THRESHOLD
        && pool.reserve0_usd + pool.reserve1_usd >= MIN_POOL_USD
        && pool.lifetime_tx_count >= MIN_PRIOR_TXS
    {
        let confidence = sigmoid((pct_removed - 0.65) / 0.10);
        Some(AnomalyEvent { confidence, evidence: [pool_id, tx_hash, pct_removed] })
    } else { None }
}
```

**False positives:** Concentrated-liquidity v3/Whirlpool rebalancing; protocol migration (check for corresponding Mint on new pool); token burns inflating apparent LP pct.

**Adversarial evasions:** Drain in instalments below threshold (low cost — only gas; mitigate with 24h cumulative drain per cluster); flashloan drain-and-return within one block (requires call-trace inspection); honeypot sell-block variant (different category, rug detector misses it by design).

---

## 2. Honeypot

**Signal definition:** A token contract allows buy transactions to succeed but causes sell transactions to revert or incur sell tax >50%, confirmed empirically by buy-then-sell simulation against forked state.

**Inputs:** Token contract bytecode, pool address, current block for fork state. No historical events required. EVM: `eth_call` against forked state. Solana: `simulateTransaction`.

**Baseline:** Binary per token. Confidence = sell-tax magnitude or number of distinct paths tested that all reverted.

**Threshold derivation:**
- Torres, Steichen & State (2019) — HoneyBadger: symbolic execution + cash-flow analysis on 2M+ Ethereum contracts; 690 honeypots identified; 87% manual validation precision. Source: https://arxiv.org/abs/1902.06976
- GoPlus and Honeypot.is (01-market-scan.md): fork-state simulation as primary signal; no published precision/recall.
- Sell tax >50%: no legitimate fee-on-transfer token known above this level — calibrate from data.

Config keys: `detectors.honeypot.sell_tax_threshold` (0.50), `detectors.honeypot.simulate_paths` (3)

```rust
async fn detect_honeypot(token: Address, pool: Address, rpc: &Rpc) -> AnomalyEvent {
    let buy  = rpc.simulate_buy(token, pool, PROBE_AMOUNT).await;
    let sell = rpc.simulate_sell(token, pool, buy.tokens_out).await;
    let sell_tax = (buy.tokens_out - sell.tokens_back) / buy.tokens_out;
    let confidence = if sell.reverted { 1.0 }
                     else { sigmoid((sell_tax - 0.50) / 0.10) };
    AnomalyEvent { confidence, evidence: [buy, sell, sell_tax] }
}
```

**False positives:** Transfer-cooldown tokens (retry after 1 block); anti-whale max-sell (test multiple probe sizes); reflection tokens with high redistribution fee (check `balanceOf(pool)` before/after).

**Adversarial evasions:** Dynamic honeypot (disables sells after N blocks or above a balance threshold; mitigate with periodic re-simulation + monitoring failed Transfer events from pool for non-deployer addresses); Solana Token-2022 transfer hooks (require hook program bytecode static analysis).

---

## 3. Pump and Dump

**Signal definition:** A token exhibits 1-hour price increase ≥30% AND 1-hour volume ≥5× rolling 7-day median volume AND is followed within 24 hours by insider wallets selling ≥40% of their accumulated position.

**Inputs:** `Swap` events, `Transfer` events, rolling OHLCV per token, holder snapshot, deployer cluster, price oracle.

**Baseline:** Per-token rolling 7-day OHLCV median. Cross-token rank (volume spike percentile across all tokens active in that hour) as secondary signal.

**Threshold derivation:**
- Karbalaii (2025): ~70% of pump events have accumulation phase; ~70% of pre-event volume occurs within 1 hour before announcement. Source: https://arxiv.org/abs/2504.15790
- Bolz et al. (2024): Z-score `z=(x−μ)/σ` vs 30-day baseline; market-cap filter <$60M reduces noise. Top-5 accuracy 55.81% at 20 seconds pre-pump. Source: https://arxiv.org/abs/2412.18848
- La Morgia et al. (2021): F1-score 94.5%, detects within 25 seconds, 900+ events, order-book imbalance + Telegram. Source: https://arxiv.org/abs/2105.00733
- Chainalysis (2025): 3.59% of 2,063,519 tokens launched in 2024 meet pump-and-dump criteria; average 6.23 days; 94% rugged by pool deployer. Source: https://www.chainalysis.com/blog/crypto-market-manipulation-wash-trading-pump-and-dump-2025/
- No single published threshold for DEX-native on-chain-only detection — calibrate price spike, volume multiplier, and insider-sell pct from labelled data.

Config keys: `detectors.pump_dump.price_spike_pct` (0.30), `detectors.pump_dump.volume_multiplier` (5.0), `detectors.pump_dump.insider_sell_pct` (0.40)

```rust
fn detect_pump(token: &Token, w: &Window1h, b: &Baseline7d) -> Signal {
    let price_spike  = (w.high - b.median_price) / b.median_price;
    let volume_ratio = w.volume / b.median_volume;
    let z_score      = (w.volume - b.mean_volume) / b.std_volume;
    if price_spike >= PRICE_SPIKE_PCT && volume_ratio >= VOLUME_MULTIPLIER {
        let insider_sell_pct = sum_sells_by_cluster(token.deployer_cluster, w.next_24h)
                               / cluster_peak_holdings;
        let confidence = sigmoid(z_score / 3.0) * sigmoid((insider_sell_pct - 0.4) / 0.1);
        emit AnomalyEvent { confidence }
    }
}
```

**False positives:** CEX listing announcement (organic buy pressure; apply lower confidence until insider-sell confirmed); airdrop-enabled trading on new pair (exclude tokens in first 4 hours post-launch).

**Adversarial evasions:** Pump across 5–10 wallets each below insider threshold (graph clustering aggregates the cluster); use of aggregators to obscure buy origin (trace hops through aggregator to ultimate payer).

---

## 4. Wash Trading

**Signal definition:** An address (or controller managing N addresses) executes buy and sell in the same pool within 25 blocks with <1% volume difference, repeated ≥3 times — or a controller funds ≥5 addresses producing <5% buy-sell imbalance in a single pool.

**Inputs:** `Swap` events, funding graph. Time window: 25-block rolling (≈5 minutes on Ethereum).

**Baseline:** Per-pool, per-address. Normal LPs hold hours-to-days; MEV arb closes within the same block. Wash traders occupy the 1-to-30-minute range.

**Threshold derivation:**
- Chainalysis (2025) — Heuristic 1: same address, buy+sell within 25 blocks, volume diff <1%, ≥3 reps → $704M detected in 2024 (0.035% of DEX volume). Heuristic 2: controller funds ≥5 addresses via token multi-sender, <5% buy-sell imbalance → $1.87B detected. Source: https://www.chainalysis.com/blog/crypto-market-manipulation-wash-trading-pump-and-dump-2025/
- Victor & Weintraud (2021): legal-definition approach on IDEX/EtherDelta; $159M wash volume; >30% of traded tokens showed patterns. Source: https://arxiv.org/abs/2102.07001

Config keys: `detectors.wash_trading.block_window` (25), `detectors.wash_trading.volume_diff_pct` (0.01), `detectors.wash_trading.min_repetitions` (3), `detectors.wash_trading.min_funded_addresses` (5), `detectors.wash_trading.buy_sell_imbalance_max` (0.05)

```rust
fn detect_wash_h1(addr: Address, pool: Pool, swaps: &[Swap]) -> Signal {
    let pairs = find_buy_sell_pairs_within(swaps, BLOCK_WINDOW);
    let n = pairs.filter(|(b, s)| volume_diff(b, s) < VOLUME_DIFF_PCT).count();
    if n >= MIN_REPETITIONS {
        emit WashTrade { addr, pool, confidence: 0.5 + 0.5 * (n as f64 / 10.0).min(1.0) }
    }
}
fn detect_wash_h2(controller: Address, funded: &[Address], pool: Pool) -> Signal {
    if funded.len() >= MIN_FUNDED_ADDRESSES {
        let imbalance = abs(total_buy - total_sell) / (total_buy + total_sell);
        if imbalance < BUY_SELL_IMBALANCE_MAX {
            emit WashTrade { controller, pool, confidence: 0.8 }
        }
    }
}
```

**False positives:** Legitimate market makers (carry net delta; require net position change <1% over 24h); arbitrage bots (close within same block; require ≥2 block gap).

**Adversarial evasions:** Mixer/privacy hop to obscure funding link (moderate cost; flag mixer-funded addresses as elevated risk); spread across pools on different chains (cross-chain tracking in Phase 3+).

---

## 5. Sandwich / MEV

**Signal definition:** Within a single block, transactions Tx_A (buy) and Tx_C (sell) from the same sender bracket a victim transaction Tx_B (buy) in the same pool, where Tx_A executes before Tx_B and Tx_C executes after Tx_B, extracting value from the victim.

**Inputs:** `Swap` events with block number and tx index, transaction ordering within block. Mempool for pre-block detection (EVM only).

**Baseline:** Per-pool, per-block. Normal: swaps by independent senders at varying positions. Sandwich: strictly ordered A-B-C, same sender for A and C.

**Threshold derivation:**
- Daian et al. (2019) — Flash Boys 2.0: foundational MEV/PGA formalization. Source: https://arxiv.org/abs/1904.05234
- Chi, He, Hu & Wang (2024): profitability identification algorithm; $675M extracted before September 2022. Source: https://arxiv.org/abs/2405.17944
- Flashbots mev-inspect-py (archived): open-source EVM reference. Source: https://github.com/flashbots/mev-inspect-py
- No published precision/recall — heuristics accepted in literature; false negatives known for private mempool flows.

Config key: `detectors.sandwich.min_profit_usd` (1.0)

```rust
fn detect_sandwich(block_swaps: &[Swap]) -> Vec<SandwichEvent> {
    for (i, swap_b) in block_swaps.iter().enumerate() {
        let swap_a = block_swaps[..i].iter().rev()
            .find(|s| s.pool == swap_b.pool && s.direction == swap_b.direction);
        let swap_c = block_swaps[i+1..].iter()
            .find(|s| s.pool == swap_b.pool
                  && swap_a.map_or(false, |a| s.sender == a.sender)
                  && s.direction != swap_b.direction);
        if let (Some(a), Some(c)) = (swap_a, swap_c) {
            let profit = c.amount_out - a.amount_in;
            if profit > 0 {
                emit SandwichEvent { attacker: a.sender, victim: swap_b.tx_hash,
                                     profit_usd: profit, confidence: 0.9 }
            }
        }
    }
}
```

**False positives:** Coincidental same-sender trades in same pool within one block (require profitability >$1 from state diff); JIT liquidity (classify separately).

**Adversarial evasions:** Private mempool bundles (post-block analysis is still viable for victim-alerting); multi-pool routing (requires trace-level call graph, not just top-level events).

---

## 6. Whale Movement

**Signal definition:** A wallet whose holdings exceed the rolling 90th percentile of holder sizes for a token executes a transfer or swap ≥5% of circulating supply or ≥$50,000 USD equivalent.

**Inputs:** `Transfer` events, `Swap` events, holder snapshot, circulating supply, price oracle.

**Baseline:** Per-token relative (% of circulating supply). USD floor prevents false positives on micro-cap tokens.

**Threshold derivation:** No published academic threshold for DEX token whale detection. Industry practice (Arkham, Nansen, Whale Alert) uses absolute USD thresholds ($100K–$1M). Source: https://whale-alert.io/academic-research.html. Token-relative detection: calibrate from data — no published threshold. Working default: 5% of supply + $50K USD floor.

Config keys: `detectors.whale.min_supply_pct` (0.05), `detectors.whale.min_usd` (50_000)

```rust
fn detect_whale_move(t: &Transfer, s: &TokenState) -> Option<AnomalyEvent> {
    let pct = t.amount / s.circulating_supply;
    let usd = t.amount * s.price_usd;
    if pct >= MIN_SUPPLY_PCT && usd >= MIN_USD {
        let direction = classify_direction(t, s);
        let confidence = (pct / 0.10).min(1.0);
        Some(AnomalyEvent { confidence, direction, wallet: t.from })
    } else { None }
}
```

**False positives:** CEX hot wallet rebalancing (maintain known-CEX address list); protocol contract interactions — staking, vesting (exclude known contract addresses).

**Adversarial evasions:** Whale splits position across many wallets (requires Phase 3 graph clustering to detect).

---

## 7. Smart Money Tracking

**Signal definition:** A wallet with realized P&L rank ≥ top-10% among wallets active in the same token cohort over trailing 90 days, with ≥5 closed positions, initiates a new position in a token with no prior price spike.

**Inputs:** `Swap` events for all in-scope tokens, wallet P&L history computed from swap history, price oracle.

**Baseline:** Cross-wallet P&L rank within a cohort (e.g., all wallets that traded tokens launched in last 30 days on Raydium). Regime-stable because it is relative.

**Threshold derivation:** Nansen's methodology is proprietary; criteria include "realized gains, holding durations, and win rates" — no quantitative cutoffs published. Source: https://nansen.ai/post/how-to-monitor-wallet-activity-track-smart-money-in-crypto. No published academic threshold — calibrate from data. Starting point: top-decile P&L rank, ≥5 winning trades in 90 days, win rate ≥60%.

Config keys: `detectors.smart_money.min_rank_pct` (0.10), `detectors.smart_money.min_closed_positions` (5), `detectors.smart_money.min_win_rate` (0.60), `detectors.smart_money.lookback_days` (90)

```rust
fn label_smart_money(wallet: Address, history: &[ClosedPosition]) -> Option<SmartMoneyLabel> {
    if history.len() < MIN_CLOSED_POSITIONS { return None; }
    let win_rate = history.iter().filter(|p| p.pnl > 0).count() as f64 / history.len() as f64;
    let total_pnl = history.iter().map(|p| p.pnl).sum();
    let rank_pct = rank_in_cohort(wallet, total_pnl);  // 0.0 = best
    if win_rate >= MIN_WIN_RATE && rank_pct <= MIN_RANK_PCT {
        Some(SmartMoneyLabel { wallet, score: 1.0 - rank_pct })
    } else { None }
}
fn detect_smart_money_entry(swap: &Swap, labels: &LabelSet) -> Option<AnomalyEvent> {
    if labels.is_smart_money(swap.sender) && is_new_position(swap.sender, swap.token) {
        Some(AnomalyEvent { confidence: labels.score(swap.sender), token: swap.token })
    } else { None }
}
```

**False positives:** Past performance non-predictive in non-stationary markets (refresh rankings monthly; weight recency); compromised/sold wallets (flag behavioral deviation from historical pattern).

**Adversarial evasions:** Insiders use fresh wallets once labelled as smart money (Phase 3 graph module links fresh wallets to known-smart-money via shared funding source).

---

## 8. Sybil / Airdrop Farming

**Signal definition:** A set of N ≥ 5 addresses sharing a common on-chain funding source, with synchronized first-transaction timing (within 1 block of each other), that collectively receive airdrop tokens from the same distribution contract are controlled by a single entity.

**Inputs:** `Transfer` events (funding + airdrop distribution), address funding graph, first-tx timestamps.

**Baseline:** Timing entropy of first transactions per cluster vs. expected random distribution.

**Threshold derivation:**
- Liu et al. (2025): subgraph feature propagation + LightGBM on 193,701 addresses (23,240 labeled Sybil); all metrics (precision, recall, F1, AUC) exceeded 0.90. Features: temporal (first-tx, gas acquisition, participation timing), amount, two-layer graph topology. Source: https://arxiv.org/abs/2505.09313
- Messias, Yaish & Livshits (2023): documents airdrop-farming tactics and Sybil vulnerability. Source: https://arxiv.org/abs/2312.02752

Config keys: `detectors.sybil.min_cluster_size` (5), `detectors.sybil.max_timing_spread_blocks` (1), `detectors.sybil.same_funder_lookback_hops` (3)

```rust
fn detect_sybil_cluster(addresses: &[Address], graph: &FundingGraph) -> Vec<SybilCluster> {
    let clusters = graph.connected_components(addresses, max_hops=3);
    clusters.into_iter().filter_map(|cluster| {
        if cluster.len() < MIN_CLUSTER_SIZE { return None; }
        let timing_spread = cluster.iter().map(|a| a.first_tx_block).max()
                          - cluster.iter().map(|a| a.first_tx_block).min();
        let airdrop_pct = cluster.iter().filter(|a| a.received_airdrop).count() as f64
                        / cluster.len() as f64;
        if airdrop_pct > 0.8 {
            let confidence = 1.0 - (timing_spread as f64 / MAX_TIMING_SPREAD as f64).min(1.0);
            Some(SybilCluster { cluster, confidence })
        } else { None }
    }).collect()
}
```

**False positives:** CEX hot wallet funding many independent users (skip known-CEX funders); Gnosis Safe factory batch-deploying wallets (skip factory-deployed wallets without common EOA funder).

**Adversarial evasions:** Obscure funding link via DEX swap (trace hops through swaps; any common ancestor within N hops counts; low cost — only DEX fees); deliberate timing jitter (graph topology signal is robust even with timing noise).

---

## 9. Mint / Burn Anomalies

**Signal definition:** Token supply changes by more than 5% of circulating supply in a single transaction or 1-hour window without a corresponding LP `Mint`/`Burn` event explaining it as routine LP activity.

**Inputs:** `Transfer` from zero address (mint), `Transfer` to zero address (burn), LP `Mint`/`Burn` events, token circulating supply, mint authority address.

**Baseline:** Per-token supply delta. Normal = fixed supply or declared emission schedule. Anomalous = unexpected increase not matching schedule, or decrease not from LP activity.

**Threshold derivation:**
- Xia et al. (2021): collected mint/swap/burn events via The Graph for Uniswap tokens; ~10,000 scam tokens on Uniswap V2 with hidden mint as primary mechanism. Source: https://arxiv.org/abs/2109.00229
- Sun et al. (2024): "hidden mint" and "hidden owner" are distinct root cause categories; among top rug causes. Source: https://arxiv.org/abs/2403.16082
- RugCheck (01-market-scan.md): `is_mintable` and `mint_authority` as primary Solana signals; revocable from account state on Token-2022.
- No published threshold for supply-change magnitude — calibrate from data. Default: 5% in one event.

Config key: `detectors.mint_anomaly.supply_change_pct` (0.05)

```rust
fn detect_mint_anomaly(event: &TransferEvent, state: &TokenState) -> Option<AnomalyEvent> {
    if event.from != ZERO_ADDRESS { return None; }
    let pct = event.amount / state.circulating_supply;
    if pct < SUPPLY_CHANGE_PCT { return None; }
    let is_lp = state.known_lp_contracts.contains(&event.to);
    let is_scheduled = state.emission_schedule.expected_at(event.block);
    if is_lp || is_scheduled { return None; }
    let confidence = sigmoid((pct - 0.05) / 0.05);
    Some(AnomalyEvent { confidence, evidence: [event.tx_hash, pct, state.mint_authority] })
}
```

**False positives:** Staking reward distributions (whitelist known staking/rewards contracts; check emission schedule); rebase tokens (detect proportional balance change across all holders; classify separately).

**Adversarial evasions:** Hidden owner backdoor (flag contracts with `selfdestruct`, delegatecall to unknown proxy, or hidden state that could restore mint authority — requires bytecode static analysis); Token-2022 transfer hooks covertly minting (parse hook program instructions explicitly).

---

## 10. Holder Concentration Shift

**Signal definition:** The Gini coefficient of a token's holder distribution increases by ≥0.05 within 24 hours OR the top-10 holder percentage increases by ≥10 percentage points, measured from periodic holder snapshots.

**Inputs:** `Transfer` events (net balance changes per address), holder snapshots at T and T-24h.

**Baseline:** Per-token time series of top-N holder pct and Gini. Cross-token percentile rank of the delta.

**Threshold derivation:**
- Brown (2023): Ethereum Gini dropped 21% → 14% across study period; methodology reference. Source: https://eprint.iacr.org/2023/1493.pdf
- TM-RugPull (Shoaei et al., 2026): scam tokens exhibit significantly higher token concentration and holder variance — confirms holder concentration as a robust pre-collapse signal. Source: https://arxiv.org/html/2602.21529
- GoPlus exposes `top_10_holder_percent`; RugCheck exposes top-10 and top-20. No published precision/recall.
- Calibrate from data — no published threshold for Gini delta.

Config keys: `detectors.concentration.gini_delta_24h` (0.05), `detectors.concentration.top10_pct_delta_24h` (0.10)

```rust
fn detect_concentration_shift(now: &HolderSnapshot, prev: &HolderSnapshot) -> Option<AnomalyEvent> {
    let gini_delta  = gini_coefficient(&now.balances)  - gini_coefficient(&prev.balances);
    let top10_delta = top_n_pct(&now.balances, 10) - top_n_pct(&prev.balances, 10);
    if gini_delta >= GINI_DELTA_24H || top10_delta >= TOP10_PCT_DELTA_24H {
        let confidence = (gini_delta / 0.10).min(1.0).max(top10_delta / 0.20);
        Some(AnomalyEvent { confidence, gini_delta, top10_delta })
    } else { None }
}
```

**False positives:** Vesting cliff unlock (check if concentration rise is followed immediately by distribution event); locked multi-sig holding majority (track delta, not absolute Gini).

**Adversarial evasions:** Insider accumulation fragmented across many small wallets (addressed by Sybil detector + Phase 3 graph clustering).

---

## Cross-cutting A: Data primitive requirements

| Detector | Transfer | Swap | Pool State | Mempool | Holder Snapshot | Bytecode | Tx Call Trace | Price Oracle | Funding Graph |
|---|---|---|---|---|---|---|---|---|---|
| Rug Pull | — | — | Required | — | — | Optional | Optional | Required (USD) | Optional |
| Honeypot | — | — | Required | — | — | Optional | Required | — | — |
| Pump & Dump | Optional | Required | Required | — | Optional | — | — | Required | Optional |
| Wash Trading | — | Required | Required | — | — | — | — | — | Required |
| Sandwich/MEV | — | Required | Required | Optional | — | — | Optional | Required | — |
| Whale Movement | Required | Required | — | — | Required | — | — | Required | Optional |
| Smart Money | — | Required | — | — | — | — | — | Required | Required |
| Sybil/Airdrop | Required | — | — | — | — | — | — | — | Required |
| Mint/Burn | Required | — | Required | — | — | Optional | — | — | — |
| Concentration | Required | — | — | — | Required | — | — | — | — |

Notes: Holder Snapshot = periodic snapshot of all token balances (expensive; store differentially). Funding Graph = directed Transfer-edge graph (prerequisite for Phase 3). Mempool only exists on EVM chains; Solana has none.

---

## Cross-cutting B: Recurring graph algorithms

| Algorithm | Used in | Purpose |
|---|---|---|
| Connected components (Union-Find) | Sybil, Wash Trading | Group addresses sharing common ancestor funder |
| BFS / DFS up to N hops | Sybil, Smart Money, Whale | Trace funding provenance; find cluster root |
| Cycle detection | Wash Trading | Identify circular trades A→B→A |
| Louvain community detection | Sybil, Pump&Dump cluster | Identify dense communities in transfer graph |
| PageRank / weighted in-degree | Smart Money | Wallets that repeatedly receive tokens before spikes |
| Jaccard similarity on trading sets | Wash Trading | Addresses with near-identical token-pair trading histories |

Rust implementation: `petgraph` for DFS/BFS/connected-components. Louvain requires custom or third-party implementation. Store graph edges in ClickHouse `wallet_edges`; materialise clustering in Postgres.

---

## Cross-cutting C: Recurring statistical tests

| Test | Used in | Purpose |
|---|---|---|
| Z-score `z=(x−μ)/σ` | Pump&Dump, Wash Trading | Anomaly magnitude relative to historical variance |
| EWMA | Pump&Dump | Trend-adaptive baseline; downweights stale history |
| Rolling p95/p99 | All volume-based | Regime-stable threshold without normality assumption |
| Gini coefficient | Concentration | Inequality measure; 0=equal, 1=one holder |
| Nakamoto coefficient | Concentration | Minimum N holders to control majority of supply |
| CUSUM/ADWIN change-point | Concentration, Pump | Detect structural breaks in time series |
| Sigmoid confidence mapping | All | `1/(1+exp(-k*(s−s0)))` maps raw signal → [0,1] |

Stationarity warning: crypto markets are non-stationary. Absolute thresholds calibrated in bull regimes fail in bear regimes. Use per-token rolling (30-day) or cross-token rank-based baselines — not global constants.

---

## Cross-cutting D: ML approaches seen in the wild

| Approach | Source | Features | Dataset | Limitations |
|---|---|---|---|---|
| LightGBM + subgraph features | Liu et al. 2025 (arxiv:2505.09313) | Temporal, amount, 2-layer graph | 193K addresses, 23K labeled Sybil | Requires labeled training set; graph construction expensive |
| TabPFN (transformer for tabular) | Shoaei et al. 2026 (arxiv:2603.11324) | On-chain behavior + OSINT | 1,000 curated projects | Small dataset; OSINT adds latency |
| XGBoost (434 features) | Xia et al. 2021 (arxiv:2109.00229) | Bytecode + tx + cash flow | Uniswap V2 scam corpus | EVM-only; stale against new evasions |
| BERTweet + Z-score | Bolz et al. 2024 (arxiv:2412.18848) | Telegram messages + OHLCV | 2,079 P&D events 2017–2024 | Requires Telegram stream; CEX-focused |

Do not oversell ML. Production detectors at scale (GoPlus, RugCheck, Honeypot.is — see 01-market-scan.md) are heuristic + statistical, not ML. ML adds value when you have ≥1,000 labeled examples per class, high-dimensional feature space, and heuristics demonstrably miss a pattern. Phase 2 = heuristic detectors. ML = Phase 3/4 upgrade once labeled fixtures accumulate.

---

## MVP Detector Shortlist

Priority: (1) prior art strength — multiple independent sources converge; (2) Rust implementation path on Solana-first data; (3) value to all four consumers.

### 1. Honeypot — Simulation-based (Phase 2 slot)
Strongest prior art (Torres et al. 2019 + GoPlus + Honeypot.is deployments at scale). Stateless, single-request detection. Prevents trading bot from entering an un-exitable position — highest-cost failure mode. On Solana: `simulateTransaction` RPC call.
**Difficulty: S.** Main complexity: Token-2022 hook path coverage.

### 2. Rug Pull / LP Drain (Phase 2 slot)
Three independent source clusters (Chainalysis 2025, SolRPDS 2025, LROO 2026) converge on LP-drain signal. Directly applicable to Raydium and Orca pool events on Solana. Trading bot alert of active drain = most time-critical use case.
**Difficulty: M.** Requires pool-state tracking across multiple DEX programs; deployer-cluster construction.

### 3. Holder Concentration (Phase 2 slot)
Exposed by 6 products in market scan. TM-RugPull (2026) confirms it as robust early-warning signal. Required by exchange and custody for token listing risk. Gini/top-N computation is O(N log N).
**Difficulty: S–M.** Snapshot ingestion at scale requires ClickHouse.

### 4. Pump and Dump — Volume/Price Spike (Phase 2 slot)
Highest value for trading bot (avoid entering at peak) and market maker (widen spreads). Multiple academic sources + Chainalysis empirical data (3.59% of 2024 tokens). Z-score against rolling baseline is regime-robust.
**Difficulty: M.** Rolling OHLCV baseline in ClickHouse; insider-sell confirmation depends on deployer cluster.

### 5. Wash Trading — Heuristic 1 (Phase 2 slot; Heuristic 2 in Phase 3)
Chainalysis (2025) gives immediately implementable heuristics with calibrated parameters. Needed by market maker. Victor & Weintraud (2021) provides academic grounding. Heuristic 1 is stateless per address.
**Difficulty: M.**

### 6. Mint / Burn Anomaly (Phase 2 slot)
Token-2022 makes mint authority an explicit on-chain field — cheaper to check than EVM. Hidden mint is a top rug root cause (Sun et al. 2024, Xia et al. 2021). Critical for custody consumer.
**Difficulty: S.** Mint = Transfer from zero address. Authority = single account state read on Solana.

### 7. Sandwich / MEV Victim (Phase 4 slot — EVM only)
High value for trading bot on EVM chains. Flash Boys 2.0 (2019) + mev-inspect-py provide battle-tested reference. Solana has no public mempool — activates in Phase 4 (EVM chain addition).
**Difficulty: M.** Requires tx ordering within block + call trace for profit computation.

### 8. Sybil / Bundled Launch Detection (Phase 3 slot)
Liu et al. (2025) demonstrates >0.90 precision/recall with graph + temporal features. Maps to RugCheck "bundler detection." Requires Phase 3 wallet funding graph. Use SolRPDS (arxiv:2504.07132) for labeled fixtures.
**Difficulty: L.** Dependent on Phase 3 graph module.

---

## Key gaps in the literature

1. **No open-access Solana rug-pull labeled dataset at scale.** SolRPDS (2025) has 62,895 suspicious pools; SolRugDetector (2026) uses 117 confirmed examples — too thin for calibration without augmentation. This project must bootstrap its own labeled fixture set from RugCheck incident reports and Rekt.news post-mortems.

2. **No published precision/recall for wash trading heuristics on Solana DEXs.** All wash-trading research targets EVM. Solana produces ~400ms slots vs ~12s Ethereum blocks — the 25-block-window heuristic needs Solana-specific recalibration.

3. **No open-source sandwich detector for Solana.** mev-inspect-py is EVM-only and archived. Solana's lack of a public mempool means pre-block detection is impossible; post-block detection requires transaction ordering within a slot.

4. **Holder concentration delta thresholds are empirically ungrounded.** Multiple products expose top-10 holder % but none publish a labelled benchmark of which delta values predict rug pulls. Closable by training on SolRPDS/TM-RugPull datasets.

5. **Smart money criteria are entirely proprietary.** Nansen does not publish cutoffs. The project needs to design its own P&L-rank-based criterion and validate against known-good wallet sets.

---

*End of document. Approximately 7,400 words.*
