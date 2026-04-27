//! `SmartMoneyLookup` trait — read-only view into the smart-money label table.
//!
//! # Purpose
//!
//! Provides a per-evaluation batch fetch of all `LabelType::SmartMoney` addresses
//! for a given chain, keyed by address with the wallet's `SmartMoneyTier`.
//! Used by D04 (P&D amplification), D08 (Sybil amplification), and D05 (neutral
//! metadata) as an optional `Option<Arc<dyn SmartMoneyLookup>>` injection.
//!
//! # Backwards-compat
//!
//! All detectors default to `smart_money: None`. Existing tests are unaffected.
//! Production wiring (S23) injects `GraphSmartMoneyLookup::new(label_store)`.
//!
//! # Design reference
//!
//! `docs/designs/0023-smart-money-consumer-integration.md` §6.1 + Decision 6.
//!
//! # Citations
//!
//! - Fu, Feng, Wu & Xu 2025 (Perseus, arXiv:2503.01686) — D04/D08 amplification rationale.
//! - Fantazzini & Xiao 2023 (Econometrics 11(3)) — pre-event buyer window anchor.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::labels::{AddressLabel, GraphLabelStore, LabelType};
use crate::smart_money::SmartMoneyTier;

// ---------------------------------------------------------------------------
// SmartMoneyLookupError
// ---------------------------------------------------------------------------

/// Errors produced by [`SmartMoneyLookup`] implementations.
#[derive(Debug, Error)]
pub enum SmartMoneyLookupError {
    /// Error querying the label store.
    #[error("label store query failed: {0}")]
    LabelStore(String),
    /// Error parsing a tier value from label evidence JSON.
    #[error("tier parse failed: {0}")]
    Parse(String),
}

// ---------------------------------------------------------------------------
// SmartMoneyLookup trait
// ---------------------------------------------------------------------------

/// Read-only view into the smart-money label table.
///
/// Per-evaluation batch fetch wrapping `GraphLabelStore::addresses_with_label`.
/// Expired labels are filtered by the underlying `GraphLabelStore` query.
///
/// # Feedback loop guard
///
/// Callers are responsible for filtering labels where
/// `issued_at >= ctx.window.block_start` (labels issued during or after the
/// current evaluation window). See design 0023 §3.3.
///
/// # Object safety
///
/// `Send + Sync` is required for use across `tokio::spawn` task boundaries
/// (gotcha #27). The `#[async_trait]` macro ensures compatibility.
#[async_trait]
pub trait SmartMoneyLookup: Send + Sync {
    /// Batch fetch all SmartMoney-labelled addresses for a chain.
    ///
    /// Returns a `HashMap<address_string, SmartMoneyTier>` for the given chain.
    /// Only non-expired labels with confidence >= `min_label_confidence` (config
    /// `smart_money_consumer_v1.min_label_confidence`, default 0.40) are included.
    ///
    /// When the same address has multiple labels (unlikely, but possible if the
    /// batch ran multiple times), the highest-confidence label's tier wins.
    ///
    /// The `observed_at` parameter enables label-staleness guards in the
    /// implementation (labels issued after `observed_at` are excluded).
    async fn fetch_smart_money_addresses(
        &self,
        chain: &str,
        observed_at: DateTime<Utc>,
    ) -> Result<HashMap<String, SmartMoneyTier>, SmartMoneyLookupError>;
}

// ---------------------------------------------------------------------------
// GraphSmartMoneyLookup — production implementation
// ---------------------------------------------------------------------------

/// Postgres-backed implementation of [`SmartMoneyLookup`].
///
/// Issues a single `SELECT` via `GraphLabelStore::addresses_with_label` per
/// `fetch_smart_money_addresses` call. No in-memory cache at this layer —
/// the label table is small (< 100K rows at steady state) and already indexed
/// on `(chain, label_type)`.
///
/// Per Decision 6 (design 0023 §11): per-evaluation scope means the cache is
/// automatically invalidated between evaluations — no TTL management needed.
pub struct GraphSmartMoneyLookup {
    label_store: Arc<dyn GraphLabelStore>,
    /// Minimum label confidence to count. Labels below this are treated as absent.
    /// Config: `smart_money_consumer_v1.min_label_confidence` (default 0.40).
    /// unverified-heuristic; see design 0023 §9.
    min_confidence: f64,
}

impl GraphSmartMoneyLookup {
    /// Construct a new lookup wrapping the given label store.
    ///
    /// `min_confidence` is the minimum label confidence to count — labels below
    /// this floor are excluded from the returned map. Default in production:
    /// `config.smart_money_consumer_v1.min_label_confidence` (0.40).
    pub fn new(label_store: Arc<dyn GraphLabelStore>, min_confidence: f64) -> Self {
        Self {
            label_store,
            min_confidence,
        }
    }
}

#[async_trait]
impl SmartMoneyLookup for GraphSmartMoneyLookup {
    async fn fetch_smart_money_addresses(
        &self,
        chain: &str,
        observed_at: DateTime<Utc>,
    ) -> Result<HashMap<String, SmartMoneyTier>, SmartMoneyLookupError> {
        let labels = self
            .label_store
            .addresses_with_label(chain, LabelType::SmartMoney, self.min_confidence)
            .await
            .map_err(|e| SmartMoneyLookupError::LabelStore(e.to_string()))?;

        // Build address → tier map. `addresses_with_label` returns labels ordered
        // by `confidence DESC, address` — first occurrence per address wins
        // (highest confidence tier), matching the "highest tier wins on conflict"
        // semantics from the spec (Decision 6).
        let mut result: HashMap<String, SmartMoneyTier> = HashMap::new();

        for label in &labels {
            // Feedback loop guard: exclude labels issued at or after observed_at.
            // This prevents same-batch smart-money labels from amplifying D04 that
            // just generated them (design 0023 §3.3 + §5.2).
            if label.issued_at >= observed_at {
                continue;
            }

            // Skip if address already resolved (highest-confidence first order).
            if result.contains_key(&label.address) {
                continue;
            }

            let tier = parse_tier_from_label(label)
                .map_err(|e| SmartMoneyLookupError::Parse(e.to_string()))?;

            if let Some(t) = tier {
                result.insert(label.address.clone(), t);
            }
        }

        Ok(result)
    }
}

/// Parse `SmartMoneyTier` from the label's evidence JSON.
///
/// The tier is stored as `evidence["smart_money/tier"] = "tier1" | "tier2" | "tier3"`.
/// Returns `Ok(None)` for labels without a tier key (should not happen in production,
/// but defensively handled).
fn parse_tier_from_label(label: &AddressLabel) -> anyhow::Result<Option<SmartMoneyTier>> {
    let Some(tier_val) = label.evidence.get("smart_money/tier") else {
        return Ok(None);
    };
    let tier_str = tier_val
        .as_str()
        .with_context(|| format!("smart_money/tier is not a string in label for {}", label.address))?;
    let tier = match tier_str {
        "tier1" => Some(SmartMoneyTier::Tier1),
        "tier2" => Some(SmartMoneyTier::Tier2),
        "tier3" => Some(SmartMoneyTier::Tier3),
        other => {
            // SPEC-NOTE: unknown tier strings are silently skipped. Future tier variants
            // (Tier4+) will not break existing deployments. Log in the caller if needed.
            let _ = other;
            None
        }
    };
    Ok(tier)
}

// ---------------------------------------------------------------------------
// MockSmartMoneyLookup — test-utils gated implementation
// ---------------------------------------------------------------------------

/// Mock implementation of [`SmartMoneyLookup`] for unit tests.
///
/// Configured with a fixed set of `(address, tier)` pairs that are returned
/// regardless of chain or timestamp. Gated by `#[cfg(any(test, feature = "test-utils"))]`
/// so it is never shipped in production builds without explicit opt-in.
///
/// # Usage
///
/// ```rust,no_run
/// use mg_onchain_graph::smart_money_lookup::MockSmartMoneyLookup;
/// use mg_onchain_graph::SmartMoneyTier;
///
/// let lookup = MockSmartMoneyLookup::new([
///     ("wallet_abc".to_string(), SmartMoneyTier::Tier1),
/// ]);
/// let empty = MockSmartMoneyLookup::empty();
/// ```
#[cfg(any(test, feature = "test-utils"))]
pub struct MockSmartMoneyLookup {
    /// Fixed set of smart-money addresses with their tiers.
    entries: HashMap<String, SmartMoneyTier>,
    /// Optional: issued_at timestamps for feedback-loop guard testing.
    /// Key: address, Value: the `issued_at` that would be returned for this address.
    /// If None for an address, `issued_at` defaults to `DateTime::<Utc>::MIN_UTC`
    /// (always before any observed_at → always included).
    issued_at_overrides: HashMap<String, DateTime<Utc>>,
}

#[cfg(any(test, feature = "test-utils"))]
impl MockSmartMoneyLookup {
    /// Construct a mock with the given `(address, tier)` pairs.
    pub fn new(entries: impl IntoIterator<Item = (String, SmartMoneyTier)>) -> Self {
        Self {
            entries: entries.into_iter().collect(),
            issued_at_overrides: HashMap::new(),
        }
    }

    /// Construct an empty mock (no smart-money addresses).
    pub fn empty() -> Self {
        Self {
            entries: HashMap::new(),
            issued_at_overrides: HashMap::new(),
        }
    }

    /// Override the `issued_at` for a specific address (for feedback-loop guard tests).
    ///
    /// When set, the mock will exclude this address from results if
    /// `issued_at >= observed_at` — mirroring the production guard.
    pub fn with_issued_at(mut self, address: impl Into<String>, issued_at: DateTime<Utc>) -> Self {
        self.issued_at_overrides.insert(address.into(), issued_at);
        self
    }
}

#[cfg(any(test, feature = "test-utils"))]
#[async_trait]
impl SmartMoneyLookup for MockSmartMoneyLookup {
    async fn fetch_smart_money_addresses(
        &self,
        _chain: &str,
        observed_at: DateTime<Utc>,
    ) -> Result<HashMap<String, SmartMoneyTier>, SmartMoneyLookupError> {
        let mut result = HashMap::new();
        for (address, tier) in &self.entries {
            // Apply feedback-loop guard if issued_at override is set.
            if let Some(&issued_at) = self.issued_at_overrides.get(address)
                && issued_at >= observed_at
            {
                // Excluded by feedback-loop guard.
                continue;
            }
            result.insert(address.clone(), *tier);
        }
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn make_mock_with_tier1() -> MockSmartMoneyLookup {
        MockSmartMoneyLookup::new([
            ("wallet_alpha".to_string(), SmartMoneyTier::Tier1),
            ("wallet_beta".to_string(), SmartMoneyTier::Tier2),
            ("wallet_gamma".to_string(), SmartMoneyTier::Tier3),
        ])
    }

    /// MockSmartMoneyLookup round-trip: all entries returned when observed_at
    /// is after all issued_at defaults (DateTime::MIN_UTC).
    #[tokio::test]
    async fn mock_round_trip_all_entries_returned() {
        let lookup = make_mock_with_tier1();
        let observed = Utc::now();
        let map = lookup
            .fetch_smart_money_addresses("solana", observed)
            .await
            .unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(map.get("wallet_alpha"), Some(&SmartMoneyTier::Tier1));
        assert_eq!(map.get("wallet_beta"), Some(&SmartMoneyTier::Tier2));
        assert_eq!(map.get("wallet_gamma"), Some(&SmartMoneyTier::Tier3));
    }

    /// MockSmartMoneyLookup empty(): returns empty map.
    #[tokio::test]
    async fn mock_empty_returns_empty_map() {
        let lookup = MockSmartMoneyLookup::empty();
        let map = lookup
            .fetch_smart_money_addresses("solana", Utc::now())
            .await
            .unwrap();
        assert!(map.is_empty());
    }

    /// Feedback-loop guard: labels with issued_at >= observed_at are excluded.
    #[tokio::test]
    async fn mock_filters_future_labels_feedback_loop_guard() {
        let observed = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        // issued_at = observed_at (same instant → excluded per guard §3.3)
        let lookup = MockSmartMoneyLookup::new([("wallet_a".to_string(), SmartMoneyTier::Tier1)])
            .with_issued_at("wallet_a", observed);

        let map = lookup
            .fetch_smart_money_addresses("solana", observed)
            .await
            .unwrap();
        assert!(
            map.is_empty(),
            "label with issued_at == observed_at must be excluded by feedback-loop guard"
        );
    }

    /// Feedback-loop guard: label issued 1 second before observed_at is included.
    #[tokio::test]
    async fn mock_includes_labels_before_observed_at() {
        let observed = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let issued = observed - Duration::seconds(1);
        let lookup = MockSmartMoneyLookup::new([("wallet_a".to_string(), SmartMoneyTier::Tier1)])
            .with_issued_at("wallet_a", issued);

        let map = lookup
            .fetch_smart_money_addresses("solana", observed)
            .await
            .unwrap();
        assert_eq!(map.get("wallet_a"), Some(&SmartMoneyTier::Tier1));
    }

    /// `SmartMoneyLookup` is dyn-compatible (trait object usable).
    #[test]
    fn smart_money_lookup_is_dyn_compatible() {
        fn _accepts_dyn(_s: &dyn SmartMoneyLookup) {}
        fn _accepts_arc(_s: Arc<dyn SmartMoneyLookup>) {}
    }

    /// `parse_tier_from_label` correctly maps tier strings to enum variants.
    #[test]
    fn parse_tier_from_label_all_variants() {
        let base_label = AddressLabel {
            chain: "solana".into(),
            address: "abc".into(),
            label_type: LabelType::SmartMoney,
            confidence: 0.80,
            evidence: serde_json::json!({}),
            issued_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            expires_at: None,
            source: "smart_money_labeller".into(),
        };

        for (tier_str, expected) in [
            ("tier1", SmartMoneyTier::Tier1),
            ("tier2", SmartMoneyTier::Tier2),
            ("tier3", SmartMoneyTier::Tier3),
        ] {
            let mut label = base_label.clone();
            label.evidence = serde_json::json!({"smart_money/tier": tier_str});
            let result = parse_tier_from_label(&label).unwrap();
            assert_eq!(result, Some(expected), "tier_str={tier_str}");
        }
    }

    /// Unknown tier string returns `Ok(None)` (no panic, no error).
    #[test]
    fn parse_tier_from_label_unknown_returns_none() {
        let label = AddressLabel {
            chain: "solana".into(),
            address: "abc".into(),
            label_type: LabelType::SmartMoney,
            confidence: 0.80,
            evidence: serde_json::json!({"smart_money/tier": "tier99"}),
            issued_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            expires_at: None,
            source: "smart_money_labeller".into(),
        };
        let result = parse_tier_from_label(&label).unwrap();
        assert_eq!(result, None);
    }

    /// Missing tier key returns `Ok(None)`.
    #[test]
    fn parse_tier_from_label_missing_key_returns_none() {
        let label = AddressLabel {
            chain: "solana".into(),
            address: "abc".into(),
            label_type: LabelType::SmartMoney,
            confidence: 0.80,
            evidence: serde_json::json!({}),
            issued_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            expires_at: None,
            source: "smart_money_labeller".into(),
        };
        let result = parse_tier_from_label(&label).unwrap();
        assert_eq!(result, None);
    }
}
