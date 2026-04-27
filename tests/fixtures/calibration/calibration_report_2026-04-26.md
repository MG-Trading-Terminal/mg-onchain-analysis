# Calibration Report — 2026-04-26

**Incidents loaded:** 7 from `tests/fixtures/calibration/evm_real_incidents.json`

**Method:** Synthetic signal mapping (live-replay deferred to Sprint 25+).
Confidence values are formula-level estimates, NOT live detector runs.
Incidents with `research_gaps` or unsupported chains are flagged as structural gaps.

---

## Per-Detector Results

### [MATCH] rekt-2023-03-001 — Euler Finance flash-loan exploit

- **Chain:** ethereum
- **Detector:** rug_pull_lp_drain
- **Expected:** severity=Critical confidence_min=0.70
- **Actual (synthetic):** severity=Critical confidence=0.92
- **Confidence gap:** 0.22 (positive = overshoots, negative = undershoots)
- **Note:** Flash-loan / governance LP drain: D02 Signal A covers LP removal but not governance vector

- **Research gaps:** EUL token contract address unconfirmed; transaction hash is DAI-drain example only; full drain TX list not extracted.

---

### [STRUCTURAL_GAP] rekt-2022-03-001 — Ronin Network bridge drain

- **Chain:** ethereum
- **Detector:** rug_pull_lp_drain
- **Expected:** severity=Critical confidence_min=0.80
- **Actual (synthetic):** severity=Undetectable confidence=0.00
- **Confidence gap:** -0.80 (positive = overshoots, negative = undershoots)
- **Structural gap:** YES — live replay not possible with current data
- **Note:** Structural gap: unsupported chain / missing TX / bridge exploit path not LP-level

- **Research gaps:** Bridge contract address not captured. Ronin is a sidechain — chain adapter not implemented. D02 signal applicability is indirect (LP drained on Ethereum side).

---

### [STRUCTURAL_GAP] rekt-2022-02-001 — Wormhole bridge exploit

- **Chain:** solana
- **Detector:** honeypot_sim
- **Expected:** severity=Critical confidence_min=0.75
- **Actual (synthetic):** severity=Undetectable confidence=0.00
- **Confidence gap:** -0.75 (positive = overshoots, negative = undershoots)
- **Structural gap:** YES — live replay not possible with current data
- **Note:** D01 honeypot simulation cannot detect bridge signature-bypass exploit. Structural mismatch: incident is a fraudulent mint, not a sell-revert pattern. D01 architecture gap — would require a separate 'mint_cap_exceeded' signal.

- **Research gaps:** Solana-side attacker address not captured. Bridge exploit — not a typical token honeypot. D01 applicability is indirect.

---

### [MATCH] rekt-2023-03-002 — SafeMoon LP drain via public burn()

- **Chain:** bsc
- **Detector:** rug_pull_lp_drain
- **Expected:** severity=Critical confidence_min=0.85
- **Actual (synthetic):** severity=Critical confidence=0.92
- **Confidence gap:** 0.07 (positive = overshoots, negative = undershoots)
- **Note:** Classic LP drain: D02 Signal A fires at 100% drain threshold

- **Research gaps:** LP pair contract address (PancakeSwap WBNB/SAFEMOON) not captured. Full LP drain TX sequence not extracted.

---

### [MATCH] rekt-2022-04-001 — Beanstalk governance flash-loan rug

- **Chain:** ethereum
- **Detector:** rug_pull_lp_drain
- **Expected:** severity=Critical confidence_min=0.75
- **Actual (synthetic):** severity=Critical confidence=0.92
- **Confidence gap:** 0.17 (positive = overshoots, negative = undershoots)
- **Note:** Flash-loan / governance LP drain: D02 Signal A covers LP removal but not governance vector

- **Research gaps:** BEAN token contract address unconfirmed (placeholder above). Attacker contract address confirmed. Governance vector — D02 covers LP drain but not governance path.

---

### [MATCH] rekt-2020-10-001 — Harvest Finance flash-loan price manipulation

- **Chain:** ethereum
- **Detector:** sandwich_mev_v1
- **Expected:** severity=High confidence_min=0.70
- **Actual (synthetic):** severity=High confidence=0.72
- **Confidence gap:** 0.02 (positive = overshoots, negative = undershoots)
- **Note:** Price manipulation sandwich analog: D13 would detect 32-round swap anomaly. Confidence ≈ 0.72 (High) — flash-loan vault exploit, not pure mempool sandwich. D13 sandwich confidence cap limits detection of vault-level exploits. Recommendation: add 'repeated_price_impact' sub-signal to D13.

- **Research gaps:** Multiple attacker TXs not enumerated. Curve pool addresses not captured. D13 target is MEV sandwich; this is a protocol-level vault exploit.

---

### [STRUCTURAL_GAP] rekt-2023-04-001 — Merlin DEX MAGE rug pull — LP drained by devs

- **Chain:** zksync
- **Detector:** rug_pull_lp_drain
- **Expected:** severity=Critical confidence_min=0.90
- **Actual (synthetic):** severity=Undetectable confidence=0.00
- **Confidence gap:** -0.90 (positive = overshoots, negative = undershoots)
- **Structural gap:** YES — live replay not possible with current data
- **Note:** Structural gap: unsupported chain / missing TX / bridge exploit path not LP-level

- **Research gaps:** MAGE token contract address not captured. Transaction hash not published in rekt.news article. zkSync is not yet a supported chain.

---

## Summary by Detector

| Detector | Samples | Severity Match | Avg Confidence Gap | Structural Gaps | Recommendation |
|----------|---------|----------------|--------------------|-----------------|----------------|
| honeypot_sim | 1 | 0/1 (0%) | -0.75 | 1 | Structural gaps only — live replay needed |
| rug_pull_lp_drain | 5 | 3/5 (60%) | -0.25 | 2 | Add flash_loan_governance sub-signal to cover governance vectors |
| sandwich_mev_v1 | 1 | 1/1 (100%) | +0.02 | 0 | Thresholds correct (formula match) |

## Global Metrics

- **Total incidents:** 7
- **Severity match (synthetic):** 4/7
- **Structural gaps (live replay impossible):** 3/7
- **Evaluable incidents (no structural gap):** 4/7

## Threshold Adjustment Recommendations

**NOTE:** These are recommendations only. Do NOT modify `config/detectors.toml` thresholds until live-replay data is available (Sprint 25+).

1. **D02 (rug_pull_lp_drain):** Formula produces correct severity for 100% LP drain incidents. Flash-loan governance vectors (Euler, Beanstalk) require a separate `flash_loan_governance` signal — D02 Signal A alone undershoots for these patterns because the drain originates from a governance call, not a direct LP remove.

2. **D01 (honeypot_sim):** Bridge signature-bypass exploits (Wormhole) are structurally undetectable by D01's sell-simulation path. A dedicated `mint_anomaly_rapid` sub-signal or integration with D06 (MintBurnAnomaly) would be needed.

3. **D13 (sandwich_mev_v1):** Vault-level price-manipulation exploits (Harvest Finance) partially map to D13's swap anomaly signals. Adding a `repeated_price_impact` sub-signal with a count threshold (≥5 identical pools in same block) would improve detection. Confidence cap (currently ~0.72 for this class) may need an upward adjustment to Critical range once the sub-signal is added.

4. **D12 (permit2_drainer_v1):** Euler and Ronin exploits bypass the Permit2 path entirely. D12 is correctly scoped to Permit2-based drains. No threshold change needed. These incidents require bridge-level monitoring outside D12's scope.

---

*Generated by `onchain-calibrate` on 2026-04-26. Synthetic methodology — live replay pending Sprint 25 EVM indexer integration.*
