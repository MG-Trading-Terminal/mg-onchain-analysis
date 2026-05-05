# Ethereum blue-chip baseline — captured 2026-04-29

Regression-artefact set. Six well-known Ethereum tokens (3 custodial
stablecoins, 1 trustless wrap, 2 memecoins). Each one has a `<symbol>_run.txt`
file with the verbatim CLI output captured against `https://ethereum-rpc.publicnode.com`.

Purpose: when the calibration code or detector logic changes, re-run the same
six tokens, diff the new composite + driving signals against this snapshot.
A spread on the well-known cases means the calibration moved.

## Captured composite verdicts

| Symbol | Address | Composite | Conf | Reality check |
|---|---|---|---|---|
| **USDT** | `0xdAC17F958D2ee523a2206206994597C13D831ec7` | **CRITICAL** | 0.87 | Tether multi-sig owner active + `issue()` mint + `pause()` — full custodial control |
| **USDC** | `0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48` | **CRITICAL** | 0.90 | Circle FiatTokenProxy (ZeppelinOS) → mint + pause + blacklist visible through delegatecall |
| **WBTC** | `0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599` | **CRITICAL** | 0.91 | BitGo controller + `mint()` selector + `pause()` — fully custodial wrapped BTC |
| **WETH** | `0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2` | **INFO / clean** | 0.00 | Trustless wrap — no Ownable, no mint, no pause; behaves exactly as advertised |
| **LINK** | `0x514910771AF9Ca656af840dff83E8264EcF986CA` | **MEDIUM** | 0.42 | No active owner, fixed supply; D04 spike sometimes fires (volume normalisation noise) |
| **PEPE** | `0x6982508145454Ce325dDbE47a25d4ec3d2311933` | **HIGH** | 0.64 | Renounced + no mint, BUT D03 + D04 stack on whale-driven volatility — appropriate "memecoin caution" verdict |

## Calibration story behind these numbers

This baseline is the validation that calibration done in T27-26 .. T27-34 is
sound across the full ladder:

- **T27-26** (entity-label suppression): without it, every actively-traded
  token false-fired HIGH because Uniswap pool contracts dominated net flow.
  With suppression, USDT/USDC/WBTC's HIGH composite is driven by D02/D06
  (real custodial control) rather than D03 (DEX-pool noise).
- **T27-30** (composite weighted noisy-OR): WETH stays INFO because it has
  no operational signals; LINK lands at MEDIUM because D04 fires but D02 is
  benign; USDT/USDC/WBTC stack to CRITICAL via D02 + D06 + D01 correlated
  custodial signals.
- **T27-31** (D04 fresh-pool guard): keeps WETH at INFO instead of false-
  firing LOW spike on micro-baseline trailing windows.

## Tolerance bands (regression bounds)

When re-running this fixture later, allow some drift since on-chain state
moves:

- **CRITICAL group (USDT/USDC/WBTC)**: composite ∈ [0.80, 0.95]. Falling
  below 0.80 means D02/D06 / proxy resolution regressed; rising to ≥0.95
  means a new signal added without the team realising.
- **WETH**: composite ∈ [0.00, 0.15]. Anything above 0.15 means a detector
  is false-firing on a structurally-clean token.
- **LINK**: composite ∈ [0.20, 0.55]. The volatile band is D04 spike noise —
  outside this range investigate.
- **PEPE**: composite ∈ [0.40, 0.85]. D03 concentration math drifts as
  whale wallets shuffle.

## How to re-run this regression

```bash
cd ~/Projects/mg-onchain-analysis
for t in USDT USDC WETH WBTC LINK PEPE; do
  echo "── $t ──"
  ./target/release/onchain-check-token "$t" \
    | grep -E "composite verdict|RECOMMENDATION" | head -2
done
```

Diff the output against the table above. A symbol falling out of its
tolerance band is a calibration regression and needs investigation
before merging.

## Per-token captured artefacts

Each `<symbol>_run.txt` is the FULL stdout of the run, including:
- Token metadata (name, symbol, decimals, total supply, bytecode size)
- Per-detector verdict (D01 / D02 / D03 / D04 / D06 / D10) with rationale
- Composite verdict + driving signals + recommendation

Treat these files as point-in-time snapshots. On-chain state evolves, so
running again later will produce different exact numbers — the *shape* of
the verdict (CRITICAL custodial vs INFO trustless vs MEDIUM memecoin) is
the regression-stable assertion.
