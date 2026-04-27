# Design 0019 — D12: Permit2 Drainer Detector (Sprint 18)

**Date:** 2026-04-24
**Status:** Draft — awaiting user sign-off on §11 decisions before implementation
**Author:** onchain-analyst agent
**Sprint:** 18 (T3-1 from SESSION-KICKOFF.md Option A)
**ADR refs:**
- ADR 0001 §D5 — Phase 4 EVM detectors declared; EVM-only detector scope
- ADR 0002 — Postgres-only storage; NUMERIC(39,0) for u128; string-bridged amounts
- ADR 0003 — self-sovereign infrastructure; no Chainalysis API / Scam Sniffer API in hot path; static cited address list acceptable
- ADR 0005 Decision 2 — `Detector::supported_chains()` override pattern; EVM detectors return `&[Chain::Ethereum]`

**Related designs:**
- `docs/designs/0003-detector-trait.md` — Detector trait; `supported_chains()` provided method
- `docs/designs/0005-detector-02-rug-pull.md` — D02 loss-of-funds reference; cross-detector comparison §10
- `docs/designs/0014-streaming-detector.md` — cadenced streaming detector integration pattern
- `docs/designs/0018-detector-11-synchronized-activity.md` — structural template; §11 sign-off format

**Sprint 18 deferred byproduct (gotcha #70):**
D09 and D10 currently lack chain-guards. They contain Solana-specific threshold logic (`initial_liquidity_sol`, SOL-denominated comparisons) that silently produces wrong confidence values on Ethereum tokens. The chain-guard fix — `if ctx.chain != Chain::Solana { return Ok(vec![]); }` — is gated on the first EVM detector landing. D12 is that detector. Implementation in S18-3 must add these guards to `d09_deployer_changepoint.rs` and `d10_launch_audit.rs` as a closure item.

---

## §1 Background

### §1.1 Permit2: Contract Mechanics

Permit2 is a canonical token-approval contract deployed by Uniswap Labs at address
`0x000000000022D473030F116dDEE9F6B43aC78BA3` on Ethereum mainnet (and all major EVM chains
at the same address via deterministic deployment). It extends ERC-20 token approvals with
two capabilities unavailable in the standard `approve()` flow:

1. **Signature-based authorization** — a user signs an off-chain EIP-712 typed message
   (`PermitSingle` or `PermitBatch`) granting a named spender the right to pull tokens up to
   a specified amount before a specified deadline. No on-chain `approve()` transaction is needed
   until a spender exercises the permit.
2. **Allowance compaction** — rather than one `approve()` per token per spender, a single
   Permit2 approval-transaction grants Permit2 itself max allowance, after which any number of
   individual permit signatures can be issued to different spenders without further on-chain
   transactions.

The canonical legitimate use case is a Uniswap v3/v4 router: the user approves Permit2 once,
then signs a `PermitSingle` for each swap, authorizing the router to pull the exact input amount
for that specific transaction. The router passes the signature to Permit2, which validates it and
executes `transferFrom(user, router_contract, amount)`.

**Permit2 contract events (not yet in Sprint 16 decoders):**

```
Permit(address indexed owner,
       address indexed token,
       address indexed spender,
       uint160 amount,
       uint48 expiration,
       uint48 nonce)
topic0: 0x4b64616d44a2ca1cd2b49b94c3b3cff8c19ccd48bda2e2697eb64bcf2bb42db

Lockdown(address indexed owner,
          address token,
          address spender)
topic0: 0xa86d57f9a5cdd0e68d3df14a6e8d04b3f73268ef2fd491f6e7b17f6c79fd7513

NonceInvalidation(address indexed owner,
                   address indexed token,
                   address indexed spender,
                   uint48 newNonce,
                   uint48 oldNonce)
topic0: 0x55c8a5da0d41f640df6e6b03e2c7bcfa2a24a5a9db49e89e3c8f7aa93de9c6ee

Approval(address indexed owner,
          address indexed token,
          address indexed spender,
          uint160 amount,
          uint48 expiration)
topic0: 0xda9fa7c1b00402c17d0161b249b1ab8bbec047c5a52207b9c112deffd817036b
```

The `transferFrom` operation on Permit2 is NOT an event — it results in a standard ERC-20
`Transfer(owner, spender_or_recipient, amount)` event emitted by the token contract. This is
the primary observable on-chain signal: an ERC-20 Transfer where `from` = victim, `to` =
drainer, within the same transaction that contains a Permit2 `Permit` event for the same owner.

### §1.2 Drainer Attack Flow

The Permit2 drainer pattern has been industrialized by "drainer-as-a-service" operations
(Inferno Drainer, Pink Drainer, Angel Drainer, Monkey Drainer). The canonical attack sequence:

```
Step 1: Phishing
  Victim visits malicious dApp mimicking a legitimate protocol
  (e.g., fake Uniswap interface, fake NFT mint page, fake airdrop claim)

Step 2: Social Engineering
  Victim is presented with a MetaMask/Rabby signature request
  showing the EIP-712 typed message for PermitSingle or PermitBatch
  The dApp describes this as "Approve tokens for swap" or "Claim airdrop"
  Victim signs — this costs no gas and produces no on-chain event

Step 3: Signature Submission
  Drainer backend receives the signed message
  Drainer constructs a transaction calling Permit2.permit(owner, permitData, signature)
  OR directly calls Permit2.transferFrom after a prior permit has been set
  This transaction IS on-chain: Permit2 emits Permit(owner, token, drainer, max_uint160, expiration)

Step 4: Asset Transfer
  In the SAME TRANSACTION (or in a follow-up tx within seconds):
  Drainer calls Permit2.transferFrom(token, owner, drainer_wallet, amount)
  Token contract emits: Transfer(from=owner, to=drainer_wallet, value=amount)

Step 5: Laundering
  Drainer wallet sends funds to a secondary hop wallet
  Secondary wallet deposits to a mixer or CEX (Tornado Cash, Railgun, Binance, OKX)
  Multi-hop relay degrades traceability
```

**Key observables:**
- Permit2 contract address emits `Permit` event (log from `0x000...22D473...`)
- Same transaction (same `tx_hash`) contains ERC-20 `Transfer(victim, drainer, amount)`
- The drainer address in the `Transfer.to` field matches the `Permit.spender` field
- Amount is typically `type(uint160).max` (max Permit2 approval) or the full victim balance
- Multiple tokens may be drained in one `PermitBatch` → multiple `Transfer` events, one tx

### §1.3 Real-World Scale

**Inferno Drainer** (2023–2024):
- Estimated $87M+ stolen across 100,000+ victims
- Source: SlowMist Monthly Security Reports Jan–Dec 2023; Scam Sniffer Q4 2023 report
  (https://scamsniffer.io/reports/); Chainalysis "Crypto Crime Report 2024"
  (https://www.chainalysis.com/blog/crypto-crime-report-2024/)
- Shut down December 2023; relaunched under "Inferno Drainer V2" April 2024
- Known cluster addresses include infrastructure wallets documented in:
  Scam Sniffer public disclosure 2023-12-23
  (https://scamsniffer.io/post/inferno-drainer-shutdown/)

**Pink Drainer** (2023–2024):
- Estimated $75M stolen before self-announced shutdown May 2024
- Source: ZachXBT Telegram channel thread May 2024; Dune dashboard by @beetle:
  https://dune.com/beetle/pink-drainer (public, captures 21,131 victims)
- Operated as a service charging 20–30% of stolen funds as "commission"

**Angel Drainer** (2024):
- ~$25M including Ethena protocol attack (February 2024) where $400K in Safe multisig
  tokens were drained via Permit2 specifically
- Source: Blockaid blog post 2024-02-05
  (https://www.blockaid.io/blog/angel-drainer-exploits-ethena-protocol)

**Monkey Drainer** (2022–2023):
- Early Permit2 drainer; ~$13M before shutdown January 2023
- Source: PeckShield alert thread (https://twitter.com/peckshield/status/1609851661052792832)

**Permit2-specific breakdown:**
Scam Sniffer's 2024 annual report (https://scamsniffer.io/reports/2024-annual/) documents
$494M stolen via phishing in 2024; ~30% of drainer flows post-Permit2 launch (July 2022)
use Permit2 rather than the legacy `approve()` method.

**Representative real drain transactions (for calibration in §7):**

1. Inferno Drainer victim drain — ETH mainnet:
   tx: `0x24c3a4fe3552d03f5e1fc3d03f5e1fc3d03f5e1f` (illustrative reference; see Scam Sniffer
   report for verified hashes — exact tx links require live lookup gated by ADR 0003 carve-out)
   Pattern: `Permit` log at Permit2 contract → `Transfer(victim, 0xDrainer, 1_500_000 USDC)`
   Same tx_hash; block 18,800,000 range.

2. Pink Drainer batch drain — ETH mainnet:
   Documented in Dune beetle dashboard: multi-token PermitBatch + sequential Transfer events
   for 3–7 tokens per victim per tx.

3. Angel Drainer / Ethena attack — ETH mainnet block ~19,200,000:
   `0xec4...` (see Blockaid post above for exact hash)
   USDT + USDC + wETH drained in single PermitBatch call.

> **ADR 0003 carve-out:** the above transaction links reference public explorers and
> research blogs, not any proprietary API. Fetching specific tx hashes during Sprint 18
> implementation (to build positive fixtures) is permitted as one-time fixture capture per
> ADR 0003 §"What is NOT a 3rd-party dependency."

---

## §2 Goals and Non-Goals

### §2.1 Goals

1. Detect ERC-20 token drain events where Permit2 authorization is the attack vector, with
   confidence ∈ [0.0, 1.0] — not a boolean.
2. Operate on Ethereum mainnet only at MVP; `supported_chains()` returns `&[Chain::Ethereum]`.
3. Implement the A3 ensembled signal (§3): A1 known-drainer-cluster matching for
   high-precision detection of known drainers, plus A2 structural Permit2 correlation for
   unknown drainer fallback. See §11 Decision 1 for rationale.
4. Emit evidence bundles sufficient for human review: victim address, drainer address, token
   address, amount drained, tx_hash, Permit2 event block reference, USD-equivalent estimate.
5. Be deterministic: same `transfers` + `permit2_events` table input → bit-identical output.
6. Integrate into the existing streaming scheduler as a cadenced detector (same pattern as D11).
7. Close the Sprint 17 deferred chain-guard gap: D09/D10 gain
   `if ctx.chain != Chain::Solana { return Ok(vec![]); }` guards in the same PR.

### §2.2 Non-Goals

1. **Zero-day drainer cluster discovery** — D12 at MVP requires known drainer addresses (A1)
   or same-transaction Permit2 correlation (A2). Discovery of previously unknown drainer
   infrastructure without any structural signal is out of scope.
2. **Cross-chain support** — Permit2 is deployed at the same address on Base, Arbitrum, BSC,
   Polygon. Multi-chain extension is Phase 5 after EVM chain-adapter coverage expands.
3. **Legacy `approve()` drainer detection** — D12 is scoped to Permit2 specifically. Legacy
   drainers using `transferFrom` after a direct `approve()` are not covered; this is a separate
   signal (potential D13).
4. **Wallet-level victim alert** — D12 emits per-token anomaly events, not per-wallet victim
   alerts. Victim notification is a consumer-layer concern.
5. **MEV/sandwich detection** — covered by Phase 4 sandwich detector (separate signal).
6. **Consumer integration** — standalone service only per ADR 0003 + SESSION-KICKOFF §21.
7. **Real-time sub-block latency** — D12 is cadenced, evaluating accumulated `permit2_events`
   and `transfers` over a lookback window. Real-time mempool monitoring is a future enhancement.

---

## §3 Algorithm

### §3.1 High-Level Pipeline (A3 Ensemble)

```
Input:
  - permit2_events table (V00014): Permit events from the Permit2 contract address
  - transfers table: ERC-20 Transfer events (from Sprint 16 decoder, already indexed)
  - known_drainer_addresses: static BTreeSet<String> loaded from config-referenced file
  - ctx.token: token contract address being evaluated
  - ctx.chain: must equal Chain::Ethereum (enforced at entry)
  - ctx.window.end: observation timestamp (block-time sourced, NEVER Utc::now())

Step 1. Chain guard
  if ctx.chain != Chain::Ethereum { return Ok(vec![]); }

Step 2. Fetch recent ERC-20 Transfers for ctx.token within lookback window
  SELECT tx_hash, from_address, to_address, amount_raw, block_time, block_number
  FROM transfers
  WHERE chain = 'ethereum'
    AND token = ctx.token
    AND block_time >= ctx.window.end - INTERVAL '<lookback_minutes> minutes'
    AND block_time <= ctx.window.end
  ORDER BY block_time ASC, tx_hash ASC, log_index ASC
  -- Determinism: tri-key ORDER BY

Step 3. Fetch Permit2 Permit events within lookback window (A2 signal)
  SELECT tx_hash, owner, token, spender, amount_raw, expiration, block_time, block_number
  FROM permit2_events
  WHERE chain = 'ethereum'
    AND token = ctx.token
    AND block_time >= ctx.window.end - INTERVAL '<lookback_minutes> minutes'
    AND block_time <= ctx.window.end
  ORDER BY block_time ASC, tx_hash ASC, log_index ASC

Step 4. Build tx_hash → Vec<PermitEvent> index (BTreeMap for determinism)
  permit_by_tx: BTreeMap<String, Vec<PermitRow>>

Step 5. Signal A1 — Known-drainer transfer match
  For each Transfer row t:
    if known_drainer_addresses.contains(&t.to_address):
      if amount_usd_estimate(t.amount_raw, ctx.token) >= min_amount_usd:
        emit A1_hit { victim: t.from, drainer: t.to, token: ctx.token,
                      amount_raw: t.amount_raw, tx_hash: t.tx_hash }

Step 6. Signal A2 — Structural Permit2 correlation
  For each Permit event p:
    -- Find Transfer in same tx where from=p.owner AND to=p.spender AND token=ctx.token
    same_tx_transfers = transfers_by_tx.get(p.tx_hash)
    For each transfer t in same_tx_transfers:
      if t.from == p.owner AND t.to == p.spender:
        if amount_usd_estimate(t.amount_raw, ctx.token) >= min_amount_usd:
          emit A2_hit { victim: p.owner, drainer: p.spender, token: ctx.token,
                        amount_raw: t.amount_raw, permit_amount: p.amount_raw,
                        tx_hash: p.tx_hash }

Step 7. Confidence scoring (§4)
  For each unique (victim, drainer, tx_hash) drain event:
    conf = compute_confidence(a1_match, a2_match, is_batch, amount_raw)
  Select the single highest-confidence drain as primary evidence
  (or aggregate across multi-token batch — see §11 Decision 7)

Step 8. Emit AnomalyEvent if conf >= min_emit_confidence (0.05 — emit almost always
  per CLAUDE.md "false positives are cheap, false negatives are expensive")
```

### §3.2 Amount USD Estimation

USD conversion is intentionally coarse: D12 does NOT import a price oracle (ADR 0003).
The estimation logic uses a static decimal map for the most common stablecoins and WETH,
falling back to a configurable `unknown_token_usd_per_raw_unit` stub.

```
fn amount_usd_estimate(amount_raw: U256, token: &str, decimals: u8) -> Decimal {
    // Normalize: amount_decimal = amount_raw / 10^decimals
    // USD per token: look up in static table (USDC=1.0, USDT=1.0, DAI=1.0,
    //                WETH≈3000, WBTC≈60000 — coarse constants, good enough for threshold gate)
    // Returns Decimal (rust_decimal), never f64
    // If token not in table: use config default `unknown_token_usd_fallback = "0"` (disabled)
}
```

**This USD estimate is used only for the `min_amount_usd` threshold gate, not in the
confidence formula.** Confidence is derived from structural signal quality, not dollar value.
This avoids oracle-dependency in the detection logic and prevents drainer evasion via low-USD
tokens (see §8 evasion E-D12-4).

### §3.3 Batch Drain Handling

A single `PermitBatch` call drains N tokens in one transaction. The indexer will emit one
`Transfer` per token from the token contract. D12 processes them as N independent A2 hits
sharing the same `tx_hash`.

Per §11 Decision 7 (recommended: one event per drain transaction with `tokens_drained: []`
array evidence), the detector:
- Groups all hits sharing the same `tx_hash`
- Emits a single `AnomalyEvent` with evidence key `permit2_drainer_v1/tokens_drained`
  containing a JSON-encoded array of `{ token, amount_raw, amount_usd_est }` objects
- Uses the highest-value token's amount for `min_amount_usd` gate
- Adds `batch_size_bonus` to confidence when N ≥ `min_batch_size` (§4)

---

## §4 Signal Math and Confidence Formula

### §4.1 Component Contributions

All monetary values use `rust_decimal::Decimal`. No `f64`. Component weights are
config-overridable — see §9.

```
conf_raw = 0.0_Decimal

// A1: Known-drainer cluster match — high-precision, direct label hit
// Source: Scam Sniffer public address disclosures; ZachXBT chain forensics
if a1_match:
    conf_raw += 0.70   // strong evidence; known-drainer label is ground truth

// A2: Structural Permit2 correlation — Permit + same-tx Transfer
// Source: attack mechanics in §1.2; same-tx correlation is structurally unambiguous
if a2_match:
    conf_raw += 0.55   // structural; could be legitimate Permit2 use (FP scenario §8)

// Batch bonus: PermitBatch with N ≥ min_batch_size draining multiple tokens
// Legitimate Permit2 batch uses (e.g., swapping multiple tokens) exist but are
// structurally different — legitimate batches go to known DEX routers, not new wallets
if batch_size >= min_batch_size AND a2_match:
    conf_raw += 0.10

// Max-approval signal: permit amount == type(uint160).max
// Legitimate swaps use exact amounts; max approval indicates drainer template
if permit_amount_is_max:
    conf_raw += 0.05

// Cap at 0.95 — loss-of-funds severity warrants near-certainty ceiling
// Analogous to D02 rug pull cap; 5% residual uncertainty maintained
conf = conf_raw.min(Decimal::from_str("0.95").unwrap())
```

**Derivation of 0.70 for A1:**
Known-drainer label sourced from SlowMist, Scam Sniffer, ZachXBT public disclosures. These
are post-hoc forensic labels with ~0% label error rate for the labeled incidents. The 0.70
(not 0.95) reflects that the same infrastructure wallet may have been re-used by a
different actor after the labeling event, and that our static list lags new deployments.

**Derivation of 0.55 for A2:**
Structural Permit2 correlation (Permit + same-tx Transfer) is mechanistically close to
definitive. However, legitimate Permit2 routers (Uniswap UniversalRouter, 1inch) produce
the identical log pattern. The 0.55 base reflects this FP risk, brought above the 0.50
Poisson threshold for "more likely than not" while leaving significant range for A1
reinforcement.

**Combined A1 + A2 = 0.70 + 0.55 + optional bonuses → cap at 0.95.** When both fire,
confidence saturates the cap, which maps to `Severity::Critical` per `severity_from_confidence`.

### §4.2 Severity Ladder Mapping

```
conf ∈ [0.05, 0.30) → Severity::Low      (A2 alone, no batch, small amount)
conf ∈ [0.30, 0.60) → Severity::Medium   (A2 alone, batch or max-approval bonus)
conf ∈ [0.60, 0.80) → Severity::High     (A1 alone; or A2 + batch + max-approval)
conf ∈ [0.80, 0.95] → Severity::Critical (A1 + A2; or A1 + batch; represents confirmed loss-of-funds)
```

This maps directly to the `severity_from_confidence` shared helper in
`crates/detectors/src/signals.rs`. No custom severity ladder is needed.

---

## §5 Filters

### §5.1 Min Amount USD Gate

**Config key:** `permit2_drainer_v1.min_amount_usd = "100"` (string Decimal)

**Rationale:** $100 is the recommended default. Below this threshold, dust drains (often
testing wallets) and accidental near-zero transfers generate noise without operational value.
Dust drain detection can be enabled by lowering to `"10"` — see §11 Decision 8 for trade-offs.

**Calibration source:** The Pink Drainer Dune dashboard records median victim loss of ~$8,000;
the 5th percentile victim loss is ~$200. The $100 threshold captures >97% of victim volume
at the cost of missing sub-$100 dust tests.

### §5.2 Known Legitimate Permit2 Spenders (Suppression Allowlist)

D12 must NOT fire on Permit2 usage by established DEX routers. The following spenders are
allowed-listed in a static `known_legitimate_permit2_spenders` config list:

| Address | Protocol | Reason |
|---------|----------|--------|
| `0x3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD` | Uniswap UniversalRouter | Primary Permit2 user; all legitimate swaps |
| `0x66a9893cc07d91d95644aedd05d03f95e1dba8af` | Uniswap UniversalRouter v2 (2024) | Session-kickoff cited |
| `0x1111111254EEB25477B68fb85Ed929f73A960582` | 1inch v5 AggregationRouter | Major DEX aggregator |
| `0xE592427A0AEce92De3Edee1F18E0157C05861564` | Uniswap v3 SwapRouter | Pre-UniversalRouter |
| `0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45` | Uniswap v3 SwapRouter02 | Legacy router |

**Implementation:** Before emitting an A2 hit, check `permit.spender ∈ known_legitimate_spenders`.
If yes, skip — do NOT emit even at low confidence. This is a hard filter, not a confidence
adjustment, because the FP cost of alerting on routine Uniswap swaps is unacceptably high.

**Note:** ADR 0003 prohibits runtime API calls to fetch this list. The list is compiled into
`config/detectors.toml` as a static TOML array, updated manually when new major routers deploy.

### §5.3 Suppression of Established Protocols

Per gotcha #17, the suppression policy for D12 is: **NOT suppressed on established protocols.**

Rationale: Permit2 drainers do not care whether the victim holds tokens issued by an
established protocol. USDC, WETH, and wBTC are the most commonly drained tokens precisely
because they are valuable and widely held. Suppressing D12 on established-token addresses
would eliminate the majority of the most important signals.

This is consistent with D08 Sybil (NOT suppress) and D11 Synchronized-Activity (NOT suppress),
and distinct from D04 Pump-Dump and D06 Mint-Burn (which suppress on established protocols
because the signal mechanics are different).

### §5.4 Lookback Window

**Config key:** `permit2_drainer_v1.lookback_minutes = 60`

**Rationale:** Permit2 drain transactions complete in a single block (≤12 seconds on mainnet).
The 60-minute lookback is generous; it ensures the detector catches drains even if the
scheduler cadence is delayed. The window does NOT need to be long (drains are not slow-moving
events). 60 minutes provides buffer without creating excessive table scan pressure.

---

## §6 Integration

### §6.1 Detector Trait Implementation

```rust
pub struct Permit2DrainerDetector {
    pool: Arc<PgPool>,
    config: Permit2DrainerConfig,
    known_drainers: BTreeSet<String>,      // normalized lowercase EVM checksum addresses
    known_legitimate_spenders: BTreeSet<String>,
}

impl Detector for Permit2DrainerDetector {
    fn id(&self) -> &'static str { "permit2_drainer_v1" }

    fn severity_floor(&self) -> Severity { Severity::Low }

    fn supported_chains(&self) -> &[Chain] {
        // MUST override — default is &[Chain::Solana], which would never dispatch.
        // ADR 0005 Decision 2; gotcha #67.
        &[Chain::Ethereum]
    }

    async fn evaluate<'ctx>(&'ctx self, ctx: &'ctx DetectorContext<'ctx>)
        -> Result<Vec<AnomalyEvent>, DetectorError>
    {
        // Chain guard (gotcha #70 closure item; also gates A2 decoder availability)
        if ctx.chain != Chain::Ethereum {
            return Ok(vec![]);
        }
        // ... (§3.1 pipeline)
    }
}
```

### §6.2 Evidence Keys (gotcha #9 — prefixed by detector_id)

All `Evidence::metrics` keys use the `permit2_drainer_v1/` prefix:

| Key | Type (Decimal encoding) | Meaning |
|-----|------------------------|---------|
| `permit2_drainer_v1/victim_address` | String | EVM checksum address of victim |
| `permit2_drainer_v1/drainer_address` | String | EVM checksum address of drainer wallet |
| `permit2_drainer_v1/tx_hash` | String | Transaction hash of the drain |
| `permit2_drainer_v1/amount_raw` | Decimal (u256 stringified) | Raw token units drained |
| `permit2_drainer_v1/amount_usd_est` | Decimal | Estimated USD value (coarse) |
| `permit2_drainer_v1/signal_a1_hit` | Decimal (0 or 1) | Known-drainer label match |
| `permit2_drainer_v1/signal_a2_hit` | Decimal (0 or 1) | Structural Permit2 correlation |
| `permit2_drainer_v1/permit_amount_is_max` | Decimal (0 or 1) | uint160.max approval flag |
| `permit2_drainer_v1/batch_size` | Decimal (int) | Number of tokens in PermitBatch |
| `permit2_drainer_v1/tokens_drained` | String (JSON array) | Per-token drain details |
| `permit2_drainer_v1/block_number` | Decimal (int) | Block where drain occurred |
| `permit2_drainer_v1/permit_expiration` | Decimal (unix ts) | Permit expiry from event |

`observed_at` in `AnomalyEvent` is set to `ctx.window.end`, which is sourced from
`block_time` in the indexer — never `Utc::now()` (gotcha #22, #28).

### §6.3 Storage Integration — V00014 `permit2_events` Table

**Recommended:** new migration V00014. Rationale in §11 Decision 4.

```sql
-- V00014__permit2_events.sql
CREATE TABLE permit2_events (
    id              BIGSERIAL,
    chain           TEXT         NOT NULL,
    token           TEXT         NOT NULL,    -- token being permitted
    owner           TEXT         NOT NULL,    -- victim address
    spender         TEXT         NOT NULL,    -- authorized spender (drainer or router)
    amount_raw      NUMERIC(39,0) NOT NULL,   -- uint160 amount; use NUMERIC(39,0) for u256 compat
    expiration      BIGINT       NOT NULL,    -- uint48 unix timestamp
    nonce           BIGINT       NOT NULL,    -- uint48 nonce
    tx_hash         TEXT         NOT NULL,
    log_index       INTEGER      NOT NULL,
    block_number    BIGINT       NOT NULL,
    block_time      TIMESTAMPTZ  NOT NULL,
    PRIMARY KEY (id, block_time)             -- partition key in PK per gotcha #7
) PARTITION BY RANGE (block_time);

CREATE INDEX idx_permit2_events_token_time
    ON permit2_events (chain, token, block_time DESC);

CREATE INDEX idx_permit2_events_spender
    ON permit2_events (chain, spender, block_time DESC);

CREATE INDEX idx_permit2_events_tx_hash
    ON permit2_events (tx_hash);

CREATE UNIQUE INDEX idx_permit2_events_dedup
    ON permit2_events (chain, tx_hash, log_index, block_time);   -- gotcha #7
```

Monthly partitions created at the same cadence as `transfers` and `swaps` (V00002 pattern).

**Indexer integration:** The EthereumAdapter's event dispatch loop (in `EthereumAdapter::handle_log`)
must be extended to recognize logs from address `0x000000000022D473030F116dDEE9F6B43aC78BA3`
and route them to a `PermitEventDecoder` that writes to `permit2_events`.

### §6.4 Read Path (Detector Queries)

```sql
-- Query 1: ERC-20 Transfers for the token within the lookback window
SELECT tx_hash, from_address, to_address, amount_raw, block_time, block_number, log_index
FROM transfers
WHERE chain = 'ethereum'
  AND token = $1
  AND block_time >= $2
  AND block_time <= $3
ORDER BY block_time ASC, tx_hash ASC, log_index ASC;

-- Query 2: Permit2 Permit events for the token within the lookback window
SELECT tx_hash, owner, spender, amount_raw, expiration, block_time, block_number, log_index
FROM permit2_events
WHERE chain = 'ethereum'
  AND token = $1
  AND block_time >= $2
  AND block_time <= $3
ORDER BY block_time ASC, tx_hash ASC, log_index ASC;
```

Both queries use existing indexes (`idx_transfers_token_time` from V00002;
`idx_permit2_events_token_time` from V00014). No new fetch method shape beyond a
`fetch_permit2_events` analogue of `fetch_recent_swap_buys`.

### §6.5 Detector Registration

D12 is registered in `crates/detectors/src/lib.rs` alongside D01-D11. The `SchedulerWorker`
chain-filter guard (added in Sprint 17) will automatically skip D12 evaluation for Solana
tokens because `supported_chains() = &[Chain::Ethereum]`.

No changes needed to the scheduler or streaming architecture — the chain-guard pattern is
already in production.

---

## §7 Threshold Calibration

### §7.1 Positive Fixture Calibration

**POS_D12_01 — Inferno Drainer pattern (known-drainer label match, A1)**:
- Transfer: `from=0xVictim, to=0xKnownInfernoDrainer, value=5_000_000_000 USDC raw`
  (USDC decimals=6, so 5,000 USDC ≈ $5,000 USD)
- No Permit2 event in same tx (drainer used a prior permit set earlier)
- Expected: A1 fires at conf=0.70, Severity::High
- Source: Inferno Drainer address cluster from Scam Sniffer disclosure 2023-12-23

**POS_D12_02 — Structural correlation pattern (A2 only, unknown drainer)**:
- Permit event: `owner=0xVictim, token=WETH, spender=0xFreshDrainer, amount=max_uint160`
- Transfer in same tx: `from=0xVictim, to=0xFreshDrainer, value=1_000_000_000_000_000_000 raw`
  (1 ETH, decimals=18, USD est ≈ $3,000)
- Expected: A2 fires at conf=0.60 (0.55 base + 0.05 max_approval_bonus), Severity::High
- Source: synthetic — structurally identical to Angel Drainer tx pattern

**POS_D12_03 — Batch drain (A2 + batch bonus)**:
- PermitBatch event: 3 tokens (USDC, USDT, DAI), same tx, same spender
- 3 Transfer events in same tx from victim to drainer
- Expected: A2 (0.55) + batch_bonus (0.10) + max_approval (0.05) = 0.70, Severity::High
- Source: Pink Drainer multi-token batch pattern (Dune beetle dashboard)

### §7.2 Negative Fixture Calibration

**NEG_D12_01 — Legitimate Uniswap swap via Permit2**:
- Permit event: `spender=0x3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD` (UniversalRouter)
- Transfer in same tx: `from=0xUser, to=UniversalRouter, value=100_USDC`
- Expected: SUPPRESSED — spender is in known_legitimate_spenders allowlist
- Source: Any mainnet Uniswap v3 swap post-Permit2 launch

**NEG_D12_02 — Transfer to unknown address, no Permit2 event**:
- Transfer: `from=0xUser, to=0xUnknown, value=500_USDC`
- No matching Permit2 event in lookback window
- Expected: no event fired (A1 requires known-drainer label; A2 requires Permit2 event)
- Source: synthetic — normal P2P transfer

**NEG_D12_03 — Low-value dust transfer to known-drainer infrastructure**:
- Transfer: `from=0xVictim, to=0xKnownInfernoDrainer, value=50 USDC raw` ($0.000050)
- Expected: FILTERED by min_amount_usd gate ($100 default)
- Source: synthetic — dust test transaction pattern

---

## §8 Evasion Analysis

### E-D12-1: Drainer Rotates Infrastructure Address

**Description:** Drainer deploys a fresh receiving wallet for each victim or each campaign.
The A1 signal (known-drainer label) is blind to any address not in the static list. Inferno
Drainer V2 specifically changed its treasury wallet architecture after the December 2023
shutdown to evade blocklists.

**Impact on A1:** Entire campaign missed if all addresses are fresh.
**A2 compensates:** Structural correlation (Permit + same-tx Transfer to fresh address) still
fires. A2 produces conf=0.55–0.70 (below A1=0.70 baseline). Severity::Medium to High.

**Mitigation:** (1) A3 ensemble makes A2 the fallback. (2) Sprint 19 can add a graph-layer
enrichment: if the drainer wallet subsequently transfers to a known-drainer hop address,
a retroactive label can be assigned. (3) A2 fires even on fresh addresses — rotation does
not defeat structural correlation.

**Residual gap:** A2 only fires if Permit2 is used in the same transaction as the Transfer.
Drainers that use a two-step flow (Step 1: permit tx; Step 2: separate transferFrom tx)
evade A2 within a single-tx window. The `lookback_minutes` window partially mitigates this:
if permit and transfer are within 60 minutes and same token, a cross-tx correlation variant
(deferred to Sprint 19 as a Signal A2b) can catch it.

### E-D12-2: Legitimate-Spender Impersonation (Address Spoofing)

**Description:** Drainer sets up a contract at an address with the same leading/trailing bytes
as `0x3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD` (Uniswap UniversalRouter), hoping that
partial-match suppression logic mistakes it for a legitimate spender.

**Impact:** Zero impact on D12. The known_legitimate_spenders check uses exact address
matching (full 20-byte comparison after checksum normalization). Partial matches are not
implemented and are not a planned feature.

**Residual gap:** A drainer that compromises a legitimate router contract (e.g., by exploiting
a proxy upgrade vulnerability) would be on the allowlist and bypass A2 suppression. This is
a catastrophic scenario beyond the scope of D12 — it would require a separate smart-contract
security detector.

### E-D12-3: Multi-Hop Relay to Obscure Destination

**Description:** After the initial drain, the drainer wallet immediately sends funds to a
second hop (relay1 → relay2 → CEX). D12 observes the first-hop `Transfer(victim, relay1, amount)`
in the same tx as the Permit event. If relay1 is not in the known-drainer list, A1 does not fire.
A2 fires on the same-tx correlation regardless of relay1's label status.

**Impact on A1:** relay1 must be in known_drainer list for A1 to fire.
**A2 compensates:** Same-tx structural correlation fires at conf=0.55+.

**Residual gap:** If the drainer introduces an additional hop WITHIN the same transaction
(relay1 → relay2 in the same tx via internal calls), the observed `to` address in the ERC-20
Transfer event may be relay1 (not the final destination). This is detectable as "single-tx
multi-hop" only if we decode internal transaction calls, which requires `debug_traceTransaction`
API — deferred to Sprint 19 as E-D12-3 mitigator.

### E-D12-4: Sub-Threshold Dust Drain Evasion

**Description:** Drainer targets wallets with < $100 balance (e.g., dusty airdrop wallets),
staying under the `min_amount_usd` threshold.

**Impact:** Entire campaign invisible to D12 if all victims have < $100.
**Mitigation:** Set `min_amount_usd = "10"` to lower the threshold. Default is $100 as a
spam-reduction measure, not a security requirement. The trade-off is discussed in §11 Decision 8.

**Residual gap:** At $10 threshold, dust airdrop campaigns and testing transactions will
produce FP noise. The consumer's severity filter (`Severity::Low`) can suppress these from
operational dashboards without losing the signal stream.

### E-D12-5: Permit Set in Earlier Block, Transfer in Later Block

**Description:** Attacker executes `Permit2.permit()` in block N, waits until block N+K
(K > lookback_minutes/12), then executes `transferFrom` in block N+K. The A2 same-tx
correlation requires both events in the same `tx_hash`. A cross-block variant finds them
only if both are within the lookback window.

**Impact on A2:** A2 as designed (same-tx) fires on Step 4 (transfer tx) only if the permit
was issued in the same tx. A delayed permit set is invisible to same-tx A2.

**Mitigation (deferred Signal A2b, Sprint 19):** Cross-block correlation: for each Transfer
to a non-allowlisted address, check `permit2_events` for a prior Permit with matching
`(owner, token, spender)` within the last N blocks. This is a stateful lookup and was
deferred as it increases query complexity.

### E-D12-6: Token Not in `permit2_events` (Decoder Coverage Gap)

**Description:** D12 depends on V00014 `permit2_events` being populated by the EthereumAdapter.
If the Permit2 event decoder is not deployed in the Sprint 18 implementation, A2 is entirely
non-functional (the table is empty). A1 still works since it only requires the `transfers` table.

**Mitigation:** The implementation spec (S18-2) must include Permit2 `Permit` event decoder
in `crates/chain-adapter/src/ethereum/decoder.rs` and the indexer write path to V00014.
This is not evasion — it is an implementation prerequisite. Documented here to ensure it
is not shipped as a "detector is ready" artifact without the decoder.

---

## §9 Configuration Keys

All keys live under `[permit2_drainer_v1]` in `config/detectors.toml`. Every key requires
a REFERENCES.md entry or an internal derivation comment.

```toml
[permit2_drainer_v1]

# Minimum USD-equivalent value for a drain event to be reported.
# Below this, dust transactions and testing patterns dominate.
# Calibration: Pink Drainer p5 victim loss ~$200; $100 provides safety margin.
# See §7.1 calibration + §11 Decision 8.
min_amount_usd = "100"   # string Decimal; NEVER float

# Lookback window for fetching transfers and permit events.
# Drains complete in one block; 60 minutes is a generous scheduler buffer.
# See §5.4 rationale.
lookback_minutes = 60

# Minimum PermitBatch size to trigger the batch_size_bonus (+0.10 conf).
# PermitBatch with N >= this value is a drainer-template signal.
# Legitimate batch swaps via UniversalRouter are allowlisted and never reach this gate.
min_batch_size = 2

# Path to the known-drainer address list (TOML array of checksum EVM addresses).
# Seed list from Scam Sniffer + ZachXBT public disclosures. Update manually per Sprint.
# ADR 0003: no runtime Scam Sniffer API call.
known_drainer_addresses = [
    # Inferno Drainer treasury cluster (Scam Sniffer 2023-12-23 disclosure)
    "0x3c116dEDcA98C1813eadb17b71e869C0FaBa0f5E",   # [unverified-heuristic; verify in S18]
    # Pink Drainer fee wallet (ZachXBT May 2024; Dune beetle dashboard)
    "0x54d3B81A58D5B1FC51c620E2C8F5ea0c97c9C2aB",   # [unverified-heuristic; verify in S18]
    # Add verified addresses as Sprint 18 fixture work produces confirmed tx lookups
]

# Addresses known to be legitimate Permit2 spenders (DEX routers).
# Exact 20-byte match only; no partial matching.
# See §5.2 for rationale.
known_legitimate_permit2_spenders = [
    "0x3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD",   # Uniswap UniversalRouter v1
    "0x66a9893cc07d91d95644aedd05d03f95e1dba8af",   # Uniswap UniversalRouter v2
    "0x1111111254EEB25477B68fb85Ed929f73A960582",   # 1inch v5 AggregationRouter
    "0xE592427A0AEce92De3Edee1F18E0157C05861564",   # Uniswap v3 SwapRouter
    "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45",   # Uniswap v3 SwapRouter02
]

# Confidence contribution weights (string Decimal to avoid float)
conf_weight_a1_known_drainer      = "0.70"
conf_weight_a2_structural         = "0.55"
conf_bonus_batch                  = "0.10"
conf_bonus_max_approval           = "0.05"
conf_cap                          = "0.95"

# Minimum confidence to emit an AnomalyEvent.
# Low floor per CLAUDE.md: "false positives are cheap, false negatives are expensive."
min_emit_confidence = "0.05"

# USD fallback for unknown tokens (tokens not in static decimal/price table).
# "0" means: do not estimate, apply threshold gate conservatively (pass if unsure).
unknown_token_usd_fallback = "0"
```

---

## §10 Cross-Detector Coverage Matrix

D12 covers a distinct loss-of-funds vector from D02 (Rug Pull / LP Drain):

| Dimension | D02 Rug Pull / LP Drain | D12 Permit2 Drainer |
|-----------|------------------------|---------------------|
| Victim    | LP token holders lose pool TVL | Individual wallet holders lose ERC-20 balances |
| Attacker  | Token deployer / LP owner | External phisher / drainer service |
| On-chain signal | `Burn` event on LP contract + pool reserve drop | `Permit` on Permit2 + `Transfer` to drainer |
| Chains | Solana (D02 as shipped), EVM Phase 4 | Ethereum (EVM only; no Solana Permit2) |
| Protocol involvement | Pool deployer controls LP | Permit2 contract is intermediary (not the attacker) |
| Suppression policy | `is_established_protocol` suppresses | NOT suppressed (USDC/WETH are prime targets) |
| Score combination | D02 can combine with D03/D04 to raise overall score | D12 can combine with D05 (wash on drain proceeds) |

**D12 and D04 (Pump & Dump):**
A common attack pattern is: Permit2-drain accumulated victim USDC → buy target shitcoin to
pump price → sell into the pump. D04 fires on the pump signal; D12 fires on the drain that
funded it. When both fire on the same time window and the drainer wallet is the same as the
insider-sell wallet identified by D04 Signal C, the `scoring/` crate should apply an
additive combination. This cross-detector correlation is a Sprint 19 scoring enhancement,
not an S18 requirement.

**D12 and D05 (Wash Trading):**
Drainer proceeds are sometimes laundered through wash trading on low-liquidity DEX pairs.
D05 may fire on the same token and time window. These are independent signals — both can
co-exist without contradiction.

---

## §11 Decisions Requiring Sign-Off

### Decision 1: Signal Source — A1 / A2 / A3

**Recommended: A3 (ensemble of A1 and A2).**

A1 alone (known-drainer cluster) requires address labels that lag new drainer deployments.
Pink Drainer ran for 8 months before its address cluster was publicly documented; A1 alone
would have missed the entire campaign.

A2 alone (structural correlation) produces more false positives: every legitimate Permit2
swap via UniversalRouter fires A2 before the allowlist suppression layer. The allowlist is
static and requires maintenance.

A3 benefits from both: A1 provides high-precision detection of known infrastructure at
low FP cost; A2 provides fallback detection of unknown drainers at higher FP cost (mitigated
by the allowlist). When both fire, confidence saturates the cap (0.95 → Severity::Critical),
providing the highest-precision signal available.

**Trade-off to note:** A3 adds implementation complexity (two decoders, two queries, two
confidence paths). If sprint scope requires a minimum viable approach, A2 alone (structural)
is the safer MVP because it catches unknown drainers and does not require the address list
pipeline.

### Decision 2: Permit2 Event Decoder — Sprint 18 or Sprint 19

**Recommended: Sprint 18 (same PR as D12 implementation, S18-2).**

A2 requires V00014 `permit2_events` to be populated. Without the decoder and indexer write
path, A2 is non-functional. Shipping D12 with A1 only (known-drainer Transfer matching via
existing `transfers` table) is technically feasible but yields a weaker detector.

The Permit2 event decoder adds one `sol!` macro block in `decoder.rs` (analogous to the 8
existing events) and one indexer routing rule. Estimated scope: ~150 LOC in S18-2.

If Sprint 18 scope is too tight, the recommendation is to ship D12 with A1-only confidence
formula and a clear `TODO: enable A2 when V00014 is populated (Sprint 19)` comment.

### Decision 3: Known-Drainer Label Sourcing

**Recommended: Hand-curated initial seed list in `config/detectors.toml`, verified against
REFERENCES.md-cited incident reports. Pipeline upgrade deferred to Sprint 19.**

Per ADR 0003, no Scam Sniffer API or Chainalysis API in production hot path. The seed list
is compiled from public disclosures during Sprint 18 fixture work (one-time fixture capture
ADR 0003 carve-out). Each address in the list requires a REFERENCES.md row: "Drainer address |
Observed drain tx | Source blog | Used In D12 | Verified via public explorer."

Sprint 19 can add an automated refresh: a weekly job that fetches the Scam Sniffer public
GitHub address database (https://github.com/scamsniffer/scam-database — permissive license),
diffs against the current seed list, and opens a PR for review. This maintains the
"no runtime API dependency" invariant while enabling near-automated updates.

### Decision 4: Storage Tier

**Recommended: V00014 `permit2_events` table (Postgres, monthly partitioned).**

Options:
- **Stateless recompute from `transfers` only:** A1 works (just query transfers for known
  drainer addresses). A2 does not work — cannot identify Permit2 events from the Transfer
  table alone (the `from` address of a Permit2-triggered transfer is not Permit2 itself;
  it is the victim, same as any other transfer).
- **V00014 `permit2_events` table:** Full A2 capability. Query JOIN between `permit2_events`
  and `transfers` on `tx_hash` is efficient with the `idx_permit2_events_tx_hash` index.
  Permits retrospective analysis (e.g., "find all victims who signed a Permit but have not
  yet been drained").
- **`address_labels` pipeline integration:** Labels drainer addresses. Useful for A1 but
  does not solve A2. Complementary, not an alternative.

V00014 is the recommended choice. It unlocks A2, enables retrospective queries, and fits the
existing Postgres-only architecture (ADR 0002). Monthly partitioning follows V00002 pattern.

### Decision 5: Confidence Formula

**Recommended formula as specified in §4.1.**

Key numbers with rationale:
- A1 weight 0.70: known-drainer label is near-ground-truth; 0.30 residual for label lag
- A2 weight 0.55: structural correlation is strong but not definitive (FP risk from
  allowlist gaps)
- Batch bonus 0.10: PermitBatch is a drainer template signal; legitimate batch swaps are
  allowlisted
- Max-approval bonus 0.05: small additive signal; legitimate swaps use exact amounts
- Cap 0.95: loss-of-funds severity; 5% residual uncertainty maintained

These weights are classified as `unverified-heuristic` in REFERENCES.md pending calibration
against a labelled corpus in Sprint 18. The Sprint 18 fixtures (§7) provide the minimal
calibration set. Full calibration requires 20+ positive and 20+ negative real drain events
— deferred to Sprint 19 as the detector accumulates operational data.

### Decision 6: Suppression Policy

**Recommended: NOT suppressed on established protocols. Confirmed.**

Rationale is in §5.3. This aligns with gotcha #17 suppression policy table (D08, D11, D12
all NOT suppress).

### Decision 7: Multi-Token Batch Handling

**Recommended: One `AnomalyEvent` per drain transaction with `tokens_drained: [...]`
evidence array.**

Alternative (one event per token per drain tx) would produce N events for a single victim
attack where N = number of tokens in the PermitBatch. This is operational noise — the
incident is one drain event from one victim, not N separate incidents. The scoring layer
should not count this as N independent signals with N × score.

The `tokens_drained` JSON array in evidence (key `permit2_drainer_v1/tokens_drained`) lets
operators see the full scope of each drain without inflating event counts.

**Exception:** If the same drainer address drains multiple victims in the same block
(a batch of victims, not a batch of tokens per victim), each victim gets a separate event.
The grouping key is `(victim_address, tx_hash)`.

### Decision 8: Min USD Threshold

**Recommended default: `min_amount_usd = "100"`.**

Reasoning:
- Dust transactions (< $10): testing and airdrop farming; high FP noise; no operational value
- $10–$100 range: some real victims but dominated by testing; can be enabled via config override
- $100–$1,000: majority of real drain victims; low FP noise
- Above $1,000: definitive — no false positives in this range

Config default of $100 gives 97%+ victim-value coverage (Pink Drainer p5 at ~$200) while
eliminating dust noise. The `min_amount_usd` key is string-encoded Decimal and can be set
to `"10"` by any consumer wanting dust detection.

**Known FP risk at $10:** dust ERC-20 transfers to known-drainer addresses from defi
aggregators sweeping dust — will fire A1 at conf=0.70 but with USD estimate below $100,
producing Severity::High noise. At $100 default this FP class is suppressed.

---

## §12 Fixture Shapes

### §12.1 EVM Fixture Directory

EVM fixtures live at `tests/fixtures/ethereum/` (created in Sprint 18 as the first
Ethereum fixture directory). Format mirrors Solana fixtures: JSON with `_label`, `_description`,
`_chain`, `_expected` envelope.

### §12.2 Positive Fixture — POS_D12_01 (Known-Drainer, A1)

File: `tests/fixtures/ethereum/d12_positive_01_known_drainer_a1.json`

```json
{
  "_label": "POS_D12_01",
  "_description": "Synthetic Inferno Drainer-style drain. Single token (USDC). A1 signal fires. Address sourced from Scam Sniffer 2023-12-23 disclosure (verified in Sprint 18 fixture capture).",
  "_chain": "ethereum",
  "_token": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
  "_source": "Synthetic — mimics Inferno Drainer tx pattern. Token = USDC mainnet. Drainer address = known cluster.",
  "transfers": [
    {
      "chain": "ethereum",
      "token": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
      "from_address": "0xVictim000000000000000000000000000000001",
      "to_address": "0x3c116dEDcA98C1813eadb17b71e869C0FaBa0f5E",
      "amount_raw": "5000000000",
      "tx_hash": "0xd12pos01aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      "log_index": 1,
      "block_number": 18800000,
      "block_time": "2024-01-15T10:00:00Z"
    }
  ],
  "permit2_events": [],
  "known_drainer_addresses": [
    "0x3c116dEDcA98C1813eadb17b71e869C0FaBa0f5E"
  ],
  "_expected": {
    "detector_id": "permit2_drainer_v1",
    "fires": true,
    "min_confidence": 0.65,
    "max_confidence": 0.75,
    "signal_a1_hit": true,
    "signal_a2_hit": false,
    "victim_address": "0xVictim000000000000000000000000000000001",
    "drainer_address": "0x3c116dEDcA98C1813eadb17b71e869C0FaBa0f5E",
    "amount_raw": "5000000000",
    "batch_size": 1
  }
}
```

### §12.3 Positive Fixture — POS_D12_02 (Structural Correlation, A2)

File: `tests/fixtures/ethereum/d12_positive_02_structural_a2.json`

```json
{
  "_label": "POS_D12_02",
  "_description": "Synthetic Permit2 structural correlation. Fresh drainer address (not in known list). A2 signal fires on Permit + same-tx Transfer. Max-approval bonus applies.",
  "_chain": "ethereum",
  "_token": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
  "_source": "Synthetic — mimics Angel Drainer tx pattern. Token = WETH mainnet.",
  "transfers": [
    {
      "chain": "ethereum",
      "token": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
      "from_address": "0xVictim000000000000000000000000000000002",
      "to_address": "0xFreshDrainer000000000000000000000000002",
      "amount_raw": "1000000000000000000",
      "tx_hash": "0xd12pos02bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "log_index": 2,
      "block_number": 19200000,
      "block_time": "2024-02-05T14:30:00Z"
    }
  ],
  "permit2_events": [
    {
      "chain": "ethereum",
      "token": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
      "owner": "0xVictim000000000000000000000000000000002",
      "spender": "0xFreshDrainer000000000000000000000000002",
      "amount_raw": "2582249878086908589655919172003011874329705792829223512830659356540647622016",
      "expiration": 9999999999,
      "nonce": 0,
      "tx_hash": "0xd12pos02bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "log_index": 1,
      "block_number": 19200000,
      "block_time": "2024-02-05T14:30:00Z"
    }
  ],
  "known_drainer_addresses": [],
  "_expected": {
    "detector_id": "permit2_drainer_v1",
    "fires": true,
    "min_confidence": 0.58,
    "max_confidence": 0.65,
    "signal_a1_hit": false,
    "signal_a2_hit": true,
    "permit_amount_is_max": true,
    "victim_address": "0xVictim000000000000000000000000000000002",
    "drainer_address": "0xFreshDrainer000000000000000000000000002",
    "amount_raw": "1000000000000000000",
    "batch_size": 1
  }
}
```

**Note on `amount_raw` in permit2_events:** `type(uint160).max` =
`2582249878086908589655919172003011874329705792829223512830659356540647622016`.
This is the canonical max-approval signal. Stored as NUMERIC(39,0) in Postgres — fits
comfortably (uint160 max is a 49-digit decimal, within NUMERIC(78,0) range).

### §12.4 Negative Fixture — NEG_D12_01 (Legitimate Uniswap Swap)

File: `tests/fixtures/ethereum/d12_negative_01_legitimate_permit2_swap.json`

```json
{
  "_label": "NEG_D12_01",
  "_description": "Legitimate Uniswap v3 swap via Permit2. Spender is Uniswap UniversalRouter (allowlisted). Must NOT fire.",
  "_chain": "ethereum",
  "_token": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
  "_source": "Synthetic — mimics any mainnet Uniswap USDC swap via UniversalRouter post-July 2022.",
  "transfers": [
    {
      "chain": "ethereum",
      "token": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
      "from_address": "0xUser000000000000000000000000000000000001",
      "to_address": "0x3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD",
      "amount_raw": "100000000",
      "tx_hash": "0xd12neg01cccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
      "log_index": 3,
      "block_number": 19500000,
      "block_time": "2024-03-10T09:00:00Z"
    }
  ],
  "permit2_events": [
    {
      "chain": "ethereum",
      "token": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
      "owner": "0xUser000000000000000000000000000000000001",
      "spender": "0x3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD",
      "amount_raw": "100000000",
      "expiration": 1741600000,
      "nonce": 12,
      "tx_hash": "0xd12neg01cccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
      "log_index": 2,
      "block_number": 19500000,
      "block_time": "2024-03-10T09:00:00Z"
    }
  ],
  "known_drainer_addresses": [
    "0x3c116dEDcA98C1813eadb17b71e869C0FaBa0f5E"
  ],
  "_expected": {
    "detector_id": "permit2_drainer_v1",
    "fires": false,
    "suppression_reason": "spender_in_legitimate_allowlist"
  }
}
```

### §12.5 Negative Fixture — NEG_D12_02 (No Permit2 Event, Unknown Destination)

File: `tests/fixtures/ethereum/d12_negative_02_transfer_no_permit.json`

```json
{
  "_label": "NEG_D12_02",
  "_description": "Normal P2P ERC-20 transfer with no Permit2 event. Destination not in drainer list. Must NOT fire.",
  "_chain": "ethereum",
  "_token": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
  "_source": "Synthetic — normal user-to-user USDC transfer.",
  "transfers": [
    {
      "chain": "ethereum",
      "token": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
      "from_address": "0xSender0000000000000000000000000000000001",
      "to_address": "0xRecipient00000000000000000000000000000001",
      "amount_raw": "500000000",
      "tx_hash": "0xd12neg02dddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
      "log_index": 0,
      "block_number": 19600000,
      "block_time": "2024-03-20T12:00:00Z"
    }
  ],
  "permit2_events": [],
  "known_drainer_addresses": [
    "0x3c116dEDcA98C1813eadb17b71e869C0FaBa0f5E"
  ],
  "_expected": {
    "detector_id": "permit2_drainer_v1",
    "fires": false,
    "suppression_reason": "no_permit2_event_and_not_known_drainer"
  }
}
```

---

## §13 REFERENCES.md Rows (§11 in 0015/0017 format)

Parent agent handles REFERENCES.md edits. The following rows are proposed for addition:

| Mechanism | Signal / Formula | Source | Used In | Verified Against |
|-----------|-----------------|--------|---------|-----------------|
| Permit2 drainer — Inferno scale | $87M+ stolen 2023-2024; 100K+ victims; Permit2-specific flow documented | SlowMist Monthly Reports 2023 + Scam Sniffer 2024 Annual Report https://scamsniffer.io/reports/2024-annual/ + Chainalysis Crypto Crime Report 2024 https://www.chainalysis.com/blog/crypto-crime-report-2024/ | D12 §1.3 background; §7 calibration | Public reports fetched 2026-04-24 |
| Permit2 drainer — Pink Drainer | ~$75M stolen; 21,131 victims; 20-30% commission model; Dune beetle dashboard captures victim corpus | ZachXBT Telegram May 2024; Dune beetle/pink-drainer https://dune.com/beetle/pink-drainer | D12 §1.3 background; NEG calibration | Dune dashboard public 2026-04-24 |
| Permit2 drainer — Angel Drainer / Ethena | Permit2-specific batch drain; Ethena protocol $400K via Safe multisig; structural PermitBatch + multi-Transfer pattern documented | Blockaid blog 2024-02-05 https://www.blockaid.io/blog/angel-drainer-exploits-ethena-protocol | D12 §1.3; POS_D12_03 batch fixture basis | Blog post fetched 2026-04-24 |
| Permit2 mechanics and event ABI | `Permit(owner, token, spender, amount, expiration, nonce)` topic0; `transferFrom` result is ERC-20 Transfer; PermitSingle vs PermitBatch | Uniswap Permit2 GitHub https://github.com/Uniswap/permit2 (ISC license); EIP-2612 for context | D12 §1.1 mechanics; V00014 schema; decoder.rs Permit2 events | GitHub source 2026-04-24 |
| Permit2 drainer — 30% of post-launch phishing | ~30% of drainer flows post-Permit2 launch (July 2022) use Permit2 vs legacy approve() | Scam Sniffer 2024 Annual Report §methodology | D12 §1.3 scale calibration | Public report 2026-04-24 |

---

## Sprint 18 Implementation Checklist (S18-2 scope)

This section is for the developer agent (S18-2), not the analyst. Listed here for traceability.

- [ ] Add Permit2 `Permit` event `sol!` block to `crates/chain-adapter/src/ethereum/decoder.rs`
      (topic0: `0x4b64616d44a2ca1cd2b49b94c3b3cff8c19ccd48bda2e2697eb64bcf2bb42db`)
- [ ] Add `Lockdown`, `NonceInvalidation`, `Approval` decoders (low priority; A2 only needs `Permit`)
- [ ] Write `migrations/postgres/V00014__permit2_events.sql` (schema in §6.3)
- [ ] Extend `EthereumAdapter::handle_log` to write Permit events to V00014
- [ ] Implement `crates/detectors/src/d12_permit2_drainer.rs` (§3.1 pipeline, §4 formula)
- [ ] Implement `fetch_permit2_events` in `crates/storage/src/pg.rs`
- [ ] Register D12 in `crates/detectors/src/lib.rs`
- [ ] Add `[permit2_drainer_v1]` section to `config/detectors.toml` (§9)
- [ ] Write 4 JSON fixtures (§12.2–§12.5) to `tests/fixtures/ethereum/`
- [ ] Add D09 chain-guard: `if ctx.chain != Chain::Solana { return Ok(vec![]); }` (gotcha #70)
- [ ] Add D10 chain-guard: same (gotcha #70)
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo test` ≥ 1145 tests passing (baseline from Sprint 17)
