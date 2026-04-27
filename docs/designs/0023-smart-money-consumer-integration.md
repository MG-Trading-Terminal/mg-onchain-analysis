# Design 0023 — Smart-Money Consumer Integration (Sprint 23, S23-1)

**Date:** 2026-04-25
**Status:** Draft — awaiting user sign-off on §11 decisions before implementation
**Author:** onchain-analyst agent
**Sprint:** 23 (S23-1: spec; S23-2: implementation)
**Predecessor:** `docs/designs/0022-smart-money-labelling-mvp.md` (Sprint 22 — labels shipped)

**ADR refs:**
- ADR 0002 — Postgres storage; NUMERIC for money; no float amounts
- ADR 0003 — self-sovereign infrastructure; no third-party labelling APIs
- ADR 0005 Decision 2 — `Detector::supported_chains()` override pattern

**Related designs:**
- `docs/designs/0007-detector-04-pump-dump.md` — D04 current spec
- `docs/designs/0008-detector-05-wash-trading.md` — D05 current spec
- `docs/designs/0015-crates-graph-phase3.md` — D08 current spec + `GraphLabelStore` trait
- `docs/designs/0017-d05-signal-b-graph-cycles.md` — D05 Signal B graph cycles
- `docs/designs/0022-smart-money-labelling-mvp.md` — smart-money labelling pipeline

**Binding prior art in REFERENCES.md:**
- Fu, Feng, Wu & Xu 2025 (Perseus, arXiv:2503.01686) — mastermind cross-event recurrence; pre-event
  buyer classification; 438 confirmed masterminds identified in live deployment
- Fantazzini & Xiao 2023 (Econometrics 11(3)) — informed early buyer, 60-min pre-event window
- Barras, Scaillet & Wermers 2010 (JoF 65(1)) — FDR skill/luck separation; tier thresholds

---

## §1 Background

### §1.1 Labels are live; labels are dead weight without consumers

Sprint 22 shipped the smart-money labelling pipeline. `address_labels` now receives
`LabelType::SmartMoney` rows with Tier 1/2/3 encoding in evidence JSON, written every 6 hours
by `SmartMoneyLabeller::run_batch`. As of Sprint 22 close:

- 1259 tests passing; 13 detectors unchanged; smart-money background task draining cleanly
- Labels written to `address_labels` via `PgGraphLabelStore::upsert_labels`
- Zero consumers of those labels — no detector reads them

Labels that are not consumed by at least one detector, scoring rule, or consumer API are
technical debt that costs DB write + TTL management overhead without providing signal value.
Sprint 23 converts the S22 investment into detector quality improvements by wiring three
existing detectors to consume smart-money labels as cross-detector amplification inputs.

### §1.2 Domain framing — alpha vs mastermind

The sprint brief identifies a critical domain tension that must be resolved before any
implementation decision is made:

**The alpha sense (Barras et al. 2010):** "smart money" means a wallet whose realized alpha
survives FDR correction at q < 0.10 — genuinely skilled vs. lucky. This is the statistical
ground truth, currently unavailable until Stage 2 FDR is unblocked.

**The crypto P&D sense (Perseus 2025):** "smart money" in a memecoin / shitcoin universe means
"early-informed buyer who positions before public knowledge." Perseus deployed from
February–October 2024, identified 438 masterminds responsible for $3.2T in artificial
trading volume. Every identified mastermind wallet is characterized by: buying before public
announcement, appearing across multiple events (recurrence ≥ 3), and selling before peak.
These are not legitimate skilled traders — they are coordinators of adversarial pump events.

**The critical asymmetry for consumer integration:**

Because the MVP uses Stage 3 timing features (recurrence, pre-event entry, sell-before-peak),
NOT Stage 2 FDR (data-blocked), the labels we produce in the shitcoin universe are primarily
the *crypto P&D sense* of smart money — informed-early-buyer / potential mastermind. Every
label carries `"calibration": "heuristic, not FDR-controlled"`.

This asymmetry determines amplification direction:
- D04 P&D: smart-money buying pre-pump = textbook mastermind signal per Perseus 2025.
  Amplification direction is UPWARD — higher confidence, higher severity.
- D08 Sybil: smart-money wallet inside a Sybil cluster = informed coordinated attacker, not a
  legitimate airdrop recipient. Amplification direction is UPWARD — common mis-intuition is
  that smart money "vouches" for legitimacy; the domain framing reverses this.
- D05 Wash trading: this is AMBIGUOUS. Smart money executing wash trades COULD be
  (a) legitimate market-making inventory management or (b) volume inflation by skilled actors
  who understand that volume metrics are being watched. The label alone cannot disambiguate.
  Safe default: NEUTRAL — emit metadata evidence, no confidence change.

### §1.3 Why this sprint is amplification-only, not new detectors

D04, D05, D08 are the three existing detectors with the strongest thematic connection to
smart-money behavior. No new detectors (D14+) are introduced in Sprint 23. The architectural
goal is: labels flow into existing detector confidence formulas as optional amplification
inputs with proper backwards-compat wiring. Future sprints can add D01, D02, D06, D09
consumption of smart-money labels if warranted.

---

## §2 Goals and Non-Goals

### §2.1 Goals

1. Implement `SmartMoneyLookup` trait in `crates/graph` (or `crates/detectors`) — wraps
   `GraphLabelStore::addresses_with_label(LabelType::SmartMoney)` as a per-address lookup.
2. Add optional `Option<Arc<dyn SmartMoneyLookup>>` field to `PumpDumpDetector` (D04),
   `WashTradingDetector` (D05), and `D08SybilDetector` (D08).
3. When `SmartMoneyLookup` is `Some`, each detector queries tiers for relevant addresses and
   applies per-tier confidence amplification (D04 + D08) or metadata emission (D05).
4. All amplification saturates at the existing confidence cap per detector:
   D04 cap = 0.95, D08 cap = 0.95, D05 Signal A cap = 0.95, D05 Signal B cap = 0.85.
5. All new evidence keys are prefixed with `pump_dump/`, `sybil_detection/`, or
   `wash_trading_h1/` respectively per gotcha #9.
6. When `SmartMoneyLookup` is `None`, detector behavior is unchanged (backwards-compat).
7. Production wiring: `crates/server/src/init/detectors.rs` injects the real lookup.
8. Test fixtures: `MockSmartMoneyLookup` injectable via `None` or mock in unit tests.
9. Deterministic output: given identical block range + label store state, output is bit-identical.
10. Add REFERENCES.md entry for D04/smart-money amplification, D08/smart-money amplification,
    D05/smart-money neutral evidence — citing Perseus 2025 (D04, D08) and design-derivation
    (D05) respectively.

### §2.2 Non-Goals

1. No new detectors (D14+).
2. No new migrations (V00016 is last per sprint brief; V00017 is next available but not
   needed here — label consumption is read-only).
3. No modification of D01-D03, D06, D07, D09-D13.
4. No modification of the smart-money labelling pipeline itself (design 0022).
5. No consumer-side integration (bot-trader, custody, MM, exchange) — standalone service only.
6. No Stage 2 FDR (data-blocked; config flag `smart_money_fdr_enabled` remains `false`).
7. No D05 confidence amplification — only metadata evidence (see Decision 5, §11).
8. No Sybil cluster aggregate PnL computation — that is Phase 5 (design 0022 §8 E-SM-1).

---

## §3 Algorithm

### §3.1 Lookup flow (all three detectors)

```
detector.evaluate(ctx)
  → if smart_money_lookup is Some:
      1. Collect relevant addresses for this evaluation
         (D04: buyers in pre-pump window; D08: cluster members; D05: round-trip wallets)
      2. For each address, call smart_money_lookup.tier_for(chain, address)
         → returns Option<SmartMoneyTier> or None
      3. Aggregate tiers: count Tier1, Tier2, Tier3 found
      4. Compute amplification delta (per §4; zero for D05)
      5. Apply: confidence_final = min(confidence_base + delta, cap)
      6. Add evidence keys: smart_money_present, smart_money_tiers, smart_money_amplification_delta
  → emit AnomalyEvent(confidence_final, ...)
```

### §3.2 Backwards-compat guarantee

When `smart_money_lookup = None` (existing tests, existing fixture runs), no DB query is
issued and the confidence formula is identical to pre-Sprint-23. The `Option<Arc<...>>` field
is `None` in all existing unit tests. Production wiring injects `Some(...)`.

### §3.3 Feedback loop guard (D04)

D04 anomaly events (pump_dump_v1) are consumed by the `SmartMoneyLabeller` as the "known
pump event" index for Stage 3. If D04 confidence feeds into smart-money labels which feed
back into D04 amplification, a runaway feedback loop is possible.

Guard: amplification applies ONLY to labels where `issued_at < ctx.window.block_start`. Labels
issued DURING or AFTER the current evaluation window are not eligible for amplification.
This is equivalent to: "smart money labels from a prior batch run amplify D04; D04 events
from this run do not yet update smart-money labels (6h batch lag)." The 6h batch interval
(design 0022 §6.4) creates a natural lag that breaks the synchronous feedback cycle.

---

## §4 Per-Detector Amplification Math

### §4.1 D04 Pump & Dump — pre-pump smart-money buyer amplification

**Rationale (Perseus 2025):** The 438 masterminds identified by Perseus all share the pattern:
systematic pre-event positioning followed by sell-before-peak. A wallet with `LabelType::SmartMoney`
(any tier) buying in the pre-pump accumulation phase is the textbook mastermind fingerprint.
The confidence of D04 is already measuring the pump signal itself; smart-money participation
amplifies the adversarial-intent hypothesis.

**Accumulation window definition:** Karbalaii 2025 documents that 70% of pump events have an
accumulation phase. Fantazzini & Xiao 2023 fix the pre-event window at 60 minutes. The
amplification query is: "how many wallets with `LabelType::SmartMoney` executed a buy swap
on this token within `pre_pump_window_minutes` of the current evaluation window start?"

```
pre_pump_window_start = ctx.window.block_start - pre_pump_window_minutes * 60s
pre_pump_buyers = swaps WHERE token = T
                       AND side = 'buy'
                       AND block_time BETWEEN pre_pump_window_start AND ctx.window.block_start
                       AND wallet IN (SmartMoney-labelled addresses for this chain)
```

**Minimum buyer count gate:** Amplification requires `smart_money_buyer_count >= 1` Tier1 OR
`smart_money_buyer_count >= 2` Tier2. A single Tier2 buyer is insufficient — single-event
presence is consistent with luck (base rate argument from design 0022 §7.1).

**Per-tier delta (additive, applied once per evaluation, not per wallet):**

```
delta_tier1 = +0.12   if tier1_count >= 1
delta_tier2 = +0.07   if tier2_count >= 2 AND tier1_count == 0
delta_tier3 = 0.00    (Tier3 = positive PnL only; insufficient signal for amplification)
```

**Justification for delta_tier1 = +0.12:**
Perseus 2025 reports that mastermind wallets (our Tier1 proxies) buy pre-event in 100% of
their confirmed pump events. Given D04's baseline confidence of 0.60 at threshold (Signal A),
adding +0.12 pushes a threshold-firing detector to 0.72 — Medium-to-High boundary. At
Signal A mid-point (0.75), Tier1 amplification yields 0.87 (High). This is calibrated so
that Tier1 amplification alone does not saturate the detector (0.95 cap requires a very strong
Signal A plus Tier1, which is the correct severity assignment for a confirmed mastermind buy
into a volume spike). No published numeric amplification delta exists in Perseus 2025 directly
— the 0.12 value is a design derivation. Tagged `unverified-heuristic` in config comment.

**Justification for delta_tier2 = +0.07:**
Tier2 = strong PnL OR ≥ 2 recurrence (one criterion). Less confident about informed-actor
status. Two Tier2 buyers required to unlock the amplifier (two independent data points at the
same event shifts probability). +0.07 at baseline yields 0.67 — still in Medium range, not a
severity escalation on its own.

**Final confidence formula for D04 with smart-money amplification:**

```
let delta = if tier1_count >= 1 { 0.12 }
            else if tier2_count >= 2 { 0.07 }
            else { 0.00 };
let cap = if base_signal == BaseSignal::A { 0.95 } else { 0.85 };
let confidence_final = (confidence_base + delta).min(cap);
```

Note: when Signal C (insider sell amplifier, existing +0.15) also fires, the deltas stack:
`confidence_final = (confidence_base + smart_money_delta + insider_amplifier).min(cap)`.
This is the correct behavior — independent signals corroborate each other. The shared cap
prevents overshoot.

**Evidence keys added (all prefixed `pump_dump/`):**

```
pump_dump/smart_money_present                  bool (1 or 0 as Decimal metric)
pump_dump/smart_money_tier1_buyer_count        Decimal(int)
pump_dump/smart_money_tier2_buyer_count        Decimal(int)
pump_dump/smart_money_tier3_buyer_count        Decimal(int)
pump_dump/smart_money_amplification_delta      Decimal (0.00, 0.07, or 0.12)
pump_dump/smart_money_pre_pump_window_minutes  Decimal (config value used)
```

### §4.2 D08 Sybil — cluster contains smart-money amplification

**Rationale (Perseus 2025 + Liu et al. 2025):** A Sybil cluster where one or more members
carry `LabelType::SmartMoney` is qualitatively different from a naive airdrop-farming
cluster. Smart-money membership in a Sybil cluster indicates that the cluster is either:
(a) operated by a financially sophisticated actor who has previously demonstrated timing
alpha, or (b) infiltrated / managed by a known mastermind wallet. Either interpretation
increases the probability that the Sybil cluster is coordinating adversarial behavior rather
than simply farming airdrops.

**Common mis-intuition:** "smart money = legitimate = cluster is less suspicious." This
reasoning reverses the domain framing. In the shitcoin universe, Tier1 smart-money wallets
are informed-early-buyer / mastermind proxies, NOT ethical skilled traders. The presence of
Tier1 in a Sybil cluster is evidence of sophistication, which increases adversarial risk.

**Addresses queried:** the full cluster member set (all members in `wallet_clusters` for
the detected cluster UUID).

**Per-tier delta:**

```
delta_tier1 = +0.08   if any cluster member is Tier1
delta_tier2 = +0.05   if any cluster member is Tier2 AND no Tier1 found
delta_tier3 = 0.00    (Tier3 insufficient for cluster amplification)
```

**Justification for delta_tier1 = +0.08:**
D08 baseline Signal A+B at full overlap (1.0, cluster_conf=0.85) = 0.74 (High). Adding +0.08
yields 0.82 (still High; cap is 0.95). At threshold (0.39), +0.08 = 0.47 (Medium). The
amplification is modest — a Sybil cluster at Low confidence plus Tier1 smart-money still
doesn't become Critical automatically. It requires both strong overlap AND smart-money to
approach the 0.90 range. Design derivation; tagged `unverified-heuristic`.

**Final confidence formula for D08 with smart-money amplification:**

```
// Existing D08 formula (unchanged):
let conf_raw_a = 0.40 + 0.40 * top_holder_overlap_pct;
let conf_raw_b = conf_raw_a * (0.50 + 0.50 * cluster_confidence);

// Smart-money amplification (new):
let delta = if tier1_found { 0.08 }
            else if tier2_found { 0.05 }
            else { 0.00 };
let confidence_final = (conf_raw_b + delta).clamp(0.0, 0.95);
```

**Evidence keys added (all prefixed `sybil_detection/`):**

```
sybil_detection/smart_money_present         bool (1 or 0 as Decimal metric)
sybil_detection/smart_money_tier1_count     Decimal(int)
sybil_detection/smart_money_tier2_count     Decimal(int)
sybil_detection/smart_money_tier3_count     Decimal(int)
sybil_detection/smart_money_amplification_delta  Decimal
```

### §4.3 D05 Wash Trading — neutral metadata evidence only

**Rationale for NEUTRAL (Decision 5):**

The smart-money label does not disambiguate the two competing hypotheses for smart-money
round-trips in D05:

(a) **Legitimate market-making:** Professional MMs on Raydium/Orca execute buy-sell cycles
to maintain inventory balance. Their round-trips satisfy D05 Signal A heuristics (same wallet,
same pool, same window) but represent legitimate price-discovery activity. Tier1 smart-money
wallets with genuine market-making activity would produce Signal A patterns.

(b) **Informed wash trading:** A sophisticated actor knowing that volume metrics are monitored
could deliberately execute wash trades to inflate baseline, then pump. This is evasion
E-D04-10 (pre-pump baseline contamination via wash trades). A Tier1 wallet executing wash
trades would be gaming the D04 baseline.

Without a secondary signal (e.g., wash volume to circulating supply > 30%, or concurrent
pool-creation event from the same wallet, or no external counterparty in the cycle), the
label alone cannot tell (a) from (b).

**Decision 5 recommendation: (c) NEUTRAL.** Emit evidence metadata ONLY. Do not change
`confidence`. The trading bot consumer can read `wash_trading_h1/smart_money_present = 1`
from the AnomalyEvent evidence and apply its own policy (e.g., "if smart_money_present AND
confidence > 0.70, escalate review; if smart_money_present AND confidence < 0.50, downgrade
priority").

**Evidence keys added (all prefixed `wash_trading_h1/`):**

```
wash_trading_h1/smart_money_present         bool (1 or 0 as Decimal metric)
wash_trading_h1/smart_money_tier1_count     Decimal(int) — wallets in round-trips that are Tier1
wash_trading_h1/smart_money_tier2_count     Decimal(int)
wash_trading_h1/smart_money_tier3_count     Decimal(int)
wash_trading_h1/smart_money_amplification_delta  Decimal (always 0.00 — neutral)
```

The `smart_money_amplification_delta = 0.00` is explicit and intentional — downstream
consumers can check the delta to understand whether smart-money affected confidence.

**D05 Signal B (cycle detection):** Same neutral treatment. Evidence keys on the cycle event:

```
wash_trading_h1/signal_b_cycles/smart_money_wallets_in_cycles  Decimal(int)
```

---

## §5 Filters

### §5.1 Maximum amplification delta guard

To prevent runaway confidence in degenerate cases (very high base + multiple deltas):

The summed delta from smart-money amplification (D04 or D08) is capped at
`max_smart_money_delta` = 0.15. This matches the existing Signal C insider amplifier cap
in D04. If both insider amplifier (+0.15) and smart-money Tier1 delta (+0.12) fire
simultaneously in D04:

```
total = base + 0.12 + 0.15 = base + 0.27
cap = 0.95
```

At base = 0.60 (threshold), total = 0.87, capped at 0.95 only if base is very high.
This is mathematically safe — the deltas are additive, the final `min(total, cap)` clamp
is the only binding constraint.

No additional guard is needed beyond the existing cap clamp.

### §5.2 Label staleness guard

Labels with `issued_at > ctx.window.block_start` (issued during or after the current
evaluation window) are excluded from amplification (feedback loop guard, §3.3).

Labels with `expires_at < ctx.observed_at` are excluded (expired labels, per design 0022
TTL = 720h). The `GraphLabelStore::addresses_with_label` query already filters expired labels.

### §5.3 Minimum confidence gate for lookup

Smart-money lookup is only triggered when the base confidence has already met a minimum:

```
D04: trigger lookup only if confidence_base >= 0.50 (Signal A or B has fired)
D08: trigger lookup only if conf_raw_b >= 0.30 (Signal A threshold met)
D05: trigger lookup always (metadata emission regardless of signal strength)
```

This prevents spending a DB query on tokens that are clearly below detection threshold.

---

## §6 Integration

### §6.1 SmartMoneyLookup trait

New trait in `crates/graph/src/smart_money_lookup.rs` (or as a `pub use` re-export from
`crates/graph/src/lib.rs`):

```rust
/// Read-only view into the smart-money label table.
///
/// Per-evaluation lookup wrapping `GraphLabelStore::addresses_with_label`.
/// Cached per-evaluation or per-batch per Decision 6 (§11).
///
/// Location: `crates/graph/src/smart_money_lookup.rs`
/// Imported by detectors via `mg_onchain_graph::smart_money_lookup::SmartMoneyLookup`.
#[async_trait]
pub trait SmartMoneyLookup: Send + Sync {
    /// Return the tier for an address, or `None` if the address has no live SmartMoney label.
    ///
    /// The implementation MUST filter expired labels.
    /// `min_confidence` = minimum label confidence to count (use config
    /// `smart_money_consumer_v1.min_label_confidence`, default 0.40 — below Tier3 base of 0.30
    /// is permissive; raise to 0.50 to exclude borderline Tier3).
    async fn tier_for(
        &self,
        chain: &str,
        address: &str,
    ) -> anyhow::Result<Option<SmartMoneyTier>>;

    /// Batch lookup for a set of addresses.
    ///
    /// Default implementation: sequential `tier_for` calls.
    /// Postgres implementation: single `WHERE address = ANY($1)` query.
    async fn tiers_for_batch(
        &self,
        chain: &str,
        addresses: &[String],
    ) -> anyhow::Result<Vec<(String, SmartMoneyTier)>> {
        let mut results = Vec::new();
        for addr in addresses {
            if let Some(tier) = self.tier_for(chain, addr).await? {
                results.push((addr.clone(), tier));
            }
        }
        Ok(results)
    }
}

/// Postgres-backed implementation of `SmartMoneyLookup`.
///
/// Issues a single `SELECT` per `tier_for` call. No in-memory cache at this layer —
/// the `address_labels` table is small (< 100K rows at steady state) and read-optimized.
/// Per-evaluation cache is owned by the caller if needed (see Decision 6).
pub struct PgSmartMoneyLookup {
    label_store: Arc<dyn GraphLabelStore>,
    min_confidence: f64,
}

impl PgSmartMoneyLookup {
    pub fn new(label_store: Arc<dyn GraphLabelStore>, min_confidence: f64) -> Self {
        Self { label_store, min_confidence }
    }
}

#[async_trait]
impl SmartMoneyLookup for PgSmartMoneyLookup {
    async fn tier_for(&self, chain: &str, address: &str) -> anyhow::Result<Option<SmartMoneyTier>> {
        let labels = self.label_store.get_labels(chain, address).await?;
        for label in labels {
            if label.label_type == LabelType::SmartMoney
                && label.confidence >= self.min_confidence
            {
                // Tier is in evidence["smart_money/tier"]
                if let Some(tier_str) = label.evidence.get("smart_money/tier").and_then(|v| v.as_str()) {
                    return Ok(match tier_str {
                        "tier1" => Some(SmartMoneyTier::Tier1),
                        "tier2" => Some(SmartMoneyTier::Tier2),
                        "tier3" => Some(SmartMoneyTier::Tier3),
                        _ => None,
                    });
                }
            }
        }
        Ok(None)
    }

    async fn tiers_for_batch(
        &self,
        chain: &str,
        addresses: &[String],
    ) -> anyhow::Result<Vec<(String, SmartMoneyTier)>> {
        // Efficient batch: fetch all SmartMoney labels for the chain, filter to addresses.
        // Uses existing `addresses_with_label` which already has an index on (chain, label_type).
        let all_labels = self.label_store
            .addresses_with_label(chain, LabelType::SmartMoney, self.min_confidence)
            .await?;
        let addr_set: std::collections::HashSet<&str> = addresses.iter().map(|s| s.as_str()).collect();
        let mut results = Vec::new();
        for label in all_labels {
            if addr_set.contains(label.address.as_str()) {
                if let Some(tier_str) = label.evidence.get("smart_money/tier").and_then(|v| v.as_str()) {
                    let tier = match tier_str {
                        "tier1" => SmartMoneyTier::Tier1,
                        "tier2" => SmartMoneyTier::Tier2,
                        "tier3" => SmartMoneyTier::Tier3,
                        _ => continue,
                    };
                    results.push((label.address, tier));
                }
            }
        }
        Ok(results)
    }
}
```

**Note on `tiers_for_batch` implementation:** `addresses_with_label` fetches ALL SmartMoney
labels for the chain (possibly thousands of rows) and filters in Rust. This is acceptable
because the label table is small (< 100K rows) and the query is indexed. A future optimization
is a `WHERE address = ANY($1)` Postgres query when the address set is large.

### §6.2 Detector struct changes

**D04 `PumpDumpDetector`:**

```rust
pub struct PumpDumpDetector {
    #[allow(dead_code)]
    thresholds: PumpDumpConfig,
    /// Smart-money lookup — None when not wired (backwards-compat, existing tests).
    /// Injected by production init/detectors.rs; None in unit tests.
    pub smart_money: Option<Arc<dyn SmartMoneyLookup>>,
}

impl PumpDumpDetector {
    pub fn new(thresholds: PumpDumpConfig) -> Self {
        Self { thresholds, smart_money: None }
    }

    pub fn with_smart_money(mut self, lookup: Arc<dyn SmartMoneyLookup>) -> Self {
        self.smart_money = Some(lookup);
        self
    }
}
```

Builder pattern (`with_smart_money`) avoids changing existing `new()` signatures, preserving
all existing call sites.

**D08 `D08SybilDetector`:**

```rust
pub struct D08SybilDetector {
    pub cluster_store: Arc<dyn ClusterStore>,
    pub label_store: Arc<dyn GraphLabelStore>,
    /// Smart-money lookup — None when not wired.
    pub smart_money: Option<Arc<dyn SmartMoneyLookup>>,
}

impl D08SybilDetector {
    pub fn new(cluster_store: Arc<dyn ClusterStore>, label_store: Arc<dyn GraphLabelStore>) -> Self {
        Self { cluster_store, label_store, smart_money: None }
    }

    pub fn with_smart_money(mut self, lookup: Arc<dyn SmartMoneyLookup>) -> Self {
        self.smart_money = Some(lookup);
        self
    }
}
```

**D05 `WashTradingDetector`:**

```rust
pub struct WashTradingDetector {
    pub thresholds: WashTradingConfig,
    /// Smart-money lookup — None when not wired (neutral metadata only; Decision 5).
    pub smart_money: Option<Arc<dyn SmartMoneyLookup>>,
}

impl WashTradingDetector {
    pub fn new(thresholds: WashTradingConfig) -> Self {
        Self { thresholds, smart_money: None }
    }

    pub fn with_smart_money(mut self, lookup: Arc<dyn SmartMoneyLookup>) -> Self {
        self.smart_money = Some(lookup);
        self
    }
}
```

### §6.3 Production wiring in `crates/server/src/init/detectors.rs`

```rust
// In build_all_detectors() or equivalent:
let sm_lookup: Arc<dyn SmartMoneyLookup> = Arc::new(PgSmartMoneyLookup::new(
    label_store.clone(),
    config.smart_money_consumer_v1.min_label_confidence,
));

let d04 = PumpDumpDetector::new(config.pump_dump.clone())
    .with_smart_money(sm_lookup.clone());

let d08 = D08SybilDetector::new(cluster_store.clone(), label_store.clone())
    .with_smart_money(sm_lookup.clone());

let d05 = WashTradingDetector::new(config.wash_trading_h1.clone())
    .with_smart_money(sm_lookup.clone());
```

All three detectors share the same `PgSmartMoneyLookup` instance (same `Arc`), so cache
state (if any) is shared across evaluations. Per Decision 6, the production implementation
uses no in-memory cache at this layer — the DB query is cheap enough.

### §6.4 MockSmartMoneyLookup for tests

```rust
/// Mock implementation for unit tests.
/// Configured with a fixed set of (address, tier) pairs.
pub struct MockSmartMoneyLookup {
    entries: std::collections::HashMap<String, SmartMoneyTier>,
}

impl MockSmartMoneyLookup {
    pub fn new(entries: impl IntoIterator<Item = (String, SmartMoneyTier)>) -> Self {
        Self { entries: entries.into_iter().collect() }
    }

    pub fn empty() -> Self {
        Self { entries: std::collections::HashMap::new() }
    }
}

#[async_trait]
impl SmartMoneyLookup for MockSmartMoneyLookup {
    async fn tier_for(&self, _chain: &str, address: &str) -> anyhow::Result<Option<SmartMoneyTier>> {
        Ok(self.entries.get(address).copied())
    }
}
```

Tests that need smart-money amplification: pass `MockSmartMoneyLookup::new([...])`.
Tests that do not need it (existing tests): pass `None` or `MockSmartMoneyLookup::empty()`.

---

## §7 Threshold Calibration

### §7.1 Per-detector delta summary table

| Detector | Tier1 delta | Tier2 delta | Tier3 delta | Min count | Cap |
|----------|-------------|-------------|-------------|-----------|-----|
| D04 P&D | +0.12 | +0.07 (need ≥ 2) | 0.00 | 1 Tier1 OR 2 Tier2 | 0.95 (A), 0.85 (B) |
| D08 Sybil | +0.08 | +0.05 | 0.00 | 1 Tier1 OR 1 Tier2 | 0.95 |
| D05 Wash | 0.00 | 0.00 | 0.00 | N/A | N/A |

### §7.2 Cap interaction examples

**D04, Signal A base = 0.65, Tier1 buyer + insider sell (Signal C):**
```
0.65 + 0.12 (smart-money) + 0.15 (Signal C) = 0.92 → min(0.92, 0.95) = 0.92 (High)
```

**D04, Signal B base = 0.75 (maximum), Tier1 buyer:**
```
0.75 + 0.12 = 0.87 → min(0.87, 0.85) = 0.85 (Signal B cap)
```
This correctly prevents Signal B + smart-money from exceeding the Signal B certainty ceiling.

**D08, overlap=0.60, cluster_conf=0.70 → conf_raw_b=0.544, Tier1 in cluster:**
```
0.544 + 0.08 = 0.624 → min(0.624, 0.95) = 0.624 (Medium-to-High boundary)
```

**D08, full overlap=1.0, cluster_conf=0.85 → conf_raw_b=0.74, Tier1:**
```
0.74 + 0.08 = 0.82 → min(0.82, 0.95) = 0.82 (High)
```

### §7.3 Derivation status per threshold

| Parameter | Value | Status | Citation |
|-----------|-------|--------|----------|
| `d04_smart_money_tier1_delta` | 0.12 | unverified-heuristic | Design derivation; Perseus 2025 behavioral anchor |
| `d04_smart_money_tier2_delta` | 0.07 | unverified-heuristic | Design derivation; Tier2 single-criterion lower confidence |
| `d04_smart_money_min_tier1_buyers` | 1 | unverified-heuristic | Conservative: any Tier1 in pre-pump window |
| `d04_smart_money_min_tier2_buyers` | 2 | unverified-heuristic | Two independent data points |
| `d04_pre_pump_window_minutes` | 60 | Fantazzini & Xiao 2023 | 60-min pre-event window |
| `d08_smart_money_tier1_delta` | 0.08 | unverified-heuristic | Design derivation; informed-coordinated-attacker |
| `d08_smart_money_tier2_delta` | 0.05 | unverified-heuristic | Design derivation |
| `min_label_confidence` | 0.40 | unverified-heuristic | Below Tier3 base 0.30 is permissive; 0.40 conservative |

All `unverified-heuristic` values require REFERENCES.md `unverified-heuristic` tag in code
comment and in config/detectors.toml. Sprint calibration plan: verify against positive
fixture corpus (D04 POS_01, POS_02, POS_03; D08 POS_D08_01) — see §12.

---

## §8 Evasion Analysis

### E-SMC-1: Address rotation between events (cross-event recurrence defeat)

**Attack:** A sophisticated actor uses wallet W1 for pump event 1, W2 for pump event 2, W3
for pump event 3. No single wallet reaches recurrence ≥ 3 (Tier1 threshold). Each wallet
appears exactly once in the Stage 3 pre-event window.

**Coverage:** Partial. The `SmartMoneyLabeller` produces no Tier1 label for any individual
wallet. D04 amplification is not triggered. D08 common-funder clustering MAY group W1/W2/W3
if they share a funder (the actor must seed three separate wallets from a common source).
Mitigation: D08 cluster aggregation of smart-money PnL is Phase 5 work (design 0022 §8
E-SM-1). At MVP, this evasion is acknowledged and documented.

**Cost of evasion:** 3× wallet management; 3× gas / funding transactions; common-funder trail
to D08. Non-trivial coordination cost.

### E-SMC-2: Tier-gaming via fake PnL (wash trading to earn smart-money label)

**Attack:** Actor uses two controlled wallets to wash-trade token T, inflating wallet A's
realized PnL. Wallet A earns Tier1/2 label. Actor then uses Wallet A to buy pre-pump on
token U. D04 amplification fires.

**Coverage:** Design 0022 §8 E-SM-2 establishes that wallets with active `wash_trading_v1`
anomaly events above confidence 0.70 are excluded from `SmartMoneyLabeller` computation.
D05 Signal A or B should fire on the wash-trade pair if trades meet Heuristic 1 thresholds.

**Residual gap:** If wash trades are executed below D05 thresholds (< 3 repetitions, or > 25
slots apart), D05 does not fire, and the exclusion gate in `SmartMoneyLabeller` is not
triggered. The attacker earns a label from below-threshold wash trades. This is noted as an
open gap; no mitigation at MVP.

### E-SMC-3: Label manipulation via known DEX programs

**Attack:** A known DEX program address (labeled `KnownDex`) accumulates positive PnL on
token T due to AMM price appreciation (LP value increases). The labeller may assign a
SmartMoney label to the DEX program address.

**Coverage:** Design 0022 §5.3 excludes `KnownDex`-labeled addresses from labelling. This
exclusion runs at corpus computation time.

### E-SMC-4: Timing gaming to avoid pre-pump window

**Attack:** An informed actor buys at `window_start - 61 minutes` (1 minute before the
60-minute pre-pump window opens). They are not counted in the pre-pump buyer set.

**Coverage:** None in MVP. The 60-minute window is anchored in Fantazzini & Xiao 2023. If
the actor is consistent (always buys exactly 61+ minutes before peak), they accumulate no
timing recurrence and may never reach Tier1. But if they are discovered once, the next
evasion round is simply shifting the window. Mitigation: widen `pre_pump_window_minutes` to
120 minutes (configurable) — but this increases false positives by including organic buyers
who entered 90+ minutes before the pump. Config-tunable; default remains 60.

---

## §9 Config Keys

All keys under `[smart_money_consumer_v1]` in `config/detectors.toml`. All monetary deltas
are `f64` probabilities, not amounts — exempt from the `rust_decimal` rule.

```toml
[smart_money_consumer_v1]
# ---- Global ----

# Minimum SmartMoney label confidence to count for amplification.
# Labels below this threshold are treated as absent.
# unverified-heuristic: 0.40 is below Tier3 base (0.30) with margin.
# Raise to 0.50 to exclude borderline Tier3 labels from amplification.
min_label_confidence = 0.40

# ---- D04 Pump & Dump amplification ----

# Confidence delta when >= 1 Tier1 smart-money wallet is in the pre-pump window.
# unverified-heuristic; Perseus 2025 (arXiv:2503.01686): masterminds buy pre-event in 100% of events.
# Design derivation: +0.12 moves threshold-confidence (0.60) to 0.72 (Medium→High boundary).
d04_tier1_delta = 0.12

# Confidence delta when >= 2 Tier2 wallets are in the pre-pump window (no Tier1 found).
# unverified-heuristic; two independent data points for Tier2.
d04_tier2_delta = 0.07

# Minimum Tier1 buyers in pre-pump window to trigger amplification.
# Conservative: 1 Tier1 wallet is sufficient given strong labelling criteria.
d04_min_tier1_buyers = 1

# Minimum Tier2 buyers in pre-pump window to trigger amplification.
# Two independent Tier2 wallets reduce single-event luck probability.
d04_min_tier2_buyers = 2

# Pre-pump window for buyer lookup (minutes).
# Fantazzini & Xiao 2023 (Econometrics 11(3)): 60-minute pre-announcement window.
# Referenced in REFERENCES.md (smart-money informed-early-buyer row).
d04_pre_pump_window_minutes = 60

# ---- D08 Sybil amplification ----

# Confidence delta when any Tier1 smart-money wallet is in the cluster.
# unverified-heuristic; informed-coordinated-attacker framing (Perseus 2025).
d08_tier1_delta = 0.08

# Confidence delta when any Tier2 smart-money wallet is in the cluster (no Tier1).
# unverified-heuristic.
d08_tier2_delta = 0.05

# ---- D05 Wash Trading (NEUTRAL — no confidence change) ----

# NOTE: D05 smart-money integration is metadata-only. These keys exist for documentation
# and consumer policy; they do not change confidence.
# See design 0023 §4.3 and Decision 5 in §11.

# Confirmed: smart-money amplification delta for D05 is always 0.00.
# d05_tier1_delta = 0.00   # implicit; not needed as config key

# ---- Feedback loop guard ----

# Labels issued within this many seconds of the evaluation window start are excluded.
# Prevents same-batch smart-money labels from amplifying D04 that just generated them.
# Default 0 = any label issued before window_start is eligible.
# Raise to 21600 (6h = one batch interval) for strict temporal separation.
feedback_guard_seconds = 21600
```

---

## §10 Cross-Detector Amplification Flow

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│  BACKGROUND TASK (6h batch — SmartMoneyLabeller)                                │
│                                                                                 │
│  swaps table + anomaly_events(D04) → WalletPnlCorpus + TimingFeatures          │
│  → address_labels WHERE label_type = 'SmartMoney' (Tier1/2/3)                  │
└─────────────────────┬───────────────────────────────────────────────────────────┘
                      │ labels read (non-expired, issued_at < window_start)
                      ▼
        SmartMoneyLookup (PgSmartMoneyLookup wrapping GraphLabelStore)
                      │
         ┌────────────┼────────────┐
         │            │            │
         ▼            ▼            ▼
    D04 P&D       D08 Sybil    D05 Wash
    (AMPLIFY)     (AMPLIFY)    (NEUTRAL)
         │            │            │
         │            │            └─► emit wash_trading_h1/smart_money_present
         │            │                (confidence unchanged)
         │            └─────────────► amplify conf_raw_b by +0.05 or +0.08
         │                            clamp to 0.95
         └─────────────────────────► amplify confidence_base by +0.07 or +0.12
                                      clamp to cap (0.95 / 0.85 per signal)
                      │
                      ▼
             AnomalyEvent (confidence_final, evidence with smart_money_* keys)
                      │
                      ▼
              scoring crate → consumer API
```

**Amplification is always additive and always bounded by the existing cap.** No new cap
values are introduced; existing caps (D04: 0.95 / 0.85, D08: 0.95, D05: 0.95 / 0.85) are
the binding constraints.

**Directionality:**
- D04: UP only. Smart-money presence increases P&D confidence.
- D08: UP only. Smart-money in cluster increases Sybil adversarial-intent confidence.
- D05: NEUTRAL. Evidence metadata only; scoring crate or consumer applies policy.

**Information boundary:** `SmartMoneyLookup` reads from `address_labels`; it NEVER writes.
It does not modify the label store. Labels are managed exclusively by `SmartMoneyLabeller`.

---

## §11 Decisions Requiring Sign-Off

### Decision 1: SmartMoneyLookup trait location

**Options:**
- A) New file `crates/graph/src/smart_money_lookup.rs` — extends graph crate (labels are graph-owned)
- B) New file `crates/detectors/src/smart_money_lookup.rs` — co-located with consumers
- C) Inline in each detector (no shared trait)

**Recommendation: A.**
Labels are graph-crate-owned (`address_labels` is managed by `crates/graph`). The lookup
trait logically belongs there — it wraps `GraphLabelStore::addresses_with_label`.
Detectors already depend on `mg_onchain_graph` (D08 imports `ClusterStore` from there). No
new dependency edge is created. Option B introduces a detectors-crate dependency on graph
that does not currently exist (D04 and D05 do not currently import `mg_onchain_graph`).
If Option B is chosen, D04 and D05 gain a new crate dep — acceptable but less clean.

**Trade-off:** Option A requires a minor re-export in `crates/graph/src/lib.rs`. Option B
avoids adding `mg_onchain_graph` to D04/D05's dependencies. The recommendation is A because
label-store APIs belong in the graph crate by design.

**Key precedent:** `ClusterStore` is in `crates/graph/src/api.rs` and imported by D08
detectors — the same pattern.

### Decision 2: Per-tier delta values for D04

**Options:**
- A) Tier1 = +0.12, Tier2 = +0.07 (recommended, §4.1)
- B) Tier1 = +0.15, Tier2 = +0.10 (match existing Signal C insider_amplifier magnitude)
- C) Single delta = +0.10 for any tier (simpler, less granular)

**Recommendation: A.**
Option B (matching Signal C) would produce D04 = base + 0.30 when both insider sell AND
smart-money fire simultaneously — too aggressive for a heuristic label not FDR-controlled.
Option C loses the tier information signal (Tier1 = mastermind proxy is meaningfully
stronger than Tier2). Option A calibrates so that Tier1 alone moves confidence one severity
band (~Medium to High boundary at threshold base), which is the correct relative amplification.

**Perseus 2025 calibration note:** Perseus reports 100% pre-event buying rate among
identified masterminds. This would justify a larger delta, but our labels are heuristic (not
FDR-controlled), so a conservative +0.12 is appropriate. When Stage 2 FDR labels mature,
the Tier1 delta can be revisited.

### Decision 3: D04 buyer-set semantics

**Options:**
- A) Count smart-money wallets that bought ANY time before evaluation window end (broad)
- B) Count smart-money wallets that bought in the last `pre_pump_window_minutes` (60 min) ← **RECOMMENDED**
- C) Count wallets in the Karbalaii-defined accumulation phase (requires D04 to detect phase boundaries)

**Recommendation: B.**
Fantazzini & Xiao 2023 define the informed-early-buyer window as 60 minutes pre-event. Option A
(all pre-window buying) is too broad — any smart-money wallet that ever traded the token would
trigger amplification, regardless of proximity to the pump. Option C requires D04 to produce
accumulation phase estimates (DG-04-1, currently Phase 5 backlog). Option B is implementable
now using the existing `swaps` table with a time-bounded query.

**Min-count threshold:** 1 Tier1 OR 2 Tier2 wallets in the 60-minute pre-pump window.
The "2 Tier2" minimum avoids single-wallet noise for the weaker label tier.

### Decision 4: D08 amplification direction

**Recommendation: UPWARD (increased confidence when smart-money is in cluster).**

The common mis-intuition is that smart money "vouches" for legitimacy. The domain framing
(§1.2) reverses this for the shitcoin universe. A Tier1 wallet — defined by crossing the
informed-early-buyer threshold across ≥ 3 pump events — operating inside a Sybil cluster
is evidence of sophisticated adversarial coordination, not a benign skilled trader who
happened to receive an airdrop.

This decision has the highest conceptual controversy. The sign-off specifically acknowledges
the counter-argument: "what if a legitimate whale who is also smart money legitimately holds
a token that happens to have a Sybil cluster in the holder set?" Response: D08 only fires on
clusters with ≥ 30% top-holder overlap AND ≥ 3 cluster members. A smart-money wallet that is
simply a holder is NOT a cluster member (clusters are defined by common-funder relationship).
For a smart-money wallet to be in a Sybil cluster AND triggering D08, it must be funded by
the same source as other cluster members. This is the key structural check that makes the
upward amplification defensible.

### Decision 5: D05 smart-money treatment

**Options:**
- A) AMPLIFY confidence (informed actors gaming volume metrics)
- B) DOWNGRADE confidence (legitimate MMs are smart but not fraudulent)
- C) NEUTRAL — emit metadata evidence only, no confidence change ← **RECOMMENDED**

**Recommendation: C.**
The label does not disambiguate market-making (legitimate) from volume-gaming (adversarial).
A consumer that cares (e.g., the trading bot) can read `wash_trading_h1/smart_money_present`
from the evidence and apply its own policy. Option A (amplify) risks producing high-confidence
wash-trading events on known market makers. Option B (downgrade) risks suppressing real wash
traders who happen to have good PnL. Option C is the safe, falsifiable default.

**Precedent:** D04 Signal C is suppressed for established protocols — not because D04 is
wrong, but because the asymmetric case (treasury sell ≠ rug) is domain knowledge not
expressible by the signal alone. Same reasoning applies here: smart-money wash = ambiguous,
emit metadata and let consumers decide.

### Decision 6: Caching strategy for SmartMoneyLookup

**Options:**
- A) No cache — per-address DB query via `get_labels` (simple, always fresh)
- B) Per-evaluation HashMap cache — load all SmartMoney labels for chain at start of evaluation,
     keep in-memory for duration of one `evaluate()` call ← **RECOMMENDED**
- C) Cross-evaluation LRU cache with 5-minute TTL (mirror TokenPriceProvider S21 pattern)

**Recommendation: B.**
Option A issues one DB query per address per evaluation. For D04, the pre-pump buyer set
may be 10-100 wallets → 10-100 sequential `get_labels` queries per D04 evaluation. At
30K evaluations/day this is 300K-3M queries/day on a table that changes only every 6 hours.

Option B uses `addresses_with_label(chain, SmartMoney, min_confidence)` ONCE per evaluation
to fetch all SmartMoney labels for the chain into a `HashMap<String, SmartMoneyTier>`, then
performs in-memory lookups. The `address_labels` table at steady state has < 100K SmartMoney
rows; the full table fetch is a single indexed query. Per-evaluation scope means the cache is
automatically invalidated between evaluations — no TTL management needed.

Option C (cross-evaluation LRU) is premature optimization. The 6h label batch interval means
cross-evaluation cache would serve stale labels for up to 6 hours — identical freshness to
the per-evaluation load. The per-evaluation approach B is simpler and correct.

**Implementation in `PgSmartMoneyLookup::tiers_for_batch`:** The batch lookup method in §6.1
already uses `addresses_with_label` to fetch all chain labels and filter to the address set.
Callers (D04, D08, D05 evaluate methods) call `tiers_for_batch` with their relevant address
set, receiving `Vec<(String, SmartMoneyTier)>` in one round-trip.

### Decision 7: Evidence key shape

**Recommendation: standardized per-detector keys as specified in §4.1, §4.2, §4.3.**

All evidence keys follow the existing gotcha #9 prefix convention: `{detector_id}/smart_money_*`.

Standard keys across all three detectors:
```
{detector_id}/smart_money_present               — Decimal(0 or 1) — bool proxy
{detector_id}/smart_money_tier1_count           — Decimal(int) — addresses at Tier1
{detector_id}/smart_money_tier2_count           — Decimal(int) — addresses at Tier2
{detector_id}/smart_money_tier3_count           — Decimal(int) — addresses at Tier3
{detector_id}/smart_money_amplification_delta   — Decimal — actual delta applied (0.00 for D05)
```

D04-specific additional key:
```
pump_dump/smart_money_pre_pump_window_minutes   — Decimal — window config used
```

These keys are added to the existing evidence ONLY when `smart_money_lookup` is `Some`.
When `None`, the keys are absent (backwards-compatible with existing consumers).

### Decision 8: Backwards compatibility

**Recommendation: `Option<Arc<dyn SmartMoneyLookup>>` field, `None` default, builder method.**

This is the cleanest backwards-compat approach:
- All existing `PumpDumpDetector::new(thresholds)` call sites continue to compile unchanged.
- `with_smart_money(lookup)` builder is the opt-in injection point.
- Existing unit tests pass `None` implicitly (default field value in new struct).
- Production wiring is the ONLY place that injects `Some(...)`.

No existing test needs to change. The `#[allow(dead_code)]` annotation on `thresholds` in
D04 already shows the pattern of fields that are unused in some configurations.

---

## §12 Test Plan

### §12.1 Unit tests — pure math (no I/O)

**D04 smart-money amplification math:**

```rust
// test: Tier1 buyer in pre-pump window → delta = +0.12
fn d04_tier1_amplification_delta() {
    let delta = compute_smart_money_delta_d04(tier1_count=1, tier2_count=0, cfg);
    assert_eq!(delta, 0.12);
}

// test: Tier2 below min count (1 of 2 required) → delta = 0.00
fn d04_tier2_insufficient_count_no_delta() {
    let delta = compute_smart_money_delta_d04(tier1_count=0, tier2_count=1, cfg);
    assert_eq!(delta, 0.00);
}

// test: Tier2 minimum count met → delta = +0.07
fn d04_tier2_min_count_delta() {
    let delta = compute_smart_money_delta_d04(tier1_count=0, tier2_count=2, cfg);
    assert_eq!(delta, 0.07);
}

// test: Tier3 only → delta = 0.00
fn d04_tier3_no_delta() {
    let delta = compute_smart_money_delta_d04(tier1_count=0, tier2_count=0, cfg);
    assert_eq!(delta, 0.00);
}

// test: cap enforced — Signal A base=0.88 + Tier1 delta + Signal C = 1.03 → clamped to 0.95
fn d04_cap_enforced_at_0_95() {
    let final_conf = apply_amplification(base=0.88, delta=0.12, signal_c=0.15, cap=0.95);
    assert_eq!(final_conf, 0.95);
}

// test: Signal B cap = 0.85 enforced with Tier1
fn d04_signal_b_cap_0_85() {
    let final_conf = apply_amplification(base=0.80, delta=0.12, signal_c=0.00, cap=0.85);
    assert_eq!(final_conf, 0.85);
}
```

**D08 smart-money amplification math:**

```rust
// test: Tier1 in cluster → delta = +0.08; full overlap=1.0 conf=0.74 → 0.82
fn d08_tier1_amplification() {
    let base = compute_sybil_confidence(1.0, 0.85);  // existing fn = 0.74
    let delta = compute_smart_money_delta_d08(tier1_found=true, tier2_found=false, cfg);
    assert_eq!(delta, 0.08);
    let final_conf = (base + delta).min(0.95);
    assert!((final_conf - 0.82).abs() < 1e-9);
}

// test: Tier2 in cluster, no Tier1 → delta = +0.05
fn d08_tier2_amplification_no_tier1() {
    let delta = compute_smart_money_delta_d08(tier1_found=false, tier2_found=true, cfg);
    assert_eq!(delta, 0.05);
}

// test: no smart money → delta = 0.00 (no change from existing behavior)
fn d08_no_smart_money_no_delta() {
    let delta = compute_smart_money_delta_d08(tier1_found=false, tier2_found=false, cfg);
    assert_eq!(delta, 0.00);
}
```

**D05 neutral (math is trivial — delta always 0.00):**

```rust
fn d05_smart_money_delta_always_zero() {
    // All tier combinations produce delta = 0.00 for D05
    for (t1, t2) in [(1, 0), (0, 2), (5, 5), (0, 0)] {
        let delta = compute_smart_money_delta_d05(t1, t2);
        assert_eq!(delta, 0.00);
    }
}
```

### §12.2 Unit tests — MockSmartMoneyLookup injection

**D04 with Tier1 mock:**

```rust
#[tokio::test]
async fn d04_smart_money_amplification_with_mock() {
    // Positive fixture: mock returns Tier1 for one of the pre-pump buyers.
    // Expected: confidence_final = confidence_base + 0.12 (clamped to 0.95)
    let lookup = MockSmartMoneyLookup::new([
        ("wallet_abc".to_string(), SmartMoneyTier::Tier1),
    ]);
    let detector = PumpDumpDetector::new(config)
        .with_smart_money(Arc::new(lookup));
    // ... inject mock context with a pre-pump buyer "wallet_abc"
    // ... assert event.confidence = base + 0.12
    // ... assert evidence contains pump_dump/smart_money_tier1_buyer_count = 1
    // ... assert evidence contains pump_dump/smart_money_amplification_delta = 0.12
}
```

**D04 without smart_money (backwards compat):**

```rust
#[tokio::test]
async fn d04_no_smart_money_field_no_change() {
    let detector = PumpDumpDetector::new(config); // smart_money = None by default
    // ... inject same mock context
    // ... assert confidence_final == confidence_base (no change)
    // ... assert evidence does NOT contain pump_dump/smart_money_present key
}
```

**D08 Tier1 in cluster:**

```rust
#[tokio::test]
async fn d08_tier1_in_cluster_amplifies() {
    let lookup = MockSmartMoneyLookup::new([
        ("cluster_member_1".to_string(), SmartMoneyTier::Tier1),
    ]);
    let detector = D08SybilDetector::new(cluster_store, label_store)
        .with_smart_money(Arc::new(lookup));
    // ... inject context where cluster contains "cluster_member_1"
    // ... assert confidence_final = min(conf_raw_b + 0.08, 0.95)
}
```

**D05 neutral — confidence unchanged, evidence present:**

```rust
#[tokio::test]
async fn d05_smart_money_metadata_no_confidence_change() {
    let lookup = MockSmartMoneyLookup::new([
        ("wash_wallet_a".to_string(), SmartMoneyTier::Tier1),
    ]);
    let detector = WashTradingDetector::new(config)
        .with_smart_money(Arc::new(lookup));
    // ... inject context where round-trip involves "wash_wallet_a"
    // ... assert confidence_final == confidence_base (unchanged)
    // ... assert evidence contains wash_trading_h1/smart_money_present = 1
    // ... assert evidence contains wash_trading_h1/smart_money_amplification_delta = 0.00
}
```

### §12.3 Calibration tests — positive fixture corpus

D04 positive fixtures POS_01, POS_02, POS_03 (existing) must continue to pass at equal or
higher confidence when smart-money mock returns Tier1 for at least one pre-pump wallet.

D08 positive fixture POS_D08_01 (existing) must fire at confidence ≥ (original + 0.08) when
Tier1 smart-money mock injected for one cluster member.

**Negative fixtures must not change:**
- NEG_D04_01, NEG_D04_02: MockSmartMoneyLookup::empty() → confidence unchanged from pre-S23.
- NEG_D08_01 (no cluster): no amplification possible (cluster_size = 0 → early return before
  smart-money lookup).
- NEG_D05_01: evidence keys present if lookup is Some; confidence unchanged.

### §12.4 Feedback loop guard test

```rust
fn test_feedback_loop_guard_excludes_recent_labels() {
    // Label issued_at = ctx.window.block_start (same instant) — EXCLUDED
    // Label issued_at = ctx.window.block_start - 1s — INCLUDED
    // Verify that the feedback_guard_seconds config (21600) correctly filters
    // labels issued within the guard window.
}
```

---

## §13 References

All citations backed by REFERENCES.md entries:

- Fu, Feng, Wu & Xu 2025 (Perseus, arXiv:2503.01686) — primary domain anchor for D04
  amplification direction and D08 amplification direction. Mastermind wallets buy pre-event
  in 100% of confirmed pump events. 438 masterminds, $3.2T artificial trading.
- Fantazzini & Xiao 2023 (Econometrics 11(3)) — 60-minute pre-event window for D04 buyer
  set definition. `d04_pre_pump_window_minutes = 60`.
- Barras, Scaillet & Wermers 2010 (JoF 65(1)) — Tier 1/2/3 criteria derivation (design 0022).
  Tier1 = strongest alpha candidate = largest amplification delta.
- Chainalysis 2025 — D08 common-funder Heuristic 2; "94% of rugged tokens had deployer as
  primary holder controller." Supports upward amplification when smart-money = cluster member.
- Liu et al. 2025 (arXiv:2505.09313) — D08 LightGBM; "fraction of cluster holding token"
  is top-5 feature. Cluster + smart-money co-presence is high-importance feature compound.
- Victor & Weintraud 2021 (arXiv:2102.07001) — D05 wash trading canonical methodology.
  Informed MMs execute ring trades as legitimate market-making — supports D05 NEUTRAL decision.
- Design 0022 (Sprint 22) — smart-money tier thresholds; label schema; pipeline architecture.
- Design 0007 (D04) — existing confidence formula + caps + Signal C insider amplifier.
- Design 0015 (D08) — existing Signal A+B confidence formula + cap = 0.95.
- Design 0008, 0017 (D05) — existing Signal A cap = 0.95, Signal B cap = 0.85.
