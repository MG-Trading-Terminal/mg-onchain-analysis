# Design 0004 — Detector D01: Honeypot (Simulation)

**Date:** 2026-04-21
**Status:** Draft
**Author:** onchain-analyst agent
**ADR refs:** ADR 0001 §D5 (MVP detector #1), ADR 0002 (Postgres-only storage)
**Trait ref:** `docs/designs/0003-detector-trait.md` — implements `Detector` trait, uses `DetectorContext`
**Stub to replace:** `crates/detectors/src/d01_honeypot.rs` (P2-5 stub)
**Detector ID:** `honeypot_sim`

---

## 1. Context

A honeypot token allows buy transactions to succeed but prevents holders from selling, either by reverting sell transactions outright or by extracting the proceeds via a covert mechanism (high transfer fee, delegate burn, freeze). The trading bot entering a honeypot position cannot exit: it cannot recover the invested SOL. This is the single highest-cost failure mode for `bot-trader-2-0` among all six MVP detector categories.

The detector reads `TokenMeta` structural fields and on-chain buy/sell evidence, then optionally simulates a sell transaction via Solana RPC. It is Solana-specific. EVM honeypot detection (via `eth_call` on forked state) is deferred to Phase 4.

This spec is the implementation contract for the P2-5 developer task. It supersedes the stub in `crates/detectors/src/d01_honeypot.rs` and must be implemented exactly as described. Where pseudocode is given, the developer translates it to Rust following the conventions established in `docs/designs/0003-detector-trait.md`.

---

## 2. Solana Honeypot Signal Taxonomy

Six distinct Solana-specific patterns constitute the D01 detection surface. EVM patterns (sell-block via `require()`, ERC-20 blacklist mapping) do not apply.

### S1 — Freeze Authority Active

**What it is:** The `freeze_authority` field on a Solana mint account holds a public key. The holder of that key can call `freeze_account` on any token account, preventing all transfers out of that account indefinitely. The victim can still receive tokens but cannot send, swap, or close the account.

**What it observes:** `TokenMeta.freeze_authority != None`

**What it ignores:** Whether freeze has actually been exercised on any account. Static presence of the authority is the signal; actual freezing events would require scanning all token account states (Phase 3 enhancement).

**Why it matters:** If a deployer retains freeze authority, they can freeze every holder's account immediately after buying and drain the pool while all holders are locked. This is the primary documented Solana honeypot mechanism (see Phantom help docs 2025, Solscan info center 2025, Smithii 2026 — all confirming freeze-authority honeypot as the most common pattern).

**Threshold:** Any non-null `freeze_authority` fires the signal. Weight in confidence formula: **0.25**.

**Known legitimate use:** USDC (`EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`) retains freeze authority for OFAC regulatory compliance. PYUSD (`2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo`) similarly. Mitigation: the jup_verified/jup_strict context signal attenuates confidence — see §6 Confidence Composition.

---

### S2 — Token-2022 Transfer Fee Above Threshold

**What it is:** The `TransferFeeConfig` extension on a Token-2022 mint encodes a basis-points fee deducted from every transfer, including swap-out transfers (sells). The fee accrues as `withheld_amount` on recipient token accounts and can be withdrawn by the `withdraw_withheld_authority`. A 9000 bps (90%) fee means the seller receives 10% of expected proceeds — functionally a near-total sell block.

**What it observes:**
1. `TokenMeta.transfer_fee != None` AND `TokenMeta.transfer_fee.fee_bps > sell_tax_threshold_bps`
2. `TokenMeta.transfer_fee.authority != None` (deployer can still raise the fee)

**Threshold:** `fee_bps > 5000` (50%). Config key: `detectors.honeypot_sim.sell_tax_threshold_bps`. Current value: **5000**. Rationale: no legitimate fee-on-transfer token in any surveyed market operates above 50% transfer fee. Torres et al. 2019 (HoneyBadger) confirmed 87% precision for a sell-tax >50% signal on EVM. Solana equivalent: same threshold applies. See §5 Threshold Table for full derivation.

**Authority sub-signal:** If `transfer_fee.authority != None` AND `transfer_fee.authority != ZERO_ADDRESS`, the deployer can increase the fee at any time. This is a secondary risk even if current bps is below threshold: the fee could be raised to 100% post-listing. Weight: add 0.10 to confidence when authority is live, regardless of current bps level. (`w_fee_auth = 0.10` — resolved from prior spec/config discrepancy; see `docs/reviews/0001-d01-honeypot-evasions.md §9.C1`.)

**What it ignores:** Tokens where `transfer_fee.fee_bps` is below 500 bps (5%) — legitimate fee-on-transfer tokens with small fees exist (e.g., certain DeFi protocols). Sub-5% fee is noise for honeypot detection; it belongs in a "high-fee disclosure" informational signal rather than honeypot.

**Weight in confidence formula:** **0.45** (primary signal — sell tax directly measures sell-ability degradation).

---

### S3 — Token-2022 Permanent Delegate

**What it is:** The `PermanentDelegate` Token-2022 extension grants a designated authority unconditional power to transfer or burn any holder's tokens without their signature. Unlike freeze authority (which blocks sending), permanent delegate allows the attacker to actively drain balances. Documented in Q1 2026: scam factory deployed thousands of tokens using this extension; RugCheck flagged >40% of new tokens using it. One confirmed case ("RED" token): tokens burned 7 seconds after purchase.

**What it observes:** `TokenMeta.permanent_delegate != None` (field added per §8 Evidence Schema note — see developer task below).

**Threshold:** Any non-null `permanent_delegate` fires the signal. No threshold calibration needed: there is no legitimate use case for a memecoin or DeFi token to grant an external authority unconditional burn rights over all holder accounts.

**Weight in confidence formula:** **0.20** (same weight as freeze authority sub-signal; permanent delegate is more severe but less common than transfer fee abuse).

**Note to developer:** `TokenMeta` in `crates/common/src/token.rs` does not currently have a `permanent_delegate` field. This is a FROZEN type. The developer must add it as an enrichment field in `crates/token-registry` (not in `crates/common`) and surface it via `ctx.registry.enrich()`. Alternatively, parse it directly from the RPC `getAccountInfo` response for the mint account. Document the resolution in the detector implementation. See §12 Developer Acceptance Checklist item #3.

---

### S4 — Token-2022 Transfer Hook Present

**What it is:** The `TransferHook` Token-2022 extension causes an arbitrary program to be invoked on every token transfer. The hook program can revert the transfer based on any logic: rejecting sells above a threshold amount, rejecting sells from non-whitelisted addresses, or implementing time-delay logic that disables sells after N blocks.

**What it observes:** `TokenMeta.transfer_hook != None` (program_id is set).

**Threshold:** Presence fires the signal. We cannot statically determine if a hook is malicious without executing it; static presence is the only tractable Phase 2 signal. Phase 3 adds bytecode analysis of the hook program.

**What it ignores:** Hooks on tokens where the hook program is a known-safe program (e.g., Token-2022 interest-bearing extension hook). A known-safe hook program list is a Phase 3 enhancement.

**Weight in confidence formula:** **0.20**.

**Note to developer:** Same `TokenMeta` frozen-type constraint as permanent delegate. Handle via registry enrichment. See §12 Checklist item #3.

---

### S5 — On-Chain Buy/Sell Ratio Evidence (SQL-derived)

**What it is:** If sells are blocked at the token contract or pool level, no sell transfers will appear in the `transfers` table for a pool address over any meaningful window. The ratio of buy-transfers to sell-transfers becomes infinite (zero sells) or very high (few allowed sells), which is detectable via `docs/queries/d01_honeypot.sql`.

**What it observes:** Result of `d01_honeypot.sql` executed against the `transfers` table for the token's primary pool over the `ctx.window` observation window:
- `buy_sell_ratio = 999.0` (sentinel) → zero sells observed → maximum sub-signal
- `buy_sell_ratio > buy_sell_ratio_sentinel` (config: 10.0) → suppressed-sell pattern

**Baseline:** No absolute threshold makes sense without normalization. The sentinel value (999.0) is set by the SQL when `sell_count = 0`. The ratio sentinel (10.0) is calibrated against the RAVE probe (clean token: 0.82 buy/sell ratio) and the principle that any token with 10× more buys than sells over a meaningful window exhibits statistically anomalous sell suppression.

**Threshold:** `buy_sell_ratio > 10.0`. Config key: `detectors.honeypot_sim.buy_sell_ratio_sentinel`. See §5.

**Minimum activity guard:** Only apply this signal when `buy_count >= 20` in the window. Below 20 buys, the pool has insufficient activity to produce a meaningful ratio — a token with 3 buys and 0 sells is not evidence of sell suppression, it may simply be newly listed. Config key: `detectors.honeypot_sim.min_buy_count_for_ratio`. Value: **20**.

**Weight in confidence formula:** **0.20**.

---

### S6 — Simulation-Detected Sell Failure (Gold Standard)

**What it is:** Directly attempts a sell transaction via Solana's `simulateTransaction` RPC method. A failed simulation that shows `err != null` when trying to sell tokens just acquired via a simulated buy is the strongest possible signal. This is the methodology used by Honeypot.is and GoPlus for their production detectors.

**What it observes:** Result of calling `simulateTransaction` with a constructed sell transaction:
- `err != null` → sell reverted → honeypot confirmed
- Post-balance of seller's token account still has tokens → sell instruction executed but tokens not transferred
- SOL received by seller is `< amount_in * (1 - slippage - pool_fee) * (1 - transfer_fee_bps/10000) * (1 - SIMULATION_FLOOR_RATIO)` → covert fee or partial-failure honeypot

**Multiple probe paths:** To catch amount-dependent honeypots (allow small sells, block large ones), simulate `simulate_paths` (config: 3) distinct amounts:
- Path 1: small amount = `pool_reserves_token * 0.001` (0.1% of pool depth)
- Path 2: medium amount = `pool_reserves_token * 0.01` (1% of pool depth)
- Path 3: large amount = `pool_reserves_token * 0.05` (5% of pool depth)

All three amounts use a freshly derived simulated keypair as the seller address. The simulation is `replaceRecentBlockhash = true` (no fresh blockhash needed from mempool).

**Weight in confidence formula:** Simulation failure overrides the weighted sum — see §6 for the override mechanism. A single path failure adds **0.80** to weighted score. All three paths failing → confidence = **1.0** (Critical).

**RPC placement:** `simulateTransaction` is not available via `ctx.store` (Postgres) or `ctx.registry`. The detector receives the RPC reference via a separate parameter outside the `Detector` trait. See §9 Failure Modes and `docs/designs/0003-detector-trait.md` Open Question #3 for the resolution.

---

## 3. Algorithm

The detector runs in two passes: static (always) then simulation (if enabled and RPC available).

### 3.1 Static Pass (no RPC — always runs)

```
FUNCTION detect_honeypot_static(ctx: DetectorContext) -> StaticResult:

  meta = ctx.registry.enrich(ctx.token, ctx.chain).await
  IF meta is Err:
    RETURN Err(MissingDependencyData)

  score_accumulator = 0.0
  signals_fired = []

  // S1: Freeze authority
  freeze_active = meta.freeze_authority IS NOT NULL
  IF freeze_active:
    score_accumulator += 0.25
    signals_fired.push(FreezeAuthority { address: meta.freeze_authority })

  // S2: Transfer fee
  IF meta.transfer_fee IS NOT NULL:
    fee_bps = meta.transfer_fee.fee_bps
    IF fee_bps > sell_tax_threshold_bps:  // config: 5000
      sell_tax_fraction = fee_bps / 10000.0
      // sigmoid scaled: sig((sell_tax - 0.50) / 0.20)
      tax_contribution = sigmoid((sell_tax_fraction - 0.50) / 0.20) * 0.45
      score_accumulator += tax_contribution
      signals_fired.push(TransferFee { bps: fee_bps })
    IF meta.transfer_fee.authority IS NOT NULL AND authority != ZERO_ADDRESS:
      // mutable fee authority: additional risk even if current fee is low
      score_accumulator += 0.10  // w_fee_auth = 0.10 (resolved spec/config discrepancy — review §9.C1)
      signals_fired.push(TransferFeeAuthorityLive { authority: meta.transfer_fee.authority })

  // S3: Permanent delegate (via registry enrichment, not TokenMeta frozen type)
  permanent_delegate = ctx.registry.permanent_delegate(ctx.token).await  // new method
  IF permanent_delegate IS NOT NULL:
    score_accumulator += 0.20
    signals_fired.push(PermanentDelegate { delegate: permanent_delegate })

  // S4: Transfer hook (via registry enrichment)
  transfer_hook = ctx.registry.transfer_hook(ctx.token).await  // new method
  IF transfer_hook IS NOT NULL:
    score_accumulator += 0.20
    signals_fired.push(TransferHook { program_id: transfer_hook })

  // S5: Buy/sell ratio from SQL
  ratio_row = ctx.store.execute(d01_honeypot_sql, params).await
  IF ratio_row has buy_count >= min_buy_count_for_ratio:  // config: 20
    ratio = ratio_row.buy_sell_ratio
    IF ratio > buy_sell_ratio_sentinel:  // config: 10.0
      // linear scale: ratio / (ratio_sentinel * 10) capped at 1
      ratio_contribution = min(ratio / (buy_sell_ratio_sentinel * 10.0), 1.0) * 0.20
      score_accumulator += ratio_contribution
      signals_fired.push(BuySellRatio { ratio: ratio })
  ELSE IF ratio_row has buy_count > 0 AND sell_count == 0 AND buy_count >= min_buy_count_for_ratio:
    // sentinel path: zero sells with meaningful buy activity
    score_accumulator += 0.20
    signals_fired.push(ZeroSells { buy_count: ratio_row.buy_count })

  raw_static_score = score_accumulator  // 0.0 .. ~1.10 (weights sum to 1.10 if all fire)
  static_confidence = sigmoid(raw_static_score / 0.55 - 1.0)
  // Maps: raw=0 → 0.27, raw=0.25 → 0.38, raw=0.55 → 0.50, raw=1.10 → 0.73
  // Calibrated so: single freeze_authority signal → ~0.30, all static signals → ~0.65

  RETURN StaticResult { confidence: static_confidence, signals: signals_fired,
                        sell_tax_bps: fee_bps, freeze_active: freeze_active }
```

**Sigmoid function:** `1.0 / (1.0 + exp(-x))`. Use `Decimal` arithmetic or f64 for internal computation only; round to 4 decimal places before storing in evidence.

**Note on summing weights:** The four static signal weights (0.25 + 0.45 + 0.20 + 0.20 + 0.20 = 1.30) can sum above 1.0. The sigmoid normalization handles this — `sigmoid(1.30/0.55 - 1.0) = sigmoid(1.36) ≈ 0.80`. This is intentional: if all static signals fire simultaneously, the detector should produce high confidence even before simulation.

---

### 3.2 Simulation Pass (RPC required — runs when `simulation_enabled = true` and RPC reference available)

```
FUNCTION detect_honeypot_simulation(
  ctx: DetectorContext,
  rpc: &SolanaRpc,
  static_result: StaticResult
) -> SimulationResult:

  // Select primary pool from meta.markets (prefer Raydium CPMM or AMM v4, then Orca)
  pool = select_primary_pool(meta.markets)
  IF pool is None:
    RETURN SimulationResult { skipped: true, reason: "no_pool_found" }

  path_results = []
  probe_amounts = compute_probe_amounts(pool.reserves, simulate_paths)
  // probe_amounts: [reserves * 0.001, reserves * 0.01, reserves * 0.05]

  FOR i, amount IN enumerate(probe_amounts):
    simulated_buyer = derive_simulation_keypair(i)  // deterministic from (token, pool, i)

    // Step 1: simulate buy
    buy_tx = build_swap_tx(
      program: pool.dex_kind,  // Raydium SwapBaseIn disc=9 or CPMM swap_base_input
      pool: pool.address,
      direction: SOL_TO_TOKEN,
      amount_in: SOL_PROBE_AMOUNT,  // config: 0.01 SOL
      slippage: 0.10,  // 10% slippage tolerance for simulation
      payer: simulated_buyer
    )
    buy_sim = rpc.simulate_transaction(buy_tx, replaceRecentBlockhash=true).await
    IF buy_sim.err IS NOT NULL:
      path_results.push(PathResult { index: i, buy_failed: true, sell_failed: null,
                                     error: buy_sim.err })
      CONTINUE  // buy failed — cannot test sell; record and move on

    tokens_received = extract_token_balance_delta(buy_sim.accounts, pool.token_mint, simulated_buyer)

    // Step 2: simulate sell
    sell_tx = build_swap_tx(
      program: pool.dex_kind,
      pool: pool.address,
      direction: TOKEN_TO_SOL,
      amount_in: tokens_received,
      slippage: 0.10,
      payer: simulated_buyer
    )
    sell_sim = rpc.simulate_transaction(sell_tx, replaceRecentBlockhash=true).await

    IF sell_sim.err IS NOT NULL:
      path_results.push(PathResult { index: i, buy_failed: false, sell_failed: true,
                                     error: sell_sim.err, amount: amount })
    ELSE:
      sol_received = extract_sol_balance_delta(sell_sim.accounts, simulated_buyer)
      expected_min = SOL_PROBE_AMOUNT * (1.0 - 0.10 - pool.fee_pct)  // after slippage + fee
      // Covert-fee check: if received << expected, transfer fee may not be reflected in config
      effective_tax = 1.0 - (sol_received / SOL_PROBE_AMOUNT)
      path_results.push(PathResult { index: i, buy_failed: false, sell_failed: false,
                                     sol_received: sol_received, effective_tax: effective_tax })

  // Compute simulation confidence contribution
  failed_paths = path_results.filter(|r| r.sell_failed OR r.buy_failed)
  n_tested = path_results.len()

  IF n_tested == 0:
    RETURN SimulationResult { skipped: true, reason: "no_paths_tested" }

  IF failed_paths.len() == n_tested:
    // All paths failed: maximum confidence
    sim_confidence_add = 1.0  // overrides weighted sum → final confidence = 1.0
  ELSE IF failed_paths.len() > 0:
    // Partial failure: proportional contribution
    sim_confidence_add = 0.80 * (failed_paths.len() as f64 / n_tested as f64)
  ELSE:
    // All paths succeeded: check for covert fee
    max_tax = path_results.map(|r| r.effective_tax).max()
    IF max_tax > sell_tax_threshold:  // config: 0.50
      sim_confidence_add = sigmoid((max_tax - 0.50) / 0.20) * 0.80
    ELSE:
      sim_confidence_add = 0.0

  RETURN SimulationResult {
    paths_tested: n_tested,
    paths_failed: failed_paths.len(),
    sim_confidence_add: sim_confidence_add,
    path_results: path_results,
    error_reason: failed_paths.first().map(|r| r.error)
  }
```

> **Sprint 7 implementation correction (P6-4 Phase C).** The spec above treats
> any path where `buy_failed OR sell_failed` as contributing to `failed_paths`.
> In practice this false-positives every token: throwaway simulation keypairs
> have no funded wSOL ATA, so the buy step universally fails at "account not
> found" before ever touching the pool. The shipped implementation gates
> `sim_confidence_add = 1.0` on at least one path reaching a successful buy.
> `all-buys-failed` is emitted as `sim_skipped = true, reason =
> "simulation_buys_all_failed"`. Signal B (`buy_success + sell_failed`) is the
> true honeypot indicator — that path remains fully scored by the §3.2 formula.
> Full fix = a follow-up task that prepends ATA-create + wSOL-wrap instructions
> to the simulated buy tx so real buys can execute against mainnet state.

---

### 3.3 Confidence Combination and Event Emission

```
FUNCTION combine_and_emit(static_result, sim_result, meta, ctx) -> Vec<AnomalyEvent>:

  IF static_result.signals.is_empty() AND (sim_result.skipped OR sim_result.sim_confidence_add == 0):
    // No signals at all: emit Info event so auditor can see "we checked"
    RETURN [AnomalyEvent {
      confidence: 0.02,
      severity: Info,
      evidence: build_evidence(static_result, sim_result, "no_signals_fired")
    }]

  final_confidence =
    IF sim_result.sim_confidence_add >= 1.0:
      1.0  // simulation confirmed all paths → maximum
    ELSE:
      min(1.0, static_result.confidence + sim_result.sim_confidence_add)

  severity = compute_severity(final_confidence, static_result, sim_result)
  evidence = build_evidence(static_result, sim_result, meta)

  RETURN [AnomalyEvent {
    detector_id: "honeypot_sim",
    token: ctx.token,
    chain: ctx.chain,
    confidence: final_confidence,
    severity: severity,
    evidence: evidence,
    block_range: ctx.window.block_start..ctx.window.block_end
  }]
```

---

## 4. Dual Algorithm Summary

| Path | When it runs | Primary inputs | Confidence contribution |
|------|-------------|----------------|------------------------|
| Static | Always | `TokenMeta` fields (freeze, fee, permanent_delegate, hook) + SQL buy/sell ratio | Sigmoid of weighted signal sum → 0.0..~0.80 |
| Simulation | When `simulation_enabled=true` AND RPC available AND pool exists | `simulateTransaction` RPC for N probe paths | 0.0 (all pass) to 1.0 (all fail) added to static |
| Combined | Always | Both paths | `min(1.0, static + sim)` with sim=1.0 override |

---

## 5. Threshold Table

| Threshold | Config Key | Current Value | Rationale | Prior Art |
|-----------|-----------|--------------|-----------|-----------|
| Sell tax (transfer fee) | `honeypot_sim.sell_tax_threshold` | **0.30 (30%)** | Lowered from 0.50: compensating control for DG3 simulation deferral. 30% effective sell fee is operational honeypot territory for automated trading. Revert to 0.50 in Phase 3 if FP rate > 5%. See `docs/reviews/0001-d01-honeypot-evasions.md §6` priority #3. | Torres et al. 2019 (EVM baseline); Solana recalibration required Sprint 3 |
| Sell tax in bps | `honeypot_sim.sell_tax_threshold_bps` | **3000** | Same threshold expressed as bps (companion change with sell_tax_threshold; must stay consistent). 3000 bps = 30%. | Same as above |
| Simulation probe paths | `honeypot_sim.simulate_paths` | 3 | Catches amount-dependent honeypots. GoPlus fork-state method uses multiple probe sizes. Cost: 6 RPC calls (3 buy + 3 sell). | Honeypot.is and GoPlus methodology |
| Buy/sell ratio sentinel | `honeypot_sim.buy_sell_ratio_sentinel` | **5.0** | Lowered from 10.0: compensating control for DG3 simulation deferral. Without simulation, S5 is the primary behavioural signal; a 5:1 ratio is already statistically anomalous. RAVE: 0.82; WET: 0.70. Retain 10.0 when simulation re-enabled. See `docs/reviews/0001-d01-honeypot-evasions.md §6.3` control #1. | RAVE probe §D01, review §6.3 |
| Min buy count for ratio | `honeypot_sim.min_buy_count_for_ratio` | 5 | Below 5 buys, statistical noise dominates. A token with 2 buys and 0 sells could simply be newly listed. | Unverified heuristic — calibrate from fixture corpus |
| SOL probe amount | `honeypot_sim.sol_probe_amount_lamports` | 10_000_000 (0.01 SOL) | Small enough to not move price materially; large enough to be above dust threshold on any real pool. | Honeypot.is uses small probe amounts |
| Simulation slippage tolerance | `honeypot_sim.simulation_slippage` | 0.10 (10%) | Wide enough to not reject valid swaps due to price movement during simulation window. | Standard practice; no academic citation |
| Transfer fee authority extra weight | `honeypot_sim.transfer_fee_authority_extra_weight` | **0.10** | Resolved spec/config discrepancy (was 0.05 in spec, 0.15 in config). Midpoint 0.10 adopted. See `docs/reviews/0001-d01-honeypot-evasions.md §9.C1`. | Chainstack blog on Token-2022 security, Offside Security blog |
| Re-evaluation interval | `honeypot_sim.reevaluation_interval_minutes` | **15** | Compensating control for DG3: new tokens re-evaluated every 15 min for first 24h. Catches E10 (delayed freeze) and E13 (oracle-gated honeypot). Consumed by scheduler, not the detector. | Review §E10, §E13, §6.3 control #2 |

### Threshold changes from architect starting values

| Threshold | Architect value | This spec value | Reason for change |
|-----------|----------------|-----------------|------------------|
| `sell_tax_threshold` | 0.50 | **Retained** 0.50 | Confirmed by Torres 2019 and industry standard. |
| `simulate_paths` | 3 | **Retained** 3 | Sufficient for amount-dependent honeypots. |
| `buy_sell_ratio_sentinel` | 10.0 | **Retained** 10.0, with new `min_buy_count_for_ratio = 20` guard | The 10.0 value is unanchored without calibration; retained with an activity guard that prevents false positives on newly listed tokens. |

### New thresholds added beyond architect starting values

- `honeypot_sim.sell_tax_threshold_bps` = 3000 (30%, lowered from 5000; companion to sell_tax_threshold)
- `honeypot_sim.min_buy_count_for_ratio` = 5 (activity guard)
- `honeypot_sim.sol_probe_amount_lamports` = 10_000_000
- `honeypot_sim.simulation_slippage` = 0.10
- `honeypot_sim.transfer_fee_authority_extra_weight` = 0.10 (resolved from 0.05 spec / 0.15 config discrepancy)
- `honeypot_sim.reevaluation_interval_minutes` = 15 (new; compensating control for DG3)

---

## 6. Confidence Composition Formula

### Weights

| Signal | Symbol | Weight |
|--------|--------|--------|
| Sell tax > threshold (S2) | `w_tax` | 0.45 |
| Freeze authority active (S1) | `w_freeze` | 0.25 |
| Buy/sell ratio > sentinel (S5) | `w_ratio` | 0.20 |
| Permanent delegate present (S3) | `w_delegate` | 0.20 |
| Transfer hook present (S4) | `w_hook` | 0.20 |
| Transfer fee authority mutable | `w_fee_auth` | 0.10 |
| **Simulation fail (partial)** | `w_sim_partial` | **0.80 × (failed/total)** |
| **Simulation fail (all paths)** | `w_sim_all` | **1.0 override** |

### Formula

Let `s_i ∈ [0, 1]` be the sub-signal intensity for each fired signal:
- `s_tax = sigmoid((sell_tax_bps/10000 - 0.50) / 0.20)` if fee_bps > sell_tax_threshold_bps, else 0
- `s_freeze = 1.0` if freeze_authority != None, else 0
- `s_ratio = min(ratio / (buy_sell_ratio_sentinel * 10), 1.0)` if ratio > sentinel AND buy_count >= 20, else 0
- `s_delegate = 1.0` if permanent_delegate != None, else 0
- `s_hook = 1.0` if transfer_hook != None, else 0
- `s_fee_auth = 1.0` if transfer_fee.authority active AND bps > 0, else 0

Raw static score:
```
raw = s_tax * 0.45
    + s_freeze * 0.25
    + s_ratio * 0.20
    + s_delegate * 0.20
    + s_hook * 0.20
    + s_fee_auth * 0.10
```

Note: `w_fee_auth = 0.10` resolves the spec/config discrepancy identified in
`docs/reviews/0001-d01-honeypot-evasions.md §9.C1`. The spec previously documented
0.05 while `config/detectors.toml` shipped 0.15. Both are now aligned at 0.10.
A pin test in `crates/detectors/src/d01_honeypot.rs` enforces this alignment.

Static confidence (sigmoid normalization):
```
static_conf = sigmoid(raw / 0.55 - 1.0)
```

This maps:
- `raw = 0.00` → `sigmoid(-1.82)` ≈ 0.14 (background noise level)
- `raw = 0.25` (freeze only) → `sigmoid(-0.55)` ≈ 0.37
- `raw = 0.45` (sell tax only) → `sigmoid(-0.18)` ≈ 0.46
- `raw = 0.55` (freeze + ratio) → `sigmoid(0.0)` = 0.50
- `raw = 0.70` (sell tax + freeze) → `sigmoid(0.27)` ≈ 0.57
- `raw = 1.10` (all five static signals) → `sigmoid(1.00)` ≈ 0.73

Simulation contribution:
```
IF sim_all_paths_failed:
  final_conf = 1.0
ELSE IF sim_partial_fail:
  sim_add = 0.80 * (paths_failed / paths_tested)
  final_conf = min(1.0, static_conf + sim_add)
ELSE IF sim_covert_fee (effective_tax > sell_tax_threshold):
  sim_add = sigmoid((effective_tax - 0.50) / 0.20) * 0.80
  final_conf = min(1.0, static_conf + sim_add)
ELSE:
  final_conf = static_conf
```

If simulation was skipped (RPC unavailable), emit evidence key `honeypot_sim/sim_skipped = "1"` and use `static_conf` as final confidence.

---

## 7. Severity Mapping

| Condition | Severity |
|-----------|----------|
| No signals fired (all static signals absent, sim pass or skipped) | `Info` |
| Single weak static signal only (e.g., freeze authority active with jup_verified=true, OR ratio > sentinel but all other signals clear) | `Low` |
| Two or more weak static signals combined, OR single strong signal (sell_tax above threshold) | `Medium` |
| Sell_tax above threshold AND buy/sell ratio above sentinel, OR simulation partial fail (1-2 of 3 paths fail) | `High` |
| Simulation fails on ALL paths, OR sell_tax above threshold AND simulation fails on ≥1 path | `Critical` |

Implementation:
```
FUNCTION compute_severity(conf, static_result, sim_result) -> Severity:
  IF sim_result.paths_failed == sim_result.paths_tested AND sim_result.paths_tested > 0:
    RETURN Critical
  IF static_result.sell_tax_above_threshold AND sim_result.paths_failed >= 1:
    RETURN Critical
  IF conf >= 0.75:
    RETURN High
  IF conf >= 0.50:
    RETURN Medium
  IF conf >= 0.30:
    RETURN Low
  RETURN Info
```

The `severity_floor()` method on `HoneypotDetector` returns `Severity::High`. This means the scheduler/gateway will never emit a honeypot event below `High` severity even if the formula produces `Low` or `Medium`. Wait — the stub sets `severity_floor = High` but this spec defines finer-grained severities. **Resolution: Remove the `severity_floor` override from the stub**. The floor concept was from the stub's placeholder logic. The real detector implements the full mapping above. The floor is not needed when the formula is correct.

---

## 8. Evidence Schema

All evidence keys use the `honeypot_sim/` prefix per the `evidence_key()` convention in `crates/detectors/src/lib.rs`.

### Required keys (MUST be present on every emitted `AnomalyEvent`)

| Key | Value type | Example | Meaning |
|-----|-----------|---------|---------|
| `honeypot_sim/freeze_authority_active` | Decimal (0 or 1) | `"1"` | 1 = freeze authority set on mint |
| `honeypot_sim/transfer_fee_bps` | Decimal | `"9000"` | Token-2022 transfer fee in basis points; 0 if none |
| `honeypot_sim/buy_sell_ratio` | Decimal | `"0.82"` | buy_count / sell_count from SQL; 999 = zero sells |
| `honeypot_sim/buy_count` | Decimal | `"127"` | Buy transfers to pool in window |
| `honeypot_sim/sell_count` | Decimal | `"0"` | Sell transfers from pool in window |
| `honeypot_sim/simulate_paths_tested` | Decimal | `"3"` | Paths tested (0 if simulation skipped) |

### Conditionally present keys (present when the condition is met)

| Key | Present when | Example value | Meaning |
|-----|-------------|---------------|---------|
| `honeypot_sim/sell_tax_est` | Simulation ran and covert tax found | `"0.91"` | Effective sell tax ratio from simulation (0..1) |
| `honeypot_sim/sim_paths_failed` | Simulation ran | `"2"` | Number of probe paths that failed |
| `honeypot_sim/sim_error_reason` | Simulation ran and at least one path failed | `"InstructionError(Custom(6001))"` | Error from the first failing sell simulation |
| `honeypot_sim/sim_path_results` | Simulation ran | `"0:ok,1:fail,2:fail"` | Compact per-path result (index:ok or index:fail) |
| `honeypot_sim/sim_skipped` | Simulation was not attempted | `"1"` | Set when RPC unavailable or no_pool_found |
| `honeypot_sim/sim_skip_reason` | `sim_skipped = "1"` | `"rpc_unavailable"` | Why simulation was skipped |
| `honeypot_sim/permanent_delegate_active` | Token-2022 permanent_delegate present | `"1"` | 1 = permanent delegate set |
| `honeypot_sim/transfer_hook_present` | Token-2022 transfer_hook present | `"1"` | 1 = transfer hook program set |
| `honeypot_sim/transfer_fee_authority_active` | Transfer fee authority is non-null and non-zero | `"1"` | Deployer can still raise fee |

### `Evidence.addresses` population

Include in `Evidence.addresses`:
- Pool address tested (primary pool)
- Simulated buyer address (if simulation ran) — confirms the simulation keypair used

### `Evidence.tx_hashes` population

Include the most recent successful sell transaction hash found in the `transfers` table for the pool (if any). This confirms "sells WERE working as of TX hash X" — useful for post-hoc review.

### `Evidence.notes` format

Human-readable summary string for auditors. Example:
```
"Sell ratio 0.82 (below sentinel 10.0); freeze_authority null; no transfer fee; simulation: 3/3 paths ok → honeypot signals absent"
```
For a positive fire:
```
"DANGER: Transfer fee 9000 bps (90%) >> threshold 5000 bps; buy/sell ratio 999 (zero sells in 48h window with 312 buys); simulation: 3/3 paths failed (InstructionError Custom 6001) → confirmed honeypot"
```

---

## 9. Failure Modes

### 9.1 TokenMeta not enriched

**Trigger:** `ctx.registry.enrich()` returns `Err` because the token has not yet been indexed.

**Action:** Return `Err(DetectorError::MissingDependencyData { ... })`. The scheduler retries after the next enrichment cycle. Do NOT emit an `AnomalyEvent` with zero evidence — an unenriched token is not evidence of safety.

---

### 9.2 Simulation RPC unavailable

**Trigger:** `simulateTransaction` RPC call returns a network error, timeout, or rate-limit response.

**Action:** Fall back to static signal only. Emit `AnomalyEvent` with static confidence. Set evidence keys `honeypot_sim/sim_skipped = "1"` and `honeypot_sim/sim_skip_reason = "rpc_unavailable"`. Reduce final confidence by a factor of 0.80 to reflect reduced certainty: `final_conf = static_conf * 0.80`. Severity cannot exceed `High` when simulation was skipped (cannot confirm Critical without simulation).

---

### 9.3 Simulation returns ambiguous result

**Trigger:** The simulation completes without `err`, but the token balance delta and SOL balance delta are both zero — the simulation succeeded but produced no meaningful output.

**Action:** Record this as an inconclusive path. Do not count as a fail. Set `sim_path_results` to `"N:inconclusive"`. If all paths are inconclusive, emit at static confidence with `sim_skipped = "1"` and `sim_skip_reason = "all_paths_inconclusive"`. Log at WARN level.

---

### 9.4 No pool found for the token

**Trigger:** `meta.markets` is empty — the token has no DEX pool yet.

**Action:** Simulation cannot run. Set `sim_skipped = "1"` with reason `"no_pool_found"`. Run static signal only. If static signals are absent too, emit `Info` severity with confidence 0.02. If static signals are present (e.g., freeze authority on a token with no pool), emit normally — the absence of a pool is not evidence of safety.

---

### 9.5 d01_honeypot.sql returns empty (no transfers in window)

**Trigger:** The `transfers` table has no rows for this token+pool in `ctx.window`.

**Action:** Set `buy_count = 0`, `sell_count = 0`, `buy_sell_ratio = 0` in evidence. The ratio signal (S5) is suppressed (buy_count < min_buy_count_for_ratio). This is a legitimate outcome for a newly indexed token — do not treat as zero-sell honeypot signal.

---

### 9.6 TokenMeta fields for permanent_delegate or transfer_hook are unavailable

**Trigger:** The registry enrichment does not yet implement `permanent_delegate()` and `transfer_hook()` methods (see §12 item #3).

**Action:** Treat as not-present (no signal fires). Log at DEBUG. These are additive signals — their absence degrades recall but does not cause false positives. This is acceptable for Phase 2 with the plan to add enrichment in Sprint 3.

---

## 10. Fixture Corpus

The files below are in `research/fixtures/honeypot/`. Developer writes `tests/fixtures/honeypot/` test integration pointing at these files.

### Negative Fixtures (detector should produce Info or Low at most)

| File | Mint | Token | Expected Max Confidence | Why Negative |
|------|------|-------|------------------------|-------------|
| `FeqiF7TEVmpYuuj4goD8WgmgyhFgFnZiWtw226wF4hNm.json` | RAVE copycat | RaveDAO | 0.10 | mint=null, freeze=null, standard SPL, 4663 observed sells, ratio 0.82 |
| `WETZjtprkDMCcUxPi9PfWnowMRZkiGGHDb9rABuRZ2U.json` | WET | HumidiFi | 0.10 | mint=null, freeze=null, standard SPL, 464 observed sells, jup_verified |
| `So11111111111111111111111111111111111111112.json` | wSOL | Wrapped SOL | 0.05 | All authorities null, score=1, jup_strict, canonical negative |

### Positive Fixtures (detector should fire with elevated confidence)

| File | Mint | Token | Expected Confidence Range | Which Signal Fires |
|------|------|-------|--------------------------|-------------------|
| `2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo.json` | PYUSD | PayPal USD | 0.20–0.35 | S1 (freeze authority active), S2 absent, simulation expected to PASS |
| `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v.json` | USDC | USD Coin | 0.20–0.30 | S1 (freeze authority for compliance), simulation expected to PASS |
| `SYNTHETIC_high_transfer_fee_positive.json` | SYNTHETIC | — | 0.50–0.70 | S2 (transfer_fee 9000 bps >> 5000 threshold), S5 (zero sells) |
| `SYNTHETIC_permanent_delegate_positive.json` | SYNTHETIC | — | 0.20–0.35 | S3 (permanent_delegate active), S5 (zero sells) |

**Notes on positive fixtures:**

PYUSD and USDC are legitimate tokens that fire the freeze-authority signal (S1) but should produce LOW confidence because jup_verified=true and score=1. These fixtures test that the detector does not mislabel regulated stablecoins as critical honeypots. They are "static-positive, simulation-negative" fixtures: the static signal fires, but simulation should pass, producing a moderate-low confidence event.

The two SYNTHETIC fixtures represent attack patterns documented in Q1 2026 (Solana's Permanent Delegate Burn Scam, March 2026) and the Token-2022 high-transfer-fee scam pattern. They are labeled `synthetic=true` in the JSON meta. The developer MUST replace these with real captured fixtures before Phase 3. The synthetic fixtures serve as unit test inputs for the `compute()` pure function only (not integration tests against real RPC).

**Fixture replacement instruction:** Add a Sprint 3 backlog item: "Capture 3 confirmed-rugged Solana tokens with freeze authority or transfer fee > 5000 bps from RugCheck `rugged=true` corpus; replace SYNTHETIC fixtures in `research/fixtures/honeypot/`."

---

## 11. Known Evasions

### E1 — Whitelist-gated honeypot (caught partially)

**Attack:** The freeze script or transfer hook checks if the seller is on a whitelist of deployer-controlled wallets. The simulation keypair generated by our detector is not on the whitelist, causing simulation to fail — creating a false positive. The real victims are the retail buyers whose wallets are also not whitelisted.

**Our algorithm:** `simulate_paths=3` tests with 3 distinct deterministic simulation keypairs. None will be whitelisted.

**Verdict:** CATCHES the honeypot (simulation fails for our keypairs, which is correct — retail buyers would also fail). False positive risk is low: if the token is genuinely safe, it would not have a whitelist gate that rejects arbitrary buyers.

**Residual risk:** If the deployer pre-whitelists many wallet addresses to create an appearance of sell-ability (inflate the sell count), our SQL buy/sell ratio signal could be fooled. The simulation signal is not fooled.

---

### E2 — Amount-dependent honeypot (caught)

**Attack:** The token allows sells of amounts below X (e.g., below 1000 tokens) but reverts sells above X. This fools detectors that only test one probe amount.

**Our algorithm:** `simulate_paths=3` tests three probe amounts (0.1%, 1%, 5% of pool depth). The large probe (5%) will fail if the honeypot has an amount ceiling.

**Verdict:** CAUGHT by multi-path simulation.

---

### E3 — Time-delay honeypot (MISSED)

**Attack:** Sells work normally for the first M blocks (to pass simulation checks), then a time-lock activates that blocks sells after block N. Alternatively, the deployer manually upgrades the hook program after launch.

**Our algorithm:** Simulation runs at detection time. It cannot observe future state.

**Verdict:** MISSED by simulation. Partial mitigation: the buy/sell ratio signal (S5) is a lagging indicator — if sells stop after block N, the ratio shifts over time. Periodic re-evaluation of the detector on the same token catches this pattern. The scheduler should re-run D01 on any token that has been previously cleared but whose buy/sell ratio rises above the sentinel in a subsequent window.

---

### E4 — Oracle/admin-gated honeypot (MISSED partially)

**Attack:** The transfer hook calls an external oracle or admin account to decide whether to permit the sell. At simulation time, the oracle returns "permit". After launch, the deployer changes the oracle state to "deny".

**Our algorithm:** Simulation tests current oracle state at time of simulation.

**Verdict:** MISSED for post-launch oracle changes. S5 (buy/sell ratio) eventually catches this once sells stop being observed. Schedule re-evaluation.

---

### E5 — Transfer fee raised post-launch (caught by fee authority signal)

**Attack:** Deploy with 0% transfer fee to pass static checks, then raise fee to 90% after listing is established.

**Our algorithm:** The `transfer_fee_authority_active` sub-signal in S2 fires whenever the fee authority is still held by the deployer (even at 0% current fee), adding 0.05 to the weighted sum. This flags the risk of a future fee raise.

**Verdict:** PARTIALLY CAUGHT. The 0.05 weight is small — the static signal fires at low confidence. Re-simulation after any on-chain fee config change (detectable by monitoring for `SetTransferFee` instructions) would catch the post-change state. Phase 3 enhancement: monitor `TransferFeeConfig` account for changes and trigger re-evaluation.

---

### E6 — Circular buy/sell with deployer wallets to inflate sell count (partially caught)

**Attack:** Deployer runs self-trades (buy from wallet A, sell from wallet B, both controlled by deployer) to inflate the sell count and defeat the buy/sell ratio signal (S5). The ratio stays near 1.0 even though real retail sells are blocked.

**Our algorithm:** S5 is fooled. Simulation (S6) is not fooled — the deployer wallets are not the simulation keypairs.

**Verdict:** S5 MISSED, S6 CATCHES. The defense is that simulation is the primary honeypot signal; S5 is a cheaper supporting signal that an attacker can evade at the cost of paying swap fees for the wash trades.

---

### E7 — Minimum-buy enforcer (creates false negative for small probe)

**Attack:** The transfer hook enforces a minimum sell amount (e.g., must sell all tokens in one transaction). Our Path 1 probe (0.1% of pool depth) sells a small amount, which is rejected. But the rejection is due to minimum-sell enforcement, not because sells are blocked.

**Our algorithm:** Path 1 fails but Paths 2 and 3 (larger amounts) may succeed. Partial failure (1 of 3 paths fail) produces a moderate confidence increase (`0.80 * 1/3 ≈ 0.27`), not a Critical event.

**Verdict:** CORRECTLY detected as elevated risk (partial failure), not mislabeled as clear. The minimum-sell constraint is itself a suspicious signal regardless.

---

## 12. Developer Acceptance Checklist

Before marking P2-5 complete, the developer must verify:

### Implementation
- [ ] `HoneypotDetector` replaces the stub in `crates/detectors/src/d01_honeypot.rs` — no code from the stub is preserved except the detector ID constant and existing test infrastructure.
- [ ] `Detector::evaluate()` calls `detect_honeypot_static()` on every invocation.
- [ ] `Detector::evaluate()` calls simulation only when `config.honeypot_sim.simulation_enabled.value == true` AND an RPC reference is injected (per OQ3 resolution in design 0003).
- [ ] The `severity_floor()` override (`Severity::High`) is REMOVED from the stub. Severity is computed by `compute_severity()` per §7 of this spec.
- [ ] All five static signals (S1–S5) are implemented.
- [ ] Simulation produces `simulate_paths` (config value) paths with the three distinct amounts described in §3.2.
- [ ] `simulateTransaction` is called with `replaceRecentBlockhash=true`.
- [ ] Confidence formula matches §6 exactly, including the sigmoid normalization parameters.
- [ ] `final_confidence = 1.0` when all simulation paths fail.
- [ ] `final_confidence = static_conf * 0.80` when simulation was skipped due to RPC unavailability.

### TokenMeta extension fields
- [ ] `permanent_delegate` and `transfer_hook` are surfaced via `ctx.registry.enrich()` extension, NOT by modifying frozen `crates/common` types. If unavailable in Phase 2, signals S3 and S4 are suppressed with a DEBUG log. Document the approach in a code comment.

### Config
- [ ] All thresholds from §5 are present in `config/detectors.toml` under `[honeypot_sim.*]` with `value`, `rationale`, and `refs` fields.
- [ ] New config keys (`sell_tax_threshold_bps`, `min_buy_count_for_ratio`, `sol_probe_amount_lamports`, `simulation_slippage`, `transfer_fee_authority_extra_weight`) are added to `HoneypotConfig` struct in `crates/detectors/src/config.rs`.
- [ ] `simulation_enabled` is a config key of type `Threshold<bool>`.

### Evidence
- [ ] All required evidence keys from §8 are present on every emitted event.
- [ ] Conditional evidence keys are present only when their conditions are met.
- [ ] `Evidence.addresses` includes pool address and simulation keypair address.
- [ ] `Evidence.tx_hashes` includes the most recent successful sell tx hash from `transfers` table, or is empty if none found.
- [ ] `Evidence.notes` contains a human-readable summary string.

### Tests
- [ ] Unit test for `compute_static()` pure function: input = RAVE fixture (all signals absent) → confidence ≤ 0.15.
- [ ] Unit test for `compute_static()`: input = SYNTHETIC_high_transfer_fee fixture (fee_bps=9000, zero sells) → confidence ≥ 0.50.
- [ ] Unit test for `compute_static()`: input = PYUSD fixture (freeze_authority active, jup_verified) → confidence ∈ [0.20, 0.35].
- [ ] Unit test for confidence combination: sim all paths fail → final_confidence = 1.0.
- [ ] Unit test for confidence combination: sim skipped → final_confidence = static_conf * 0.80.
- [ ] Unit test for severity: confidence=1.0 with all-paths-fail → Critical.
- [ ] Unit test for severity: freeze_authority only (no other signals, sim skipped) → Low.
- [ ] Integration test (against Postgres test container): run `d01_honeypot.sql` with canned transfer rows; verify returned `buy_sell_ratio`.
- [ ] Config load test: all new keys present in `config/detectors.toml` and deserialize without error.

### Cross-references
- [ ] `REFERENCES.md` updated with new sources cited in this spec (Honeypot.is methodology, Solana `simulateTransaction` RPC docs, Token-2022 `TransferFeeConfig` extension docs, Token-2022 `PermanentDelegate` extension docs, Permanent Delegate Burn Scam article, Chainstack Token-2022 blog).
- [ ] `config/detectors.toml` updated with new `honeypot_sim` keys.

---

## 13. Design Gaps Requiring Developer Input

Five areas where this spec cannot be fully definitive without implementation context:

### DG1 — RPC injection pattern (OQ3 resolution)

Design 0003 Open Question #3 notes that `simulateTransaction` is not part of `DetectorContext`. This spec states the detector receives an RPC reference "separately". The developer must decide:
- (a) Add `rpc: Option<&'ctx dyn SolanaRpc>` to `DetectorContext` — simplest, makes simulation opt-in at context construction time.
- (b) `HoneypotDetector` holds an `Arc<dyn SolanaRpc>` field injected at construction — avoids changing `DetectorContext` but makes the detector non-generic.

Recommendation: option (a). It extends `DetectorContext` minimally without breaking the other 5 detectors (they just see `rpc: None`). Document the decision in `docs/designs/0003-detector-trait.md` as an addendum.

### DG2 — permanent_delegate and transfer_hook enrichment path

These fields are not in frozen `TokenMeta`. The developer must choose:
- (a) Add new `enrich_token_extensions(mint, chain)` method to `TokenRegistry` that fetches Token-2022 extension data from RPC and caches it.
- (b) Parse the extensions from the existing `getAccountInfo` call inside `enrich_token()` and surface them via new fields on a `TokenExtensions` struct attached to `TokenMeta` at the registry level.
- (c) Parse in the detector itself using the `SolanaRpc` reference — no registry change needed but duplicates RPC logic.

Recommendation: option (b) — parse during existing enrichment, store in a sidecar Postgres table `token_extensions (chain, mint, permanent_delegate, transfer_hook_program, updated_at)`, expose via `TokenRegistry::token_extensions(mint, chain)`. This keeps the frozen `TokenMeta` untouched while making extension data queryable.

### DG3 — Simulation keypair derivation

The spec says "deterministic from (token, pool, i)". The developer must choose a derivation scheme that is:
- Deterministic (same inputs → same output, for reproducibility)
- Not predictable by adversaries who might whitelist pre-computed simulation keypairs
- Generates valid Solana keypairs with a non-zero SOL balance in the simulation context (simulation does not require real balance — `replaceRecentBlockhash=true` and the simulation context uses fake state)

Recommendation: derive from `sha256(token_bytes || pool_bytes || [i as u8])` as a 32-byte seed to `ed25519::from_seed()`. The keypair has no real balance but `simulateTransaction` does not require real SOL for simulation.

> **Sprint 7 status (P6-4 Phase C closure).** Orchestration + pool selection
> (DG4) + deterministic keypair derivation + buy→sell simulation loop +
> confidence combination shipped. Implementation matches the recommendation
> above (SHA-256 seed → `solana_sdk::signer::SeedDerivable::from_seed`, see
> `crates/dex-adapter/src/solana/simulation.rs`). **Pool-state fetching + ATA
> derivation + wSOL wrap are carried as a follow-up task.** Without them,
> `PoolAccountProvider` is `NotWired` in production, so D01 S6 currently
> contributes only `simulate_skipped = true` evidence with reason
> `"pool_account_provider_not_wired"`. Static signals S1–S5 plus §14
> compensating controls remain the live defense until the follow-up lands.
> See `CHANGELOG.md` Sprint 7 entry and `SPRINTS.md §Sprint 7` for details.

### DG4 — Pool selection priority for multi-pool tokens

`meta.markets` may have 1..N pools. The simulation must pick a primary pool. Priority rule needed: prefer the pool with highest `liquidity_usd` among Raydium CPMM, then Raydium AMM v4, then Orca Whirlpool, then any other. If all pools have `liquidity_usd = 0` (not yet enriched), run simulation on all pools up to `simulate_paths` limit and take the worst-case result.

### DG5 — `compute_probe_amounts` and pool reserve source

The algorithm references `pool.reserves` to compute probe amounts as fractions of pool depth. `MarketInfo` in frozen `TokenMeta` does not have a `reserves` field, only `liquidity_usd` and `lp_provider_count`. Options:
- Use `liquidity_usd / price` to approximate token reserves (requires price oracle).
- Use `sol_probe_amount_lamports` config value as a fixed probe (simpler, no reserves needed).
- Fetch reserves from the pool account via `getAccountInfo` in the detector.

Recommendation: use `sol_probe_amount_lamports` as a fixed SOL input for the buy simulation. The three paths then differ only in the buyer wallet (not in input amount). This simplifies the implementation and avoids the reserves-fetch dependency. The amount-dependent honeypot detection becomes "whitelist-per-wallet" detection rather than "amount-threshold" detection — which is still useful. Document this simplification in the detector code.

---

## 14. Compensating Controls for DG3 Simulation Deferral

Simulation (S6) is deferred to Phase 3 pending `crates/dex-adapter` instruction builders.
Without simulation, approximately 25–40% of real-world Token-2022 honeypots will produce
only a Low-severity event (see `docs/reviews/0001-d01-honeypot-evasions.md §6.2`).

Three compensating controls are in place for the static-only Phase 2 / Sprint 2 exit.
These controls are formally accepted as the risk mitigation package in lieu of simulation.

### Control 1 — Lowered `buy_sell_ratio_sentinel` (10.0 → 5.0)

Without simulation, S5 is the primary behavioural signal. A token with a 7:1 buy/sell
ratio is already statistically anomalous for a normally-trading token (RAVE probe: 0.82;
WET probe: 0.70). The original 10.0 threshold was calibrated for sim-backed mode where S6
catches the cases S5 misses. In static-only mode 5.0 is the appropriate value.

When simulation is re-enabled (Phase 3), restore `buy_sell_ratio_sentinel` to 10.0.

Source: `docs/reviews/0001-d01-honeypot-evasions.md §6.3` control #1.

### Control 2 — Added `reevaluation_interval_minutes = 15`

Any token that produces a D01 event must be re-evaluated every 15 minutes for the first
24 hours after listing. This catches:
- **E10 (delayed freeze)**: freeze authority retained but not activated at launch; timer-
  locked PDA activates freeze after accumulation. Re-evaluation detects the change in
  the token's authority state or the onset of `freeze_account` instructions.
- **E13 (oracle-gated honeypot)**: transfer hook calls an attacker-controlled oracle that
  returns "permit" at detection time but is flipped to "deny" after accumulation. Re-
  evaluation with a fresh simulation (once S6 is implemented) catches the oracle flip.

**Scheduler implementation required (Sprint 2 exit test or early Sprint 3):** The
detector itself does not schedule re-evaluation. The `reevaluation_interval_minutes`
config value is consumed by the server scheduler in `crates/server`. A TODO comment in
`crates/detectors/src/d01_honeypot.rs` tracks this dependency.

Source: `docs/reviews/0001-d01-honeypot-evasions.md §6.3` control #2.

### Control 3 — Lowered sell-tax thresholds (`sell_tax_threshold` 0.50→0.30, `sell_tax_threshold_bps` 5000→3000)

The original 50% threshold was derived from EVM data (Torres 2019). Solana Token-2022
fees can be set at any bps value. A 30% effective sell fee means the seller recovers only
70 cents per dollar — operational honeypot territory for any automated trading use case.
The two threshold values are a linked pair and must remain numerically consistent.

Source: `docs/reviews/0001-d01-honeypot-evasions.md §6` priority #3.

### Known Worst-Case False Negative

A crafted token designed to score as low as possible while still being a functional
honeypot scores 0.276 / `Severity::Low` (static-only mode):
- No freeze authority (S1 = 0), no transfer fee (S2 = 0), no permanent delegate (S3 = 0)
- Transfer hook present (S4 = 0.20)
- Deployer wash-sells maintain buy/sell ratio at 4.5 (below new 5.0 sentinel → S5 = 0)

This worst-case false negative is documented in `docs/reviews/0001-d01-honeypot-evasions.md §5`
(the E11 crafted token). It cannot be caught without simulation (S6) or Phase 3 wallet
clustering to detect the wash-sell inflation. Bot-trader-2-0 integration documentation
must note this limitation explicitly.

---

## References (new sources added by this spec)

See `REFERENCES.md` for rows added. New sources beyond what was already in REFERENCES.md:

1. Honeypot.is simulation methodology — `api.honeypot.is/v2/IsHoneypot` (live API call 2026-04-21). Used for simulation probe design.
2. Solana `simulateTransaction` RPC — `solana.com/docs/rpc/http/simulatetransaction` (fetched 2026-04-21). Used for simulation algorithm.
3. Token-2022 `TransferFeeConfig` extension — `solana.com/docs/tokens/extensions/transfer-fees` (fetched 2026-04-21). Used for S2 signal design.
4. Token-2022 `PermanentDelegate` extension — `solana.com/developers/guides/token-extensions/permanent-delegate` (referenced 2026-04-21). Used for S3 signal design.
5. Permanent Delegate Burn Scam, March 2026 — dev.to/ohmygod, CryptoRank.io (fetched 2026-04-21). Empirical evidence for S3 signal prevalence ($50M+ Q1 2026 losses).
6. Chainstack Token-2022 Transfer Hooks blog — `chainstack.com/solana-token-2022-fee-transfer-hooks/` (referenced 2026-04-21). Technical detail on hook execution context.
7. Offside Security Token-2022 Best Practices Part 2 — `blog.offside.io/p/token-2022-security-best-practices-part-2` (referenced 2026-04-21). Security framing for mutable fee authority risk.
8. Phantom help docs: Frozen tokens on Solana — `help.phantom.com/hc/en-us/articles/29763090277139` (referenced 2026-04-21). Consumer-facing documentation of freeze authority honeypot.
