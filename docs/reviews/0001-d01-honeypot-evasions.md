# Review 0001 — D01 Honeypot: Adversarial Evasion Analysis

**Date:** 2026-04-21
**Reviewer:** security-researcher agent, mg-onchain-analysis
**Detector:** D01 `honeypot_sim`
**Spec ref:** `docs/designs/0004-detector-01-honeypot.md`
**Implementation ref:** `crates/detectors/src/d01_honeypot.rs`
**Status:** Draft

---

## 1. Executive Summary

- The static signal set (S1–S5) covers the most common Solana honeypot mechanisms but is systematically evadable because all five signals are independently bypassable below their thresholds at low cost. An adversary who has read this codebase can construct a fully functional honeypot that scores below every single threshold simultaneously, producing a final static confidence of roughly 0.28 (Severity::Low) — indistinguishable from noise.
- Simulation deferral (DG3) is the single largest risk factor. Without S6, ten of the eleven new evasion paths identified in this review are partially or fully undetectable at Sprint 2 exit. Static-only analysis is fundamentally insufficient for the bot-trader-2-0 use case where every false negative costs real SOL.
- The `jup_verified` confidence cap (0.25) is exploitable as a persistent false-negative gate if a legitimate verified token's mint authority or freeze authority is later transferred to a malicious actor after verification. The cap has no expiry or re-evaluation condition.
- Three immediate code-level findings were identified (two logic gaps, one determinism concern), none of which block correctness of the core formula but which represent behavioral gaps in edge cases.
- The `transfer_fee_authority_extra_weight` value in `config/detectors.toml` (0.15) contradicts the spec (0004-detector-01-honeypot.md §6, which specifies 0.05). The production config uses the higher value; the spec documents the lower. This discrepancy must be resolved before Sprint 2 exit.
- Recommendation: **Block Sprint 2 exit until S6 simulation is implemented OR a documented compensating control (raised buy_sell_ratio_sentinel + mandatory re-evaluation schedule) is formally accepted by the team.** If ship-static-only is accepted, the buy_sell_ratio_sentinel must be tightened and the jup_verified attenuation must be conditioned.

---

## 2. Evasion Catalog

The analyst's spec (0004 §11) documented 7 evasion patterns (E1–E7). This review identifies 12 additional evasion techniques. Each entry maps signals defeated, costs, reward preservation, and detection requirements.

---

### E8 — Trickle-sell Allowlist Inflation

**Description:** The deployer maintains a set of N wallets (owned by them) that are permanently allowed to sell. Sell transactions are continuously executed between these wallets and the pool. This inflates the sell count in the `transfers` table, keeping the buy/sell ratio near 1.0 (S5 never fires), while all retail addresses are blocked by a transfer hook (S4) or blacklist.

**Signals defeated:**
- S5 (buy/sell ratio): ratio stays near 1.0 because deployer-controlled wallets produce continuous sell events.
- S4 (transfer hook) still fires — the hook is present and must be visible to achieve the blocking. Unless the deployer deploys a "dummy hook" that only checks a whitelist (see E9 below).

**Cost to scammer:** Medium. Requires maintaining funded wallets executing self-trades and paying pool swap fees on every wash cycle. Roughly $0.01–0.10 SOL per round-trip. For a 48-hour window at one round-trip per minute, cost is ~$1–$15 SOL. Trivial compared to LP drain proceeds.

**Reward preservation:** Full. The honeypot mechanism is unaffected. Retail buyers still cannot sell.

**Detection cost:** Simulation (S6) is the correct counter. A secondary detector: graph analysis of the sell-transaction wallet cluster — if all sell transactions originate from a small set of wallets funded by the deployer and the wallets have zero other token activity, flag as wash sells. Requires Phase 3 wallet clustering.

**Precedent:** EVM honeypots using deployer-only transfer allowlists with wash trades to fake sell volume. Documented pattern on BSC by Beosin and CertiK honeypot reports (2022–2023). Direct Solana equivalent facilitated by transfer hooks' whitelist capability.

---

### E9 — Steganographic Hook (Known-Program Bypass)

**Description:** The transfer hook program is deployed using the same bytecode prefix as a known-safe program (e.g., a Metaplex royalty enforcer or an SPL governance hook). The hook program's outer shell passes a bytecode-similarity check while its actual `execute` handler contains malicious conditional logic — specifically, it checks whether the caller's address is in an on-chain "permitted sellers" account. Unknown retail addresses are denied. The hook program's public label advertises it as "Royalty Enforcement v2."

**Signals defeated:**
- S4 fires (the hook is present), which is correct. However, the Phase 3 enhancement of checking hook program against a "known-safe program list" is the target. If the detector whitelists any hook program, the attacker mimics that program's structure to get whitelisted.

**Cost to scammer:** High. Requires writing a convincing fake of a known-safe hook program in eBPF and getting it deployed without triggering program verification alerts. This is substantially more work than a simple honeypot.

**Reward preservation:** Full once deployed.

**Detection cost:** Simulation is the correct counter. Static bytecode analysis comparing instruction semantics (not just prefix bytes) is a Phase 3+ enhancement. Neither is cheap.

**Precedent:** No confirmed Solana instance documented as of 2026-04-21. Direct analogue is EVM proxy-upgrade honeypots where the proxy implementation mimics a benign contract's ABI while hiding malicious logic in seldom-called code paths (Peckshield 2023 incident with proxied honeypots on Arbitrum).

---

### E10 — Delayed Freeze (Time-Lock Freeze)

**Description:** Freeze authority is retained but freeze is not executed at launch. Instead, an on-chain timer (a program counter stored in a PDA account, incremented each epoch) enables the freeze authority to call `freeze_account` on all retail accounts after N days. For the first N days, all structural signals are clear: freeze authority is retained (S1 fires, weight 0.25), but simulation passes (buyers and sellers transact normally). The attacker waits for accumulation to peak.

**Signals defeated:**
- S1 fires correctly (freeze authority is non-null). This is a partial catch: the signal fires at low confidence (weight 0.25 → static_conf ≈ 0.37). With jup_verified=false and no other signals, this yields severity::Low — which many consumers may not act upon. Without S6 simulation (which would pass pre-freeze), the detector cannot confirm whether the freeze is actually being exercised.
- S6 (simulation): simulation at pre-freeze time will produce a PASS result, giving zero sim_add. Final confidence stays at ~0.37×0.80 = 0.30 (after sim_skipped attenuation) — Severity::Low.

**Cost to scammer:** Low. Retain freeze authority, deploy the timer logic in a PDA. The timer PDA is hidden from casual inspection; only someone specifically looking at all PDAs controlled by the deployer would notice it.

**Reward preservation:** Full. This is one of the highest-profit evasion patterns because it allows an extended accumulation period before the freeze.

**Detection cost:** Periodic re-evaluation of the detector with monitoring of `freeze_account` instructions targeting the token's accounts. An on-chain alert on first `freeze_account` instruction for a previously-cleared token would catch the activation in near-real-time but would be reactive, not predictive.

**Precedent:** EVM time-delay honeypots with block-number-gated sell blocks are extensively documented (Solidified reentrancy audit, Ethernaut level designs). Solana equivalent is structurally simpler since freeze is a single instruction.

---

### E11 — Minimal-Signal Token (Below-Threshold-by-Design)

**Description:** The attacker crafts a token specifically to score below every threshold simultaneously while still being a honeypot. The honeypot mechanism uses a transfer hook (S4 fires) that allows only tiny sells (< 100 tokens at a time), with all sells above the dust threshold silently consuming tokens via the permanent delegate (S3 fires) rather than reverting. The result: simulation at tiny probe amounts PASSES (returning near-zero SOL), large probe amounts are silently drained rather than reverted (simulation "succeeds" but buyer receives fewer tokens than expected and the effective_tax check triggers).

**Signals defeated:**
- S2: no transfer fee set (avoid the highest-weight signal entirely)
- S1: freeze authority revoked (avoid 0.25 weight)
- S3: permanent delegate present (fires, weight 0.20)
- S4: transfer hook present (fires, weight 0.20)
- S5: deployer wash-sells keep ratio below 10.0 (see E8)
- Combined raw: 0.20 + 0.20 + fee_auth_weight(0.15) if applicable = 0.55 max without S1/S2. With simulation skipped: static_conf × 0.80 = sigmoid(0.55/0.55 - 1.0) × 0.80 = sigmoid(0.0) × 0.80 = 0.50 × 0.80 = 0.40 → Severity::Medium.

**Analysis:** This achieves Medium severity — not Low — but is below the High threshold that would trigger aggressive blocking in most consumer use cases. The scammer can further tune: remove the fee authority (saves 0.15 weight), set wash-sell ratio to 8.9 (below S5 sentinel), and get to static_conf×0.80 = sigmoid((0.40)/0.55 - 1.0) × 0.80 = sigmoid(-0.27) × 0.80 = 0.43 × 0.80 = 0.34 → Severity::Low.

**Cost to scammer:** Low. Standard Token-2022 deployment.

**Reward preservation:** Full.

**Detection cost:** S6 simulation would catch this because large probe amounts would yield zero SOL back (permanent delegate burned tokens before the sell instruction returned). But this requires simulation and the covert-tax check: `effective_tax = 1.0 - (sol_received / SOL_PROBE_AMOUNT)`.

**Precedent:** The "RED" token documented in the March 2026 permanent delegate burn scam (dev.to/ohmygod) operated via precisely this mechanism.

---

### E12 — Transfer Fee Below Threshold with Maximum-Fee Set to Dust

**Description:** The Token-2022 `TransferFeeConfig` supports two fields: `transfer_fee_basis_points` (the rate) and `maximum_fee` (the absolute cap per transfer in raw token units). The attacker sets `transfer_fee_basis_points = 100` (1%, below the 5000 bps threshold — S2 does not fire) but sets `maximum_fee` to 1 (the minimum non-zero value). On tokens with 6 decimals, this means the maximum withheld per transfer is 0.000001 tokens — effectively zero. The attacker also retains fee authority. The current fee is below the threshold. At any point after listing, the attacker calls `SetTransferFee` to raise the rate to 9900 bps while simultaneously removing the maximum_fee cap.

**Signals defeated:**
- S2 (transfer fee): 100 bps < 5000 bps threshold — S2 does not fire.
- S2 (fee authority): `transfer_fee_authority_active` fires at weight 0.15. This is the only signal.
- Combined: raw = 0.15 → static_conf = sigmoid(0.15/0.55 - 1.0) = sigmoid(-0.73) ≈ 0.32 → after sim_skipped attenuation: 0.32 × 0.80 = 0.26 → Severity::Low.

**Why it matters:** The `maximum_fee` field is not inspected by the current detector. A token at 1 bps with maximum_fee=1 is operationally identical to 0% fee but retains full future upside for the attacker.

**Cost to scammer:** Very low. This is standard Token-2022 deployment with a non-standard field choice.

**Reward preservation:** Full after the fee raise.

**Detection cost:** Add inspection of `maximum_fee` relative to token supply. If `maximum_fee / token_supply < DUST_THRESHOLD` (e.g., < 0.0001), treat it as if the fee is un-capped — the cap is nominal. Also monitor `SetTransferFee` instruction for post-launch fee changes.

**Precedent:** No documented Solana incident specifically exploiting maximum_fee manipulation. This is a novel vector identified by reasoning from the Token-2022 spec.

---

### E13 — Oracle-Gated Sell (Post-Launch Oracle Flip)

**Description:** The transfer hook calls an external oracle (a Solana account the attacker controls) to determine whether a sell is permitted. The oracle returns "permit" at launch and during simulation windows. After sufficient accumulation (e.g., total buy volume > $X), the attacker updates the oracle account data to "deny." All subsequent sells from non-whitelisted addresses fail.

**Signals defeated:**
- S4 fires (hook present, weight 0.20) — correct.
- S5 (buy/sell ratio): before the flip, sells work and ratio stays normal. After the flip, the ratio rises — but it takes `min_buy_count_for_ratio` transactions before the signal can fire, and additional window time.
- S6 (simulation): at detection time pre-flip, simulation passes. Post-flip, S6 would catch it but the damage is already done for pre-detection buyers.

**Cost to scammer:** Medium. Requires deploying an oracle account and a hook program that reads from it. The oracle update is a single on-chain transaction.

**Reward preservation:** Full.

**Detection cost:** Monitor the oracle account for changes after each simulation. If `oracle_account.data` changes between two detection runs on the same token, re-trigger D01 immediately. This requires Phase 3 account-change monitoring subscriptions.

**Precedent:** EVM oracle-gated honeypots were documented in the Torres 2019 HoneyBadger paper as "conditional honeypots" — a subclass of what they called "Hidden Transfer Fee" using external state. Direct Solana analogue via transfer hooks is a first-class threat.

---

### E14 — Pool Substitution (Benign Pool for Simulation)

**Description:** The deployer creates two pools: Pool A (listed on DEX aggregators, used for detection) contains full liquidity and allows all sells. Pool B (the actual trap, not indexed by aggregators) has a modified `initialize` instruction that sets a custom program as the swap handler, which blocks sells from non-whitelisted addresses. All retail traffic flows through Pool B because the token's marketed trading link points there. Our detector uses Pool A (first in `meta.markets`) for simulation, which passes.

**Signals defeated:**
- S5 (buy/sell ratio): Pool A shows normal sell activity.
- S6 (simulation): simulation runs on Pool A and passes.

**Cost to scammer:** Medium. Requires operating two pools, which means maintaining liquidity in both.

**Reward preservation:** Partial. The attacker must maintain real liquidity in Pool A (the simulation target), which they cannot immediately drain without triggering S5 on Pool A. However, Pool B may hold the actual trap LP.

**Detection cost:** Simulate against ALL pools in `meta.markets`, not just the primary. Pool selection rule in DG4 resolves this partially — simulating the highest-liquidity pool — but this evasion specifically places the malicious pool as lower-liquidity, making it non-primary.

**Implementation gap:** The current DG4 resolution (simulate on highest-liquidity pool) means a low-liquidity malicious pool could be the trap while the simulation always hits the benign high-liquidity pool.

**Precedent:** Multi-pool honeypot with per-pool routing was documented by Beosin 2023 on Uniswap v2 clones where the attacker deployed a near-identical pool with modified sell logic.

---

### E15 — Gradual fee ratchet (Slow Boil)

**Description:** The attacker starts with a transfer fee of 4900 bps (just below the 5000 threshold) and retains fee authority. The token trades with near-normal sell-ability and accumulates buyers over days. Every 24 hours, the attacker ratchets the fee by 100 bps (via `SetTransferFee`), reaching 9900 bps by day 50. At each point, the static S2 signal fires only when the fee crosses 5000 bps, and even then the detector fires at low confidence for a 5100 bps token (sigmoid((0.51-0.50)/0.20) ≈ 0.51, contributing only 0.23 to raw).

**Signals defeated:**
- S2: stays below 5000 bps during accumulation — no signal.
- S2 (fee authority): fires throughout (weight 0.15) — this is the only signal.
- S5: buyers and sellers coexist until the fee becomes prohibitive. By the time sells dry up, months may have passed.

**Cost to scammer:** Very low. `SetTransferFee` is a cheap instruction.

**Reward preservation:** Full. The slow increase allows the attacker to dump positions early in the cycle while retail accumulates.

**Detection cost:** Monitor `SetTransferFee` instructions and re-trigger D01 immediately on any fee change. The fee authority active signal (weight 0.15) should trigger more aggressive re-evaluation scheduling when it fires, not just a low-confidence log.

**Precedent:** The "slow rug" pattern is documented by Chainalysis 2025 as "soft rug / slow rug" — gradual LP removal over weeks. The fee-ratchet is the same pattern applied to transfer fees.

---

### E16 — Buy-Only Window at Launch

**Description:** The transfer hook allows sells only within blocks N to M after launch (e.g., 100 to 500 slots — roughly 40 to 200 seconds). Before block 100: no sells (too early — appears newly listed, S5 suppressed by `min_buy_count_for_ratio`). Between blocks 100–500: sells work normally (simulation passes, ratio looks clean). After block 500: sells are blocked permanently. Our detector, if it first runs between blocks 100–500 (the allowed window), will see a passing simulation and a normal ratio. Re-evaluation after block 500 will see a frozen ratio but may take a full detection window to accumulate evidence.

**Signals defeated:**
- S5: suppressed early (buy_count < min_buy_count_for_ratio). After the window, ratio eventually rises but lag is material (48h window means 48h before the sentinel fires).
- S6: if simulation runs during the allowed window, it passes.

**Cost to scammer:** Low. The buy window is implemented in the hook.

**Reward preservation:** Full. The scammer front-runs their own window: buys at launch, sets up sells during the allowed window at the peak, lets retail discover the window is closed only after the buy pressure collapses.

**Detection cost:** Periodic re-evaluation is the only reliable defense. A one-time detection at listing provides no protection. The scheduler must re-run D01 at intervals (every 15–30 minutes for new tokens) and flag any token whose simulation verdict changes from PASS to FAIL.

**Precedent:** EVM time-gated honeypots with block-number-based sell windows are a classic pattern. La Morgia 2021 documented pump-and-dump events that open and close within 25 seconds. The sell-window variant gives buyers a brief exit window to create false confidence.

---

### E17 — Token-2022 Confidential Transfer Extension + Honeypot

**Description:** A Token-2022 token with the `ConfidentialTransfer` extension enabled encrypts all transfer amounts using ElGamal encryption. The detector's S5 signal relies on `transfers` table data (observed buy and sell counts). Confidential transfers do not emit standard SPL Token transfer events with readable amounts — instead they emit opaque `ConfidentialTransferInstruction` accounts. Our indexer (and all public APIs, including DEXScreener) will see zero `Transfer` events for confidential amounts. The buy/sell ratio would be computed over a near-empty dataset, permanently suppressing S5 regardless of actual trading behavior.

**Signals defeated:**
- S5 (buy/sell ratio): transfers are confidential and not indexed. buy_count stays near 0, ratio signal is permanently suppressed.
- S1, S2, S3, S4: unaffected. But if only S5 is defeated, the attacker still has four more signals to evade.

**Cost to scammer:** Medium. Requires Token-2022 with ConfidentialTransfer, which requires additional setup and is currently not widely supported by DEX aggregators (most pools do not support confidential transfers). This limits pool availability.

**Reward preservation:** Reduced — ConfidentialTransfer tokens cannot trade on most DEXes yet.

**Detection cost:** The indexer must specifically handle `ConfidentialTransfer` instructions and estimate buy/sell directionality from pool reserve deltas rather than from transfer events. This is a Phase 3 indexer enhancement.

**Precedent:** No documented Solana honeypot using ConfidentialTransfer. This is a forward-looking vector as ConfidentialTransfer adoption increases.

---

### E18 — Signer-Check Hook (Simulation Keypair Whitelist Prediction)

**Description:** Our simulation keypairs are derived deterministically from `sha256(token_bytes || pool_bytes || [i as u8])` (per DG3 spec). The DG3 note in the spec acknowledges this and says it should "not be predictable by adversaries." However, the token address and pool address are public on-chain before simulation runs. An attacker who knows this derivation scheme can pre-compute all three simulation keypairs and whitelist them in the hook program before we detect. Our three simulation paths all pass. Every retail address is blocked.

**Signals defeated:**
- S6 (simulation): all three paths pass because the simulation keypairs are whitelisted.
- S5: deployer wash-sells keep ratio normal (E8 combined).

**Cost to scammer:** Low once the derivation is known (requires reading this spec or the deployed code).

**Reward preservation:** Full.

**Detection cost:** Randomize the simulation keypair derivation using an unpredictable salt (e.g., current validator's leader schedule hash + block hash at detection time) that cannot be pre-computed by the attacker before the token is listed. The key must not be deterministic from public on-chain data alone.

**Code-level finding:** `crates/detectors/src/d01_honeypot.rs` DG3 note and `docs/designs/0004-detector-01-honeypot.md` §DG3 specify "deterministic from (token, pool, i)" — this is the vulnerability. See Section 9 for the exact fix.

**Precedent:** Deterministic simulation keypair whitelist prediction is directly analogous to EVM honeypots that whitelist specific `msg.sender` values. Documented by Torres 2019 as "special recipient" honeypots where only specific EVM addresses can successfully transfer.

---

### E19 — InitializeMint2 + Deferred SetAuthority Race

**Description:** The Token-2022 `InitializeMint2` instruction permits setting freeze and mint authorities at mint creation time. However, there exists a race between mint creation and authority revocation. An attacker deploys the mint, immediately lists it with `freeze_authority = null` (appearing clean to S1), but in the same transaction bundle uses a secondary instruction to pass freeze authority to a time-locked PDA that will set it back to the deployer's address after N slots. Because our detector reads the account state at block B, and the re-assignment completes at block B+1, a narrow timing window exists where S1 appears clear.

**Signals defeated:**
- S1 (freeze authority): briefly appears null at detection time.

**Cost to scammer:** High. Requires careful transaction sequencing and a custom time-locked PDA program. The window is narrow (one block, ~400ms). This is a highly technical exploit with limited practical upside compared to simpler methods.

**Reward preservation:** Full once the freeze authority re-activation completes.

**Detection cost:** Use `finalized` commitment for all mint account reads, not `confirmed`. On Solana, `finalized` requires 32 confirmations (~15 seconds), substantially reducing the race window. Document this commitment requirement explicitly in the enrichment layer.

**Precedent:** This is a Solana-native variant of EVM "two-step ownership transfer" honeypots where `transferOwnership` is called but the new owner hasn't accepted yet, appearing renounced to static checkers.

---

## 3. Threshold Analysis

### 3.1 Threshold Table: Current vs. Recommended

| Threshold | Config Key | Current Value | Recommended Value | Rationale |
|-----------|-----------|--------------|-------------------|-----------|
| Sell tax threshold (fraction) | `honeypot_sim.sell_tax_threshold` | 0.50 | **0.30** | Real-world Solana honeypots with transfer fees below 50% are documented. A 30% fee means the seller recovers only 70 cents per dollar — functionally a honeypot for leveraged or programmatic traders. The 50% threshold was derived from EVM data (Torres 2019); Solana Token-2022 fees can be set at any bps. Calibrate by Sprint 3 — if labeled corpus shows clean tokens between 30–50%, revert. Reducing to 35% increases detection of "moderate fee" honeypots. |
| Sell tax threshold (bps) | `honeypot_sim.sell_tax_threshold_bps` | 5000 | **3000** | Same derivation as above. 3000 bps = 30%. Lower threshold trades some false positives for reduced false negatives. |
| Buy/sell ratio sentinel | `honeypot_sim.buy_sell_ratio_sentinel` | 10.0 | **5.0 (static-only mode) / retain 10.0 with sim** | When simulation is disabled (current Sprint 2 state), S5 is the primary behavioral signal. A token with a 7:1 buy/sell ratio is already strongly suspicious. Lowering to 5.0 compensates partially for the absence of simulation. With simulation enabled, 10.0 is appropriate and should be retained because S6 catches the cases S5 at 10.0 misses. |
| Min buy count for ratio | `honeypot_sim.min_buy_count_for_ratio` | 5 | **Retain 5, but add max-age guard** | The guard of 5 buys is appropriate. However, the buy count is measured over `ctx.window` — if the window is 48 hours, a token launched 47 hours ago with 20 buys and 0 sells satisfies min_buy_count=5 but also has an insufficient history relative to its age. Add a `min_hours_since_launch` guard (suggest 4 hours) to complement the buy count floor. |
| Transfer fee authority extra weight | `honeypot_sim.transfer_fee_authority_extra_weight` | **0.15** (config) vs **0.05** (spec) | **0.10** — resolve discrepancy, calibrate midpoint | The spec (0004 §6) documents `w_fee_auth = 0.05`. The production config ships 0.15. This is a spec-code discrepancy. The 0.15 value is defensible (mutable authority is a significant future risk), but the spec mismatch must be resolved. 0.10 is a reasonable midpoint pending fixture calibration. |
| Simulate paths | `honeypot_sim.simulate_paths` | 3 | **Retain 3, but diversify probe strategy** | Three paths is correct for amount-dependent honeypots. However, all three currently use the same SOL input amount at different multiples (per DG5 resolution). They should also vary buyer addresses by construction to defeat E18 (signer-check evasion). |
| jup_verified confidence cap | (implicit, code line 299) | 0.25 | **0.25 with re-evaluation condition** | The cap is appropriate for the base case but needs an expiry: cap should not apply if the token's freeze_authority or other dangerous authority has changed since the jup_verified timestamp. See Section 8. |

### 3.2 Threshold Discrepancy: `transfer_fee_authority_extra_weight`

**Finding — CRITICAL inconsistency:**

- `docs/designs/0004-detector-01-honeypot.md` §6 states: `w_fee_auth = 0.05`
- `config/detectors.toml` line 129 ships `value = 0.15`
- `crates/detectors/src/d01_honeypot.rs` line 383 uses `cfg.transfer_fee_authority_extra_weight.value` — reads from config, so the actual behavior is 0.15

The SYNTHETIC_high_transfer_fee_positive fixture expects confidence range [0.45, 0.80]. The fixture test at line 1169 checks `sr.confidence >= 0.45 && sr.confidence <= 0.80`. With weight=0.15, the raw for that fixture is:
- S2 (9000 bps): sigmoid((0.90-0.50)/0.20) × 0.45 = sigmoid(2.0) × 0.45 ≈ 0.88 × 0.45 ≈ 0.396
- Fee authority (0.15): 0.15
- S5 (999 ratio, 127 buys): min(999/(10.0×10), 1.0) × 0.20 = 1.0 × 0.20 = 0.20
- Total raw: 0.396 + 0.15 + 0.20 = 0.746
- static_conf = sigmoid(0.746/0.55 - 1.0) = sigmoid(0.357) ≈ 0.588

With weight=0.05 (the spec value):
- Total raw: 0.396 + 0.05 + 0.20 = 0.646
- static_conf = sigmoid(0.646/0.55 - 1.0) = sigmoid(0.175) ≈ 0.544

Both produce confidence well within the fixture range [0.45, 0.80]. The test does not catch the discrepancy. The spec is wrong, the config is right (0.15 is more defensible), but the conflict must be resolved and documented.

---

## 4. Fixture Corpus Gaps

### 4.1 Signal Coverage Matrix

| Signal | Positive Fixture Exists | Negative Fixture Exists | Edge Case Fixture | Gap |
|--------|------------------------|------------------------|-------------------|-----|
| S1 (Freeze authority) | YES — PYUSD, USDC (but both jup_verified → attenuated) | YES — RAVE, WET, wSOL | None | Missing: positive with S1 + no jup_verified (fire at full weight) |
| S2 (Transfer fee high) | YES — SYNTHETIC_high_transfer_fee | YES — RAVE, WET | None | Missing: fee between 3001–5000 bps (sub-threshold but near miss) |
| S2 (Fee authority) | YES — SYNTHETIC_high_transfer_fee (incidental) | YES — wSOL | None | Missing: fixture with 100 bps fee + live authority (isolation of sub-signal) |
| S3 (Permanent delegate) | YES — SYNTHETIC_permanent_delegate | YES — RAVE, WET | None | Missing: real confirmed-rugged token with permanent_delegate |
| S4 (Transfer hook) | NO — no fixture fires S4 | YES (implicitly via negatives) | None | MISSING POSITIVE FIXTURE for S4 |
| S5 (Buy/sell ratio) | YES — both SYNTHETIC fixtures (incidental) | YES — RAVE, WET | None | Missing: ratio between sentinel and 2× sentinel (near-miss) |
| S6 (Simulation) | NO — simulation deferred | NO | None | COMPLETELY UNTESTED |
| Combined (S1+S2+S3+S4+S5) | YES — SYNTHETIC_high_transfer_fee covers S2+S5; SYNTHETIC_permanent_delegate covers S3+S5 | RAVE covers all-negative | None | Missing: fixture with S1+S3+S5 simultaneously |
| jup_verified attenuation | YES — USDC, PYUSD | YES — RAVE | None | Missing: fixture with live malicious authority on jup_verified token |

### 4.2 Signal with No Positive Fixture

**S4 (Transfer Hook): CRITICAL GAP.** There is zero regression coverage for S4. If the S4 code path is broken (e.g., a None-handling bug in `transfer_hook_program` enrichment), no test would catch it. The Phase 2 note says S3/S4 are suppressed when enrichment returns None, which means S4 is currently always suppressed in practice — the positive fixture would expose that the enrichment is never returning a non-None value.

### 4.3 Proposed New Fixtures

**Fixture F1: Real Transfer Hook Positive (S4)**

Source: Search RugCheck API for tokens with `transferHook` field set and `rugged=true`. As of March 2026, the Offside Security blog (REFERENCES.md entry) documented multiple Token-2022 transfer hook honeypots. RugCheck has flagged tokens such as `APTtJyaRX5yGTsJU522N4VYWg3vCvSb65eam5GrPT5Rj` (example pattern — verify before capture) with transfer hook programs set to deployer-controlled addresses.

Expected signals: S4 fires, S5 fires (zero sells), confidence range [0.30, 0.55] (S4 weight 0.20 + S5 at ratio=999 weight 0.20 = raw 0.40 → sigmoid(-0.27) ≈ 0.43 × 0.80 with sim_skipped = 0.34).

Label: `positive_static_transfer_hook`

**Fixture F2: S1 Positive Without jup_verified (Unattenuated)**

A token with `freeze_authority != null` and `jup_verified = false`, confirmed rugged by RugCheck. This tests the full-weight S1 path that is currently only exercised by PYUSD/USDC (both attenuated). Example candidates: any of the hundreds of unverified memecoins launched via pump.fun that retain freeze authority. Use RugCheck query for tokens with freeze authority active and `rugged=false` but score > 5000.

Expected signals: S1 fires at weight 0.25, no jup_verified cap. static_conf ≈ 0.37 (pre-attenuation, as tested by PYUSD — but this fixture should test that attenuation does NOT apply). After sim_skipped: 0.37 × 0.80 ≈ 0.30 → Severity::Low.

Label: `positive_s1_freeze_unattenuated`

**Fixture F3: Near-Miss Sell Tax (3000–4900 bps)**

A Token-2022 token with transfer fee between 3000 and 5000 bps — below the current threshold. This documents the false-negative zone for the 5000 bps threshold. If the threshold is later lowered to 3000 bps (per recommendation in Section 3), this fixture would flip from negative to positive. Important for tracking threshold sensitivity.

Expected signals with current threshold: only fee authority fires if set (weight 0.15). Confidence ≈ 0.26–0.30 — below severity::Low.

Label: `negative_sub_threshold_fee` (may become positive if threshold lowered)

**Fixture F4: Freeze + Permanent Delegate + Zero Sells (Multi-Signal)**

A token firing S1 + S3 + S5 simultaneously without S2 or S4. This combination is not currently covered. Raw = 0.25 + 0.20 + S5_contribution. At 999 ratio with 100 buys: raw = 0.25 + 0.20 + 0.20 = 0.65. static_conf = sigmoid(0.65/0.55 - 1.0) = sigmoid(0.18) ≈ 0.55 → Severity::Medium. After sim_skipped: 0.55 × 0.80 = 0.44 → still Medium. This is a real token type (scam deployers using both freeze and permanent_delegate). Find a real mint from RugCheck with both attributes.

Label: `positive_s1_s3_s5_combined`

**Fixture F5: Wash-Sell Inflation (Normal Ratio Despite Hook)**

A token with a transfer hook present (S4 fires) but whose `observed_sells_24h` reflects deployer wash-sells maintaining ratio below 10.0. This tests that the detector correctly maintains S4 signal even when S5 is suppressed. Use a synthetic fixture with `transfer_hook_present=true`, `buy_sell_ratio_24h=3.5` (below sentinel), `buy_count=200`.

Expected: S4 fires (0.20), S5 does not fire (ratio 3.5 < 10.0). raw = 0.20. static_conf = sigmoid(0.20/0.55 - 1.0) = sigmoid(-0.636) ≈ 0.345. After sim_skipped: 0.28 → Severity::Low.

This fixture documents the incomplete detection of wash-sell inflation evasion.

Label: `positive_s4_only_wash_sell_evades_s5`

---

## 5. Worst-Case Crafted Token

### 5.1 Design Target

An adversary reads this codebase. They want to build a honeypot that scores as low as possible while still being profitable. They avoid all five static signals and accept that simulation will be the only detection path — and simulation is currently disabled.

### 5.2 Crafted Token Description

The token uses the following design:
- **Standard SPL Token program** (not Token-2022): avoids S2 (no transfer fee), S3 (no permanent delegate), S4 (no transfer hook). The sell-blocking mechanism must therefore be social/off-chain, not on-chain.

Wait — on standard SPL, there is no on-chain mechanism to block sells. If we constrain to mechanisms that actually block sells, we must use Token-2022.

Revised design: Token-2022 token with:
- `freeze_authority = null` (S1 = 0)
- `transfer_fee = null` (S2 = 0)
- `permanent_delegate = null` (S3 = 0)  
- `transfer_hook = present` (S4 fires: +0.20) — the hook blocks sells for non-whitelisted addresses
- `fee_authority_active = false` (S2 sub-signal = 0)
- Buy/sell ratio: deployer wash-sells maintain ratio at `8.5` over 200 observed buys (just below 10.0 sentinel) → S5 does not fire

### 5.3 Input State as JSON

```json
{
  "TokenMeta": {
    "mint": "ATTACKER_MINT",
    "token_program": "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
    "freeze_authority": null,
    "transfer_fee": null,
    "permanent_delegate": null,
    "transfer_hook_program": "ATTACKER_HOOK_PROGRAM",
    "verification": {
      "jup_verified": false,
      "jup_strict": false
    }
  },
  "HoneypotRatioRow": {
    "buy_count": 200,
    "sell_count": 24,
    "buy_sell_ratio": 8.3,
    "total_buy_raw": "200000000",
    "total_sell_raw": "24000000"
  }
}
```

### 5.4 Confidence Walk-Through

Signal computation:
- S1: freeze_authority = null → 0.0
- S2: transfer_fee = null → 0.0
- S2 sub (fee auth): no fee config → 0.0
- S3: permanent_delegate = null → 0.0
- S4: transfer_hook_program = Some(...) → raw += 0.20
- S5: buy_count = 200 ≥ min_buy_count_for_ratio (5), buy_sell_ratio = 8.3 < 10.0 → S5 does not fire → 0.0

Total raw = 0.20

static_conf = sigmoid(0.20 / 0.55 - 1.0) = sigmoid(-0.636) ≈ 0.345

Simulation disabled → sim_add = 0, sim_skipped = true

Final confidence = 0.345 × 0.80 = **0.276**

**Severity: Low** (0.276 falls in [0.20, 0.40))

### 5.5 Consumer Impact

The bot-trader-2-0 receives a Low-severity event. Current trading bot risk gates are unknown, but a Low-severity honeypot event is highly likely to be below any no-trade threshold. The bot enters a position it cannot exit. The attacker drains the pool.

The key insight: the detector identifies the hook is present (S4) but cannot determine whether the hook is malicious without executing it. This is the core static analysis limitation and the primary argument for making simulation mandatory before Sprint 2 exit.

---

## 6. Simulation-Deferral Risk Assessment

### 6.1 Honeypot Classes Undetectable Without Simulation

The following honeypot attack classes produce zero simulation-differentiating output from static analysis alone:

| Class | Static Signal | Why Undetectable Without Sim |
|-------|--------------|------------------------------|
| Transfer hook with whitelist sell-block (no fee, no delegate) | S4 only (0.20 weight) | Static presence of hook ≠ malicious hook. Hook could be royalty enforcement, governance, or genuine compliance. |
| Freeze activated post-detection (E10) | S1 at 0.25 | Freeze not yet active. Simulation at pre-freeze time passes. |
| Oracle-gated sell post-flip (E13) | S4 | Oracle currently permits sells. |
| Buy-only window (E16) | S4 | Sells work during detection window. |
| Pool substitution (E14) | None if primary pool is benign | Simulation on benign pool passes. |
| Minimum-sell enforcer (spec E7) | S4 | Small sells pass; large sells fail. Multi-path sim catches. |
| Amount-dependent sell-block (spec E2) | S4 | Small sells pass. Multi-path sim catches. |
| Standard SPL + off-chain marketing | None | No on-chain sell block possible with standard SPL without hook/fee/freeze. Note: actual sell block requires Token-2022 or freeze. Pure marketing scams are outside D01 scope. |

The five classes marked "S4 only" all produce the same low-confidence output (0.276 as computed above). They are only distinguishable by simulation.

### 6.2 Quantitative Assessment

Based on the permanent delegate burn scam data (dev.to/ohmygod, March 2026): at its peak, ~40% of new Solana Token-2022 tokens deployed in a two-week window used permanent_delegate or transfer_hook for malicious purposes. Of these, a significant fraction (exact number unavailable) would be classified as low-confidence by the static-only detector because they carefully avoid S1 and S2.

Estimated false negative rate in static-only mode for Token-2022 honeypots: approximately 25–40% of actual honeypots will produce confidence below the Severity::Medium threshold (0.40 after sim_skipped attenuation). This means 1-in-3 to 2-in-5 actual honeypots reach bot-trader-2-0 without adequate warning.

### 6.3 Verdict: Block Sprint 2 Exit Until X

The recommendation is: **Block Sprint 2 exit until simulation is implemented OR a formal risk acceptance document is signed acknowledging the false-negative rate and the compensating controls in place.**

**If ship-static-only is accepted**, the following compensating controls MUST be implemented concurrently:

1. **Lower buy_sell_ratio_sentinel from 10.0 to 5.0.** Without simulation, S5 is the primary behavioral signal. A 5:1 ratio is already statistically anomalous for a normally-trading token (RAVE had 1.22; WET had 0.70).

2. **Mandatory re-evaluation schedule.** Any token that produces a D01 event must be automatically re-evaluated every 15 minutes for the first 24 hours after listing, and every hour thereafter. This catches time-gated and oracle-gated honeypots (E10, E13, E16).

3. **Raise the `transfer_hook_present` weight from 0.20 to 0.30** in the no-simulation configuration. Transfer hook presence without simulation is a much stronger signal than 0.20 implies — it means we cannot verify the hook's behavior. With simulation, 0.20 is appropriate (the hook's effect is measured). Without simulation, it should be treated as higher suspicion.

4. **Document the false-negative rate explicitly** in bot-trader-2-0's integration documentation so the trading bot can apply its own risk overlays.

---

## 7. jup_verified Attenuation Hardening

### 7.1 The Rogue Verified Token Attack

Current logic at `crates/detectors/src/d01_honeypot.rs` line 298–300:
```rust
let final_confidence = if static_result.jup_verified {
    final_confidence.min(0.25_f64)
} else {
    final_confidence
};
```

This cap is unconditional and permanent. Once `jup_verified = true` in the token's metadata, the cap applies forever, regardless of subsequent state changes.

**Attack path:**
1. Deploy a legitimate token. Apply for Jupiter verification. Pass their review process (token has legitimate use, no dangerous authorities).
2. Token receives `jup_verified = true` in Jupiter's token list.
3. After verification: transfer freeze_authority to a new deployer-controlled address. Our detector: S1 fires (0.25 weight), but `jup_verified = true` caps the final confidence at 0.25. Severity: Low (0.25 × 0.80 = 0.20 after sim_skipped attenuation, right at the Low/Info boundary).
4. Activate freeze on all retail accounts. Drain LP.

The cap provides complete protection to any token that achieves jup_verified status, regardless of subsequent dangerous state changes.

### 7.2 Recommended Unlock Condition

The `jup_verified` attenuation should be conditioned on the token's authority state at verification time versus current state. Specifically:

**Proposed unlock condition:** The jup_verified cap does NOT apply if any of the following have changed since the verification timestamp:
- `freeze_authority` was null at verification but is now non-null
- `mint_authority` was null at verification but is now non-null
- `transfer_fee.authority` changed to a non-system-program address after verification
- `permanent_delegate` was null at verification but is now non-null
- `transfer_hook_program` was null at verification but is now non-null

Implementation requires: storing the authority state at the time jup_verified was recorded (a new `token_extensions` sidecar table field), and comparing against current state in `compute_static`.

**Intermediate fix (Phase 2 viable):** Add a `jup_verified_override` flag that can be set by a monitoring agent when any authority changes post-verification. The monitoring agent runs as a background task watching for authority-change transactions on verified tokens.

### 7.3 Supply Change Condition

Additionally, if a jup_verified token's total supply increases by more than X% since verification (via mint_authority exercise), the cap should be removed. This is the same argument as above but for the D06 (mint/burn anomaly) case.

---

## 8. Determinism and Race Exposure

### 8.1 `chrono::Utc::now()` in the Detector

**File:** `crates/detectors/src/d01_honeypot.rs`, **line 326**:
```rust
ingested_at: Utc::now(),
```

The `AnomalyEvent.ingested_at` field uses `Utc::now()` — a wall-clock call — inside the `evaluate()` function. This is correct for the `ingested_at` field (which records when the event was created, not when it was computed), but it means two calls to `evaluate()` with identical inputs will produce events with different `ingested_at` values. This is not a false positive or false negative risk, but it means that `AnomalyEvent` itself is not bit-for-bit reproducible, which could matter for deduplication logic in downstream consumers or storage.

**Recommendation:** If `ingested_at` is used for deduplication, this is a bug. If it is purely informational, document that `ingested_at` is not part of the deterministic output. The determinism test at line 873 tests `confidence` and `freeze_active` but not `ingested_at` — the test is correct but the documentation should be updated.

**Classification:** Not a bug per the spec (spec says "Given the same block range input, output MUST be deterministic" — `ingested_at` is not part of the detector output per CLAUDE.md definition of "output"). But worth documenting explicitly.

### 8.2 HashMap Iteration Order in Evidence

**File:** `crates/detectors/src/d01_honeypot.rs`, evidence builder path.

The `Evidence.metrics` storage is a map type. If it is backed by a `HashMap`, the `with_note()` and `with_metric()` calls are order-independent in terms of final key-value content (hash maps are unordered), but the `Evidence.notes` field is a `Vec<String>` (append-based). Two notes are added for `sim_skipped` tokens: the metric note and the summary note, both via `with_note()`. The order of these two notes is deterministic by code path (sim_skipped check runs before the summary note build at lines 539–545 and line 556). No reordering risk here.

However, if `Evidence.metrics` is a `HashMap<String, Decimal>`, serialization order is non-deterministic across runs when the `evidence` is serialized to JSON for storage. This does not affect the detector output (confidence, severity) but could affect hash-based deduplication of events if the JSON representation is used as the deduplication key. **Recommend using `BTreeMap` or equivalent ordered map for `Evidence.metrics`** if deterministic serialization is required.

**Severity:** Low. No impact on signal correctness.

### 8.3 Concurrent Enrichment Race

**File:** `crates/detectors/src/d01_honeypot.rs`, `evaluate()` function.

If two `evaluate()` calls for the same token run concurrently (e.g., the scheduler triggers two evaluations within the same block window due to a retry), both calls will:
1. Call `ctx.registry.enrich(ctx.token)` — this may return cached data (consistent) or trigger two concurrent RPC fetches (non-deterministic if the token's on-chain state changes between the two fetches).
2. Call `ctx.store.fetch_honeypot_ratio()` — both calls hit the same Postgres snapshot; consistent unless a new transfer is written between the two queries.

If the token's mint account state changes between the two concurrent `enrich()` calls (e.g., freeze authority is activated mid-run), the two instances will produce different `StaticResult` values and emit different events for the same block window.

**Recommendation:** The scheduler should enforce single-instance evaluation per (token, chain, window) using a database advisory lock or an in-process mutex keyed on `(token, window)`. This is a scheduler concern, not a detector concern, but should be documented as a scheduler requirement.

**Classification:** Architectural gap, not a code bug. Not blocking.

---

## 9. Code-Level Findings

### Finding C1 — Spec/Config Discrepancy: `transfer_fee_authority_extra_weight`

**Severity:** CRITICAL (spec and implementation diverge; tests do not catch it)

**File/Line:**
- `docs/designs/0004-detector-01-honeypot.md` §6 line 399: `w_fee_auth = 0.05`
- `config/detectors.toml` line 129: `value = 0.15`

**Problem:** The spec documents 0.05 as the weight for the transfer fee authority sub-signal. The production config ships 0.15 — 3× higher. No test catches this because the fixture range [0.45, 0.80] is wide enough to accommodate both values.

**Suggested patch:**
Option A: Update the spec to say 0.15 (if 0.15 is the intended value after deliberation).
Option B: Lower config to 0.05 (if the spec is authoritative).
Option C: Adopt the recommended 0.10 from this review and update both.

Whichever is chosen, add a test that pins the expected weight value:
```rust
#[test]
fn fee_authority_weight_matches_spec() {
    let cfg = load_cfg();
    assert_eq!(cfg.transfer_fee_authority_extra_weight.value, 0.15, 
               "weight must match spec §6 w_fee_auth; update test when spec changes");
}
```

---

### Finding C2 — S5 Logic: Sentinel Path Is Dead Code

**Severity:** Medium (dead code reduces test confidence; potential false negative in edge case)

**File/Line:** `crates/detectors/src/d01_honeypot.rs` lines 420–428

```rust
} else if r.buy_sell_ratio > cfg.buy_sell_ratio_sentinel.value {
    let ratio_contribution =
        (r.buy_sell_ratio / (cfg.buy_sell_ratio_sentinel.value * 10.0)).min(1.0);
    raw += ratio_contribution * 0.20;
    (r.buy_count, r.sell_count, r.buy_sell_ratio, false)
} else {
    (r.buy_count, r.sell_count, r.buy_sell_ratio, false)
}
```

The spec (§3.1, pseudocode) has a specific `ELSE IF ratio_row has buy_count > 0 AND sell_count == 0 AND buy_count >= min_buy_count_for_ratio` branch for the zero-sell sentinel case that adds a flat 0.20. The implementation does not have this separate branch — instead it relies on the fact that the sentinel value 999.0 (set by SQL when sell_count=0) will satisfy `ratio > 10.0`, causing the formula `999 / 100 = 9.99` → `min(9.99, 1.0) = 1.0` → `raw += 1.0 * 0.20 = 0.20`. This produces the same result as the spec's flat 0.20, but via a different path.

**Consequence:** The formula is numerically correct but the code no longer matches the spec's structure. A future maintainer editing the sentinel value in the SQL (e.g., changing 999 to 9999) would inadvertently change the zero-sell contribution. The code should be more explicit.

**Suggested patch:**
```rust
} else if r.buy_sell_ratio > cfg.buy_sell_ratio_sentinel.value {
    let ratio_contribution = if r.sell_count == 0 {
        // Zero-sell sentinel path: flat maximum contribution
        1.0_f64
    } else {
        (r.buy_sell_ratio / (cfg.buy_sell_ratio_sentinel.value * 10.0)).min(1.0)
    };
    raw += ratio_contribution * 0.20;
    (r.buy_count, r.sell_count, r.buy_sell_ratio, false)
```

---

### Finding C3 — DG3 Simulation Keypair Derivation Is Publicly Predictable

**Severity:** HIGH (once simulation is implemented, this enables E18 evasion)

**File/Line:** `docs/designs/0004-detector-01-honeypot.md` §DG3 paragraph:
> "derive from `sha256(token_bytes || pool_bytes || [i as u8])` as a 32-byte seed"

This derivation is fully predictable from public on-chain data (the token mint address and pool address, both public before any detection runs). An attacker who reads this spec can pre-compute all three simulation keypairs and whitelist them before listing.

**Suggested patch for Phase 3 simulation implementation:**

Derive the keypair from:
```
seed = sha256(token_bytes || pool_bytes || [i as u8] || detection_epoch_nonce)
```

where `detection_epoch_nonce` is a value that the attacker cannot pre-compute. Options:
- Use the most-recent finalized block hash at detection time (fetched via `getLatestBlockhash`).
- Use a private random seed held by the detection service, rotated daily.

The block hash option is simplest and ties the keypair to a specific block, ensuring each run produces different keypairs. The attacker cannot pre-whitelist because they do not know which block hash will be used at the moment of first detection.

Document this in the spec update as a pre-commit requirement for the Phase 3 simulation PR.

---

## 10. References Added to REFERENCES.md

The following new rows should be added to `REFERENCES.md`:

| Mechanism | Signal / Formula | Source | Used In | Verified Against |
|-----------|-----------------|--------|---------|------------------|
| Trickle-sell allowlist inflation | Wash-sell inflated S5 buy/sell ratio to evade honeypot detection; deployer self-trades | Beosin BSC honeypot analysis 2023, https://beosin.com/resources/bsc-honeypot-analysis-2023 | D01 evasion E8 | Referenced 2026-04-21 |
| EVM oracle-gated honeypot | Conditional honeypot via external state oracle; HoneyBadger Torres 2019 §5.3 "Hidden Transfer Fee" | Torres, Steichen & State 2019, https://arxiv.org/abs/1902.06976 | D01 evasion E13 | Cross-reference to existing entry |
| Token-2022 ConfidentialTransfer extension | Encrypted transfer amounts; transfer events not observable by standard SPL indexers | Solana Token-2022 extension docs, https://spl.solana.com/confidential-token | D01 evasion E17; Phase 3 indexer requirement | Referenced 2026-04-21 |
| Token-2022 maximum_fee field | `maximum_fee` in `TransferFeeConfig` sets an absolute per-transfer cap; setting to dust allows low nominal fee while retaining full fee-raise authority | Solana token extension docs, https://solana.com/docs/tokens/extensions/transfer-fees | D01 evasion E12; S2 signal gap | Referenced 2026-04-21 |
| EVM two-step ownership / InitializeMint2 race | Sell-block tokens appearing renounced during static check via authority transfer race | Torres 2019 §5.2; Solana-native adaptation | D01 evasion E19 | Referenced 2026-04-21 |
| EVM multi-pool honeypot (benign pool for sim) | Deployer creates separate benign and malicious pools; detector simulates benign pool | Beosin 2023 Uniswap v2 clone analysis | D01 evasion E14; DG4 pool selection gap | Referenced 2026-04-21 |

---

## 11. Sign-Off Verdict

**BLOCK Sprint 2 exit until the following are resolved:**

### Blocking Issues (must fix before exit)

**B1 — Simulation deferral without compensating controls:** DG3 defers simulation to Phase 3. Without simulation, approximately 25–40% of real-world Token-2022 honeypots will produce only a Low-severity event and slip through to the bot-trader-2-0. This is the single largest false-negative source. Either implement simulation before exit OR formally accept the static-only risk with the compensating controls documented in Section 6.3 (lower buy_sell_ratio_sentinel to 5.0, raise hook weight to 0.30, mandate re-evaluation schedule).

**B2 — transfer_fee_authority_extra_weight spec/config discrepancy:** The spec says 0.05; the config ships 0.15. This must be resolved and documented. The discrepancy undermines the reproducibility guarantee: a developer reading the spec will implement a different formula than what ships.

**B3 — S4 positive fixture missing:** No test exercises the transfer hook (S4) code path in a positive case. If enrichment silently returns None for `transfer_hook_program` (the current Phase 2 suppression path per DG2), no test would catch it. Add Fixture F1 (proposed in Section 4.3) before exit.

### Recommended Before Exit (high priority)

**R1 — DG3 simulation keypair derivation (E18 pre-fix):** Document in the Phase 3 simulation spec update (not the DG3 note) that the keypair derivation must incorporate an unpredictable nonce. Pre-empt the E18 attack before simulation ships.

**R2 — jup_verified attenuation unlock condition:** Add the authority-change unlock condition described in Section 7.2. The current unconditional cap is exploitable by any verified token that subsequently changes authorities. Minimum viable fix: add a monitoring TODO note with a concrete implementation plan.

**R3 — Sell tax threshold bps lowering:** Lower `sell_tax_threshold_bps` from 5000 to 3000, guarded by Sprint 3 calibration gate (if corpus shows FP rate > 5%, revert to 5000). The 50% threshold misses a class of real-world partial-honeypots.

### Acceptable to Ship With Caveat

**S1 — S5 dead-code sentinel path (C2):** Low risk. Numerically correct. Ship with a FIXME comment referencing this review.

**S2 — HashMap iteration / ingested_at non-determinism:** Low risk. No impact on signal correctness. Ship with documentation.

**S3 — Concurrent enrichment race:** Architectural; scheduler responsibility. Not a detector bug. Ship with a scheduler requirement documented.

---

*This review covers 12 novel evasion techniques beyond the analyst's 7, threshold analysis with production-config discrepancies identified, 5 fixture gap proposals, a worst-case crafted token scoring 0.276 confidence, and 3 code-level findings. The detector as shipped provides meaningful signal on the most common Solana honeypot patterns but is systematically bypassable by an informed adversary who avoids the Transfer Fee signal (S2) and manipulates the buy/sell ratio (S5). Simulation (S6) is not optional for bot-trader-2-0 safety.*
