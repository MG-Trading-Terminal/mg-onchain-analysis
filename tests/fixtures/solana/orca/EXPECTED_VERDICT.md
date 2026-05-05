# ORCA (Solana) — regression artefact from real CLI run

**Token:** ORCA (Orca DEX governance token)
**Mint:** `orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE`
**Chain:** Solana mainnet
**Captured:** 2026-04-28 via public `api.mainnet-beta.solana.com`
**Tool:** `target/release/onchain-check-token --token orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE`
**Raw transcript (live RPC run):** [`cli_output_2026-04-28.txt`](cli_output_2026-04-28.txt) (verbatim stdout)
**Holders snapshot (replay input):** [`largest_accounts_full.json`](largest_accounts_full.json) — 89,756 owners, getTokenLargestAccounts shape

This document is a **regression artefact**, not a prediction. Every value
below was produced by the CLI calling `mg_onchain_chain_adapter::solana::*`
into `mg_onchain_detectors::signals::*` against real on-chain state. When
the captured numbers stop matching what a fresh run produces, that's the
signal to investigate (either the chain state changed materially or our
code regressed).

The companion `tests/fixtures/bsc/zbt/` directory holds the labelled-positive
counterpart (CRITICAL verdict on a real rug). ORCA is the labelled-negative
half of the calibration pair.

## What the CLI actually fetched

| Property | Captured value |
|---|---|
| `getProgramAccounts` (token-account count) | **292,754** |
| Non-zero holders | **89,826** |
| Zero-balance accounts (long tail) | **202,928** |
| Summed active supply | `74,999,558,708,883` raw (= 74,999,558.708883 ORCA) |
| Decimals (from mint) | 6 |
| `is_initialized` | true |
| Mint authority | **`GwH3Hiv5mACLX3ufTw1pFsrhSPon5tdw252DBs4Rx4PV`** |
| Freeze authority | None |
| `getSignaturesForAddress` | **rate-limited (HTTP 429)** after 2 pages — D04 / D10 / D11 returned UNKNOWN |

## Detector verdicts as actually produced

| Detector | Captured verdict | Confidence | Source path |
|---|---|---|---|
| **D01 honeypot_static** | `Low` | 0.269 | `signals::sigmoid(0 / 0.55 − 1.0)` floor (no Token-2022 ext, freeze=None) |
| **D02/D06 mint-state** | `Medium` | 0.40 | mint authority active, freeze authority renounced |
| **D03 holder_concentration** | **`MEDIUM`** | top-10 = **63.34%** / Gini = **0.9981** | full distribution over 89,826 holders |
| **D04 pump-dump proxy** | `UNKNOWN` | n/a | RPC 429 on page 1 |
| **D10 launch_audit** | `UNKNOWN` | n/a | RPC 429 after 2 pages — observed window <7d, cannot distinguish "young" from "mature with throttled scan" |
| **D11 sync-activity proxy** | `UNKNOWN` | n/a | shares signature path with D04 |

## Real on-chain findings worth flagging

### 1. Mint authority is also the top-1 holder

The address that can mint more ORCA — `GwH3Hiv5mACLX3ufTw1pFsrhSPon5tdw252DBs4Rx4PV`
— is byte-identical to the largest holder (14.2 T raw = **18.93%** of active
supply). This is the Orca DAO governance PDA serving as both treasury custodian
and emissions controller. Two consequences:

- **D02/D06 will fire `Medium` on every DAO-governed token** with this pattern,
  even when fully legitimate. Consumers need an entity-label suppression
  (`{address: GwH3Hiv5mACLX3ufTw1pFsrhSPon5tdw252DBs4Rx4PV → label: "orca_dao_pda"}`)
  to filter the false positive.
- **D03 top-10 inflation comes partly from this same address.** The DAO PDA's
  18.93% share is doing a lot of the work pushing top-10 from "diffuse" to
  the 63% MEDIUM bucket.

### 2. D03 calibration finding — healthy DAO tokens fire MEDIUM, not NONE

The earlier prediction-version of this file expected D03 = `NONE / LOW` with
Gini ~ 0.6–0.7. **The real numbers are Gini = 0.998 and top-10 = 63%**, both
materially higher.

What drives the gap:
- **Long tail of dust accounts.** 202,928 zero-balance accounts and a tail of
  micro-balance holders inflate Gini toward 1.0 — Gini is dominated by the
  inequality between large vaults and millions of dust accounts. For governance
  tokens with broad airdrop distribution + permanent token-account creation
  (Solana doesn't auto-close empty token accounts), Gini ≈ 0.99 is the *normal*
  baseline, not a rug indicator.
- **Top-10 includes treasury + DEX program vaults + CEX hot wallets.** All are
  legitimate "concentrated holders" by chain accounting but not by economic
  ownership. A 63% top-10 share on a healthy DEX governance token is expected.

**Therefore D03 alone is not actionable.** It must be combined with at least
one of: deployer-history concentration (D09), wash-trading evidence (D05), or
mint authority pointing to an unverified EOA (D06 with no DAO label). The
composite is the signal; D03 in isolation false-positives on every healthy
liquid token with broad distribution.

### 3. D01 sigmoid floor calibration issue

With **zero** honeypot signals (raw = 0.0), the static sigmoid produces
confidence 0.269, which `severity_from_confidence` maps to `Low`. The
"no signal at all" case should produce `Info` or `None`, not `Low`. The
formula `sigmoid(raw/0.55 − 1.0)` has its zero-input value pinned at
sigmoid(−1.0) ≈ 0.269 by design (so the curve has the right slope around
the firing threshold), but the consequence is a non-zero floor on every
token regardless of state. Two options for a follow-up sprint:

- Replace the offset-sigmoid with a piecewise function that hard-zeros for
  raw < ε.
- Map confidences below 0.30 to `Info` in the severity ladder (currently
  `Low` covers 0.20 – 0.40).

Until then, consumers must filter `severity == Low && confidence < 0.30` as
a no-signal case.

## Top-10 owners (as captured)

| # | Owner address | Raw amount | % of active supply |
|---|---|---:|---:|
| 1 | `GwH3Hiv5mACLX3ufTw1pFsrhSPon5tdw252DBs4Rx4PV` ◄ also mint authority | 14,200,768,337,139 | **18.9345%** |
| 2 | `CSqKhyW1cpdyjheAx5HXx4ibcnYrzpL5JywEMAkZixBK` | 9,242,120,556,277 | 12.3229% |
| 3 | `DbfGbmN89NGpXoEyuNj49tn29XTAr9Tw7y29bBnsJ74v` | 5,884,520,808,619 | 7.8461% |
| 4 | `5Je5sHZL5HrF8YDiwZDbpnsRSzzbeNYAjQSx4e2U5Uxd` | 4,048,206,787,747 | 5.3976% |
| 5 | `5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9` | 3,310,994,314,839 | 4.4147% |
| 6 | `7NQr63ojGTySSaPqD8w3bQ7pTFk4TdmvYGVjZzNtfwML` | 3,000,000,010,001 | 4.0000% |
| 7 | `Fue5m7uwemAhv1uyPSC44hd6UpBgAkubxiFCvBGmc8Ah` | 2,863,213,647,943 | 3.8176% |
| 8 | `35naBFBZG6bRw9kwjmsUdmTxXJTYUvw5V3SBtDeFDbfn` | 1,650,785,538,613 | 2.2011% |
| 9 | `AuVBa5BkKaxWNTn66GwE2MR7LtnYoFioWQcr1VTcej44` | 1,650,785,538,613 | 2.2011% |
| 10 | `5Z8wujmSTqvj7dYk8zQ4C5pwJwxoCpw4u6irZwUS6ZeJ` | 1,650,785,538,613 | 2.2011% |

The three identical balances at positions #8/#9/#10 (1,650,785,538,613 raw)
are a hint these are vaults of the same protocol family (likely Orca Whirlpool
LP positions or program-controlled vaults).

## Why D04 / D10 / D11 are UNKNOWN here

Public mainnet-beta rejects `getSignaturesForAddress` with HTTP 429 after
~2 pages on a hot mint. The CLI handles this gracefully — returns an
explicit UNKNOWN verdict with the rate-limit error attached, instead of
fabricating a number from partial data. To produce real D04/D10/D11 verdicts
on this token, run against a self-hosted RPC node:

```bash
./target/release/onchain-check-token \
  --token orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE \
  --rpc http://your-solana-node:8899
```

This is the operator deployment pattern from `feedback_cli_first_product`:
single binary + self-hosted RPC = full detector coverage. Public RPC is
sufficient only for D01 / D02 / D03 / D06 (state-only paths).

## Aggregate verdict (composite, manually computed)

The CLI does not yet emit a composite score (T27-7+ work). From the captured
detector outputs, the signals that actually fired with non-zero data are:

- **D03 MEDIUM** (top-10 = 63%) — explained above as healthy-DAO baseline
- **D02/D06 Medium** (mint authority active) — explained as DAO PDA
- **D01 Low (floor)** — a sigmoid artefact, not a real signal

Net verdict: **MEDIUM-driven-by-known-DAO-pattern**. With entity-label
suppression on the DAO PDA, the verdict collapses to `LOW` (only D03 top-10
remains, and even that is mostly DAO-treasury-driven). For now consumers
should treat ORCA as a **labelled-negative reference** for the calibration
pair.

## Comparison to labelled-positive ZBT (BSC)

`tests/fixtures/bsc/zbt/EXPECTED_VERDICT.md` (T26-10) holds the contrast
half. Once both fixtures are captured from real CLI runs, they form the
calibration pair driving threshold tuning.

| Signal | ORCA (Solana) | ZBT (BSC) |
|---|---|---|
| **D03 concentration** | MEDIUM (63%, but DAO-driven) | CRITICAL (top-1 ≈ 80%) |
| **D02/D06 mint authority** | Medium (DAO PDA) | High (active EOA owner + bytecode `mint()`) |
| **D01 honeypot** | Low (floor) | High (sell reverts) |
| **D04 pump-dump** | UNKNOWN (RPC) | NONE (steady-but-wash-driven) |
| **Composite** | MEDIUM (suppressible) | CRITICAL (compositionally) |

The point is not that any single detector tells you ORCA is healthy and ZBT
is a rug — it's that the **pattern of which detectors fire and how they
combine** does.

## Deterministic replay (the actual regression check)

The live-RPC capture above drifts every minute as new SPL accounts open and
balances move. To get a **bit-identical** regression run, point the CLI at
the captured holders fixture:

```bash
cargo run --release --bin onchain-check-token -- \
  --token orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE \
  --holders-file tests/fixtures/solana/orca/largest_accounts_full.json
```

This skips `get_token_holders` (so no public-RPC dependency for the D03 path)
and feeds the captured 89,756-owner snapshot directly into
`mg_onchain_detectors::signals::*`. **Expected output (assert exactly):**

| Field | Value |
|---|---:|
| Non-zero holders | `89756` |
| Zero-balance accounts | `0` (file is pre-aggregated by owner; dust accounts already collapsed) |
| Summed supply | `74999558708884` raw |
| `gini_descending` | `0.9981390335483853484782753804` |
| `top_n_pct(10)` | `0.6440043995542425093543931685` (64.400439955424250935439316850%) |
| Severity | `MEDIUM` |
| Top-1 owner | `GwH3Hiv5mACLX3ufTw1pFsrhSPon5tdw252DBs4Rx4PV` (18.9345%) |
| Top-3 owner | `DbfGbmN89NGpXoEyuNj49tn29XTAr9Tw7y29bBnsJ74v` (8.6727%) |

Mint state, age, and signature-rate paths still hit the live RPC — those
fixtures are not yet wired (Sprint 27 carry-forward T27-8). When they are,
this file gets a `--mint-file` / `--signatures-file` companion and the whole
verdict becomes RPC-free for CI.

When the rust_decimal precision rules change or the signal formulas get
recalibrated, the asserted values above shift — that's the regression
trigger. The bit-identical match is the entire point.

## Re-capture policy

ORCA's on-chain state evolves over time (DAO emissions, holder distribution
shifts, mint authority can be rotated by DAO vote). Re-run the CLI when:

- Mint authority changes (`grep mint_authority cli_output_2026-04-28.txt` vs new run)
- Top-1 share crosses 25% (current: 18.93%)
- Total holder count changes by ±20% (current: 89,826)
- Any new detector lands that adds to the verdict surface

When re-capturing, save the new transcript as `cli_output_YYYY-MM-DD.txt`
alongside this file, append a "What changed" section, and update the
captured-values table. **Do not rewrite the prior section** — historical
snapshots are part of the regression record.
