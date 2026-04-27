# Review 0004 — D07 Token-2022 Withdraw-Withheld Drain: Adversarial Evasion Analysis

**Date:** 2026-04-21
**Reviewer:** security-researcher agent, mg-onchain-analysis
**Detector:** D07 `withdraw_withheld_drain`
**Spec ref:** `docs/designs/0012-detector-07-withdraw-withheld.md`
**Implementation ref:** `crates/detectors/src/d07_withdraw_withheld.rs`
**SQL ref:** `docs/queries/d07_withdraw_withheld.sql`
**Migration ref:** `migrations/postgres/V00007__token2022_instructions.sql`
**Chain-adapter ref:** `crates/chain-adapter/src/solana/token2022.rs`
**Config ref:** `config/detectors.toml` `[withdraw_withheld.*]`
**Prior reviews:** `docs/reviews/0001-d01-honeypot-evasions.md`, `docs/reviews/0002-d02-rug-pull-evasions.md`, `docs/reviews/0003-d04-pump-dump-evasions.md`
**Motivating gap:** `docs/reviews/0002-d02-rug-pull-evasions.md` §E-D02-11
**Status:** Draft

---

## 1. Executive Summary

D07 closes the specific gap identified as E-D02-11 — `WithdrawWithheld*` instructions bypass
both D02 Signal A (no LP Burn row) and D06 Signals B/C (no zero-address Transfer). That core
blind spot is now addressed. The implementation is structurally correct, the confidence formulas
match the spec exactly, and the determinism contract is maintained in the pure computation
functions.

However, the adversarial surface is wider than the spec's eight evasions acknowledge.
This review identifies twelve evasion paths, four of which are new. Three code-level findings
are present, two of which affect operational correctness (not merely audit quality). One
confirmed stub `pool_volume_usd = 0.0_f64` at `d07_withdraw_withheld.rs:364` permanently
disables the established-protocol suppression guard, meaning any established protocol whose
`withdraw_withheld_authority` is compromised will have Signal A silently suppressed. This is
the highest-severity code finding.

The Signal B fresh-wallet check has a structural dependency gap: `wallet_funding_events` is
created by V00007 as an empty table with no indexer write path documented or implemented.
If the indexer never populates it, the `fresh_wallet_bonus` never fires, and Signal B is
permanently weakened to a maximum of 0.55 (rapid-rotation-only) or 0.40 (base) rather than
the intended 0.75 cap. This silently reduces Signal B's detection capability without any
observable error.

Three threshold values require immediate adjustment before corpus calibration is possible.
The `fresh_wallet_funding_hours = 48` parameter is trivially evaded with one day of planning.
The `min_extraction_events = 3` threshold allows a scammer to extract up to $999 three times
before detection. The `established_protocol_fee_extraction_allowlist_pct = 0.90` is
indefensible given that the pool_volume_usd it is measured against is hardcoded to zero.

**Sign-off: Block.** Two conditions must be met before ship:
1. The `pool_volume_usd = 0.0_f64` stub at line 364 must be documented as a named accepted
   risk or — better — the suppression logic must be inverted: when pool_volume_usd is unknown,
   do NOT suppress (fire Signal A), not suppress silently. The current behavior is the wrong
   failure mode for security logic.
2. The `wallet_funding_events` indexer write path must be confirmed as implemented or explicitly
   documented as a known gap that causes `fresh_wallet_bonus` to always be 0.0.

The remaining findings may ship as documented accepted risk.

---

## 2. Evasion Catalog

The spec (0012 §14) documented eight evasion patterns (E-D07-1 through E-D07-8). This review
identifies four additional evasion techniques and sharpens the analysis of two existing ones.
Each entry maps which signal it defeats, attacker cost, preserved reward, and Phase fix.

---

### E-D07-1 — Slow Drip Straddling Window Boundaries

**Spec ref:** 0012 §14 E-D07-1. This review adds precision missed by the spec.

**Description:** The spec documents the $499/$499 week-straddling variant but understates the
optimal attack. The detection window is a sliding 168-hour window evaluated at each detector
run interval. The attacker does not need to straddle calendar weeks — they need to straddle the
168-hour window boundary at each evaluation run. If the scheduler evaluates D07 every 24 hours,
the attacker extracts on day 0, day 8, and day 16 — each extraction falls in a different
168-hour window (0-7d, 8-15d, 16-23d). Event count is always 1 per window (below `min_extraction_events
= 3`). Cumulative USD is always below $1,000 per window. Signal A never fires.

With this pattern, the attacker extracts indefinitely at slightly below the thresholds. The
constraint is only that the withheld balance has time to accumulate between extractions. A token
with 50% fee and moderate volume ($50K/week in swaps) accumulates ~$25K in withheld fees per
week. Extracting $999 every 8 days captures ~$3,600/month invisibly.

**Signals defeated:** Signal A fully. Signal B only fires if the attacker changes authority.

**Cost:** Negligible. One instruction per extraction, timed at 8-day intervals.

**Reward preserved:** Full. The attacker extracts the vast majority of accumulated fees.

**Detection cost (additional signal required):** A 30-day cumulative query (W3-30d) with
threshold `min_cumulative_withdraw_usd_30d`. The spec correctly identifies this as DG-D07-4.
Phase 3 enhancement. In Phase 2 the only partial mitigation is ensuring `min_extraction_events`
is as low as is practical without excessive false positives — currently 3, which the attacker
bypasses by running only 1 event per window.

**Cross-ref:** DG-D07-4; E-D02-7 (trickle drain, same temporal evasion pattern applied to LP
burns). The fix structure is identical: a multi-window cumulative query.

---

### E-D07-2 — Authority Tenure Gaming (8-Day Rotation)

**Spec ref:** 0012 §14 E-D07-2. This review adds the compounding variant.

**Description:** The attacker rotates `withdraw_withheld_authority` every 8 days — one day
above `min_authority_tenure_days = 7`. Signal B fires at base confidence 0.40 on every rotation
(authority rotation is detected), but the `rapid_rotation_bonus = 0.15` is never applied
(8 > 7). With no fresh-wallet check (see Signal B weakness analysis in §5), maximum Signal B
confidence is 0.40 — Severity Info. Most consumers filter at Medium (0.60+).

**Compounding variant:** The attacker uses the same wallet as "new authority" on every rotation
(wallet A rotates to wallet B, then back to wallet A, alternating). This makes each rotation
look like a "re-key" event. Tenure calculation in `evaluate_signal_b` uses
`rotation_rows[idx - 1].row.block_time` — with alternating wallets, each authority's actual
tenure is 8 days. The rapid-rotation bonus still does not fire. Signal B emits 0.40 on every
rotation indefinitely.

**Signals defeated:** Signal B `rapid_rotation_bonus` never fires. Signal A still fires if
extraction thresholds are met.

**Cost:** Low. One `SetAuthority` instruction every 8 days.

**Reward preserved:** Full for Signal A bypass. Partial for Signal B (still emits at 0.40).

**Detection cost:** The spec's fix (rotation-count feature: ≥3 distinct rotations within
30 days upgrades Signal B by +0.15) is the correct mitigation. This is a Phase 3 enhancement
to Query W2. The alternating-wallet variant requires an additional check: if the same pair of
wallets appears as (A→B, B→A) within the window, treat the pair's combined tenure as the
effective tenure (always 8 days). This requires the rotation history to track wallet identity,
not just timestamp deltas.

**Cross-ref:** D06 `mint_authority_grace_period_days` (same evasion pattern, different authority
type). E-D02-15 (lock-expiry same-block drain — tenure gaming to defeat a time-based guard).

---

### E-D07-3 — Jupiter-Routed Extraction (CPI Wrapping)

**Spec ref:** 0012 §14 E-D07-3.

**Description:** The attacker routes `WithdrawWithheldTokensFromAccounts` through the Jupiter
aggregator program as a CPI. The top-level instruction is a Jupiter program invocation; the
Token-2022 instruction is an inner instruction. The decoder at `token2022.rs` processes inner
instructions via the `inner_instructions` HashMap, so this IS caught — but only if the indexer
populates `inner_instructions` correctly for every transaction.

**Evasion refinement (new, not in spec):** A subtler variant routes the extraction through a
**custom intermediary program** deployed by the attacker (not Jupiter) that calls the
Token-2022 program via CPI. The pre-filter at the decoder level checks whether
`TOKEN_2022_PROGRAM` appears in the transaction's account keys. If the custom intermediary
program is constructed such that the Token-2022 program ID does NOT appear in the outer
transaction's `accountKeys` (possible if the CPI target is referenced only in an inner
instruction's program_id, not in the outer accounts list), the pre-filter at the indexer level
may skip the transaction entirely before the decoder runs.

The pre-filter implementation in `token2022.rs` documentation (line 20-22) says the CALLER
must have already pre-filtered, but does not specify whether the pre-filter checks the
outer `accountKeys` array or the full flattened account list including inner instruction
program IDs. If the implementation checks only outer `accountKeys`, a CPI where Token-2022 is
invoked only in an inner instruction but not present in the outer accounts would be missed.

**Signals defeated:** If the pre-filter misses the transaction, Signal A misses entirely.

**Cost:** Medium. Requires deploying a custom wrapper program.

**Detection cost:** Verify the indexer's pre-filter checks the full flattened account list
(outer accountKeys UNION all inner instruction program IDs), not just the outer accountKeys.
This is a code correctness requirement, not a Phase 3 enhancement.

---

### E-D07-4 — CPI-Proxy via Deployer-Controlled PDA

**Spec ref:** 0012 §14 E-D07-4.

**Description:** Deployer creates a custom program with a PDA that holds the
`withdraw_withheld_authority` role. Signal A fires with `authority_match = "unknown"` and
confidence reduced by 0.10 (per `d07_withdraw_withheld.rs:318`). The authority recorded in
evidence is the PDA address, not the deployer wallet.

**Additional attack surface (new):** The deployer can structure the PDA so that its controlling
program has an **upgrade authority different from the deployer** — e.g., set to a burner wallet
or renounced. This makes the PDA look like it belongs to a legitimate autonomous program. The
`authority_match = "unknown"` confidence reduction of 0.10 is the only signal. At the base case
(3 events, $1,000, unknown authority), Signal A fires at `0.60 - 0.10 = 0.50`, Severity Info —
below the actionable threshold for most consumers.

The attacker's goal is not to defeat Signal A entirely but to reduce its severity enough that
automated consumers do not block. At 0.50, a consumer with a Medium (0.60) filter threshold
passes the token. The attacker can extract $999 three times across three windows at 0.50
confidence with no automated response.

**Signals defeated:** Signal A confidence reduced to potentially below consumer thresholds.

**Cost:** Medium-High (requires Solana BPF program deployment).

**Reward preserved:** Partial. Signal A fires at reduced confidence. Attribution is degraded.

**Phase fix:** When `authority_match = "unknown"`, check if the instruction signer is a PDA
whose owner program has the token deployer as upgrade authority. If so, reclassify as
`authority_match = "deployer_pda"` with 0.05 penalty instead of 0.10. Phase 3 graph crate.

---

### E-D07-5 — MultiSig Masquerading as Legitimate Treasury

**Spec ref:** 0012 §14 E-D07-5.

**Description:** The deployer sets `withdraw_withheld_authority` to a Squads v4 multisig
program account that they fully control (all member wallets funded from the same source, not
individually visible without graph analysis). The rotation triggers Signal B. Signal A fires
on extraction. But the multisig program account was created 72 hours before the rotation,
bypassing the `fresh_wallet_funding_hours = 48` check (the multisig account is the "new
authority" and its first SOL receipt was 72h before rotation, which is > 48h, so no bonus).

**Squads-specific refinement (new):** Squads v4 has an internal member rotation mechanism.
The attacker creates the multisig with 3 member wallets, waits 48+ hours for it to age past
the fresh-wallet threshold, then rotates the internal Squads members (a Squads-internal
instruction, not a Token-2022 SetAuthority instruction). D07 Signal B does NOT fire on
Squads-internal member rotation — it only fires on Token-2022 `SetAuthority` instructions
targeting `WithdrawWithheldTokens`. The attacker can cycle through disposable member wallets
inside the Squads multisig indefinitely without triggering any D07 signal.

**Signals defeated:** Signal B `fresh_wallet_bonus` never fires after initial rotation.
Squads-internal member rotations are entirely invisible to D07.

**Cost:** Low after initial setup. Squads member rotation is cheap and has no D07 visibility.

**Reward preserved:** Full.

**Phase fix:** Monitor Squads v4 member-rotation instructions for the multisig accounts that
hold `withdraw_withheld_authority` roles. This requires the indexer to track Squads program
instructions, not just Token-2022 instructions. Phase 3 enhancement.

---

### E-D07-6 — On-Chain Burn Post-Extraction (Deflationary Cover)

**Spec ref:** 0012 §14 E-D07-6.

**Description:** Covered by the spec. Signal A fires on extraction; D06 Signal B fires on
the burn. D07's evidence includes extraction tx_hash for correlation.

**Refinement (new — not in spec):** The attacker burns the tokens NOT from their own wallet
but sends them to a **charity/DAO program** that then executes the burn via governance. This
produces a `burn_authority = charity_program` in D06 evidence rather than `= deployer_wallet`.
The scoring crate's proposed rule (D07 Signal A + D06 Signal B from the same authority address
within 24h) does NOT fire because the addresses differ. Without the same-authority correlation,
the two signals appear to be from independent actors.

**Signals defeated:** The scoring-crate correlation rule proposed in the spec is defeated.
D07 Signal A still fires. D06 Signal B still fires. But combined-severity escalation fails
unless the scoring crate correlates via token address rather than authority address.

**Detection cost:** The scoring crate correlation rule should key on `(token_address, window)`
not `(authority_address, token_address, window)`. Any D07 Signal A AND D06 Signal B for the
same token within 24h should escalate combined severity regardless of authority match.

---

### E-D07-7 — Whitelist Override via Temporary Fee-Rate Zero Reset

**Spec ref:** 0012 §14 E-D07-7. This review adds a new variant.

**Description:** The spec notes that D07 captures `transfer_fee_bps` at evaluation time (which
may be 0 after a fee reset), setting `combined_with_d01_s2 = "0"`. This defeats the D01-linkage
evidence key.

**New variant — Maximum-Fee Dust Reset:** Instead of resetting `fee_basis_points` to 0, the
attacker sets `maximum_fee` to 1 (one raw token unit). The fee basis points remain high (e.g.,
5000 bps), but the per-transfer fee is capped at 1 raw unit — negligible. New buyers accumulate
no meaningful withheld balance. D01 Signal S2 does NOT fire (it checks `fee_bps`, not
`maximum_fee`, per REFERENCES.md "Token-2022 maximum_fee field abuse"). The `combined_with_d01_s2`
evidence key evaluates `fee_bps > sell_tax_threshold_bps = 3000` — this IS true (5000 > 3000),
so `combined_with_d01_s2 = "1"` fires. But there is nothing left to extract because
`maximum_fee = 1` made actual accumulation negligible.

This variant defeats the extraction model itself (no accumulation) but is included because an
attacker who does the reverse — raises `maximum_fee` back to a large value after accumulation —
executes the actual drain while temporarily suppressing D01 S2 during the high-value phase.
The sequence: deploy with `maximum_fee = large` → accumulate → reset to `maximum_fee = 1`
(D01 S2 would now fire at `fee_bps = 5000` but the fee is effectively zero) → extract via
`WithdrawWithheldTokensFromMint` (the already-accumulated balance in the mint) → D07 Signal A
fires but `combined_with_d01_s2` may show fee effectively zero if eval happens after max_fee reset.

**Signals defeated:** D01 S2 linkage via `combined_with_d01_s2` is degraded.

**Phase fix:** Store `fee_bps_at_extraction` and `max_fee_at_extraction` in
`token2022_instructions` at instruction decode time (from pre-execution mint account state).
DG-D07-2 workaround per spec §14 E-D07-7.

---

### E-D07-8 — LP-Add Laundering of Extracted Fees

**Spec ref:** 0012 §14 E-D07-8. Covered by the spec. No additional analysis required.

---

### E-D07-9 — Harvest-Without-Withdraw: Value Staging in Mint's Withheld Balance

**Description (new):** `HarvestWithheldTokensToMint` (discriminator byte 29) is permissionless —
anyone can call it to consolidate withheld balances from individual token accounts into the
mint's global `withheld_amount` field. This instruction IS decoded and stored in
`token2022_instructions` with `instruction_kind = 'harvest_withheld_to_mint'`. Query W1 for
Signal A explicitly EXCLUDES harvest rows:

```sql
AND instruction_kind IN (
  'withdraw_withheld_from_accounts',
  'withdraw_withheld_from_mint'
)
```

The attacker's tactic: call `HarvestWithheldTokensToMint` repeatedly across many evaluation
windows to consolidate value into the mint's withheld balance without triggering Signal A.
At the moment of maximum accumulation, call `WithdrawWithheldTokensFromMint` once — a single
extraction event. Event count = 1 (below `min_extraction_events = 3`). Signal A does not fire.
Even if the single extraction is $50,000, the event count gate prevents Signal A from firing.

**Why this matters:** The USD gate alone does not fire when `event_count < min_extraction_events`.
The code at `d07_withdraw_withheld.rs:303` returns `None` if `event_count < min_events` before
any USD check. An attacker who harvests for 30 days and extracts once bypasses Signal A
entirely.

**Signals defeated:** Signal A fully. Signal B only fires if the authority was rotated.

**Cost:** Negligible. `HarvestWithheldTokensToMint` is permissionless and costs ~0.000005 SOL
per call.

**Reward preserved:** Full. The attacker extracts the entire accumulated balance in one
transaction.

**Detection cost:** Add a sub-signal for `HarvestWithheldTokensToMint` rows preceding a single
large `WithdrawWithheldTokensFromMint` event. Alternatively, lower `min_extraction_events` to 1
but add a higher USD floor for single-event cases (e.g., if event_count == 1 AND
cumulative_usd >= 5000, fire at reduced confidence 0.65). The spec's DG-D07-3 documents the
harvest-then-withdraw pattern but marks it as a Phase 3 enhancement. This review escalates it
to a HIGH gap because the single-event bypass is trivially achievable.

**Fixture gap:** No fixture for Harvest-without-Withdraw pattern. See §4.

---

### E-D07-10 — Cross-Authority Simultaneous Harvest (Multi-Mint Drain)

**Description (new):** A single `withdraw_withheld_authority` wallet controls multiple
Token-2022 mints — a common pattern when a deployer launches a suite of related shitcoins
from the same operational wallet. D07 is evaluated per-mint. The attacker drains all mints
simultaneously: 1 extraction event per mint, each below `min_extraction_events = 3`, cumulative
USD across all mints potentially large.

D07 evaluates `(chain, mint)` pairs in isolation. There is no cross-mint aggregation. A wallet
that extracts $900 from 10 different mints ($9,000 total) in one day triggers Signal A on zero
of those mints (1 event each, < 3 minimum). The single authority that orchestrated the drain
is not visible in any individual D07 evaluation.

**Signals defeated:** Signal A fully across all mints. Signal B fires if there were prior
rotations, but each rotation is also below the rapid-rotation bonus.

**Cost:** Negligible (deploying multiple Token-2022 mints is cheap on Solana).

**Reward preserved:** Full. The attacker extracts $9,000 with zero Signal A events.

**Detection cost:** Cross-mint aggregation by authority address: if the same
`withdraw_withheld_authority` wallet has fired Signal A events (or near-threshold events)
across N distinct mints within a 24-hour window, escalate a cross-mint composite alert.
This requires the scoring crate or a separate aggregator to correlate authority addresses
across distinct token evaluations. Phase 3 enhancement. In Phase 2: no mitigation available.

**Cross-ref:** E-D02-8 (multi-actor LP cluster drain — same aggregation gap, different domain).
The fix structure is identical: cross-entity aggregation that the per-entity detector cannot
provide alone.

---

### E-D07-11 — Scheduler-Controlled Extraction (No EOA Signer Visible)

**Description (new):** The deployer deploys an on-chain program that holds the
`withdraw_withheld_authority` keypair as a program-owned PDA and exposes a `crank_fees`
instruction callable by anyone (or by a Clockwork/Jito scheduler). The scheduler calls
`crank_fees` on a schedule — e.g., every 6 hours — which internally calls
`WithdrawWithheldTokensFromAccounts` via CPI. Each CPI execution extracts the accumulated
fees to a beneficiary account encoded in the program's state.

From D07's perspective:
- The `authority` field in `token2022_instructions` is the PDA, not the deployer EOA.
- `authority_match = "unknown"` (PDA vs previously recorded deployer wallet).
- Signal A fires with -0.10 confidence penalty.
- The `destination` field may be a token account controlled by the PDA program, then
  swept automatically to the deployer — one hop removed from evidence.
- Signal B fired once when the deployer originally rotated to the PDA.

The operational signature: regular, precisely timed extraction events at 6-hour intervals, all
with `authority = PDA`, `authority_match = "unknown"`, confidence ~0.50 (below 0.60 Medium).

**Signals defeated:** Signal A confidence reduced. Attribution completely obscured.
Signal B fires once at rotation but `fresh_wallet_bonus` may not apply if PDA was
created >48h before rotation.

**Cost:** High (BPF program deployment), but cost is amortized across indefinite extraction.

**Reward preserved:** Full.

**Phase fix:** When extraction events are precisely periodic (interval variance < 5%), flag
regularity as a programmatic-extraction evidence note. This is a behavioral pattern that
distinguishes bot-driven from human-driven extraction. Requires time-series analysis of
extraction intervals. Phase 3.

---

### E-D07-12 — Pre-Extraction Fee-Config-Authority Suppression Manipulation

**Description (new):** This evasion exploits the `established_protocol_fee_extraction_allowlist_pct`
suppression check combined with the `pool_volume_usd = 0.0_f64` stub.

For a token where `is_established_protocol(meta) = true`, the suppression check at
`d07_withdraw_withheld.rs:362-383` evaluates:
```rust
let pool_volume_usd = 0.0_f64; // TODO(DG-D07-2)
if pool_volume_usd == 0.0_f64 {
    (false, false) // suppression NOT applied
}
```

The current behavior when `pool_volume_usd = 0` is to NOT suppress (fire Signal A regardless).
This is the correct failure mode for security. However, an attacker who can get their token
classified as `is_established_protocol = true` AND ensures the pool has zero ClickHouse swap
volume (e.g., by launching a new pool, or during the first hours after a ClickHouse backfill
lag) can keep `pool_volume_usd = 0` and have Signal A fire, which is the intended behavior.

The real attack surface: **an adversary who compromises the `withdraw_withheld_authority` of
a genuinely established protocol**. With `pool_volume_usd = 0.0_f64` hardcoded:
- The ratio check `extraction_usd / pool_volume_usd` is never computed.
- The suppression is never applied.
- Signal A fires on the established protocol.

This produces a **false positive** for the established protocol case rather than a false
negative. The spec's stated concern (§9) is that full suppression allows a compromised
established protocol authority to extract silently. The stub correctly avoids that. But when
DG-D07-2 is resolved and actual pool volume is queried, an extraction of $850 USD against
a pool with $1,000 USD volume yields ratio = 0.85 < 0.90 — suppressed. An attacker who
knows this can extract exactly $899 (ratio 0.899) every window without Signal A firing.

**Attack:** Compromise an established protocol's `withdraw_withheld_authority`. Extract at
ratio = 0.89 (just below 0.90). Signal A is suppressed indefinitely. Extract $899 every 7
days = ~$46,000/year invisibly.

**Signals defeated:** Signal A fully when DG-D07-2 is resolved and ratio stays below 0.90.
Signal B fires once at the authority change.

**Phase fix:** Lower `established_protocol_fee_extraction_allowlist_pct` to 0.50. A legitimate
protocol would never need to extract more than 50% of its pool volume as fees in a 7-day window
(equivalent to a 50% effective fee rate on 100% of volume — economically impossible at normal
fee rates of 1-5%). The current 0.90 threshold is too permissive by a factor of ~2.

**Cross-ref:** §5 threshold analysis; see recommendation T3.

---

## 3. Threshold Analysis

### Current production thresholds (from `config/detectors.toml`)

| Config key | Current value | Assessment |
|-----------|--------------|------------|
| `min_extraction_events` | 3 | Loose — single-event large extraction invisible (E-D07-9) |
| `min_cumulative_withdraw_usd` | 1000.0 | Adequate for $1K+ pools; inadequate for micro-cap |
| `authority_rotation_window_days` | 30 | Appropriate |
| `min_authority_tenure_days` | 7 | Exploitable — 8-day rotation defeats bonus (E-D07-2) |
| `min_withheld_at_rotation_usd` | 500.0 | Adequate given current fresh-wallet check is broken |
| `fresh_wallet_funding_hours` | 48 | Trivially evaded (fund wallet 49h before rotation) |
| `detection_window_hours` | 168 | Appropriate; cross-window accumulation gap is known |
| `established_protocol_fee_extraction_allowlist_pct` | 0.90 | Too permissive (E-D07-12); moot while stub active |

### Proposed threshold adjustments (Sprint 6 immediate)

**T1 — `min_extraction_events`: Lower from 3 to 1 for single-event large extractions.**

The current threshold requires 3 events. E-D07-9 demonstrates that a single
`WithdrawWithheldTokensFromMint` can drain 100% of the accumulated withheld balance. The
event-count gate defeats Signal A in the single-event case regardless of USD amount.

Proposed fix: two-tier logic. If `event_count >= 3`, current formula applies. If
`event_count == 1`, apply a higher USD floor (`min_single_event_withdraw_usd`, suggested
$5,000) with confidence = 0.65 (below the multi-event base of 0.60+).

Rationale: a single extraction event at $5,000+ is not noise. Legitimate protocol fee
collection happens as a batch sweep, typically once per period — which is exactly the
single-event pattern. The $5,000 floor filters dust-value single-event noise.

**T2 — `fresh_wallet_funding_hours`: Raise from 48 to 24 OR add a minimum-age floor.**

48 hours is the published spec value but it is trivially evaded: fund the new authority
wallet 49 hours before the `SetAuthority` instruction. The bot-trader planning horizon
for a token launch is routinely 72-96 hours. An attacker can fund the authority wallet
during the same planning session with one day's buffer.

Proposed fix (option A): Lower to 24 hours. A 24-hour planning buffer is still achievable
for a sophisticated attacker but requires same-day coordination — more operational friction.

Proposed fix (option B): Add a minimum-age floor — the new authority must have been funded
for fewer than 24 hours AND have no prior transaction history (zero nonce before the first
SOL). This closes the "old wallet repurposed as a temporary extraction wallet" pattern.

Rationale: 48h is consistent with D01 review's disposable-wallet analysis but that was
calibrated for Ethereum, where wallet creation costs more and is more deliberately planned.
On Solana, wallet funding costs ~0.000005 SOL and can be scripted in a single transaction.

**T3 — `established_protocol_fee_extraction_allowlist_pct`: Lower from 0.90 to 0.50.**

The 0.90 threshold is documented as "deliberately permissive" in the spec rationale. But the
rationale assumes normal fee rates. Consider: a Token-2022 mint with 10% fee (1000 bps) on
a pool with $100K/week volume accumulates $10K in fees per week. At the 0.90 threshold,
Signal A is suppressed if extraction is <= $90K in a week with $100K volume. That requires a
>90% fee rate to achieve legitimately. No legitimate protocol runs >50% effective fees.

Lowering to 0.50 means: extraction > 50% of pool volume fires Signal A on established protocols.
Any extraction rate achievable with a fee below 50% at normal volume would be suppressed.
Above 50%, something anomalous is happening regardless of protocol status.

This recommendation is only operationally relevant after DG-D07-2 (pool_volume_usd stub)
is resolved.

---

## 4. Fixture Gap Analysis

### Current fixture coverage (6 fixtures)

| Fixture | Type | Coverage |
|---------|------|---------|
| POS-D07-01 | Synthetic positive | Signal A, combined_with_d01_s2 |
| POS-D07-02 | Synthetic positive | Signal B + A composite |
| POS-D07-03 | Synthetic positive | Repeated extraction, high-confidence |
| NEG-D07-01 | Real negative (PYUSD) | Established protocol, zero fee → InsufficientBaseline |
| NEG-D07-02 | Synthetic negative | No TransferFeeConfig → InsufficientBaseline |
| NEG-D07-03 | Real negative (wSOL) | Legacy SPL → InsufficientBaseline |

### Identified coverage gaps

**Gap FG-1: No real-world rugged Token-2022 withdraw_withheld fixture.**

The spec §16 acknowledges this (POS-D07-03 is synthetic). As of 2026-04-21, no confirmed
live `withdraw_withheld` rug is documented. The following tokens merit manual investigation
as candidate corpus entries for Sprint 6:

1. Tokens found via the query: `api.rugcheck.xyz/v1/tokens/{mint}/report` where
   `rugged = true` AND `transferFee.withdrawWithheldAuthority != null` AND
   `transferFee.transferFee.basisPoints > 1000`. RugCheck's live API returns
   `transferFee` structure but does not expose whether extraction instructions
   were executed. Cross-reference with Solana `getSignaturesForAddress` on the
   `withdraw_withheld_authority` pubkey to find `WithdrawWithheld*` instructions.

2. Targeted search: tokens with `rugged = true` on RugCheck where
   `insiderNetworks` lists addresses that also appear as signers of
   `withdraw_withheld_from_accounts` instructions. This requires cross-joining
   RugCheck metadata with Solana instruction history, achievable via
   Helius `searchAssets` + `getTransactionHistory`.

3. ZachXBT / Rekt News search for "Token-2022 fee drain" or "transfer fee scam Solana"
   post-2025. Sun et al. 2024 taxonomy category "Hidden Fee" (7 of 34 categories)
   confirms the pattern exists in EVM; Solana-native incidents are likely underreported
   due to attribution difficulty.

**Gap FG-2: No legitimate multisig rotation fixture (false positive test).**

A team reorganizing their treasury by moving `withdraw_withheld_authority` from a hot
wallet to a Squads multisig would trigger Signal B. If the hot wallet is fresh (funded
recently for the key ceremony), `fresh_wallet_bonus` fires. The new Squads multisig
might not be in `wallet_funding_events`. Signal B emits at 0.40–0.75.

Proposed fixture: `NEG_D07_004_legitimate_squads_rotation.json`. State: a
jup_strict-verified Token-2022 token with `is_established_protocol = true`, executing a
documented treasury key rotation to a new Squads multisig. Authority rotation is within
30 days. New authority (Squads account) is 3 days old (below 48h threshold? No —
3 days = 72 hours > 48 hours, so `is_fresh_wallet = false`). Expected: Signal B fires
at base 0.40 (no bonuses). This tests the false positive path and confirms the evidence
bundle is sufficient for human review to dismiss the alert.

**Gap FG-3: No boundary test at `established_protocol_fee_extraction_allowlist_pct`.**

No fixture tests Signal A suppression at ratio 0.89 (should suppress) vs 0.91 (should fire).
This boundary is currently moot because `pool_volume_usd = 0.0_f64` is hardcoded, but the
fixture must exist before DG-D07-2 is resolved.

Proposed fixture: `NEG_D07_005_established_protocol_low_ratio.json`. Pool volume = $10,000
in window. Extraction = $8,900 (ratio 0.89 < 0.90). Expected: suppressed.
Companion: `POS_D07_004_established_protocol_high_ratio.json`. Pool volume = $10,000.
Extraction = $9,100 (ratio 0.91 > 0.90). Expected: Signal A fires with
`established_protocol_suppression_skipped_reason = "1"`.

**Gap FG-4: No Harvest-without-Withdraw fixture (E-D07-9).**

Proposed fixture: `SYNTH_D07_NEG_004_harvest_only_no_extract.json`. State: 10
`harvest_withheld_to_mint` rows, zero `withdraw_withheld_from_*` rows. Expected:
Signal A does not fire. Expected error: `MissingDependencyData` if the only rows
are harvest rows (since W1 query filters harvest rows, and the guard at line 187
checks if `w1_result.events.is_empty() && rotation_rows.is_empty()`). Wait —
the guard only returns `MissingDependencyData` if BOTH are empty. If there are harvest
rows (stored in `token2022_instructions`) but no withdrawal rows, the W1 query returns
zero extraction events but the guard does not fire because the table is not empty.
The detector correctly returns no events (Signal A does not fire). This is the correct
behavior and warrants a test confirming it.

---

## 5. Signal B Fresh-Wallet Proxy Attack Surface

### The `wallet_funding_events` population dependency

Query W2 LEFT JOINs `wallet_funding_events` to find the new authority's `first_sol_time`.
The V00007 migration at `/migrations/postgres/V00007__token2022_instructions.sql` creates
the `wallet_funding_events` table with schema `(chain TEXT, wallet TEXT, first_sol_time
TIMESTAMPTZ, first_sol_tx TEXT, PRIMARY KEY (chain, wallet))`.

The table is created but has no confirmed indexer write path. The spec §4 states: "For Signal
B's fresh-wallet detection: the indexer must record when the new authority wallet first
received SOL." The spec's failure mode documentation says: "If the sidecar is absent, emit
`authority_is_fresh_wallet = "0"` with an evidence note that the check was skipped."

**Code behavior when table is empty:** In `evaluate_signal_b` at `d07_withdraw_withheld.rs:436`,
the `new_authority_first_sol_time` field on `AuthorityRotationRow` is populated from the
LEFT JOIN. If `wallet_funding_events` has no rows for the new authority, `first_sol_time = NULL`,
which maps to `rotation.new_authority_first_sol_time = None`. The code at line 436 then
evaluates `if let Some(first_sol) = rotation.new_authority_first_sol_time { ... } else {
(false, -1_i64) }`. Result: `is_fresh_wallet = false`, `fresh_wallet_bonus = 0.0`.

If the indexer never writes to `wallet_funding_events`, Signal B is permanently limited to:
- Base: 0.40
- Rapid rotation only: 0.55
- Never reaches: 0.60 (fresh-wallet only) or 0.75 (both bonuses)

This is a silent capability degradation. No error is logged. No `MissingDependencyData`
error is returned. D07 silently produces lower-confidence Signal B events, and the consumer
has no way to distinguish "this rotation involved a fresh wallet (signal not available)" from
"this rotation involved an established wallet (bonus correctly not applied)."

**Recommended fix:** When `wallet_funding_events` returns NULL for a new authority (LEFT JOIN
miss), emit an evidence note `"wallet_funding_sidecar_unavailable"` at `WARN` log level and
flag the Signal B event with an additional evidence key `"signal_b_fresh_wallet_check_skipped"`.
The code at line 719 already adds this note when `rotation_within_fresh_wallet_hours == -1`,
which is the correct behavior — but it does not log at WARN. The scheduler should monitor for
tokens where Signal B consistently emits `wallet_funding_sidecar_unavailable` and flag the
indexer dependency as unresolved.

### Fresh-wallet timing evasion (trivial)

`fresh_wallet_funding_hours = 48`. An attacker funds the new authority wallet 49 hours before
submitting `SetAuthority`. The check at `d07_withdraw_withheld.rs:439`:

```rust
let fresh = hours >= 0 && hours < fresh_wallet_hours as i64;
```

At `hours = 49` and `fresh_wallet_hours = 48`: `49 < 48` is false. `is_fresh_wallet = false`.
The fresh-wallet bonus does not fire. The attacker plans all token operations >49 hours in
advance — a trivial operational constraint.

**Recommendation:** See T2 in §3.

### Legitimate rotation false positive estimate

A legitimate team conducting a key rotation ceremony — e.g., migrating from a hot wallet to a
hardware wallet or Squads multisig — may fund the new wallet within 48 hours. Common scenarios:
1. Same-day hardware wallet setup: wallet funded day-of. `hours < 48`. Signal B fires at 0.60.
2. Next-day key ceremony: wallet funded 24h before rotation. `hours = 24 < 48`. Signal B fires at 0.60.

Without `wallet_funding_events` being populated, these false positives never fire (bonus = 0).
If the sidecar is properly implemented, legitimate rotations with same-day funding will produce
Signal B at 0.60. For jup_strict tokens, `established_protocol` suppression does NOT apply to
Signal B. The FP rate for Signal B on legitimate treasury key rotations is expected to be
non-negligible once the sidecar is wired up.

---

## 6. Conditional Suppression Exploitation

### The `pool_volume_usd = 0.0_f64` stub

At `d07_withdraw_withheld.rs:364`:

```rust
let pool_volume_usd = 0.0_f64; // TODO(DG-D07-2): query ClickHouse swaps table
```

The established-protocol suppression guard evaluates:
```rust
if pool_volume_usd == 0.0_f64 {
    (false, false) // suppression NOT applied — fire Signal A
}
```

This means: for ALL established protocols, Signal A is never suppressed (pool_volume_usd
is always 0.0, so the `else if let Some(usd) = cumulative_usd { ... }` branch is never
reached). The spec's documented behavior for `pool_volume_usd = 0` is to fire Signal A
regardless (correct failure mode: "fire" rather than "suppress"). So the stub does implement
the spec's intended fallback behavior.

**However**, the consequence is:

1. Any established-protocol Token-2022 token with any extraction activity at the
   `min_extraction_events` and `min_cumulative_withdraw_usd` thresholds fires Signal A.
   This is the correct behavior from a security perspective.

2. The `established_protocol_fee_extraction_allowlist_pct = 0.90` config value is entirely
   non-operational. The config key exists and is parsed, but the ratio check that uses it
   is never reached. This is a dead config value. Any Sprint 6 threshold calibration for
   `established_protocol_fee_extraction_allowlist_pct` calibrates a value that has no
   effect on production behavior until DG-D07-2 is resolved.

3. When DG-D07-2 IS resolved (ClickHouse query added), the suppression suddenly becomes
   operational. The transition from "never suppress established protocols" to "suppress
   when ratio < 0.90" is a behavioral change that consumers depend on. This must be
   explicitly communicated as a breaking change when DG-D07-2 ships.

**Attack scenario post-DG-D07-2:** As detailed in E-D07-12, an attacker who compromises
an established protocol's `withdraw_withheld_authority` can extract at ratio 0.89 per window
indefinitely without Signal A firing. The 0.90 threshold must be lowered to 0.50 before
DG-D07-2 ships.

---

## 7. Code-Level Findings

### CF-1 (MEDIUM) — `pool_volume_usd` hardcoded stub permanently disables established-protocol ratio check

**File:** `crates/detectors/src/d07_withdraw_withheld.rs`
**Line:** 364

```rust
let pool_volume_usd = 0.0_f64; // TODO(DG-D07-2): query ClickHouse swaps table
```

**Impact:** The `established_protocol_fee_extraction_allowlist_pct` threshold is entirely
non-operational. Signal A fires on established protocols regardless of extraction/volume ratio.
This is a false positive risk (not a false negative risk), but it also means the suppression
logic is untested. When DG-D07-2 is resolved and real pool volume is substituted, the
suppression may introduce false negatives.

**Recommended fix:** Change the failure mode documentation to make the stub explicit:
```rust
// DG-D07-2: pool_volume_usd stub — always 0.0 until ClickHouse swaps query is implemented.
// When pool_volume_usd = 0.0, suppression is NOT applied (fire Signal A).
// This is the correct security-safe failure mode per spec §13.
// WARNING: established_protocol_fee_extraction_allowlist_pct is non-operational until
// this stub is replaced. Track as accepted risk ref DG-D07-2.
let pool_volume_usd = 0.0_f64;
```

Add a config-pin test that confirms this behavior and fails loudly if
`pool_volume_usd` is ever non-zero without a corresponding test fixture.

---

### CF-2 (LOW) — `authority_tenure_days` always returns -1 in Signal A evidence

**File:** `crates/detectors/src/d07_withdraw_withheld.rs`
**Line:** 394

```rust
authority_tenure_days: -1, // DG-D07-1: not available without set_authority history cross-ref
```

**Impact:** The evidence key `withdraw_withheld/authority_tenure_days` is always `-1` in Signal A
events. The spec §10 specifies this key should contain "Days the current authority has held the
role; -1 if no rotation history available." With the Signal B rotation query already run (W2),
the tenure information is available in `rotation_rows` when Signal B also fires. For the
composite case (Signal A + Signal B), `authority_tenure_days` could be populated from
`signal_b_result.prev_authority_tenure_days`.

**Impact on consumers:** Consumers that filter or sort on `authority_tenure_days` always see
-1. Human reviewers cannot assess authority tenure from Signal A evidence alone.

**Recommended fix (Advisory):** When `signal_b_result.is_some()`, populate
`authority_tenure_days` from `sb.rotation_row`'s tenure calculation. The tenure is available
in the `SignalBResult` struct as `prev_authority_tenure_days`. Pass it through to
`build_signal_a_event` and use it when `sb.is_some()`.

---

### CF-3 (LOW) — `ingested_at: ctx.observed_at` vs `Utc::now()` — determinism verified

**File:** `crates/detectors/src/d07_withdraw_withheld.rs`
**Lines:** 636, 746

```rust
ingested_at: ctx.observed_at,
```

Both `build_signal_a_event` and `build_signal_b_event` use `ctx.observed_at` — a context
value passed in externally — rather than calling `Utc::now()` directly. This is the correct
implementation. Reviews 0002 (D02) and 0003 (D04) both flagged `Utc::now()` inside
`make_event()` as a CRITICAL determinism violation. D07 correctly avoids this.

**Verdict:** No violation. This finding is a positive confirmation of correct implementation.
Document as compliance with CLAUDE.md reproducibility rule #5.

---

### CF-4 (LOW) — No `BTreeMap` enforcement visible in `Evidence` construction

**File:** `crates/detectors/src/d07_withdraw_withheld.rs`
**Lines:** 537-625 (Signal A), 668-735 (Signal B)

The spec §20 acceptance checklist item states: "`BTreeMap` used for all intermediate
collections contributing to `Evidence::metrics` (determinism contract per CLAUDE.md
reproducibility rule)." The `Evidence::new()` builder pattern is used throughout
`build_signal_a_event` and `build_signal_b_event`. Whether the underlying `Evidence`
struct uses a `BTreeMap` or `HashMap` for metrics storage is a question of the
`mg_onchain_common::anomaly::Evidence` implementation, which is not inspected in this
review (beyond the scope of the `crates/detectors/` advisory constraint).

**Risk:** If `Evidence::metrics` uses `HashMap` internally, `Evidence::with_metric()`
inserts are non-deterministic in iteration order. While this does not affect confidence
computation (metrics are accumulated, not iterated during computation), it may affect
downstream hashing or serialization of `AnomalyEvent` structs.

**Recommended check (Advisory):** Confirm `mg_onchain_common::anomaly::Evidence` uses
`BTreeMap<String, Decimal>` for `metrics`. If it uses `HashMap`, the evidence serialization
order is non-deterministic, violating CLAUDE.md rule #5.

---

### CF-5 (LOW) — Decimal-to-f64 conversion via `to_string().parse()` has an implicit panic path

**File:** `crates/detectors/src/d07_withdraw_withheld.rs`
**Lines:** 327, 344, 368

```rust
let usd_f64: f64 = usd.to_string().parse().unwrap_or(0.0_f64);
```

The `Decimal::to_string()` → `str::parse::<f64>()` round-trip is the correct approach for
converting `rust_decimal::Decimal` to `f64` for ratio math (ratios are not monetary amounts
so f64 precision loss is acceptable here). However, the `unwrap_or(0.0_f64)` fallback
silently substitutes 0.0 if the parse fails. A `Decimal` value that cannot be represented
as a float (e.g., `Decimal::MAX = 79,228,162,514,264,337,593,543,950,335`) produces an
infinite or NaN f64, which `parse()` from a string would return as `f64::INFINITY` or
an error. On parse error, `unwrap_or(0.0)` produces 0.0, which makes `usd_gate_met = true`
only if the minimum is 0 — but since `min_usd = 1000.0 > 0.0`, `usd_gate_met = false`.
Signal A would not fire on a maximum-value extraction amount. This is an unlikely edge case
but should be explicitly handled.

**Recommended fix (Advisory):** If `usd_f64.is_nan() || usd_f64.is_infinite()`, emit a
`tracing::warn!` and treat as USD unavailable (fallback to event-count-only). No panic path.

---

## 8. Worst-Case Crafted Token — $50K Extraction Over 30 Days Without Triggering D07

This section constructs a Token-2022 mint that extracts $50,000 over 30 days while remaining
below D07's Signal A and Signal B thresholds at every evaluation window.

### Token configuration

```json
{
  "mint": "CRAFT_D07_EVASION_MINT",
  "token_program": "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
  "transfer_fee": {
    "fee_bps": 5000,
    "max_fee_raw": "999999999999",
    "transfer_fee_config_authority": "DeployerWalletPublic",
    "withdraw_withheld_authority": "ExtractorPDA_OwnedByCustomProgram"
  },
  "verification": {
    "jup_verified": false,
    "jup_strict": false
  },
  "rugcheck_score": 90
}
```

**Design choices:**
- `fee_bps = 5000` (50% fee) to accumulate value rapidly.
- `withdraw_withheld_authority` is a PDA owned by a custom program. `authority_match = "unknown"`
  in all Signal A events. Confidence penalty: -0.10.
- Custom program's upgrade authority is a burner wallet (renounced-looking).

### Authority rotation calendar

No rotations are executed. Signal B never fires. The custom program PDA is set as authority
at mint creation time (no `SetAuthority` instruction is needed if the PDA is the initial
authority).

### Per-day extraction instruction stream

The custom program (scheduler-controlled via Clockwork) calls `crank_fees` every 9 days.
Each `crank_fees` calls `WithdrawWithheldTokensFromMint` once via CPI.

| Day | Instruction | amount_usd | Window events (7d lookback) | Signal A? |
|-----|-------------|------------|----------------------------|-----------|
| 0 | Deploy mint, set PDA as withdraw_withheld_authority | 0 | 0 | No |
| 9 | WithdrawWithheldFromMint (CPI via crank) | $1,650 | 1 event | No (count < 3) |
| 18 | WithdrawWithheldFromMint (CPI via crank) | $1,650 | 1 event (day 9 outside window) | No (count < 3) |
| 27 | WithdrawWithheldFromMint (CPI via crank) | $1,650 | 1 event (day 18 outside window) | No (count < 3) |

Three extractions, 9 days apart, each in a separate 7-day detection window. Each window has
event_count = 1, below `min_extraction_events = 3`. Signal A never fires.

Total extracted in 27 days: $4,950. To reach $50,000, the attacker runs 30 extraction events
over 270 days at this cadence.

**Acceleration variant:** If the token's trading volume is higher (e.g., $2M/week in swaps
at 50% fee), each extraction can be $10,000 USD at 9-day intervals. 5 extractions over 45 days
= $50,000, with Signal A never firing. The only detectable signal is D01 Signal S2 firing at
listing time (fee_bps = 5000 > 3000).

### Why D07 misses this

1. `min_extraction_events = 3` — 9-day spacing ensures each 7-day window has at most 1 event.
2. `authority_match = "unknown"` — PDA via CPI reduces confidence by 0.10, but this only
   matters if Signal A fires in the first place.
3. No Signal B — no `SetAuthority` instruction ever executed.
4. `wallet_funding_events` unpopulated — no fresh-wallet bonus possible regardless.

### Mitigation requirement

This attack requires resolving E-D07-9 (single-event large extraction) by adding a
single-event USD floor and lowering `min_extraction_events` to 1 for high-USD cases.
A single extraction of $1,650 at event_count = 1 might be below the proposed $5,000
floor. The attacker wins unless the single-event floor is set at or below $1,650.

Alternatively, a 30-day cumulative query (DG-D07-4) would accumulate 3 × $1,650 = $4,950
in the first 27 days — below a 30-day threshold of $3,000 only if set below $4,950.
At `min_cumulative_withdraw_usd_30d = 3,000`, this attack is detected at day 27.

---

## 9. E-D02-11 Gap Closure Assessment

**Original gap (E-D02-11):** `WithdrawWithheld*` instructions produce no LP Burn rows.
D02 Signal A sees zero qualifying pool_events rows. D02 Signal B sees unchanged LP burn
percentage. D06 Signals B and C see no zero-address Transfer. The extraction is entirely
invisible to all D01–D06 detectors (with D01 S2 as only static precondition).

**D07 closure status:** PARTIALLY CLOSED.

D07 correctly introduces a dedicated instruction-level signal (Signal A) that fires on
confirmed `WithdrawWithheld*` events regardless of LP state. This closes the core blind spot
from E-D02-11 for the case where:
- The `token2022_instructions` table is populated (indexer running).
- `event_count >= 3` within the 7-day window.
- `cumulative_usd >= $1,000` within the 7-day window.
- The authority is the recorded `withdraw_withheld_authority` (or Signal A fires with
  reduced confidence for unknown authority).

**Residual gaps that E-D02-11 considered fully closed but D07 does not address:**

1. **Single-event large extraction (E-D07-9):** A single `WithdrawWithheldFromMint` for
   $50,000 does not fire Signal A. This is a fundamental gap in D07's event-count gate.
   E-D02-11 does not contemplate this case because it pre-dates D07's design.

2. **Cross-mint simultaneous extraction (E-D07-10):** The original E-D02-11 analysis
   described a single-token attack. Multi-mint coordinated extraction below per-token
   thresholds is not covered.

3. **Scheduler-controlled extraction via CPI (E-D07-11):** The original E-D02-11 assumed
   the authority is an EOA. CPI-based extraction via a custom program reduces Signal A
   confidence to potentially below consumer thresholds.

4. **`wallet_funding_events` unpopulated (§5):** Signal B's fresh-wallet check is
   non-operational if the indexer does not write to the table. E-D02-11 pre-dates this
   implementation gap.

**Verdict:** E-D02-11 is CLOSED for the baseline case (direct extraction by a recorded
authority, 3+ events, $1,000+ USD). The original D02 review's detection cost estimate
("a cross-detector linkage is needed") is fulfilled by D07's design. Three residual Phase 3
gaps remain (E-D07-9, E-D07-10, E-D07-11) that were not in scope for D02's E-D02-11.
These should be tracked in ROADMAP.md as Phase 3 backlog items, not as Sprint 6 blocking items.

---

## 10. Sign-Off Verdict

**Block.** Two conditions must be resolved before Sprint 6 exit:

**Blocking condition B1:** The `pool_volume_usd = 0.0_f64` stub behavior must be explicitly
documented as named accepted risk `ACCEPTED-RISK-D07-01` in the codebase. Currently there is
only a `// TODO(DG-D07-2)` comment. The documentation must state:
- What the current behavior is (Signal A always fires for established protocols, suppression
  never applied).
- What the behavior will be after DG-D07-2 is resolved.
- That `established_protocol_fee_extraction_allowlist_pct` is a non-operational config value.
- That `established_protocol_fee_extraction_allowlist_pct` MUST be lowered to 0.50 before
  DG-D07-2 ships.

**Blocking condition B2:** The `wallet_funding_events` indexer write path must be confirmed
as either (a) implemented and tested, or (b) documented as `ACCEPTED-RISK-D07-02` with
explicit acknowledgment that Signal B `fresh_wallet_bonus` is permanently 0.0 until resolved.
The current code emits a note `"wallet_funding_sidecar_unavailable"` when the check misses,
but this requires `rotation_within_fresh_wallet_hours == -1` to be true, which it is (line 415:
`(false, -1_i64)` when `new_authority_first_sol_time = None`). So the note IS emitted. What
is missing is a `WARN`-level log in the scheduler to surface the dependency gap.

**Ship-with-caveat items (documented, not blocking):**
- E-D07-9 (single-event large extraction): document as Phase 3 gap in ROADMAP.md.
- E-D07-10 (cross-mint simultaneous extraction): document as Phase 3 gap.
- E-D07-11 (scheduler-controlled extraction): document as Phase 3 gap.
- CF-2 (`authority_tenure_days` always -1 in Signal A): acceptable for MVP, document as
  a known limitation.
- CF-5 (Decimal-to-f64 fallback): acceptable for MVP, very unlikely edge case.

---

## 11. REFERENCES.md Rows Added

The following rows are proposed additions to `REFERENCES.md`:

| Mechanism | Signal / Formula | Source | Used In | Verified Against |
|-----------|-----------------|--------|---------|-----------------|
| Harvest-without-Withdraw single-event extraction bypass | `HarvestWithheldTokensToMint` (permissionless) consolidates withheld balance into mint; single `WithdrawWithheldFromMint` extracts full balance; `min_extraction_events = 3` gate defeats Signal A when event_count = 1 | Solana Token-2022 instruction spec, https://spl.solana.com/token-2022/extensions#transfer-fees; E-D07-9 (this review, 0004) | D07 Signal A gap; proposed `min_single_event_withdraw_usd` two-tier gate; Phase 3 DG-D07-3 extension | Security review 0004, 2026-04-21 |
| Cross-mint simultaneous authority drain (multi-mint fragmentation) | Single `withdraw_withheld_authority` controls N Token-2022 mints; drains each at event_count = 1 per mint; per-mint event gate defeats Signal A on all mints simultaneously; cross-entity aggregation not available in Phase 2 | Design gap surfaced in review 0004 §E-D07-10; analogous to D02 E-D02-8 multi-actor LP cluster drain (Alhaidari et al. 2025 SolRPDS) | D07 Signal A cross-mint aggregation gap; Phase 3 scoring-crate authority-cluster signal | Security review 0004, 2026-04-21 |
| Squads v4 internal member rotation as D07 Signal B evasion | Token-2022 `SetAuthority` rotates to a Squads multisig once; Signal B fires once. Subsequent Squads-internal member rotations (new Squads instruction, not Token-2022 SetAuthority) are invisible to D07 Signal B. Authority effectively cycles through disposable wallets without any D07 observation. | Squads Protocol v4 documentation, https://docs.squads.so/squads-mpc-overview; evasion design: review 0004 §E-D07-5 | D07 Signal B gap (Squads internal rotation); Phase 3 Squads instruction indexing requirement | Security review 0004, 2026-04-21 |
| Scheduler-controlled CPI extraction (programmatic drain, no EOA visible) | On-chain program holds withdraw_withheld_authority PDA; scheduler calls crank_fees instruction which executes WithdrawWithheld* via CPI; authority in evidence is PDA not deployer EOA; Signal A confidence reduced by 0.10 unknown-authority penalty; extraction runs indefinitely below consumer alert thresholds | Clockwork/Jito scheduler documentation; Solana PDA mechanics; evasion design: review 0004 §E-D07-11 | D07 Signal A authority attribution gap; Phase 3 programmatic-extraction periodicity detection | Security review 0004, 2026-04-21 |
| `pool_volume_usd` stub non-operational behavior for established-protocol suppression | `pool_volume_usd = 0.0_f64` hardcoded at d07_withdraw_withheld.rs:364 means established_protocol_fee_extraction_allowlist_pct is a non-operational config value; suppression never applied in MVP; must lower threshold to 0.50 before DG-D07-2 ships to prevent ratio-straddling attack on compromised established protocols | Code inspection: crates/detectors/src/d07_withdraw_withheld.rs line 364; spec: docs/designs/0012 §13 DG-D07-2; evasion: review 0004 §E-D07-12 | D07 established-protocol suppression path; DG-D07-2 resolution prerequisite | Security review 0004, 2026-04-21 |
