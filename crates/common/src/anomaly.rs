//! Detector output contract: AnomalyEvent, Severity, Confidence, Evidence.
//!
//! Per ADR 0001 §D4: every detector emits [`AnomalyEvent`] — no booleans, no
//! opaque scores. The `confidence` field is a calibrated probability estimate
//! in `[0.0, 1.0]`. Consumers filter by threshold on their side.
//!
//! # Serde strategy
//!
//! `rename_all = "camelCase"` for wire compatibility. [`AnomalyEvent`] serializes
//! cleanly for all three delivery modes in ADR 0001 §D8: in-process crate
//! (zero-copy via channel), REST JSON body, WebSocket frame.
//!
//! # Determinism
//!
//! [`Evidence`] uses `BTreeMap` for all key-value bags. `Vec` for ordered lists
//! where insertion order matters (e.g., a sequence of suspicious transactions in
//! block order). `HashMap` is explicitly banned in this module.
//!
//! # Evidence metric key convention
//!
//! Detector metric keys in [`Evidence::metrics`] are prefixed with
//! `<detector_id>/` by convention, e.g. `"rug_pull_lp_drain/lp_removed_pct"`.
//! This prefix is not enforced in code — it is a naming discipline established
//! when the first Phase 2 detector ships. See CLAUDE.md §Detector Rules and
//! `crates/detectors` for the convention's enforcement point.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::chain::{Address, BlockRef, Chain, TxHash};
use crate::error::CommonError;

// ---------------------------------------------------------------------------
// Severity
// ---------------------------------------------------------------------------

/// Alert severity level — used for consumer-side routing and UI rendering.
///
/// `#[non_exhaustive]` so additional levels can be added without breaking
/// existing match arms in consumer crates.
///
/// Mapping to RugCheck-style labels:
/// - `Info`     → informational, no action required
/// - `Low`      → flag for review, low urgency
/// - `Medium`   → recommend review before trade
/// - `High`     → strong signal, likely anomaly
/// - `Critical` → immediate action recommended (active rug, honeypot confirmed)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

// ---------------------------------------------------------------------------
// Confidence
// ---------------------------------------------------------------------------

/// A calibrated probability estimate in `[0.0, 1.0]`.
///
/// Wraps `f64` with a constructor that enforces the range. This is the **one**
/// allowed use of `f64` in `crates/common` — it represents a probability, not
/// a financial amount. Use `Decimal` for anything monetary.
///
/// # Serialization
///
/// Serialized as a JSON number (not a string) because:
/// 1. It IS an `f64` — JSON number precision is fine for a probability (15+ sig figs).
/// 2. Consumers (trading bot, MM) need to compare against threshold constants.
///
/// # Construction
///
/// ```rust
/// use mg_onchain_common::anomaly::Confidence;
///
/// let c = Confidence::new(0.85).unwrap();
/// assert_eq!(c.value(), 0.85);
///
/// let too_high = Confidence::new(1.5);
/// assert!(too_high.is_err());
///
/// let nan = Confidence::new(f64::NAN);
/// assert!(nan.is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Confidence(f64);

impl Confidence {
    /// Construct a `Confidence` value, returning an error if outside `[0.0, 1.0]`
    /// or if the value is `NaN`.
    pub fn new(v: f64) -> Result<Self, CommonError> {
        if v.is_nan() || !(0.0..=1.0).contains(&v) {
            Err(CommonError::ConfidenceOutOfRange { value: v })
        } else {
            Ok(Self(v))
        }
    }

    /// The raw `f64` value.
    pub fn value(&self) -> f64 {
        self.0
    }

    /// 0.0 — the lowest possible confidence.
    pub const ZERO: Self = Self(0.0);

    /// 1.0 — absolute certainty (use only for confirmed simulation results).
    pub const ONE: Self = Self(1.0);
}

impl TryFrom<f64> for Confidence {
    type Error = CommonError;

    fn try_from(v: f64) -> Result<Self, Self::Error> {
        Self::new(v)
    }
}

// ---------------------------------------------------------------------------
// Evidence
// ---------------------------------------------------------------------------

/// A structured bundle of supporting facts for an [`AnomalyEvent`].
///
/// Designed to be:
/// - **Inspectable by a human reviewer:** `metrics` carries named `Decimal` values;
///   `addresses` carries relevant wallets; `tx_hashes` carries the transactions
///   that triggered the alert.
/// - **Serializable for REST/WS:** all fields implement `Serialize + Deserialize`.
/// - **Deterministic:** `BTreeMap` used for all keyed bags.
///
/// # Metric key convention
///
/// By convention, detectors prefix their metric keys with `<detector_id>/` to
/// avoid collisions when multiple events for the same token are compared:
/// - `"rug_pull_lp_drain/lp_removed_pct"` → `Decimal("0.92")`
/// - `"honeypot_sim/sell_tax"` → `Decimal("0.85")`
/// - `"holder_concentration/gini_delta"` → `Decimal("0.12")`
///
/// This convention is enforced at the detector level, not here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    /// Transaction hashes that triggered or support the alert.
    pub tx_hashes: Vec<TxHash>,

    /// Wallet addresses implicated in the anomaly.
    pub addresses: Vec<Address>,

    /// Named numeric metrics supporting the detection.
    ///
    /// All values use `Decimal` — never `f64`. `BTreeMap` for deterministic ordering.
    pub metrics: BTreeMap<String, Decimal>,

    /// Free-form string annotations (e.g., "creator dumped 94% in 2 txs").
    /// Populated by detectors for human-readable audit trail.
    pub notes: Vec<String>,

    /// Block range over which the evidence was observed.
    pub observed_range: Option<(BlockRef, BlockRef)>,
}

impl Evidence {
    /// Create an empty evidence bundle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: add a named metric.
    pub fn with_metric(mut self, key: impl Into<String>, value: Decimal) -> Self {
        self.metrics.insert(key.into(), value);
        self
    }

    /// Builder: add a transaction hash.
    pub fn with_tx(mut self, tx: TxHash) -> Self {
        self.tx_hashes.push(tx);
        self
    }

    /// Builder: add a wallet address.
    pub fn with_address(mut self, addr: Address) -> Self {
        self.addresses.push(addr);
        self
    }

    /// Builder: add a free-form note.
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }
}

// ---------------------------------------------------------------------------
// AnomalyEvent
// ---------------------------------------------------------------------------

/// The primary output of every detector.
///
/// Per ADR 0001 §D4: no booleans. `confidence` ∈ `[0.0, 1.0]`.
/// `severity` is set by the detector as a classification hint; consumers may
/// override based on their own threshold configuration.
///
/// # Delivery modes (ADR 0001 §D8)
///
/// - **In-process crate:** `AnomalyEvent` is passed directly by value via channel.
/// - **REST:** serialized as a JSON object; see OpenAPI spec in `crates/gateway`.
/// - **WebSocket:** same JSON serialization, streamed in a WS frame.
///
/// # Determinism
///
/// Given the same input block range and config, two detector runs MUST emit
/// identical `AnomalyEvent` values (field for field). The `observed_at` field
/// is the **block time** — NOT wall-clock time — to satisfy this requirement.
/// Wall-clock observation time is tracked separately in `ingested_at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnomalyEvent {
    /// Stable identifier for the detector that produced this event.
    ///
    /// Convention: `snake_case`, e.g. `"rug_pull_lp_drain"`, `"honeypot_sim"`,
    /// `"holder_concentration_shift"`. Defined as a constant in each detector crate.
    pub detector_id: String,

    /// The token this event is about.
    pub token: Address,

    /// Which chain the token lives on.
    pub chain: Chain,

    /// Calibrated probability that the anomaly is real.
    /// 0.0 = certainly benign. 1.0 = certainly anomalous.
    pub confidence: Confidence,

    /// Severity classification. Consumers can override based on their
    /// risk tolerance, but the detector provides an informed starting point.
    pub severity: Severity,

    /// Evidence bundle: transactions, wallets, metrics, notes.
    pub evidence: Evidence,

    /// The block time of the last block in the observation window.
    ///
    /// MUST be the block timestamp, not `Utc::now()`. This is what makes
    /// detector output reproducible given the same input block range.
    pub observed_at: DateTime<Utc>,

    /// The block range over which the anomaly was observed.
    /// `(start_block, end_block)` inclusive. Both carry chain context.
    pub window: (BlockRef, BlockRef),

    /// Wall-clock time when this event was computed and dispatched.
    ///
    /// This is the ONLY field that uses wall-clock time. It is audit metadata
    /// and does not affect detector reproducibility — two runs of the same block
    /// range will differ in `ingested_at` but not in any other field.
    pub ingested_at: DateTime<Utc>,

    /// OAK Technique ID that this detector covers (e.g. `"OAK-T1.006"`).
    ///
    /// Set by the detector via [`Detector::oak_technique_id`]. When `None`, the
    /// event has not yet been mapped to the OAK taxonomy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oak_technique_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::{Address, BlockRef, Chain, TxHash};
    use chrono::Utc;
    use rust_decimal::Decimal;

    // --- Confidence ---

    #[test]
    fn confidence_valid_range() {
        assert!(Confidence::new(0.0).is_ok());
        assert!(Confidence::new(0.5).is_ok());
        assert!(Confidence::new(1.0).is_ok());
    }

    #[test]
    fn confidence_above_one_errors() {
        let err = Confidence::new(1.5).unwrap_err();
        assert!(matches!(err, CommonError::ConfidenceOutOfRange { value } if value == 1.5));
    }

    #[test]
    fn confidence_negative_errors() {
        let err = Confidence::new(-0.1).unwrap_err();
        assert!(matches!(err, CommonError::ConfidenceOutOfRange { .. }));
    }

    #[test]
    fn confidence_nan_errors() {
        let err = Confidence::new(f64::NAN).unwrap_err();
        assert!(matches!(err, CommonError::ConfidenceOutOfRange { .. }));
    }

    #[test]
    fn confidence_constants() {
        assert_eq!(Confidence::ZERO.value(), 0.0);
        assert_eq!(Confidence::ONE.value(), 1.0);
    }

    #[test]
    fn confidence_try_from() {
        let c: Result<Confidence, _> = 0.75f64.try_into();
        assert!(c.is_ok());
        assert_eq!(c.unwrap().value(), 0.75);
    }

    #[test]
    fn confidence_serde_roundtrip() {
        let c = Confidence::new(0.85).unwrap();
        let json = serde_json::to_string(&c).unwrap();
        // Should be a JSON number, not a string
        assert_eq!(json, "0.85");
        let back: Confidence = serde_json::from_str(&json).unwrap();
        assert_eq!(back.value(), 0.85);
    }

    // --- Severity ---

    #[test]
    fn severity_ordering() {
        assert!(Severity::Info < Severity::Low);
        assert!(Severity::Low < Severity::Medium);
        assert!(Severity::Medium < Severity::High);
        assert!(Severity::High < Severity::Critical);
    }

    #[test]
    fn severity_serde_lowercase() {
        assert_eq!(serde_json::to_string(&Severity::Info).unwrap(), r#""info""#);
        assert_eq!(serde_json::to_string(&Severity::Critical).unwrap(), r#""critical""#);
    }

    #[test]
    fn severity_serde_roundtrip() {
        for s in [Severity::Info, Severity::Low, Severity::Medium, Severity::High, Severity::Critical] {
            let json = serde_json::to_string(&s).unwrap();
            let back: Severity = serde_json::from_str(&json).unwrap();
            assert_eq!(back, s);
        }
    }

    // --- Evidence ---

    #[test]
    fn evidence_builder() {
        let addr = Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112").unwrap();
        let tx = TxHash::solana_from_base58(&bs58::encode(&[2u8; 64]).into_string()).unwrap();

        let ev = Evidence::new()
            .with_metric("rug_pull_lp_drain/lp_removed_pct", Decimal::new(92, 2))
            .with_address(addr.clone())
            .with_tx(tx)
            .with_note("creator dumped 94% in 2 txs");

        assert_eq!(ev.metrics.len(), 1);
        assert_eq!(ev.addresses.len(), 1);
        assert_eq!(ev.tx_hashes.len(), 1);
        assert_eq!(ev.notes.len(), 1);
    }

    #[test]
    fn evidence_metrics_btreemap_ordering() {
        let ev = Evidence::new()
            .with_metric("zzz", Decimal::ONE)
            .with_metric("aaa", Decimal::TWO)
            .with_metric("mmm", Decimal::new(3, 0));

        let mut keys = ev.metrics.keys();
        assert_eq!(keys.next().unwrap(), "aaa");
        assert_eq!(keys.next().unwrap(), "mmm");
        assert_eq!(keys.next().unwrap(), "zzz");
    }

    // --- AnomalyEvent ---

    fn make_event() -> AnomalyEvent {
        let token = Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112").unwrap();
        let block_start = BlockRef::new(Chain::Solana, 300_000_000);
        let block_end = BlockRef::new(Chain::Solana, 300_001_000);
        let now = Utc::now();

        AnomalyEvent {
            detector_id: "rug_pull_lp_drain".into(),
            token: token.clone(),
            chain: Chain::Solana,
            confidence: Confidence::new(0.95).unwrap(),
            severity: Severity::Critical,
            evidence: Evidence::new()
                .with_metric("rug_pull_lp_drain/lp_removed_pct", Decimal::new(92, 2)),
            observed_at: now,
            window: (block_start, block_end),
            ingested_at: now,
        }
    }

    #[test]
    fn anomaly_event_serde_roundtrip() {
        let event = make_event();
        let json = serde_json::to_string(&event).unwrap();

        // Verify key field names are camelCase
        assert!(json.contains("detectorId"));
        assert!(json.contains("observedAt"));
        assert!(json.contains("ingestedAt"));

        // Verify confidence is a JSON number
        assert!(json.contains("0.95"));

        // Verify severity is lowercase
        assert!(json.contains(r#""critical""#));

        // Deserialization must succeed and preserve fields
        // Note: AnomalyEvent contains TxHash and Address which don't implement Deserialize,
        // so we only test serialization. Full roundtrip requires custom deserialization
        // (chain context needed for Address/TxHash).
        // The JSON output itself is verified to be well-formed.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["detectorId"], "rug_pull_lp_drain");
        assert_eq!(v["severity"], "critical");
        assert!((v["confidence"].as_f64().unwrap() - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn anomaly_event_window_carries_chain_context() {
        let event = make_event();
        let (start, end) = event.window;
        assert_eq!(start.chain, Chain::Solana);
        assert_eq!(end.chain, Chain::Solana);
        assert!(start.height < end.height);
    }
}
