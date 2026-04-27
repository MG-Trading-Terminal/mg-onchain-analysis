# Design 0012 — Detector D07: Token-2022 Withdraw-Withheld Drain

**Date:** 2026-04-21
**Status:** Draft
**Author:** onchain-analyst agent
**ADR refs:**
- ADR 0001 §D4 — `AnomalyEvent { confidence, severity, evidence }` output contract
- ADR 0001 §D5 — detection gap explicitly designated Phase 3, promoted to Sprint 5 (E-D02-11)
- ADR 0001 §D7 — fixture corpus bootstrapping
- ADR 0002 — Postgres-only storage; all queries in PostgreSQL dialect
- ADR 0003 — self-sovereign infrastructure; no 3rd-party runtime dependencies
**Trait ref:** `docs/designs/0003-detector-trait.md` — implements `Detector` trait, uses `DetectorContext`
**Evasion ref:** `docs/reviews/0002-d02-rug-pull-evasions.md` §E-D02-11 — original writeup of this gap
**Coverage matrix ref:** `docs/designs/0009-detector-06-mint-burn.md` §10 — formal D06 coverage boundary
**D01 cross-link:** `crates/detectors/src/d01_honeypot.rs` Signal S2 — transfer_fee_bps threshold
**Suppression ref:** `crates/detectors/src/token_status.rs` — `is_established_protocol` predicate
**Query ref:** `docs/queries/d07_withdraw_withheld.sql` — Queries W1, W2, W3
**Detector ID:** `withdraw_withheld_drain`

---

## 1. Context

The Token-2022 program (program ID `TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb`) extends the
standard SPL Token program with optional per-mint extensions stored as a TLV (type-length-value)
array in the mint account. One extension is `TransferFeeConfig` (extension type 1). When present,
every `TransferChecked` instruction on the mint deducts a fee — in basis points — from the
transferred amount and accumulates it as `withheld_amount` in the destination token account.

The accumulated `withheld_amount` is NOT automatically sent anywhere. It sits in each token account
until one of two instructions is executed:

1. `HarvestWithheldTokensToMint` — moves withheld balances from a set of token accounts into the
   mint's own withheld balance. This may be called by anyone.
2. `WithdrawWithheldTokensFromMint` — moves the mint's accumulated withheld balance to a specified
   destination token account. May only be called by the `withdraw_withheld_authority`.
3. `WithdrawWithheldTokensFromAccounts` — directly moves withheld balances from a provided array of
   token accounts to a destination. May only be called by the `withdraw_withheld_authority`.

The `withdraw_withheld_authority` is a public key stored inside the `TransferFeeConfig` TLV
extension. It is distinct from the mint's `mint_authority` (which controls new supply) and from
the `transfer_fee_config_authority` (which controls the fee rate). Any of the three can be set to
different wallets, or revoked independently.

**Why no existing detector catches this:**

- D02 (`rug_pull_lp_drain`) looks for `pool_events` rows with `event_kind = 'burn'`. A
  `WithdrawWithheld*` instruction produces a SPL Token Transfer from withheld accounts to the
  authority's token account — no LP burn event is emitted. D02 Signal A sees zero qualifying rows;
  D02 Signal B sees unchanged LP burn percentage. The pool appears intact.

- D06 (`mint_burn_anomaly`) looks for `Transfer` events where `from_address = zero_address` (mint)
  or `to_address = zero_address` (burn). A `WithdrawWithheld*` instruction produces a Transfer from
  the withheld token account (non-zero address) to the authority's token account (non-zero
  address). No zero-address Transfer is emitted. D06 Signals B and C are entirely blind.

- D01 (`honeypot_sim`) Signal S2 fires when `transfer_fee.fee_bps > sell_tax_threshold_bps` at
  listing time. This provides a static precondition signal — the high fee is visible before any
  withdrawal — but D01 does not monitor subsequent extraction events. Once D01 fires, subsequent
  `WithdrawWithheld*` extractions are invisible to D01.

D07 closes this gap by monitoring the Token-2022 instruction stream directly, introducing a new
storage table (`token2022_instructions`) and three PostgreSQL queries for extraction events,
authority history, and cumulative USD flows.

This spec is the implementation contract for the Sprint 5 P5-5 developer task. The developer
implements `crates/detectors/src/d07_withdraw_withheld.rs` and the V00007 migration. No changes to
`crates/common` are required — `TokenMeta.transfer_fee` already carries `fee_bps`, `max_fee_raw`,
and `authority`. The `withdraw_withheld_authority` (the extraction authority) is tracked in the new
`token2022_instructions` table, not in `TokenMeta`, because it is a dynamic event rather than a
static metadata field.

---

## 2. Token-2022 Mechanics Deep Dive

### 2.1 TransferFeeConfig Extension Layout

The Token-2022 `TransferFeeConfig` TLV extension (extension type byte = 1) encodes:

- `transfer_fee_config_authority`: Pubkey — who can change the fee rate (may be revoked)
- `withdraw_withheld_authority`: Pubkey — who can extract accumulated withheld balances
- `withheld_amount`: u64 — withheld balance currently held at the MINT account level
- `older_transfer_fee`: struct { epoch: u64, maximum_fee: u64, transfer_fee_basis_points: u16 }
- `newer_transfer_fee`: struct { epoch: u64, maximum_fee: u64, transfer_fee_basis_points: u16 }

The active fee is `newer_transfer_fee` if `newer_transfer_fee.epoch <= current_epoch`, else
`older_transfer_fee`. The `maximum_fee` field (u64, raw token units) caps the absolute fee per
transfer regardless of the basis-points rate.

**Key asymmetry:** `TokenMeta.transfer_fee.authority` (as stored in `crates/common`) tracks the
`transfer_fee_config_authority` — the wallet that can raise or lower the fee rate. The
`withdraw_withheld_authority` is a distinct field and is NOT currently stored in `TokenMeta`. D07
reads it from the `token2022_instructions` table (new, V00007 migration) where the indexer persists
it from the decoded `TransferFeeConfig` extension at mint creation and from subsequent
`SetAuthority` instructions targeting it.

### 2.2 Attack Pattern

1. Deployer creates a Token-2022 mint with `transfer_fee_basis_points = 3000–9999` (30%–99.99%)
   and `withdraw_withheld_authority = deployer_wallet` (or a fresh hot wallet funded 1–48 hours
   before deployment).

2. Users discover the token via DEX aggregators or social channels. Jupiter does NOT warn about
   `withdraw_withheld_authority` by default (as of 2026-04-21 live verification). The fee appears
   as "transfer fee" in some wallets but the withheld accumulation mechanism is not surfaced.

3. Every swap or transfer deducts the fee. For a 5000 bps (50%) fee, every buyer who swaps 1 SOL
   worth of tokens receives only 0.5 SOL worth of tokens; the other 0.5 SOL worth of tokens
   accumulates as `withheld_amount` in the Raydium pool's token account.

4. Deployer calls `WithdrawWithheldTokensFromAccounts` with the pool's token account and any other
   heavily-traded accounts in the list. The withheld balances transfer to the deployer's token
   account. No LP burn occurs; pool reserves appear unaffected.

5. Deployer swaps their extracted tokens for SOL/USDC via a separate route, completing the drain.

6. Step 4–5 can repeat indefinitely as long as the token has trading volume. The scam is
   sustainable for days or weeks without any LP manipulation.

### 2.3 Why `TransferFeeConfig.authority` (in TokenMeta) Is Not the Same as `withdraw_withheld_authority`

`TokenMeta.transfer_fee.authority` = `transfer_fee_config_authority` — controls fee rate.
`withdraw_withheld_authority` — controls extraction. These are two separate Pubkeys in the
`TransferFeeConfig` TLV. A deployer commonly sets:
- `transfer_fee_config_authority` = deployer (to allow fee rate changes) — this IS in `TokenMeta`
- `withdraw_withheld_authority` = a separate hot wallet (for operational security) — NOT in `TokenMeta`

D07 must track the `withdraw_withheld_authority` as a separate field. It is stored in the
`token2022_instructions` table by the indexer at mint creation time and updated on
`SetAuthority { authority_type: WithdrawWithheldTokens }` instructions.

---

## 3. Signal Taxonomy

D07 produces one to three `AnomalyEvent`s from a single `evaluate()` call:

| Signal | When it fires | Confidence band | Severity range | Event-based? |
|--------|--------------|-----------------|----------------|--------------|
| A — Active extraction (primary) | `WithdrawWithheld*` instructions executed in window, cumulative USD ≥ threshold, count ≥ threshold | 0.60–0.90 | Medium–Critical | Yes — event-based |
| B — Authority rotation alert | `SetAuthority { authority_type: WithdrawWithheldTokens }` within `authority_rotation_window_days`; new authority is a fresh wallet | 0.40–0.75 | Info–High | Yes — event-based |
| A+B composite | Signal A fires AND Signal B rotation preceded it within the window | `min(0.95, signal_a_conf + 0.10)` | High–Critical | Composite |

**Signal C (cross-detector linkage) is a scoring-layer concern:** When D01 Signal S2 fires
(`transfer_fee_bps > sell_tax_threshold_bps`) AND D07 Signal A fires within the same evaluation
window, the scoring crate elevates severity by one band above what Signal A alone produced. D07
emits `withdraw_withheld/combined_with_d01_s2 = "1"` as an evidence key so the scoring crate can
identify the pairing. D07 does NOT invoke D01 directly and does NOT read D01 output. The linkage
is evidence-key-based.

**Established-protocol handling:**

| Signal | `is_established_protocol` = true | `is_established_protocol` = false |
|--------|----------------------------------|-----------------------------------|
| A | Conditionally suppressed — see §9 | Fires normally |
| B | NOT suppressed (rotation always suspicious) | Fires normally |
| A+B composite | Conditionally suppressed per Signal A rule | Fires normally |

---

## 4. Inputs

### Static (from `ctx.registry.enrich()`)

- `TokenMeta.transfer_fee: Option<TransferFeeConfig>` — presence confirms Token-2022 with fee
  extension. D07 immediately returns `Err(DetectorError::InsufficientBaseline { reason: "not a
  Token-2022 mint with TransferFeeConfig" })` if `None`.
- `TokenMeta.transfer_fee.fee_bps: u16` — surfaced in evidence as
  `withdraw_withheld/transfer_fee_bps` and used to check D01 S2 overlap.
- `TokenMeta.markets: Vec<MarketInfo>` — provides pool addresses for USD volume cross-reference in
  the established-protocol suppression check.
- `TokenMeta.verification` — passed to `is_established_protocol(meta)`.
- `TokenMeta.rugcheck_score` — passed to `is_established_protocol(meta)`.

### Event-based (from `ctx.store` — PostgreSQL `token2022_instructions` table, V00007)

Query W1: `fetch_withdraw_withheld_events(chain, mint, window_hours)` — withdraw instruction rows
with `amount_raw`, `authority`, `tx_hash`, `block_time`.

Query W2: `fetch_withdraw_authority_history(chain, mint, window_days)` — `SetAuthority`
instruction rows targeting `authority_type = 'withdraw_withheld'`.

Query W3: `fetch_cumulative_withheld_usd(chain, mint, window_hours)` — sum of USD-valued
extractions from W1 rows that have a USD price annotation.

### Sidecar (from `wallet_funding_events` — existing or new table)

For Signal B's fresh-wallet detection: the indexer must record when the new authority wallet first
received SOL. If this data is available (the `wallet_funding_events` table or equivalent), D07
checks whether the new authority received its first SOL within `fresh_wallet_funding_hours` (48h)
before the rotation instruction. If the sidecar is absent, emit `authority_is_fresh_wallet = "0"`
with an evidence note that the check was skipped.

### Context parameters

- `ctx.window.start`, `ctx.window.end` — observation window for W1/W3 queries
- `ctx.window.block_start`, `ctx.window.block_end` — block range for W1/W3
- `sell_tax_threshold_bps` — from D01 config, passed as context parameter to populate
  `combined_with_d01_s2` evidence key

---

## 5. Baseline

**Signal A:** No rolling baseline required. The signal is event-based: any execution of
`WithdrawWithheld*` instructions by a non-program authority on a high-fee token is anomalous.
The baseline is "no extraction events within the window." Thresholds filter noise from legitimate
small protocol operations.

**Signal B:** No rolling baseline. The signal is a discrete `SetAuthority` instruction event. The
baseline is "no authority rotation within `authority_rotation_window_days`."

**USD valuation baseline:** The `token2022_instructions` table stores `amount_raw` (raw token
units). USD conversion uses the price oracle annotation from the indexer, stored as
`amount_usd: Option<Decimal>` in the table. If price is unavailable (`amount_usd = None`),
the USD sum from Query W3 returns NULL; D07 falls back to raw-unit comparison only and emits
`withdraw_withheld/cumulative_withdrawn_usd = "0"` with an evidence note.

**Regime stability:** All thresholds are either event counts (count ≥ N) or USD amounts (≥ $M).
USD amounts are not normalized to a cross-token baseline, so they are sensitive to token USD
price swings. For tokens where price data is unavailable, Signal A falls back to event-count-only
evaluation. This degrades recall for micro-cap tokens without USD price feeds but does not
produce false positives.

---

## 6. Signal Definitions

### Signal A — Active Extraction Event

**Precise signal definition:** One or more `WithdrawWithheldTokensFromAccounts` or
`WithdrawWithheldTokensFromMint` instructions executed against a Token-2022 mint within the
detection window, where:
1. The count of such instructions ≥ `min_extraction_events` (default 3), AND
2. The cumulative USD value extracted ≥ `min_cumulative_withdraw_usd` (default $1,000 USD), AND
3. The extracting authority is the current `withdraw_withheld_authority` (strong match) OR is an
   unrecognized wallet (weak match — possible authority rotation or CPI proxy).

**Authority match semantics:**
- `authority_match = "exact"`: the instruction signer matches the authority recorded in
  `token2022_instructions.withdraw_withheld_authority` for this mint. Strong signal.
- `authority_match = "unknown"`: the instruction signer is NOT the recorded authority. Possible
  CPI proxying (a program calling on behalf of the authority) or authority rotation that was not
  yet indexed. Fires Signal A at reduced confidence (subtract 0.10 from the formula result before
  applying the cap).

**Confidence formula (Signal A):**

```
extraction_event_factor = min(0.15, (event_count - min_extraction_events) * 0.03)
usd_ratio               = cumulative_usd / min_cumulative_withdraw_usd
usd_factor              = if usd_ratio > 1.0 { (usd_ratio.ln() * 0.10).min(0.15) } else { 0.0 }
authority_penalty       = if authority_match == "unknown" { 0.10 } else { 0.0 }

conf_raw = 0.60 + extraction_event_factor + usd_factor - authority_penalty
conf     = min(0.90, conf_raw.max(0.0))
```

Worked examples:
- 3 events, exactly $1,000, exact authority: `0.60 + 0.0 + 0.0 = 0.60`
- 5 events, $5,000, exact authority: `0.60 + 0.06 + ln(5.0)*0.10 = 0.60 + 0.06 + 0.161 = 0.821`
- 10 events, $50,000, exact authority: `0.60 + 0.15 + ln(50.0)*0.10 = 0.60 + 0.15 + 0.391 = min(0.90, 1.141) = 0.90`

**Why 0.60 base:** The extraction event alone is observable evidence of attacker action (not
merely structural risk). The 0.60 base places Signal A at the Medium-to-High boundary immediately,
reflecting the severity of confirmed value extraction. It is not 0.75 or higher because: (a) some
legitimate Token-2022 protocols do withdraw fees to treasury addresses on a schedule, and (b) our
USD threshold filters small-amount noise but does not guarantee attacker intent.

**Established-protocol suppression:** See §9. Signal A on established protocols is suppressed ONLY
when `extraction_usd / pool_volume_usd_in_window <= established_protocol_fee_extraction_allowlist_pct`.
Legitimate protocols withdraw fees proportional to their volume. If the ratio exceeds the allowlist
threshold (default 0.90 — more than 90% of pool volume was extracted as fees, which is anomalous
even for legitimate protocols), Signal A fires regardless of established-protocol status.

---

### Signal B — Authority Rotation Alert

**Precise signal definition:** A `SetAuthority { authority_type: WithdrawWithheldTokens }` instruction
is recorded within `authority_rotation_window_days` (default 30d), where the new authority wallet
either:
1. Has a first SOL receipt within `fresh_wallet_funding_hours` (default 48h) before the rotation
   instruction (disposable wallet pattern), AND/OR
2. The previous authority had been active for fewer than `min_authority_tenure_days` (default 7d)
   (rapid rotation — consistent with weekly key cycling to evade tenure guards).

**Confidence formula (Signal B):**

```
fresh_wallet_bonus    = if authority_is_fresh_wallet { 0.20 } else { 0.0 }
rapid_rotation_bonus  = if prev_authority_tenure_days < min_authority_tenure_days { 0.15 } else { 0.0 }

conf = min(0.75, 0.40 + fresh_wallet_bonus + rapid_rotation_bonus)
```

At minimum (rotation with no fresh-wallet or rapid-rotation signals): `conf = 0.40`, severity Info.
With fresh wallet only: `conf = min(0.75, 0.60)`, severity Medium.
With rapid rotation only: `conf = min(0.75, 0.55)`, severity Medium.
With both: `conf = min(0.75, 0.75) = 0.75`, severity High.

**Why capped at 0.75:** Signal B is predictive, not confirmatory. A rotation followed by no
extraction is a false alarm. The 0.75 cap reserves the High band for the composite (A+B) case
where rotation is followed by confirmed extraction — an operational kill-chain signal.

**Established-protocol suppression:** NOT applied. Authority rotation on an established protocol
treasury is still operationally suspicious. Signal B fires regardless of `is_established_protocol`.
The evidence bundle provides full context for a human reviewer to dismiss it if appropriate.

---

### Signal A+B Composite — Rotation Followed by Extraction

When BOTH Signal A fires AND Signal B rotation was recorded within the same evaluation window
(i.e., the rotation event `block_time` is within `[ctx.window.start, ctx.window.end]`), D07
emits the Signal A event with confidence upgraded:

```
composite_conf = min(0.95, signal_a_conf + 0.10)
```

This composite represents the full operational kill-chain: a new disposable authority was installed,
then extraction was executed. The 0.95 cap is one step below 1.0 to preserve the invariant that
confidence 1.0 is reserved for simulation-confirmed results (D01 convention).

When the composite fires:
- Emit one `AnomalyEvent` for Signal A (upgraded confidence, composite flag in evidence).
- Emit one `AnomalyEvent` for Signal B (its own confidence, unchanged).
- The scoring crate receives two events and can combine them.

---

## 7. Threshold Table

| Config key | Default | Value rationale | Prior art |
|------------|---------|-----------------|-----------|
| `withdraw_withheld.min_extraction_events` | 3 | Single extraction events can occur in legitimate protocol fee collection. Three events within the detection window indicates an operational pattern rather than a one-off treasury operation. Unverified-heuristic; calibrate from corpus in Sprint 6. | Design derivation; no published Solana-specific corpus |
| `withdraw_withheld.min_cumulative_withdraw_usd` | 1000 | $1,000 USD filters noise from micro-extractions (dust accounts, low-value tokens). Calibrated to the D02 `min_pool_usd` floor (Chainalysis 2025) for consistency. Any scam token below $1K extraction has negligible impact on bot-trader position. Unverified-heuristic. | Chainalysis 2025 dust-filter convention; design derivation |
| `withdraw_withheld.authority_rotation_window_days` | 30 | Signal B looks back 30 days for rotation events. Consistent with D02's `minimum_lock_horizon_days` (30d) and D06's `hidden_mint_window_days` (30d) for regime coherence. An attacker rotating weekly every 7 days will have ≥4 rotations in 30d — detectable as rapid rotation. | Design derivation; consistent with D02/D06 windows |
| `withdraw_withheld.min_authority_tenure_days` | 7 | Authorities active fewer than 7 days before rotation are classified as "disposable." 7 days matches the D06 `mint_authority_grace_period_days` heuristic: legitimate projects take longer to establish key management practices. Unverified-heuristic. | D06 grace period analogy; Sun et al. 2024 §4 authority rotation pattern |
| `withdraw_withheld.min_withheld_at_rotation_usd` | 500 | The minimum accumulated withheld value at the time of a rotation for Signal B to fire with elevated confidence. If withheld balance is below $500 at rotation time, the fresh-wallet bonus is NOT applied (rotation with no accumulated value is low-risk). Unverified-heuristic. | Design derivation; calibrate Sprint 6 |
| `withdraw_withheld.fresh_wallet_funding_hours` | 48 | A wallet that received its first SOL within 48h before being set as `withdraw_withheld_authority` is classified as a disposable key. 48h is long enough to capture same-day disposable wallet creation but short enough to distinguish from normal key rotation cadences. Consistent with the D01 honeypot review disposable-wallet analysis. | E-D02-11 review; design derivation |
| `withdraw_withheld.detection_window_hours` | 168 | 7-day detection window for Signal A extraction event accumulation. Long enough to catch slow-drip extractors (E-D07-1) while remaining operationally relevant (a 7-day-old extraction is still actionable for the bot-trader's open positions). | D04/D05 7-day window convention; consistent with sprint review cycle |
| `withdraw_withheld.cross_detector_composite_enabled` | true | Enable Signal C evidence-key emission for the scoring crate. Can be disabled in test environments or when D01 output is unavailable. | Design derivation |
| `withdraw_withheld.established_protocol_fee_extraction_allowlist_pct` | 0.90 | Legitimate Token-2022 protocols (e.g., a PayPal treasury authority on PYUSD) withdraw fees proportional to their transaction volume. An extraction-to-pool-volume ratio above 90% is anomalous even for established protocols — it implies the entire pool's swap volume was captured as fees, which contradicts normal AMM economics (the fee rate would need to be ~100% basis points to achieve this). The 90% threshold is deliberately permissive to avoid suppressing legitimate protocols while still catching the extreme end of fee extraction relative to volume. Unverified-heuristic; no published calibration point. | Design derivation; D06 established-protocol asymmetry pattern |

All thresholds are published in `config/detectors.toml` under `[withdraw_withheld.*]`. See §13
for the full TOML stub.

---

## 8. Confidence Composition Summary

| Scenario | Formula | Conf range | Severity |
|----------|---------|------------|----------|
| Signal A alone (exact authority, low count/USD) | `0.60 + 0.0 + 0.0 = 0.60` | 0.60 | Medium |
| Signal A alone (exact authority, high count/USD) | `min(0.90, 0.60 + 0.15 + 0.15) = 0.90` | 0.90 | Critical |
| Signal A alone (unknown authority) | subtract 0.10 from above | 0.50–0.80 | Low–High |
| Signal B alone (no bonuses) | `0.40` | 0.40 | Info |
| Signal B alone (fresh wallet) | `0.60` | 0.60 | Medium |
| Signal B alone (rapid rotation + fresh wallet) | `0.75` | 0.75 | High |
| Signal A+B composite (rotation + extraction) | `min(0.95, signal_a_conf + 0.10)` | 0.70–0.95 | High–Critical |

**Severity mapping** via `severity_from_confidence` (consistent with all D01–D06 conventions):
- `0.0 ≤ conf < 0.40` → `Info`
- `0.40 ≤ conf < 0.60` → `Info` (low bound of actionable range)
- `0.60 ≤ conf < 0.75` → `Medium`
- `0.75 ≤ conf < 0.90` → `High`
- `0.90 ≤ conf ≤ 1.00` → `Critical`

**Priority ordering for a single `evaluate()` call:**
1. If Signal A fires AND Signal B is present in window → emit Signal A event (composite confidence);
   emit Signal B event (own confidence). Two events.
2. If Signal A fires, Signal B absent → emit Signal A event only. One event.
3. If Signal B fires, Signal A absent → emit Signal B event only. One event.
4. If neither fires → empty `Vec<AnomalyEvent>`.

---

## 9. `is_established_protocol` Policy Per Signal

The asymmetric suppression contract from `token_status.rs` (module-level doc, P4-0 asymmetric
contract) applies to D07 as follows:

| Signal | Type | `is_established_protocol = true` | `is_established_protocol = false` |
|--------|------|----------------------------------|-----------------------------------|
| A — Extraction event | Event-based (observed extraction) | **CONDITIONALLY suppressed**: suppressed ONLY when `extraction_usd / pool_volume_usd_in_window <= established_protocol_fee_extraction_allowlist_pct (0.90)`. If ratio > 0.90, fires regardless of established-protocol status — even legitimate protocols should not extract more than 90% of pool volume as fees. | Fires normally per §6. |
| B — Authority rotation | Event-based (observed instruction) | **NOT suppressed**. Rotation is operationally suspicious regardless of token provenance. Signal B fires at standard confidence formula. Evidence note: `"established_protocol = true; Signal B not suppressed per design 0012 §9"`. | Fires normally per §6. |
| A+B composite | Composite | Same conditional suppression as Signal A. The composite only fires if Signal A fires; if Signal A is suppressed, the composite is also suppressed. Signal B sub-event is not suppressed regardless. | Fires normally. |

**Why Signal A is conditionally (not fully) suppressed for established protocols:**

Full suppression would allow a compromised PYUSD `withdraw_withheld_authority` (e.g., a malicious
PayPal employee or a key compromise) to drain fees invisibly. The extraction-to-volume ratio check
preserves observability for genuine anomalies while suppressing routine fee collection operations.
At a 0.90 ratio, a legitimate protocol that sweeps 100% of pool volume as fees would fire — which
is the correct behavior (even legitimate protocols cannot justify a 100% fee relative to volume).

**Why Signal B is never suppressed:**

Authority key rotation is an operational event that warrants human review on any token,
established or not. An established protocol rotating their `withdraw_withheld_authority` to a
fresh wallet funded 2 hours earlier should surface as an alert for the custody consumer's
compliance team even if the token itself is legitimate. D07 emits the evidence; the consumer
decides the disposition.

---

## 10. Evidence Schema

All keys are prefixed `withdraw_withheld/`. String-encoded values follow the `AnomalyEvent.evidence`
contract per `docs/designs/0003-detector-trait.md` §Evidence.

**Signal A evidence:**

| Key | Type | Values | Meaning |
|-----|------|--------|---------|
| `withdraw_withheld/signal` | String (notes) | `"extraction_event"` | Which signal triggered this event |
| `withdraw_withheld/extraction_event_count` | Decimal | Integer ≥ 0 | Count of `WithdrawWithheld*` instructions in window |
| `withdraw_withheld/cumulative_withdrawn_raw` | String | u128 decimal string | Total raw token units extracted across all events in window |
| `withdraw_withheld/cumulative_withdrawn_usd` | Decimal | Decimal string, `"0"` if price unavailable | USD value of total extraction in window |
| `withdraw_withheld/authority_address` | Address (addresses vec) | Base58 pubkey | The `withdraw_withheld_authority` address at time of latest extraction |
| `withdraw_withheld/authority_is_fresh_wallet` | Decimal | `"0"` or `"1"` | `"1"` if authority received its first SOL within `fresh_wallet_funding_hours` before rotation; `"0"` otherwise or if sidecar unavailable |
| `withdraw_withheld/authority_tenure_days` | Decimal | Integer or `-1` | Days the current authority has held the role; `-1` if no rotation history available |
| `withdraw_withheld/latest_extraction_txs` | String (notes) | JSON array of up to 5 tx_hash strings | Most recent extraction transaction hashes |
| `withdraw_withheld/rotation_detected` | Decimal | `"0"` or `"1"` | `"1"` if a rotation event was found in the detection window alongside extraction |
| `withdraw_withheld/rotation_tx_hash` | String (notes) | Base58 tx_hash or `""` | Transaction that executed the rotation; empty if no rotation |
| `withdraw_withheld/transfer_fee_bps` | Decimal | Integer 0–10000 | Current `transfer_fee.fee_bps` from `TokenMeta`; surfaces D01 overlap context |
| `withdraw_withheld/combined_with_d01_s2` | Decimal | `"0"` or `"1"` | `"1"` if `transfer_fee_bps > sell_tax_threshold_bps` (D01 S2 would fire); scoring crate uses this for Signal C composite |
| `withdraw_withheld/established_protocol_suppression_skipped_reason` | Decimal | `"0"` or `"1"` | `"1"` if established-protocol suppression was checked but overridden because extraction ratio > allowlist pct; `"0"` if not applicable or suppression applied normally |
| `withdraw_withheld/authority_match` | String (notes) | `"exact"` or `"unknown"` | Whether the instruction signer matched the recorded `withdraw_withheld_authority` |

**Signal B evidence** (in addition to authority-related keys above):

| Key | Type | Values | Meaning |
|-----|------|--------|---------|
| `withdraw_withheld/signal` | String (notes) | `"authority_rotation"` | Which signal triggered this event |
| `withdraw_withheld/rotation_tx_hash` | String (notes) | Base58 tx_hash | Transaction that executed the rotation |
| `withdraw_withheld/prev_authority_address` | Address (addresses vec) | Base58 pubkey or `"none"` | Previous `withdraw_withheld_authority` |
| `withdraw_withheld/prev_authority_tenure_days` | Decimal | Integer | Days the previous authority held the role |
| `withdraw_withheld/withheld_at_rotation_usd` | Decimal | Decimal string | USD value of accumulated withheld balance at time of rotation; `"0"` if price unavailable |
| `withdraw_withheld/rotation_within_fresh_wallet_hours` | Decimal | Integer | Hours between the new authority's first SOL receipt and the rotation instruction; `-1` if sidecar unavailable |

**`Evidence.tx_hashes`:** For Signal A, include the latest extraction tx_hash and up to 4 more from
the window. For Signal B, include the rotation tx_hash.

**`Evidence.addresses`:** For Signal A, include `authority_address`. For Signal B, include both
`authority_address` (new) and `prev_authority_address` (previous).

**`Evidence.notes`:** Human-readable summary string, e.g.:
- Signal A: `"D07: 5 WithdrawWithheld instructions; $12,340 extracted in 168h window; authority 7Gab...XkQ (exact match, tenure 3d); transfer_fee_bps=5000"`
- Signal B: `"D07: withdraw_withheld_authority rotated to fresh wallet (funded 4h before rotation); prev authority tenure 2d; $8,200 withheld at rotation"`

---

## 11. Instruction Decoding Dependency — Precondition for D07

D07 is entirely dependent on decoded Token-2022 instructions stored in a new PostgreSQL table.
This is the most significant implementation prerequisite. The developer MUST implement the following
before implementing the detector itself.

### 11.1 New Storage Table: `token2022_instructions` (Migration V00007)

```sql
-- Migration V00007: Token-2022 instruction event store.
-- Stores decoded WithdrawWithheld*, SetAuthority (WithdrawWithheldTokens),
-- and HarvestWithheldTokensToMint instructions indexed by (chain, mint, block_time).
--
-- ADR 0002: Postgres-only. No ClickHouse equivalent — instruction events are
-- low-volume (one row per instruction, not per token account transfer).

CREATE TABLE IF NOT EXISTS token2022_instructions (
    id             BIGSERIAL       PRIMARY KEY,
    chain          TEXT            NOT NULL,
    mint           TEXT            NOT NULL,
    tx_hash        TEXT            NOT NULL,
    block_height   BIGINT          NOT NULL,
    block_time     TIMESTAMPTZ     NOT NULL,
    instruction_kind TEXT          NOT NULL,  -- 'withdraw_withheld_from_accounts' | 'withdraw_withheld_from_mint' | 'harvest_withheld_to_mint' | 'set_authority_withdraw_withheld'
    authority      TEXT,                      -- signer for withdraw/set_authority; NULL for harvest (permissionless)
    destination    TEXT,                      -- destination token account for withdraw instructions; NULL otherwise
    amount_raw     NUMERIC,                   -- token units extracted; NULL for set_authority instructions
    amount_usd     NUMERIC,                   -- USD value at block_time from indexer price feed; NULL if no price
    new_authority  TEXT,                      -- populated for set_authority_withdraw_withheld; new authority pubkey or 'none' if revoked
    prev_authority TEXT,                      -- previous authority pubkey; populated for set_authority_withdraw_withheld
    ingested_at    TIMESTAMPTZ     NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_t22_instructions_chain_mint_time
    ON token2022_instructions (chain, mint, block_time);

CREATE INDEX IF NOT EXISTS idx_t22_instructions_kind
    ON token2022_instructions (instruction_kind, block_time);
```

### 11.2 Which Instructions Must Be Decoded

The indexer's chain-adapter or a new `crates/spl-token-adapter` crate MUST decode the following
Token-2022 instructions and emit rows into `token2022_instructions`:

| Instruction | Discriminator (Token-2022 program) | Fields to extract |
|-------------|-----------------------------------|-------------------|
| `WithdrawWithheldTokensFromMint` | instruction data byte index 0 = 27 | authority (account[0]), destination (account[1]), amount_raw (from account state delta) |
| `WithdrawWithheldTokensFromAccounts` | instruction data byte index 0 = 28 | authority (account[0]), destination (account[1]), source_accounts (account[2..N]), amount_raw (sum of withheld balances on source accounts pre-instruction) |
| `HarvestWithheldTokensToMint` | instruction data byte index 0 = 29 | mint (account[0]), source_accounts (account[1..N]); no authority (permissionless) |
| `SetAuthority { authority_type: WithdrawWithheldTokens }` | standard Token-2022 SetAuthority (byte 6), authority_type byte identifies field | current_authority (account[1]), new_authority (instruction data) |

**Architecture recommendation:** The decode logic belongs in a new `crates/spl-token-adapter` crate
(or as a submodule of `crates/chain-adapter/src/solana/token2022.rs`). Architect's call on crate
boundary. The important constraint is that D07 reads from `token2022_instructions` as a
first-class storage table — the detector is NOT responsible for instruction decoding and MUST
return `Err(DetectorError::MissingDependencyData { dependency: "token2022_instructions" })` if the
table is empty for the queried (chain, mint, window) combination.

### 11.3 CPI Considerations

Token-2022 `WithdrawWithheld*` instructions may be invoked via CPI (Cross-Program Invocation) from
a wrapper program. The instruction decoder must inspect the inner instructions of each transaction
(using the Solana `getTransaction` RPC response's `meta.innerInstructions` field) in addition to
the top-level instructions. A `WithdrawWithheld*` CPI has the same instruction data layout as
a direct call; the difference is that the calling program ID appears as the outer instruction,
not the Token-2022 program ID (`TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb`).

The authority recorded in `token2022_instructions` for a CPI call should be the account designated
as the `authority` in the Token-2022 instruction accounts list (account index 0 for `WithdrawWithheld*`),
NOT the calling program. The calling program is recorded in a separate `cpi_program` column if
the developer chooses to add it; that column is optional for the D07 MVP.

---

## 12. Algorithm (PostgreSQL Queries)

Queries are written in PostgreSQL dialect per ADR 0002. Saved as `docs/queries/d07_withdraw_withheld.sql`.

### Query W1 — Fetch Extraction Events

Fetches all `WithdrawWithheld*` instruction rows for a given mint within the detection window.

Parameters:
- `$1 chain TEXT`
- `$2 mint TEXT`
- `$3 window_start TIMESTAMPTZ`
- `$4 window_end TIMESTAMPTZ`

Returns: rows with `instruction_kind`, `authority`, `amount_raw`, `amount_usd`, `tx_hash`,
`block_time`, `block_height`.

### Query W2 — Fetch Authority Rotation History

Fetches all `set_authority_withdraw_withheld` rows within `authority_rotation_window_days`.

Parameters:
- `$1 chain TEXT`
- `$2 mint TEXT`
- `$3 lookback_start TIMESTAMPTZ` — `window_end - authority_rotation_window_days`
- `$4 window_end TIMESTAMPTZ`

Returns: rows with `authority` (previous), `new_authority`, `tx_hash`, `block_time`, and (via
LEFT JOIN to a wallet_funding_events table if present) `new_authority_first_sol_time`.

### Query W3 — Fetch Cumulative Withheld USD

Aggregation over W1 rows to produce a single cumulative USD total. Implemented as a CTE over W1
rather than a separate table scan.

Parameters: same as W1.

Returns: `SUM(amount_raw)` as `cumulative_raw`, `SUM(amount_usd)` as `cumulative_usd`,
`COUNT(*)` as `event_count`.

Full SQL text is in `docs/queries/d07_withdraw_withheld.sql`.

---

## 13. Failure Modes and Fallbacks

| Condition | Behavior | Evidence annotation |
|-----------|----------|---------------------|
| `TokenMeta.transfer_fee = None` | `Err(DetectorError::InsufficientBaseline { detector_id: "withdraw_withheld_drain", reason: "not a Token-2022 mint with TransferFeeConfig" })` | No event emitted; no log at Info or above |
| `token2022_instructions` table empty for (chain, mint, window) | `Err(DetectorError::MissingDependencyData { dependency: "token2022_instructions" })` — NOT retried automatically; scheduler emits a `tracing::warn!` and marks the token as `pending_decoder_dependency` | No Signal A or B event; scoring crate sees `MissingDependencyData` and should NOT penalize the token for the missing signal |
| `amount_usd = NULL` on all rows in W1/W3 | USD sum is NULL; fall back to event-count-only evaluation for Signal A. `min_cumulative_withdraw_usd` gate is skipped (event count gate alone governs). Emit `cumulative_withdrawn_usd = "0"` with evidence note `"price_data_unavailable"` | Evidence note in `withdraw_withheld/cumulative_withdrawn_usd` |
| Wallet funding sidecar absent for new authority | `authority_is_fresh_wallet = "0"`; `fresh_wallet_bonus = 0.0` in Signal B formula; evidence note `"wallet_funding_sidecar_unavailable"` | Reduces Signal B confidence but does not block it |
| `pool_volume_usd_in_window = 0` (no trades) | Skip the established-protocol extraction-ratio check; do not apply `established_protocol_fee_extraction_allowlist_pct` guard; proceed with standard Signal A evaluation | Evidence note `"pool_volume_zero_skip_ep_ratio_check"` |
| Signal A fires but authority is NOT `is_established_protocol` AND `extraction_usd / pool_volume_usd > 0.90` | Fire Signal A regardless — the ratio override applies | `established_protocol_suppression_skipped_reason = "1"` |
| `cross_detector_composite_enabled = false` | Skip `combined_with_d01_s2` evidence key population; emit Signal A/B events normally without the D01 linkage key | No error; no functional change to confidence |
| Query W1 timeout | `Err(DetectorError::TransientQuery)` — retried by scheduler per `DetectorError::is_retryable()` | Scheduler handles retry |

---

## 14. Adversarial Evasions

### E-D07-1 — Slow Drip Below Per-Event Threshold

**Description:** The attacker calls `WithdrawWithheldTokensFromAccounts` once per day extracting
$200 USD worth of withheld fees. Over 7 days: 7 events, $1,400 cumulative. Signal A fires only
if event_count ≥ 3 (yes, met at day 3) AND cumulative_usd ≥ $1,000 (met at day 5). Detection
occurs at day 5, but the attacker has already extracted $1,000 before detection.

**Evasion refined:** If the attacker spaces extractions to stay just below $1,000 cumulative
within any 7-day window by crossing weeks (extract $499 in week 1, $499 in week 2), and counts
stay below 3 per window, Signal A never fires.

**Catch/miss:** PARTIAL MISS — slow drip across consecutive detection windows may not accumulate
within a single 7-day rolling window. The event-count gate catches daily extraction patterns
(3 events in 3 days) but the USD gate may miss week-straddling extractors.

**Phase fix:** Add a 30-day cumulative query variant (extend W3 to support configurable
`cumulative_window_days` parameter). Calibrate `min_cumulative_withdraw_usd_30d = 3000` as a
companion threshold. Phase 3 enhancement.

---

### E-D07-2 — Cross-Authority Harvest Evasion (Tenure Gaming)

**Description:** The deployer rotates `withdraw_withheld_authority` to a new wallet every 8 days
(one day above `min_authority_tenure_days = 7`). Each authority holds the role for 8 days before
being rotated. Signal B's `rapid_rotation_bonus` requires `prev_authority_tenure_days < 7` — with
8-day tenures, the bonus is never applied. Signal B fires at base confidence 0.40 only (Info
severity), which many consumers filter out.

**Catch/miss:** PARTIAL MISS — Signal B fires at 0.40 but without the rapid-rotation bonus, the
confidence is below the actionable threshold for most consumers. The attack requires the deployer
to maintain rotation discipline (exactly 8+ days per rotation).

**Phase fix:** Add a rotation-count feature: if the same mint has ≥ 3 distinct rotations within
`authority_rotation_window_days`, upgrade Signal B confidence by +0.15 (rotation-frequency bonus)
regardless of individual tenure length. This catch is a Phase 3 enhancement to Query W2.

---

### E-D07-3 — Jupiter-Routed Extraction (Aggregator Wrapper)

**Description:** The authority calls `WithdrawWithheldTokensFromAccounts` through a Jupiter
aggregator route, wrapping the Token-2022 instruction in a multi-hop transaction. The outer
instruction is a Jupiter program invocation; the Token-2022 instruction appears as an inner
instruction (CPI). The indexer must inspect `meta.innerInstructions` to find it.

**Catch/miss:** CATCH if the indexer decodes inner instructions (§11.3). MISS if the indexer
only processes top-level instructions. The §11.3 spec explicitly requires inner instruction
inspection for CPI coverage. If the CPI decoder is not implemented, this evasion is a full miss.

**Phase fix:** §11.3 CPI handling is a precondition, not a Phase 3 enhancement. The developer
acceptance checklist item #4 covers this.

---

### E-D07-4 — CPI-Proxied Withdraw via Deployer-Controlled Program

**Description:** The deployer deploys a thin wrapper program that holds the `withdraw_withheld_authority`
keypair as a program-derived address (PDA). The program exposes an instruction (`drain_fees`) that
calls `WithdrawWithheldTokensFromAccounts` via CPI. The outer instruction signer is the deployer's
user wallet, but the Token-2022 authority account is the program's PDA (not the deployer's wallet
directly). The `authority_match` field in D07 would record `"unknown"` because the PDA does not
match the previously-recorded `withdraw_withheld_authority` (which was set to the deployer wallet
before they rotated it to the PDA).

**Catch/miss:** PARTIAL CATCH — Signal A fires with `authority_match = "unknown"` and confidence
reduced by 0.10. The extraction is still detected; attribution is degraded.

**Phase fix:** Phase 3 graph analysis: when `authority_match = "unknown"`, check if the
instruction signer account is a PDA owned by a program whose upgrade authority is the token
deployer. If so, reclassify as `authority_match = "deployer_proxy"` and restore full confidence.

---

### E-D07-5 — Legitimate-Looking MultiSig Extraction

**Description:** The deployer sets `withdraw_withheld_authority` to a 3-of-5 Squads multisig
program that they fully control (all 5 signers are funded by the deployer). The multisig appears
as a legitimate governance mechanism. Signal B fires on the rotation to the multisig. Signal A
fires on the actual extraction. The multisig's `authority_is_fresh_wallet` check may not fire if
the multisig program account was created more than 48h before the rotation (the deployer can
create it ahead of time).

**Catch/miss:** PARTIAL CATCH — Signal A fires on extraction. Signal B fires on rotation but
`fresh_wallet_bonus` may not apply (multisig program older than 48h). The multisig authority
degrades evidence quality — the `authority_address` in evidence is the multisig program account,
not the deployer's EOA.

**Phase fix:** Phase 3: detect when `withdraw_withheld_authority` is a Squads multisig and check
whether the multisig member wallets share a funding source with the token deployer. Requires
wallet graph.

---

### E-D07-6 — On-Chain Burn Post-Extraction (Treasury Laundering)

**Description:** After extracting withheld fees to their wallet, the deployer burns 20% of the
extracted tokens (to a zero address). This appears as a "deflationary" treasury operation. D06
Signal B (burn event) fires, but at the burner address (the deployer). D07 Signal A has already
fired for the extraction. The burn makes the deployer's action look like "responsible token
management" to human reviewers scanning individual detector outputs.

**Catch/miss:** CATCH for D07 Signal A (extraction is detected). D06 Signal B also fires on the
burn. The scoring crate receiving BOTH D07 Signal A AND D06 Signal B from the same deployer address
should elevate combined severity. This is a scoring-layer concern. D07's evidence includes the
extraction tx_hash, which a reviewer can cross-reference with the burn tx_hash.

**Phase fix:** No D07-specific fix required. Document as a scoring-crate aggregation rule: D07
Signal A + D06 Signal B from the same authority address within 24h = escalate composite severity.

---

### E-D07-7 — Whitelist Override via Fee-Rate Zero Reset

**Description:** The deployer temporarily sets `transfer_fee_basis_points = 0` via
`SetTransferFee` before calling `WithdrawWithheldTokensFromAccounts`. At the moment of extraction,
the observable fee rate is 0. A naive detector that checks fee_bps at extraction time would miss
the high-fee context. D01 Signal S2 would not fire on a zero-fee token.

**Catch/miss:** PARTIAL CATCH — D07 Signal A fires regardless of the current fee rate (it monitors
the instruction, not the fee). The `withdraw_withheld/transfer_fee_bps` evidence key captures the
fee rate AT THE TIME of D07 evaluation (which may be 0 after the reset). D01 Signal S2 may not
co-fire, setting `combined_with_d01_s2 = "0"`.

**Phase fix:** The indexer should store the historical `fee_bps` at the time of the extraction
instruction (captured from the mint account state in the transaction's pre-execution state).
This requires storing `fee_bps_at_extraction` as an additional column in `token2022_instructions`.
Phase 3 enhancement; not required for MVP.

---

### E-D07-8 — LP-Add Laundering of Extracted Fees

**Description:** After extracting withheld fees to their token account, the deployer adds the
extracted tokens back as LP liquidity on Raydium. The extraction (D07 Signal A) and the LP add
(D02 Signal A — `PoolEventKind::Mint`, not covered by D02 Signal A which only looks at Burn
events) appear to cancel each other out to a human reviewer. The pool's liquidity increases,
which looks positive. In reality, the deployer extracted value (withheld fees as tokens) and
re-added it as LP — but they now hold LP tokens (withdrawable at will) instead of the original
withheld balance (locked in the fee mechanism).

**Catch/miss:** CATCH for D07 Signal A (extraction is detected). D02 Signal B (latent risk) may
not fire because LP burn percentage increases. The combined signal is D07 Signal A (extraction
detected) + LP add with deployer as LP provider (D02 Signal B: single-provider, 0% burned). The
scoring crate should treat D07 Signal A + deployer-addressed LP add as an elevated risk.

**Phase fix:** Scoring crate rule: D07 Signal A within 24h of a deployer-attributed LP add event
= escalate to `High` regardless of individual signal confidence. Phase 3 enhancement.

---

## 15. Cross-Detector Relations

### D07 ← D01 Signal S2 (Transfer fee above threshold)

D01 Signal S2 fires at listing time when `transfer_fee_bps > sell_tax_threshold_bps` (default
3000 bps = 30%). D01 S2 is the static precondition that makes D07 relevant: a token with a high
transfer fee will accumulate meaningful withheld balances. D07 checks whether
`transfer_fee_bps > sell_tax_threshold_bps` and sets `combined_with_d01_s2 = "1"` in evidence.
The scoring crate treats `combined_with_d01_s2 = "1"` + `D07 Signal A` as the Signal C composite,
elevating severity by one band.

**Direction of dependency:** D07 reads `TokenMeta.transfer_fee.fee_bps` directly. It does NOT
read D01's output events. The linkage is one-directional evidence annotation, not a runtime call.

### D07 ← D02 Signal B (Latent LP risk)

If D02 Signal B is firing (deployer controls LP with low burn percentage) AND D07 Signal A fires
(deployer is extracting withheld fees), the deployer has two independent value-extraction vectors.
The scoring crate should escalate the combined alert to Critical. D07 does NOT read D02 output;
this is a scoring-crate aggregation concern.

### D07 ← D06 Signal A (Active mint authority)

D06 Signal A fires when `mint_authority.is_some()`. A token with BOTH active mint authority AND
active `withdraw_withheld_authority` has two orthogonal attack paths (hidden mint + fee drain).
D07 evidence includes `transfer_fee_bps` which is sufficient context for the scoring crate to
identify the overlap. No D07-specific cross-detector logic required.

### D07 → Scoring Crate (Signal C composite)

D07 emits `combined_with_d01_s2 = "1"` when D01 S2 would fire. The scoring crate:
1. Receives D07 Signal A event with `combined_with_d01_s2 = "1"`.
2. Checks whether a D01 Signal S2 event exists for the same token in the same window.
3. If both present: elevate severity by one band above what D07 Signal A alone assigned.

The scoring crate's implementation of this rule is outside D07's scope. D07's obligation is to
correctly populate `combined_with_d01_s2`.

---

## 16. Fixture Corpus (6 fixtures: 3 positive + 3 negative)

Root: `research/fixtures/withdraw_withheld/`. All fixtures MUST include `"_fixture_meta"` with
`"detector": "D07"`, `"expected_signals"`, `"expected_confidence_gte"`, and `"synthetic": true`
if not a live on-chain fetch.

---

### POS-D07-01 — High-Fee Extraction (Signal A + Signal C Composite)

**Type:** Synthetic positive
**File:** `research/fixtures/withdraw_withheld/SYNTH_D07_POS_001_high_fee_extraction.json`
**Expected signals:** Signal A fires. `combined_with_d01_s2 = "1"` (fee_bps = 5000 > 3000).
No Signal B (no rotation in window).

**State:**
```json
{
  "_fixture_meta": {
    "detector": "D07",
    "expected_signals": ["extraction_event"],
    "expected_confidence_gte": "0.75",
    "expected_severity": "High",
    "synthetic": true,
    "rationale": "Token-2022 mint with 5000 bps transfer fee. 5 extraction events in 24h window. Authority = deployer. combined_with_d01_s2 = 1. Signal A fires; no rotation in window so no Signal B."
  }
}
```

**TokenMeta fields:**
- `transfer_fee: { fee_bps: 5000, max_fee_raw: "999999999999", authority: "DeployerWalletAAAA..." }`
- `token_program: "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"` (Token-2022)
- `verification: { jup_verified: false, jup_strict: false }`, `rugcheck_score: 88`

**Simulated `token2022_instructions` rows (5 events, same authority):**
- 5 `withdraw_withheld_from_accounts` rows, each `amount_usd: 400.00`
- Cumulative: 5 events, $2,000 USD
- All `authority = "DeployerWalletAAAA..."`

**Expected output:**
- Signal A: `conf = min(0.90, 0.60 + min(0.15, (5-3)*0.03) + (ln(2.0)*0.10)) = min(0.90, 0.60 + 0.06 + 0.069) = 0.729`, severity High.
- `combined_with_d01_s2 = "1"` (5000 > 3000).
- `rotation_detected = "0"`, no Signal B.

---

### POS-D07-02 — Authority Rotation Followed by Extraction (Signal B + Signal A Composite)

**Type:** Synthetic positive
**File:** `research/fixtures/withdraw_withheld/SYNTH_D07_POS_002_rotation_then_extraction.json`
**Expected signals:** Signal B fires (rapid rotation + fresh wallet). Signal A fires within same window.
Composite: Signal A confidence upgraded.

**State:**
- `transfer_fee: { fee_bps: 3500, max_fee_raw: "500000000000", authority: "OldAuthorityAAAA..." }`

**Simulated instructions:**
- `set_authority_withdraw_withheld`: `block_time = window_start + 1h`,
  `prev_authority = "OldAuthorityAAAA..."` (tenure 3d = below 7d threshold),
  `new_authority = "FreshWalletBBBB..."` (first SOL receipt 2h before rotation = below 48h threshold),
  `withheld_at_rotation_usd = 8200`
- 3 `withdraw_withheld_from_accounts` rows after rotation: total $5,500 USD, authority = "FreshWalletBBBB..."

**Expected output:**
- Signal B: `conf = min(0.75, 0.40 + 0.20 + 0.15) = 0.75`, severity High.
- Signal A: base `conf = 0.60 + 0.0 + (ln(5.5)*0.10) = 0.60 + 0.170 = 0.770`
- Composite: `conf = min(0.95, 0.770 + 0.10) = 0.870`, severity Critical.
- `rotation_detected = "1"`, `authority_is_fresh_wallet = "1"`.

---

### POS-D07-03 — Repeated Extraction Pattern Over Multiple Days

**Type:** Synthetic positive (live fixture attempted; see note below)
**File:** `research/fixtures/withdraw_withheld/SYNTH_D07_POS_003_repeated_extraction.json`
**Note:** A live RugCheck query for tokens with `rugged=true` AND `transferFee.withdrawWithheldAuthority != null`
was attempted. As of 2026-04-21, RugCheck's live API returns `transferFee` in the JSON response
but does NOT expose whether extraction instructions were executed (no `withdraw_withheld_events`
endpoint). No confirmed live `withdraw_withheld` rug fixture was found. This fixture remains
synthetic until a real incident is documented. See `research/fixtures/solana-corpus-phase2.md` for
the RugCheck corpus scan methodology.

**State:** 8 extraction events over 7 days, 2 distinct authority wallets (one rotation mid-window),
cumulative $12,000 USD. fee_bps = 8000 (80%).

**Expected signals:** Signal A (high confidence), Signal B (rotation), composite.

**Expected output:**
- Signal A: `conf = min(0.90, 0.60 + min(0.15, (8-3)*0.03) + ln(12.0)*0.10) = min(0.90, 0.60 + 0.15 + 0.249) = 0.90`, severity Critical.
- Signal B: `conf = 0.75` (assuming fresh wallet + rapid rotation), severity High.
- Composite: `conf = min(0.95, 0.90 + 0.10) = 0.95`, severity Critical.

---

### NEG-D07-01 — PYUSD (Established Protocol, Fee Extraction Below Volume Ratio Threshold)

**Type:** Real negative fixture (reuse `research/fixtures/` PYUSD entry or create new)
**File:** `research/fixtures/withdraw_withheld/NEG_D07_001_pyusd_established.json`
**Expected signals:** Signal A suppressed (established protocol + ratio below 0.90). Signal B
fires if rotation occurred (but PYUSD PayPal treasury has stable authority — no rotation expected).

**State:**
- `mint: "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo"` (PYUSD)
- `transfer_fee: { fee_bps: 0, ... }` (PYUSD transfer fee is 0 bps as of 2026-04-21)
- `verification: { jup_verified: true, jup_strict: true }` → `is_established_protocol = true`

**Note:** PYUSD currently has 0 bps transfer fee, meaning `TokenMeta.transfer_fee.fee_bps = 0`.
D07 would return `Err(InsufficientBaseline { reason: "transfer_fee_bps = 0; no meaningful withheld balance accumulates" })`.
The fixture tests the established-protocol path where any extraction events that do exist fall
far below the established-protocol ratio threshold.

**Expected output:** `Err(InsufficientBaseline)` or no events (zero-fee token accumulates no
meaningful withheld balance). BELOW_THRESHOLD verdict.

---

### NEG-D07-02 — Token-2022 Without TransferFeeConfig Extension

**Type:** Synthetic negative
**File:** `research/fixtures/withdraw_withheld/NEG_D07_002_no_transfer_fee_extension.json`
**Expected signals:** None. `Err(InsufficientBaseline)`.

**State:**
- `token_program: "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"` (Token-2022 program)
- `transfer_fee: null` (Token-2022 mint without the TransferFeeConfig extension — e.g., a mint
  with only PermanentDelegate or TransferHook extension)

**Expected output:** `Err(DetectorError::InsufficientBaseline { reason: "not a Token-2022 mint with TransferFeeConfig" })`

**Test assertion:** `result.is_err()` and error variant is `InsufficientBaseline`. Zero events in output.

---

### NEG-D07-03 — Legacy SPL Token (Not Token-2022)

**Type:** Real negative fixture (reuse wSOL or BONK)
**File:** `research/fixtures/withdraw_withheld/NEG_D07_003_legacy_spl.json`
**Expected signals:** None. `Err(InsufficientBaseline)`.

**State:**
- `mint: "So11111111111111111111111111111111111111112"` (wSOL)
- `token_program: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"` (standard SPL Token)
- `transfer_fee: null`

**Expected output:** `Err(DetectorError::InsufficientBaseline { reason: "not a Token-2022 mint with TransferFeeConfig" })`

**Test assertion:** Same as NEG-D07-02. This fixture confirms that the token-program check fires
before any query is attempted, preventing spurious `token2022_instructions` table lookups on
standard SPL tokens.

---

## 17. Config Stub (TOML)

The developer MUST add the following block to `config/detectors.toml`:

```toml
[withdraw_withheld.min_extraction_events]
value     = 3
rationale = """Single extraction events can occur in legitimate protocol fee collection (e.g.,
              a protocol sweeping a small treasury fee monthly). Three events within the 7-day
              detection window indicates an operational extraction pattern rather than a one-off
              treasury operation. Classified as unverified-heuristic; calibrate from labelled
              corpus in Sprint 6 once real withdraw_withheld rug incidents are documented."""
refs      = ["D07/withdraw_withheld_drain"]

[withdraw_withheld.min_cumulative_withdraw_usd]
value     = 1000.0
rationale = """$1,000 USD filters noise from micro-extractions on dust-value tokens or from
              low-fee tokens where withheld balances accumulate slowly. Calibrated to match the
              D02 `min_pool_usd` floor (Chainalysis 2025: $1,000 as the dust filter for LP drain
              significance). Any extraction below $1,000 has negligible impact on bot-trader
              positions. Classified as unverified-heuristic; review against first live rug corpus."""
refs      = ["D07/withdraw_withheld_drain", "Chainalysis2025/rug-pull-base-rate"]

[withdraw_withheld.authority_rotation_window_days]
value     = 30
rationale = """Signal B looks back 30 days for rotation events. Consistent with D02
              `minimum_lock_horizon_days` (30d) and D06 `hidden_mint_window_days` (30d) for
              detection window coherence. A 30-day lookback covers at least 4 rotation cycles
              for an attacker rotating every 8 days (E-D07-2 evasion pattern)."""
refs      = ["D07/withdraw_withheld_drain"]

[withdraw_withheld.min_authority_tenure_days]
value     = 7
rationale = """Authorities holding the withdraw_withheld_authority role for fewer than 7 days
              before rotation are classified as disposable keys. 7 days is consistent with D06
              `mint_authority_grace_period_days` (Sun et al. 2024: non-rugged tokens revoke
              within 1-7 days; rugged tokens rarely revoke). A withdrawal authority active for
              fewer than 7 days has not established a track record of legitimate operation."""
refs      = ["D07/withdraw_withheld_drain", "Sun2024/hidden-mint-authority-rotation"]

[withdraw_withheld.min_withheld_at_rotation_usd]
value     = 500.0
rationale = """The minimum accumulated withheld value at the time of a Signal B authority rotation
              for the fresh_wallet_bonus to apply. Rotations when withheld balance is below $500
              carry lower immediate risk (the attacker has less to extract). $500 is half the
              Signal A USD threshold ($1,000), calibrated to surface pre-extraction rotations
              while filtering dust-value rotations. Unverified-heuristic."""
refs      = ["D07/withdraw_withheld_drain"]

[withdraw_withheld.fresh_wallet_funding_hours]
value     = 48
rationale = """A wallet that received its first SOL within 48 hours before being set as
              withdraw_withheld_authority is classified as a disposable key. 48 hours is long
              enough to capture same-day disposable wallet creation and allows for overnight
              key ceremonies, but short enough to distinguish from normal key rotation cadences
              where new wallets are funded and seasoned for several days before deployment.
              Consistent with the E-D02-11 review's disposable-wallet analysis."""
refs      = ["D07/withdraw_withheld_drain", "D02-review-E-D02-11/disposable-wallet"]

[withdraw_withheld.detection_window_hours]
value     = 168
rationale = """7-day detection window for Signal A extraction event accumulation. Consistent with
              D04/D05 7-day observation windows. Long enough to aggregate slow-drip extraction
              patterns (E-D07-1) and short enough to remain operationally actionable — a 7-day-old
              extraction event still affects open bot-trader positions in a token with active
              trading volume."""
refs      = ["D07/withdraw_withheld_drain"]

[withdraw_withheld.cross_detector_composite_enabled]
value     = true
rationale = """Enable Signal C evidence-key emission (combined_with_d01_s2) for the scoring crate.
              When true, D07 checks transfer_fee_bps against D01's sell_tax_threshold_bps and
              emits the linkage evidence key. Can be set to false in test environments where D01
              output is unavailable or in single-detector evaluation mode."""
refs      = ["D07/withdraw_withheld_drain"]

[withdraw_withheld.established_protocol_fee_extraction_allowlist_pct]
value     = 0.90
rationale = """Legitimate Token-2022 protocols may withdraw fees to treasury addresses. The
              extraction-to-pool-volume ratio cap at 0.90 (90%) means that if a protocol extracts
              more than 90% of its pool's trading volume in fees within the window, Signal A fires
              regardless of established-protocol status. Normal AMM economics make a >90% fee-to-
              volume ratio impossible at any fee rate below ~100%; this threshold catches only
              extreme extraction events while suppressing normal fee sweeps. Unverified-heuristic;
              no published calibration for this specific metric. Review after first live corpus."""
refs      = ["D07/withdraw_withheld_drain"]
```

---

## 18. Known Calibration Gaps (for Sprint 6 corpus)

The following open calibration questions MUST be addressed once the first live `withdraw_withheld`
rug corpus is assembled:

| Question | Gap | Proposed resolution |
|----------|-----|---------------------|
| Are there legitimate Token-2022 protocols with >3 `withdraw_withheld` events per 7 days? | `min_extraction_events = 3` may produce false positives on high-volume legitimate protocols. | Audit extraction patterns on all Token-2022 tokens with jup_strict=true. If any legitimate protocol exceeds 3 events/7d, raise threshold or add an established-protocol extraction-frequency allowlist. |
| Is $1,000 USD the right floor for micro-cap shitcoins? | A $50K pool with 50% fee accumulates $25K/day at moderate volume. $1,000 threshold fires quickly and correctly. For a $5K pool, $1,000 may take weeks. | Consider a floor expressed as % of pool liquidity (e.g., `min_cumulative_withdraw_pct_of_pool = 0.05`) as an alternative or companion gate. |
| How often is `withdraw_withheld_authority` != `transfer_fee_config_authority`? | Unknown — current `TokenMeta.transfer_fee.authority` only captures `transfer_fee_config_authority`. The `withdraw_withheld_authority` may be a different wallet. | The V00007 migration resolves this by explicitly storing `withdraw_withheld_authority` from the mint's TLV extension at mint creation time. |
| What is the false positive rate of Signal B on legitimate key rotation? | Unknown. No corpus of legitimate Token-2022 protocol key rotations exists as of 2026-04-21. | Build a negative fixture set from jup_strict Token-2022 tokens; run Signal B against their authority history. |

---

## 19. Design Gaps

### DG-D07-1 — `withdraw_withheld_authority` Not in TokenMeta

**Description:** `TokenMeta.transfer_fee.authority` stores the `transfer_fee_config_authority`
(who controls the fee rate), NOT the `withdraw_withheld_authority` (who controls extraction).
These are distinct fields in the Token-2022 `TransferFeeConfig` TLV extension. D07's Signal A
compares the instruction signer against the `withdraw_withheld_authority` stored in
`token2022_instructions` — which is correct — but the static metadata layer (`TokenMeta`) does
not expose the `withdraw_withheld_authority` for pre-query checks.

**Impact:** D07 cannot quickly reject a token as "withdraw_withheld_authority = None (revoked)"
without querying the `token2022_instructions` table. For tokens with revoked extraction authority,
the table query would return no `set_authority` rows, and D07 would correctly find no extraction
events. Performance impact: one additional table scan per evaluation cycle for tokens with
`transfer_fee.is_some()` but revoked extraction authority.

**Mitigation (MVP):** D07 proceeds with the table query for all tokens with
`TokenMeta.transfer_fee.is_some()`. The query is indexed on `(chain, mint, block_time)` and
returns quickly for tokens with no rows.

**Phase 3 resolution:** Add `withdraw_withheld_authority: Option<Address>` to `TokenMeta`
(requires `crates/common` extension and a `token-registry` enrichment pass). This enables D07 to
skip the table query entirely when `withdraw_withheld_authority = None`.

---

### DG-D07-2 — USD Valuation Dependency

**Description:** Signal A's `min_cumulative_withdraw_usd` gate requires USD price annotation on
`token2022_instructions` rows. The indexer writes `amount_usd` from a price feed at ingestion
time. For low-liquidity or newly listed tokens without a reliable price feed, `amount_usd` will be
NULL, falling back to event-count-only evaluation. This degrades Signal A's USD gate entirely for
micro-cap tokens — the population most likely to host scam operations.

**Impact:** False negatives on micro-cap Token-2022 scams where the indexer has no USD price.
Event-count-only evaluation (≥3 events) fires but without the USD gate provides weaker evidence.

**Mitigation (MVP):** Fallback to event-count-only. Emit `cumulative_withdrawn_usd = "0"` with
`evidence note: "price_data_unavailable"` so the scoring crate can apply lower weight.

**Phase 3 resolution:** Integrate a Pyth oracle price feed directly into the indexer for all
Token-2022 mints. Compute USD value at instruction block_time from the oracle's confirmed price.
This does not require a 3rd-party SaaS (ADR 0003) — Pyth is an on-chain oracle directly readable
via the Pyth program account on Solana.

---

### DG-D07-3 — Permissionless Harvest Instruction Not Directly Dangerous

**Description:** `HarvestWithheldTokensToMint` is permissionless — anyone can call it to move
withheld balances from token accounts to the mint's global `withheld_amount`. D07 records these
events in `token2022_instructions` but does NOT fire Signal A on harvest alone. Signal A requires
a subsequent `WithdrawWithheldTokensFromMint` instruction (which IS authority-gated). A scammer
could use harvest events as a preparation step before extraction, visible in the table but not
individually alarming.

**Impact:** No false positives (correct). Potential false negative: if the indexer only decodes
`HarvestWithheldTokensToMint` and misses the subsequent `WithdrawWithheldTokensFromMint`, Signal A
would not fire for the extraction step.

**Mitigation (MVP):** The decoder MUST decode all three instruction types (`Harvest`, `WithdrawFromMint`,
`WithdrawFromAccounts`). The developer acceptance checklist item #3 covers this explicitly.

**Phase 3 resolution:** Add a `harvest_then_withdraw` sub-signal: if `HarvestWithheldTokensToMint`
occurs within 1 hour before `WithdrawWithheldTokensFromMint`, emit an evidence note flagging the
two-step extraction pattern. This strengthens the evidence bundle for human reviewers.

---

### DG-D07-4 — No Cross-Window Cumulative Accumulation

**Description:** Signal A's `min_cumulative_withdraw_usd` is evaluated within a single 7-day
detection window. An attacker who extracts $499 in week 1 and $499 in week 2 (the slow-drip
evasion E-D07-1) never crosses $1,000 within a single window.

**Impact:** Slow-drip extractors can sustainably drain token value across multiple weeks without
triggering Signal A.

**Mitigation (MVP):** Event-count gate (≥3 events within 7 days) catches daily extraction patterns
even when per-window USD stays below threshold. Signal B fires if authority is rotated.

**Phase 3 resolution:** Add a 30-day cumulative query variant (W3-30d) with a companion threshold
`min_cumulative_withdraw_usd_30d = 3000`. This closes the week-straddling gap.

---

### DG-D07-5 — Instruction-Level CPI Decoding Adds Indexer Complexity

**Description:** CPI (Cross-Program Invocation) `WithdrawWithheld*` instructions require
inspection of `meta.innerInstructions` in the Solana transaction response. The standard `getBlock`
RPC response includes `innerInstructions` only when the transaction is fetched with
`transactionDetails = "full"` encoding. High-TPS Solana blocks with many transactions may make
full-transaction encoding expensive at scale.

**Impact:** Indexer performance degrades if every transaction must be decoded at full detail.
Selective decoding (only transactions involving the Token-2022 program ID in their
`accountKeys`) mitigates this: filter transactions to those where
`TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb` appears in the transaction's account keys before
doing full inner-instruction inspection.

**Mitigation (MVP):** Implement account-key pre-filter in the chain-adapter before fetching full
transaction detail. Document in the `chain-adapter` crate module comment.

**Phase 3 resolution:** Geyser plugin integration (Helius/Triton) provides pre-filtered
Token-2022 program instruction streams without requiring full transaction fetches. This eliminates
the CPI decoding overhead entirely.

---

## 20. Developer Acceptance Checklist

The following constitutes the acceptance criterion for the Sprint 5 P5-5 developer implementing
this design:

- [ ] Migration V00007 `token2022_instructions` table created and applied; index on
      `(chain, mint, block_time)` confirmed active.
- [ ] Instruction decoder implemented in `crates/chain-adapter/src/solana/token2022.rs` or
      equivalent; decodes `WithdrawWithheldTokensFromMint` (discriminator byte 27),
      `WithdrawWithheldTokensFromAccounts` (byte 28), `HarvestWithheldTokensToMint` (byte 29),
      and `SetAuthority { authority_type: WithdrawWithheldTokens }`.
- [ ] Inner instructions (CPI) inspected via `meta.innerInstructions` in the transaction response;
      Token-2022 program ID `TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb` used as the CPI
      program ID filter.
- [ ] Account-key pre-filter implemented in the chain-adapter: transactions without
      `TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb` in their account keys are skipped before
      full inner-instruction decode.
- [ ] `crates/detectors/src/d07_withdraw_withheld.rs` created; `Detector::ID = "withdraw_withheld_drain"`.
- [ ] `fetch_rows()` / `compute()` split implemented per `docs/designs/0003-detector-trait.md`
      §mock.rs pattern.
- [ ] D07 returns `Err(DetectorError::InsufficientBaseline)` when `TokenMeta.transfer_fee = None`.
- [ ] D07 returns `Err(DetectorError::MissingDependencyData)` when `token2022_instructions` table
      returns zero rows for the (chain, mint) combination across the full detection window.
- [ ] Signal A fires when W1 event_count ≥ `min_extraction_events` AND cumulative_usd ≥
      `min_cumulative_withdraw_usd` (or event_count gate alone when USD unavailable); confidence
      formula from §6 applied exactly.
- [ ] Signal A authority_match: `"exact"` when instruction signer matches recorded authority;
      `"unknown"` otherwise; 0.10 confidence penalty for `"unknown"` applied before cap.
- [ ] Signal B fires on `set_authority_withdraw_withheld` rows within `authority_rotation_window_days`;
      confidence formula from §6 applied.
- [ ] Signal B: `fresh_wallet_bonus = 0.20` applied when `new_authority_first_sol_time` is within
      `fresh_wallet_funding_hours` of the rotation block_time; bonus = 0.0 when sidecar absent.
- [ ] Signal B: `rapid_rotation_bonus = 0.15` applied when previous authority tenure <
      `min_authority_tenure_days`.
- [ ] Signal B NOT suppressed when `is_established_protocol(meta) = true`.
- [ ] Signal A composite upgrade: when rotation event is in window AND Signal A fires, composite
      confidence = `min(0.95, signal_a_conf + 0.10)`.
- [ ] `combined_with_d01_s2 = "1"` set when `transfer_fee_bps > sell_tax_threshold_bps` AND
      `cross_detector_composite_enabled = true`.
- [ ] Established-protocol suppression: Signal A suppressed when `is_established_protocol = true`
      AND `extraction_usd / pool_volume_usd <= 0.90`; not suppressed when ratio > 0.90 or
      pool_volume_usd = 0; `established_protocol_suppression_skipped_reason = "1"` set when
      override applies.
- [ ] `BTreeMap` used for all intermediate collections contributing to `Evidence::metrics`
      (determinism contract per CLAUDE.md reproducibility rule).
- [ ] No `Utc::now()` calls in computation path; all timestamps from `ctx.window.end` or
      row-level `block_time` from the database.
- [ ] `config/detectors.toml` expanded with all 9 threshold keys from §17.
- [ ] Unit test: POS-D07-01 (Signal A fires, no Signal B, `combined_with_d01_s2 = "1"`).
- [ ] Unit test: POS-D07-02 (Signal B fires at 0.75, Signal A fires, composite at 0.87).
- [ ] Unit test: NEG-D07-02 (Token-2022 without TransferFeeConfig → `Err(InsufficientBaseline)`).
- [ ] Unit test: NEG-D07-03 (legacy SPL token → `Err(InsufficientBaseline)`).
- [ ] `REFERENCES.md` D07 entry added (see §21).

---

## 21. References

All referenced sources are documented in `REFERENCES.md`. D07-specific entries to add:

| Mechanism | Signal / Formula | Source | Used In | Verified Against |
|-----------|--------|--------|---------|-----------------|
| Token-2022 `WithdrawWithheldTokensFromAccounts` as non-LP drain path | `withdraw_withheld_authority` extracts accumulated transfer fees without LP Burn event; bypasses D02 Signal A and D06 Signals B/C entirely | Solana Token-2022 SPL program docs, https://spl.solana.com/token-2022/extensions#transfer-fees; Sun et al. 2024 §4 "Hidden Fee" taxonomy (category 7 of 34) | D07 Signal A detection; V00007 migration; E-D02-11 Phase 3 gap closure | Referenced 2026-04-21 |
| D07 Confidence Formula — Signal A base (0.60) | Event-based confirmed extraction warrants Medium-to-High threshold immediately; 0.60 base is above the structural-risk base (0.40 for Signal B) to reflect higher evidence quality | Design derivation; consistent with D02 Signal A base calibration (0.60 for event-based drain with single actor) | D07 Signal A `conf_raw` formula §6 | Design derivation 2026-04-21 |
| Token-2022 instruction discriminators (byte offsets 27–29) | `WithdrawWithheldTokensFromMint` = byte 27; `WithdrawWithheldTokensFromAccounts` = byte 28; `HarvestWithheldTokensToMint` = byte 29; per spl-token-2022 program source | SPL Token-2022 source, https://github.com/solana-labs/solana-program-library/blob/master/token/program-2022/src/instruction.rs | V00007 migration decoder; §11.2 Which Instructions Must Be Decoded | Cross-referenced against SPL source 2026-04-21 |

The existing REFERENCES.md entry for "Token-2022 withdraw_withheld drain path" (D02 evasion
E-D02-11 row) should have its `Used In` column updated to include `D07 withdraw_withheld_drain
Signal A, B; V00007 migration` once D07 ships.

---

## 22. Non-Goals

This design explicitly does NOT cover:

- EVM chain fee-sink honeypots (Torres et al. 2019 §5.3 "hidden fee" category) — Phase 4; EVM
  adapter required.
- Token-2022 `ConfidentialTransfer` extension (D01 evasion E17) — encrypted amounts; separate
  Phase 3 indexer concern.
- Token-2022 `PermanentDelegate` extraction — covered by D01 Signal S3; D07 does not duplicate.
- Solana native SOL withheld fee accounts (protocol-level, not Token-2022) — out of scope.
- Multi-chain `withdraw_withheld` equivalents (EVM ERC-20 fee tokens with similar patterns) —
  Phase 4 adaptation.
- ML-based classification of extraction patterns — Phase 5; insufficient labelled corpus.
- Governance-controlled treasury fee extraction on established protocols — suppressed via
  `is_established_protocol` check per §9.

---

## 23. P6-1 Calibration Amendment (2026-04-21)

**Review ref:** `docs/reviews/0004-d07-withdraw-withheld-evasions.md`
**Sprint:** P6-1 post-review fixes
**Status:** Applied

This section documents all threshold and logic changes applied in P6-1 after the security review
verdict `BLOCK` (blocking conditions B1 and B2). All changes are in effect in the production
config as of 2026-04-21.

---

### 23.1 T1 — New Threshold `min_single_event_withdraw_usd` + Two-Tier Signal A

**Closes:** E-D07-9 (Harvest-Without-Withdraw single-event bypass)
**Review ref:** §4 T1

**Problem:** The `min_extraction_events = 3` gate prevented Signal A from firing when an attacker
used `HarvestWithheldTokensToMint` (permissionless) to consolidate the mint's withheld balance and
then called `WithdrawWithheldTokensFromMint` once. A single extraction event of any USD value never
triggered Signal A regardless of size.

**Fix:** Signal A now uses a three-tier detection gate:

| Tier | Condition | Confidence | Evidence key |
|------|-----------|-----------|--------------|
| `recurring` | `event_count >= min_extraction_events` (3) | Primary spec §6 formula (base 0.60) | `detection_tier = "recurring"` |
| `two_event` | `event_count == 2 AND cumulative_usd >= min_cumulative_withdraw_usd` ($1,000) | Fixed 0.60 | `detection_tier = "two_event"` |
| `single_event` | `event_count == 1 AND cumulative_usd >= min_single_event_withdraw_usd` ($5,000) | Fixed 0.65 | `detection_tier = "single_event"` |

**New threshold:** `min_single_event_withdraw_usd = 5000.0` (5× `min_cumulative_withdraw_usd`).
Set at $5,000 to ensure a single-event extraction represents meaningful value and is not triggered
by routine dust sweeps or one-off legitimate fee collection events below that floor.

**Authority penalty** (−0.10 for `authority_match = "unknown"`) still applies in all tiers.

**Composite upgrade** (Signal A + Signal B, +0.10) still applies in all tiers when a rotation is
in the detection window.

**Config key added:** `[withdraw_withheld.min_single_event_withdraw_usd]` in `config/detectors.toml`
and `config/detectors.toml.example`.

**Tests added (4):**
- `signal_a_single_event_above_floor_fires_at_0_65` — event_count=1, usd=$5,500 → 0.65
- `signal_a_single_event_below_floor_no_fire` — event_count=1, usd=$1,500 → None
- `signal_a_two_events_fires_at_0_60` — event_count=2, usd=$2,500 → 0.60
- `signal_a_three_events_uses_recurring_tier` — event_count=3, usd=$3,500 → primary formula

---

### 23.2 T2 — `established_protocol_fee_extraction_allowlist_pct` Lowered 0.90 → 0.50

**Closes:** E-D07-12 (pre-extraction fee-config-authority suppression manipulation)
**Review ref:** §4 T2

**Problem:** The 0.90 threshold was indefensible — it allowed suppression when an attacker
extracts up to 90% of pool volume as fees, requiring a >90% effective fee rate, which is
economically impossible at normal fee rates. An attacker who compromises an established
protocol's `withdraw_withheld_authority` and extracts at ratio 0.85–0.89 would permanently
evade Signal A after DG-D07-2 resolves the `pool_volume_usd` stub.

**Fix:** Lowered to 0.50. Any extraction exceeding 50% of pool volume fires Signal A on
established protocols regardless of protocol status.

**Note:** This threshold is non-operational in MVP while `pool_volume_usd` is hardcoded to 0.0
(see ACCEPTED-RISK-D07-01 in §23.4 below). It MUST be set to a corpus-calibrated value before
DG-D07-2 ships.

---

### 23.3 T3 — `fresh_wallet_funding_hours` Lowered 48 → 24

**Review ref:** §4 T3

**Problem:** 48 hours is trivially evaded on Solana — an attacker can fund the new authority
wallet 49 hours before `SetAuthority`, costing ~0.000005 SOL. The 48h window was calibrated for
Ethereum where wallet creation has higher operational cost.

**Fix:** Lowered to 24 hours. 24h increases operational friction without blocking legitimate
same-day key ceremonies (hardware wallet rotations funded day-of still trigger the fresh-wallet
check, which is the correct behavior — it warrants human review).

---

### 23.4 ACCEPTED-RISK-D07-01 — `pool_volume_usd` Stub

**Blocking condition:** B1 (review §2)

The `pool_volume_usd = 0.0_f64` stub at `crates/detectors/src/d07_withdraw_withheld.rs` (in
`evaluate_signal_a`) is documented as `ACCEPTED-RISK-D07-01`. Current behavior:

- Signal A suppression for established protocols is **NEVER APPLIED** in MVP because
  `pool_volume_usd` is hardcoded to 0.0. The ratio check is never reached.
- This is the **security-safe failure mode** — we over-alert rather than silently suppress.
- `established_protocol_fee_extraction_allowlist_pct` is a non-operational config value until
  DG-D07-2 resolves the stub.

**Before DG-D07-2 ships:**
1. Lower `established_protocol_fee_extraction_allowlist_pct` to a corpus-calibrated value
   (0.50 applied in T2 is the floor; corpus may warrant further lowering).
2. Add regression tests for the suppression path (currently dead code).

---

### 23.5 ACCEPTED-RISK-D07-02 — `wallet_funding_events` Depopulation

**Blocking condition:** B2 (review §2)

The `wallet_funding_events` table (V00007 migration) exists but has no indexer write path in
Phase 2. Consequence:

- `fetch_wallet_funding_time` returns `None` for every query.
- `authority_is_fresh_wallet` evidence key is always `"0"`.
- `fresh_wallet_bonus` is permanently 0.0 in Signal B.
- Signal B fires at maximum 0.55 (rapid rotation only) or 0.40 (base) rather than the
  intended 0.75 maximum.

**Operational surface:** When Signal B fires and the sidecar is empty, a `tracing::warn!` is
emitted with the message `"D07 Signal B: wallet_funding_events sidecar is empty; fresh_wallet_bonus
disabled (ACCEPTED-RISK-D07-02). Phase 3 indexer write path required."` The warn fires at most
once per `(chain, token, evaluation)` — `evaluate_signal_b` returns a single `Option<SignalBResult>`
(the most recent rotation in window), so one warn per `evaluate()` call is guaranteed.

**Resolution:** Indexer write path lands Phase 3 (blockchain-engineer P6-4 or Sprint 7).
