//! Test-only mock implementations for injection into `DetectorContext`.
//!
//! # Pattern
//!
//! Detectors are unit-testable without a live database by splitting the
//! implementation into two layers:
//!
//! 1. **`fetch_rows(store, ctx) -> Result<Vec<MyRow>>`** — async function that
//!    executes SQL and deserialises into a plain struct. This layer is tested in
//!    integration tests (real Postgres container) but NOT in unit tests.
//!
//! 2. **`compute(rows, config) -> Vec<AnomalyEvent>`** — pure synchronous function
//!    that applies the detector's logic to the fetched rows. This is what unit
//!    tests exercise by calling it directly with canned `Vec<MyRow>` inputs.
//!
//! The `MockTokenRegistry` in this module provides a stubbed `TokenRegistry`-shaped
//! value that returns predetermined `TokenMeta` data, enabling tests of the full
//! `evaluate()` path without any I/O.
//!
//! # Downstream reuse
//!
//! These mocks are gated behind `#[cfg(any(test, feature = "test-utils"))]` so
//! downstream crates (e.g. `crates/server` integration tests) can enable the
//! `test-utils` feature to reuse them without shipping mock code in production
//! binaries.
//!
//! # TODO(developer)
//!
//! As the query surface grows, consider a lightweight "query fixture" pattern:
//! `MockPgRunner` reads from a JSON file in
//! `tests/fixtures/solana/<token>/d01_result.json` and returns typed rows.
//! This decouples fixture maintenance from test code and supports the
//! `CLAUDE.md` §"labelled test fixture" requirement.

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use rust_decimal::Decimal;

    use mg_onchain_common::anomaly::Evidence;
    use mg_onchain_common::chain::{Address, BlockRef, Chain};
    use mg_onchain_common::token::{
        InsiderNetwork, JupiterVerification, MarketInfo, TokenMeta, TopHolder, TransferFeeConfig,
    };

    // -------------------------------------------------------------------------
    // CannedRow
    // -------------------------------------------------------------------------

    /// A canned Postgres row result for testing.
    ///
    /// Each detector test constructs the specific row shape it needs and passes
    /// it to the detector's `compute()` function directly (no DB required).
    ///
    /// See module-level doc for the `fetch_rows` / `compute` split rationale.
    #[derive(Debug, Clone, Default)]
    pub struct CannedRow {
        /// String-keyed fields, matching column names in the SQL result set.
        pub fields: BTreeMap<String, String>,
    }

    impl CannedRow {
        /// Create an empty canned row.
        pub fn new() -> Self {
            Self::default()
        }

        /// Builder: set a field value.
        pub fn with_field(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
            self.fields.insert(key.into(), value.into());
            self
        }

        /// Get a field as a string slice. Returns `""` if the field is missing.
        pub fn get(&self, key: &str) -> &str {
            self.fields.get(key).map(String::as_str).unwrap_or("")
        }

        /// Get a field parsed as `f64`. Returns `None` if missing or unparseable.
        pub fn get_f64(&self, key: &str) -> Option<f64> {
            self.fields.get(key)?.parse().ok()
        }

        /// Get a field parsed as `i64`. Returns `None` if missing or unparseable.
        pub fn get_i64(&self, key: &str) -> Option<i64> {
            self.fields.get(key)?.parse().ok()
        }

        /// Get a field parsed as `Decimal`. Returns `None` if missing or unparseable.
        pub fn get_decimal(&self, key: &str) -> Option<Decimal> {
            self.fields.get(key)?.parse().ok()
        }
    }

    // -------------------------------------------------------------------------
    // MockTokenMeta builder
    // -------------------------------------------------------------------------

    /// Build a minimal `TokenMeta` suitable for detector unit tests.
    ///
    /// All optional fields default to `None`; callers override what their test
    /// requires.
    pub struct MockTokenMetaBuilder {
        meta: TokenMeta,
    }

    impl MockTokenMetaBuilder {
        /// Create a builder with a well-formed but minimal Solana token meta.
        pub fn new_solana(mint: &str) -> Self {
            let addr = Address::parse(Chain::Solana, mint)
                .expect("test mint address must be valid Solana Base58");
            Self {
                meta: TokenMeta {
                    mint: addr,
                    chain: Chain::Solana,
                    symbol: None,
                    name: None,
                    decimals: 6,
                    token_program: None,
                    total_supply_raw: 1_000_000_000_000_000u128, // 1B tokens at 6 decimals
                    circulating_supply_raw: Some(1_000_000_000_000_000u128),
                    mint_authority: None,
                    freeze_authority: None,
                    creator: None,
                    creator_balance_raw: 0,
                    transfer_fee: None,
                    permanent_delegate: None,
                    transfer_hook_program: None,
                    non_transferable: false,
                    confidential_transfer: false,
                    top_holders: vec![],
                    total_holders: 1000,
                    markets: vec![],
                    total_market_liquidity_usd: Decimal::new(50_000, 0),
                    lockers: vec![],
                    graph_insiders_detected: false,
                    insider_networks: vec![],
                    launchpad: None,
                    deploy_platform: None,
                    detected_at: Some(Utc::now()),
                    rugged: false,
                    verification: JupiterVerification {
                        jup_verified: false,
                        jup_strict: false,
                    },
                    rugcheck_score: None,
                    buy_tax: None,
                    sell_tax: None,
                    transfer_tax: None,
                    honeypot_flags: vec![],
                    updated_at: Utc::now(),
                },
            }
        }

        /// Set mint authority (simulates an active mint authority — risk signal).
        pub fn with_mint_authority(mut self, authority: &str) -> Self {
            self.meta.mint_authority =
                Some(Address::parse(Chain::Solana, authority).expect("valid Solana address"));
            self
        }

        /// Set freeze authority.
        pub fn with_freeze_authority(mut self, authority: &str) -> Self {
            self.meta.freeze_authority =
                Some(Address::parse(Chain::Solana, authority).expect("valid Solana address"));
            self
        }

        /// Set a Token-2022 transfer fee.
        pub fn with_transfer_fee(mut self, fee_bps: u16, authority: Option<&str>) -> Self {
            let auth = authority.and_then(|a| Address::parse(Chain::Solana, a).ok());
            self.meta.transfer_fee = Some(TransferFeeConfig {
                fee_bps,
                max_fee_raw: u128::MAX / 2,
                authority: auth,
            });
            self
        }

        /// Set a Token-2022 permanent delegate (DG2 — S3 signal).
        pub fn with_permanent_delegate(mut self, delegate: &str) -> Self {
            self.meta.permanent_delegate =
                Some(Address::parse(Chain::Solana, delegate).expect("valid Solana address"));
            self
        }

        /// Set a Token-2022 transfer hook program (DG2 — S4 signal).
        pub fn with_transfer_hook_program(mut self, program: &str) -> Self {
            self.meta.transfer_hook_program =
                Some(Address::parse(Chain::Solana, program).expect("valid Solana address"));
            self
        }

        /// Set the Token-2022 NonTransferable extension flag (ext discriminator 9).
        ///
        /// When `true`, D01 S1 freeze-authority weight is attenuated; D05 returns
        /// `InsufficientBaseline`.
        pub fn with_non_transferable(mut self) -> Self {
            self.meta.non_transferable = true;
            self
        }

        /// Set the Token-2022 ConfidentialTransferMint extension flag (ext discriminator 4).
        ///
        /// When `true`, D05 returns `InsufficientBaseline` (amounts are ZK-encrypted).
        pub fn with_confidential_transfer(mut self) -> Self {
            self.meta.confidential_transfer = true;
            self
        }

        /// Set Jupiter verification flags.
        pub fn jup_verified(mut self, verified: bool, strict: bool) -> Self {
            self.meta.verification = JupiterVerification {
                jup_verified: verified,
                jup_strict: strict,
            };
            self
        }

        /// Mark as rugged (positive-class label).
        pub fn rugged(mut self) -> Self {
            self.meta.rugged = true;
            self
        }

        /// Set the total holders count.
        pub fn with_total_holders(mut self, n: u64) -> Self {
            self.meta.total_holders = n;
            self
        }

        /// Add an insider network.
        pub fn with_insider_network(mut self, network: InsiderNetwork) -> Self {
            self.meta.insider_networks.push(network);
            self
        }

        /// Add a top holder.
        pub fn with_top_holder(mut self, holder: TopHolder) -> Self {
            self.meta.top_holders.push(holder);
            self
        }

        /// Add a market (DEX pool).
        pub fn with_market(mut self, market: MarketInfo) -> Self {
            self.meta.markets.push(market);
            self
        }

        /// Set `total_supply_raw` (raw token units as u128).
        pub fn with_total_supply(mut self, supply: u128) -> Self {
            self.meta.total_supply_raw = supply;
            self
        }

        /// Set `circulating_supply_raw` (raw token units as u128).
        pub fn with_circulating_supply(mut self, supply: u128) -> Self {
            self.meta.circulating_supply_raw = Some(supply);
            self
        }

        /// Set `detected_at` (first-seen timestamp, used for token age computation).
        ///
        /// This should be a deterministic value in tests (not `Utc::now()`).
        pub fn with_detected_at(mut self, at: chrono::DateTime<Utc>) -> Self {
            self.meta.detected_at = Some(at);
            self
        }

        /// Set `rugcheck_score` (normalised 0–100, lower = safer).
        pub fn with_rugcheck_score(mut self, score: u32) -> Self {
            self.meta.rugcheck_score = Some(score);
            self
        }

        /// Finalize and return the `TokenMeta`.
        pub fn build(self) -> TokenMeta {
            self.meta
        }
    }

    // -------------------------------------------------------------------------
    // Evidence assertion helpers
    // -------------------------------------------------------------------------

    /// Assert that an `Evidence` bundle contains a metric key with a known value.
    ///
    /// Panics with a descriptive message if the key is missing or the value does
    /// not match.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// assert_evidence_metric(
    ///     &event.evidence,
    ///     "honeypot_sim/freeze_authority_active",
    ///     Decimal::ZERO,
    /// );
    /// ```
    pub fn assert_evidence_metric(evidence: &Evidence, key: &str, expected: Decimal) {
        let got = evidence.metrics.get(key).copied().unwrap_or_else(|| {
            panic!(
                "evidence missing key '{key}'. Available keys: {:?}",
                evidence.metrics.keys().collect::<Vec<_>>()
            )
        });
        assert_eq!(
            got, expected,
            "evidence key '{key}': expected {expected}, got {got}"
        );
    }

    /// Assert that an `Evidence` bundle contains a specific key (value unchecked).
    pub fn assert_evidence_has_key(evidence: &Evidence, key: &str) {
        assert!(
            evidence.metrics.contains_key(key),
            "evidence missing key '{key}'. Available keys: {:?}",
            evidence.metrics.keys().collect::<Vec<_>>()
        );
    }

    // -------------------------------------------------------------------------
    // Solana null address constant for tests
    // -------------------------------------------------------------------------

    /// Solana null/zero address (system program): the canonical burn address.
    ///
    /// Use as `zero_address` in `DetectorContext` for Solana unit tests.
    pub const SOLANA_ZERO_ADDRESS: &str = "11111111111111111111111111111111";

    /// A well-formed 32-byte Solana address (the SOL native mint) usable in tests.
    ///
    /// This is a deterministic placeholder — use in fixtures where the actual
    /// token address doesn't matter, only the structure.
    pub const SOL_NATIVE_MINT: &str = "So11111111111111111111111111111111111111112";

    // -------------------------------------------------------------------------
    // BlockRef helpers for tests
    // -------------------------------------------------------------------------

    /// Build a `DetectorWindow` spanning the given slot range.
    ///
    /// Block times are set to `Utc::now()` minus the range span, purely for
    /// unit-test purposes. Production code always derives times from block metadata.
    pub fn test_window(start_slot: u64, end_slot: u64) -> crate::context::DetectorWindow {
        use chrono::Duration;
        let now = Utc::now();
        let duration_secs = (end_slot - start_slot) as i64; // 1 slot ≈ 1 second on Solana
        crate::context::DetectorWindow {
            start: now - Duration::seconds(duration_secs),
            end: now,
            block_start: BlockRef::new(Chain::Solana, start_slot),
            block_end: BlockRef::new(Chain::Solana, end_slot),
        }
    }
}

// Re-export for convenience in sibling test modules.
#[cfg(any(test, feature = "test-utils"))]
pub use test_utils::*;
