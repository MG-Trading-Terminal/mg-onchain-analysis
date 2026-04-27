# Review 0002 — D02 Rug Pull / LP Drain: Adversarial Evasion Analysis

**Date:** 2026-04-21
**Reviewer:** security-researcher agent, mg-onchain-analysis
**Detector:** D02 `rug_pull_lp_drain`
**Spec ref:** `docs/designs/0005-detector-02-rug-pull.md`
**Implementation ref:** `crates/detectors/src/d02_rug_pull.rs`
**SQL ref:** `docs/queries/d02_rug_pull_lp_drain.sql`
**Config ref:** `config/detectors.toml` `[rug_pull_lp_drain.*]`
**Prior review:** `docs/reviews/0001-d01-honeypot-evasions.md`
**Status:** Draft

---

## 1. Executive Summary

- Signal A (event-based drain) is sound for single-actor single-pool drains but has four
  exploitable gaps: multi-actor splitting, 24-hour trickle below the 60-minute window,
  cross-chain bridge drain, and Token-2022 `withdraw_withheld` as a non-LP drain path that
  produces zero `pool_events` Burn rows and bypasses Signal A entirely.

- Signal B (latent risk) is the more dangerous gap: a sophisticated attacker can construct
  a rug that never triggers Signal B by staging LP from a freshly funded non-deployer wallet
  with a barely-above-floor `effective_safe_pct` until the night of the drain, then rotating
  to a single-provider structure in one block while simultaneously executing the drain. The
  detector evaluates state and events from the same window; the state check has no hysteresis.

- The `ingested_at: Utc::now()` call inside `make_event()` introduces a wall-clock dependency
  that makes two back-to-back evaluations of the same input produce `AnomalyEvent` structs
  that differ in `ingested_at`. This is not a correctness bug for confidence math but violates
  the CLAUDE.md reproducibility requirement ("given the same block range input, output MUST be
  deterministic") if downstream consumers hash or deduplicate on the full event struct.

- The dead-pool skip (DG-D02-4) has an exploitable narrow band: an attacker who drains all
  but $1,001 of liquidity straddles both branches — Signal B does not fire (effective_safe_pct
  moves to 100% if pool marks LP burned, per PumpSwap semantics), and the pool is not
  considered dead because `liquidity_usd >= min_pool_usd`. Signal A fires correctly only if
  the drain query sees a Burn row. If the drain happens via a mechanism that does not produce a
  `pool_events` Burn row (oracle-gated admin withdrawal, Token-2022 `withdraw_withheld`), both
  branches miss.

- The per-pool Signal A suppression of Signal B (DG-D02-5) is exploitable: an attacker who
  controls two pools can arrange for a small, below-threshold Burn on pool A to never produce
  Signal A (no suppression triggered), while the real drain happens on pool B which lacks a
  Signal B precursor because it was seeded with above-floor LP burn percentage at launch.

- Sign-off: **Block Sprint 3 exit until findings C1, C2, and C3 are resolved or documented
  as accepted risk with compensating controls. Findings C4 and C5 may ship as known gaps
  with REFERENCES.md entries.**

---

## 2. Evasion Catalog

The spec (0005-detector-02-rug-pull.md §9) documented six evasion patterns (E-D02-1 through
E-D02-6). This review identifies ten additional techniques. Each entry maps which signal it
defeats, the attacker's cost, preserved reward, detection cost, and precedent.

---

### E-D02-7 — 24-Hour Trickle Drain (window escape)

**Description:** The deployer executes LP removals over 25–48 hours, each burn removing
4–8% of LP supply. No single burn or 60-minute cumulative window crosses `lp_removal_threshold
= 0.65`. At the end of 48 hours, 100% of LP has been removed across ~15 transactions.

**Signals defeated:**
- Signal A: the `drain_window_minutes = 60` rolling window accumulates per-actor burns within
  one hour. A burn of 6% every 5 hours accumulates to only 6% per window, never crossing 65%.
  The existing `E-D02-1` trickle mitigation (cumulative window sum) catches instalments within
  60 minutes but NOT instalments spaced more than 60 minutes apart.
- Signal B: fires throughout the trickle period (0% burned, latent risk), which is the correct
  leading indicator. However, if the bot-trader position closes before the 48-hour drain
  completes, Signal B alone may not trigger a position exit — it fires at 0.75/High, which
  some consumers may have set a threshold above.

**Distinguishing from E-D02-1:** E-D02-1 is caught within the 60-minute window by the
cumulative sum. E-D02-7 exploits the gap between the 60-minute window and 24h+ horizons.

**Cost:** Low. Only gas fees per burn transaction (~0.000005 SOL per tx on Solana). 15
transactions costs ~0.000075 SOL — negligible.

**Reward preserved:** Full. The attacker recovers all pool liquidity across 48 hours while
each individual transaction is invisible to Signal A.

**Detection cost (additional signal required):** A 24-hour cumulative drain query: sum of
all burn events by actor across a 24-hour rolling window. If cumulative_removed_pct_24h >=
lp_removal_threshold, fire Signal A with moderate confidence (0.75). Add a new config key
`drain_window_24h_minutes = 1440`. This is a Phase 2 extension to the SQL query — add a
second CTE with a 24-hour window partitioned identically. Cost is one additional SQL pass.

**Precedent:** The LROO (Shoaei et al. 2026) paper describes tokens reaching zero liquidity
within "1-3 days" — this implies multi-day trickle drain was observed in their corpus. The
SolRPDS (Alhaidari et al. 2025) trickle-drain analysis confirms most drains complete in
minutes but notes a "slow drain" sub-category spanning 24-72 hours.

---

### E-D02-8 — Multi-Actor Cluster Drain (social splitting)

**Description:** The deployer pre-distributes LP tokens across 5–10 colluding wallets (each
wallet receives ~10–15% of LP supply) before launch. All wallets are funded from the same
source but have no direct on-chain connection to the deployer visible without graph analysis.
On rug day, each wallet burns its 10–15% position within 60 minutes. No single actor removes
>=65%. Cumulative cluster removal is 100%.

**Signals defeated:**
- Signal A: the SQL query partitions by `(chain, pool, actor)`. Each actor accumulates to
  only 10–15%. No single actor crosses `lp_removal_threshold = 0.65`.
- Signal B: before the drain, each wallet holds 10–15% of LP supply. `lp_provider_count` is
  5–10, which means `single_provider_bonus = 0` (no bonus). Signal B fires at base confidence
  ~0.75 (0% burned, no lockers, no bonus) — Medium/High. This is the correct leading indicator
  but the lower confidence may not block a position entry.

**E-D02-1 partial mitigation gap:** The spec notes this as the "full evasion path" for
E-D02-1. It is correct. Phase 3 wallet-cluster aggregation is required.

**Cost:** Medium. Requires managing 5–10 funded wallets with LP positions. On-chain cost:
~0.1–0.5 SOL in rent and fees. Time cost: pre-distribution must happen before launch.

**Reward preserved:** Full.

**Detection cost:** Phase 3 graph clustering. In Phase 2, a partial mitigation: if N>=3
distinct actors each burn >10% of LP supply within 60 minutes, fire Signal A at reduced
confidence (0.75) with evidence noting "multi-actor cluster burn." This requires an
aggregated query variant: `GROUP BY pool, window` rather than `GROUP BY pool, actor, window`.
Add config key `cluster_burn_min_actors = 3` and `cluster_burn_min_pct_each = 0.10`.

**Precedent:** ZachXBT (2024) documented multi-wallet LP pre-distribution in the BALD BSC
rug ($26M). SolRPDS (Alhaidari et al. 2025) identifies "multi-actor coordination" as a
category in their suspicious pool classification.

---

### E-D02-9 — Pre-Drain LP Transfer to Non-Deployer (actor rotation evasion upgrade)

**Description:** This extends E-D02-3 (admin-key rotation) with a specific timing attack.
Seven days before the drain, the deployer transfers LP position to a wallet that has been
active on-chain for 90 days and has interacted with 20+ legitimate protocols (purchased from
a "history wash" service or built organically). This wallet has no connecting transaction to
the deployer wallet visible without subgraph tracing. Signal B fires throughout the pre-drain
period with the new wallet as LP provider. When the drain occurs, the actor in Signal A
evidence is the "aged" wallet, not the deployer — human review sees a credible-looking actor
rather than a fresh disposable wallet.

**Signals defeated:**
- Signal A fires correctly (drain is above threshold). The attacker does not defeat Signal A.
- The attack goal is defeating human review and auto-attribution. Without graph clustering,
  the actor appears to be a normal LP provider exiting. The bot-trader stops the position
  correctly, but the deployer escapes attribution.

**Why it matters for D02 specifically:** The spec records `worst.actor` in the evidence
bundle. If consumers filter alerts by "deployer-affiliated actor only," this evasion converts
a Critical (deployer drain) to an unattributed drain that may receive a lower response
priority. More importantly, if Signal B is tuned to fire only when `actor == deployer`, the
aged wallet never triggers that variant.

**Cost:** Medium-High. Either requires buying an aged wallet (gray market) or building it
organically over 90 days.

**Reward preserved:** Full. And the deployer has deniability.

**Detection cost:** Phase 3 graph crate with funding-source tracing. In Phase 2: lower the
bar for flagging any LP drain regardless of actor attribution. The current implementation
correctly does this (actor identity does not affect confidence). Ensure no future filter is
added that conditions Signal A on actor == deployer.

**Precedent:** The EVM equivalent (RAFT protocol hack, October 2023) used a wallet with
14-day on-chain history as the attacker address. RektNews documented the history-washing
pattern in the Platypus Finance post-mortem (2023).

---

### E-D02-10 — Fake Locker Program with Matching Discriminator

**Description:** This extends E-D02-4 (fake locker) with a specific bypass of the
known-locker whitelist. The attacker deploys a program whose account `discriminator` matches
one of the known-safe lockers (e.g., Raydium Locker, Fluxbeam). The program's outer
discriminator check passes whitelist matching. The inner implementation stores LP tokens in
an attacker-controlled PDA with an admin instruction (`withdraw_all`) that can be called at
any time. The token-registry whitelist checks program ID, not upgrade authority.

**Signals defeated:**
- Signal B: `effective_safe_pct` includes the "locked" amount from the fake locker,
  potentially reaching or exceeding `lp_safe_floor_pct = 70.0`. Signal B does not fire.

**Spec's own gap (spec §9 E-D02-4 last paragraph):** The spec explicitly acknowledges this:
"Static program ID matching is not sufficient; the locker program's upgrade authority should
also be checked." This review elevates it from a known residual gap to a CRITICAL finding
because the whitelist approach is a Phase 2 mitigation; the upgrade-authority check is
deferred to Phase 3. During the Phase 2-to-Phase 3 gap, any deployment that clones a
known locker discriminator defeats the primary mitigation.

**Cost:** High. Requires writing a convincing clone of a known locker program in eBPF, which
requires Solana BPF development skill.

**Reward preserved:** Full. And the attacker has additional time to accumulate before Signal
B fires (it doesn't, until the drain).

**Detection cost (Phase 2 compensating control):** When ingesting a locker into token-registry,
also fetch the locker program account's `upgrade_authority`. If the upgrade authority is the
token deployer, set `locked_amount_raw = 0` for that locker regardless of whitelist status.
This costs one additional RPC call per locker at ingestion time.

**Precedent:** Sun et al. 2024 explicitly categorizes "Fake LP Lock" (taxonomy category 19
of 34). The EVM analogue is documented in the Rekt News MISO hack post-mortem (2021) where
a "vesting contract" had an admin withdrawal backdoor.

---

### E-D02-11 — Token-2022 Withdraw-Withheld as Non-LP Drain

**Description:** Token-2022 `TransferFeeConfig` extension has a `withdraw_withheld_authority`
that can call `withdraw_withheld_tokens_from_accounts` or `withdraw_withheld_tokens_from_mint`
to extract accumulated transfer fees. If the token's transfer fee is set to a high basis
points value (e.g., 1000 bps = 10%), every swap through the pool accumulates a fee in the
withheld account. Over time, the withheld balance represents a substantial fraction of
circulating supply. The authority calls `withdraw_withheld` to drain this value. This
transaction does NOT produce a `pool_events` Burn row — it is an SPL Token-2022 instruction,
not a pool Burn instruction.

**Signals defeated:**
- Signal A: requires a `pool_events` row with `event_kind = 'burn'`. A
  `withdraw_withheld_tokens_from_accounts` instruction produces a Transfer to the authority
  wallet, not a Burn. Signal A sees zero qualifying rows.
- Signal B: the LP itself is not being burned. `lp_burned_pct` remains unchanged.
  `effective_safe_pct` is unaffected. Signal B does not fire unless LP was already below
  the safe floor for other reasons.

**Scope:** This is a value-extraction attack rather than a classic LP drain. The pool is
not drained of its SOL/USDC reserves, but the token holders' circulating supply is
systematically taxed and redirected to the authority wallet. For the bot-trader, this is
equivalent to a hidden sell tax that the bot does not simulate.

**Detection cost:** D01 (honeypot simulator) should catch this via the transfer fee authority
signal (S2) if fee basis points are above `sell_tax_threshold_bps = 3000`. However, D02 has
no coverage. A cross-detector linkage is needed: if D01 fires on high transfer fee authority
AND the authority has recently called `withdraw_withheld`, escalate to D02 confidence. This
is a Phase 3 cross-detector composition.

**Cost:** Medium. Requires launching with Token-2022, which is publicly visible. The fee must
be high enough to accumulate meaningful value, which may trigger D01. If deployed with
`maximum_fee` dust cap (E12 from D01 review), the fee appears harmless until raised — see
E12 from `docs/reviews/0001-d01-honeypot-evasions.md`.

**Precedent:** No confirmed Solana Token-2022 `withdraw_withheld` rug documented as of
2026-04-21. The EVM analogue is the fee-sink honeypot documented in Torres et al. 2019 §5.3
("hidden fee" category). Sun et al. 2024 taxonomy category "Hidden Fee" (category 7 of 34).

---

### E-D02-12 — Cross-Chain Bridge Drain (LP metric staleness)

**Description:** The attacker adds liquidity to the Solana pool from a wallet funded via a
cross-chain bridge (e.g., Wormhole, deBridge). On rug day, the attacker sends the LP
position back through the bridge to an EVM chain, then removes liquidity from the bridged
position on the EVM side. The Solana pool's `pool_events` table sees no Burn event. The
token-registry's `lp_burned_pct` remains at 0%. The pool's on-chain state eventually
reflects reduced liquidity, but only after the bridge settlement delay (minutes to hours).

**Signals defeated:**
- Signal A: no Burn row in Solana `pool_events`. The indexer cannot observe EVM-side events.
- Signal B: `lp_burned_pct` and `lp_provider_count` are stale. Until the Solana pool's
  reserve update propagates to the indexer, the pool appears intact.

**Scope:** The bridge must support LP token bridging, which is not universally available.
Raydium AMM v4 LP tokens are plain SPL tokens — they can be bridged via Wormhole. PumpSwap
LP is non-transferable by design. This evasion applies to Raydium-based pools.

**Cost:** High. Bridge fees + cross-chain coordination + risk of bridge failure during drain.
Only viable for large-value pools where bridge cost is negligible relative to proceeds.

**Detection cost:** Monitor for LP token transfers to known bridge program addresses (Wormhole
Token Bridge: `wormDTUJ6AWPNvk59vGQbDvGJmqbDTdgWgAqcLBCgUb`, deBridge:
`dBridgeTokenMint...`). Flag any LP token transfer to a bridge as a potential drain event.
This is a Phase 3 signal.

**Precedent:** No confirmed Solana LP bridge drain documented as of 2026-04-21. The pattern
is theoretically sound. Chainalysis 2025 notes cross-chain flows as a money-laundering
vector after rug events, not as the drain mechanism itself.

---

### E-D02-13 — Oracle-Gated Drain (programmatic trigger)

**Description:** The deployer deploys a custom AMM or a custom LP management contract. The
contract holds LP tokens on behalf of the "pool." An admin instruction on the custom contract
is gated behind an oracle price: "if price of TOKEN > X USDC, release all LP to admin." The
deployer pumps the price above X (coordinating with E-D02-8 multi-wallet buys), then calls
the release instruction. The Burn event fires from the custom contract — not from the
deployer wallet directly. The custom contract is the actor in Signal A evidence.

**Signals defeated:**
- Signal B: `effective_safe_pct` appears inflated because LP tokens are held by the custom
  contract. If the custom contract is not in the known-locker whitelist, token-registry
  correctly treats locked_pct = 0. Signal B fires correctly. If the custom contract mimics
  a known locker (E-D02-10), Signal B is suppressed.
- Signal A: fires correctly when the drain happens. The actor is the custom contract's PDA,
  not the deployer. Without Phase 3 attribution, the connection to the deployer is invisible.

**Cost:** High. Requires custom program development and oracle integration.

**Reward preserved:** Full.

**Detection cost:** Phase 3 graph attribution. In Phase 2: the custom contract's upgrade
authority should be checked — if it is the token deployer, flag the LP position as
deployer-controlled regardless of whitelist status (same compensating control as E-D02-10).

**Precedent:** EVM oracle-gated honeypot is documented in Torres et al. 2019 §5.3 and in
the D01 review (E13). The LP-drain variant is a direct extension of the oracle-flip pattern
from sell-blocking to liquidity withdrawal.

---

### E-D02-14 — Wash-Deploy to Inflate Prior TX Count

**Description:** Before the drain, the deployer (or colluding wallets) executes wash trades
on the pool — self-buy-and-sell cycles with small amounts. This inflates `lifetime_tx_count`
past `min_prior_txs = 100` within minutes of pool creation. Once the tx count threshold is
cleared, Signal A becomes eligible. But the attacker's goal here is the inverse: they want
the pool to look "mature" so that a very fast drain (in the first real hour after retail
accumulates) appears credible as a routine large-LP exit, not a rushed rug.

**Signals defeated:**
- Signal A guard: `prior_tx_count >= min_prior_txs`. This guard is satisfied via wash trades.
  The guard was designed to exclude freshly-created pools with negligible activity. A fresh
  pool with 100 self-trades passes the guard.
- Signal B: wash trades also inflate apparent pool activity, which may give consumers
  false confidence that the pool is active and legitimate.

**Note:** Signal A fires when the actual drain happens regardless. The evasion targets
consumer perception and human review, not the signal formula itself. A pool that drains 3
minutes after launch with 100 prior self-trades in those 3 minutes looks different from a
pool with 10,000 genuine trades.

**Cost:** Very low. 100 self-trades on Solana costs ~0.0005 SOL in fees. The wash trades
themselves can be sub-$1 amounts; pool depth does not matter.

**Reward preserved:** Full.

**Detection cost:** Add a `pool_age_hours` field to the drain evidence bundle. If
`lifetime_tx_count >= min_prior_txs AND pool_age_hours < 1`, flag in notes as "pool age
anomaly." Also compute the ratio `lifetime_tx_count / pool_age_hours` — a pool with 200 txs
in 0.1 hours has 2000 tx/hour velocity, which is detectable as wash. This is a Phase 2
evidence enrichment, not a separate signal. The D05 (Wash Trading H1) detector provides the
formal coverage.

**Precedent:** Chainalysis 2025 notes deployers inflate baseline metrics before executing
the rug. SolRPDS (Alhaidari et al. 2025) specifically calls out "inactivity state" after an
initial burst as a precursor pattern, implying an initial burst-to-inactivity cycle.

---

### E-D02-15 — Compounded Lock Expiry + Same-Block Drain

**Description:** The deployer creates a genuine (whitelist-approved) LP locker with
`unlock_at = now + 31 days`. Signal B does not fire because `effective_safe_pct = 100%`
(all LP is in a recognized locker). This is the E-D02-6 pattern from the spec. The extension:
on day 30, the lock is within `minimum_lock_horizon_days = 30` of expiry. Signal B NOW fires
(the locker is no longer counted as "active"). The deployer monitors this transition and
executes the drain within the same block that the locker's expiry crosses the horizon. The
alert fires at the same time as the drain — zero warning window.

**Signals defeated:**
- Signal B: fires correctly when `unlock_at <= now + 30d`, but the drain happens
  simultaneously. The latent-risk alert and the drain event arrive in the same evaluation
  cycle. The bot-trader receives Signal B and Signal A at the same time — there was never a
  "warning before drain" period.

**Why this is worse than E-D02-6:** The spec's own analysis of E-D02-6 says "Signal A fires
when the drain eventually occurs." This is correct. But it accepts a 35-day window where the
token appears safe. The compounded variant accepts a 30-day safe window, then the bot-trader
gets zero advance warning because the drain is executed the moment Signal B transitions from
"not firing" to "firing."

**Cost:** Low. Requires the deployer to monitor the `unlock_at` on-chain and submit a drain
transaction at the precise block when the lock expires. Solana bots can do this trivially.

**Reward preserved:** Full.

**Detection cost:** The spec proposes a "lock expiring soon" variant of Signal B when
`unlock_at < now + 2 * minimum_lock_horizon_days`. This should be implemented as a Phase 2
addition, not deferred to Phase 3. Specifically: if any active locker has
`unlock_at < now + 2 * minimum_lock_horizon_days (60 days)`, emit Signal B at confidence
0.50 + expiry_proximity_bonus where `expiry_proximity_bonus` scales from 0 (60 days out) to
0.20 (1 day out). This gives the bot-trader a 60-day warning that a locker is approaching
the danger zone.

**Precedent:** Documented in Certik's "Time-lock Bypass" post-mortem category (2023).
Multiple EVM projects have used legitimate 30-day timelocks followed by immediate drain.

---

### E-D02-16 — Pre-Drain Holder Migration to Defeat Deployer Attribution

**Description:** Three weeks before the drain, the deployer's whale wallet (which holds
80%+ of token supply) distributes tokens to 200 wallets, each holding <1% of supply. This
creates the appearance of healthy holder distribution (Detector D03 no longer fires). The
deployer controls all 200 wallets via a single master keypair, but there is no on-chain
connection visible without wallet clustering. On drain day, the LP is removed (Signal A
fires). D03 holder concentration is quiet. Human review sees a well-distributed token whose
LP was unexpectedly removed — the combined signal (D02 Critical + D03 previously-quiet)
is less alarming than D02 Critical + D03 Critical simultaneously.

**Signals defeated:**
- D02 Signal A fires correctly. D02 Signal B fires during the pre-drain period (0% LP burned).
- D03 (holder concentration) is suppressed during the pre-drain period by the distribution.
  The combined multi-detector score is lower than it would be with genuine concentration.

**Cost:** Medium. Gas for 200 token transfers and the overhead of managing 200 wallets.

**Reward preserved:** Full from D02. The evasion goal is defeating the *combined*
D02+D03 signal. D02 alone still fires; the attacker reduces the overall risk score
to potentially avoid blocking by systems that require multiple signals to fire.

**Detection cost:** Phase 3 wallet clustering. In Phase 2: the scoring crate should weight
D02 Signal A at maximum severity regardless of D03 state. A Critical LP drain should never
be attenuated by a simultaneously-quiet D03 signal.

**Precedent:** The "holder dilution before rug" pattern is documented in SolRPDS (Alhaidari
et al. 2025) as a pre-rug behavioral pattern. ZachXBT identified this in the TRUMP/MELANIA
token coordinated distribution analysis (February 2026).

---

## 3. Threshold Analysis

Current thresholds from `config/detectors.toml` `[rug_pull_lp_drain.*]`:

| Threshold | Current Value | Assessment | Direction | Proposed Value | Citation |
|-----------|--------------|------------|-----------|---------------|----------|
| `lp_removal_threshold` | 0.65 | Correctly calibrated from Chainalysis 2025. Not a round number. Resists naive evasion. | Keep | **0.65** | Chainalysis 2025 |
| `min_pool_usd` | 1000.0 | Appropriate dust filter. Risk: attacker maintains $1,001 liquidity post-partial-drain to stay above floor and suppress dead-pool check while still draining 99.9% of value. Floor should be verified against real drain corpus. | Tighten | **1500.0** | Chainalysis 2025 + dead-pool straddling gap |
| `min_prior_txs` | 100 | Loose. Achievable in minutes via wash-deploy (E-D02-14). Does not distinguish genuine activity from self-trades. | Add companion | **100 + pool_age_hours >= 1** | E-D02-14 analysis; SolRPDS 2025 |
| `lp_safe_floor_pct` | 70.0 | Correctly sourced from SolRPDS 2025 Table 3. Appropriate for Solana. | Keep | **70.0** | Alhaidari et al. 2025 |
| `minimum_lock_horizon_days` | 30 | Loose for the compounded expiry attack (E-D02-15). A 30-day horizon gives zero advance warning when drain is timed to coincide with the expiry-horizon boundary. | Tighten + add expiry-proximity signal | **45 days** + expiry_proximity_bonus | E-D02-15 analysis; Certik 2023 |
| `single_provider_bonus` | 0.15 | Correctly calibrated to RAVE probe anchor. Appropriate magnitude. | Keep | **0.15** | RAVE probe anchor |
| `drain_window_minutes` | 60 | Correct for single-actor fast drain. Misses 24h trickle (E-D02-7). Needs a companion 24h window. | Add companion | **60 (existing) + 1440 (new `drain_window_24h_minutes`)** | E-D02-7 analysis; LROO 2026 |
| `lp_providers_threshold` | 1 | Correctly set. Only single-provider pools get bonus. | Keep | **1** | SolRPDS 2025 |

### Threshold Tightening Rationale Details

**`min_pool_usd`: 1000 → 1500**

The dead-pool straddling attack (§8 below) is viable at $1,001 residual liquidity. The
Chainalysis 2025 $1,000 floor is a dust filter; lifting to $1,500 reduces the straddling
surface by 50% (an attacker needs to leave $1,501 instead of $1,001, meaning they can only
extract ~99.87% of value from a $1M pool instead of ~99.90%). Not a large reduction, but
combined with other controls it narrows the gap. False positive impact: pools between $1,000
and $1,500 that would have fired now do not. At this size, harm from a drain is minimal.
Classify as "unverified-heuristic adjustment" in REFERENCES.md — no academic citation for
the 1500 value specifically.

**`minimum_lock_horizon_days`: 30 → 45**

A 45-day horizon means an attacker must maintain a lock for 15 more days past the original
30-day floor to suppress Signal B. Combined with the expiry-proximity signal (emit Signal B
at low confidence when unlock_at < now + 90 days), this gives a 90-day total warning runway
instead of 30 days. The 45-day value is unverified-heuristic; calibrate from labelled corpus
in Sprint 4.

---

## 4. Fixture Corpus Gaps

Current corpus: 6 fixtures (RAVE latent, RAVE post-drain, SYNTHETIC Raydium drain, $WIF
negative, USDC negative, BONK near-threshold negative).

### Missing Coverage

**G1 — Real Raydium AMM v4 Drain (replaces SYNTHETIC)**
The SYNTHETIC fixture explicitly requires replacement. The corpus has zero confirmed non-PumpSwap
drain fixtures. Without a real Raydium v4 drain, Signal A is only validated against a
synthetic input with placeholder addresses.

**G2 — 24-Hour Trickle Drain Pattern**
No fixture tests the case where individual burns are below threshold but cumulative 24h burns
exceed it. This is the primary gap for E-D02-7. A synthetic or captured fixture should have
10–15 Burn rows each at 5–7% LP removal, spanning a 20-hour window.

**G3 — Expiring Locker (Near-Horizon)**
No fixture tests the state where `unlock_at` is between `now + 30d` and `now + 60d`. This
is the E-D02-15 danger zone. A fixture with `unlock_at = now + 31 days` should produce Signal
B (locker not counted as active, effective_safe_pct below floor). A fixture with
`unlock_at = now + 45 days` should NOT produce Signal B (locker still active). These are
boundary tests that the current corpus completely lacks.

**G4 — Multi-Pool Token Where Signal A on Pool 1 Does Not Suppress Signal B on Pool 2**
The DG-D02-5 suppression test (`different_pools_both_can_fire`) is a unit test against pure
functions, not an integration test with realistic multi-pool TokenMeta. A fixture of a
confirmed multi-pool drain where pool A is drained (Signal A) and pool B remains in latent
risk state (Signal B) would validate the per-pool suppression logic end-to-end.

**G5 — Dead-Pool Straddling (DG-D02-4 Boundary)**
No fixture tests `lp_burned_pct = 100 AND liquidity_usd = 1001` (just above the dead-pool
floor). This should produce Signal B not firing (burned >= floor) and Signal A firing only
if a Burn event is in pool_events. The boundary is exploitable (§8 below) and needs a test.

### Proposed New Fixtures

| Priority | File name | Signal | Source | Construction method |
|----------|-----------|--------|--------|---------------------|
| HIGH | `RAYDIUM-V4-confirmed-drain.json` | A only | RugCheck `rugged=true` API filter, dex=raydium_amm | Query RugCheck API for rugged=true tokens with Raydium AMM pool; capture pool_events Burn rows from Postgres pipeline |
| HIGH | `SYNTHETIC-trickle-drain-24h.json` | A (24h window) | Synthetic | Construct 15 Burn rows at 6% each, spaced 90 minutes apart; confirm cumulative_removed_pct crosses 0.65 at row 11; verify current 60-minute window misses it |
| MEDIUM | `SYNTHETIC-expiring-locker-boundary.json` | B (near-horizon) | Synthetic | Two variants: unlock_at = now+31d (Signal B fires) and unlock_at = now+45d (Signal B suppressed if minimum_lock_horizon raised to 45d) |
| MEDIUM | `MULTI-POOL-drain-plus-latent.json` | A on pool1 + B on pool2 | Real token (search RugCheck for multi-pool + rugged) | Capture pre/post drain state of both pools; validate per-pool suppression logic |
| LOW | `SYNTHETIC-dead-pool-straddling.json` | Neither (gap test) | Synthetic | lp_burned_pct=100, liquidity_usd=1001; confirm both Signal A and B miss without Burn row |

---

## 5. DG-D02-5 Suppression Analysis

### Is per-pool suppression correct?

The recommendation (spec §10 DG-D02-5, implemented in `d02_rug_pull.rs` line 276–285) is:
"If Signal A fires for pool P, suppress Signal B for pool P." The implementation is correct
for the intended use case: if a drain is actively observed, the latent-risk warning for the
same pool is superseded and redundant.

### Exploitability

**Exploit path:** An adversary who controls two pools (pool A: attacker's primary drain
pool; pool B: attacker's decoy pool) can arrange:

1. Pool A has 0% LP burned, 1 provider — Signal B fires (0.75/High).
2. Pool B has 100% LP burned (launched with burn at creation), $5K liquidity — Signal B
   does not fire (effective_safe_pct >= 70%).
3. The attacker creates a small, below-threshold Burn on pool B within the 60-minute window.
   Signal A does NOT fire for pool B (burn is 5%, below 0.65 threshold). The suppression
   check (`signal_a.is_some()`) is `false` for pool B. Signal B for pool B would have fired
   if pool B had low effective_safe_pct, but it does not — pool B looks safe.
4. The real drain happens on pool A. Signal A fires. Signal B for pool A is suppressed.

Net result: Signal A fires for pool A (correct). Signal B for pool A is suppressed (correct
— Signal A is more informative). Signal B for pool B does not fire (pool B looks safe). The
adversary has not defeated the detector — they merely confirm it behaves correctly. The attack
does not help the adversary.

**Conclusion:** The suppression logic cannot be exploited to mask a real drain because:
- Suppression only affects the SAME pool where Signal A fires.
- A different pool's Signal B is unaffected by Signal A on another pool.
- An attacker who wants to drain pool A cannot use Signal A on pool A to suppress Signal B
  on pool B.

The only scenario where suppression matters adversarially is if an attacker creates a small
Burn on pool A (below threshold → Signal A does not fire → suppression not triggered → Signal
B fires), then drains pool B (Signal A fires for B). This is standard multi-pool drain covered
by E-D02-2 in the spec.

**Verdict: DG-D02-5 suppression logic is safe as implemented.** The per-pool scope is the
correct scope. No redesign needed.

---

## 6. Worst-Case Crafted Token

A rug puller who has read this codebase constructs the following token to maximize damage
while staying below confidence threshold for the maximum possible time.

### Construction

```json
{
  "_evasion_design": {
    "goal": "Pass Signal B for 30 days. Execute 48-hour trickle drain. Total damage: full pool value.",
    "techniques_combined": ["E-D02-6 (lock just above horizon)", "E-D02-8 (multi-actor cluster)", "E-D02-9 (actor rotation)", "E-D02-14 (wash-deploy tx count)"]
  },
  "mint": "CRAFTED_TOKEN_MINT_...",
  "chain": "solana",
  "symbol": "SAFE",
  "name": "SafeDAO LP Locker Community Token",
  "decimals": 6,
  "token_program": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
  "mint_authority": null,
  "freeze_authority": null,
  "markets": [
    {
      "pool_address": "RAYDIUM_POOL_...",
      "dex": "raydium_amm",
      "lp_burned_pct": "0.00",
      "liquidity_usd": "500000.00",
      "lp_provider_count": 8
    }
  ],
  "lockers": [
    {
      "locker_address": "RAYDIUM_LOCKER_PROGRAM_...",
      "locker_name": "Raydium LP Locker",
      "locked_amount_raw": "720000000000000",
      "unlock_at": "<now + 31 days>"
    }
  ]
}
```

**LP breakdown at launch:**
- Pool LP total supply: 1,000,000,000,000 (raw)
- Locked in Raydium Locker: 72% of LP supply (`locked_amount_raw / lp_total_supply = 0.72`)
- LP burned: 0%
- effective_safe_pct = 0% burned + 72% locked = **72%** >= lp_safe_floor_pct (70%)

**Signal B result:** `effective_safe_pct = 72% >= 70%` → Signal B does NOT fire.

**Signal A result:** No drain yet → Signal A does NOT fire.

**lp_provider_count = 8:** No single-provider bonus. Signal B would have been 0.50 even
without the lock.

**Day 0 through Day 30:** The detector evaluates this token every cycle. Both signals are
silent. The bot-trader has no reason to avoid this pool. Social media sees 8 LP providers,
a Raydium-locked locker, $500K liquidity. This looks like a legitimate project.

**Day 30: Lock expires into the danger zone.** `unlock_at` is now within
`minimum_lock_horizon_days = 30`. The locker contribution drops to zero. `effective_safe_pct`
drops from 72% to 0% in one evaluation cycle. Signal B fires at 0.75/High.

**Day 30 to Day 31:** While Signal B is newly firing (and the bot-trader may be exiting
positions), the attacker's 8 LP provider wallets (each holding 12.5% of LP) begin executing
small Burn transactions staggered 90 minutes apart. Each wallet burns 6.25% of LP supply per
transaction. Per-actor, per-60-minute window: 6.25% << 65% threshold. Signal A never fires.

**Day 31 (48 hours of trickle):** All 8 wallets have burned their LP positions. Pool is
empty. Signal A never fired during the drain. Signal B fired at day 30 but the
drain completed before most consumers would have acted (the bot-trader may be mid-position-close).

**Total damage:** $500,000 pool fully drained. Signal A: zero events. Signal B: fired at
day 30 (one cycle before drain begins), but trickle drain was invisible throughout.

### Why this is the worst case

1. Maximum latent-risk suppression: 30-day locker at 72% keeps Signal B silent for 30 days.
2. Zero Signal A events during the drain (24h trickle, multi-actor).
3. Single warning cycle (Signal B fires day 30, drain starts immediately) — insufficient
   warning for consumers to close positions gracefully if they have concurrent open positions.
4. All LP provider wallets aged and funded from a single clean CEX withdrawal with no
   connecting graph edges visible in Phase 2.

### Confidence walkthrough at drain time

- Signal B at day 30 (lock expires): effective_safe_pct = 0%, 8 providers, no bonus.
  `latent_conf = 0.50 + 1.0 * 0.25 + 0 = 0.75` → capped at **0.75/High**.
- Signal A during 48-hour trickle: each individual 6.25% burn: 6.25% < 65% → no row
  returned by query. 60-minute cumulative: 6.25% per actor per 60 min = 6.25% << 65%.
  Signal A confidence: **0.0** (no event fired).
- Net highest event: **0.75/High** from Signal B only.

---

## 7. DG-D02-4 Dead-Pool Skip Exploitation Analysis

### The mechanism

From `d02_rug_pull.rs` lines 260–285:

```
is_dead_pool = lp_burned_pct >= 100.0 AND liquidity_usd < min_pool_usd
Signal B is skipped when is_dead_pool = true.
Signal A is evaluated regardless of is_dead_pool.
```

The intent: a PumpSwap pool that has been drained (100% LP marked burned, $0.002 liquidity)
should not fire Signal B ("nothing to protect"). Signal A fires from the Burn event row.

### Exploitation scenario

**Setup:** Attacker executes a partial drain, leaving exactly $1,001 of liquidity in the pool.
The PumpSwap AMM does NOT mark `lp_burned_pct = 100%` because the drain was partial.

**State after partial drain:**
- `lp_burned_pct = 0%` (PumpSwap only marks 100% burned after FULL drain)
- `liquidity_usd = 1001`
- `effective_safe_pct = 0%` → Signal B: 0.75/High. FIRES correctly.
- Signal A: only fires if cumulative burn crosses 65% threshold for any actor. If the
  attacker drained 99.9% of the $500K pool (leaving $500 — below min_pool_usd!), Signal A
  would fire. But if they left $501, pool_usd_for_guard = $501 < $1000 → BOTH signals
  skipped.

**The straddling band:** An attacker who leaves exactly $1,001 of the original $500K pool:
- `pool_usd_for_guard = 1001 >= 1000` → pool passes the dust filter.
- `lp_burned_pct = 0%` → is_dead_pool = false (PumpSwap does not mark 100% burned for partial).
- Signal B fires (0% burned < 70% floor) at 0.75/High.
- Signal A: fires IF the Burn rows in pool_events show cumulative_removed_pct >= 0.65.

For Raydium AMM (not PumpSwap): a partial drain of 99.8% leaves $1,001 on a $500K pool.
The actor's `cumulative_removed_pct = 0.998 >= 0.65`. Signal A fires at confidence ≈ 0.92.

**Conclusion:** The straddling attack fails for Raydium because Signal A fires on any drain
>= 65% regardless of residual liquidity. The straddling attack is only viable for drains that
extract less than 65% (< $325,000 of $500K) — at which point the pool retains $175K and is
not "straddled" meaningfully.

**Where DG-D02-4 genuinely creates a gap:** The dead-pool branch (`lp_burned_pct == 100 AND
liquidity < min_pool_usd`) correctly skips Signal B. But consider this sequence:
1. PumpSwap pool is fully drained (Signal A fires for the Burn event). `lp_burned_pct` is
   set to 100% by PumpSwap. `liquidity_usd = 0.002`.
2. `is_dead_pool = true` → Signal B skipped. Correct.
3. The pool is re-seeded: someone adds $5,000 of liquidity to the dead pool. Now
   `liquidity_usd = 5000 >= min_pool_usd`. But `lp_burned_pct` might still be 100% in the
   `TokenMeta` snapshot if the registry has not yet processed the new Mint event.
4. Next evaluation cycle: `lp_burned_pct = 100%` (stale), `liquidity_usd = 5000`. Signal B:
   effective_safe_pct = 100% >= 70% → does NOT fire. `is_dead_pool = false` (liquidity is
   above floor). Signal A: no Burn events since the re-seed.

The re-seeded pool is structurally unsafe (new LP provider with 0% of the new LP burned,
$5K at risk) but neither signal fires because `lp_burned_pct` is stale at 100%.

**Verdict: DG-D02-4 is exploitable only in the re-seed scenario.** The simple straddling
attack fails. The re-seed scenario requires a fix: when `lp_burned_pct = 100% AND
liquidity_usd >= min_pool_usd`, the registry must re-fetch `lp_burned_pct` from the live
pool state before using the cached value.

---

## 8. Determinism and Code-Level Findings

### C1 — `ingested_at: Utc::now()` breaks full-struct reproducibility

**Location:** `crates/detectors/src/d02_rug_pull.rs` line 795:

```rust
fn make_event(ctx: &DetectorContext<'_>, ...) -> AnomalyEvent {
    let confidence = Confidence::new(confidence_f64).unwrap_or(Confidence::ZERO);
    AnomalyEvent {
        ...
        observed_at: ctx.window.end,      // deterministic (block-time sourced)
        window: (ctx.window.block_start, ctx.window.block_end), // deterministic
        ingested_at: Utc::now(),           // <<< wall-clock: NON-DETERMINISTIC
    }
}
```

**Impact:** The CLAUDE.md reproducibility requirement states "given the same block range
input, output MUST be deterministic." Two evaluations of the same input produce `AnomalyEvent`
structs that differ in `ingested_at` by milliseconds. If the consumer hashes or deduplicates
by the full struct (e.g., for idempotent DB inserts), every re-evaluation produces a
different hash. This is the same finding identified in D01 review (0001 §C1) — it was not
fixed during D02 implementation.

**Fix:** `ingested_at` should be supplied by the caller (scheduler/orchestrator), not
generated inside `make_event`. Pass `ingested_at: chrono::DateTime<Utc>` as a parameter to
`make_event`, or set it in `DetectorContext` as `ctx.ingested_at`. The orchestrator sets
`ingested_at` once per evaluation batch. All events in that batch share the same
`ingested_at`, which is deterministic for re-runs of the same batch.

**Severity:** MEDIUM. Correctness of confidence math is unaffected. Reproducibility is
violated only for `ingested_at`. If consumers ignore this field for deduplication (using
`(detector_id, token, chain, window.block_start, window.block_end)` as the dedupe key), the
impact is limited to audit log clutter.

---

### C2 — `max_by` with `partial_cmp` fallback to `Equal` on NaN loses worst drain

**Location:** `crates/detectors/src/d02_rug_pull.rs` lines 389–396:

```rust
let worst = drain_rows
    .into_iter()
    .max_by(|a, b| {
        a.cumulative_removed_pct
            .partial_cmp(&b.cumulative_removed_pct)
            .unwrap_or(std::cmp::Ordering::Equal)  // <<< NaN fallback
    })
    .expect("non-empty Vec must have a max");
```

**Impact:** `cumulative_removed_pct` is `f64` (from the SQL query as `DOUBLE PRECISION`).
If the SQL returns a NaN value (malformed data or division by zero if `lp_total_supply = 0`
is passed as `$5`), `partial_cmp` returns `None`, and the fallback is `Ordering::Equal`.
`max_by` with all-Equal comparisons returns the LAST element, not the element with the
highest cumulative drain. In pathological cases with NaN rows, the "worst" drain row selected
may not be the actual worst drain — it is whichever row happened to be last in the result
set. This affects both confidence (Signal A formula uses `worst.cumulative_removed_pct`) and
evidence (`worst.tx_hash`, `worst.actor`).

**Note on SQL guard:** The SQL query passes `$5 = lp_total_supply` from the Postgres pools
table. If `lp_total_supply = 0`, the query computes `lp_tokens::DOUBLE PRECISION /
0::DOUBLE PRECISION` = `Inf` or `NaN` in Postgres (PostgreSQL returns `Infinity` for
division by a zero cast to float; behavior differs from integer division). The caller guards
`lp_total_supply` via `fetch_rug_pull_drain_events` but the guard is in the business logic
layer, not the SQL. A `lp_total_supply = 0` value reaching the query produces Infinity, not
NaN, which would `partial_cmp` correctly as `Some(Greater)`. So the NaN path requires a
genuinely malformed float from the DB (rare), but the defense should still be explicit.

**Fix:**

```rust
let worst = drain_rows
    .into_iter()
    .max_by(|a, b| {
        a.cumulative_removed_pct
            .partial_cmp(&b.cumulative_removed_pct)
            .unwrap_or_else(|| {
                // NaN comparison: treat NaN as less than any real value.
                // If a is NaN and b is not, b wins. If both NaN, equal.
                match (a.cumulative_removed_pct.is_nan(), b.cumulative_removed_pct.is_nan()) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => std::cmp::Ordering::Equal,
                }
            })
    })
    .expect("non-empty Vec must have a max");
```

**Severity:** LOW. NaN rows from Postgres DOUBLE PRECISION are unlikely under normal
conditions. The fix is a one-liner that makes the NaN handling explicit rather than relying
on the `Equal` fallback.

---

### C3 — Signal B `pool_usd` guard fires AFTER `effective_safe_pct` check (ordering issue)

**Location:** `crates/detectors/src/d02_rug_pull.rs` lines 576–590:

```rust
// effective_safe_pct check
let effective_safe_pct = market.lp_burned_pct + active_locked_pct;
let lp_safe_floor = ...;
if effective_safe_pct >= lp_safe_floor {
    return None;  // Pool is protected — exit early
}

// pool_usd guard (noise filter)
let pool_usd: Decimal = pool_row_opt
    .map(|r| r.liquidity_usd)
    .unwrap_or(market.liquidity_usd);
let min_pool_usd = ...;
if pool_usd < min_pool_usd {
    return None;  // Pool is dust
}
```

**Impact:** The `pool_usd` guard fires AFTER the `effective_safe_pct` check. This means that
for pools with `effective_safe_pct >= floor`, the pool_usd check is never reached — which is
correct (safe pools are exited early). However, for at-risk pools with `effective_safe_pct <
floor`, the full locker computation (`compute_active_locked_pct`) has already been executed
before the pool_usd dust filter runs. For tokens with hundreds of pools (unusual but possible
for established memes), this creates unnecessary computation on dust pools that would have
been filtered before the locker computation if the order were reversed.

**More importantly:** The `min_pool_usd` check is also performed by the CALLER before
invoking `evaluate_signal_b` (line 247–257 in the main `evaluate()` loop). The guard is
redundant inside `evaluate_signal_b`. If the caller guard is ever removed or reordered in a
future refactor, the inner guard provides correct fallback behavior. But the redundancy is
a code clarity issue: a reviewer reading `evaluate_signal_b` in isolation does not know the
caller already checked the pool USD floor.

**Fix:** This is a minor code quality issue, not a correctness bug. Document the guard
redundancy with a comment in `evaluate_signal_b`:

```rust
// pool_usd guard — also checked by caller before evaluate_signal_b is invoked,
// but retained here for correctness if called in isolation from tests.
```

**Severity:** LOW (code clarity). No correctness impact.

---

### C4 — `compute_signal_b_confidence` does not validate `effective_safe_pct >= floor`

**Location:** `crates/detectors/src/d02_rug_pull.rs` lines 664–703.

`compute_signal_b_confidence` is a public function (called from tests directly). It does NOT
check whether `effective_safe_pct >= lp_safe_floor`. It always returns a `SignalBResult` with
a non-zero confidence. The caller (`evaluate_signal_b`) checks the floor and returns `None`
if `effective_safe_pct >= floor`. But the test `signal_b_wif_high_burn_no_signal` (line 1002)
calls `compute_signal_b_confidence` directly and then checks `result.effective_safe_pct >=
floor` — it does NOT assert that no event would be emitted. The test comment says "But
`compute_signal_b_confidence` itself doesn't check the floor; it returns a confidence value."
This is documented but means a caller who uses `compute_signal_b_confidence` directly (without
going through `evaluate_signal_b`) would emit a false positive confidence for a $WIF-like
pool.

**Fix:** Either:
(a) Return `Option<SignalBResult>` from `compute_signal_b_confidence` (returning `None` when
    `effective_safe_pct >= floor`), making the function self-documenting and safe for direct
    callers; or
(b) Add a debug_assert in `evaluate_signal_b` that `effective_safe_pct < lp_safe_floor` when
    `compute_signal_b_confidence` is called, to catch misuse in debug builds.

Option (a) is preferred for a public function. This is an advisory finding — the crates/
directory is read-only for this review.

**Severity:** LOW. Only affects callers who use the public function directly. No production
path calls `compute_signal_b_confidence` without the floor check.

---

### C5 — HashMap iteration order in `meta.markets` is not guaranteed

**Location:** The concern is whether `meta.markets: Vec<MarketInfo>` has a stable iteration
order. In `d02_rug_pull.rs` lines 217–293, the for loop iterates `&meta.markets`. If
`TokenMeta.markets` is a `Vec`, iteration order is insertion order — deterministic given the
same input. If it were a `HashMap<Address, MarketInfo>`, iteration order would be
non-deterministic across runs.

**Verdict:** `meta.markets` is a `Vec<MarketInfo>` per `crates/common/src/token.rs`. No
HashMap iteration non-determinism in Signal B computation.

**Where HashMap is used:** The `Evidence::metrics` field (used in `build_signal_a_evidence`
and `build_signal_b_evidence`) is likely a `HashMap<String, Decimal>`. If the consumer
serializes the full evidence struct for hashing/deduplication, the serialization order of
metrics keys is non-deterministic in standard Rust `HashMap`. Use `BTreeMap<String, Decimal>`
for deterministic serialization.

**Severity:** LOW for production correctness (metrics are display-only). MEDIUM if consumers
hash evidence for deduplication, as two identical alerts may produce different hashes depending
on HashMap iteration order.

---

## 9. Sign-off Verdict

**Block Sprint 3 exit until the following are resolved:**

**CRITICAL (block ship):**

- **C2 (NaN fallback in max_by):** Low probability but the NaN handling should be explicit.
  One-line fix. No reason to defer.

**HIGH (resolve or document accepted risk):**

- **E-D02-11 (Token-2022 withdraw_withheld as non-LP drain):** This is a complete Signal A
  bypass with zero coverage in D02 and only partial coverage in D01 (D01 fires on high
  transfer fee, but not on the withdrawal itself). Document explicitly in REFERENCES.md as a
  known gap. Add a backlog item: "Phase 2 extension: cross-detector linkage D01 fee authority
  + D02 withdraw_withheld monitoring." This is not a blocker if documented.

- **E-D02-15 (compounded lock expiry + same-block drain):** The lack of an expiry-proximity
  signal means the attacker gets a 30-day safe window with zero advance warning to the
  bot-trader. The spec deferred the "lock expiring soon" variant to Phase 3. Elevate to Phase
  2 — it is a single additional condition in `evaluate_signal_b` and requires no new data
  source.

**MEDIUM (ship with documented caveat):**

- **E-D02-7 (24h trickle drain):** The 60-minute window misses multi-hour trickle drains.
  Signal B fires throughout (latent risk). Document as a known gap with the backlog item for
  a 24h companion window in the next sprint.

- **C1 (ingested_at wall-clock):** Does not affect confidence math. Document the
  reproducibility caveat for consumers who use the full struct for deduplication.

- **DG-D02-4 re-seed scenario:** Document the re-seed gap in the design notes. Requires a
  registry fix (re-fetch lp_burned_pct when liquidity re-appears on a 100%-burned pool).

**LOW (no action required before Sprint 3 exit):**

- C3, C4, C5: code quality issues. Carry as technical debt. Address in Sprint 4 cleanup.

---

## 10. REFERENCES.md Rows Added by This Review

The following entries should be added to `REFERENCES.md`:

| Mechanism | Signal / Formula | Source | Used In | Verified Against |
|-----------|-----------------|--------|---------|-----------------|
| 24h trickle drain sub-category | Multi-day slow LP drain (1–3 day drain observed in LROO corpus) | Shoaei et al. 2026 (LROO) §3, https://arxiv.org/html/2603.11324 | D02 evasion E-D02-7; `drain_window_24h_minutes` backlog | Live fetch 2026-04-21 |
| Multi-actor LP pre-distribution rug | Deployer seeds N wallets with LP before launch; each burns < threshold; cluster burns 100% | Alhaidari et al. 2025 (SolRPDS) multi-actor coordination category, https://arxiv.org/abs/2504.07132; ZachXBT BALD BSC post-mortem 2024 | D02 evasion E-D02-8; Phase 3 cluster aggregation query | Referenced 2026-04-21 |
| Token-2022 withdraw_withheld drain path | `withdraw_withheld_tokens_from_accounts` extracts fee-withheld value without producing pool Burn event; bypasses Signal A entirely | Solana Token-2022 TransferFeeConfig docs, https://solana.com/docs/tokens/extensions/transfer-fees; Sun et al. 2024 §4 "Hidden Fee" taxonomy category | D02 evasion E-D02-11; Phase 3 cross-detector D01/D02 linkage | Referenced 2026-04-21 |
| Time-lock bypass via lock-expiry drain | Legitimate timelocked locker expired; drain executed same block as lock expiry; zero advance-warning window | Certik Security "Time-lock Bypass" post-mortem category, https://certik.com/blog/security/time-lock-vulnerabilities-in-defi (2023) | D02 evasion E-D02-15; `expiry_proximity_bonus` backlog | Referenced 2026-04-21 |
| Holder dilution before rug (pre-drain distribution) | Deployer distributes token supply to N wallets before rug; suppresses D03 concentration signal; documented as pre-rug behavioral pattern | Alhaidari et al. 2025 (SolRPDS) Table 2 pre-rug behavioral features; ZachXBT TRUMP/MELANIA distribution analysis Feb 2026 | D02 evasion E-D02-16; scoring crate: D02 Critical must not be attenuated by D03 quiet signal | Referenced 2026-04-21 |
| Fake locker upgrade authority check | Locker program upgrade authority controlled by deployer enables token release; discriminator whitelist insufficient | Sun et al. 2024 §4 "Fake LP Lock" (one of 9 undetectable categories), https://arxiv.org/abs/2403.16082 | D02 evasion E-D02-10; Phase 2 compensating control: check locker program upgrade authority at ingestion | Live fetch 2026-04-21 |
