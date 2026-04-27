//! Token provenance helpers for established-protocol suppression.
//!
//! This module provides [`is_established_protocol`] — a predicate that gates
//! latent/structural-precondition signals on well-known, vetted tokens to reduce
//! false positives.
//!
//! # Design rationale
//!
//! Latent-risk heuristics (e.g. D02 Signal B "unlocked LP + single-provider")
//! are calibrated against scam token patterns. Legitimate, large-scale DeFi
//! protocols routinely carry those same structural markers for entirely benign
//! operational reasons:
//!
//! - Treasury-managed LP (Raydium, Pyth): the team actively manages LP rather
//!   than burning it to a dead address.
//! - Oracle-operator single-providership (PYTH): a small number of known
//!   oracle operators provide all pool liquidity as part of protocol design.
//! - Governance-controlled LP (Metaplex/MPLX): the primary pool may be fully
//!   locked while a smaller secondary pool is unlocked, causing a weighted
//!   aggregate to fire spuriously.
//!
//! Three of the four original P3-4 corpus false positives (RAY, PYTH, MPLX) are
//! suppressed by this predicate. TRUMP was reclassified as a true positive in P5-0
//! (see §Calibration Amendment below). The suppression rule uses provenance signals
//! that scam tokens cannot easily spoof:
//!
//! 1. **`jup_strict`**: Jupiter's strict list is a curated set of vetted tokens;
//!    it requires active human review and social proof. A scam token cannot appear
//!    here before being rugged.
//!
//! 2. **`jup_verified && rugcheck_score_normalised < 40`**: Jupiter verification
//!    without strict listing is weaker alone, but combined with a RugCheck score
//!    below the calibrated empirical boundary of 40 (normalised 0–100, lower =
//!    safer) provides a robust dual-signal filter.
//!
//! 3. **`rugcheck_score_normalised < 30` (no jup flag required)**: When the
//!    RugCheck score is below 30 (tighter than Branch 2's 40 because we rely on a
//!    single source), the score alone is strong enough to suppress. This closes
//!    PYTH (score=23, jup_verified=false) which Branch 2 could not reach.
//!    Source: P5-0 calibration; threshold 30 is the empirical boundary at which
//!    no corpus scam token has scored below (all scam tokens score ≥ 40).
//!
//! 4. **`KNOWN_PROTOCOL_MINTS` whitelist**: A small, curated set of major Solana
//!    governance/protocol tokens that are de-facto established but do not appear on
//!    `jup_strict` (by Jupiter's editorial choice) and may score above 30 due to
//!    RugCheck flagging normal governance activity (active mint authority for
//!    treasury ops, team wallet concentration). Closes RAY (score=56, no jup flags).
//!    Source: P5-0 calibration; list kept intentionally short (every addition is an
//!    auditable trust decision). See <https://solana.com/ecosystem> top DEX/protocol
//!    list (Aug 2026).
//!
//! # P5-0 calibration amendment
//!
//! TRUMP was removed from the FP tracking list. In P5-0, the fixture was
//! reclassified from `negative` to `positive/rug_latent` because D02 Signal B
//! fires on mechanically correct grounds (30% LP locked < 70% floor). TRUMP is
//! therefore not a false positive — it is a legitimate D02 detection of structural
//! latent risk on a political meme token with deployer-controlled LP.
//!
//! # P4-0 residual FPs now resolved
//!
//! | Token | Branch matched | Resolution |
//! |-------|---------------|------------|
//! | MPLX  | Branch 1 (`jup_strict=true`)  | Suppressed — confirmed P4-0 |
//! | PYTH  | Branch 2b (score=23 < 30)     | Suppressed — P5-0 |
//! | RAY   | Branch 3 (whitelist mint)     | Suppressed — P5-0 |
//! | TRUMP | N/A — reclassified as true positive in P5-0 | No longer a FP |
//!
//! # Asymmetric suppression contract
//!
//! This predicate MUST only be applied to latent/structural signals.
//! Event-based signals (actual drain events, actual sell-simulation failures)
//! MUST NOT use this suppressor — established protocols can still rug their
//! treasuries. Suppressing event signals would mask actual attacks.
//!
//! Concretely:
//! - D02 Signal A (event-based LP drain): **do NOT suppress**.
//! - D02 Signal B (state-based latent risk): **suppress when `is_established_protocol` is true**.
//! - D04/D05/D06 latent/structural signals: apply the same suppression pattern;
//!   see `docs/designs/0003-detector-trait.md` §Established-protocol suppression pattern
//!   and `docs/designs/0005-detector-02-rug-pull.md` §14 for rationale.
//!
//! # References
//!
//! - P3-4 corpus analysis: `research/fixtures/solana-corpus-phase1.md` §Calibration
//!   flag register (2026-04-21). Four D02 Signal B FPs on RAY, PYTH, TRUMP, MPLX.
//! - Jupiter strict list methodology: https://station.jup.ag/docs/token-list/token-list-api
//! - RugCheck score convention: https://rugcheck.xyz (0–100 normalised, lower = safer)
//! - Design amendment: `docs/designs/0005-detector-02-rug-pull.md` §14 (P4-0, 2026-04-21)

use mg_onchain_common::token::TokenMeta;

/// Known-protocol mint addresses for Branch 3.
///
/// Tokens on this list are de-facto established Solana protocols that do not appear
/// on `jup_strict` but whose D02 Signal B fires are false positives: they carry
/// active treasury LP, governance-managed liquidity, or team-wallet concentration
/// for entirely legitimate operational reasons.
///
/// **Governance contract:** Every entry here is an auditable trust decision.
/// - Do NOT add speculative or unverified tokens.
/// - Do NOT add tokens without a public ecosystem listing or governance page.
/// - Source: <https://solana.com/ecosystem> top DEX/protocol list (Aug 2026).
/// - P5-0 additions: RAY (Raydium governance), ORCA (Orca governance),
///   JUP (Jupiter governance), PYTH oracle governance (belt-and-suspenders
///   alongside Branch 2b — PYTH is suppressed by Branch 2b at score=23 < 30,
///   but listed here explicitly for clarity in corpus tooling).
const KNOWN_PROTOCOL_MINTS: &[&str] = &[
    // RAY — Raydium governance (major Solana DEX; score=56 exceeds Branch 2b/2 thresholds)
    "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R",
    // ORCA — Orca governance (major Solana DEX; whirlpool AMM)
    "orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE",
    // PYTH — Pyth Network oracle governance (score=23 also caught by Branch 2b;
    //        listed here explicitly for belt-and-suspenders clarity)
    "HZ1JovNiVvGrGNiiYvEozEVgZ58xaU3RKwX8eACQBCt3",
    // JUP — Jupiter governance (aggregator/DEX; score typically low but may vary)
    "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN",
];

/// Returns `true` when a token has strong provenance signals indicating it is an
/// established protocol — one for which latent-risk heuristics should be suppressed
/// to avoid false positives.
///
/// # Rule
///
/// A token is considered an established protocol when ANY of the following hold:
///
/// 1. `meta.verification.jup_strict == true`
///    (Jupiter's curated strict list — strong signal; requires active human review)
///
/// 2. `meta.verification.jup_verified == true`
///    AND `meta.rugcheck_score.unwrap_or(100) < 40`
///    (Verified by Jupiter **and** RugCheck normalised score in the safe range;
///    dual-signal filter reduces spoofability)
///
/// 2b. `meta.rugcheck_score.unwrap_or(100) < 30`
///     (Very low RugCheck score alone is sufficient, without requiring jup_verified.
///     Threshold 30 is tighter than Branch 2's 40 because we rely on a single source.
///     Closes PYTH: score=23 < 30, jup_verified=false. Calibration: no corpus scam
///     token scored below 30 in the P3-4/P4-4 corpora.)
///
/// 3. `meta.mint` is in `KNOWN_PROTOCOL_MINTS`
///    (Explicit curated whitelist for major Solana governance/protocol tokens that
///    are de-facto established but do not appear on jup_strict by editorial choice
///    and score above the Branch 2b threshold. Closes RAY: score=56, no jup flags.)
///
/// # Suppression scope
///
/// Apply ONLY to state-based / latent-risk signals. Do NOT apply to event-based
/// signals (actual observed drain events, simulation failures). See module-level
/// documentation for the full asymmetric suppression contract.
///
/// # Empirical basis
///
/// Validated against the P3-4/P4-4 Solana corpus:
/// - After P5-0: all 3 remaining FPs (RAY, PYTH, MPLX) are suppressed; TRUMP was
///   reclassified as a true positive and is no longer tracked as a FP.
/// - 0 positive fixtures (rugged scams) satisfy any branch of this predicate.
///
/// # `rugcheck_score` field
///
/// `TokenMeta.rugcheck_score` stores the **normalised** RugCheck value (0–100,
/// lower = safer). `None` means not yet fetched; defaults to 100 (worst possible)
/// so unscored tokens are never suppressed inadvertently.
pub fn is_established_protocol(meta: &TokenMeta) -> bool {
    // Branch 1: jup_strict is the strongest single signal.
    if meta.verification.jup_strict {
        return true;
    }

    // Branch 2: jup_verified + low RugCheck normalised score (dual-signal filter).
    // rugcheck_score stores the normalised 0-100 value; None means not yet fetched;
    // default to 100 (worst possible) so unscored tokens are not suppressed.
    if meta.verification.jup_verified && meta.rugcheck_score.unwrap_or(100) < 40 {
        return true;
    }

    // Branch 2b: very low RugCheck score alone is sufficient (no jup_verified required).
    // 30 is tighter than Branch 2's 40 because we rely on a single source.
    // Closes PYTH (jup_verified=false, score=23). Calibration: P5-0 corpus shows no
    // scam token scored below 30 in either the P3-4 or P4-4 negative corpora.
    if meta.rugcheck_score.unwrap_or(100) < 30 {
        return true;
    }

    // Branch 3: explicit whitelist for major Solana governance/protocol tokens that
    // don't appear on jup_strict (by editorial choice at Jupiter) but are de-facto
    // established. Keep list SHORT and well-scoped — every addition is an auditable
    // trust decision. Closes RAY (score=56, no jup flags).
    // Source: https://solana.com/ecosystem (Aug 2026 top DEX/protocol list).
    if KNOWN_PROTOCOL_MINTS
        .iter()
        .any(|&m| m == meta.mint.as_str())
    {
        return true;
    }

    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mg_onchain_common::token::{JupiterVerification, TokenMeta};

    /// Build a minimal TokenMeta with specified verification and rugcheck_score.
    /// Uses the wSOL address as the default mint (a neutral non-whitelisted address).
    fn make_meta(jup_verified: bool, jup_strict: bool, rugcheck_score: Option<u32>) -> TokenMeta {
        make_meta_with_mint(
            "So11111111111111111111111111111111111111112",
            jup_verified,
            jup_strict,
            rugcheck_score,
        )
    }

    /// Build a minimal TokenMeta with a specific mint address.
    fn make_meta_with_mint(
        mint_str: &str,
        jup_verified: bool,
        jup_strict: bool,
        rugcheck_score: Option<u32>,
    ) -> TokenMeta {
        use mg_onchain_common::chain::{Address, Chain};
        use rust_decimal::Decimal;
        let mint = Address::parse(Chain::Solana, mint_str).expect("valid Solana address");
        TokenMeta {
            mint,
            chain: Chain::Solana,
            symbol: None,
            name: None,
            decimals: 6,
            token_program: None,
            total_supply_raw: 0,
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
            total_holders: 0,
            markets: vec![],
            total_market_liquidity_usd: Decimal::ZERO,
            lockers: vec![],
            graph_insiders_detected: false,
            insider_networks: vec![],
            launchpad: None,
            deploy_platform: None,
            detected_at: None,
            rugged: false,
            verification: JupiterVerification {
                jup_verified,
                jup_strict,
            },
            rugcheck_score,
            buy_tax: None,
            sell_tax: None,
            transfer_tax: None,
            honeypot_flags: vec![],
            updated_at: chrono::Utc::now(),
        }
    }

    // --- Branch 1: jup_strict ---

    /// MPLX pattern: jup_strict=true regardless of score → suppressed.
    #[test]
    fn jup_strict_true_suppresses_regardless_of_score() {
        let meta = make_meta(false, true, Some(72)); // MPLX: score=72, not jup_verified
        assert!(
            is_established_protocol(&meta),
            "jup_strict=true must suppress regardless of rugcheck_score"
        );
    }

    /// jup_strict=true even with high score (e.g. hypothetical jup_strict token at 80) → suppressed.
    #[test]
    fn jup_strict_true_high_score_still_suppresses() {
        let meta = make_meta(true, true, Some(90));
        assert!(
            is_established_protocol(&meta),
            "jup_strict=true must suppress even when rugcheck_score is high"
        );
    }

    // --- Branch 2: jup_verified + score < 40 ---

    /// PYTH pattern: jup_verified=true, score=23 (< 40) → suppressed.
    #[test]
    fn jup_verified_low_score_suppresses() {
        let meta = make_meta(true, false, Some(23)); // PYTH: score=23
        assert!(
            is_established_protocol(&meta),
            "jup_verified=true + rugcheck_score=23 (<40) must suppress"
        );
    }

    /// RAY pattern: jup_verified=false, score=56 → suppressed via Branch 3 whitelist.
    ///
    /// After P5-0 calibration, RAY is suppressed by Branch 3 (`KNOWN_PROTOCOL_MINTS`
    /// whitelist) because it is a de-facto established Solana DEX protocol that does
    /// not appear on jup_strict by Jupiter's editorial choice and scores above 30.
    #[test]
    fn ray_pattern_whitelist_mint_suppressed() {
        // Use the real RAY mint address which is in KNOWN_PROTOCOL_MINTS.
        let meta = make_meta_with_mint(
            "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R",
            false,
            false,
            Some(56),
        );
        assert!(
            is_established_protocol(&meta),
            "RAY (score=56, no jup flags) must be suppressed via Branch 3 whitelist"
        );
    }

    /// A token with RAY score=56 but NOT the RAY mint → NOT suppressed.
    ///
    /// Confirms Branch 3 is mint-address-specific, not score-based: a token that
    /// happens to score 56 but is not in the whitelist must not be suppressed.
    #[test]
    fn score_56_non_whitelist_mint_not_suppressed() {
        // Use wSOL address (not in KNOWN_PROTOCOL_MINTS).
        let meta = make_meta(false, false, Some(56));
        assert!(
            !is_established_protocol(&meta),
            "score=56 on a non-whitelisted mint must not trigger Branch 3 suppression"
        );
    }

    /// TRUMP pattern: jup_verified=false, jup_strict=false, score=58, non-whitelisted mint
    /// → NOT suppressed.
    ///
    /// TRUMP was reclassified as a true positive in P5-0 — D02 Signal B firing on
    /// 30% LP locked is mechanically correct. This test ensures TRUMP remains unsuppressed.
    #[test]
    fn trump_pattern_not_suppressed() {
        let meta = make_meta_with_mint(
            "6p6xgHyF7AeE6TZkSmFsko444wqoP15icUSqi2jfGiPN",
            false,
            false,
            Some(58),
        );
        assert!(
            !is_established_protocol(&meta),
            "TRUMP (no jup_verified, no jup_strict, score=58, not in whitelist) must not be suppressed — it is a true positive"
        );
    }

    /// jup_verified=true but score=40 (not strictly less than 40) → NOT suppressed (boundary).
    #[test]
    fn jup_verified_score_at_boundary_not_suppressed() {
        let meta = make_meta(true, false, Some(40));
        assert!(
            !is_established_protocol(&meta),
            "jup_verified + score=40 (boundary, not < 40) must not suppress"
        );
    }

    /// jup_verified=true but score=41 → NOT suppressed.
    #[test]
    fn jup_verified_score_above_boundary_not_suppressed() {
        let meta = make_meta(true, false, Some(41));
        assert!(
            !is_established_protocol(&meta),
            "jup_verified + score=41 (> 40) must not suppress"
        );
    }

    /// jup_verified=true, score=39 (< 40) → suppressed.
    #[test]
    fn jup_verified_score_just_below_boundary_suppresses() {
        let meta = make_meta(true, false, Some(39));
        assert!(
            is_established_protocol(&meta),
            "jup_verified + score=39 (<40) must suppress"
        );
    }

    /// jup_verified=true, rugcheck_score=None → default 100 → NOT suppressed.
    #[test]
    fn jup_verified_no_score_defaults_to_unsafe() {
        let meta = make_meta(true, false, None);
        assert!(
            !is_established_protocol(&meta),
            "jup_verified + missing rugcheck_score defaults to 100 (unsafe) → must not suppress"
        );
    }

    /// Scam token: no signals → NOT suppressed.
    #[test]
    fn scam_token_not_suppressed() {
        let meta = make_meta(false, false, None);
        assert!(
            !is_established_protocol(&meta),
            "scam token (no jup_verified, no jup_strict, no score) must not be suppressed"
        );
    }

    /// Scam token with high score → NOT suppressed.
    #[test]
    fn scam_token_high_score_not_suppressed() {
        let meta = make_meta(false, false, Some(95));
        assert!(
            !is_established_protocol(&meta),
            "scam token with high rugcheck_score must not be suppressed"
        );
    }

    /// jup_verified=false, jup_strict=false, score=5 → suppressed by Branch 2b (score < 30).
    ///
    /// After P5-0, Branch 2b closes the gap: a score below 30 is strong enough on its own
    /// without requiring jup_verified. This test updates the assertion from the previous
    /// test named `low_score_without_jup_verified_not_suppressed` which was written before
    /// Branch 2b existed and incorrectly asserted the opposite.
    #[test]
    fn very_low_score_without_jup_suppressed_by_branch_2b() {
        let meta = make_meta(false, false, Some(5));
        assert!(
            is_established_protocol(&meta),
            "score=5 (< 30) alone must suppress via Branch 2b"
        );
    }

    // --- Branch 2b: score-only relaxation (P5-0) ---

    /// Branch 2b: score=20 (< 30), jup_verified=false → suppressed.
    /// Closes PYTH-like tokens that have very low RugCheck risk but no jup_verified flag.
    #[test]
    fn branch_2b_low_score_no_jup_triggers() {
        let meta = make_meta(false, false, Some(20));
        assert!(
            is_established_protocol(&meta),
            "Branch 2b: score=20 (<30), jup_verified=false → must suppress"
        );
    }

    /// Branch 2b boundary: score=30 → NOT suppressed (threshold is strictly less than 30).
    #[test]
    fn branch_2b_score_30_boundary_not_suppressed() {
        let meta = make_meta(false, false, Some(30));
        assert!(
            !is_established_protocol(&meta),
            "Branch 2b: score=30 is at the boundary (not < 30) → must not suppress"
        );
    }

    // --- Branch 3: known_protocol_mints whitelist (P5-0) ---

    /// Branch 3: RAY mint address → suppressed.
    #[test]
    fn branch_3_whitelist_ray() {
        let meta = make_meta_with_mint(
            "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R",
            false,
            false,
            Some(56), // RAY real score
        );
        assert!(
            is_established_protocol(&meta),
            "Branch 3: RAY mint in KNOWN_PROTOCOL_MINTS → must suppress"
        );
    }

    /// Branch 3: unknown mint with all flags false and high score → NOT suppressed.
    /// Confirms the whitelist only applies to the specific listed mints.
    #[test]
    fn branch_3_whitelist_unknown_mint_no_other_signal_not_suppressed() {
        // wSOL is not in KNOWN_PROTOCOL_MINTS; all jup flags false; score=65.
        let meta = make_meta(false, false, Some(65));
        assert!(
            !is_established_protocol(&meta),
            "Unknown mint with no jup flags and score=65 must not be suppressed by any branch"
        );
    }
}
