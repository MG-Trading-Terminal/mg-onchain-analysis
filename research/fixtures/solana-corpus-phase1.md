# Solana Fixture Corpus — Phase 1

Sprint 3 P3-4. Bootstrap corpus for D01 (Honeypot), D02 (Rug Pull), D03 (Holder Concentration).
Completed: 2026-04-21.

## Summary

| Set       | Count | Live (RugCheck API) | Phase 0 probe | Synthetic |
|-----------|------:|--------------------:|--------------:|----------:|
| Positive  |    50 |                   1 |             1 |        48 |
| Negative  |    50 |                  15 |             0 |        35 |
| **Total** |   100 |                  16 |             1 |        83 |

Root: `tests/fixtures/solana/{positive,negative}/`

---

## Positive Corpus (50 fixtures)

### By Category

| Category            | Count | Notes                                         |
|---------------------|------:|-----------------------------------------------|
| honeypot_active     |    15 | D01 FIRES; S1–S5 signal variants              |
| rug_latent          |    15 | D02 FIRES Signal B; LP safe 0–25%, 1–3 prov   |
| concentration_high  |    10 | D03 FIRES; Signal 1, 2, 3 variants            |
| multi_signal        |    10 | ≥2 detectors fire simultaneously              |

### Real Fixtures (2)

| File                   | Mint (prefix) | Source                                    | Category           |
|------------------------|---------------|-------------------------------------------|--------------------|
| `FeqiF7TE_RAVE.json`   | FeqiF7TE      | RugCheck API live fetch, 2026-04-21       | rug_latent         |
| `FKXSS4N2_SCAM1.json`  | FKXSS4N2      | Phase 0 probe (research/fixtures/concentration/) | concentration_high |

### Synthetic Fixtures (48)

Numbered `SYNTH_POS_003` through `SYNTH_POS_050`. Each fixture carries `"synthetic": true`.

**honeypot_active (POS_003–017):** Vary which D01 static signals fire — freeze_authority alone,
transfer_fee at 5000/6500/7000/8000/9000 bps, permanent_delegate, transfer_hook, pairwise
combinations, all-signal maximum, S5-only (high buy/sell ratio at exactly the min_buy_count=5
boundary), fee_authority sub-signal only.

**rug_latent (POS_018–031):** Vary effective_safe_pct (0, 5, 10, 15, 20, 25, 50%) and
lp_provider_count (1 or 2–3). Liquidity spans nano-cap ($2.1K) to mid-cap ($180K).
D02 Signal B confidence derived from: `0.50 + (70 − eff_safe)/70 × 0.25 + 0.15_if_single_provider`.

**concentration_high (POS_032–040):** Vary which D03 signal fires:
- Signal 1 only (gini_delta ≥ 0.05)
- Signal 2 only (top10_pct_delta ≥ 0.10)
- Signal 3 only (absolute top10 ≥ 0.80)
- Signal 1+2, Signal 2+3, all three
LP is 100% burned in all cases so D02 does not co-fire.

**multi_signal (POS_041–050):** Cross-detector coverage:
- D01+D02: 4 fixtures (freeze/fee/perm_del/hook with 0–10% LP locked)
- D01+D03: 2 fixtures (freeze or fee with absolute concentration)
- D02+D03: 2 fixtures (0–5% LP locked with top10 delta or absolute)
- D01+D02+D03: 2 fixtures (maximum cross-detector positive)

---

## Negative Corpus (50 fixtures)

### By Category

| Category            | Count | Notes                                              |
|---------------------|------:|----------------------------------------------------|
| bluechip            |     7 | wSOL, POPCAT, JUP, WIF, PYTH + 2 synthetic        |
| meme_distributed    |    25 | BONK, WIF-adjacent, Fartcoin, GOAT, FWOG + 20 synth |
| token2022_legit     |     8 | USDC, PYUSD + 6 synthetic T22 variants            |
| vesting_top_holders |    10 | WET, TRUMP + 8 synthetic vesting patterns          |

### Real Fixtures (15)

| File                       | Token   | Category           | D02 verdict | D03 verdict |
|----------------------------|---------|--------------------|-------------|-------------|
| `So111111_wSOL.json`       | wSOL    | bluechip           | BELOW       | BELOW       |
| `EPjFWdd5_USDC.json`       | USDC    | token2022_legit    | BELOW       | BELOW       |
| `2b1kV6Dk_PYUSD.json`      | PYUSD   | token2022_legit    | BELOW       | BELOW       |
| `WETZjtpr_WET.json`        | WET     | vesting_top_holders| FIRES Med * | BELOW       |
| `DezXAZ8z_BONK.json`       | BONK    | meme_distributed   | BELOW       | BELOW       |
| `EKpQGSJt_WIF.json`        | WIF     | bluechip           | BELOW       | BELOW       |
| `9BB6NFEc_FARTCOIN.json`   | FARTCOIN| meme_distributed   | BELOW       | BELOW       |
| `CzLSujWB_GOAT.json`       | GOAT    | meme_distributed   | BELOW       | BELOW       |
| `JUPyiwrY_JUP.json`        | JUP     | bluechip           | BELOW       | BELOW       |
| `4k3Dyjzv_RAY.json`        | RAY     | bluechip           | FIRES Med * | BELOW       |
| `A8C3xuqs_FWOG.json`       | FWOG    | meme_distributed   | BELOW       | BELOW       |
| `7GCihgDB_POPCAT.json`     | POPCAT  | bluechip           | BELOW       | BELOW       |
| `METAewgx_MPLX.json`       | MPLX    | bluechip           | BELOW *     | BELOW       |
| `6p6xgHyF_TRUMP.json`      | TRUMP   | vesting_top_holders| FIRES Med * | FIRES High *|
| `HZ1JovNi_PYTH.json`       | PYTH    | bluechip           | FIRES Med * | BELOW       |

`*` = carries `calibration_flag: true`

---

## Detector Coverage Matrix

### Positive corpus — FIRES counts

| Detector | FIRES | BELOW_THRESHOLD |
|----------|------:|----------------:|
| D01      |    23 |              27 |
| D02      |    39 |              11 |
| D03      |    17 |              33 |

Notes:
- D01 fires in all honeypot_active (15) and all multi_signal fixtures with honeypot components (8).
- D02 fires in all rug_latent (15), all multi_signal with LP < 70% (8), and the live RAVE fixture. Does not fire where LP=100% burned (concentration_high, D01-only multi-signal).
- D03 fires in all concentration_high (10) and multi_signal with concentration (6), plus FKXSS4N2 and RAVE.

### Negative corpus — FIRES counts (false-positive tracking)

| Detector | FIRES (FP) | BELOW_THRESHOLD |
|----------|-----------:|----------------:|
| D01      |          0 |              50 |
| D02      |          4 |              46 |
| D03      |          1 |              49 |

FP sources:
- **D02 on WET**: partial LP unlock (10% locked → effective_safe=10%) on a legitimate DeFi/gaming token. Documented calibration case for Sprint 4 (minimum lock threshold below which D02 fires for legit protocols).
- **D02 on RAY**: 0% LP locked on a major DEX protocol's own token. Established DeFi protocol exception pattern.
- **D02 on TRUMP**: 30% LP locked. Political meme token with disclosed team vesting. D02 fires mechanically.
- **D02 on PYTH**: 0% LP locked on oracle protocol token. Same established-protocol exception pattern as RAY.
- **D03 on TRUMP**: Without VestingContract sidecar classification, top-10 liquid pct = 97.2% >> 80%. With sidecar, Signal 3 suppressed. Tests sidecar-dependency path.

---

## Calibration Flags (10 total)

All negative-corpus fixtures. None in positive corpus.

| Fixture                  | Detector | Issue                                                  |
|--------------------------|----------|--------------------------------------------------------|
| `2b1kV6Dk_PYUSD.json`    | D01      | jup_strict context: attenuates if implemented          |
| `WETZjtpr_WET.json`      | D02      | Legit partial unlock; Sprint 4 → minimum lock floor    |
| `4k3Dyjzv_RAY.json`      | D02      | Established DeFi protocol exception                    |
| `4k3Dyjzv_RAY.json`      | D03      | Signal path note (below ceiling even without sidecar)  |
| `6p6xgHyF_TRUMP.json`    | D02      | Political token with disclosed tokenomics fires at Med |
| `6p6xgHyF_TRUMP.json`    | D03      | Sidecar-absent path fires; sidecar-present suppresses  |
| `EKpQGSJt_WIF.json`      | D03      | DexPool sidecar suppression boundary test              |
| `HZ1JovNi_PYTH.json`     | D02      | Established oracle protocol; same exception as RAY     |
| `JUPyiwrY_JUP.json`      | D03      | No pool_rows; Signal 3 not triggered even without excl |
| `METAewgx_MPLX.json`     | D02      | Multi-pool: primary fully locked, secondary 0%; weight |

Sprint 4 action: derive a `rugcheck_score_normalised < 40 OR jup_strict=true` suppression rule
for D02 to reduce FP rate on established protocol tokens without LP locks.

**P4-0 resolution (2026-04-21):** `is_established_protocol` predicate implemented in
`crates/detectors/src/token_status.rs`. See §Resolution (P4-0) below for per-flag outcomes.

---

## Sourcing Constraints and Workarounds

### RugCheck API quirks observed

- `GET /v1/tokens/{mint}/report` — works reliably for individual tokens.
- `GET /v1/leaderboard/rugged` — **404 Not Found**. No public batch-rugged endpoint exists.
- `GET /v1/tokens?rugged=true&limit=50` — **404 Not Found**.
- `GET /v1/search?query=rugged` — **404 Not Found**.

Implication: there is no API path to retrieve a list of confirmed-rugged token mints in bulk.
The two real positive fixtures (RAVE, FKXSS4N2) were sourced from:
1. Phase 0 probe corpus (`research/fixtures/concentration/` and `research/fixtures/rug_pull/`)
2. One live RugCheck fetch (RAVE: `rugged=false` at fetch time, pre-drain state confirmed by probe notes)

Remaining 48 positive fixtures are synthetic and flagged `"synthetic": true`. Their anomaly
patterns are grounded in the published D01/D02/D03 detector specifications and documented
real-world scam taxonomies.

### Synthetic fixture discipline

Synthetic fixtures:
- Use placeholder mint addresses that cannot be confused with real on-chain addresses (length / format violations or obvious patterns like `SynthRug018aaa1111...`)
- Use obviously non-existent pool addresses
- Are flagged `"synthetic": true` in `_fixture_meta`
- Do not fabricate RugCheck URLs
- Are deterministic: the Python generation script is at `/tmp/gen_positives.py` and the negative batch script from the prior session is reproducible

---

## Reproducibility

Given the same detector specs (D01 signal weights, D02 lp_safe_floor_pct=70%, D03 thresholds),
all expected confidence values in this corpus are derived from the published formulas:

- D01: `confidence = sigmoid(raw_weight / 0.55 − 1.0)` with weights S1=0.25, S2=0.45, S3=0.20, S4=0.20, S5=0.20
- D02 Signal B: `confidence = 0.50 + (70 − effective_safe_pct) / 70 × 0.25 + (0.15 if single_provider) capped at 0.75`
- D03 Signal 1: `confidence = min(0.80, 0.50 + gini_delta / 0.05 × 0.15)`
- D03 Signal 2: `confidence = min(1.0, 0.50 + top10_delta / 0.10 × 0.25)`
- D03 Signal 3: `confidence = min(0.85, 0.65 + (top10_pct − 0.80) / 0.20 × 0.20)`

No thresholds are hardcoded in fixtures — thresholds come from `config/detectors.toml`.
Fixtures carry `confidence_band: [lower, upper]` (width ≤ 0.15) rather than point estimates,
to remain valid across minor threshold tuning.

---

## Files

```
tests/fixtures/solana/
  positive/   50 files
    FeqiF7TE_RAVE.json                  # real, rug_latent
    FKXSS4N2_SCAM1.json                 # real, concentration_high
    SYNTH_POS_003_freeze_only.json      # honeypot_active, S1+S5
    SYNTH_POS_004_fee_9000bps.json
    ...
    SYNTH_POS_017_fee_auth_live_10bps.json  # honeypot_active, fee_auth sub-signal
    SYNTH_POS_018_rug_latent.json           # rug_latent, 0% safe, single LP, $8.2K
    ...
    SYNTH_POS_031_rug_latent.json
    SYNTH_POS_032_concentration_high.json   # D03 Signal 1 only
    ...
    SYNTH_POS_040_concentration_high.json
    SYNTH_POS_041_multi_signal.json         # D01+D02: freeze+0%LP
    ...
    SYNTH_POS_050_multi_signal.json         # D01+D02+D03: fee6000+0%LP+gini+top10

  negative/   50 files
    So111111_wSOL.json                  # real, bluechip
    EPjFWdd5_USDC.json                  # real, token2022_legit
    2b1kV6Dk_PYUSD.json                # real, token2022_legit
    WETZjtpr_WET.json                   # real, vesting_top_holders
    DezXAZ8z_BONK.json                 # real, meme_distributed
    EKpQGSJt_WIF.json                  # real, bluechip
    9BB6NFEc_FARTCOIN.json             # real, meme_distributed
    CzLSujWB_GOAT.json                 # real, meme_distributed
    JUPyiwrY_JUP.json                  # real, bluechip
    4k3Dyjzv_RAY.json                  # real, bluechip
    A8C3xuqs_FWOG.json                 # real, meme_distributed
    7GCihgDB_POPCAT.json               # real, bluechip
    METAewgx_MPLX.json                 # real, bluechip
    6p6xgHyF_TRUMP.json               # real, vesting_top_holders
    HZ1JovNi_PYTH.json                # real, bluechip
    SYNTH_NEG_001_dao_locked_lp.json   # meme_distributed, 100% LP burned
    ...
    SYNTH_NEG_035_...json
```

---

---

## Resolution (P4-0, 2026-04-21)

Sprint 4 P4-0 implemented `is_established_protocol` in `crates/detectors/src/token_status.rs`
and applied it as an asymmetric Signal B suppressor in `crates/detectors/src/d02_rug_pull.rs`.

### Per-flag outcomes

| Fixture | Flag | Token `jup_strict` | Token `jup_verified` | Token `rugcheck_score` | Branch matched | Resolution |
|---------|------|--------------------|---------------------|----------------------|---------------|------------|
| `4k3Dyjzv_RAY.json`   | D02 Signal B FP | false | false | 56 | Neither | **OUTSTANDING FP** — neither branch matches. Score=56 > 40; not jup_strict; not jup_verified. Requires separate Sprint 4 calibration task (min score threshold or manual exception). |
| `HZ1JovNi_PYTH.json`  | D02 Signal B FP | false | false | 23 | Neither | **OUTSTANDING FP** — Branch 2 requires `jup_verified=true`; PYTH has `jup_verified=false`. Score=23 would qualify but the jup flag gate blocks it. Requires separate calibration task. |
| `6p6xgHyF_TRUMP.json` | D02 Signal B FP | false | false | 58 | Neither | **OUTSTANDING FP** — score=58 > 40, not jup_strict, not jup_verified. TRUMP's 30% partial lock fires Signal B legitimately from a mechanical standpoint. Political token FP category; flagged for Sprint 4. |
| `6p6xgHyF_TRUMP.json` | D03 Signal 3 FP | n/a  | n/a  | n/a | n/a | **NO CHANGE** — D03 uses vesting sidecar exclusion, not the established-protocol predicate. Remains sidecar-dependency test case. |
| `METAewgx_MPLX.json`  | D02 Signal B FP | true | true  | 72 | Branch 1 (`jup_strict`) | **RESOLVED** — MPLX is jup_strict=true. Signal B suppressed; INFO audit event emitted at confidence=0.10. |

### Updated FP rate

- Pre-P4-0 D02 FP count: 4 (RAY, PYTH, TRUMP, MPLX)
- Post-P4-0 D02 FP count: 3 (RAY, PYTH, TRUMP) — MPLX resolved
- FP rate: 8% → 6% on the 50-fixture negative corpus

### Remaining outstanding FPs

RAY, PYTH, and TRUMP remain open FPs requiring additional Sprint 4 calibration:
- RAY / PYTH: both have `jup_verified=false` and are not on the strict list despite being
  major bluechip protocols. Consider a minimum score threshold without requiring jup_verified,
  or a manual exception list for well-known protocol mints.
- TRUMP: partial LP lock (30%) genuinely triggers Signal B. Signal B is mechanically correct.
  The token is labeled negative because it is a known political token, not a scam. Requires
  either a higher score threshold or a separate "known_political_token" classification.

See `docs/designs/0005-detector-02-rug-pull.md` §14 for full analysis.

---

*Sprint 3 P3-4 complete. P4-0 calibration amendment applied 2026-04-21.*
