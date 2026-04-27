//! Holder account classification.
//!
//! Classifies each holder address into one of:
//!   - `burn_address`     — the Solana null key (11111...1111)
//!   - `dex_pool`         — token account owned by a known DEX program
//!   - `vesting_contract` — token account owned by a known vesting program
//!   - `cex_hot_wallet`   — address in the CEX seed list
//!   - `liquid`           — everything else (EOA or unrecognised program)
//!
//! Classification is pure (no I/O beyond the owner lookup); the RPC call
//! to resolve the token-account → owner wallet goes through [`SolanaRpc`].
//!
//! # Classification ladder (early-exit, most specific first)
//!
//! 1. Burn address check (no RPC needed — just string comparison).
//! 2. CEX wallet lookup (no RPC needed — in-memory map).
//! 3. DEX pool check: fetch token account owner → match against DEX program list.
//! 4. Vesting check: fetch token account owner → match against vesting program list.
//! 5. Fallback: `liquid`, confidence=0.5, TTL=24h.
//!
//! # Why confidence < 1.0 for `liquid`?
//!
//! We cannot positively confirm a `liquid` classification — we only confirm
//! that none of our known patterns matched. The holder might be a program we
//! don't know yet. Confidence=0.5 signals "unknown, assumed liquid"; detectors
//! can treat low-confidence classifications as requiring re-check.
//!
//! # Sidecar table
//!
//! Results are written to the `holder_classifications` Postgres table
//! (migration V00003). The table key is `(chain, address)`. Existing rows
//! are upserted only if the new classification has higher confidence or the
//! row has expired (TTL).

use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use tracing::{debug, instrument};

use crate::cex_registry::CexRegistry;
use crate::error::RegistryError;
use crate::programs::{classify_dex_owner, classify_vesting_owner, is_burn_address};
use crate::rpc::SolanaRpc;

// ---------------------------------------------------------------------------
// HolderKind enum
// ---------------------------------------------------------------------------

/// The classification result for a single holder address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolderKind {
    BurnAddress,
    DexPool { subkind: String },
    VestingContract { subkind: String },
    CexHotWallet { subkind: String },
    /// Liquid EOA or unrecognised program. Low confidence.
    Liquid,
}

impl HolderKind {
    /// Returns the `kind` string stored in the sidecar table.
    pub fn kind_str(&self) -> &'static str {
        match self {
            HolderKind::BurnAddress => "burn_address",
            HolderKind::DexPool { .. } => "dex_pool",
            HolderKind::VestingContract { .. } => "vesting_contract",
            HolderKind::CexHotWallet { .. } => "cex_hot_wallet",
            HolderKind::Liquid => "liquid",
        }
    }

    /// Returns the optional `subkind` string for the sidecar table.
    pub fn subkind_str(&self) -> Option<&str> {
        match self {
            HolderKind::DexPool { subkind }
            | HolderKind::VestingContract { subkind }
            | HolderKind::CexHotWallet { subkind } => Some(subkind.as_str()),
            _ => None,
        }
    }

    /// Confidence for this classification (stored as DOUBLE PRECISION in PG).
    pub fn confidence(&self) -> f64 {
        match self {
            HolderKind::BurnAddress => 1.0,
            HolderKind::DexPool { .. } => 0.95,
            HolderKind::VestingContract { .. } => 0.90,
            HolderKind::CexHotWallet { .. } => 0.85,
            HolderKind::Liquid => 0.5,
        }
    }

    /// How long until this classification should be re-evaluated.
    /// `None` = permanent (e.g. burn address never changes).
    pub fn ttl(&self) -> Option<Duration> {
        match self {
            HolderKind::BurnAddress => None,   // burn address is permanent
            HolderKind::DexPool { .. } => None, // program ownership doesn't change
            HolderKind::VestingContract { .. } => Some(Duration::days(7)),
            HolderKind::CexHotWallet { .. } => Some(Duration::days(30)),
            HolderKind::Liquid => Some(Duration::hours(24)),
        }
    }
}

/// A completed classification for one holder address.
#[derive(Debug, Clone)]
pub struct Classification {
    pub chain: String,
    pub address: String,
    pub kind: HolderKind,
    pub classified_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Evidence JSON stored in the sidecar table `evidence` column.
    pub evidence: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Classifier
// ---------------------------------------------------------------------------

/// Classifies holder addresses using the classification ladder.
///
/// Constructed with references to the RPC client and CEX registry.
/// The RPC client is used only for steps 3 and 4 (owner lookup).
/// Steps 1, 2, 5 are pure (no I/O).
pub struct HolderClassifier<'a> {
    rpc: &'a dyn SolanaRpc,
    cex: &'a CexRegistry,
}

impl<'a> HolderClassifier<'a> {
    /// Construct a new classifier.
    pub fn new(rpc: &'a dyn SolanaRpc, cex: &'a CexRegistry) -> Self {
        Self { rpc, cex }
    }

    /// Classify a single holder address.
    ///
    /// `address` is a Solana Base58 address (the **token account** address,
    /// not the owner wallet — the owner is resolved via RPC for DEX/vesting checks).
    ///
    /// `chain` is stored in the classification row (currently always `"solana"`).
    #[instrument(skip(self), fields(address, chain))]
    pub async fn classify(
        &self,
        address: &str,
        chain: &str,
    ) -> Result<Classification, RegistryError> {
        let now = Utc::now();

        // Step 1: Burn address (no RPC needed)
        if is_burn_address(address) {
            return Ok(Classification {
                chain: chain.to_owned(),
                address: address.to_owned(),
                kind: HolderKind::BurnAddress,
                classified_at: now,
                expires_at: None,
                evidence: json!({ "reason": "null_key_is_canonical_burn_address" }),
            });
        }

        // Step 2: CEX hot wallet (no RPC needed)
        if let Some(cex_match) = self.cex.lookup(address) {
            return Ok(Classification {
                chain: chain.to_owned(),
                address: address.to_owned(),
                kind: HolderKind::CexHotWallet {
                    subkind: cex_match.exchange.clone(),
                },
                classified_at: now,
                expires_at: Some(now + Duration::days(30)),
                evidence: json!({
                    "exchange": cex_match.exchange,
                    "source": "cex_wallets.json seed list"
                }),
            });
        }

        // Step 3 & 4: DEX pool and vesting check — requires owner lookup from RPC.
        // The `address` here is the SPL token account address. We need the program
        // that owns it to determine if it's a DEX pool or vesting contract.
        match self.rpc.get_token_account_owner(address).await {
            Ok(Some(owner)) => {
                // Step 3: DEX pool check
                if let Some(subkind) = classify_dex_owner(&owner) {
                    return Ok(Classification {
                        chain: chain.to_owned(),
                        address: address.to_owned(),
                        kind: HolderKind::DexPool {
                            subkind: subkind.to_owned(),
                        },
                        classified_at: now,
                        expires_at: None, // program ownership is permanent
                        evidence: json!({
                            "owner_program": owner,
                            "subkind": subkind
                        }),
                    });
                }

                // Step 4: Vesting contract check
                if let Some(subkind) = classify_vesting_owner(&owner) {
                    let ttl = HolderKind::VestingContract { subkind: String::new() }.ttl();
                    return Ok(Classification {
                        chain: chain.to_owned(),
                        address: address.to_owned(),
                        kind: HolderKind::VestingContract {
                            subkind: subkind.to_owned(),
                        },
                        classified_at: now,
                        expires_at: ttl.map(|d| now + d),
                        evidence: json!({
                            "owner_program": owner,
                            "subkind": subkind
                        }),
                    });
                }

                // Step 5: Fallback — liquid (EOA or unknown program)
                debug!(address, owner, "holder classified as liquid (fallback)");
                Ok(Classification {
                    chain: chain.to_owned(),
                    address: address.to_owned(),
                    kind: HolderKind::Liquid,
                    classified_at: now,
                    expires_at: Some(now + Duration::hours(24)),
                    evidence: json!({
                        "owner_program": owner,
                        "reason": "no_known_pattern_matched"
                    }),
                })
            }
            Ok(None) | Err(_) => {
                // Cannot resolve owner — treat as liquid with low confidence.
                // RPC errors here are non-fatal for classification; the caller
                // can retry later. We still return a `Liquid` result.
                debug!(address, "owner lookup failed — defaulting to liquid");
                Ok(Classification {
                    chain: chain.to_owned(),
                    address: address.to_owned(),
                    kind: HolderKind::Liquid,
                    classified_at: now,
                    expires_at: Some(now + Duration::hours(24)),
                    evidence: json!({ "reason": "owner_lookup_unavailable" }),
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Postgres upsert for classification sidecar
// ---------------------------------------------------------------------------

/// Upsert a classification row into the `holder_classifications` table.
///
/// Uses ON CONFLICT to update only when the new confidence is higher than
/// the existing one, or when the existing row has expired.
///
/// This is the write side of the sidecar; the read side is a LEFT JOIN
/// in detector queries.
pub async fn upsert_classification(
    pool: &sqlx::PgPool,
    c: &Classification,
) -> Result<(), RegistryError> {
    let expires_at = c.expires_at;
    let confidence = c.kind.confidence();
    let subkind = c.kind.subkind_str();
    let evidence_json = c.evidence.clone();

    sqlx::query(
        r#"INSERT INTO holder_classifications
            (chain, address, kind, subkind, confidence, classified_at, expires_at, evidence)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
           ON CONFLICT (chain, address) DO UPDATE SET
             kind          = EXCLUDED.kind,
             subkind       = EXCLUDED.subkind,
             confidence    = EXCLUDED.confidence,
             classified_at = EXCLUDED.classified_at,
             expires_at    = EXCLUDED.expires_at,
             evidence      = EXCLUDED.evidence
           WHERE EXCLUDED.confidence >= holder_classifications.confidence
              OR holder_classifications.expires_at < now()"#,
    )
    .bind(&c.chain)
    .bind(&c.address)
    .bind(c.kind.kind_str())
    .bind(subkind)
    .bind(confidence)
    .bind(c.classified_at)
    .bind(expires_at)
    .bind(sqlx::types::Json(evidence_json))
    .execute(pool)
    .await
    .map_err(|e| RegistryError::Internal(format!("upsert_classification: {e}")))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cex_registry::CexRegistry;
    use crate::programs::{BURN_ADDRESS, ORCA_WHIRLPOOL, RAYDIUM_AMM_V4, STREAMFLOW_TIMELOCK, JUPITER_LOCK};
    use crate::rpc::tests::MockSolanaRpc;

    fn make_cex_json(address: &str, exchange: &str) -> String {
        format!(
            r#"{{"wallets":[{{"address":"{}","exchange":"{}","label":"test","source":"test"}}]}}"#,
            address, exchange
        )
    }

    // --- Rule 1: burn address ---

    #[tokio::test]
    async fn classify_burn_address() {
        let rpc = MockSolanaRpc::default();
        let cex = CexRegistry::from_json(r#"{"wallets":[]}"#).unwrap();
        let classifier = HolderClassifier::new(&rpc, &cex);
        let result = classifier.classify(BURN_ADDRESS, "solana").await.unwrap();
        assert_eq!(result.kind.kind_str(), "burn_address");
        assert_eq!(result.kind.confidence(), 1.0);
        assert!(result.expires_at.is_none(), "burn address has no expiry");
    }

    // --- Rule 2: CEX hot wallet ---

    #[tokio::test]
    async fn classify_cex_hot_wallet() {
        let cex_addr = "5tzFkiKscXHK5ZXCGbCAbZxLKQAFqobMVBkn5cCgPKEu";
        let rpc = MockSolanaRpc::default();
        let cex = CexRegistry::from_json(&make_cex_json(cex_addr, "binance")).unwrap();
        let classifier = HolderClassifier::new(&rpc, &cex);
        let result = classifier.classify(cex_addr, "solana").await.unwrap();
        assert_eq!(result.kind.kind_str(), "cex_hot_wallet");
        assert_eq!(
            result.kind.subkind_str(),
            Some("binance"),
            "subkind must be exchange name"
        );
        assert!(result.expires_at.is_some(), "CEX wallets expire (must re-verify)");
    }

    // --- Rule 3: DEX pool (Raydium v4 owner) ---

    #[tokio::test]
    async fn classify_dex_pool_raydium_v4() {
        let token_account = "SomeTokenAccount1111111111111111111111111111";
        let rpc = MockSolanaRpc {
            token_account_owner: Some(Ok(Some(RAYDIUM_AMM_V4.to_owned()))),
            ..Default::default()
        };
        let cex = CexRegistry::from_json(r#"{"wallets":[]}"#).unwrap();
        let classifier = HolderClassifier::new(&rpc, &cex);
        let result = classifier.classify(token_account, "solana").await.unwrap();
        assert_eq!(result.kind.kind_str(), "dex_pool");
        assert_eq!(result.kind.subkind_str(), Some("raydium_amm_v4"));
        assert_eq!(result.kind.confidence(), 0.95);
        assert!(result.expires_at.is_none(), "DEX pool ownership is permanent");
    }

    // --- Rule 3: DEX pool (Orca owner) ---

    #[tokio::test]
    async fn classify_dex_pool_orca_whirlpool() {
        let token_account = "SomeTokenAccount1111111111111111111111111112";
        let rpc = MockSolanaRpc {
            token_account_owner: Some(Ok(Some(ORCA_WHIRLPOOL.to_owned()))),
            ..Default::default()
        };
        let cex = CexRegistry::from_json(r#"{"wallets":[]}"#).unwrap();
        let classifier = HolderClassifier::new(&rpc, &cex);
        let result = classifier.classify(token_account, "solana").await.unwrap();
        assert_eq!(result.kind.kind_str(), "dex_pool");
        assert_eq!(result.kind.subkind_str(), Some("orca_whirlpool"));
    }

    // --- Rule 4: Vesting contract (Streamflow) ---

    #[tokio::test]
    async fn classify_vesting_streamflow() {
        let token_account = "SomeVestingTokenAccount11111111111111111111";
        let rpc = MockSolanaRpc {
            token_account_owner: Some(Ok(Some(STREAMFLOW_TIMELOCK.to_owned()))),
            ..Default::default()
        };
        let cex = CexRegistry::from_json(r#"{"wallets":[]}"#).unwrap();
        let classifier = HolderClassifier::new(&rpc, &cex);
        let result = classifier.classify(token_account, "solana").await.unwrap();
        assert_eq!(result.kind.kind_str(), "vesting_contract");
        assert_eq!(result.kind.subkind_str(), Some("streamflow"));
        assert_eq!(result.kind.confidence(), 0.90);
        assert!(result.expires_at.is_some(), "vesting classification expires");
    }

    // --- Rule 4: Vesting contract (Jupiter Lock) ---

    #[tokio::test]
    async fn classify_vesting_jupiter_lock() {
        let token_account = "SomeJupLockTokenAccount111111111111111111111";
        let rpc = MockSolanaRpc {
            token_account_owner: Some(Ok(Some(JUPITER_LOCK.to_owned()))),
            ..Default::default()
        };
        let cex = CexRegistry::from_json(r#"{"wallets":[]}"#).unwrap();
        let classifier = HolderClassifier::new(&rpc, &cex);
        let result = classifier.classify(token_account, "solana").await.unwrap();
        assert_eq!(result.kind.kind_str(), "vesting_contract");
        assert_eq!(result.kind.subkind_str(), Some("jupiter_lock"));
    }

    // --- Rule 5: fallback to liquid ---

    #[tokio::test]
    async fn classify_unknown_owner_returns_liquid() {
        let token_account = "SomeRandomWallet11111111111111111111111111";
        let rpc = MockSolanaRpc {
            token_account_owner: Some(Ok(Some(
                "SomeRandomProgram111111111111111111111111111".to_owned(),
            ))),
            ..Default::default()
        };
        let cex = CexRegistry::from_json(r#"{"wallets":[]}"#).unwrap();
        let classifier = HolderClassifier::new(&rpc, &cex);
        let result = classifier.classify(token_account, "solana").await.unwrap();
        assert_eq!(result.kind.kind_str(), "liquid");
        assert_eq!(result.kind.confidence(), 0.5);
        assert!(result.expires_at.is_some(), "liquid classification expires in 24h");
    }

    // --- Rule 5: fallback when owner lookup returns None ---

    #[tokio::test]
    async fn classify_no_owner_returns_liquid() {
        let token_account = "SomeClosedAccount11111111111111111111111111";
        let rpc = MockSolanaRpc {
            token_account_owner: Some(Ok(None)), // account not found
            ..Default::default()
        };
        let cex = CexRegistry::from_json(r#"{"wallets":[]}"#).unwrap();
        let classifier = HolderClassifier::new(&rpc, &cex);
        let result = classifier.classify(token_account, "solana").await.unwrap();
        assert_eq!(result.kind.kind_str(), "liquid");
    }

    // --- CEX takes priority over owner lookup (burn address check is first) ---

    #[tokio::test]
    async fn burn_address_takes_priority_over_cex_lookup() {
        // If somehow the burn address were in the CEX list, burn_address wins.
        let rpc = MockSolanaRpc::default();
        let cex = CexRegistry::from_json(&make_cex_json(BURN_ADDRESS, "fake_exchange")).unwrap();
        let classifier = HolderClassifier::new(&rpc, &cex);
        let result = classifier.classify(BURN_ADDRESS, "solana").await.unwrap();
        // Burn address check runs first and returns early.
        assert_eq!(result.kind.kind_str(), "burn_address");
    }
}
