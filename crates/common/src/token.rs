//! Token metadata and holder state types.
//!
//! [`TokenMeta`] is the primary type here. It is designed as a **superset** of the
//! RugCheck v1 API live-response (verified 2026-04-21, `research/01-market-scan.md`
//! §RugCheck.xyz "Specific signals exposed") plus Honeypot.is EVM fields reserved
//! for Phase 4.
//!
//! Fields marked "Phase 4 reserved" are present in the struct now so that
//! `TokenMeta` can be written to storage and served via REST in a future-compatible
//! way, but they will be `None` for all Phase 1/2 events. Do not branch on them
//! in Phase 2 detector code.
//!
//! # Serde strategy
//!
//! `rename_all = "camelCase"` for REST/WS wire compatibility with RugCheck field
//! names. The RugCheck API uses camelCase (e.g. `mintAuthority`, `freezeAuthority`,
//! `topHolders`). All amount fields serialize as strings.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::amount::{deserialize_u128_from_str, serialize_u128_as_str};
use crate::chain::{Address, BlockRef, Chain};
use crate::event::DexKind;

// ---------------------------------------------------------------------------
// TokenMeta
// ---------------------------------------------------------------------------

/// Full token metadata for a Solana SPL or EVM ERC-20 token.
///
/// Updated by the `token-registry` crate whenever on-chain state changes.
/// Persisted in Postgres (`tokens` table) for hot access.
///
/// Designed as a superset of the RugCheck v1 API live-response (ADR 0001 §D6).
/// Field names mirror RugCheck camelCase names where possible.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenMeta {
    // --- Identity ---

    /// The token's mint address (Solana) or contract address (EVM).
    /// Stored in chain-canonical form (Base58 / checksummed hex).
    pub mint: Address,

    /// Which chain this token lives on.
    pub chain: Chain,

    /// Ticker symbol. May be empty for newly deployed tokens not yet indexed.
    pub symbol: Option<String>,

    /// Human-readable name.
    pub name: Option<String>,

    /// Decimal exponent. CRITICAL: never hardcode 18 for EVM tokens.
    /// RugCheck field: `decimals`.
    pub decimals: u8,

    /// Token program address (Solana). Distinguishes SPL Token from Token-2022.
    /// `None` for EVM tokens.
    /// RugCheck field: `tokenProgram`.
    pub token_program: Option<Address>,

    // --- Supply ---

    /// Total on-chain supply in raw units. Serialized as string.
    /// RugCheck field: `totalSupply` (raw).
    #[serde(
        serialize_with = "serialize_u128_as_str",
        deserialize_with = "deserialize_u128_from_str"
    )]
    pub total_supply_raw: u128,

    /// Circulating supply (excluding locked/burned/deployer cluster) in raw units.
    /// Computed by `token-registry`; may lag real-time. `None` if not yet computed.
    #[serde(
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub circulating_supply_raw: Option<u128>,

    // --- Authority flags (RugCheck core signals) ---

    /// Who can mint new tokens. `None` = authority revoked (safer).
    /// RugCheck field: `mintAuthority`.
    pub mint_authority: Option<Address>,

    /// Who can freeze token accounts. `None` = authority revoked.
    /// RugCheck field: `freezeAuthority`.
    pub freeze_authority: Option<Address>,

    // --- Deployer / creator ---

    /// Wallet that deployed the token.
    /// RugCheck field: `creator`.
    pub creator: Option<Address>,

    /// Current balance of the creator wallet in raw token units.
    /// RugCheck field: `creatorBalance` (raw).
    #[serde(
        serialize_with = "serialize_u128_as_str",
        deserialize_with = "deserialize_u128_from_str",
        default
    )]
    pub creator_balance_raw: u128,

    // --- Token-2022 extensions (Solana) ---

    /// Transfer fee configuration for Token-2022 tokens.
    /// `None` for standard SPL tokens or EVM tokens.
    /// RugCheck field: `transferFee`.
    pub transfer_fee: Option<TransferFeeConfig>,

    /// Permanent delegate authority (Token-2022 extension). When set, this address
    /// can transfer or burn any token account's balance without the owner's consent.
    /// A major Solana scam vector since early 2026 (~40% of new tokens flagged by RugCheck).
    /// `None` for standard SPL tokens, EVM tokens, or Token-2022 tokens without the extension.
    /// RugCheck field: `permanentDelegate`.
    ///
    /// Added as pre-authorised DG2 in P2-5 implementation.
    /// Reference: docs/designs/0004-detector-01-honeypot.md §S3
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permanent_delegate: Option<Address>,

    /// Transfer hook program address (Token-2022 extension). When set, this program
    /// is invoked on every transfer and can revert the tx (legitimate fee/compliance
    /// hook, or malicious block-on-sell). `None` for standard SPL / EVM / Token-2022
    /// without the extension.
    /// RugCheck field: `transferHookProgramId`.
    ///
    /// Added as pre-authorised DG2 in P2-5 implementation.
    /// Reference: docs/designs/0004-detector-01-honeypot.md §S4
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub transfer_hook_program: Option<Address>,

    /// Token-2022 `NonTransferable` extension marker (discriminator 9).
    ///
    /// When `true`, the mint is structurally untransferable at the program level —
    /// every transfer attempt reverts. Legitimate for soulbound tokens, governance
    /// stakes, identity NFTs. D01 Signal A (freeze authority) weight is attenuated
    /// when this flag is set; D05 returns `InsufficientBaseline` (wash trading is
    /// structurally impossible by design).
    ///
    /// Pre-authorised additive extension (serde default false, SemVer-safe).
    /// Added in P6-2 action item #6. Populated by `token-registry` enrichment.
    #[serde(default)]
    pub non_transferable: bool,

    /// Token-2022 `ConfidentialTransferMint` extension marker (discriminator 4).
    ///
    /// When `true`, transfer amounts are ZK-encrypted — on-chain amounts appear as
    /// opaque ciphertexts. D05's wash-trading heuristic relies on observable amounts
    /// and returns `InsufficientBaseline` when this flag is set rather than silently
    /// treating the token as "no wash trading detected".
    ///
    /// Pre-authorised additive extension (serde default false, SemVer-safe).
    /// Added in P6-2 action item #7. Populated by `token-registry` enrichment.
    #[serde(default)]
    pub confidential_transfer: bool,

    // --- Holder distribution ---

    /// Top-N holders by balance. N is configurable; typically 20 (RugCheck default).
    /// RugCheck field: `topHolders`.
    pub top_holders: Vec<TopHolder>,

    /// Total number of token holder accounts at snapshot time.
    /// RugCheck field: `totalHolders`.
    pub total_holders: u64,

    // --- Market / LP data ---

    /// All known markets / DEX pools for this token.
    /// RugCheck field: `markets`.
    pub markets: Vec<MarketInfo>,

    /// Total market liquidity across all pools, in USD. Uses `Decimal`.
    pub total_market_liquidity_usd: Decimal,

    // --- LP lockers ---

    /// LP lock contracts holding liquidity for this token.
    /// RugCheck field: `lockers`.
    pub lockers: Vec<LockerInfo>,

    // --- Insider / bundler graph ---

    /// Whether the RugCheck insider-graph analysis detected bundler clusters.
    /// RugCheck field: `graphInsidersDetected`.
    pub graph_insiders_detected: bool,

    /// Grouped insider wallet networks flagged by RugCheck bundler analysis.
    /// RugCheck field: `insiderNetworks`.
    pub insider_networks: Vec<InsiderNetwork>,

    // --- Launch context ---

    /// Launchpad where the token originated (e.g., "pump.fun", "Raydium AMM").
    /// RugCheck field: `launchpad`.
    pub launchpad: Option<String>,

    /// Specific deploy platform, may differ from launchpad.
    /// RugCheck field: `deployPlatform`.
    pub deploy_platform: Option<String>,

    /// Timestamp when this token was first detected on-chain.
    /// RugCheck field: `detectedAt`.
    pub detected_at: Option<DateTime<Utc>>,

    // --- Ground-truth label ---

    /// `true` if RugCheck has flagged this token as rugged (post-hoc label).
    /// Used as the positive-class label in fixture corpus (ADR 0001 §D7).
    /// RugCheck field: `rugged`.
    pub rugged: bool,

    // --- Jupiter verification (negative-label filter) ---

    /// Jupiter exchange verification status.
    /// RugCheck field: `verification`.
    pub verification: JupiterVerification,

    // --- RugCheck risk score (for comparison only) ---

    /// Raw RugCheck score (0–1000). Stored for comparison/calibration only.
    /// Do NOT use as a detector output — use `AnomalyEvent.confidence` instead.
    pub rugcheck_score: Option<u32>,

    // --- Phase 4 reserved: EVM honeypot simulation fields ---
    // These will be `None` until Phase 4 EVM chains are activated.
    // Sourced from Honeypot.is `simulationResult` schema (live-verified 2026-04-21).

    /// Phase 4 reserved. Honeypot.is field: `simulationResult.buyTax`.
    /// Tax rate on buys as a percentage (0.0–100.0). Uses `Decimal`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub buy_tax: Option<Decimal>,

    /// Phase 4 reserved. Honeypot.is field: `simulationResult.sellTax`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sell_tax: Option<Decimal>,

    /// Phase 4 reserved. Honeypot.is field: `simulationResult.transferTax`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub transfer_tax: Option<Decimal>,

    /// Phase 4 reserved. Honeypot.is `flags[]` — revert reasons / risk categories.
    /// Examples: `"HoneypotSellBlock"`, `"HighSellTax"`, `"TransferPausable"`.
    #[serde(default)]
    pub honeypot_flags: Vec<String>,

    // --- Metadata freshness ---

    /// When this `TokenMeta` record was last refreshed from on-chain state.
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Supporting types for TokenMeta
// ---------------------------------------------------------------------------

/// Token-2022 transfer fee configuration.
///
/// From RugCheck `transferFee` field (live-verified). Contains the basis-points
/// fee rate, maximum fee amount, and the authority that can change it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferFeeConfig {
    /// Fee rate in basis points (1 bp = 0.01%). Range: 0–10_000.
    pub fee_bps: u16,

    /// Maximum fee in raw token units. Serialized as string.
    #[serde(
        serialize_with = "serialize_u128_as_str",
        deserialize_with = "deserialize_u128_from_str"
    )]
    pub max_fee_raw: u128,

    /// Authority that can update the transfer fee. `None` = revoked.
    pub authority: Option<Address>,
}

/// A single entry in the top-holders list.
///
/// Maps to RugCheck `topHolders[i]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopHolder {
    pub address: Address,

    /// Percentage of total supply held (0.0–100.0). Uses `Decimal`.
    pub pct: Decimal,

    /// Raw token balance. Serialized as string.
    #[serde(
        serialize_with = "serialize_u128_as_str",
        deserialize_with = "deserialize_u128_from_str"
    )]
    pub amount_raw: u128,

    /// Whether this is a known insider / deployer cluster wallet.
    pub is_insider: bool,
}

/// A DEX market / LP pool for a token.
///
/// Maps to RugCheck `markets[i]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketInfo {
    /// Pool / pair contract address.
    pub pool_address: Address,

    /// DEX protocol.
    pub dex: DexKind,

    /// Percentage of LP tokens burned (permanently locked). 0.0–100.0.
    /// RugCheck field: `lp_burned_pct` — kept snake_case to match RugCheck wire format.
    /// Per ADR 0001 §D6 and OQ1 resolution: this field lives on `MarketInfo`.
    #[serde(rename = "lp_burned_pct")]
    pub lp_burned_pct: Decimal,

    /// Current pool liquidity in USD. Uses `Decimal`.
    pub liquidity_usd: Decimal,

    /// Number of LP token holders (LPs providing liquidity).
    pub lp_provider_count: u64,
}

/// An LP lock contract holding tokens for this token's liquidity.
///
/// Maps to RugCheck `lockers[i]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockerInfo {
    /// The lock contract address.
    pub locker_address: Address,

    /// Human-readable locker name (e.g., "Unicrypt", "Team Finance").
    pub locker_name: Option<String>,

    /// Raw LP tokens locked. Serialized as string.
    #[serde(
        serialize_with = "serialize_u128_as_str",
        deserialize_with = "deserialize_u128_from_str"
    )]
    pub locked_amount_raw: u128,

    /// When the lock expires. `None` if permanent / no expiry.
    pub unlock_at: Option<DateTime<Utc>>,
}

/// A cluster of wallets flagged as likely coordinated insiders by the
/// RugCheck bundler-graph analysis.
///
/// Maps to RugCheck `insiderNetworks[i]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InsiderNetwork {
    /// Wallet addresses in this cluster.
    pub members: Vec<Address>,

    /// Percentage of total supply controlled by this cluster. Uses `Decimal`.
    pub supply_pct: Decimal,

    /// Whether this cluster appears to have coordinated buy/sell behavior.
    pub is_bundler: bool,
}

/// Jupiter verification flags for this token.
///
/// Maps to RugCheck `verification` field. Used as negative-class labels for
/// the fixture corpus (ADR 0001 §D7): `jup_verified` tokens are expected to be
/// non-rugged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JupiterVerification {
    /// Token appears in Jupiter's "verified" list.
    pub jup_verified: bool,

    /// Token appears in Jupiter's strict (curated) list.
    pub jup_strict: bool,
}

// ---------------------------------------------------------------------------
// HolderSnapshot
// ---------------------------------------------------------------------------

/// A periodic snapshot of the full holder distribution for a token.
///
/// Stored in ClickHouse (`holder_snapshots` table). The `is_full` flag
/// distinguishes a full snapshot of all holders from a delta snapshot that
/// contains only addresses whose balance changed since the previous snapshot.
///
/// **Delta semantics (OQ5 resolution):** The `is_full` field is present and the
/// struct supports both full snapshots and deltas. The ClickHouse storage merge
/// strategy (ReplacingMergeTree vs CollapsingMergeTree) will be determined by
/// the data-engineer in Task 4. The struct is intentionally agnostic to the
/// merge strategy — `balances` always contains the relevant addresses for the
/// given snapshot type.
///
/// **Determinism:** `balances` uses `BTreeMap<String, u128>` (address canonical
/// string → raw balance) so that iteration order is deterministic regardless of
/// ingestion order. Use the address canonical string (not `Address` struct) as
/// the key to keep this struct self-contained for serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HolderSnapshot {
    pub token: Address,
    pub chain: Chain,

    /// Block at which this snapshot was taken.
    pub block: BlockRef,

    /// Wall-clock time of the block.
    pub block_time: DateTime<Utc>,

    /// True = full snapshot of all holders. False = delta (only changed balances).
    pub is_full: bool,

    /// Holder address (canonical string) → raw balance.
    ///
    /// `BTreeMap` for deterministic ordering and serialization.
    /// For delta snapshots: only addresses with changed balances are included.
    /// A balance of `0` means the account was closed (balance went to zero).
    pub balances: BTreeMap<String, u128>,

    /// Total number of non-zero holder accounts at snapshot time.
    /// For delta snapshots, this is the running total (not just delta count).
    pub total_holders: u64,

    /// Pre-computed Gini coefficient for this snapshot. `None` for delta snapshots.
    /// Uses `Decimal`.
    pub gini: Option<Decimal>,

    /// Pre-computed top-10 holder percentage. `None` for delta snapshots.
    /// Uses `Decimal`.
    pub top10_pct: Option<Decimal>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::{Address, Chain};
    use chrono::Utc;
    use rust_decimal::Decimal;

    fn sol_mint() -> Address {
        Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112").unwrap()
    }

    fn make_minimal_token_meta() -> TokenMeta {
        TokenMeta {
            mint: sol_mint(),
            chain: Chain::Solana,
            symbol: Some("SOL".into()),
            name: Some("Wrapped SOL".into()),
            decimals: 9,
            token_program: None,
            total_supply_raw: 1_000_000_000_000_000,
            circulating_supply_raw: None,
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
            total_holders: 100_000,
            markets: vec![],
            total_market_liquidity_usd: Decimal::new(5_000_000, 0),
            lockers: vec![],
            graph_insiders_detected: false,
            insider_networks: vec![],
            launchpad: None,
            deploy_platform: None,
            detected_at: None,
            rugged: false,
            verification: JupiterVerification { jup_verified: true, jup_strict: true },
            rugcheck_score: Some(0),
            buy_tax: None,
            sell_tax: None,
            transfer_tax: None,
            honeypot_flags: vec![],
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn token_meta_serde_roundtrip() {
        let meta = make_minimal_token_meta();
        let json = serde_json::to_string(&meta).unwrap();
        // Phase 4 reserved fields should be absent from JSON when None / empty
        assert!(!json.contains("buyTax"));
        assert!(!json.contains("sellTax"));
        assert!(!json.contains("transferTax"));
        // total_supply_raw must be a string
        assert!(json.contains(r#""totalSupplyRaw":"#));
        assert!(json.contains(r#""1000000000000000""#));
    }

    #[test]
    fn token_meta_phase4_fields_default_absent() {
        // Verifies Phase 4 reserved fields don't break construction or serialization.
        let meta = make_minimal_token_meta();
        assert!(meta.buy_tax.is_none());
        assert!(meta.sell_tax.is_none());
        assert!(meta.transfer_tax.is_none());
        assert!(meta.honeypot_flags.is_empty());
    }

    /// P6-2: non_transferable and confidential_transfer default false,
    /// serde-skip absent from JSON (serde `default` without `skip_serializing_if`
    /// means they serialize as `false` — that is fine for boolean fields).
    #[test]
    fn token_meta_t22_marker_fields_default_false() {
        let meta = make_minimal_token_meta();
        assert!(!meta.non_transferable, "non_transferable must default to false");
        assert!(!meta.confidential_transfer, "confidential_transfer must default to false");
    }

    /// P6-2: non_transferable and confidential_transfer survive serde round-trip.
    #[test]
    fn token_meta_t22_marker_fields_serde_roundtrip() {
        let mut meta = make_minimal_token_meta();
        meta.non_transferable = true;
        meta.confidential_transfer = true;

        let json = serde_json::to_string(&meta).unwrap();
        let back: TokenMeta = serde_json::from_str(&json).unwrap();

        assert!(back.non_transferable, "non_transferable must survive serde round-trip");
        assert!(back.confidential_transfer, "confidential_transfer must survive serde round-trip");
    }

    /// P6-2: a JSON payload without the new fields deserializes with defaults (backward compat).
    #[test]
    fn token_meta_t22_marker_fields_backward_compat() {
        // Simulate a JSON blob from before P6-2 (no non_transferable / confidential_transfer keys).
        let meta = make_minimal_token_meta();
        let mut json_val = serde_json::to_value(&meta).unwrap();
        // Remove the new fields to simulate old serialised form.
        json_val.as_object_mut().unwrap().remove("nonTransferable");
        json_val.as_object_mut().unwrap().remove("confidentialTransfer");
        let back: TokenMeta = serde_json::from_value(json_val).unwrap();
        assert!(!back.non_transferable, "missing field must default to false");
        assert!(!back.confidential_transfer, "missing field must default to false");
    }

    #[test]
    fn holder_snapshot_balances_is_btreemap() {
        // Verify deterministic ordering: BTreeMap keys are sorted.
        let mut snapshot = HolderSnapshot {
            token: sol_mint(),
            chain: Chain::Solana,
            block: BlockRef::new(Chain::Solana, 1_000_000),
            block_time: Utc::now(),
            is_full: true,
            balances: BTreeMap::new(),
            total_holders: 3,
            gini: Some(Decimal::new(42, 2)),
            top10_pct: Some(Decimal::new(80, 2)),
        };
        // Insert in reverse order — BTreeMap will sort
        snapshot.balances.insert("zzz".into(), 100);
        snapshot.balances.insert("aaa".into(), 200);
        snapshot.balances.insert("mmm".into(), 150);

        let mut keys = snapshot.balances.keys();
        assert_eq!(keys.next().unwrap(), "aaa");
        assert_eq!(keys.next().unwrap(), "mmm");
        assert_eq!(keys.next().unwrap(), "zzz");
    }

    #[test]
    fn holder_snapshot_serde_roundtrip() {
        let mut balances = BTreeMap::new();
        balances.insert(
            "So11111111111111111111111111111111111111112".to_string(),
            500_000_000u128,
        );
        let snapshot = HolderSnapshot {
            token: sol_mint(),
            chain: Chain::Solana,
            block: BlockRef::new(Chain::Solana, 999_999),
            block_time: Utc::now(),
            is_full: false,
            balances,
            total_holders: 42,
            gini: None,
            top10_pct: None,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        // Balances values are u128 — serialized as plain JSON number here
        // (BTreeMap<String, u128> uses default serde, which serializes u128 as a number).
        // This is acceptable because map values are addressed via key lookup, not
        // appended to a JSON array where type confusion is a risk. A future phase
        // may switch to BTreeMap<String, String> if u128 overflow becomes relevant.
        assert!(json.contains("500000000"));
    }

    #[test]
    fn jupiter_verification_default() {
        let v = JupiterVerification::default();
        assert!(!v.jup_verified);
        assert!(!v.jup_strict);
    }

    #[test]
    fn transfer_fee_config_serde() {
        let fee = TransferFeeConfig {
            fee_bps: 100,
            max_fee_raw: 1_000_000,
            authority: None,
        };
        let json = serde_json::to_string(&fee).unwrap();
        assert!(json.contains(r#""feeBps":100"#));
        assert!(json.contains(r#""maxFeeRaw":"1000000""#));
    }
}
