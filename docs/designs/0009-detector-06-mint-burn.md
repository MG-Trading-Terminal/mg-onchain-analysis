# Design 0009 — Detector D06: Mint / Burn Anomaly

**Date:** 2026-04-21
**Status:** Draft
**Author:** onchain-analyst agent
**ADR refs:**
- ADR 0001 §D4 — `AnomalyEvent { confidence, severity, evidence }` output contract
- ADR 0001 §D5 — MVP detector #6 (Mint/Burn Anomaly), priority S
- ADR 0001 §D7 — fixture corpus bootstrapping from RugCheck rugged=true / jup_verified
- ADR 0002 — Postgres-only storage; all queries in PostgreSQL dialect
- ADR 0003 — self-sovereign infrastructure; no 3rd-party runtime dependencies
**Trait ref:** `docs/designs/0003-detector-trait.md` — implements `Detector` trait, uses `DetectorContext`;
  `fetch_rows` / `compute` split for testability
**Query ref:** `docs/queries/d06_mint_burn_anomaly.sql` — Query 1 (unexpected mints) + Query 2 (unexpected burns)
**Suppression ref:** `crates/detectors/src/token_status.rs` — `is_established_protocol` predicate
**Detector ID:** `mint_burn_anomaly`

---

## 1. Context

Hidden mint is the primary root-cause mechanism in the largest published corpus of scam tokens.
Xia et al. (2021) catalogued approximately 10,000 scam tokens on Uniswap V2; the hidden mint — deployer
retaining mint authority and inflating supply after holders buy in — was the leading attack vector. Sun
et al. (2024) refined this into a 34-category taxonomy, classifying "hidden mint" and "hidden owner" as
distinct but often co-occurring root causes, responsible for a disproportionate share of confirmed rug
events. RugCheck exposes `mintAuthority` and `is_mintable` as its primary risk flags, confirming that
practitioners have independently converged on the same signal.

On Solana, the attack is particularly cheap to execute. The mint authority is a single account field on
the mint account itself. Any wallet holding that keypair can call `spl-token mint` at any time. Token-2022
extends this surface: `withdraw_withheld` on `TransferFeeConfig` is a distinct but related value-extraction
path (documented in E-D02-11 of `docs/reviews/0002-d02-rug-pull-evasions.md`).

D06 has three design constraints:

1. **Static versus dynamic detection.** Mint authority presence is a structural risk — it enables the
   attack even before a mint event fires. Signal A (static) fires immediately at low confidence. Signal B
   (event-based) fires only when a mint or burn event is observed. Signal C (composite) requires both
   active authority AND observed cumulative supply growth AND non-LP recipients.

2. **Established-protocol asymmetry (P4-0 contract).** USDC is a regulated stablecoin with active mint
   authority (Circle mints on demand). Signal A MUST be dampened (not suppressed) for established protocols
   to preserve observability while reducing noise. Signals B and C MUST be fully suppressed because treasury
   minting operations on established protocols are not anomalous.

3. **Token-2022 `withdraw_withheld` coverage boundary.** E-D02-11 documents a value-extraction path that
   does not produce a Transfer from/to zero address. D06's mint/burn heuristics cannot cover this path
   directly. The cross-detector coverage matrix in §10 documents precisely what D06 covers and what is a
   confirmed gap for D07 in Phase 3.

This spec is the implementation contract for the P4-3 developer task. The developer implements
`crates/detectors/src/d06_mint_burn.rs` without modifying any frozen type in `crates/common`.

---

## 2. Signal Taxonomy

D06 produces one to two `AnomalyEvent`s from a single `evaluate()` call:

| Signal | When it fires | Confidence band | Severity range | Event-based? |
|--------|--------------|-----------------|----------------|--------------|
| A — Active mint authority (static) | `TokenMeta.mint_authority.is_some()` AND token age > `mint_authority_grace_period_days` | 0.10–0.20 | Info | No — state-based |
| B — Supply change event (event-based) | Single Transfer from/to zero address ≥ `supply_change_threshold_pct` of circulating supply; recipient NOT in known LP set | 0.55–0.85 | Medium–High | Yes — event-based |
| C — Hidden mint pattern (composite) | Mint authority active AND cumulative supply increase ≥ `hidden_mint_cumulative_pct` within `hidden_mint_window_days` AND token age ≥ `min_token_age_days_for_hidden_mint` AND mints went to non-LP recipients | 0.75–0.95 | High–Critical | Composite |

**Established-protocol handling:**

| Signal | `is_established_protocol` = true | `is_established_protocol` = false |
|--------|----------------------------------|-----------------------------------|
| A | Dampened: `conf = base * established_protocol_confidence_dampening`; emit `signal = "info_suppressed"` | Full confidence: `conf = 0.20` |
| B | Fully suppressed; no event emitted | Fires normally |
| C | Fully suppressed; no event emitted | Fires normally |

---

## 3. Inputs

### Static (from `ctx.registry.enrich()`)

- `TokenMeta.mint_authority: Option<Address>` — presence = active mint authority
- `TokenMeta.circulating_supply_raw: Option<u128>` — denominator for supply change pct; fallback to `total_supply_raw` with evidence annotation
- `TokenMeta.total_supply_raw: u128` — fallback supply denominator
- `TokenMeta.verification.jup_strict: bool` — part of `is_established_protocol` predicate
- `TokenMeta.verification.jup_verified: bool` — part of `is_established_protocol` predicate
- `TokenMeta.rugcheck_score: Option<u32>` — part of `is_established_protocol` predicate
- `TokenMeta.detected_at: Option<DateTime<Utc>>` — used to compute token age
- `TokenMeta.transfer_fee: Option<TransferFeeConfig>` — specifically `withdraw_withheld_authority` for Signal B `supply_redirection_anomaly` subvariant (see §10)
- `TokenMeta.markets: Vec<MarketInfo>` — provides `pool_address` list for known-LP exclusion

### Event-based (from `ctx.store` — PostgreSQL `transfers` table)

Query 1 (Signal B mints): transfers where `from_address = zero_address` AND `to_address != zero_address` AND `to_address` not in known LP set AND `amount_raw / circulating_supply_raw >= supply_change_threshold_pct`.

Query 2 (Signal B burns): transfers where `to_address = zero_address` AND `from_address != zero_address` AND `from_address` not in known LP set AND `amount_raw / circulating_supply_raw >= supply_change_threshold_pct`.

Query 3 (Signal C cumulative aggregate): SUM of mint `amount_raw` across the `hidden_mint_window_days` window, grouped by recipient address, for mints to non-LP recipients. Returns cumulative pct and distinct recipient count.

### Sidecar (optional, from `holder_classifications`)

- `recipient_holder_kind` — classification of mint recipient (`deployer`, `insider`, `dex_pool`, `vesting_contract`, `unknown`). Used in evidence, not in gate logic. If sidecar is absent for an address, emit `recipient_holder_kind = "unknown"`.

### Context parameters

- `ctx.window.start`, `ctx.window.end` — observation window for Signal B and C queries
- `ctx.zero_address` — chain-canonical zero address (differs between Solana and EVM)

---

## 4. Baseline

**Signal A:** No rolling baseline required. Structural check on a binary field. The `mint_authority_grace_period_days` threshold separates newly-launched tokens (where revocation is expected but not yet done) from mature tokens where continued presence is anomalous.

**Signal B:** Per-token supply denominator (`circulating_supply_raw` or `total_supply_raw`). "Normal" = fixed supply; a single-event supply change ≥ 5% is anomalous by definition for fixed-supply tokens. For emission-schedule tokens, scheduled recipients are excluded via `known_emission_recipients` parameter passed to the query. The baseline choice is per-token and regime-stable because it is normalized to token supply, not to a USD or volume metric.

**Signal C:** Per-token rolling supply delta over `hidden_mint_window_days`. The 30-day cumulative threshold (20%) is calibrated against Sun et al. (2024) "hidden mint" patterns. Normal legitimate projects that mint tokens post-launch for treasury or liquidity purposes either: (a) revoke authority immediately after the mint, (b) mint to LP contracts (excluded by the non-LP gate), or (c) follow a disclosed emission schedule (excluded by `known_emission_recipients`). Cumulative 20% growth to non-LP addresses over 30 days without authority revocation has no known legitimate explanation for fixed-supply tokens.

---

## 5. Signal Definitions

### Signal A — Active Mint Authority (Static)

**Precise signal definition:** `TokenMeta.mint_authority.is_some()` AND token age in days exceeds `mint_authority_grace_period_days` (default 7).

**Rationale for grace period:** Legitimate new projects often deploy with mint authority to allow genesis minting (airdrops, LP seeding) and revoke it within the first week. Sun et al. (2024) observe that rugged tokens almost never revoke mint authority, while non-rugged tokens revoke within the first few days. The 7-day grace period prevents alerting on every new token.

**Token age computation:**
- If `TokenMeta.detected_at.is_some()`: `token_age_days = (ctx.window.end - detected_at).num_days()`
- If `TokenMeta.detected_at.is_none()`: skip grace period check; fire Signal A regardless (conservative; we lose nothing by being alert on a token of unknown age).

**Established-protocol handling:**
- `is_established_protocol(meta) = true`: emit with `confidence = 0.20 * established_protocol_confidence_dampening (default 0.5) = 0.10`, severity `Info`, signal key `"info_suppressed"`. Do NOT suppress entirely — preserves observability for audit.
- `is_established_protocol(meta) = false`: emit with `confidence = 0.20`, severity `Info`.

Signal A does NOT co-fire with Signal C. If Signal C fires, Signal A is redundant and should be omitted from the output (`Vec<AnomalyEvent>`) to avoid double-counting in the scoring crate. If Signal C does not fire, Signal A MAY fire alone.

---

### Signal B — Supply Change Event (Event-based)

**Precise signal definition:** A `Transfer` event with `from_address = zero_address` (mint) or `to_address = zero_address` (burn) where `amount_raw / supply_denominator >= supply_change_threshold_pct` AND `recipient_address` (for mints) or `burner_address` (for burns) is NOT in `known_lp_addresses` AND NOT in `known_emission_recipients`.

**Supply denominator selection:**
1. `circulating_supply_raw` if `Some` and non-zero — preferred.
2. `total_supply_raw` if circulating is `None` or zero — fallback. Emit `supply_base = "total"` in evidence and `tracing::warn!`.
3. If both are zero: return `Err(DetectorError::InsufficientBaseline)` — cannot normalize.

**Non-LP recipient signal weight:** When the mint recipient is NOT a known LP contract, `non_lp_recipient_signal_weight` (default 0.30) is added to the confidence formula. When the recipient IS a known LP, Signal B is suppressed entirely (the mint is routine LP provisioning, not anomalous).

**Confidence formula (Signal B):**

```
conf_raw = 0.55 + (supply_change_pct - supply_change_threshold_pct) / supply_change_threshold_pct * 0.30
           + (if recipient_not_lp { non_lp_recipient_signal_weight } else { 0.0 })
conf = min(0.85, conf_raw)
```

At threshold exactly (supply_change_pct = 0.05): `conf = 0.55 + 0.0 + 0.30 = 0.85` (non-LP) or `0.55 + 0.0 = 0.55` (LP excluded, so signal B does not fire for LP; this case never reaches the formula).
At threshold exactly (non-LP): `conf = min(0.85, 0.55 + 0.0 + 0.30) = 0.85`.
At 2x threshold (supply_change_pct = 0.10, non-LP): `conf = min(0.85, 0.55 + 0.30 + 0.30) = 0.85` — saturates.
At 2x threshold (supply_change_pct = 0.10, LP): signal suppressed.

Note: when the non-LP recipient gate suppresses the event (LP recipient), Signal B does NOT fire at all. The gate is a hard exclusion, not a confidence deduction.

**Severity mapping from confidence:**
- `0.55 ≤ conf < 0.65` → `Medium`
- `0.65 ≤ conf < 0.80` → `High`
- `0.80 ≤ conf ≤ 0.85` → `High` (capped; Critical reserved for Signal C)

**Burn events:** The same formula applies to burns (Query 2). Burn recipients are typically zero address; the anomaly check is on the `burner` (from_address): if from_address is a known LP contract, the burn is routine LP activity and is suppressed. If from_address is a non-LP address burning a significant fraction of supply, it is anomalous (possible burn-then-remint laundering setup — see §8 Evasion E6).

**Established-protocol suppression:** If `is_established_protocol(meta) = true`, Signal B is fully suppressed; no event emitted. USDC minting $1B to a Coinbase address should not fire at high confidence.

---

### Signal C — Hidden Mint Pattern (Composite)

**Precise signal definition:** ALL of the following must hold simultaneously:
1. `TokenMeta.mint_authority.is_some()` — authority is still active
2. Cumulative supply increase from non-LP mint events within the last `hidden_mint_window_days` (30d) ≥ `hidden_mint_cumulative_pct` (0.20 = 20%) of supply denominator
3. Token age ≥ `min_token_age_days_for_hidden_mint` (14 days) — prevents firing on genesis-phase tokens where large mints are expected
4. At least one of the accumulating mint events went to a non-LP recipient

**Rationale:** Sun et al. (2024) define the "hidden mint" category precisely as: mint authority retained + supply inflated post-genesis + recipients are deployer-controlled wallets. The 20% cumulative threshold over 30 days is deliberately permissive (lower than a single-event threshold) because the attack can be spread across many small mints. The 14-day minimum age guards against legitimate genesis-phase multi-mint activity.

**Confidence formula (Signal C):**

```
conf_raw = 0.75 + (cumulative_pct - hidden_mint_cumulative_pct) * 1.0
conf = min(0.95, conf_raw)
```

At cumulative exactly 20% (threshold): `conf = 0.75`.
At cumulative 30%: `conf = 0.75 + 0.10 = 0.85`.
At cumulative 40%: `conf = 0.75 + 0.20 = 0.95` — saturates.

The formula saturates fast because by 40% cumulative non-LP inflation the evidence is overwhelming.

**Severity mapping from confidence:**
- `0.75 ≤ conf < 0.85` → `High`
- `0.85 ≤ conf ≤ 0.95` → `Critical`

**Established-protocol suppression:** If `is_established_protocol(meta) = true`, Signal C is fully suppressed; no event emitted.

**Co-fire with Signal A:** When Signal C fires, Signal A MUST be omitted from the output (Signal C strictly subsumes Signal A's information content and adds the event-based evidence).

---

## 6. Threshold Table

| Config key | Default | Derivation | Prior art |
|------------|---------|------------|-----------|
| `mint_anomaly.mint_authority_grace_period_days` | 7 | Observed in corpus: non-rugged tokens revoke within 1–7 days; rugged tokens almost never revoke. 7 days is conservative to cover legitimate delayed revocations. | Sun et al. 2024 §4 "hidden mint" pattern; RugCheck `mintAuthority` signal rationale |
| `mint_anomaly.supply_change_threshold_pct` | 0.05 | research/02-detection-methodology.md §9 working default. No published Solana-specific calibration. 5% in one event is anomalous for fixed-supply tokens. | Xia et al. 2021 (hidden mint identification); research/02-detection-methodology.md §9 |
| `mint_anomaly.hidden_mint_cumulative_pct` | 0.20 | Sun et al. 2024 "hidden mint" category: supply inflation is the defining characteristic. 20% cumulative threshold catches fragmentation evasions (Evasion E2) while providing 4× headroom above the per-event threshold. Unverified-heuristic; calibrate from labelled corpus in Sprint 5. | Sun et al. 2024 §4; no published numeric threshold for cumulative variant |
| `mint_anomaly.hidden_mint_window_days` | 30 | 30-day rolling window is regime-stable and long enough to accumulate evidence from slow-trickle hidden mints while short enough to remain actionable. Consistent with D04/D05 observation windows. Unverified-heuristic. | research/02-detection-methodology.md §Cross-cutting C |
| `mint_anomaly.min_token_age_days_for_hidden_mint` | 14 | 14 days gives genesis-phase minting activity (airdrops, LP seeding, team allocation) time to settle before Signal C fires. Shorter windows produce FPs on legitimate multi-round token launches. Longer windows reduce coverage of early-stage rugs. Unverified-heuristic. | No prior art; design derivation from D01 grace period analogy |
| `mint_anomaly.established_protocol_confidence_dampening` | 0.5 | Halves Signal A confidence for established protocols (USDC, PYUSD). Rationale: Signal A MUST remain observable for audit purposes; full suppression would hide audit trail for regulated stablecoins. 0.5 halving is consistent with D03's sidecar-exclusion partial attenuation pattern. Unverified-heuristic. | token_status.rs P4-0 design; D02 §14 asymmetric contract |
| `mint_anomaly.non_lp_recipient_signal_weight` | 0.30 | Non-LP recipient is the key distinguishing feature between a hidden mint (attacker benefits) and a legitimate treasury/LP mint (protocol benefits). Weight 0.30 elevates borderline Signal B confidence from 0.55 to 0.85 when recipient is non-LP. Calibrated to reach the High severity band. Unverified-heuristic; calibrate from labelled corpus. | Sun et al. 2024 "hidden mint" recipient analysis |

All thresholds are published in `config/detectors.toml` under `[mint_burn_anomaly.*]`. See §11 Config Stub for the full TOML expansion of the stub from Sprint 4.

---

## 7. Confidence Composition Summary

| Signal | Base | Upper cap | Severity band |
|--------|------|-----------|---------------|
| A (established_protocol=false) | 0.20 | 0.20 (fixed) | Info |
| A (established_protocol=true, dampened) | 0.10 | 0.10 (fixed) | Info |
| B (non-LP recipient) | 0.85 = min(0.85, 0.55 + (diff/threshold)*0.30 + 0.30) | 0.85 | Medium–High |
| B (LP recipient) | suppressed | — | — |
| C | min(0.95, 0.75 + (cumulative_pct - 0.20) * 1.0) | 0.95 | High–Critical |

**Priority ordering:** When multiple signals fire in a single evaluation:
1. Signal C fires → emit Signal C event; omit Signal A (subsumes it). Signal B MAY co-fire as a separate event if a distinct single-event anomaly also exceeded threshold within the window.
2. Signal B fires, Signal C does not → emit Signal B; emit Signal A if it independently fires and Signal C has not.
3. Signal A fires alone → emit Signal A.

A single `evaluate()` call may return at most two `AnomalyEvent`s: one for Signal C (or B), and one for Signal A — but Signal A is omitted when Signal C fires. This preserves the scoring crate's ability to weight independent signals.

---

## 8. Adversarial Evasions

The following evasion patterns are documented in order of attacker cost.

### E-D06-1 — Mint to Jupiter Aggregator as Apparent LP

**Description:** The attacker mints tokens to their own wallet through a Jupiter swap route. The immediate recipient of the mint Transfer event appears to be a Jupiter program account or aggregator intermediary, not the deployer's wallet. The Jupiter Aggregator program (`JUP4Fb2cqiRUcaTHdrPC8h2gNsA2ETXiPDD33WcGuJB`) is NOT in the known LP set. Signal B's "non-LP recipient" gate fires, but the classification of the recipient as `unknown` muddies the evidence.

**Signals affected:** Signal B's `recipient_is_known_lp` = "0" fires correctly, but `recipient_holder_kind` = "unknown" rather than "deployer" — reduces evidence quality.

**Attacker cost:** Low. Jupiter aggregation is a standard operation.

**Mitigation:** Sidecar `holder_classifications` should classify Jupiter program accounts as `dex_aggregator` rather than `unknown`. D06 evidence accurately emits `recipient_holder_kind = "dex_aggregator"` and the scoring crate can weight it. Full coverage requires Phase 3 hop-tracing to find the ultimate beneficiary. Partial mitigation: any mint to a non-LP, non-emission-recipient address fires Signal B regardless of subsequent routing.

---

### E-D06-2 — Trickle Mint Below Per-Event Threshold

**Description:** The attacker mints in multiple small transactions, each below `supply_change_threshold_pct = 0.05` (5%). If the attacker mints 100 transactions × 0.4% each, Signal B never fires per event. Over 30 days, cumulative = 40%, which fires Signal C.

**Signals defeated:** Signal B.
**Signals preserved:** Signal C catches cumulative ≥ 20% over 30 days.

**Attacker cost:** Low. Only Solana transaction fees (~0.000005 SOL per tx). 100 txs costs ~0.0005 SOL.

**Residual gap:** If the attacker spaces trickle mints beyond the `hidden_mint_window_days` = 30 day window (minting 19.9% in month 1, pausing, minting 19.9% in month 2), Signal C never fires on a single window. This is documented as Evasion E5.

---

### E-D06-3 — Mint Authority Transfer Before Minting

**Description:** The attacker transfers mint authority to a fresh, unlinked wallet immediately before executing the mint. The fresh wallet has no on-chain connection to the deployer visible in Phase 2. Signal A fires on the new authority (any non-null authority fires the check). Signal B fires on the observed mint event. Signal C fires if cumulative threshold is breached. D06 correctly flags the new authority as active, but the `mint_authority` evidence key shows the fresh wallet address, losing the deployer connection in Phase 2 evidence.

**Signals defeated:** None — all three signals still fire correctly.
**Attacker benefit:** Evidence quality degraded; deployer attribution requires Phase 3 graph analysis.

**Attacker cost:** Low. One `set_authority` instruction before mint.

**Mitigation:** Phase 3 wallet graph tracks authority rotation events. In Phase 2, evidence notes the current mint authority address; the reviewer can manually link it.

---

### E-D06-4 — Mint Directly to LP Pool (Recipient is Known LP)

**Description:** The attacker mints tokens directly to a Raydium or Orca pool contract address. The pool is in `known_lp_addresses`. Signal B's non-LP gate excludes the event. The attacker then removes LP (triggering D02 Signal A), but D06 is blind to the intermediate step.

**Signals defeated:** Signal B's per-event gate.
**Signals preserved:** Signal A (authority still active). Signal C requires non-LP recipients; if ALL mints go to LP, Signal C does not fire. D02 Signal A fires on the subsequent LP removal.

**Attacker cost:** Low-Medium. Minting to LP artificially inflates pool reserves; attacker must drain via LP removal (D02 coverage).

**Note:** This evasion trades D06 Signal B/C coverage for D02 Signal A coverage. The scoring crate combining D02 + D06 provides cross-detector coverage.

---

### E-D06-5 — Time-Delayed Mint After `hidden_mint_window_days`

**Description:** The attacker mints 19.9% of supply in week 1 (below Signal C threshold in window 1). Waits 31 days. Mints another 19.9% in week 5 (each window independently below threshold). Net supply inflation: 39.8% over 65 days; Signal C never fires in a single 30-day window.

**Signals defeated:** Signal C cumulative window check.
**Signals preserved:** Signal A fires throughout (authority remains active). Each individual mint may also fire Signal B if the per-event threshold is breached.

**Attacker cost:** Low (only time). The attacker must hold the inflated supply without selling for 30+ days per cycle.

**Residual gap:** Documented as Design Gap DG-D06-5 (cross-window accumulation). Phase 3 mitigation: extend cumulative query to 90-day window with decaying weight. Not in MVP scope.

---

### E-D06-6 — Burn-Then-Remint Laundering

**Description:** The attacker burns 10% of supply from their wallet (Signal B burn fires if ≥ 5%), then mints 10% to a fresh address (Signal B mint fires). Net supply is unchanged; attacker has moved tokens to a clean address. Signal B fires twice (once on burn, once on mint), but the connection between the two events requires transaction correlation beyond per-event analysis.

**Signals defeated:** No signal is defeated; both events fire Signal B.
**Attacker benefit:** Wallet attribution laundered. Fresh recipient address not linked to deployer in Phase 2.

**Attacker cost:** Low. Two transactions.

**Mitigation:** Both burn and mint events produce Signal B evidence independently. Evidence bundle includes `tx_hash` for each; a manual reviewer or Phase 3 scoring aggregation can correlate the pair.

---

### E-D06-7 — Token-2022 Withhold Extraction Without Mint (E-D02-11 Overlap)

**Description:** The attacker uses Token-2022 `TransferFeeConfig` with a high `transfer_fee_basis_points`. Fees accumulate in withheld accounts. The `withdraw_withheld_authority` calls `withdraw_withheld_tokens_from_accounts`. This produces a Transfer to the authority wallet (not from zero address). No `from_address = zero_address` event is emitted. D06 Signals B and C are entirely blind to this extraction path.

**Signals defeated:** Signal B, Signal C. Signal A fires only if `mint_authority` is also active (separate field from `withdraw_withheld_authority`).

**Residual detection:** D01 Signal S2 fires if `transfer_fee.fee_bps > sell_tax_threshold_bps`. A partial D06 mitigation (Signal B subvariant `supply_redirection_anomaly`) is specified in §10. Full coverage is a Phase 3 gap — documented as D07 candidate.

---

## 9. Evidence Keys

All keys are prefixed `mint_burn_anomaly/`. Values are `Decimal` encoded as strings per the frozen `AnomalyEvent.Evidence.metrics: BTreeMap<String, Decimal>` contract.

| Key | Type | Values | Signal | Meaning |
|-----|------|--------|--------|---------|
| `mint_burn_anomaly/signal` | String (via notes) | `"mint_authority_active"` \| `"supply_change_event"` \| `"hidden_mint_pattern"` \| `"info_suppressed"` | A/B/C | Which signal triggered this event |
| `mint_burn_anomaly/mint_authority` | Address (via addresses) | Base58 string or `"revoked"` | A/B/C | Current mint authority; `"revoked"` if `None` |
| `mint_burn_anomaly/supply_change_pct` | Decimal | Signed; positive = mint, negative = burn | B | `amount_raw / supply_denominator`; negative for burns |
| `mint_burn_anomaly/recipient_address` | Address (via addresses) | Base58 / checksummed hex | B | Recipient of mint event (or burner for burn events) |
| `mint_burn_anomaly/recipient_is_known_lp` | Decimal | `0` or `1` | B | `1` = recipient is in `known_lp_addresses` (event suppressed); `0` = non-LP |
| `mint_burn_anomaly/recipient_holder_kind` | String (via notes) | `"deployer"` \| `"insider"` \| `"dex_pool"` \| `"dex_aggregator"` \| `"vesting_contract"` \| `"unknown"` | B | From `holder_classifications` sidecar; `"unknown"` if not classified |
| `mint_burn_anomaly/cumulative_supply_change_30d_pct` | Decimal | `0.0`–`N.NN` | C | Cumulative non-LP mint as fraction of supply denominator over 30d window |
| `mint_burn_anomaly/mint_event_count_30d` | Decimal | Integer | C | Number of distinct non-LP mint events in 30d window (for Signal C) |
| `mint_burn_anomaly/supply_base` | String (via notes) | `"circulating"` \| `"total"` | A/B/C | Which supply denominator was used; `"total"` emitted only on fallback |
| `mint_burn_anomaly/token_age_days` | Decimal | Integer | A | Computed from `detected_at`; `-1` if age unknown |
| `mint_burn_anomaly/established_protocol_dampened_signal_a` | Decimal | `0` or `1` | A | `1` if established-protocol dampening was applied to Signal A confidence |

**`Evidence.tx_hashes`:** For Signal B, include the triggering `tx_hash`. For Signal C, include the most recent mint `tx_hash` in the 30-day window (the one that pushed cumulative over threshold).

**`Evidence.addresses`:** For Signal B, include `recipient_address`. For all signals, include `mint_authority` if active.

**`Evidence.notes`:** Human-readable summary: "Signal B: mint of 12.3% of circulating supply to non-LP address ABC...XYZ (kind: unknown)."

---

## 10. Cross-Detector Relations

### D06 coverage boundary for E-D02-11 (Token-2022 `withdraw_withheld`)

E-D02-11 (`docs/reviews/0002-d02-rug-pull-evasions.md §E-D02-11`) documents the `withdraw_withheld_tokens_from_accounts` instruction as a value-extraction path that:
- Does NOT produce a Transfer from zero address (not a mint).
- Does NOT produce a `pool_events` Burn row (not an LP drain in D02's sense).
- Produces a Transfer from each withheld-fee account to the `withdraw_withheld_authority`.

**Coverage matrix:**

| Detection path | D06 covers? | Mechanism | Conditions |
|----------------|-------------|-----------|------------|
| Mint authority active while high-fee Token-2022 token exists | Signal A: YES | `mint_authority.is_some()` (separate from withdraw authority) | Only if mint_authority is also set |
| `withdraw_withheld` Transfer to authority (post-extraction Transfer) | Signal B `supply_redirection_anomaly` subvariant: PARTIAL | If `from_address = transfer_fee.withdraw_withheld_authority` AND `to_address` is not LP AND amount ≥ threshold — this is a Transfer event (not mint/burn), but the Query 1/2 filters require from/to = zero_address. **Not covered by Queries 1 or 2.** | Requires a dedicated query variant |
| Silent authority rotation on TransferFeeConfig | NOT covered | D06 has no mechanism to observe `set_authority` instructions | D07 Phase 3 gap |
| High basis-point transfer fee itself | NOT covered (D01 covers) | D01 Signal S2 fires if `fee_bps > sell_tax_threshold_bps` | Cross-detector: D01+D06 |

**D07 Phase 3 candidate (explicitly documented gap):**

D07 should cover: (1) monitor `withdraw_withheld_tokens_from_accounts` instruction; (2) compare cumulative withheld extraction to circulating supply; (3) detect `set_authority` on `withdraw_withheld_authority` (authority rotation); (4) cross-detector linkage: D01 high-fee + D07 extraction = combined rug event.

**MVP Phase 2 partial mitigation (Signal B `supply_redirection_anomaly` subvariant):**

When `TokenMeta.transfer_fee.is_some()` AND a Transfer event is observed where:
- `from_address = transfer_fee.withdraw_withheld_authority`
- `to_address` is not a known LP and not the mint account itself
- `amount_raw / supply_denominator >= supply_change_threshold_pct`

Then treat this as a Signal B event with `signal = "supply_redirection_anomaly"` at confidence `0.65` (lower than normal Signal B because the heuristic is imprecise — the Transfer is an ordinary SPL instruction, not a zero-address transfer). Emit `recipient_holder_kind` from sidecar for the receiving address.

This subvariant requires a third query (Query 3-alt) in `d06_mint_burn.sql`. The MVP may ship without this query and document it as DG-D06-3 to be added in a follow-on task.

### D06 and D02

D04 Signal C (insider sell-off) and D06 Signal C (hidden mint pattern) both fire on deployer-controlled wallet activity. When D06 Signal C fires, the scoring crate should treat it as a strong pre-rug signal and elevate D02 Signal B confidence. This is a scoring crate concern, not a D06 concern — D06 emits its evidence and the scoring crate composes.

### D06 and D01

D01 Signal S2 (high transfer fee) and D06 Signal A (active mint authority) both surface from `TokenMeta`. They may co-fire on the same token. The scoring crate should apply a mild positive correlation boost when both fire simultaneously — a token with both active mint authority AND a high transfer fee has two orthogonal attack vectors.

---

## 11. Failure Modes and Fallbacks

| Condition | Behavior | Evidence annotation |
|-----------|----------|---------------------|
| `circulating_supply_raw = None` | Fallback to `total_supply_raw`; proceed normally | `supply_base = "total"`; `tracing::warn!` logged |
| `circulating_supply_raw = Some(0)` | Fallback to `total_supply_raw` if non-zero; else `Err(InsufficientBaseline)` | `supply_base = "total"` or error |
| `total_supply_raw = 0` | `Err(InsufficientBaseline { fallback_used: true })` — cannot normalize | No event emitted |
| `detected_at = None` (token age unknown) | Skip grace period check for Signal A; fire Signal A if `mint_authority.is_some()` regardless of age | `token_age_days = -1` in evidence |
| No mint/burn events in window | Signal B and C do not fire; Signal A may still fire | Normal Signal A evidence |
| `holder_classifications` sidecar absent for recipient | Emit `recipient_holder_kind = "unknown"`; do not block Signal B | Evidence note: `"recipient classification unavailable"` |
| Signal C cumulative query times out | `Err(DetectorError::TransientQuery)` — retry; do not partially fire Signal C | Scheduler retries per `DetectorError::is_retryable()` |
| `known_lp_addresses` list empty | Signal B fires on ALL mints/burns (no LP exclusion); higher FP risk. Log `tracing::warn!` | `recipient_is_known_lp = 0` on every row |

---

## 12. Fixture Corpus (6 fixtures, 3 positive + 3 negative)

Root: `tests/fixtures/solana/positive/` and `tests/fixtures/solana/negative/`.
All new fixtures MUST include `"synthetic": true` if not a live on-chain fetch, and `"_fixture_meta"` with `"detector": "D06"`, `"expected_signals"`, and `"expected_confidence_gte"`.

---

### POS-D06-01 — Hidden Mint Pattern (Sun 2024 archetype)

**Type:** Synthetic positive
**File:** `tests/fixtures/solana/positive/SYNTH_D06_POS_001_hidden_mint.json`
**Expected signals:** Signal A (active mint authority) → suppressed by Signal C; Signal C fires.

**State:**
```json
{
  "_fixture_meta": {
    "detector": "D06",
    "expected_signals": ["hidden_mint_pattern"],
    "expected_confidence_gte": "0.85",
    "expected_severity": "Critical",
    "synthetic": true,
    "rationale": "Sun et al. 2024 hidden mint archetype: authority active, 35% cumulative non-LP mint over 25 days, token age 20 days."
  },
  "mint_authority": "DeployerWalletAAAA1111111111111111111111111",
  "detected_at": "<window_end - 20 days>",
  "circulating_supply_raw": "1000000000000",
  "total_supply_raw": "1350000000000",
  "verification": { "jup_verified": false, "jup_strict": false },
  "rugcheck_score": 87,
  "markets": [
    { "pool_address": "PoolAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" }
  ]
}
```

**Event data (30-day window, non-LP mint events):**
- Day 5: mint 50,000,000,000 raw units → non-LP address DeployerWalletAAAA (5% of supply) — Signal B would fire; Signal C accumulates.
- Day 15: mint 150,000,000,000 raw units → non-LP address InsiderWalletBBBB (15% of supply) — Signal B fires; Signal C accumulates.
- Day 25: mint 150,000,000,000 raw units → non-LP address InsiderWalletCCCC (15% of supply) — Signal B fires; Signal C fires at cumulative 35%.

**Expected output:**
- Signal B for the Day 25 event: `conf = min(0.85, 0.55 + (0.15-0.05)/0.05*0.30 + 0.30) = 0.85`, severity High.
- Signal C: `conf = min(0.95, 0.75 + (0.35-0.20)*1.0) = 0.90`, severity Critical.
- Signal A: NOT emitted (C subsumes it).

---

### POS-D06-02 — Supply Inflation (Single Large Mint, Deployer Cluster)

**Type:** Synthetic positive
**File:** `tests/fixtures/solana/positive/SYNTH_D06_POS_002_single_large_mint.json`
**Expected signals:** Signal B (supply change event, non-LP, large single mint). Signal A also fires independently.

**State:**
- Token age: 3 days (inside grace period? No — wait, 3 days < 7 = inside grace → Signal A suppressed by grace period. Token age = 8 days.)
- `mint_authority`: active (`"DeployerWalletDDDD1111111111111111111111111"`)
- `detected_at`: 8 days before `ctx.window.end`
- `circulating_supply_raw`: 1,000,000,000,000
- `verification.jup_strict`: false, `jup_verified`: false, `rugcheck_score`: 91

**Event data:**
- Single mint: `amount_raw` = 500,000,000,000 (50% of circulating supply) → recipient = `InsiderWalletEEEE` (not LP)

**Expected output:**
- Signal B: `conf = min(0.85, 0.55 + (0.50-0.05)/0.05*0.30 + 0.30) = 0.85`, severity High.
- Signal A: `conf = 0.20`, severity Info (co-fires with Signal B since Signal C did not fire, and 30-day cumulative has not crossed 20%).
- Signal C: cumulative = 50% but this is a single event, so cumulative = 50% ≥ 20%; AND token age 8 ≥ 14? NO — token age 8 < 14, so Signal C does NOT fire. Signal A emits alongside Signal B.

---

### POS-D06-03 — Token-2022 Withhold Extraction Heuristic (Partial D06 Coverage of E-D02-11)

**Type:** Synthetic positive
**File:** `tests/fixtures/solana/positive/SYNTH_D06_POS_003_withdraw_withheld_partial.json`
**Expected signals:** Signal A (mint authority active), Signal B subvariant `supply_redirection_anomaly` (if MVP query 3-alt is implemented; else DG-D06-3 documented gap).

**State:**
- Token-2022 with `transfer_fee.fee_bps` = 500 (5%), `transfer_fee.withdraw_withheld_authority` = `"AuthorityWalletFFFF1111111111111111111111111"`
- `mint_authority`: active (same wallet as withhold authority in this scenario)
- Token age: 30 days
- `circulating_supply_raw`: 1,000,000,000,000
- `verification.jup_strict`: false, `jup_verified`: false

**Event data:**
- Transfer (NOT from zero address): `from_address = "AuthorityWalletFFFF1111111111111111111111111"` (the `withdraw_withheld_authority`), `to_address = "FreshWalletGGGG111111111111111111111111111"`, `amount_raw = 80,000,000,000` (8% of supply).

**Expected output (if supply_redirection_anomaly subvariant implemented):**
- Signal B subvariant: `conf = 0.65`, `signal = "supply_redirection_anomaly"`, severity Medium.
- Signal A: `conf = 0.20`, severity Info.

**Expected output (if subvariant NOT implemented — DG-D06-3 gap):**
- Signal A only: `conf = 0.20`, severity Info.
- Evidence note: `"withdraw_withheld extraction requires D07 Phase 3 coverage; D06 coverage partial via Signal A only"`.

**Fixture value:** Tests the E-D02-11 partial coverage boundary. Documents precisely what D06 catches versus what D07 must cover. Serves as a regression fixture for the DG-D06-3 gap documentation.

---

### NEG-D06-01 — wSOL (Fixed Supply, No Mint Authority)

**Type:** Real negative fixture (reuse from existing Phase 1 corpus)
**File:** `tests/fixtures/solana/negative/So111111_wSOL.json` (existing; D06 verdict column to be added)
**Expected signals:** None.

**Verification:**
- `mint_authority`: `None` (revoked post-genesis — wSOL supply is fixed at Solana native SOL equivalent; the SPL wSOL mint has no active mint authority)
- No Transfer events from zero address in any window (supply never changes post-genesis)
- Signal A: does not fire (no mint authority)
- Signal B: does not fire (no qualifying transfer events)
- Signal C: does not fire

**D06 verdict:** `BELOW_THRESHOLD`

---

### NEG-D06-02 — USDC (Active Mint Authority, Established Protocol Dampening)

**Type:** Real negative fixture (reuse existing or enrich)
**File:** `tests/fixtures/solana/negative/EPjFWdd5_USDC.json` (existing; D06 verdict to be added)
**Expected signals:** Signal A fires at dampened confidence `0.10`; Signal B and C suppressed.

**State (from live RugCheck fetch):**
- `mint_authority`: `Some(...)` (Circle's mint authority — active by design)
- `verification.jup_strict`: `true` → `is_established_protocol(meta) = true`
- `rugcheck_score`: within safe range

**Expected output:**
- Signal A: `conf = 0.20 * 0.50 = 0.10`, severity Info, `signal = "info_suppressed"`, `established_protocol_dampened_signal_a = 1`.
- Signal B: SUPPRESSED (even if Circle minted new USDC in the observation window, it is not anomalous).
- Signal C: SUPPRESSED.

**Test assertion:** `events[0].confidence == Decimal::from_str("0.10").unwrap()`, `events[0].severity == Severity::Info`. Any Signal B or C in output is a bug.

---

### NEG-D06-03 — BONK (Burns via LP, No Anomalous Pattern)

**Type:** Real negative fixture (reuse existing)
**File:** `tests/fixtures/solana/negative/DezXAZ8z_BONK.json` (existing; D06 verdict to be added)
**Expected signals:** None (or Signal B suppressed by LP gate).

**State:**
- BONK has burned significant supply via LP burns (token.bonkcoin.com Raydium LP burns). These are burn events (to zero address) where `from_address` is a Raydium pool contract. Pool contracts ARE in `known_lp_addresses`.
- `mint_authority`: `None` (BONK mint authority was revoked)

**Expected output:**
- Signal A: does not fire (no mint authority).
- Signal B: burn events from LP pool do NOT fire (LP exclusion gate suppresses them).
- Signal C: does not fire (no non-LP mint events; authority revoked).

**D06 verdict:** `BELOW_THRESHOLD`

**Test assertion:** `events.is_empty()`. Any Signal A/B/C output is a bug.

---

## 13. Config Stub Expansion

The Sprint 4 P4-3 developer MUST expand the `[mint_burn_anomaly]` stub in `config/detectors.toml` with the following keys. The existing `supply_change_pct` entry retains its current value and rationale; new keys are added below it.

```toml
[mint_burn_anomaly.mint_authority_grace_period_days]
value     = 7
rationale = """New projects commonly deploy with mint authority to execute genesis-phase \
              minting (airdrops, LP seeding, team allocation) and revoke within the first \
              week. Sun et al. (2024) observe that rugged tokens almost never revoke; \
              non-rugged tokens revoke within 1-7 days post-launch. A 7-day grace period \
              prevents alerting on every new token while remaining short enough to catch \
              projects that retain authority without a stated reason. Classified as \
              unverified-heuristic; calibrate from labelled corpus in Sprint 5. \
              Source: Sun et al. 2024 §4 hidden mint pattern; RugCheck mintAuthority signal."""
refs      = ["D06/mint_burn_anomaly"]

[mint_burn_anomaly.hidden_mint_cumulative_pct]
value     = 0.20
rationale = """Sun et al. (2024) hidden mint category: supply inflation post-genesis is \
              the defining characteristic of the pattern. A 20% cumulative non-LP mint \
              over 30 days provides 4x headroom above the per-event 5% threshold (to \
              catch trickle-mint evasion E2) while remaining high enough to distinguish \
              from legitimate treasury operations. Classified as unverified-heuristic; \
              calibrate from labelled corpus in Sprint 5. \
              Source: Sun et al. 2024 §4; no published numeric threshold for cumulative variant."""
refs      = ["D06/mint_burn_anomaly"]

[mint_burn_anomaly.hidden_mint_window_days]
value     = 30
rationale = """30-day rolling window for Signal C cumulative supply change calculation. \
              Consistent with D04/D05 detection windows; long enough to aggregate \
              trickle-mint attacks while short enough to remain actionable. A 30-day \
              window is standard in academic sources for supply anomaly detection. \
              Classified as unverified-heuristic. \
              Source: research/02-detection-methodology.md §Cross-cutting C."""
refs      = ["D06/mint_burn_anomaly"]

[mint_burn_anomaly.min_token_age_days_for_hidden_mint]
value     = 14
rationale = """14 days gives genesis-phase minting activity time to settle before \
              Signal C fires. Tokens legitimately execute multi-round mints (airdrops, \
              LP seeding, team vesting) within the first two weeks. A 14-day floor \
              prevents false positives on legitimate launch-phase activity while \
              remaining short enough to catch early rugs. Design derivation; \
              no prior academic citation. Calibrate from labelled corpus in Sprint 5."""
refs      = ["D06/mint_burn_anomaly"]

[mint_burn_anomaly.established_protocol_confidence_dampening]
value     = 0.5
rationale = """Signal A confidence multiplier for established protocols (is_established_protocol \
              = true). Value 0.5 halves base confidence from 0.20 to 0.10, retaining \
              Signal A at Info severity for audit observability while preventing spurious \
              Medium alerts on regulated stablecoins (USDC, PYUSD). Full suppression \
              is rejected because it would hide the audit trail. 0.5 halving is \
              consistent with the D02/D03 asymmetric suppression pattern (P4-0). \
              Source: token_status.rs P4-0; docs/designs/0005-detector-02-rug-pull.md §14."""
refs      = ["D06/mint_burn_anomaly"]

[mint_burn_anomaly.non_lp_recipient_signal_weight]
value     = 0.30
rationale = """Additive confidence bonus when the mint recipient is NOT a known LP contract. \
              Non-LP recipient is the key distinguishing feature between a hidden mint \
              (attacker benefits) and legitimate treasury/LP mint (protocol benefits). \
              0.30 elevates baseline Signal B confidence from 0.55 to 0.85 at threshold, \
              placing it firmly in the High severity band. Calibrated so borderline cases \
              (exactly at supply_change_threshold) reach High; above threshold saturates \
              the 0.85 cap. Classified as unverified-heuristic; calibrate from labelled \
              corpus in Sprint 5. \
              Source: Sun et al. 2024 hidden mint recipient analysis; design derivation."""
refs      = ["D06/mint_burn_anomaly"]
```

---

## 14. Design Gaps

### DG-D06-1 — Token Age Unknown: Grace Period Degraded

**Description:** When `TokenMeta.detected_at = None` (token not yet enriched with first-seen timestamp), Signal A fires regardless of actual token age. A brand-new token that was just deployed fires Signal A even if it is 10 seconds old. This is a conservative false-positive source.

**Impact:** False positives on newly indexed tokens in the first enrichment cycle. Very short-lived — within one enrichment cycle (seconds to minutes), `detected_at` is populated.

**Mitigation (MVP):** Fire Signal A regardless when `detected_at = None`; emit `token_age_days = -1` in evidence so downstream consumers can filter. The scoring crate can apply a lower weight to Signal A events with `token_age_days = -1`.

**Phase 3 resolution:** The indexer populates `detected_at` on first-observed block; this gap narrows to the latency between first observation and registry enrichment.

---

### DG-D06-2 — LP Detection Depends on `known_lp_addresses` Sidecar Completeness

**Description:** Signal B's non-LP recipient gate relies on a pre-populated `known_lp_addresses` list passed from the `pools` table. If a new pool has been created but not yet indexed in the `pools` table, its address is not in the exclusion list. A legitimate mint-to-new-LP event produces a false positive.

**Impact:** False positive rate proportional to the lag between pool creation and pool indexing. In normal operation (pool indexer active), lag is seconds to minutes. During indexer outage or for very new tokens, lag may be longer.

**Mitigation (MVP):** The query parameter `$6 known_lp_addresses` is populated from `ctx.registry.enrich(token).markets` which fetches from the `pools` table. If a pool was indexed before the mint event, it is excluded. Emit evidence `recipient_is_known_lp = 0` when exclusion list is known to be stale (add a `known_lp_addresses_count = 0` guard: if the list is empty, emit an Info warning that LP exclusion is unavailable).

**Phase 3 resolution:** Pool indexer real-time streaming ensures near-zero lag.

---

### DG-D06-3 — `withdraw_withheld` Extraction Requires Token-2022 Decoder

**Description:** The E-D02-11 `withdraw_withheld_tokens_from_accounts` instruction is a Token-2022-specific instruction not representable as a standard SPL `Transfer` from zero address. The Signal B `supply_redirection_anomaly` subvariant (§10) requires a separate query that identifies Transfers from `transfer_fee.withdraw_withheld_authority` to non-LP addresses. The MVP query (Queries 1 and 2 in `d06_mint_burn.sql`) does not include this variant.

**Impact:** Complete blind spot for Token-2022 fee-extraction rugs (E-D02-11 evasion E-D06-7). D01 Signal S2 provides partial coverage via fee detection, but does not detect the extraction event itself.

**Mitigation (MVP):** Signal A fires if `mint_authority.is_some()` (covers tokens where the same deployer holds both mint and withhold authority). The `supply_redirection_anomaly` subvariant query is documented as a follow-on task in the D06 developer acceptance checklist (item #10).

**Phase 3 resolution:** D07 candidate. Full Token-2022 decoder for `withdraw_withheld` instruction in the indexer; cross-detector linkage D01 + D07 for combined rug severity.

---

### DG-D06-4 — Fragmentation Evasion: Mint to Many Small Wallets Below Signal B Threshold

**Description:** An attacker mints to 20 distinct wallets × 0.4% supply each. No single mint exceeds the 5% Signal B threshold. Cumulative = 8% over one event window — below Signal C's 20% threshold. D06 is entirely silent on this pattern if executed within a short window.

**Impact:** Attacker can distribute up to 19.9% of supply to insider wallets without triggering Signal B or Signal C, provided each individual mint stays below 5%.

**Mitigation (MVP):** None effective in Phase 2. Signal A fires (authority remains active). The distribution is visible in the `holder_concentration` D03 detector as a top-N shift if the recipients accumulate enough supply.

**Phase 3 resolution:** Graph-level aggregation: sum all mints within a window grouped by cluster (connected-component of mint recipients + deployer). If cluster-level cumulative mint ≥ `supply_change_threshold_pct`, fire a cluster variant of Signal B. Requires Phase 3 wallet graph.

---

### DG-D06-5 — Cross-Window Cumulative Accumulation (Time-Delayed Evasion)

**Description:** Evasion E5: attacker mints 19.9% in month 1, pauses, mints 19.9% in month 2. Each 30-day window independently below Signal C threshold. D06 never fires Signal C.

**Impact:** Attacker can inflate supply by 39.8% over 65 days without triggering Signal C.

**Mitigation (MVP):** Signal A fires throughout (authority remains active). Individual events below 5% will not fire Signal B. If individual events ≥ 5%, Signal B fires on each.

**Phase 3 resolution:** Extend cumulative query to a 90-day window with exponential decay weighting (older events count less). Add a `cross_window_cumulative_pct` metric computed over the full token history. Flag when `cross_window_cumulative_pct` crosses a higher threshold (e.g., 30%) over the token's lifetime.

---

## 15. `is_established_protocol` Per-Signal Decision

The asymmetric suppression contract (P4-0, `token_status.rs`, `docs/designs/0003-detector-trait.md §Established-protocol suppression pattern`) applies to D06 as follows:

| Signal | Type | `is_established_protocol = true` | `is_established_protocol = false` |
|--------|------|----------------------------------|-----------------------------------|
| A — Active mint authority | State-based / latent structural | **DAMPENED** (not suppressed): `conf = base * 0.5 = 0.10`; signal key `"info_suppressed"`; severity Info. Observability preserved for audit. | Full confidence 0.20, severity Info. |
| B — Supply change event | Event-based (observed Transfer) | **FULLY SUPPRESSED**: no event emitted. Rationale: established protocols execute treasury operations that are structurally identical to hidden mints; suppressing avoids high-confidence false alarms on USDC minting. | Fires normally. |
| C — Hidden mint pattern | Composite (event + state) | **FULLY SUPPRESSED**: no event emitted. Same rationale as Signal B. | Fires normally. |

**Why Signal A is dampened but not suppressed:** Signal A is a structural audit flag (authority present), not a behavioral alert. USDC's active mint authority is a well-known fact; generating an Info-level audit note for it at 0.10 confidence is appropriate for custody consumer compliance logging. Full suppression would cause D06 to emit zero events for USDC, which could be misread as "D06 has no opinion on USDC." The dampened Info event makes D06's position explicit: "we see active authority; we're not alarmed because of jup_strict status."

**Why Signals B and C are fully suppressed (not just dampened):** Signal B and C are event-based or composite signals with High/Critical severity. A 0.85-confidence Critical event on USDC would override any downstream dampening — the scoring crate cannot easily attenuate Critical events without special-casing. Full suppression is safer. If an established protocol were genuinely attacked (malicious Circle employee minting unauthorized supply), D01 would detect sell-blocking and D02 would detect any LP drain; D06 Signal B/C would be redundant.

---

## 16. Known Calibration Flags (for Sprint 5 corpus)

The following tokens from the existing Phase 1 corpus carry specific D06 behavior that MUST be validated once D06 is implemented:

| Fixture | Expected D06 verdict | Notes |
|---------|---------------------|-------|
| `EPjFWdd5_USDC.json` | Signal A at 0.10, Info, `info_suppressed` | Tests established-protocol dampening path |
| `So111111_wSOL.json` | BELOW_THRESHOLD | Tests zero-signal negative path (no mint authority, no events) |
| `DezXAZ8z_BONK.json` | BELOW_THRESHOLD | Tests LP-burn exclusion; BONK burns are from LP pool addresses |
| `2b1kV6Dk_PYUSD.json` | Signal A at 0.10, Info, `info_suppressed` | Same as USDC; jup_strict=true |

---

## 17. Developer Acceptance Checklist

The following constitutes the acceptance criterion for the P4-3 developer implementing this design:

- [ ] `crates/detectors/src/d06_mint_burn.rs` created; `Detector::ID = "mint_burn_anomaly"`.
- [ ] `fetch_rows()` / `compute()` split implemented per `docs/designs/0003-detector-trait.md §mock.rs` pattern.
- [ ] Signal A: fires on `mint_authority.is_some()` AND token age > grace period OR age unknown.
- [ ] Signal A: dampened (not suppressed) when `is_established_protocol(meta) = true`; `conf = 0.10`; signal key `"info_suppressed"` in evidence notes.
- [ ] Signal A: NOT emitted when Signal C fires.
- [ ] Signal B (Query 1 — mints): fires on single Transfer from zero address ≥ threshold AND non-LP recipient; confidence formula from §5.
- [ ] Signal B (Query 2 — burns): fires on single Transfer to zero address ≥ threshold AND non-LP burner; same confidence formula.
- [ ] Signal B: FULLY suppressed (no event emitted) when `is_established_protocol(meta) = true`.
- [ ] Signal C: fires when all four conditions in §5 hold; confidence formula from §5.
- [ ] Signal C: FULLY suppressed (no event emitted) when `is_established_protocol(meta) = true`.
- [ ] Supply denominator fallback: `circulating_supply_raw` → `total_supply_raw`; `supply_base = "total"` evidence annotation; `tracing::warn!` logged.
- [ ] Zero denominator: `Err(DetectorError::InsufficientBaseline)` returned.
- [ ] `config/detectors.toml` expanded with all 6 new threshold keys from §13 (retaining existing `supply_change_pct`).
- [ ] `AllDetectorConfigs.mint_burn_anomaly` struct in `config.rs` updated to include all new `Threshold<T>` fields.
- [ ] Unit test: POS-D06-01 (Signal C fires, Signal A omitted).
- [ ] Unit test: POS-D06-02 (Signal B + Signal A co-fire; Signal C suppressed by age < 14d).
- [ ] Unit test: NEG-D06-02 (USDC — Signal A at 0.10, Signal B+C suppressed).
- [ ] Unit test: NEG-D06-03 (BONK — BELOW_THRESHOLD; LP burn events excluded).
- [ ] Integration test: `tests/fixtures/solana/negative/So111111_wSOL.json` produces empty event vec.
- [ ] Evidence key `mint_burn_anomaly/signal` correctly populated for all three signal variants.
- [ ] Evidence key `mint_burn_anomaly/supply_base` set to `"circulating"` or `"total"`.
- [ ] `BTreeMap` used for all intermediate collections contributing to `Evidence::metrics` (determinism contract).
- [ ] No `Utc::now()` calls in computation path (use `ctx.window.end` for age computation).
- [ ] (DG-D06-3 backlog item) Comment or `TODO` in code: `supply_redirection_anomaly` subvariant for Token-2022 `withdraw_withheld` not implemented in MVP; see docs/designs/0009 §10 and §14 DG-D06-3.
- [ ] `REFERENCES.md` verified to contain entries for Xia et al. 2021 and Sun et al. 2024 (both already present; confirm D06 `Used In` column updated).

---

## 18. Non-Goals

This design explicitly does NOT cover:

- Token-2022 `withdraw_withheld` as a first-class detected event (D07 Phase 3).
- Cross-window cumulative supply tracking beyond 30 days (DG-D06-5; Phase 3).
- Wallet-graph-level aggregation of fragmented mints (DG-D06-4; Phase 3).
- EVM chain `mint()` / `burn()` function calls (Phase 4; EVM adapter required; same signal logic applies via zero-address Transfer events).
- Scheduled emission schedule validation (distinguishing expected vesting mints from anomalous mints requires on-chain vesting schedule parsing — Phase 3).
- ML-based supply anomaly classification (Phase 4/5; requires ≥1,000 labelled examples per class).

---

## 19. References

All referenced sources are documented in `REFERENCES.md`. D06-specific entries:

| Mechanism | Signal | Source | Used In | Verified Against |
|-----------|--------|--------|---------|-----------------|
| Mint/burn anomaly (hidden mint, ~10k scam tokens) | Supply increase > threshold, non-LP recipient | Xia et al. 2021, https://arxiv.org/abs/2109.00229 | D06 Signal B, Signal C baseline | Already in REFERENCES.md |
| Rug pull root causes — hidden mint + hidden owner categories | Active mint authority as structural risk; cumulative supply inflation pattern | Sun et al. 2024, https://arxiv.org/abs/2403.16082 | D06 Signal A, B, C; threshold derivation for cumulative_pct | Already in REFERENCES.md |
| Token-2022 withdraw_withheld drain path | E-D02-11: value extraction without LP Burn row | Token-2022 TransferFeeConfig docs + Sun et al. 2024 §4 "Hidden Fee" | D06 §10 cross-detector coverage matrix; DG-D06-3 | Already in REFERENCES.md |

No new REFERENCES.md entries required for D06 — all primary sources (Xia 2021, Sun 2024, Token-2022 docs) are already present from Phase 0 / D02 research. The `Used In` column for the existing Xia 2021 and Sun 2024 entries should be updated to include `D06 mint_burn_anomaly` once the detector ships.
