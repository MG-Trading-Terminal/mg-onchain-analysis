# Design 0021 — D13: Sandwich / MEV Detector (Sprint 20)

**Date:** 2026-04-24
**Status:** Draft — awaiting user sign-off on §11 decisions before implementation
**Author:** onchain-analyst agent
**Sprint:** 20 (S20-1 from SESSION-KICKOFF.md Option A)
**ADR refs:**
- ADR 0001 §D1 — Solana-first; EVM detectors are Phase 4
- ADR 0001 §D5 — MVP detector shortlist; sandwich/MEV declared Phase 4 EVM-only
- ADR 0002 — Postgres-only storage; NUMERIC(39,0) for u128; string-bridged amounts
- ADR 0003 — Self-sovereign infrastructure; no Flashbots Relay API / Eden API / MEV-Boost API
  in production hot path; mev-inspect-py as citation only
- ADR 0005 Decision 2 — `Detector::supported_chains()` override pattern; EVM detectors
  return `&[Chain::Ethereum]`

**Related designs:**
- `docs/designs/0003-detector-trait.md` — Detector trait; `supported_chains()` provided method
- `docs/designs/0008-detector-05-wash-trading.md` — D05 wash trading; same tx-ordering analysis
  spirit (pattern across multiple ordered events in a window)
- `docs/designs/0017-d05-signal-b-graph-cycles.md` — graph cycle detection; precedent for
  same-block multi-event sequence analysis
- `docs/designs/0019-detector-12-permit2-drainer.md` — D12 structural template; most recent
  EVM detector; evidence prefix / suppression / fixture shape patterns followed here
- `docs/designs/0020-server-binary-production-entry.md` — Sprint 19 production wiring;
  D13 will register in `init::detectors::build_all_detectors` as the 13th streaming detector

---

## §1 Background

### §1.1 Sandwich Attack Mechanics

A sandwich attack is the canonical hostile MEV (Maximal Extractable Value) pattern on EVM
blockchains. The name derives from the spatial position of the attacker's transactions around
the victim's in the block ordering:

```
Block N:
  tx[i]   — Attacker FRONT-RUN:  buys token T on pool P (pushes price up)
  tx[i+1] — Victim:              swaps token T on pool P at elevated price (worse execution)
  tx[i+2] — Attacker BACK-RUN:  sells token T on pool P at still-elevated price (collects profit)
```

**Step-by-step mechanics on a constant-product AMM (Uniswap v2 model):**

Let pool P hold reserves `(R_A, R_B)` for token pair (A, B). Victim intends to sell `v` units
of A for B, expecting output `y_victim = v * R_B / (R_A + v)` (ignoring fee for clarity).

1. **Front-run:** Attacker buys token B by selling `f` units of A. New reserves become
   `(R_A + f, R_B - y_front)` where `y_front = f * R_B / (R_A + f)`. Price of B in terms of A
   has increased — B is now more expensive.

2. **Victim execution:** Victim's tx executes at the new reserves. Their input of `v` units of A
   yields `y_victim_actual = v * (R_B - y_front) / (R_A + f + v)`. This is strictly less than
   their expected output — the victim suffers slippage imposed by the front-run.

3. **Back-run:** Attacker sells `y_front` units of B back into the pool at the new reserve state
   `(R_A + f + v, R_B - y_front + y_victim_actual_fee_adjusted)`. The price has been pushed
   further by the victim's trade; the attacker receives more A than they initially spent.

**Net result:**
- Attacker's P&L = (A received in back-run) − (A spent in front-run) > 0
- Victim's effective exchange rate is worse than the pre-block rate by the amount the attacker
  extracted

This is the adversarial MEV form — distinct from benign MEV (arbitrage between pools, which
equilibrates prices and benefits the market). Sandwich attacks impose pure externality on victims.

**Uniswap v3 / concentrated liquidity pools:** The same structure applies. The price impact
of the front-run is a function of the active tick range's liquidity, not total pool liquidity.
Detection mechanics are identical (swap event ordering in a block); the amounts require I256
interpretation per gotcha #62.

### §1.2 Detection Approach: In-Block Tx-Ordering Analysis

Sandwich detection does not require mempool visibility. All three transactions appear in the
same block, in tx-ordering sequence, as committed state. The detector reads committed swap
events from the `swaps` table (populated by the EthereumAdapter's UniV2 + UniV3 decoders,
Sprint 16) and reconstructs the F-V-B (Front-Victim-Back) triple from the block's ordering.

This post-hoc approach has a deliberate 12-second latency (one Ethereum block time) relative
to the attack, but produces a clean, deterministic, reorg-safe signal. Real-time mempool
detection (pre-confirmation alerting) would provide an earlier signal but requires mempool
subscription infrastructure that is explicitly deferred to Sprint 21+ (see §2.2 and §11
Decision 8).

**Key properties of the in-block signal:**
- `block_number` is the natural grouping key — all three txs share it
- `pool_address` (the `pool` column in `swaps`) narrows further — the sandwich must occur
  on the same pool to extract value
- `log_index` within the block reflects tx-ordering (lower `log_index` = earlier in block)
- Attacker address appears in both front-run and back-run swaps (same `to` / `sender` field)

### §1.3 Real-World Scale

The academic and empirical record establishes both the prevalence and profitability of
sandwich MEV:

**Daian et al. 2019 — "Flash Boys 2.0"** (arXiv:1904.05234): The foundational formalization
of priority gas auctions (PGA) and front-running on Ethereum. Introduces the term MEV and
proves that sandwich attacks are a Nash equilibrium strategy for rational block producers.
Establishes that sandwich profit grows monotonically with victim swap size and pool liquidity
depth. The paper specifically identifies Uniswap v1 (constant-product AMM) as the primary
attack venue.

**Chi, He, Hu & Wang 2024** (arXiv:2405.17944): Profitability-based empirical study of MEV
extraction on Ethereum mainnet before September 2022. Key results:
- $675M total MEV extracted across all categories
- Sandwich attacks constitute approximately 35–40% of total MEV by value
- Median sandwich profit: ~$32 per attack; 95th percentile: ~$1,800
- Minimum profitable sandwich requires victim swap size ≥ ~$500 on typical Uniswap v2 pools
- Chi et al. classify sandwiches using a profit-based criterion: attacker's net change in
  reserve-equivalent assets must be positive after the back-run; this is the basis for the
  A2 profit-only signal option in §11 Decision 1

**Flashbots mev-inspect-py** (github.com/flashbots/mev-inspect-py, archived): The reference
open-source Ethereum MEV classification system. Their sandwich detection uses:
- Block-level grouping by pool address
- Transaction index ordering within the block
- Same attacker address in front + back swap
- Net USD profit calculation using Uniswap pool reserves
This implementation is the closest available reference for our A1 strict 3-swap signal.

**CoW Protocol, Flashbots Protect, 1inch Fusion:** MEV-protection layers that aggregate
user intent off-chain and settle on-chain as batch trades. Their settlement contracts
(`0x9008D19f58AAbD9eD0D60971565AA8510560ab41` for CoW Protocol Settlement,
`0xC92E8bdf79f0507f65a392b0ab4667716BFE0110` for Flashbots Protect relay, 1inch Fusion
auction contract) appear in sandwich-like patterns in the event stream but are legitimate
batch settlements. These must be in the suppression allowlist (§5.2).

### §1.4 How D13 Differs from D12 (Permit2 Drainer)

D12 detects a victim-specific, single-transaction loss event (Permit2 drain). D13 detects
an adversarial extraction pattern that spans three transactions in a single block and harms
victims through price degradation rather than direct asset removal.

| Dimension | D12 Permit2 Drainer | D13 Sandwich / MEV |
|-----------|---------------------|-------------------|
| Evidence shape | One tx: Permit + Transfer | Three txs in same block: front-run → victim → back-run |
| Victim harm type | Direct asset drain (tokens removed from wallet) | Indirect slippage (worse swap rate than expected) |
| Attacker signal | Known-drainer address OR same-tx Permit+Transfer correlation | Same address in front-run + back-run on same pool |
| Pool involvement | None (Permit2 contract; pool is incidental) | Central — pool P is the extraction venue |
| Severity | Critical (direct loss of tokens) | Medium to High (slippage, not total loss) |
| Suppression | NOT on established protocols (USDC/WETH are prime targets) | Suppress builder-extracted MEV; allowlist CoW/Flashbots Protect settlement contracts |
| Chain | Ethereum | Ethereum (`supported_chains()` override) |
| Migration | V00014 `permit2_events` | V00015 `mev_events` IF C2/C3 chosen (see §11 Decision 3) |

---

## §2 Goals and Non-Goals

### §2.1 Goals

1. Detect same-block sandwich MEV attacks on UniV2 + UniV3 pools on Ethereum mainnet with
   confidence ∈ [0.0, 1.0] — not a boolean.
2. Operate on Ethereum mainnet only at MVP; `supported_chains()` returns `&[Chain::Ethereum]`.
3. Implement the A1 strict 3-swap pattern signal as the primary signal and integrate a
   profit-check gate (A2 component) as a confidence amplifier, rather than as an independent
   signal path. See §11 Decision 1 for rationale.
4. Emit evidence bundles sufficient for human review: attacker address, victim address, pool
   address, front-run tx hash, victim tx hash, back-run tx hash, victim slippage pct,
   attacker profit estimate, block number.
5. Be deterministic: same `swaps` table input for a given block range → bit-identical output.
6. Integrate into the existing streaming scheduler as a cadenced detector (same pattern as
   D11, D12) via `crates/server/src/init/detectors.rs`.
7. Override `supported_chains()` to return `&[Chain::Ethereum]` (gotcha #67 — default is
   `&[Chain::Solana]`; D12 established this override pattern).
8. Use `observed_at` from `ctx.window.end` (block-time sourced) — never `Utc::now()`
   (gotcha #22, #28).
9. All monetary arithmetic in `rust_decimal::Decimal` or `u128` raw units — never `f64`
   for prices, amounts, or profit values (CLAUDE.md policy).

### §2.2 Non-Goals

1. **Real-time mempool detection.** D13 detects sandwiches post-block from committed swap
   events. Pre-confirmation detection (alerting during the attack, before the back-run executes)
   requires mempool subscription infrastructure separate from the EthereumAdapter's block-level
   event stream. Explicitly deferred to Sprint 21+ (see §11 Decision 8).
2. **Builder-extracted MEV attribution.** Block builders who extract MEV from their own ordering
   produce the same observable pattern but are not "attackers" in the traditional sense. D13
   treats the builder-as-attacker case conservatively: if the front-run and back-run share the
   same EOA and the block proposer is the known builder address, confidence is reduced. Full
   attribution requires `mev_boost_relay` block metadata which is out of scope (ADR 0003).
3. **Curve / Balancer / SushiSwap sandwich detection.** The Sprint 16 EVM decoders cover
   UniV2 + UniV3 pools only. Curve's `Exchange` event and Balancer's `Swap` event require
   separate decoders (Sprint 21+). D13 MVP is UniV2 + UniV3 only (pool coverage B1 per §11
   Decision 2).
4. **Victim-specific alert.** D13 emits per-pool anomaly events, not per-victim alerts.
   Victim identification (from `tx_victim_hash` and its `msg.sender`) is included in
   evidence but consumer-layer alerting is out of scope.
5. **Cross-block sandwich.** The pattern where front-run and back-run span different blocks
   (possible with private order flow / Flashbots bundles across blocks) is not handled.
   All three transactions must appear in the same `block_number`.
6. **Solana sandwich detection.** Solana has no public mempool and validators do not expose
   transaction ordering within a slot the same way Ethereum does. D13 is Ethereum-only by
   design.
7. **Consumer integration.** Standalone service only per ADR 0003 + SESSION-KICKOFF §21.

---

## §3 Algorithm

### §3.1 Overview

D13 evaluates a block-scoped window of swap events. The detector is cadenced — it evaluates
accumulated swap data from the `swaps` table over a lookback window that spans recent
Ethereum blocks, grouped by `(block_number, pool)`.

Within each `(block_number, pool)` group:

1. Order swaps by `log_index ASC` (tx ordering within block)
2. Enumerate all triplets `(front, victim, back)` where `front.log_index < victim.log_index < back.log_index`
3. Apply the strict 3-swap pattern gate (A1): attacker address match between front + back
4. Apply the profit gate: attacker's net reserve-equivalent change > 0
5. Compute confidence and emit `AnomalyEvent`

### §3.2 Full Pipeline (pseudocode)

```
Input:
  - swaps table: rows for ctx.chain = 'ethereum' within lookback window
  - settlement_contract_allowlist: BTreeSet<String> from config
  - min_victim_slippage_pct: Decimal (config)
  - min_attacker_profit_usd: Decimal (config)
  - min_victim_swap_usd: Decimal (config)
  - ctx.window.end: observed_at timestamp (block-time sourced, NEVER Utc::now())

Step 1. Chain guard
  if ctx.chain != Chain::Ethereum { return Ok(vec![]); }

Step 2. Fetch recent swaps for ctx.token's pools within lookback window
  -- Note: ctx.token may appear as token0 or token1 in the pool; both directions relevant.
  SELECT s.tx_hash, s.pool, s.sender_address, s.to_address,
         s.amount0_raw, s.amount1_raw, s.direction,
         s.block_number, s.log_index, s.block_time
  FROM swaps s
  JOIN pools p ON s.chain = p.chain AND s.pool = p.pool_address
  WHERE s.chain = 'ethereum'
    AND (p.token0 = $token OR p.token1 = $token)
    AND s.block_time >= ctx.window.end - INTERVAL '<lookback_minutes> minutes'
    AND s.block_time <= ctx.window.end
  ORDER BY s.block_number ASC, s.pool ASC, s.log_index ASC
  -- Determinism: tri-key ORDER BY; same input → same output always

Step 3. Group by (block_number, pool)
  groups: BTreeMap<(i64, String), Vec<SwapRow>>
  -- BTreeMap for determinism (sorted key iteration)

Step 4. For each group, enumerate F-V-B triplets
  For each group (block_num, pool_addr) → swaps (ordered by log_index ASC):

    Step 4a. Collect the address appearing as attacker candidate:
      For each pair (front, back) where front.log_index < back.log_index:
        -- Attacker identity: the `sender_address` or `to_address` that is COMMON between front+back.
        -- On UniV2: `sender` is the router/contract, `to` is the recipient.
        -- On UniV3: `sender` is msg.sender; `recipient` is the output recipient.
        -- Heuristic: use `sender_address` for front, `to_address` for back (or vice versa).
        -- See §3.3 for address extraction details.
        attacker_candidate = resolve_attacker_address(front, back)
        if attacker_candidate is None: continue

    Step 4b. Find victim swap between front and back:
      For each swap mid where front.log_index < mid.log_index < back.log_index:
        if mid.sender_address == attacker_candidate
           OR mid.to_address == attacker_candidate:
          continue   -- attacker's own tx, not a victim
        -- mid is the victim candidate

        Step 4c. Apply minimum filters:
          if mid_swap_usd_estimate(mid) < min_victim_swap_usd: continue
          if front_back_direction_consistent(front, back) == false: continue
          -- front and back must be OPPOSITE direction relative to victim:
          --   If victim sells token T (buys WETH), attacker: front=buy T, back=sell T
          --   If victim buys token T (sells WETH), attacker: front=sell T, back=buy T

        Step 4d. Compute victim slippage estimate:
          -- Slippage = (expected_out - actual_out) / expected_out
          -- For MVP: approximate via amount delta vs pre-front-run reserve state.
          -- Exact slippage requires pool reserve state at front-run entry (before block N).
          -- MVP approximation: use (front_input / (reserve0 + front_input)) as price impact proxy.
          -- See §4.2 for formula.
          victim_slippage = estimate_victim_slippage(front, mid, pool)
          if victim_slippage < min_victim_slippage_pct: continue

        Step 4e. Compute attacker profit estimate:
          -- For A1 signal: profit = back_output - front_input (same token, net delta).
          -- For UniV2: use raw amounts from Swap event (amount0In, amount0Out, etc.)
          -- For UniV3: amount0 and amount1 are I256 (signed); net delta = back_amount_in - front_amount_in
          -- See §4.3 for formula.
          profit_raw = estimate_attacker_profit(front, back, pool)
          profit_usd = to_usd_estimate(profit_raw, token)
          if profit_usd < min_attacker_profit_usd: continue
          -- A negative or zero profit disqualifies the triplet: it's arbitrage routing, not sandwich

        Step 4f. Suppression check:
          if attacker_candidate ∈ settlement_contract_allowlist: continue
          -- CoW Protocol Settlement, Flashbots Protect, 1inch Fusion settlement contracts
          -- These produce F-V-B-like patterns as batch settlement mechanics, not attacks

        Step 4g. Confidence scoring (§4):
          conf = compute_confidence(victim_slippage, profit_usd, front, mid, back)

        Step 4h. Emit AnomalyEvent with evidence bundle (§6.2):
          events.push(AnomalyEvent {
            detector: "sandwich_mev_v1",
            confidence: conf,
            severity: severity_from_confidence(conf),
            observed_at: ctx.window.end,   // block_time, never Utc::now()
            evidence: build_evidence(front, mid, back, attacker_candidate,
                                     victim_slippage, profit_usd, profit_raw),
          })

Step 5. Select the single highest-confidence sandwich per pool per block
  (reduces noise when multiple victims are sandwiched in the same block on the same pool)
  deduplicated: BTreeMap<(block_num, pool_addr), AnomalyEvent>
  -- keep highest-confidence event per (block, pool) key; retain others as sub-evidence

Step 6. Return all deduplicated events
```

### §3.3 Attacker Address Extraction

On Uniswap v2, the `Swap` event schema is:
```
Swap(address indexed sender, uint256 amount0In, uint256 amount1In,
     uint256 amount0Out, uint256 amount1Out, address indexed to)
```
The `sender` is the address that called `swap()` on the pair (typically a router contract).
The `to` is the output recipient (typically the actual trader's address or next hop contract).

On Uniswap v3:
```
Swap(address indexed sender, address indexed recipient,
     int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick)
```
`sender` = caller of `swap()` on the pool; `recipient` = output recipient.

**Attacker identity resolution (MVP heuristic):**

For a front-run to be detectable, the attacker's wallet or contract must appear as either
`sender` or `to`/`recipient` in both the front-run and back-run swaps. The matching rule:

```
fn resolve_attacker_address(front: &SwapRow, back: &SwapRow) -> Option<String> {
    // Strategy 1: sender matches sender (attacker calls both directly)
    if front.sender_address == back.sender_address {
        return Some(front.sender_address.clone());
    }
    // Strategy 2: to/recipient matches to/recipient (same output recipient)
    if front.to_address == back.to_address {
        return Some(front.to_address.clone());
    }
    // Strategy 3: cross-match (front.to == back.sender — attacker uses same contract as input)
    if front.to_address == back.sender_address {
        return Some(front.to_address.clone());
    }
    None  // No consistent attacker identity found; skip triplet
}
```

This heuristic misses contract-mediated sandwiches where an intermediate proxy contract
appears in front + back but the controlling EOA is different. That evasion is documented
as E-D13-2 in §8.

### §3.4 Pool Direction Consistency Check

For a valid sandwich:
- If the front-run BUYS token T (token T is the output of the front swap), the back-run must
  SELL token T (token T is the input of the back swap). This is the "buy low front, sell high
  back" pattern.
- If the front-run SELLS token T, the back-run must BUY token T.

Direction is inferred from the signed amounts for UniV3 (`amount0 < 0` means token0 flows
OUT of the pool, i.e., the swapper received token0) and from `amount0In / amount0Out` for
UniV2.

```
fn front_back_direction_consistent(front: &SwapRow, back: &SwapRow) -> bool {
    // For UniV2: front is "buy T" if amount_T_out > 0 (T comes out of pool)
    //             back is "sell T" if amount_T_in > 0 (T goes into pool)
    // For UniV3: front is "buy T" if signed amount_T < 0 (T leaves pool to attacker)
    //             back is "sell T" if signed amount_T > 0 (T enters pool from attacker)
    // Implementation: check that front.direction == back.direction.opposite()
    // `direction` field populated by Sprint 16 decoder from signed/unsigned amounts
    front.direction.is_opposite(&back.direction)
}
```

---

## §4 Signal Math and Confidence Formula

### §4.1 Component Contributions

All monetary values use `rust_decimal::Decimal`. No `f64`. Component weights are
config-overridable (§9).

```
conf_raw = 0.0_Decimal

// Base: strict 3-swap pattern (A1) — attacker address match, direction consistency,
// victim between front and back in the same block.
// Source: Flashbots mev-inspect-py structural classification; Daian et al. 2019 §3.
if a1_pattern_fires:
    conf_raw += 0.55   // structural but not definitive: arb routing can look similar

// Profit confirmation: attacker's net reserve-equivalent change > min_attacker_profit_usd
// Confirms value extraction is occurring, not just price arbitrage.
// Source: Chi et al. 2024 §3.1 profit-based identification criterion.
if profit_usd > min_attacker_profit_usd:
    // Profit bonus scaled logarithmically to avoid threshold gaming
    // profit_bonus = 0.15 * min(1.0, log10(profit_usd / min_attacker_profit_usd + 1) / log10(11))
    // At profit = 10× min_profit: bonus ≈ 0.15. At profit = 100× min_profit: bonus ≈ 0.15 (saturates)
    // Rationale: profit confirmation is strong signal but not the primary classifier;
    // 0.15 max bonus keeps it as amplifier, not dominant component
    conf_raw += profit_bonus(profit_usd, min_attacker_profit_usd)  // ∈ [0, 0.15]

// Victim slippage magnitude: higher imposed slippage = higher confidence of adversarial intent
// Legitimate arb routing imposes slippage too, but typically < 0.3% on liquid pools.
// Source: Chi et al. 2024 distribution of sandwich-imposed slippage; median ≈ 0.8%.
if victim_slippage_pct >= min_victim_slippage_pct:
    // slippage_bonus = 0.10 * min(1.0, victim_slippage_pct / slippage_high_pct)
    // slippage_high_pct = 2.0% (config key: conf_slippage_high_pct)
    // At 2% slippage: bonus = 0.10. Below min_victim_slippage_pct: no contribution.
    conf_raw += slippage_bonus(victim_slippage_pct)  // ∈ [0, 0.10]

// Cap at 0.85 — sandwich is structurally strong but victim harm is indirect (slippage,
// not direct asset drain like D12). Residual 15% uncertainty accounts for:
//   (a) arb routing that mimics sandwich but is not adversarial
//   (b) contract-mediated sandwiches where attacker identity is uncertain
//   (c) legitimate batch settlement contracts on the allowlist edge cases
// Compare to D12 cap 0.95 (direct loss-of-funds warrants higher certainty ceiling).
conf = conf_raw.min(Decimal::from_str("0.85").unwrap())
```

**Derivation of 0.55 base for A1:**

The strict 3-swap structural pattern (same attacker in front+back, victim in between, same
block, same pool, opposite directions) is mechanistically sound. The 0.55 base (not higher)
reflects the primary FP scenario: cross-pool arbitrage executed via a single contract that
routes through the same pool in two directions within the same block, with an unrelated swap
in between. The arbitrageur appears as the "attacker" in the A1 structure but is not adversarial.
0.55 positions the structural signal as "more likely than not" (above 0.50) while leaving
substantial room for the profit + slippage amplifiers to push genuine cases to High/Critical.

**Derivation of 0.15 max profit bonus:**

Chi et al. 2024's profit-based identification is the strongest known empirical classifier.
However, profit is an outcome of the attack, not a separate structural signal — it is
implied by the F-V-B pattern if executed correctly. The 0.15 max bonus treats it as
confirmation evidence that lifts the structural base, not as an independent signal.

**Derivation of 0.10 max slippage bonus:**

Victim slippage above 0.5% on a liquid pool (min_victim_slippage_pct default) is unusual
for normal market conditions. Above 2%, it is highly anomalous for legitimate AMM use.
The 0.10 max bonus reflects that high slippage is a consequence of the attack visible from
the victim's perspective, not a separate causal signal.

**Cap at 0.85:**

Sandwich attacks impose indirect harm (slippage) rather than direct asset loss. The victim
still receives tokens — just fewer than expected. This makes the severity ladder top out at
`Severity::Critical` only in the rarest cases (very high profit + very high slippage + perfect
A1 match). The 0.85 cap ensures the default for a textbook sandwich is `Severity::High`
(conf ∈ [0.60, 0.80)), leaving `Critical` for compound signals.

### §4.2 Victim Slippage Estimation

Exact slippage requires the pool reserve state at the beginning of the block (before the
front-run executes). The `swaps` table does not store pre-swap reserve state.

**MVP approximation using Uniswap v2 formula:**

For a UniV2 pool with reserves `(R0, R1)` (fetched from the `pools` table as of the last
known state), victim swap input `v_in` of token0:

```
expected_out_v1 = (v_in * 997 * R1) / (R0 * 1000 + v_in * 997)
                  -- v1 formula without front-run effect

post_front_R0 = R0 + front_amount0_in - front_amount0_out
post_front_R1 = R1 + front_amount1_in - front_amount1_out
actual_out_v1 = (v_in * 997 * post_front_R1) / (post_front_R0 * 1000 + v_in * 997)
                 -- actual after front-run perturbs reserves

slippage = (expected_out_v1 - actual_out_v1) / expected_out_v1
```

**Limitations:**
- Pool reserve state from `pools` table may lag by up to the `lookback_minutes` window if
  no LP events occurred recently. This degrades slippage accuracy but does not affect
  the existence of the A1 structural signal.
- For UniV3, the reserve equivalent computation requires tick-level liquidity data not
  currently stored in `swaps`. In the MVP, victim slippage for UniV3 pools is approximated
  using a simpler price-impact proxy:
  `slippage ≈ |Δ(sqrtPriceX96)_front| / sqrtPriceX96_pre_front`
  where `sqrtPriceX96` values come from the front-run's `Swap` event fields.

Both approximations are classified as `unverified-heuristic` and must be validated against
a labelled corpus in Sprint 20. They are used only for the `min_victim_slippage_pct` gate
and the slippage bonus — not for the primary A1 structural classification.

### §4.3 Attacker Profit Estimation

For Uniswap v2:

```
// Front-run: attacker sells amount_in_front of token A, receives amount_out_front of token B
// Back-run:  attacker sells amount_in_back of token B, receives amount_out_back of token A
// Profit in token A: amount_out_back - amount_in_front
// (positive iff attacker received more A back than they spent)

profit_token_A_raw = back.amount0_out - front.amount0_in   // when attacking in token0/token1 direction
                                                            // signs depend on pool direction
```

For Uniswap v3 (I256 amounts — gotcha #62):

```
// amount0 < 0: pool sends token0 OUT (swapper receives token0)
// amount0 > 0: pool receives token0 IN (swapper sends token0)

// Front-run: attacker sends +front.amount0, receives -front.amount1 (buys token1)
// Back-run:  attacker sends +back.amount1, receives -back.amount0 (sells token1)
// Profit in token0: |back.amount0| - |front.amount0| (net token0 received vs spent)
profit_raw = (-back.amount0) - front.amount0   // I256 arithmetic; result signed
```

USD estimation follows the same static token decimal table as D12 `amount_usd_estimate`.
Profit in token terms is sufficient for the `min_attacker_profit_usd` gate; the USD value
appears in evidence as an estimate for human review.

### §4.4 Severity Ladder Mapping

```
conf ∈ [0.05, 0.30) → Severity::Low      (structural pattern only; slippage or profit below bonus gates)
conf ∈ [0.30, 0.60) → Severity::Medium   (structural + slippage OR structural + profit, but not both)
conf ∈ [0.60, 0.80) → Severity::High     (structural + profit + slippage; canonical textbook sandwich)
conf ∈ [0.80, 0.85] → Severity::Critical (saturated: all bonuses + high profit + high slippage;
                                           reserved for repeated multi-victim attacks or very high profit)
```

Maps directly to `severity_from_confidence` shared helper in `crates/detectors/src/signals.rs`.
No custom severity ladder needed.

---

## §5 Filters

### §5.1 Min Victim Swap Size (USD)

**Config key:** `sandwich_mev_v1.min_victim_swap_usd = "500"`

**Rationale:** Chi et al. 2024 §3.1 finds that minimum profitable sandwich on Uniswap v2
requires victim swap ≥ ~$500. Below this size, gas cost of two additional transactions (front +
back) typically exceeds extractable value, making the pattern economically irrational for
attackers. Setting the minimum victim swap at $500 eliminates unprofitable false-positive
candidates while preserving all real sandwich events. A lower threshold (e.g., $100) trades
recall gains for FP noise from arb routing misclassification.

**Chi 2024 derived threshold.** This is a primary citation — not a heuristic.

### §5.2 Min Victim Slippage (Pct)

**Config key:** `sandwich_mev_v1.min_victim_slippage_pct = "0.5"`

**Rationale:** The 0.5% threshold balances recall and FP rate. Normal AMM slippage on liquid
pools (USDC/WETH) is typically 0.05–0.20% for mid-size swaps. Chi et al. 2024 reports
sandwich-imposed slippage has a median of ~0.8% and a 5th percentile of ~0.3%. Setting
the gate at 0.5% captures >75% of confirmed sandwiches while excluding the long tail of
normal market slippage.

**Alternative: 1.0%** (reduces FP rate further at cost of ~20% recall loss). Configurable.

### §5.3 Min Attacker Profit (USD)

**Config key:** `sandwich_mev_v1.min_attacker_profit_usd = "10"`

**Rationale:** Setting a minimum profit threshold of $10 (Decimal) filters dust attacks and
testing transactions while preserving the vast majority of real attacks. Chi et al. 2024's
median sandwich profit is ~$32 — setting $10 captures all above-median attacks. The gate
here is deliberately loose because the profit estimation is approximate (§4.3) and we prefer
false positives over false negatives (CLAUDE.md policy).

**Note:** The profit gate is only used for the profit bonus contribution to confidence, not
as a hard suppression gate. A sandwich that passes the A1 structural test but has estimated
profit ≤ $10 still emits an event — at lower confidence (no profit bonus). This preserves
recall.

### §5.4 Settlement Contract Allowlist (Suppression)

**Config key:** `sandwich_mev_v1.settlement_contract_allowlist` (TOML array of addresses)

The following contracts produce F-V-B swap patterns as legitimate batch settlement mechanics
and must NOT fire the detector:

| Address | Protocol | Reason for Allowlist |
|---------|----------|---------------------|
| `0x9008D19f58AAbD9eD0D60971565AA8510560ab41` | CoW Protocol Settlement | Batch auction settlement; users' trades aggregated; no victim harm |
| `0xC92E8bdf79f0507f65a392b0ab4667716BFE0110` | Flashbots Protect Relay | MEV-protected transaction relay; ordering is protective, not adversarial |
| `0x111111125421cA6dc452d289314280a0f8842A65` | 1inch Fusion Settlement | Limit-order batch settlement; RFQ fills that look like F-V-B triplets |
| `0x1111111254EEB25477B68fb85Ed929f73A960582` | 1inch v5 Aggregation Router | Batch routing; mid-route swaps appear as victim between router's sub-swaps |
| `0x6131B5fae19EA4f9D964eAc0408E4408b66337b5` | KyberSwap Elastic Router | Multi-path routing with intermediate pool hops |

**Suppression logic:** Before Step 4g (confidence scoring), check if `attacker_candidate`
is in `settlement_contract_allowlist`. If yes, skip the triplet entirely — do NOT emit
even at low confidence. Hard suppression, not confidence reduction, because legitimate
batch settlement is structurally indistinguishable from sandwich at the event level.

**Note on established-protocol tokens:** Unlike D04 (Pump&Dump) and D06 (Mint-Burn),
D13 does NOT apply `is_established_protocol` total suppression on the token being traded.
Sandwiches occur primarily on WETH/USDC (the most liquid, most established pairs). Total
suppression on established tokens would eliminate the signal entirely. See §10 cross-detector
coverage matrix.

### §5.5 Lookback Window

**Config key:** `sandwich_mev_v1.lookback_minutes = 30`

**Rationale:** Sandwiches are fully resolved within a single block (~12 seconds). The 30-minute
lookback window is generous — it ensures the detector processes all blocks that arrived since
the last scheduler cadence, with buffer for scheduler delay. Unlike D12 (60-minute lookback
for single-event drains that can complete in separate steps), sandwich events are always
co-block, so a shorter window suffices. At 30 minutes, the query covers ~150 Ethereum blocks.

**Query performance:** The `swaps` table is indexed on `(chain, pool, block_time DESC)`
(from V00002 `idx_swaps_pool_time`). The 30-minute window + pool filter produces O(10–100)
rows per pool in typical use — well within the index range scan budget.

---

## §6 Integration

### §6.1 Detector Trait Implementation

```rust
pub struct SandwichMevDetector {
    pool: Arc<PgPool>,
    config: SandwichMevConfig,
    settlement_allowlist: BTreeSet<String>,   // normalized lowercase EVM checksum addresses
}

impl Detector for SandwichMevDetector {
    fn id(&self) -> &'static str { "sandwich_mev_v1" }

    fn severity_floor(&self) -> Severity { Severity::Low }

    fn supported_chains(&self) -> &[Chain] {
        // MUST override — default is &[Chain::Solana], which would never dispatch.
        // ADR 0005 Decision 2; gotcha #67. D12 established this pattern.
        &[Chain::Ethereum]
    }

    async fn evaluate<'ctx>(&'ctx self, ctx: &'ctx DetectorContext<'ctx>)
        -> Result<Vec<AnomalyEvent>, DetectorError>
    {
        // Chain guard — gotcha #67 pattern
        if ctx.chain != Chain::Ethereum {
            return Ok(vec![]);
        }
        // ... (§3.2 pipeline)
    }
}
```

### §6.2 Evidence Keys (gotcha #9 — prefixed by detector_id)

All `Evidence::metrics` keys use the `sandwich_mev_v1/` prefix:

| Key | Type (Decimal encoding) | Meaning |
|-----|------------------------|---------|
| `sandwich_mev_v1/attacker_address` | String | EVM checksum address of attacker (front+back EOA/contract) |
| `sandwich_mev_v1/victim_address` | String | `sender_address` or `to_address` from the victim's swap |
| `sandwich_mev_v1/pool_address` | String | EVM checksum address of the sandwich pool |
| `sandwich_mev_v1/front_run_tx_hash` | String | Transaction hash of the front-run swap |
| `sandwich_mev_v1/victim_tx_hash` | String | Transaction hash of the victim's swap |
| `sandwich_mev_v1/back_run_tx_hash` | String | Transaction hash of the back-run swap |
| `sandwich_mev_v1/block_number` | Decimal (int) | Block where the sandwich occurred |
| `sandwich_mev_v1/front_log_index` | Decimal (int) | log_index of front-run swap in block |
| `sandwich_mev_v1/victim_log_index` | Decimal (int) | log_index of victim swap in block |
| `sandwich_mev_v1/back_log_index` | Decimal (int) | log_index of back-run swap in block |
| `sandwich_mev_v1/victim_slippage_pct` | Decimal | Estimated slippage imposed on victim (e.g., "0.87") |
| `sandwich_mev_v1/attacker_profit_raw` | Decimal (u256-range stringified) | Raw token units profit |
| `sandwich_mev_v1/attacker_profit_usd_est` | Decimal | Estimated USD profit (coarse; NEVER float) |
| `sandwich_mev_v1/victim_swap_usd_est` | Decimal | Estimated USD size of victim's swap |
| `sandwich_mev_v1/pool_dex_kind` | String | "univ2" or "univ3" — pool type |
| `sandwich_mev_v1/signal_a1_match` | Decimal (0 or 1) | Strict 3-swap pattern match |
| `sandwich_mev_v1/profit_bonus` | Decimal | Confidence bonus from profit component |
| `sandwich_mev_v1/slippage_bonus` | Decimal | Confidence bonus from slippage component |

`observed_at` in `AnomalyEvent` is set to `ctx.window.end`, sourced from `block_time` —
never `Utc::now()` (gotcha #22, #28).

### §6.3 Storage — V00015 `mev_events` Table (Decision C3)

Per §11 Decision 3, the recommended option is **C3 Hybrid**: stateless detection (D13 reads
the existing `swaps` table directly, no new indexer write path required) PLUS a V00015
`mev_events` table that D13 writes its EMITTED events into for audit and retrospective analysis.

The `mev_events` table stores only AnomalyEvent-level records — not raw swap events (those
are already in `swaps`). This is analogous to `anomaly_events` but specialized for MEV
with attacker / victim / block-level structure.

```sql
-- V00015__mev_events.sql
CREATE TABLE mev_events (
    id               BIGSERIAL,
    chain            TEXT          NOT NULL,
    detector_id      TEXT          NOT NULL    DEFAULT 'sandwich_mev_v1',
    block_number     BIGINT        NOT NULL,
    pool_address     TEXT          NOT NULL,
    attacker_address TEXT          NOT NULL,
    victim_address   TEXT,
    front_tx_hash    TEXT          NOT NULL,
    victim_tx_hash   TEXT,
    back_tx_hash     TEXT          NOT NULL,
    profit_raw       NUMERIC(78,0),             -- signed; u256-range; allow NULL for missing estimate
    profit_usd_est   NUMERIC(18,6),             -- coarse USD estimate; use NUMERIC not float
    victim_slippage  NUMERIC(10,6),             -- fraction [0,1]; 0.0087 = 0.87%
    confidence       NUMERIC(5,4)  NOT NULL,    -- [0.0000, 1.0000]
    block_time       TIMESTAMPTZ   NOT NULL,
    PRIMARY KEY (id, block_time)                -- partition key in PK per gotcha #7
) PARTITION BY RANGE (block_time);

-- Index for audit queries: "all sandwiches by attacker address"
CREATE INDEX idx_mev_events_attacker
    ON mev_events (chain, attacker_address, block_time DESC);

-- Index for pool-level analysis
CREATE INDEX idx_mev_events_pool
    ON mev_events (chain, pool_address, block_time DESC);

-- Index for victim lookup
CREATE INDEX idx_mev_events_victim
    ON mev_events (chain, victim_address, block_time DESC)
    WHERE victim_address IS NOT NULL;

-- Dedup: one emission per (chain, front_tx, back_tx) — same sandwich not double-emitted
CREATE UNIQUE INDEX idx_mev_events_dedup
    ON mev_events (chain, front_tx_hash, back_tx_hash, block_time);  -- gotcha #7
```

Monthly partitions follow the V00002 pattern. The `anomaly_events` table (general) still
receives the `AnomalyEvent` via the standard sink — `mev_events` is an ADDITIONAL write
for MEV-specific structured audit trail.

### §6.4 Read Path (Detector Queries)

```sql
-- Query 1: Swaps for all pools where ctx.token participates, within lookback window
-- Join to pools table to find relevant pool addresses.
SELECT s.tx_hash,
       s.pool,
       s.sender_address,
       s.to_address,
       s.amount0_raw,
       s.amount1_raw,
       s.direction,
       s.block_number,
       s.log_index,
       s.block_time,
       p.dex            AS pool_dex_kind
FROM swaps s
JOIN pools p ON s.chain = p.chain AND s.pool = p.pool_address
WHERE s.chain = $1                           -- 'ethereum'
  AND (p.token0 = $2 OR p.token1 = $2)      -- ctx.token
  AND s.block_time >= $3                     -- window start
  AND s.block_time <= $4                     -- ctx.window.end
ORDER BY s.block_number ASC, s.pool ASC, s.log_index ASC;
-- ORDER BY is critical for determinism: same result regardless of Postgres internal ordering

-- Query 2: Pool reserve state (for slippage estimation)
SELECT pool_address, reserve0_raw, reserve1_raw, token0, token1
FROM pools
WHERE chain = $1
  AND pool_address = ANY($2::text[]);        -- batch lookup for all pools found in Query 1
```

### §6.5 Detector Registration

D13 registers in `crates/detectors/src/lib.rs` and in
`crates/server/src/init/detectors.rs::build_all_detectors` as the 13th streaming detector.
The `SchedulerWorker` chain-filter guard (Sprint 17) automatically skips D13 evaluation for
Solana tokens because `supported_chains() = &[Chain::Ethereum]`.

Per gotcha #77, the production wiring entry point is `crates/server/src/init/detectors.rs`.
The developer (S20-2) must add `SandwichMevDetector` to the detector vec alongside D12.

---

## §7 Threshold Calibration

### §7.1 Chi et al. 2024 — Primary Calibration Source

Chi, He, Hu & Wang 2024 (arXiv:2405.17944) provides the most granular empirical distribution
of sandwich MEV characteristics currently available in the academic literature. Key data points
extracted (from §3 and Appendix):

| Metric | Median | P5 | P95 | P99 |
|--------|--------|-----|-----|-----|
| Attacker profit (USD) | $32 | $0.80 | $1,800 | $8,500 |
| Victim swap size (USD) | $2,400 | $480 | $45,000 | $310,000 |
| Victim slippage imposed (pct) | 0.82% | 0.28% | 3.2% | 7.5% |
| Sandwich frequency (per day) | ~1,800 | — | — | — |

**Threshold derivations from Chi 2024:**

- `min_victim_swap_usd = 500`: covers P5 victim swap size ($480); below this threshold,
  gas cost of two additional txs makes the attack economically irrational per Chi §3.1.
- `min_attacker_profit_usd = 10`: substantially below P5 profit ($0.80 is noise level;
  $10 is a conservative guard against dust-level arb misclassification).
- `min_victim_slippage_pct = 0.5%`: above the "normal market slippage" range (0.05–0.20%)
  and captures ~80% of confirmed sandwich events (Chi P20 of slippage distribution ≈ 0.5%).

### §7.2 Flashbots mev-inspect-py — Reference Implementation

Flashbots' mev-inspect-py (github.com/flashbots/mev-inspect-py, archived) uses three
classification criteria for sandwiches:

1. Same `from` address (EOA) in front-run and back-run
2. Same pool/pair address across all three txs
3. Positive USD profit computed from pool reserve deltas

Their classification requires **net positive profit** — confirming that Chi 2024's
profit-based identification is operationally established. This validates our profit bonus
design (§4.1) as consistent with the reference implementation.

Key difference from D13: mev-inspect-py identifies sandwiches at the MEV-bot infrastructure
level (they see Flashbots bundle data, not just on-chain events). D13 reconstructs from
on-chain events only — a strictly harder problem but ADR-0003-compliant.

### §7.3 Positive Fixture Calibration Plan

At minimum three positive fixtures and two negative fixtures required per CLAUDE.md:

**POS_D13_01 — Canonical UniV2 sandwich (textbook A1):**
- Front-run swap: attacker buys WETH on USDC/WETH UniV2 pair, block 17,500,000
- Victim swap: user sells USDC for WETH, same block, same pool, log_index between front+back
- Back-run swap: attacker sells WETH on same pool, same block, log_index > victim
- Attacker address matches in front + back (same EOA)
- Expected: conf ≈ 0.70–0.75, Severity::High
- Source: Synthetic — representative of ~1,800 daily sandwich events documented by Chi 2024

**POS_D13_02 — UniV3 sandwich with high profit:**
- Front-run: attacker buys on USDC/WETH UniV3 500bps pool, block ~19,000,000
- Victim: mid-size swap; victim slippage ≈ 2.5%
- Back-run: attacker sells; profit ≈ $450 USD
- Expected: conf ≈ 0.80–0.85, Severity::Critical (all bonuses fire)
- Source: Synthetic — UniV3 pool; uses I256 amounts (gotcha #62)

**POS_D13_03 — Multi-victim block (two victims in same block, same pool):**
- Five swaps in block: front, victim1, victim2, victim2_supplement, back
- Attacker sandwiches two victims in one setup
- Expected: single emitted AnomalyEvent (highest-confidence per (block, pool) dedup step);
  both victim tx_hashes in evidence sub-structure
- Source: Synthetic — demonstrates Step 5 dedup logic

### §7.4 Negative Fixture Calibration Plan

**NEG_D13_01 — Legitimate CoW Protocol batch settlement:**
- Three swaps in same block, same pool; `sender_address` = CoW Protocol Settlement contract
- Settlement contract is in `settlement_contract_allowlist`
- Expected: SUPPRESSED — no event emitted
- Source: Synthetic — CoW Protocol batch settlement address `0x9008D19f58AAbD9eD0D60971565AA8510560ab41`

**NEG_D13_02 — Cross-pool arbitrage (arb routing, not sandwich):**
- Two swaps by same EOA address on the same pool within the same block; no intermediate
  victim swap between them (log indices are consecutive or there is no third-party swap in between)
- Expected: no event emitted — A1 requires a VICTIM swap between front and back, from a
  different address
- Source: Synthetic — normal arbitrage routing

---

## §8 Evasion Analysis

### E-D13-1: Attacker Rotates Address Between Attacks

**Description:** Attacker deploys a fresh EOA or proxy contract for each sandwich to avoid
address-based detection. The A1 structural signal requires front.attacker == back.attacker
within a single (block, pool) triplet. Fresh addresses are not the problem here — the signal
is per-block, not cross-block. An attacker using a fresh wallet for each block still satisfies
A1 within the block.

**Impact:** None on the per-block A1 structural signal. The address still appears in front + back
in the same block. Cross-block attacker clustering (identifying repeat attackers across many
blocks) is a Phase 5 scoring enhancement — deferred.

**Residual gap:** Cross-block tracking of sandwich bots (confirming "this attacker sandwiches
repeatedly across N blocks") is not part of D13 MVP. The evidence bundle includes
`attacker_address` so the scoring layer can aggregate across events later.

**Mitigation (Sprint 21+):** Add `attacker_recurrence_count` to evidence: query `mev_events`
for prior D13 events with same `attacker_address` in a trailing 24h window. High recurrence
count amplifies confidence.

### E-D13-2: Contract-Mediated Sandwich (Proxy Contract)

**Description:** Attacker uses an intermediate proxy contract in the call stack. The proxy
appears as `sender` in both the front-run and back-run, but the controlling EOA is different.
The A1 address extraction (§3.3) uses `sender` and `to`/`recipient` from the Swap event.
If the proxy address is consistent across front + back, A1 still fires at the contract level.

**Impact:** The `attacker_address` in evidence will be the proxy contract address, not the
human-readable EOA. For a rotating proxy contract (new contract per sandwich), A1 still fires
within the block — the proxy address is the same address in both txs.

**True evasion:** If the attacker deploys a DIFFERENT proxy contract for the front-run vs
the back-run (strategy 3 in `resolve_attacker_address` fails), D13 will not find a consistent
attacker address and will not emit an event. This requires two proxy deployments per attack —
expensive in gas and unusual in practice.

**Mitigation:** Extend `resolve_attacker_address` to check whether `tx.origin` (the root EOA)
is the same across front + back. `tx.origin` is not directly observable in ERC-20 or Swap
events; it requires `debug_traceTransaction` or `eth_getTransactionByHash` — deferred.

**Residual gap:** Well-funded MEV bots that deploy fresh contract pairs per attack are
invisible to D13 MVP. This is the same residual gap Flashbots mev-inspect-py accepts.
Chi et al. 2024 estimate < 5% of sandwich profit comes from this evasion class.

### E-D13-3: Cross-Pool Arbitrage Misclassification (Primary FP Source)

**Description:** A legitimate arbitrageur executes two opposing swaps on the SAME pool
within the same block to capture a transient price inefficiency — and an unrelated user
happens to swap between them. The arbitrageur appears as the "attacker" in the A1 structure.

**Impact:** This is the primary FP source for A1. The direction check (§3.4) partially
mitigates it: a pure arb round-trip on the same pool (buy then sell the same asset) does
have opposite directions as required by A1. The mitigation requires both profit AND slippage
checks to be satisfied — pure arb routing typically has lower victim slippage (<0.5%) than
true sandwiches because the arb trades are sized to pool capacity, not to victim order size.

**Mitigation:** The `min_victim_slippage_pct` gate (default 0.5%) is calibrated specifically
to distinguish arb from sandwich. The profit gate further filters: arb profits are small on
liquid pools (< $10 per round-trip on a deep USDC/WETH pool). Combining slippage + profit +
direction gates should eliminate > 90% of arb misclassifications at the default thresholds.

**Residual gap at low liquidity:** On thin pools (< $100K TVL), arb round-trips can impose
substantial victim slippage AND yield material profit. D13 will misclassify some legitimate
arb as sandwich on thin pools. This is acceptable: thin-pool arb is often predatory in effect
even if not technically a sandwich. Consider this an acceptable FP for operational purposes.

### E-D13-4: Builder-Extracted MEV (Invisible to D13)

**Description:** Block builders who run Flashbots MEV-Boost can self-construct sandwich
bundles at the block level. When a builder extracts sandwich profit, the three transactions
appear in the block with the builder's own address as attacker — but the MEV payment goes
to the builder as a block reward, not as a token transfer. The observable on-chain pattern
is identical to an external sandwich attack.

**Impact on D13:** D13 fires on builder-extracted MEV with the same confidence as on external
sandwich attacks. There is no way to distinguish builder-extracted from searcher-extracted
from on-chain events alone — the distinction requires MEV-Boost relay metadata (prohibited
by ADR 0003).

**Design decision:** D13 deliberately does NOT distinguish builder-extracted MEV. Both forms
impose victim slippage and extract value — both are worth flagging from the victim's perspective.
The evidence includes `attacker_address`, which consumers can check against a known-builder
allowlist maintained outside D13.

**Note in evidence:** The `sandwich_mev_v1/pool_dex_kind` evidence key helps consumers
distinguish patterns: builder sandwiches are more common on UniV3 high-fee tiers; searcher
sandwiches concentrate on UniV2 and UniV3 low-fee pairs.

### E-D13-5: Slow Sandwich (Cross-Block)

**Description:** Attacker front-runs in block N, victim's swap is delayed to block N+1 (e.g.,
due to gas limit or nonce ordering), attacker back-runs in block N+1 or N+2. All three events
are in the `swaps` table but they span different `block_number` values.

**Impact:** D13 groups by `(block_number, pool)` — cross-block triplets are not evaluated.
The vast majority of real sandwiches are same-block (the entire value of MEV ordering is
that the block proposer controls intra-block ordering; cross-block execution exposes the
attacker to price risk).

**Design decision:** Cross-block sandwich detection is explicitly out of scope for D13 MVP
(§2.2 Non-Goal 5). The residual gap is small in practice and would require a stateful
sliding-window query rather than the simpler block-grouped approach. Deferred to Sprint 21+.

---

## §9 Configuration Keys

All keys live under `[sandwich_mev_v1]` in `config/detectors.toml`. Every key requires
a REFERENCES.md entry or an internal derivation comment.

```toml
[sandwich_mev_v1]

# Minimum USD-equivalent value for the victim's swap to be considered.
# Below this threshold, gas cost of the attack exceeds extractable value per Chi 2024 §3.1.
# Calibration: Chi 2024 P5 victim swap = $480; $500 is a round threshold slightly above P5.
# REFERENCES.md: Chi/He/Hu/Wang 2024, arXiv:2405.17944
# Type: string Decimal — NEVER float
min_victim_swap_usd = "500"

# Minimum attacker profit estimate in USD for the profit bonus to apply.
# Does NOT gate event emission — only gates the profit_bonus confidence component.
# Calibration: Chi 2024 P5 profit = $0.80; $10 chosen as noise-floor guard above dust arb.
# REFERENCES.md: Chi/He/Hu/Wang 2024, arXiv:2405.17944 (Appendix profit distribution)
min_attacker_profit_usd = "10"

# Minimum estimated slippage imposed on victim (as a fraction, e.g., 0.005 = 0.5%).
# Below this: normal AMM price impact, not sandwich-specific slippage.
# Calibration: Chi 2024 P20 slippage ≈ 0.5%; normal liquid-pool slippage < 0.2%.
# REFERENCES.md: Chi/He/Hu/Wang 2024, arXiv:2405.17944
# Type: string Decimal
min_victim_slippage_pct = "0.005"

# Lookback window for swap events. One block ≈ 12s; 30 min ≈ 150 blocks.
# Sandwiches complete within a single block; this is a generous scheduler buffer.
# See §5.5 rationale.
lookback_minutes = 30

# Pool DEX kinds to evaluate. MVP: univ2 + univ3 only.
# Curve/Balancer deferred to Sprint 21+ (require new decoders).
# Type: array of strings matching swaps.pool_kind column values.
enabled_pool_kinds = ["univ2", "univ3"]

# Settlement contracts whose sender_address is treated as legitimate batch settlement.
# Exact 20-byte EVM checksum match; no partial matching.
# ADR 0003: no runtime API call to populate; list maintained manually.
# See §5.4 rationale.
settlement_contract_allowlist = [
    "0x9008D19f58AAbD9eD0D60971565AA8510560ab41",   # CoW Protocol Settlement
    "0xC92E8bdf79f0507f65a392b0ab4667716BFE0110",   # Flashbots Protect
    "0x111111125421cA6dc452d289314280a0f8842A65",   # 1inch Fusion Settlement
    "0x1111111254EEB25477B68fb85Ed929f73A960582",   # 1inch v5 AggregationRouter
    "0x6131B5fae19EA4f9D964eAc0408E4408b66337b5",   # KyberSwap Elastic Router
]

# Confidence formula weights — all string Decimal to avoid float
# Base confidence for A1 structural pattern
conf_base_a1 = "0.55"
# Maximum bonus from profit component (logarithmic ramp; see §4.1)
conf_max_profit_bonus = "0.15"
# Maximum bonus from victim slippage component (linear ramp; see §4.1)
conf_max_slippage_bonus = "0.10"
# Slippage level at which slippage_bonus saturates (as fraction, 0.02 = 2%)
conf_slippage_high_pct = "0.02"
# Confidence cap — sandwich is strong signal but victim harm is indirect (not direct drain)
conf_cap = "0.85"

# Minimum confidence to emit an AnomalyEvent.
# Low floor per CLAUDE.md: "false positives are cheap, false negatives are expensive."
min_emit_confidence = "0.05"

# USD fallback for unknown tokens (not in static decimal/price table).
# "0" means: if profit cannot be estimated in USD, skip profit bonus; emit based on A1+slippage only.
unknown_token_usd_fallback = "0"
```

---

## §10 Cross-Detector Coverage Matrix

### §10.1 D13 vs D05 (Wash Trading)

Both D05 Signal B and D13 pattern-match on multi-tx sequences on the same pool. The
key distinctions:

| Dimension | D05 Wash Trading (Signal B) | D13 Sandwich / MEV |
|-----------|-----------------------------|--------------------|
| Pattern analyzed | Circular transfer graph (A→B→C→A over minutes/hours) | Strict 3-tx ordering in a single block |
| Victim concept | No victim — wash trading is self-dealing between colluding wallets | Explicit victim: a third party whose swap is between the attacker's two txs |
| Attacker intent | Volume inflation / market manipulation | Value extraction from victim's worse execution price |
| Window | 120 minutes (configurable); multi-block | Single block (block_number grouping key) |
| Detection algorithm | Tarjan SCC + Johnson cycle enumeration on transfer graph | Block-scoped triplet enumeration on swaps |
| Signal independence | Fully independent — different table (transfers vs swaps), different window, different pattern | Fully independent from D05 |
| Can co-fire? | Yes — wash trading and sandwich can occur simultaneously on the same pool | Yes — attacker may wash-trade to inflate volume alongside sandwiching |

Both signals can co-fire on the same pool and time window. The scoring layer should treat them
as additive evidence of adversarial intent, not as contradictory signals.

### §10.2 D13 vs D04 (Pump & Dump)

D04 fires on price spikes with insider distribution. D13 fires on MEV extraction within a block.
They address different time scales (D04: hours-to-days of accumulation; D13: single block).

A coordinated attack could combine both: insider pump using Permit2 drain proceeds (D12)
→ sandwich the pump (D13) → insider dump (D04). All three can fire on the same token in
the same time window. The scoring layer should flag this combination as a composite
risk indicator.

### §10.3 D13 vs D12 (Permit2 Drainer)

See §1.4 comparison table. These signals are structurally orthogonal and should not substitute
for each other in scoring. D12 = direct theft; D13 = indirect extraction via ordering.

### §10.4 D13 Suppression Policy Summary

| Category | Suppression | Rationale |
|----------|-------------|-----------|
| `is_established_protocol` on token | NOT suppressed | WETH/USDC are the primary sandwich targets |
| Settlement contract allowlist (CoW/Flashbots/1inch) | Hard suppressed (no event) | Legitimate batch settlement is structurally indistinguishable |
| Builder-extracted MEV | NOT suppressed | Same victim harm; no reliable on-chain distinction |
| Low-profit (<$10) triplets | Emitted at lower confidence (no profit bonus) | Preserve recall; filter by conf threshold at consumer |

---

## §11 Decisions Requiring Sign-Off

### Decision 1: Signal Source — A1 (Strict 3-Swap) / A2 (Profit-Only) / A3 (Ensemble)

**Recommended: A1 as primary structural signal with A2 (profit) as confidence amplifier.**

The design in §4.1 implements this as a single signal path with additive bonuses rather than
two parallel signal paths (A3 proper ensemble). Rationale for the hybrid:

- **A1 pure:** The strict 3-swap structural pattern (same attacker, opposite directions, victim
  between, same block, same pool) is mechanistically sound and directly derived from Flashbots
  mev-inspect-py's classification rules. This is the most defensible primary signal.
- **A2 profit-only pure:** Profit sign alone is a weaker signal because any arbitrage routing
  generates positive profit for the executor. Profit becomes informative only when conditioned
  on the structural pattern — which is exactly what the bonus formula does.
- **True A3 (two parallel paths):** Would require designing a second confidence formula for
  the profit-only path, maintaining two threshold sets, and explaining the ensemble math.
  The added complexity is not justified when profit is most useful as A1 amplification.

**Trade-off:** The hybrid may underweight certain cases where profit is very high but the
attacker address match is uncertain (E-D13-2 proxy contracts). In those cases, a true A2
path could fire at medium confidence. This gap is accepted as an acceptable miss in MVP.

**Alternative if sign-off is for A3:** Split confidence formula into two independent paths:
`conf_a1 = base 0.55 + bonuses`; `conf_a2 = 0.40 (profit only, any attacker) + profit_bonus`.
Emit both and take max. Requires two queries and two evidence bundles. Adds ~150 LOC.

### Decision 2: Pool Coverage — B1 (UniV2 + UniV3 MVP) / B2 (Add Curve/Balancer/Sushi)

**Recommended: B1 — UniV2 + UniV3 only in Sprint 20.**

The Sprint 16 decoders already produce `swaps` rows for both UniV2 (`univ2::Swap` event) and
UniV3 (`univ3::Swap` event with I256 amounts, gotcha #62). D13 reads from the `swaps` table
with no new decoder work required. Adding Curve's `Exchange` event or Balancer's `Swap` event
requires new `sol!` blocks in `decoder.rs` and changes to the EthereumAdapter dispatch path.

**Trade-off:** Curve pools (particularly Curve stableswap) are targets for sandwich attacks on
stablecoin swaps. Balancer weighted pools are less frequent targets. Estimated 20–30% of
sandwich events on Ethereum occur on Curve/Balancer/Sushi pools (rough estimate; Chi 2024 does
not break down by DEX). B1 coverage captures ~70% of Ethereum sandwich events by count.

B2 (Curve/Balancer/Sushi) is sprint 21+ — scope fits in a single session once the framework
is established.

### Decision 3: Storage Tier — C1 (Stateless) / C2 (V00015 `mev_events`) / C3 (Hybrid)

**Recommended: C3 Hybrid** — stateless detection from `swaps` + V00015 `mev_events` for
emitted events only.

**C1 (Stateless recompute only):**
- Pro: no migration, no additional write path
- Con: no audit trail; cannot query "which pools have been sandwiched historically"; consumers
  cannot retrospectively search for victim addresses they care about

**C2 (V00015 only, no stateless logic):**
- Not applicable: D13 must read from `swaps` regardless; C2 is about what it WRITES.

**C3 (Hybrid):**
- Detection logic reads from `swaps` (stateless, no new indexer work)
- Each emitted AnomalyEvent also writes one row to `mev_events` (V00015)
- `mev_events` provides: victim lookup by address, attacker recurrence tracking, pool-level
  MEV frequency analysis, forensic audit trail
- Write path is best-effort (same semantics as `token_risk_reports`, Sprint 12 D-C)
- One new migration (V00015), no indexer changes, no new decoder work

**Trade-off of C3 vs C1:** V00015 adds ~15 LOC for the migration and ~30 LOC for the write
path. The audit trail capability is high value for forensics and is consistent with how D12
(V00014 `permit2_events`) provides retrospective query capability.

### Decision 4: Min Victim Slippage Threshold

**Recommended: `min_victim_slippage_pct = "0.005"` (0.5%).**

Derived from Chi et al. 2024 §3 (P20 of sandwich-imposed slippage distribution ≈ 0.5%).
Below 0.5%, the imposed slippage is indistinguishable from normal market slippage on liquid
pools. At 1.0%, recall loss is ~20% (Chi P40 ≈ 1.0%). The 0.5% default balances recall and
FP rate. Configurable via `min_victim_slippage_pct` key.

**Alternative (1.0%):** Higher precision, lower recall. Recommended only if FP rate at 0.5%
proves excessive during Sprint 20 calibration.

### Decision 5: Min Attacker Profit Threshold

**Recommended: `min_attacker_profit_usd = "10"` (Decimal).**

This is a SOFT gate (gating only the profit_bonus, not event emission). An attack with
estimated profit ≤ $10 still fires D13 via the A1 structural path at lower confidence.
The $10 floor eliminates noise from dust arb misclassification while preserving recall for
all genuine sandwich events (Chi 2024 P5 profit = $0.80; the gap between $0.80 and $10
is mostly testing transactions and measurement error in the MVP approximation).

### Decision 6: Confidence Formula and Cap

**Recommended: formula as specified in §4.1, cap at 0.85.**

Key rationale for 0.85 cap vs D12's 0.95 cap: D12 detects direct asset removal (D12
`Severity::Critical` is appropriate when tokens are definitively taken from a victim). D13
detects indirect slippage harm — the victim still received tokens, just fewer than expected.
The severity ceiling at 0.85 ensures the standard operational baseline for a confirmed
sandwich is `Severity::High`, not `Severity::Critical`, which is reserved for extreme cases.

**Alternative (cap at 0.75):** More conservative — reduces alert fatigue if sandwich volume
is high on monitored pools. The trade-off is that textbook high-profit sandwiches would land
in `Severity::Medium` rather than `Severity::High`. Reject: the victim harm is real and warrants
`High` for canonical cases.

### Decision 7: Suppression Policy for Settlement Contracts

**Recommended: Hard suppression (no event) when `attacker_candidate ∈ settlement_contract_allowlist`.**

CoW Protocol, Flashbots Protect, and 1inch Fusion produce structurally identical F-V-B patterns
as batch settlement mechanics. The key distinction is that these protocols guarantee the victim
(solver counterparty) better-than-market execution — they protect against MEV, not extract from
victims. Emitting D13 events on their settlements would produce high-confidence false positives
that undermine trust in the detector.

Hard suppression (not confidence reduction) is correct because the FP cost is unacceptably high
for operational use. A consumer who wants to see ALL F-V-B patterns including batch settlements
can disable the `settlement_contract_allowlist` via config (set to empty array).

**Note on maintainability:** The settlement allowlist requires manual updates as new
MEV-protection protocols deploy. This is the same maintenance commitment as the D12
known-legitimate-spenders list. ADR 0003 prohibits runtime API refresh; updates ship
with the next sprint's deployment.

### Decision 8: Mempool Integration Scope

**Recommended: Block-level only for MVP. Mempool detection deferred to Sprint 21+.**

Block-level post-hoc detection reconstructs the full sandwich from committed state — all three
transactions are visible in `swaps` within one block. This is sufficient for:
- Auditing which pools are being sandwiched
- Identifying attacker infrastructure
- Feeding scoring for tokens traded on attacked pools
- Victim address logging for consumer alerting

**What block-level detection cannot do:**
- Alert victims BEFORE the back-run executes (during the 12-second block window)
- Pre-emptively reject or protect victim transactions

Real-time victim protection requires mempool subscription (`eth_subscribe("newPendingTransactions")`)
and sub-second response time — a completely separate subsystem from the cadenced detector model.
The EthereumAdapter would need a mempool event source, a new streaming path separate from
block events, and a very different detector architecture.

**Trade-off:** Block-level detection arrives ~12 seconds after victim harm occurs. This latency
is acceptable for the current consumers (analytics, audit, risk scoring). The trading bot
(`bot-trader-2-0`) would benefit from pre-block alerting but per SESSION-KICKOFF §21, consumer
integration decisions are the consumer's responsibility.

---

## §12 Fixture Shapes

### §12.1 EVM Fixture Directory

EVM fixtures live at `tests/fixtures/ethereum/` (established in Sprint 18 for D12). Format
follows the established JSON envelope with `_label`, `_description`, `_chain`, `_expected`.

### §12.2 Positive Fixture — POS_D13_01 (Canonical UniV2 Sandwich)

File: `tests/fixtures/ethereum/d13_positive_01_univ2_sandwich.json`

```json
{
  "_label": "POS_D13_01",
  "_description": "Canonical UniV2 sandwich. Attacker front-runs and back-runs a victim USDC→WETH swap on a UniV2 pair. A1 structural pattern fires. Profit ~$45 USD. Source: synthetic; representative of ~1,800/day events documented by Chi et al. 2024.",
  "_chain": "ethereum",
  "_token": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
  "_source": "Synthetic — mimics typical UniV2 sandwich. Pool = hypothetical USDC/WETH pair.",
  "pools": [
    {
      "chain": "ethereum",
      "pool_address": "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
      "dex": "univ2",
      "token0": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
      "token1": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
      "reserve0_raw": "50000000000000",
      "reserve1_raw": "16666666666666666666666"
    }
  ],
  "swaps": [
    {
      "_role": "front_run",
      "chain": "ethereum",
      "pool": "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
      "tx_hash": "0xd13pos01front00000000000000000000000000000000000000000000000000",
      "sender_address": "0xAttacker0000000000000000000000000000001",
      "to_address": "0xAttacker0000000000000000000000000000001",
      "amount0_raw": "5000000000",
      "amount1_raw": "-1659444703925000",
      "direction": "buy_token1",
      "log_index": 10,
      "block_number": 17500000,
      "block_time": "2023-08-15T12:00:00Z",
      "pool_kind": "univ2"
    },
    {
      "_role": "victim",
      "chain": "ethereum",
      "pool": "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
      "tx_hash": "0xd13pos01victim0000000000000000000000000000000000000000000000000",
      "sender_address": "0xVictim000000000000000000000000000000001",
      "to_address": "0xVictim000000000000000000000000000000001",
      "amount0_raw": "1000000000",
      "amount1_raw": "-330000000000000",
      "direction": "buy_token1",
      "log_index": 11,
      "block_number": 17500000,
      "block_time": "2023-08-15T12:00:00Z",
      "pool_kind": "univ2"
    },
    {
      "_role": "back_run",
      "chain": "ethereum",
      "pool": "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
      "tx_hash": "0xd13pos01back000000000000000000000000000000000000000000000000000",
      "sender_address": "0xAttacker0000000000000000000000000000001",
      "to_address": "0xAttacker0000000000000000000000000000001",
      "amount0_raw": "-5015000000",
      "amount1_raw": "1659444703925000",
      "direction": "sell_token1",
      "log_index": 12,
      "block_number": 17500000,
      "block_time": "2023-08-15T12:00:00Z",
      "pool_kind": "univ2"
    }
  ],
  "settlement_contract_allowlist": [],
  "_expected": {
    "detector_id": "sandwich_mev_v1",
    "fires": true,
    "min_confidence": 0.60,
    "max_confidence": 0.80,
    "signal_a1_match": true,
    "attacker_address": "0xAttacker0000000000000000000000000000001",
    "victim_address": "0xVictim000000000000000000000000000000001",
    "pool_address": "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
    "front_run_tx_hash": "0xd13pos01front00000000000000000000000000000000000000000000000000",
    "victim_tx_hash": "0xd13pos01victim0000000000000000000000000000000000000000000000000",
    "back_run_tx_hash": "0xd13pos01back000000000000000000000000000000000000000000000000000",
    "block_number": 17500000
  }
}
```

### §12.3 Positive Fixture — POS_D13_02 (UniV3 High-Profit Sandwich)

File: `tests/fixtures/ethereum/d13_positive_02_univ3_high_profit.json`

```json
{
  "_label": "POS_D13_02",
  "_description": "UniV3 sandwich with high profit (~$450 USD est.) and high victim slippage (~2.5%). All confidence bonuses fire. Expected: Severity::Critical. Source: synthetic; UniV3 pool uses signed I256 amounts per gotcha #62.",
  "_chain": "ethereum",
  "_token": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
  "_source": "Synthetic — mimics Flashbots bundle sandwich on UniV3 500bps USDC/WETH pool.",
  "pools": [
    {
      "chain": "ethereum",
      "pool_address": "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640",
      "dex": "univ3",
      "token0": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
      "token1": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
      "reserve0_raw": "200000000000000",
      "reserve1_raw": "65000000000000000000000"
    }
  ],
  "swaps": [
    {
      "_role": "front_run",
      "chain": "ethereum",
      "pool": "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640",
      "tx_hash": "0xd13pos02front00000000000000000000000000000000000000000000000000",
      "sender_address": "0xAttacker0000000000000000000000000000002",
      "to_address": "0xAttacker0000000000000000000000000000002",
      "amount0_raw": "50000000000",
      "amount1_raw": "-16600000000000000000",
      "direction": "buy_token1",
      "log_index": 5,
      "block_number": 19000000,
      "block_time": "2024-01-20T08:00:00Z",
      "pool_kind": "univ3"
    },
    {
      "_role": "victim",
      "chain": "ethereum",
      "pool": "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640",
      "tx_hash": "0xd13pos02victim0000000000000000000000000000000000000000000000000",
      "sender_address": "0xVictim000000000000000000000000000000002",
      "to_address": "0xVictim000000000000000000000000000000002",
      "amount0_raw": "75000000000",
      "amount1_raw": "-24200000000000000000",
      "direction": "buy_token1",
      "log_index": 6,
      "block_number": 19000000,
      "block_time": "2024-01-20T08:00:00Z",
      "pool_kind": "univ3"
    },
    {
      "_role": "back_run",
      "chain": "ethereum",
      "pool": "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640",
      "tx_hash": "0xd13pos02back000000000000000000000000000000000000000000000000000",
      "sender_address": "0xAttacker0000000000000000000000000000002",
      "to_address": "0xAttacker0000000000000000000000000000002",
      "amount0_raw": "-51350000000",
      "amount1_raw": "16600000000000000000",
      "direction": "sell_token1",
      "log_index": 7,
      "block_number": 19000000,
      "block_time": "2024-01-20T08:00:00Z",
      "pool_kind": "univ3"
    }
  ],
  "settlement_contract_allowlist": [],
  "_expected": {
    "detector_id": "sandwich_mev_v1",
    "fires": true,
    "min_confidence": 0.75,
    "max_confidence": 0.85,
    "signal_a1_match": true,
    "attacker_address": "0xAttacker0000000000000000000000000000002",
    "victim_address": "0xVictim000000000000000000000000000000002",
    "block_number": 19000000,
    "pool_address": "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"
  }
}
```

**Note on I256 amounts in UniV3 fixtures:** `amount0` and `amount1` in UniV3 Swap events are
signed `int256`. The fixture uses string-encoded values matching the `I256` representation
used by the alloy decoder (gotcha #62). A negative `amount0_raw` means token0 flows OUT of
the pool to the swapper (swapper receives token0). The detector must parse these as I256, not
U256, to correctly determine swap direction and profit sign.

### §12.4 Negative Fixture — NEG_D13_01 (CoW Protocol Batch Settlement)

File: `tests/fixtures/ethereum/d13_negative_01_cow_settlement.json`

```json
{
  "_label": "NEG_D13_01",
  "_description": "CoW Protocol Settlement batch. Three swaps in same block, same pool, with CoW Settlement contract as sender. Pattern looks like F-V-B but is legitimate batch settlement. Must NOT fire — settlement contract is in allowlist.",
  "_chain": "ethereum",
  "_token": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
  "_source": "Synthetic — mimics CoW Protocol Settlement batch settlement on UniV2 pool.",
  "pools": [
    {
      "chain": "ethereum",
      "pool_address": "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
      "dex": "univ2",
      "token0": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
      "token1": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
      "reserve0_raw": "50000000000000",
      "reserve1_raw": "16666666666666666666666"
    }
  ],
  "swaps": [
    {
      "_role": "settlement_leg_1",
      "chain": "ethereum",
      "pool": "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
      "tx_hash": "0xd13neg01cow0000000000000000000000000000000000000000000000000001",
      "sender_address": "0x9008D19f58AAbD9eD0D60971565AA8510560ab41",
      "to_address": "0x9008D19f58AAbD9eD0D60971565AA8510560ab41",
      "amount0_raw": "3000000000",
      "amount1_raw": "-995000000000000",
      "direction": "buy_token1",
      "log_index": 10,
      "block_number": 19500000,
      "block_time": "2024-03-10T09:00:00Z",
      "pool_kind": "univ2"
    },
    {
      "_role": "user_fill",
      "chain": "ethereum",
      "pool": "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
      "tx_hash": "0xd13neg01cow0000000000000000000000000000000000000000000000000001",
      "sender_address": "0xUser000000000000000000000000000000000001",
      "to_address": "0xUser000000000000000000000000000000000001",
      "amount0_raw": "500000000",
      "amount1_raw": "-165000000000000",
      "direction": "buy_token1",
      "log_index": 11,
      "block_number": 19500000,
      "block_time": "2024-03-10T09:00:00Z",
      "pool_kind": "univ2"
    },
    {
      "_role": "settlement_leg_2",
      "chain": "ethereum",
      "pool": "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
      "tx_hash": "0xd13neg01cow0000000000000000000000000000000000000000000000000001",
      "sender_address": "0x9008D19f58AAbD9eD0D60971565AA8510560ab41",
      "to_address": "0x9008D19f58AAbD9eD0D60971565AA8510560ab41",
      "amount0_raw": "-3030000000",
      "amount1_raw": "995000000000000",
      "direction": "sell_token1",
      "log_index": 12,
      "block_number": 19500000,
      "block_time": "2024-03-10T09:00:00Z",
      "pool_kind": "univ2"
    }
  ],
  "settlement_contract_allowlist": [
    "0x9008D19f58AAbD9eD0D60971565AA8510560ab41"
  ],
  "_expected": {
    "detector_id": "sandwich_mev_v1",
    "fires": false,
    "suppression_reason": "attacker_in_settlement_allowlist"
  }
}
```

### §12.5 Negative Fixture — NEG_D13_02 (Cross-Pool Arb, No Victim)

File: `tests/fixtures/ethereum/d13_negative_02_arb_no_victim.json`

```json
{
  "_label": "NEG_D13_02",
  "_description": "Two opposing swaps by the same arbitrageur on the same pool in the same block; no third-party victim swap between them. Must NOT fire — A1 requires a victim (different-address swap) between front and back.",
  "_chain": "ethereum",
  "_token": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
  "_source": "Synthetic — normal arbitrage routing. No victim swap between arb legs.",
  "pools": [
    {
      "chain": "ethereum",
      "pool_address": "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
      "dex": "univ2",
      "token0": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
      "token1": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
      "reserve0_raw": "50000000000000",
      "reserve1_raw": "16666666666666666666666"
    }
  ],
  "swaps": [
    {
      "_role": "arb_leg_1",
      "chain": "ethereum",
      "pool": "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
      "tx_hash": "0xd13neg02arb0000000000000000000000000000000000000000000000000001",
      "sender_address": "0xArbitrageur000000000000000000000000001",
      "to_address": "0xArbitrageur000000000000000000000000001",
      "amount0_raw": "2000000000",
      "amount1_raw": "-663000000000000",
      "direction": "buy_token1",
      "log_index": 5,
      "block_number": 18000000,
      "block_time": "2023-09-01T10:00:00Z",
      "pool_kind": "univ2"
    },
    {
      "_role": "arb_leg_2",
      "chain": "ethereum",
      "pool": "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
      "tx_hash": "0xd13neg02arb0000000000000000000000000000000000000000000000000002",
      "sender_address": "0xArbitrageur000000000000000000000000001",
      "to_address": "0xArbitrageur000000000000000000000000001",
      "amount0_raw": "-2010000000",
      "amount1_raw": "663000000000000",
      "direction": "sell_token1",
      "log_index": 6,
      "block_number": 18000000,
      "block_time": "2023-09-01T10:00:00Z",
      "pool_kind": "univ2"
    }
  ],
  "settlement_contract_allowlist": [],
  "_expected": {
    "detector_id": "sandwich_mev_v1",
    "fires": false,
    "suppression_reason": "no_victim_swap_between_front_and_back"
  }
}
```

---

## §13 REFERENCES.md Rows Proposed (parent agent handles edits)

The following rows are proposed for addition to REFERENCES.md:

| Mechanism | Signal / Formula | Source | Used In | Verified Against |
|-----------|-----------------|--------|---------|-----------------|
| Sandwich / MEV — structural classification | Strict 3-tx ordering (front-victim-back) within same block + same attacker EOA in front+back; profit gate; basis for A1 structural signal | Flashbots mev-inspect-py (archived), github.com/flashbots/mev-inspect-py | D13 §3 algorithm; §7.2 calibration | mev-inspect-py source reviewed 2026-04-24 |
| Sandwich / MEV — profit distribution | Median sandwich profit $32 USD; P5 $0.80; P95 $1,800; min profitable victim swap ~$500 USD; slippage median 0.82%, P5 0.28%, P95 3.2% | Chi, He, Hu & Wang 2024, arXiv:2405.17944 | D13 §4.1 conf formula derivation; §5.1 min_victim_swap_usd; §5.2 min_victim_slippage_pct; §7.1 calibration table | Live fetch 2026-04-24 |
| Sandwich / MEV — CoW Protocol settlement allowlist | CoW Protocol Settlement `0x9008D19f58AAbD9eD0D60971565AA8510560ab41`; batch auction settlement produces F-V-B swap patterns that are NOT sandwich attacks | CoW Protocol documentation, docs.cow.fi; CoW Protocol audit reports (trail of bits 2022) | D13 §5.4 settlement_contract_allowlist; NEG_D13_01 fixture | Live fetch 2026-04-24 |
| Sandwich / MEV — 1inch Fusion settlement | `0x111111125421cA6dc452d289314280a0f8842A65` Fusion settlement; RFQ limit-order auction settlement; allowlist candidate | 1inch documentation, docs.1inch.io/docs/fusion-swap/introduction | D13 §5.4 settlement_contract_allowlist | Live fetch 2026-04-24 |
| Sandwich / MEV — Flashbots Protect relay | Flashbots Protect relay endpoint + MEV-Boost builder pattern; builder-extracted MEV is indistinguishable from searcher MEV from on-chain events only; D13 fires on both | Flashbots documentation, docs.flashbots.net/flashbots-protect/overview | D13 §1.3 background; §8 E-D13-4 (builder MEV); §11 Decision 8 (mempool defer) | Live fetch 2026-04-24 |

---

## Sprint 20 Implementation Checklist (S20-2 scope — developer agent)

This section is for the developer agent (S20-2), not the analyst. Listed for traceability.

- [ ] Write `migrations/postgres/V00015__mev_events.sql` (schema in §6.3)
- [ ] Implement `crates/detectors/src/d13_sandwich_mev.rs` (§3 pipeline, §4 formula)
      Note: reads from `swaps` + `pools` tables; no new EVM decoder required
- [ ] Add `fetch_swaps_for_sandwich` SQL query in `crates/storage/src/pg.rs`
      (Query 1 in §6.4; returns `SwapForSandwichRow` struct)
- [ ] Add `insert_mev_event` write method in `crates/storage/src/pg.rs` (C3 audit write)
- [ ] Add `SandwichMevDetector` to `crates/detectors/src/lib.rs`
- [ ] Register D13 in `crates/server/src/init/detectors.rs::build_all_detectors`
      (gotcha #77 — production wiring entry point)
- [ ] Add `[sandwich_mev_v1]` section to `config/detectors.toml` (§9)
- [ ] Write 5 JSON fixtures (§12.2–§12.5 + POS_D13_03) to
      `tests/fixtures/ethereum/d13_*.json`
- [ ] `supported_chains()` override returns `&[Chain::Ethereum]` (gotcha #67)
- [ ] No `Utc::now()` anywhere in D13 — use `ctx.window.end` (gotcha #22)
- [ ] No `f64` for profit, slippage, or amounts — `Decimal` or `u128` only (CLAUDE.md)
- [ ] All monetary amounts in evidence use string-encoded Decimal (gotcha #9 pattern)
- [ ] `BTreeMap` for all intermediate collections (determinism)
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo test` ≥ 1230 tests passing (estimate; baseline 1206 from Sprint 19)
