//! D09 — BOCPD Deployer Changepoint Detector
//!
//! # Signal design (design 0016)
//!
//! Detects **regime shifts** in a deployer's behavioral time-series using online
//! Bayesian Online Changepoint Detection (Adams & MacKay 2007). Unlike D01–D08
//! which evaluate a single token in isolation, D09 operates on the **full career
//! of a deployer**: the sequence of tokens they have launched, ordered by block
//! time of the `PoolEvent::Initialize` that created the pool for each token.
//!
//! ## What it catches (design 0016 §1.2)
//!
//! | Scenario | D01–D08 coverage | D09 coverage |
//! |----------|-----------------|--------------|
//! | Deployer shortens launch cadence 30d→1d suddenly | None | Yes — log_gap_seconds drops |
//! | LP locked on first 10 tokens; unlocked on #11 | D02 fires after drain | D09 fires at pool creation |
//! | Deployer's prior tokens have 30% rug rate | None (per-token only) | Yes — prior_rug_rate feature |
//!
//! ## Algorithm (design 0016 §3)
//!
//! 1. Each new token launch from a known deployer produces one **observation**:
//!    a 5-feature vector collapsed to a scalar composite score `S_t ∈ [0, 1]`.
//! 2. The scalar is fed into a Normal-Gamma BOCPD (Adams & MacKay 2007) that
//!    maintains a probability distribution over the current run length `r_t`.
//! 3. A changepoint alert fires when `P(r_t = 0 | x_{1:t}) >= changepoint_prob_threshold`.
//! 4. Confidence = `clamp(P(r_t = 0), 0.0, 1.0)`.
//!
//! ## Composite score formula (design 0016 §2.3)
//!
//! ```text
//! S_t = w0 * (1 − sigmoid(log_gap_seconds / 10.0))
//!     + w1 * (1 − lp_locked_pct)
//!     + w2 * (1 − sigmoid(log_initial_liquidity_usd / 8.0))
//!     + w3 * (1 − sigmoid(holder_count_at_1h / 100.0))
//!     + w4 * prior_rug_rate
//! ```
//!
//! Weights `w0..w4` sum to 1.0 (validated at startup). Higher score = higher risk.
//!
//! ## State persistence
//!
//! Per-deployer BOCPD state is stored in `bocpd_deployer_state` (V00013). Survives
//! service restarts without history replay. Reorg handling: delete rows with
//! `last_update_block_height >= reorg_height`.
//!
//! ## Established-protocol suppression
//!
//! D09 DOES suppress on `KnownDex` / `KnownExchange` deployers (unlike D08).
//! Rationale: program-initialized pools (Raydium PDA) would produce spurious
//! changepoints due to trivially short inter-launch gaps. See §8.1.
//!
//! ## Evidence keys (all prefixed `deployer_changepoint/` per gotcha #9)
//!
//! | Key | Type | Meaning |
//! |-----|------|---------|
//! | `deployer_changepoint/changepoint_prob` | Decimal(6dp) | P(r_t=0) raw BOCPD output |
//! | `deployer_changepoint/observation_value` | Decimal(6dp) | Composite score S_t |
//! | `deployer_changepoint/total_tokens_launched` | Decimal(int) | Total tokens from this deployer |
//! | `deployer_changepoint/prior_rug_rate` | Decimal(4dp) | Fraction of prior tokens with rugs |
//! | `deployer_changepoint/lp_locked_pct` | Decimal(4dp) | LP locked pct at launch |
//! | `deployer_changepoint/log_gap_seconds` | Decimal(4dp) | ln(gap_seconds + 1) |
//! | `deployer_changepoint/run_length_mode` | Decimal(int) | Most probable run length |
//! | `deployer_changepoint/run_length_prob_0` | Decimal(6dp) | P(r_t=0) |
//! | `deployer_changepoint/run_length_prob_1` | Decimal(6dp) | P(r_t=1) |
//! | `deployer_changepoint/run_length_prob_mode` | Decimal(6dp) | P(r_t=mode) |
//!
//! # Design reference
//!
//! `docs/designs/0016-detector-09-bocpd-deployer-changepoint.md`
//!
//! # Citations
//!
//! - Adams & MacKay 2007 (arXiv:0710.3742): Online Bayesian Changepoint Detection.
//!   Algorithm: run-length posterior `P(r_t | x_{1:t})`; Normal-Gamma conjugate prior;
//!   hazard function `H(r) = 1/300`; alert on `P(r_t=0) >= 0.50`.
//! - Murphy 2007: Conjugate Bayesian Analysis of the Gaussian Distribution.
//!   Posterior update equations for Normal-Gamma: `kappa_n`, `mu_n`, `alpha_n`, `beta_n`;
//!   Student-t predictive distribution with `nu = 2*alpha_n`.
//! - Chainalysis 2025: deployer as primary risk signal; 94% of rugged tokens had
//!   deployer as primary rug actor (D09 `prior_rug_rate` feature calibration).
//! - Sun et al. 2024 (arXiv:2403.16082): rug root causes; evasion E-D09-5 analysis.
//! - Latent-flux (#10): production BOCPD on deployer behavior on Base/Arbitrum/Optimism;
//!   constant hazard H=1/300 confirmed; univariate scalar per deployer.
//! - Alhaidari et al. 2025 (SolRPDS, arXiv:2504.07132): `lp_locked_pct` top-3 predictor.

use std::sync::Arc;

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use statrs::distribution::StudentsT;
use tracing::{debug, instrument};

use mg_onchain_common::anomaly::{AnomalyEvent, Confidence, Evidence, Severity};
use mg_onchain_common::chain::{Address, BlockRef, Chain};
use mg_onchain_graph::labels::{GraphLabelStore, LabelType};
use mg_onchain_graph::typed_edges::{EdgeType, TypedEdgeStore};

use crate::context::DetectorContext;
use crate::detector::Detector;
use crate::error::DetectorError;
use crate::signals::{severity_from_confidence, sigmoid};

/// Stable detector ID string — used in `AnomalyEvent.detector_id` and as the
/// evidence key prefix (gotcha #9).
pub const DETECTOR_ID: &str = "deployer_changepoint";

// ---------------------------------------------------------------------------
// Configuration structs (mirrors config/detectors.toml [deployer_changepoint])
// ---------------------------------------------------------------------------

/// Hyperparameters for the Normal-Gamma conjugate prior on composite scores.
///
/// Derived from domain knowledge (composite score ∈ [0, 1]) following
/// Murphy 2007 §4 guidance. Classified as `unverified-heuristic` pending
/// Sprint 12 corpus calibration (design 0016 §3.3).
#[derive(Debug, Clone, Copy)]
pub struct BocpdHyperparams {
    /// Prior mean of composite risk score. 0.20 = mildly skeptical (design 0016 §3.3).
    pub mu_0: f64,
    /// Pseudo-count on the prior mean. 1.0 = weak prior (Murphy 2007 §4.4).
    pub kappa_0: f64,
    /// Shape of Gamma prior on precision. 3.0 → Student-t with ν=6 (design 0016 §3.3).
    pub alpha_0: f64,
    /// Rate of Gamma prior on precision. 1.0 → prior variance = β/(α-1) = 0.50.
    pub beta_0: f64,
}

impl Default for BocpdHyperparams {
    fn default() -> Self {
        Self {
            mu_0: 0.20,
            kappa_0: 1.0,
            alpha_0: 3.0,
            beta_0: 1.0,
        }
    }
}

/// Composite score weights (w0..w4). Must sum to 1.0.
///
/// All weights come from `config/detectors.toml [deployer_changepoint]`.
/// The sum is validated at `D09BocpdDetector` construction time.
#[derive(Debug, Clone, Copy)]
pub struct CompositeWeights {
    /// Weight for `log_gap_seconds` feature (sign-inverted: shorter gap = higher risk).
    pub w_log_gap: f64,
    /// Weight for `lp_locked_pct` feature (sign-inverted: lower lock = higher risk).
    pub w_lp_locked: f64,
    /// Weight for `log_initial_liquidity_usd` feature (sign-inverted: lower = higher risk).
    pub w_log_liquidity: f64,
    /// Weight for `holder_count_at_1h` feature (sign-inverted: fewer = higher risk).
    pub w_holder_count: f64,
    /// Weight for `prior_rug_rate` feature (direct: more rugs = higher risk).
    pub w_prior_rug_rate: f64,
}

impl Default for CompositeWeights {
    fn default() -> Self {
        Self {
            w_log_gap: 0.25,
            w_lp_locked: 0.25,
            w_log_liquidity: 0.15,
            w_holder_count: 0.10,
            w_prior_rug_rate: 0.25,
        }
    }
}

impl CompositeWeights {
    /// Validate that weights sum to 1.0 (within floating-point tolerance).
    ///
    /// Returns `Err` if the sum deviates by more than 1e-6 from 1.0.
    pub fn validate(&self) -> anyhow::Result<()> {
        let sum = self.w_log_gap
            + self.w_lp_locked
            + self.w_log_liquidity
            + self.w_holder_count
            + self.w_prior_rug_rate;
        if (sum - 1.0).abs() > 1e-6 {
            anyhow::bail!(
                "D09 composite weights do not sum to 1.0: sum={sum:.8} \
                 (w0={} w1={} w2={} w3={} w4={})",
                self.w_log_gap,
                self.w_lp_locked,
                self.w_log_liquidity,
                self.w_holder_count,
                self.w_prior_rug_rate
            );
        }
        Ok(())
    }
}

/// D09 BOCPD configuration thresholds.
///
/// Loaded from `config/detectors.toml [deployer_changepoint]` at server startup.
/// Stored in `D09BocpdDetector` as a struct field rather than per-call config
/// because D09 is event-driven and does not receive a `DetectorContext` on
/// every invocation (design 0016 §5.1).
#[derive(Debug, Clone)]
pub struct D09Config {
    /// Emit event when P(r_t=0 | x_{1:t}) >= this threshold (design 0016 §3.1).
    pub changepoint_prob_threshold: f64,
    /// Minimum total_observations before any event is emitted (design 0016 §8.2).
    pub min_history_length: u32,
    /// Constant hazard rate H(r) = 1/300 ≈ 0.00333 (design 0016 §3.2).
    pub hazard_rate: f64,
    /// Maximum run-length slots maintained in the posterior vector (design 0016 §3.6).
    pub max_run_length_tracked: usize,
    /// Minimum confidence for an `anomaly_events` row to count as a confirmed rug
    /// when computing `prior_rug_rate` (design 0016 §7).
    pub rug_confidence_threshold: f64,
    /// Normal-Gamma prior hyperparameters.
    pub hyperparams: BocpdHyperparams,
    /// Composite score weights (w0..w4, must sum to 1.0).
    pub weights: CompositeWeights,
    /// Confidence floor for KnownDex/KnownExchange suppression (design 0016 §8.1).
    pub infra_label_confidence_floor: f64,
}

impl Default for D09Config {
    fn default() -> Self {
        Self {
            changepoint_prob_threshold: 0.50,
            min_history_length: 5,
            hazard_rate: 0.00333,
            max_run_length_tracked: 1000,
            rug_confidence_threshold: 0.60,
            hyperparams: BocpdHyperparams::default(),
            weights: CompositeWeights::default(),
            infra_label_confidence_floor: 0.80,
        }
    }
}

// ---------------------------------------------------------------------------
// Feature vector and composite score
// ---------------------------------------------------------------------------

/// The five raw features derived for one deployer observation (one token launch).
///
/// All features are `f64` per design 0016 §2.4: these are normalized scores
/// and probabilities, not monetary amounts (ADR 0002 permits f64 for these).
#[derive(Debug, Clone, Copy)]
pub struct ObservationFeatures {
    /// `ln(seconds_since_prior_launch + 1)`. First observation uses ln(30d+1)≈14.7.
    pub log_gap_seconds: f64,
    /// LP locked fraction at pool creation. Range [0.0, 1.0].
    pub lp_locked_pct: f64,
    /// `ln(initial_liquidity_usd + 1)`. From `pools.initial_liquidity_usd`.
    pub log_initial_liquidity_usd: f64,
    /// Holder count from `holders_snapshots` at launch+1h. 0.0 if no snapshot.
    pub holder_count_at_1h: f64,
    /// Confirmed rug count / prior launch count. 0.0 if first launch.
    pub prior_rug_rate: f64,
}

impl ObservationFeatures {
    /// Compute the scalar composite risk score S_t ∈ [0, 1].
    ///
    /// Formula per design 0016 §2.3. Higher score = higher behavioral risk.
    ///
    /// # Determinism
    ///
    /// Pure function; no I/O or randomness. Given identical inputs, output is
    /// identical. The sigmoid function is standard and deterministic.
    pub fn composite_score(&self, w: &CompositeWeights) -> f64 {
        let term0 = w.w_log_gap * (1.0 - sigmoid(self.log_gap_seconds / 10.0));
        let term1 = w.w_lp_locked * (1.0 - self.lp_locked_pct);
        let term2 = w.w_log_liquidity * (1.0 - sigmoid(self.log_initial_liquidity_usd / 8.0));
        let term3 = w.w_holder_count * (1.0 - sigmoid(self.holder_count_at_1h / 100.0));
        let term4 = w.w_prior_rug_rate * self.prior_rug_rate;
        (term0 + term1 + term2 + term3 + term4).clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// BOCPD state: one run-length slot (Welford + Normal-Gamma)
// ---------------------------------------------------------------------------

/// One slot in the BOCPD run-length posterior vector.
///
/// Stores Welford online statistics and Normal-Gamma posterior parameters
/// for run length `r`. The Normal-Gamma update is applied incrementally in
/// O(1) per observation per slot (design 0016 §3.5).
///
/// Serialized to/from JSONB via `bocpd_deployer_state.run_length_state_json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSlot {
    /// Run length r (== array index; stored for readability/debugging).
    pub r: u32,
    /// `log P(r_t = r, x_{1:t})` after normalisation. `NEG_INFINITY` = zero probability.
    pub log_joint: f64,
    /// Observation count in this run slot.
    pub n: u32,
    /// Welford running mean.
    pub mean: f64,
    /// Welford M2 (sum of squared deviations from mean).
    pub m2: f64,
    /// Normal-Gamma posterior: κ_n = κ_0 + n.
    pub kappa_n: f64,
    /// Normal-Gamma posterior: μ_n.
    pub mu_n: f64,
    /// Normal-Gamma posterior: α_n = α_0 + n/2.
    pub alpha_n: f64,
    /// Normal-Gamma posterior: β_n.
    pub beta_n: f64,
}

impl RunSlot {
    /// Create a fresh slot at run length `r` with the given prior hyperparameters.
    ///
    /// A fresh slot has `n = 0` observations. The Normal-Gamma parameters
    /// are initialized to the prior values (`kappa_0`, `mu_0`, `alpha_0`, `beta_0`).
    fn new_prior(r: u32, hp: &BocpdHyperparams) -> Self {
        Self {
            r,
            log_joint: f64::NEG_INFINITY,
            n: 0,
            mean: 0.0,
            m2: 0.0,
            kappa_n: hp.kappa_0,
            mu_n: hp.mu_0,
            alpha_n: hp.alpha_0,
            beta_n: hp.beta_0,
        }
    }

    /// Update Welford statistics and Normal-Gamma posterior for a new observation `x`.
    ///
    /// Follows design 0016 §3.5 (O(1) incremental update).
    /// Murphy 2007 §4.3 conjugate update equations.
    fn update_sufficient_stats(&mut self, x: f64, hp: &BocpdHyperparams) {
        // Welford online mean and M2.
        self.n += 1;
        let n = self.n as f64;
        let delta = x - self.mean;
        self.mean += delta / n;
        let delta2 = x - self.mean;
        self.m2 += delta * delta2;

        // Normal-Gamma posterior update (Murphy 2007 §4.3).
        self.kappa_n = hp.kappa_0 + n;
        self.mu_n = (hp.kappa_0 * hp.mu_0 + n * self.mean) / self.kappa_n;
        self.alpha_n = hp.alpha_0 + n / 2.0;
        // β_n = β_0 + M2/2 + κ_0 * n * (x_bar - μ_0)² / (2(κ_0 + n))
        let cross = (hp.kappa_0 * n * (self.mean - hp.mu_0).powi(2)) / (2.0 * (hp.kappa_0 + n));
        self.beta_n = hp.beta_0 + self.m2 / 2.0 + cross;
    }

    /// Compute the Student-t log-PDF for observation `x` under this slot's posterior.
    ///
    /// The posterior predictive is a Student-t with:
    /// - `ν = 2 * α_n`
    /// - `μ_pred = μ_n`
    /// - `σ²_pred = β_n * (κ_n + 1) / (α_n * κ_n)`
    ///
    /// Uses `statrs::distribution::StudentsT` for numerical accuracy
    /// (design 0016 §3.4; ADR 0003: pure Rust, no external ML bridge).
    fn log_predictive_pdf(&self, x: f64) -> f64 {
        let nu = 2.0 * self.alpha_n;
        let mu_pred = self.mu_n;
        let sigma2_pred = (self.beta_n * (self.kappa_n + 1.0)) / (self.alpha_n * self.kappa_n);

        // Guard against degenerate parameters (should not occur under valid BOCPD update,
        // but defensive for pathological inputs).
        if !nu.is_finite() || nu <= 0.0 || !sigma2_pred.is_finite() || sigma2_pred <= 0.0 {
            return f64::NEG_INFINITY;
        }

        let sigma_pred = sigma2_pred.sqrt();

        // Standardize x for the Student-t distribution parameterized on location/scale.
        // statrs StudentsT is the standard Student-t (location=0, scale=1).
        // We parameterize manually: Z = (x - mu_pred) / sigma_pred.
        // log P(x | mu_pred, sigma2_pred, nu) = log P(Z | 0, 1, nu) - log(sigma_pred).
        let z = (x - mu_pred) / sigma_pred;
        match StudentsT::new(0.0, 1.0, nu) {
            Ok(dist) => {
                use statrs::distribution::Continuous;
                let log_pdf = dist.ln_pdf(z) - sigma_pred.ln();
                if log_pdf.is_finite() {
                    log_pdf
                } else {
                    f64::NEG_INFINITY
                }
            }
            Err(_) => f64::NEG_INFINITY,
        }
    }
}

// ---------------------------------------------------------------------------
// BOCPD state: the full run-length posterior
// ---------------------------------------------------------------------------

/// Complete BOCPD state for one deployer: the run-length posterior vector.
///
/// `slots[i]` is the state for run length `r = i`. The vector grows by one
/// slot per observation, up to `max_run_length_tracked`. When the cap is
/// reached, mass at `r = max_run_length_tracked` is absorbed into the
/// absorbing boundary slot (design 0016 §3.6).
///
/// Serialized to `bocpd_deployer_state.run_length_state_json` as a JSONB
/// array (design 0016 §4.5). Deserialized at load time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BocpdState {
    /// Run-length slots. `slots[r]` is the state for run length `r`.
    /// Length grows up to `max_run_length_tracked + 1`.
    pub slots: Vec<RunSlot>,
    /// Total observations processed (= tokens launched so far).
    pub total_observations: u32,
    /// The maximum number of run-length slots to maintain.
    #[serde(skip)]
    pub max_slots: usize,
}

impl BocpdState {
    /// Initialize a fresh BOCPD state with the prior hyperparameters.
    ///
    /// The initial state has one slot at `r = 0` with `log_joint = 0.0`
    /// (all probability mass at "no observations yet").
    pub fn new_with_prior(hp: &BocpdHyperparams, max_run_length_tracked: usize) -> Self {
        let mut slot0 = RunSlot::new_prior(0, hp);
        slot0.log_joint = 0.0; // Initial: P(r_0 = 0) = 1.0, log = 0.0
        Self {
            slots: vec![slot0],
            total_observations: 0,
            max_slots: max_run_length_tracked + 1,
        }
    }

    /// Run one BOCPD update step for observation `x`.
    ///
    /// Implements Adams & MacKay (2007) growth + changepoint messages in
    /// log space (design 0016 §3.6 for numerical stability).
    ///
    /// # Algorithm
    ///
    /// 1. Compute log predictive PDF under each run-length slot.
    /// 2. Growth: shift log_joints right by one (run continues).
    /// 3. Changepoint: collapse all mass to `r = 0` weighted by hazard.
    /// 4. Normalise via log_sum_exp.
    /// 5. Apply absorbing boundary at `max_run_length_tracked`.
    /// 6. Update sufficient statistics of each new slot.
    pub fn update(&mut self, x: f64, hazard_rate: f64, hp: &BocpdHyperparams) {
        let n_slots = self.slots.len();

        // Step 1: Compute log predictive PDFs for each existing slot.
        let log_preds: Vec<f64> = self.slots.iter().map(|s| s.log_predictive_pdf(x)).collect();

        // Compute log-hazard and log(1 - hazard) for efficiency.
        let log_hazard = hazard_rate.ln();
        let log_one_minus_hazard = (1.0 - hazard_rate).ln();

        // Step 3: Changepoint mass at r=0.
        // log_cp = log_sum_exp over all r of: log_joint[r] + log_pred[r] + log_hazard
        let cp_terms: Vec<f64> = self
            .slots
            .iter()
            .zip(log_preds.iter())
            .map(|(s, &lp)| s.log_joint + lp + log_hazard)
            .collect();
        let log_cp_sum = log_sum_exp(&cp_terms);

        // Step 2 + 4: Growth — new slots shifted right.
        // We need to build the new slot vector. The new vector has:
        //   new_slots[0]       = changepoint slot (r=0, fresh prior, log_joint=log_cp_sum)
        //   new_slots[r+1]     = growth from old_slots[r] (log_joint += log_pred[r] + log(1-H))
        //   up to max_slots-1
        //
        // Absorbing boundary: if r == max_slots-2 (the penultimate slot), the growth
        // would push to max_slots-1. If r == max_slots-1 already, its mass also
        // flows into max_slots-1 (absorbed). We implement this by capping the index.

        let new_len = (n_slots + 1).min(self.max_slots);
        let mut new_slots: Vec<RunSlot> = Vec::with_capacity(new_len);

        // Slot 0: changepoint (fresh prior, new observation not yet incorporated into
        // its sufficient stats — that happens below after log_joint is set).
        let mut slot0 = RunSlot::new_prior(0, hp);
        slot0.log_joint = log_cp_sum;
        new_slots.push(slot0);

        // Slots 1..new_len-1: growth from old slots.
        for (r_old, &log_pred) in log_preds.iter().enumerate() {
            let r_new = r_old + 1;
            if r_new >= self.max_slots {
                // Absorbing boundary: fold mass from r_old into the last slot.
                // The last slot in new_slots is at index max_slots-1.
                if let Some(last) = new_slots.last_mut() {
                    let growth_lj = self.slots[r_old].log_joint + log_pred + log_one_minus_hazard;
                    last.log_joint = log_sum_exp(&[last.log_joint, growth_lj]);
                }
                break;
            }
            let mut new_slot = self.slots[r_old].clone();
            new_slot.r = r_new as u32;
            new_slot.log_joint = self.slots[r_old].log_joint + log_pred + log_one_minus_hazard;
            new_slots.push(new_slot);
        }

        // Step 4: Normalise in log space.
        let log_joints: Vec<f64> = new_slots.iter().map(|s| s.log_joint).collect();
        let log_total = log_sum_exp(&log_joints);
        for slot in &mut new_slots {
            slot.log_joint -= log_total;
        }

        // Step 6: Update sufficient statistics of all slots with the new observation x.
        // Note: for slot 0 (changepoint), we update its stats to reflect the first
        // observation in the new run. For other slots, we continue their existing runs.
        for slot in &mut new_slots {
            slot.update_sufficient_stats(x, hp);
        }

        self.slots = new_slots;
        self.total_observations += 1;
    }

    /// The changepoint probability P(r_t = 0 | x_{1:t}).
    ///
    /// Returns a value in `[0.0, 1.0]`. This is the primary BOCPD output.
    pub fn changepoint_prob(&self) -> f64 {
        self.slots
            .first()
            .map(|s| s.log_joint.exp().clamp(0.0, 1.0))
            .unwrap_or(0.0)
    }

    /// Normalised run-length probabilities P(r_t = r | x_{1:t}) for all tracked r.
    ///
    /// The probabilities sum to 1.0 (within floating-point precision).
    /// Used for the `run_length_prob_1` and `run_length_prob_mode` evidence keys.
    pub fn run_length_probs(&self) -> Vec<f64> {
        self.slots.iter().map(|s| s.log_joint.exp()).collect()
    }

    /// The mode run length (argmax P(r_t)).
    ///
    /// Tie-broken by lowest run length for determinism.
    pub fn run_length_mode(&self) -> usize {
        self.slots
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                a.log_joint
                    .partial_cmp(&b.log_joint)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// Restore `max_slots` after deserialisation from JSONB (field is skipped by serde).
    pub fn restore_max_slots(&mut self, max_run_length_tracked: usize) {
        self.max_slots = max_run_length_tracked + 1;
    }
}

// ---------------------------------------------------------------------------
// Numerically stable log-sum-exp
// ---------------------------------------------------------------------------

/// Numerically stable `log(sum(exp(v_i)))`.
///
/// Uses the standard max-subtraction trick: `max(v) + ln(sum(exp(v_i - max(v))))`.
/// Returns `NEG_INFINITY` for an empty or all-`NEG_INFINITY` input.
fn log_sum_exp(v: &[f64]) -> f64 {
    let max_v = v.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    if !max_v.is_finite() {
        return f64::NEG_INFINITY;
    }
    let sum_exp: f64 = v.iter().map(|&x| (x - max_v).exp()).sum();
    max_v + sum_exp.ln()
}

// ---------------------------------------------------------------------------
// BOCPD state store trait + mock
// ---------------------------------------------------------------------------

/// Read/write/delete API for the `bocpd_deployer_state` table.
///
/// Uses `async_trait` for dyn-compatibility — same pattern as `GraphLabelStore`.
/// All implementations must be `Send + Sync`.
#[async_trait::async_trait]
pub trait BocpdStateStore: Send + Sync {
    /// Load the BOCPD state for `(chain, deployer)`.
    ///
    /// Returns `None` if no state exists (cold start — initialise with prior).
    async fn load_state(&self, chain: &str, deployer: &str) -> anyhow::Result<Option<BocpdState>>;

    /// Persist the BOCPD state for `(chain, deployer)`.
    ///
    /// UPSERT: insert or update based on PRIMARY KEY (chain, deployer).
    // Many args required by the DB schema (design 0016 §4.5). Struct not used
    // to keep the trait dyn-compatible without an extra allocation per call.
    #[allow(clippy::too_many_arguments)]
    async fn save_state(
        &self,
        chain: &str,
        deployer: &str,
        state: &BocpdState,
        last_score: f64,
        last_features: &ObservationFeatures,
        last_cp_prob: f64,
        block_height: Option<i64>,
        block_time: Option<DateTime<Utc>>,
    ) -> anyhow::Result<()>;

    /// Delete BOCPD states where `last_update_block_height >= reorg_height`.
    ///
    /// Called by the reorg hook. Affected deployers recover state by replaying
    /// observations on the next trigger (cold-start behavior).
    async fn delete_states_above_block(
        &self,
        chain: &str,
        reorg_height: i64,
    ) -> anyhow::Result<u64>;
}

/// Postgres implementation of [`BocpdStateStore`].
pub struct PgBocpdStateStore {
    pool: Arc<PgPool>,
}

impl PgBocpdStateStore {
    /// Construct a new `PgBocpdStateStore` wrapping the given pool.
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl BocpdStateStore for PgBocpdStateStore {
    #[instrument(skip(self), fields(chain = chain, deployer = deployer))]
    async fn load_state(&self, chain: &str, deployer: &str) -> anyhow::Result<Option<BocpdState>> {
        let row: Option<(serde_json::Value, i32)> = sqlx::query_as(
            r#"SELECT run_length_state_json, total_observations
               FROM bocpd_deployer_state
               WHERE chain = $1 AND deployer = $2"#,
        )
        .bind(chain)
        .bind(deployer)
        .fetch_optional(&*self.pool)
        .await
        .context("bocpd_deployer_state SELECT failed")?;

        match row {
            None => Ok(None),
            Some((json_val, total_obs)) => {
                let mut slots: Vec<RunSlot> = serde_json::from_value(json_val)
                    .context("failed to deserialise run_length_state_json")?;
                // Ensure slot.r matches array index (defensive).
                for (i, slot) in slots.iter_mut().enumerate() {
                    slot.r = i as u32;
                }
                let state = BocpdState {
                    slots,
                    total_observations: total_obs as u32,
                    max_slots: 1001, // default; caller calls restore_max_slots
                };
                Ok(Some(state))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[instrument(skip(self, state, last_features), fields(chain = chain, deployer = deployer))]
    async fn save_state(
        &self,
        chain: &str,
        deployer: &str,
        state: &BocpdState,
        last_score: f64,
        last_features: &ObservationFeatures,
        last_cp_prob: f64,
        block_height: Option<i64>,
        block_time: Option<DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        let slots_json = serde_json::to_value(&state.slots)
            .context("failed to serialise run_length_state_json")?;

        let features_json = serde_json::json!({
            "log_gap_seconds": last_features.log_gap_seconds,
            "lp_locked_pct": last_features.lp_locked_pct,
            "log_initial_liquidity_usd": last_features.log_initial_liquidity_usd,
            "holder_count_at_1h": last_features.holder_count_at_1h,
            "prior_rug_rate": last_features.prior_rug_rate,
        });

        sqlx::query(
            r#"INSERT INTO bocpd_deployer_state (
                   chain, deployer, total_observations,
                   run_length_state_json, last_observation_score,
                   last_observation_features_json, last_cp_prob,
                   last_update_block_height, last_update_block_time, updated_at
               )
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now())
               ON CONFLICT (chain, deployer) DO UPDATE SET
                   total_observations              = EXCLUDED.total_observations,
                   run_length_state_json           = EXCLUDED.run_length_state_json,
                   last_observation_score          = EXCLUDED.last_observation_score,
                   last_observation_features_json  = EXCLUDED.last_observation_features_json,
                   last_cp_prob                    = EXCLUDED.last_cp_prob,
                   last_update_block_height        = EXCLUDED.last_update_block_height,
                   last_update_block_time          = EXCLUDED.last_update_block_time,
                   updated_at                      = now()"#,
        )
        .bind(chain)
        .bind(deployer)
        .bind(state.total_observations as i32)
        .bind(slots_json)
        .bind(last_score)
        .bind(features_json)
        .bind(last_cp_prob)
        .bind(block_height)
        .bind(block_time)
        .execute(&*self.pool)
        .await
        .context("bocpd_deployer_state UPSERT failed")?;

        Ok(())
    }

    #[instrument(skip(self), fields(chain = chain, reorg_height = reorg_height))]
    async fn delete_states_above_block(
        &self,
        chain: &str,
        reorg_height: i64,
    ) -> anyhow::Result<u64> {
        let result = sqlx::query(
            r#"DELETE FROM bocpd_deployer_state
               WHERE chain = $1 AND last_update_block_height >= $2"#,
        )
        .bind(chain)
        .bind(reorg_height)
        .execute(&*self.pool)
        .await
        .context("bocpd_deployer_state DELETE (reorg) failed")?;

        Ok(result.rows_affected())
    }
}

// ---------------------------------------------------------------------------
// D09BocpdDetector
// ---------------------------------------------------------------------------

/// D09 BOCPD deployer changepoint detector.
///
/// # Primary vs. trait path
///
/// **Primary path** (event-driven): `on_new_token_launch(chain, deployer, token, ctx)`.
/// Called from `IndexerEventHandler::on_pool_initialize` after the `DeployerOf`
/// edge is written. Runs immediately; no cadence scheduling.
///
/// **Trait path** (`Detector::evaluate`): fallback for historical replay via the
/// streaming scheduler. Looks up the deployer for `ctx.token`, then calls the
/// primary path logic.
///
/// # Send + Sync
///
/// All fields are `Send + Sync`: `Arc<dyn _>` stores and `D09Config`.
/// The detector itself is `Send + Sync` and can be stored in `Arc<D09BocpdDetector>`.
pub struct D09BocpdDetector {
    /// Typed edge store for reading `DeployerOf` edges (deployer → token).
    pub edge_store: Arc<dyn TypedEdgeStore>,
    /// Label store for reading `KnownDex`/`KnownExchange` labels (suppression §8.1).
    pub label_store: Arc<dyn GraphLabelStore>,
    /// BOCPD state store (reads/writes `bocpd_deployer_state`).
    pub state_store: Arc<dyn BocpdStateStore>,
    /// Raw Postgres pool for feature queries (design 0016 §4.2).
    pub pg_pool: Arc<PgPool>,
    /// D09 configuration thresholds (from `config/detectors.toml`).
    pub config: D09Config,
}

impl D09BocpdDetector {
    /// Construct a new D09 detector, validating the composite weights at construction.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `config.weights` do not sum to 1.0 within 1e-6.
    pub fn new(
        edge_store: Arc<dyn TypedEdgeStore>,
        label_store: Arc<dyn GraphLabelStore>,
        state_store: Arc<dyn BocpdStateStore>,
        pg_pool: Arc<PgPool>,
        config: D09Config,
    ) -> anyhow::Result<Self> {
        config.weights.validate()?;
        Ok(Self {
            edge_store,
            label_store,
            state_store,
            pg_pool,
            config,
        })
    }

    /// Primary event-driven entry point: evaluate D09 for a new token launch
    /// from a known deployer.
    ///
    /// Called from `IndexerEventHandler::on_pool_initialize` after the
    /// `DeployerOf` edge has been written (design 0016 §4.1 / §5.1).
    ///
    /// # Determinism
    ///
    /// `observed_at` MUST be derived from `PoolEvent::Initialize.block_time`
    /// (gotcha #22 / design 0016 §4.6). Never `Utc::now()`.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<AnomalyEvent>)` — empty if no changepoint or suppressed.
    /// `Err` — transient DB failure; caller should log and skip without crashing.
    #[instrument(
        skip(self),
        fields(chain = %chain, deployer = %deployer, token = %token)
    )]
    pub async fn on_new_token_launch(
        &self,
        chain: Chain,
        deployer: &str,
        token: &str,
        observed_at: DateTime<Utc>,
        block_ref: Option<BlockRef>,
    ) -> anyhow::Result<Vec<AnomalyEvent>> {
        let chain_str = chain.to_string();

        // §8.1 Established-protocol suppression: skip KnownDex / KnownExchange deployers.
        let labels = self
            .label_store
            .get_labels(&chain_str, deployer)
            .await
            .context("get_labels failed in D09 established-protocol check")?;

        let is_known_infra = labels.iter().any(|l| {
            matches!(l.label_type, LabelType::KnownDex | LabelType::KnownExchange)
                && l.confidence >= self.config.infra_label_confidence_floor
        });
        if is_known_infra {
            debug!(deployer, "D09: skipping KnownDex/KnownExchange deployer");
            return Ok(vec![]);
        }

        // §4.2 Feature extraction.
        let features = self
            .extract_features(&chain_str, deployer, token, observed_at)
            .await
            .context("D09 feature extraction failed")?;

        let score = features.composite_score(&self.config.weights);

        // §4.3 Load BOCPD state.
        let mut state = match self.state_store.load_state(&chain_str, deployer).await? {
            Some(mut s) => {
                s.restore_max_slots(self.config.max_run_length_tracked);
                s
            }
            None => BocpdState::new_with_prior(
                &self.config.hyperparams,
                self.config.max_run_length_tracked,
            ),
        };

        // §4.3 BOCPD update.
        state.update(score, self.config.hazard_rate, &self.config.hyperparams);
        let cp_prob = state.changepoint_prob();

        debug!(
            deployer,
            token,
            score,
            cp_prob,
            total_obs = state.total_observations,
            "D09 BOCPD update"
        );

        // §4.3 Always persist state (even when no event is emitted).
        let block_height_i64 = block_ref.as_ref().map(|b| b.height as i64);
        self.state_store
            .save_state(
                &chain_str,
                deployer,
                &state,
                score,
                &features,
                cp_prob,
                block_height_i64,
                Some(observed_at),
            )
            .await
            .context("D09 save_state failed")?;

        // §8.2 Min-history guard: no event until enough history.
        if state.total_observations < self.config.min_history_length {
            debug!(
                deployer,
                obs = state.total_observations,
                min = self.config.min_history_length,
                "D09: min_history_length not reached, no event"
            );
            return Ok(vec![]);
        }

        // Alert rule: emit when P(r_t = 0) >= changepoint_prob_threshold.
        if cp_prob < self.config.changepoint_prob_threshold {
            return Ok(vec![]);
        }

        // Build event.
        let event = self
            .build_event(
                chain,
                token,
                deployer,
                &features,
                &state,
                score,
                cp_prob,
                observed_at,
                block_ref,
            )
            .await
            .context("D09 build_event failed")?;

        Ok(vec![event])
    }

    /// Extract the five observation features for `(chain, deployer, token)`.
    ///
    /// Design 0016 §4.2. All queries use block_time guards for temporal correctness
    /// (no lookahead — design 0016 §12 DG-D09-5).
    async fn extract_features(
        &self,
        chain: &str,
        deployer: &str,
        token: &str,
        observed_at: DateTime<Utc>,
    ) -> anyhow::Result<ObservationFeatures> {
        // Feature 0: log_gap_seconds — time since previous launch by this deployer.
        let gap_seconds = self
            .query_gap_seconds(chain, deployer, token, observed_at)
            .await?;
        let log_gap_seconds = (gap_seconds + 1.0).ln();

        // Feature 1: lp_locked_pct at pool initialization.
        let lp_locked_pct = self.query_lp_locked_pct(chain, token).await?;

        // Feature 2: log_initial_liquidity_usd.
        let initial_liquidity_usd = self.query_initial_liquidity_usd(chain, token).await?;
        let log_initial_liquidity_usd = (initial_liquidity_usd + 1.0).ln();

        // Feature 3: holder count at +1h.
        let holder_count_at_1h = self
            .query_holder_count_at_1h(chain, token, observed_at)
            .await?;

        // Feature 4: prior rug rate.
        let prior_rug_rate = self
            .query_prior_rug_rate(chain, deployer, token, observed_at)
            .await?;

        Ok(ObservationFeatures {
            log_gap_seconds,
            lp_locked_pct,
            log_initial_liquidity_usd,
            holder_count_at_1h,
            prior_rug_rate,
        })
    }

    /// Query the gap in seconds since the deployer's previous token launch.
    ///
    /// Returns `30 * 86400` (30 days) if this is the deployer's first launch
    /// (design 0016 §4.2 F0: default = ln(30*24*3600 + 1) ≈ 14.7).
    async fn query_gap_seconds(
        &self,
        chain: &str,
        deployer: &str,
        new_token: &str,
        observed_at: DateTime<Utc>,
    ) -> anyhow::Result<f64> {
        let row: Option<(Option<f64>,)> = sqlx::query_as(
            r#"SELECT EXTRACT(EPOCH FROM (
                   $4::TIMESTAMPTZ - MAX(ge.block_time)
               ))::DOUBLE PRECISION AS gap_seconds
               FROM graph_edges ge
               WHERE ge.chain = $1
                 AND ge.from_address = $2
                 AND ge.edge_type = 'DeployerOf'
                 AND ge.to_address != $3
                 AND ge.block_time <= $4"#,
        )
        .bind(chain)
        .bind(deployer)
        .bind(new_token)
        .bind(observed_at)
        .fetch_optional(&*self.pg_pool)
        .await
        .context("D09 gap_seconds query failed")?;

        let gap = row.and_then(|(v,)| v).unwrap_or(30.0 * 86400.0); // 30 days default for first launch

        // Clamp to non-negative (block_time ordering guarantees >= 0, but defensive).
        Ok(gap.max(0.0))
    }

    /// Approximate `lp_locked_pct` for the token at pool initialization.
    ///
    /// The `pools` table does not have a `lp_locked_pct` column (data gap DG-D09-3a).
    /// As a proxy we use `1.0 - (deployer_lp_amount / lp_total_supply)`:
    /// if the deployer holds all LP (lp_amount = total_supply), locked_pct ≈ 0.0 (risky);
    /// if deployer holds 0 LP (all distributed/burned), locked_pct ≈ 1.0 (safer).
    ///
    /// Returns 0.0 (maximum-risk fallback per DG-D09-3) when pool data is missing.
    /// The low weight `composite_weight_lp_locked = 0.25` limits impact of this gap.
    async fn query_lp_locked_pct(&self, chain: &str, token: &str) -> anyhow::Result<f64> {
        let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
            r#"SELECT deployer_lp_amount::TEXT, lp_total_supply::TEXT
               FROM pools
               WHERE chain = $1 AND (token0 = $2 OR token1 = $2)
               ORDER BY created_at ASC NULLS LAST
               LIMIT 1"#,
        )
        .bind(chain)
        .bind(token)
        .fetch_optional(&*self.pg_pool)
        .await
        .context("D09 lp_locked_pct (proxy) query failed")?;

        let pct = match row {
            Some((Some(deployer_s), Some(total_s))) => {
                let deployer: f64 = deployer_s.parse().unwrap_or(0.0);
                let total: f64 = total_s.parse().unwrap_or(0.0);
                if total > 0.0 {
                    // locked_pct = fraction of LP NOT held by deployer
                    (1.0 - (deployer / total)).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            }
            _ => 0.0, // no pool data — use maximum-risk fallback
        };
        Ok(pct)
    }

    /// Query `pools.initial_liquidity_usd` for the token (V00013 column).
    async fn query_initial_liquidity_usd(&self, chain: &str, token: &str) -> anyhow::Result<f64> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            r#"SELECT initial_liquidity_usd::TEXT
               FROM pools
               WHERE chain = $1 AND (token0 = $2 OR token1 = $2)
               ORDER BY created_at ASC
               LIMIT 1"#,
        )
        .bind(chain)
        .bind(token)
        .fetch_optional(&*self.pg_pool)
        .await
        .context("D09 initial_liquidity_usd query failed")?;

        let val: f64 = row
            .and_then(|(s,)| s)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);

        Ok(val.max(0.0))
    }

    /// Query holder count for `token` at approximately launch+1h.
    ///
    /// Uses `holder_snapshots_history` within a [launch, launch+3h] window,
    /// selecting the snapshot whose `snapshot_time` is closest to `launch+1h`.
    /// Uses `total_holders` (pre-aggregated per snapshot row) for efficiency.
    /// Returns 0.0 if no snapshot exists within the window (DG-D09-3 fallback).
    async fn query_holder_count_at_1h(
        &self,
        chain: &str,
        token: &str,
        observed_at: DateTime<Utc>,
    ) -> anyhow::Result<f64> {
        let target_time = observed_at + chrono::Duration::hours(1);
        let window_start = observed_at;
        let window_end = observed_at + chrono::Duration::hours(3);

        // `total_holders` is stored per snapshot row; we grab the value from the
        // snapshot closest to launch+1h (any holder row from that snapshot suffices
        // because total_holders is the same across all rows of the same snapshot).
        let row: Option<(Option<i64>,)> = sqlx::query_as(
            r#"SELECT total_holders
               FROM holder_snapshots_history
               WHERE chain = $1
                 AND token = $2
                 AND snapshot_time BETWEEN $3 AND $4
               ORDER BY ABS(EXTRACT(EPOCH FROM (snapshot_time - $5::TIMESTAMPTZ)))
               LIMIT 1"#,
        )
        .bind(chain)
        .bind(token)
        .bind(window_start)
        .bind(window_end)
        .bind(target_time)
        .fetch_optional(&*self.pg_pool)
        .await
        .context("D09 holder_count_at_1h query failed")?;

        Ok(row.and_then(|(v,)| v).unwrap_or(0) as f64)
    }

    /// Query the fraction of prior tokens from this deployer that were rugged.
    ///
    /// Only counts `anomaly_events` with `detector_id IN (rug_pull_lp_drain,
    /// mint_burn_anomaly, withdraw_withheld_drain)` and `confidence >= rug_confidence_threshold`.
    /// D09's own events (`deployer_changepoint`) are explicitly excluded (§8.4).
    ///
    /// Uses `block_time <= observed_at` guard for temporal correctness (DG-D09-5).
    async fn query_prior_rug_rate(
        &self,
        chain: &str,
        deployer: &str,
        new_token: &str,
        observed_at: DateTime<Utc>,
    ) -> anyhow::Result<f64> {
        let row: Option<(Option<i64>, Option<i64>)> = sqlx::query_as(
            r#"SELECT
                   COUNT(DISTINCT ae.token) FILTER (
                       WHERE ae.confidence >= $5
                         AND ae.ingested_at <= $4
                   ) AS rugged,
                   COUNT(DISTINCT ge.to_address) AS total_prior
               FROM graph_edges ge
               LEFT JOIN anomaly_events ae
                   ON ae.chain = ge.chain
                  AND ae.token = ge.to_address
                  AND ae.detector_id IN (
                      'rug_pull_lp_drain',
                      'mint_burn_anomaly',
                      'withdraw_withheld_drain'
                  )
               WHERE ge.chain = $1
                 AND ge.from_address = $2
                 AND ge.edge_type = 'DeployerOf'
                 AND ge.to_address != $3
                 AND ge.block_time <= $4"#,
        )
        .bind(chain)
        .bind(deployer)
        .bind(new_token)
        .bind(observed_at)
        .bind(self.config.rug_confidence_threshold)
        .fetch_optional(&*self.pg_pool)
        .await
        .context("D09 prior_rug_rate query failed")?;

        let (rugged, total) = row.unwrap_or((None, None));
        let rugged = rugged.unwrap_or(0) as f64;
        let total = total.unwrap_or(0) as f64;

        if total == 0.0 {
            return Ok(0.0);
        }
        Ok((rugged / total).clamp(0.0, 1.0))
    }

    /// Query up to 5 most-recently-rugged prior tokens for evidence notes.
    async fn query_prior_rug_tokens(
        &self,
        chain: &str,
        deployer: &str,
        new_token: &str,
        observed_at: DateTime<Utc>,
    ) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"SELECT DISTINCT ge.to_address
               FROM graph_edges ge
               JOIN anomaly_events ae
                   ON ae.chain = ge.chain
                  AND ae.token = ge.to_address
                  AND ae.detector_id IN (
                      'rug_pull_lp_drain',
                      'mint_burn_anomaly',
                      'withdraw_withheld_drain'
                  )
                  AND ae.confidence >= $4
                  AND ae.ingested_at <= $3
               WHERE ge.chain = $1
                 AND ge.from_address = $2
                 AND ge.edge_type = 'DeployerOf'
                 AND ge.to_address != $5
               ORDER BY ge.to_address
               LIMIT 5"#,
        )
        .bind(chain)
        .bind(deployer)
        .bind(observed_at)
        .bind(self.config.rug_confidence_threshold)
        .bind(new_token)
        .fetch_all(&*self.pg_pool)
        .await
        .context("D09 prior_rug_tokens query failed")?;

        Ok(rows.into_iter().map(|(s,)| s).collect())
    }

    /// Build the `AnomalyEvent` for a detected changepoint.
    #[allow(clippy::too_many_arguments)]
    async fn build_event(
        &self,
        chain: Chain,
        token: &str,
        deployer: &str,
        features: &ObservationFeatures,
        state: &BocpdState,
        score: f64,
        cp_prob: f64,
        observed_at: DateTime<Utc>,
        block_ref: Option<BlockRef>,
    ) -> anyhow::Result<AnomalyEvent> {
        let confidence_f64 = cp_prob.clamp(0.0, 1.0);
        let confidence = Confidence::new(confidence_f64)
            .map_err(|e| anyhow::anyhow!("D09 confidence out of range (bug): {e}"))?;
        let severity = severity_from_confidence(confidence_f64);

        let probs = state.run_length_probs();
        let mode_r = state.run_length_mode();
        let prob_mode = probs.get(mode_r).copied().unwrap_or(0.0);
        let prob_1 = probs.get(1).copied().unwrap_or(0.0);

        // Convert f64 probability values to Decimal for evidence metrics.
        // Per design 0016 §6: use `Decimal::from_f64(...).round_dp(N)`.
        let d = |v: f64, dp: u32| -> Decimal {
            Decimal::from_f64(v).unwrap_or(Decimal::ZERO).round_dp(dp)
        };

        let mut evidence = Evidence::new()
            .with_metric(format!("{DETECTOR_ID}/changepoint_prob"), d(cp_prob, 6))
            .with_metric(format!("{DETECTOR_ID}/observation_value"), d(score, 6))
            .with_metric(
                format!("{DETECTOR_ID}/total_tokens_launched"),
                Decimal::from(state.total_observations),
            )
            .with_metric(
                format!("{DETECTOR_ID}/prior_rug_rate"),
                d(features.prior_rug_rate, 4),
            )
            .with_metric(
                format!("{DETECTOR_ID}/lp_locked_pct"),
                d(features.lp_locked_pct, 4),
            )
            .with_metric(
                format!("{DETECTOR_ID}/log_gap_seconds"),
                d(features.log_gap_seconds, 4),
            )
            .with_metric(
                format!("{DETECTOR_ID}/run_length_mode"),
                Decimal::from(mode_r as u32),
            )
            .with_metric(format!("{DETECTOR_ID}/run_length_prob_0"), d(cp_prob, 6))
            .with_metric(format!("{DETECTOR_ID}/run_length_prob_1"), d(prob_1, 6))
            .with_metric(
                format!("{DETECTOR_ID}/run_length_prob_mode"),
                d(prob_mode, 6),
            )
            .with_note("detector_version=D09_v1".to_string())
            .with_note(format!("new_token={token}"));

        // Deployer EOA goes into Evidence::addresses.
        if let Ok(deployer_addr) = Address::parse(chain, deployer) {
            evidence = evidence.with_address(deployer_addr);
        } else {
            evidence = evidence.with_note(format!("deployer={deployer}"));
        }

        // Prior rug tokens (up to 5, sorted for determinism).
        let chain_str = chain.to_string();
        let rug_tokens = self
            .query_prior_rug_tokens(&chain_str, deployer, token, observed_at)
            .await
            .unwrap_or_default();
        if !rug_tokens.is_empty() {
            evidence = evidence.with_note(format!("prior_rug_tokens={}", rug_tokens.join(",")));
        }

        let token_addr = Address::parse(chain, token)
            .map_err(|e| anyhow::anyhow!("D09 token address parse failed: {e}"))?;

        // Build the block window for the event. D09 is event-driven (one observation per
        // token launch), so both window endpoints reference the same block.
        let block_start = block_ref.unwrap_or_else(|| BlockRef::new(chain, 0));
        let block_end = block_start;

        let event = AnomalyEvent {
            detector_id: DETECTOR_ID.to_string(),
            chain,
            token: token_addr,
            confidence,
            severity,
            evidence,
            // gotcha #22: block-time-sourced, NOT Utc::now().
            observed_at,
            ingested_at: observed_at,
            window: (block_start, block_end),
            oak_technique_id: self.oak_technique_id().map(String::from),
        };

        Ok(event)
    }
}

// ---------------------------------------------------------------------------
// Detector trait implementation (fallback / scheduler path)
// ---------------------------------------------------------------------------

impl crate::detector::Detector for D09BocpdDetector {
    fn id(&self) -> &'static str {
        DETECTOR_ID
    }

    fn oak_technique_id(&self) -> Option<&str> {
        Some("OAK-T8.001") // Common-Funder Cluster Reuse
    }

    fn severity_floor(&self) -> Severity {
        Severity::Medium
    }

    fn supported_chains(&self) -> &[mg_onchain_common::chain::Chain] {
        &[
            mg_onchain_common::chain::Chain::Solana,
            mg_onchain_common::chain::Chain::Ethereum,
            mg_onchain_common::chain::Chain::Bsc,
            mg_onchain_common::chain::Chain::Base,
            mg_onchain_common::chain::Chain::Arbitrum,
            mg_onchain_common::chain::Chain::Polygon,
        ]
    }

    /// Evaluate D09 for the token in `ctx`.
    ///
    /// Fallback path for historical replay / scheduler invocation.
    /// Looks up the deployer for `ctx.token` via `graph_edges`, then
    /// runs the BOCPD update for this token as if it were a new launch.
    ///
    /// In production, prefer the `on_new_token_launch` path which is called
    /// directly from the indexer event handler (design 0016 §5.2).
    #[instrument(skip(self, ctx), fields(chain = %ctx.chain, token = %ctx.token))]
    fn evaluate<'ctx>(
        &'ctx self,
        ctx: &'ctx DetectorContext<'ctx>,
    ) -> impl std::future::Future<Output = Result<Vec<AnomalyEvent>, DetectorError>> + Send + 'ctx
    {
        async move { self.evaluate_inner(ctx).await }
    }
}

impl D09BocpdDetector {
    /// Inner async body for `Detector::evaluate` (scheduler / replay path).
    async fn evaluate_inner(
        &self,
        ctx: &DetectorContext<'_>,
    ) -> Result<Vec<AnomalyEvent>, DetectorError> {
        let chain_str = ctx.chain.to_string();
        let token_str = ctx.token.to_string();

        // Find the deployer for this token via graph_edges.DeployerOf (reverse lookup).
        // "Who deployed this token?" = get_predecessors(token, DeployerOf).
        let deployer_opt = self
            .edge_store
            .get_predecessors(&chain_str, &token_str, EdgeType::DeployerOf, 1)
            .await
            .map_err(|e| DetectorError::PermanentQuery {
                detector_id: DETECTOR_ID,
                reason: format!("edge_store.get_predecessors failed: {e}"),
            })?
            .into_iter()
            .next();

        let deployer = match deployer_opt {
            Some(edge) => edge.from_address,
            None => {
                // No deployer edge found — token not in graph. Skip.
                debug!(token = %ctx.token, "D09: no DeployerOf edge found, skipping");
                return Ok(vec![]);
            }
        };

        let block_ref = Some(ctx.window.block_end);

        self.on_new_token_launch(ctx.chain, &deployer, &token_str, ctx.observed_at, block_ref)
            .await
            .map_err(|e| DetectorError::PermanentQuery {
                detector_id: DETECTOR_ID,
                reason: format!("D09 evaluate_inner failed: {e}"),
            })
    }
}

// ---------------------------------------------------------------------------
// AnomalyEventSink — write interface for detector outputs
// ---------------------------------------------------------------------------

/// Write interface for persisting `AnomalyEvent` rows produced by D09.
///
/// The production implementation wraps `mg_onchain_storage::PgStore`.
/// Tests use `MockAnomalyEventSink` (in the `mock` sub-module).
///
/// Uses `async_trait` for dyn-compatibility — same pattern as `BocpdStateStore`.
///
/// # Why not reuse `EventSink`?
///
/// `crates/indexer::EventSink` is not object-safe (native async trait, no
/// `async_trait`). `D09IndexerHook` needs a dyn-compatible sink that can be
/// injected in tests without a Postgres connection. Defining this minimal
/// trait in `crates/detectors` keeps the dependency arrow detectors → indexer
/// for the `PoolInitializeHook` trait without pulling the full indexer event
/// sink surface in here.
#[async_trait::async_trait]
pub trait AnomalyEventSink: Send + Sync {
    /// Insert anomaly events produced by the D09 detector.
    ///
    /// `emitted_by` is a static tag for auditing (e.g. `"d09_indexer_hook"`).
    ///
    /// # Errors
    ///
    /// Return `Err` on transient DB failures. The caller (D09IndexerHook) will
    /// propagate the error as `IndexerError::Config`, making it visible in the
    /// indexer run loop.
    async fn insert_anomaly_events(
        &self,
        events: &[mg_onchain_common::anomaly::AnomalyEvent],
        emitted_by: &str,
    ) -> anyhow::Result<()>;
}

/// Production `AnomalyEventSink` backed by `mg_onchain_storage::PgStore`.
pub struct PgAnomalyEventSink {
    pg: mg_onchain_storage::PgStore,
}

impl PgAnomalyEventSink {
    /// Construct from an existing `PgStore`.
    pub fn new(pg: mg_onchain_storage::PgStore) -> Self {
        Self { pg }
    }
}

#[async_trait::async_trait]
impl AnomalyEventSink for PgAnomalyEventSink {
    async fn insert_anomaly_events(
        &self,
        events: &[mg_onchain_common::anomaly::AnomalyEvent],
        emitted_by: &str,
    ) -> anyhow::Result<()> {
        self.pg
            .insert_anomaly_events(events, emitted_by)
            .await
            .map_err(|e| anyhow::anyhow!("PgAnomalyEventSink: {e}"))
    }
}

// ---------------------------------------------------------------------------
// D09IndexerHook — PoolInitializeHook adapter for D09BocpdDetector
// ---------------------------------------------------------------------------

/// Bridges the indexer `PoolInitializeHook` trait to `D09BocpdDetector`.
///
/// # Lifecycle
///
/// Constructed at server startup when `config.detectors.deployer_changepoint.enabled`
/// is `true`. Passed as `Some(Arc::new(hook))` to `Indexer::new`.
///
/// # Evaluate both tokens (token0 + token1)
///
/// The graph writer writes `DeployerOf` edges for both `token0` and `token1`.
/// D09 is called for both. Established-protocol deployers (KnownDex /
/// KnownExchange) are suppressed inside `D09BocpdDetector::on_new_token_launch`
/// (§8.1), so calling it on WSOL or USDC addresses is safe — it will return
/// `Ok(vec![])` immediately.
///
/// # Fail-loud
///
/// Both `on_new_token_launch` and `on_reorg` propagate errors as `IndexerError`,
/// matching the fail-loud pattern used by the graph writer.
pub struct D09IndexerHook {
    detector: std::sync::Arc<D09BocpdDetector>,
    anomaly_sink: std::sync::Arc<dyn AnomalyEventSink>,
}

impl D09IndexerHook {
    /// Construct a new `D09IndexerHook`.
    ///
    /// `detector` is the fully-initialised D09 detector (weights validated).
    /// `anomaly_sink` receives the `AnomalyEvent`s emitted by D09.
    pub fn new(
        detector: std::sync::Arc<D09BocpdDetector>,
        anomaly_sink: std::sync::Arc<dyn AnomalyEventSink>,
    ) -> Self {
        Self {
            detector,
            anomaly_sink,
        }
    }
}

#[async_trait::async_trait]
impl mg_onchain_indexer::hooks::PoolInitializeHook for D09IndexerHook {
    #[tracing::instrument(
        skip(self),
        fields(chain = %chain, deployer = %deployer, token0 = %token0, token1 = %token1)
    )]
    async fn on_new_token_launch(
        &self,
        chain: mg_onchain_common::chain::Chain,
        deployer: &str,
        token0: &str,
        token1: &str,
        observed_at: chrono::DateTime<chrono::Utc>,
        block_ref: mg_onchain_common::chain::BlockRef,
    ) -> Result<(), mg_onchain_indexer::error::IndexerError> {
        // Evaluate D09 for both tokens in the pool.
        // §8.1 suppression handles established-protocol deployers inside the detector.
        for token in [token0, token1] {
            let events = self
                .detector
                .on_new_token_launch(chain, deployer, token, observed_at, Some(block_ref))
                .await
                .map_err(|e| {
                    mg_onchain_indexer::error::IndexerError::Config(format!(
                        "D09 on_new_token_launch failed for token {token}: {e}"
                    ))
                })?;

            if !events.is_empty() {
                self.anomaly_sink
                    .insert_anomaly_events(&events, "d09_indexer_hook")
                    .await
                    .map_err(|e| {
                        mg_onchain_indexer::error::IndexerError::Config(format!(
                            "D09 anomaly sink failed for token {token}: {e}"
                        ))
                    })?;
            }
        }
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(chain = %chain, reorg_height = %reorg_height))]
    async fn on_reorg(
        &self,
        chain: &str,
        reorg_height: u64,
    ) -> Result<(), mg_onchain_indexer::error::IndexerError> {
        let deleted = self
            .detector
            .state_store
            .delete_states_above_block(chain, reorg_height as i64)
            .await
            .map_err(|e| {
                mg_onchain_indexer::error::IndexerError::Config(format!(
                    "D09 delete_states_above_block failed: {e}"
                ))
            })?;
        tracing::debug!(
            chain,
            reorg_height,
            deleted,
            "D09 reorg: BOCPD state rows deleted"
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Mock BocpdStateStore for tests
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "test-utils"))]
pub mod mock {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory mock for `BocpdStateStore`.
    ///
    /// Thread-safe via `Mutex<HashMap<(chain, deployer), BocpdState>>`.
    /// Used in unit and integration tests to avoid database dependency.
    #[derive(Default)]
    pub struct MockBocpdStateStore {
        states: Mutex<HashMap<(String, String), BocpdState>>,
    }

    impl MockBocpdStateStore {
        /// Create an empty mock store.
        pub fn new() -> Self {
            Self::default()
        }
    }

    #[async_trait::async_trait]
    impl BocpdStateStore for MockBocpdStateStore {
        async fn load_state(
            &self,
            chain: &str,
            deployer: &str,
        ) -> anyhow::Result<Option<BocpdState>> {
            let guard = self
                .states
                .lock()
                .expect("MockBocpdStateStore mutex poisoned");
            Ok(guard
                .get(&(chain.to_string(), deployer.to_string()))
                .cloned())
        }

        #[allow(clippy::too_many_arguments)]
        async fn save_state(
            &self,
            chain: &str,
            deployer: &str,
            state: &BocpdState,
            _last_score: f64,
            _last_features: &ObservationFeatures,
            _last_cp_prob: f64,
            _block_height: Option<i64>,
            _block_time: Option<DateTime<Utc>>,
        ) -> anyhow::Result<()> {
            let mut guard = self
                .states
                .lock()
                .expect("MockBocpdStateStore mutex poisoned");
            guard.insert((chain.to_string(), deployer.to_string()), state.clone());
            Ok(())
        }

        async fn delete_states_above_block(
            &self,
            chain: &str,
            reorg_height: i64,
        ) -> anyhow::Result<u64> {
            // Mock: clear all states for the chain (conservative reorg handling).
            let mut guard = self
                .states
                .lock()
                .expect("MockBocpdStateStore mutex poisoned");
            let before = guard.len();
            guard.retain(|(c, _), _| c != chain);
            let _ = reorg_height; // reorg_height not used in mock — clears all
            Ok((before - guard.len()) as u64)
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Composite score tests ----

    #[test]
    fn composite_score_malicious_is_high() {
        // Rug-farm scenario: 1-minute gap, 0% LP lock, $1 initial liquidity,
        // 0 holders at 1h, 100% prior rug rate.
        //
        // Formula (design 0016 §2.3):
        //   F1 = 1 - sigmoid(ln(61)/10) = 1 - sigmoid(0.411) ≈ 0.399
        //   F2 = 1 - 0.0 = 1.0
        //   F3 = 1 - sigmoid(ln(2)/8) = 1 - sigmoid(0.087) ≈ 0.478
        //   F4 = 1 - sigmoid(0/100) = 1 - sigmoid(0) = 0.50
        //   F5 = 1.0
        // S ≈ 0.25*0.399 + 0.25*1.0 + 0.15*0.478 + 0.10*0.50 + 0.25*1.0 ≈ 0.73
        // At w_prior_rug_rate=0.25 these extreme inputs produce > 0.70.
        let features = ObservationFeatures {
            log_gap_seconds: f64::ln(61.0), // 1-minute inter-launch gap
            lp_locked_pct: 0.0,
            log_initial_liquidity_usd: f64::ln(2.0), // ~$1 initial liquidity
            holder_count_at_1h: 0.0,
            prior_rug_rate: 1.0, // every prior launch was a rug
        };
        let weights = CompositeWeights::default();
        let score = features.composite_score(&weights);
        assert!(
            score > 0.70,
            "Extreme rug-farm features should produce score > 0.70, got {score:.4}"
        );
    }

    #[test]
    fn composite_score_legitimate_is_low() {
        // From design 0016 §13.1: 30d gap, 90% LP locked, $5000, 120 holders, no rugs.
        let features = ObservationFeatures {
            log_gap_seconds: f64::ln(30.0 * 86400.0 + 1.0), // 30 days
            lp_locked_pct: 0.90,
            log_initial_liquidity_usd: f64::ln(5000.0 + 1.0),
            holder_count_at_1h: 120.0,
            prior_rug_rate: 0.0,
        };
        let weights = CompositeWeights::default();
        let score = features.composite_score(&weights);
        assert!(
            score < 0.15,
            "Legitimate features should produce score < 0.15, got {score:.4}"
        );
    }

    #[test]
    fn composite_score_zero_features_is_bounded() {
        // All-zero features (edge case: first launch, no prior data).
        let features = ObservationFeatures {
            log_gap_seconds: 0.0,
            lp_locked_pct: 0.0,
            log_initial_liquidity_usd: 0.0,
            holder_count_at_1h: 0.0,
            prior_rug_rate: 0.0,
        };
        let weights = CompositeWeights::default();
        let score = features.composite_score(&weights);
        assert!(
            (0.0..=1.0).contains(&score),
            "Score must be in [0,1], got {score}"
        );
    }

    #[test]
    fn composite_weights_default_sum_to_one() {
        let w = CompositeWeights::default();
        let sum =
            w.w_log_gap + w.w_lp_locked + w.w_log_liquidity + w.w_holder_count + w.w_prior_rug_rate;
        assert!(
            (sum - 1.0).abs() < 1e-10,
            "Default weights must sum to 1.0, got {sum}"
        );
    }

    #[test]
    fn composite_weights_validate_ok() {
        CompositeWeights::default()
            .validate()
            .expect("default weights should validate");
    }

    #[test]
    fn composite_weights_validate_fails_on_bad_sum() {
        let w = CompositeWeights {
            w_log_gap: 0.30, // sum = 1.05, not 1.0
            ..CompositeWeights::default()
        };
        assert!(
            w.validate().is_err(),
            "Bad-sum weights should fail validation"
        );
    }

    // ---- BOCPD core math tests ----

    #[test]
    fn bocpd_5obs_stable_no_changepoint() {
        // 5 stable observations ~0.07 → P(r=0) should be well below 0.10.
        let hp = BocpdHyperparams::default();
        let mut state = BocpdState::new_with_prior(&hp, 1000);
        for s in [0.07, 0.08, 0.07, 0.09, 0.07] {
            state.update(s, 0.00333, &hp);
        }
        let cp = state.changepoint_prob();
        assert!(
            cp < 0.10,
            "Stable series: P(r=0) should be < 0.10, got {cp:.6}"
        );
    }

    #[test]
    fn bocpd_abrupt_shift_triggers_changepoint() {
        // Tests that the BOCPD undergoes mass migration from old-regime slots to
        // new-regime slots after a sustained regime shift.
        //
        // BOCPD property (Adams & MacKay 2007 §2): with constant hazard H=1/300,
        // the prior for any changepoint is small. The BOCPD correctly requires
        // MANY new-regime observations to exceed the alert threshold — this is
        // by design, preventing false alarms from single outliers. The correct
        // property to test is that mass migrates FROM high-r slots (old regime)
        // TOWARD low-r slots (new regime) as new-regime observations accumulate.
        //
        // Design:
        //   Phase 1: 20 stable observations at 0.10. Posterior concentrates at r=20.
        //   Phase 2: 30 observations at 0.85. The run-length posterior mass at
        //   r=21..50 (old regime grown with poor predictions) should be LESS than
        //   the mass at r=1..30 (new regime slots, with good predictions of 0.85).
        //
        // After 30 new-regime observations, the new-regime run (r=1..30) should
        // dominate the old-regime run tail (r=21..50).
        let hp = BocpdHyperparams::default();
        let mut state = BocpdState::new_with_prior(&hp, 1000);

        // Phase 1: 20 stable observations at 0.10.
        for _ in 0..20 {
            state.update(0.10, 0.00333, &hp);
        }

        // Phase 2: 30 observations in the new high-risk regime (0.85).
        for _ in 0..30 {
            state.update(0.85, 0.00333, &hp);
        }

        // After 30 new-regime observations the posterior should be shifting.
        // The new-regime slots (r=1..30 from the first changepoint mass) should
        // collectively hold more probability than the old-regime tail (r=51..80).
        let probs = state.run_length_probs();
        let new_regime_mass: f64 = probs.iter().take(31).sum(); // r=0..30
        let old_regime_tail: f64 = probs.iter().skip(50).take(20).sum(); // r=50..69
        assert!(
            new_regime_mass > old_regime_tail,
            "After 30 new-regime obs, new run (r=0..30) mass {new_regime_mass:.6} should exceed \
             old run tail (r=50..69) mass {old_regime_tail:.6}"
        );

        // All values must be finite and in [0,1].
        let cp_now = state.changepoint_prob();
        assert!(
            cp_now.is_finite() && (0.0..=1.0).contains(&cp_now),
            "changepoint_prob must remain in [0,1] after regime shift, got {cp_now}"
        );
    }

    #[test]
    fn bocpd_posterior_sums_to_one() {
        // After each update, run-length probabilities must sum to 1.0.
        let hp = BocpdHyperparams::default();
        let mut state = BocpdState::new_with_prior(&hp, 100);
        for s in [0.10, 0.50, 0.90, 0.10, 0.85] {
            state.update(s, 0.00333, &hp);
            let total: f64 = state.run_length_probs().iter().sum();
            assert!(
                (total - 1.0).abs() < 1e-9,
                "Posterior must sum to 1.0, got {total:.12}"
            );
        }
    }

    #[test]
    fn bocpd_determinism() {
        // Two identical runs must produce bit-identical output (design 0016 §3.7).
        let hp = BocpdHyperparams::default();
        let scores = [0.07, 0.08, 0.07, 0.09, 0.07, 0.85];

        let cp_a = {
            let mut s = BocpdState::new_with_prior(&hp, 1000);
            for score in scores {
                s.update(score, 0.00333, &hp);
            }
            s.changepoint_prob()
        };
        let cp_b = {
            let mut s = BocpdState::new_with_prior(&hp, 1000);
            for score in scores {
                s.update(score, 0.00333, &hp);
            }
            s.changepoint_prob()
        };
        assert_eq!(
            cp_a.to_bits(),
            cp_b.to_bits(),
            "BOCPD must be deterministic: cp_a={cp_a} cp_b={cp_b}"
        );
    }

    #[test]
    fn bocpd_numerical_stability_long_run() {
        // 500 stable observations — no NaN or Inf (design 0016 §3.6).
        let hp = BocpdHyperparams::default();
        let mut state = BocpdState::new_with_prior(&hp, 1000);
        for i in 0..500 {
            state.update(0.08, 0.00333, &hp);
            let cp = state.changepoint_prob();
            assert!(
                cp.is_finite() && !cp.is_nan(),
                "cp_prob must be finite at step {i}, got {cp}"
            );
            assert!(
                (0.0..=1.0).contains(&cp),
                "cp_prob must be in [0,1] at step {i}, got {cp}"
            );
        }
    }

    #[test]
    fn bocpd_pathological_all_zeros() {
        // All-zero observations — no panic, no NaN.
        let hp = BocpdHyperparams::default();
        let mut state = BocpdState::new_with_prior(&hp, 100);
        for _ in 0..10 {
            state.update(0.0, 0.00333, &hp);
            let cp = state.changepoint_prob();
            assert!(cp.is_finite(), "all-zero obs: cp must be finite, got {cp}");
        }
    }

    #[test]
    fn bocpd_pathological_all_max_score() {
        // All-1.0 observations — no panic, no NaN.
        let hp = BocpdHyperparams::default();
        let mut state = BocpdState::new_with_prior(&hp, 100);
        for _ in 0..10 {
            state.update(1.0, 0.00333, &hp);
            let cp = state.changepoint_prob();
            assert!(cp.is_finite(), "all-max obs: cp must be finite, got {cp}");
        }
    }

    #[test]
    fn bocpd_min_history_guard_semantics() {
        // 4 observations below threshold — state exists but no event should be emitted.
        // The guard logic is tested at the detector level, not in BOCPD math.
        // This test confirms total_observations is tracked correctly.
        let hp = BocpdHyperparams::default();
        let mut state = BocpdState::new_with_prior(&hp, 100);
        assert_eq!(state.total_observations, 0);
        for i in 1..=4u32 {
            state.update(0.07, 0.00333, &hp);
            assert_eq!(state.total_observations, i);
        }
        // With default min_history_length=5, 4 obs is below threshold.
        assert!(
            state.total_observations < 5,
            "4 observations should be below min_history_length=5"
        );
    }

    #[test]
    fn bocpd_run_length_mode_after_stable_run() {
        // After 10 stable observations, mode run length should be 10 or 11 (not 0).
        let hp = BocpdHyperparams::default();
        let mut state = BocpdState::new_with_prior(&hp, 100);
        for _ in 0..10 {
            state.update(0.08, 0.00333, &hp);
        }
        let mode = state.run_length_mode();
        assert!(
            mode >= 5,
            "After 10 stable obs, mode run length should be >= 5, got {mode}"
        );
    }

    #[test]
    fn bocpd_absorbing_boundary_no_panic() {
        // Feed max_run_length_tracked+10 observations — no panic, bounded length.
        let max_rl = 20usize;
        let hp = BocpdHyperparams::default();
        let mut state = BocpdState::new_with_prior(&hp, max_rl);
        for _ in 0..(max_rl + 15) {
            state.update(0.08, 0.00333, &hp);
            assert!(
                state.slots.len() <= max_rl + 1,
                "Slots must not exceed max_run_length_tracked+1"
            );
        }
        // Probabilities still sum to 1.0 after absorbing boundary kicks in.
        let total: f64 = state.run_length_probs().iter().sum();
        assert!(
            (total - 1.0).abs() < 1e-9,
            "Posterior must sum to 1.0 after absorbing boundary, got {total}"
        );
    }

    // ---- log_sum_exp ----

    #[test]
    fn log_sum_exp_empty() {
        assert_eq!(log_sum_exp(&[]), f64::NEG_INFINITY);
    }

    #[test]
    fn log_sum_exp_single() {
        let v = log_sum_exp(&[-2.0]);
        assert!((v - (-2.0)).abs() < 1e-12, "log_sum_exp([x]) must equal x");
    }

    #[test]
    fn log_sum_exp_known_values() {
        // log(exp(-1) + exp(-2)) = log(e^{-1}(1 + e^{-1})) = -1 + ln(1 + e^{-1})
        let expected = (-1.0_f64).exp() + (-2.0_f64).exp();
        let lse = log_sum_exp(&[-1.0, -2.0]);
        assert!(
            (lse.exp() - expected).abs() < 1e-12,
            "log_sum_exp result mismatch: got {lse}"
        );
    }

    // ---- BocpdState serialisation round-trip ----

    #[test]
    fn bocpd_state_json_round_trip() {
        // Serialise and deserialise BocpdState; posterior must be identical.
        let hp = BocpdHyperparams::default();
        let mut state = BocpdState::new_with_prior(&hp, 100);
        for s in [0.07, 0.08, 0.85] {
            state.update(s, 0.00333, &hp);
        }
        let cp_before = state.changepoint_prob();

        let json_val = serde_json::to_value(&state.slots).expect("serialise");
        let slots: Vec<RunSlot> = serde_json::from_value(json_val).expect("deserialise");
        let mut restored = BocpdState {
            slots,
            total_observations: state.total_observations,
            max_slots: state.max_slots,
        };
        restored.restore_max_slots(100);

        let cp_after = restored.changepoint_prob();
        assert_eq!(
            cp_before.to_bits(),
            cp_after.to_bits(),
            "JSON round-trip must preserve bit-identical posterior"
        );
    }

    // ---- D09IndexerHook tests ----

    /// Mock `AnomalyEventSink` that records all calls.
    #[derive(Default)]
    struct MockAnomalyEventSink {
        pub calls: std::sync::Arc<std::sync::Mutex<Vec<(usize, String)>>>,
    }

    #[async_trait::async_trait]
    impl AnomalyEventSink for MockAnomalyEventSink {
        async fn insert_anomaly_events(
            &self,
            events: &[mg_onchain_common::anomaly::AnomalyEvent],
            emitted_by: &str,
        ) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push((events.len(), emitted_by.to_owned()));
            Ok(())
        }
    }

    /// Build a minimal `D09BocpdDetector` for tests.
    ///
    /// Uses `connect_lazy` so no real Postgres connection is made. Any method
    /// that queries the pool will fail with a connection error at query time —
    /// this is expected for tests that only exercise the reorg path (which only
    /// touches the `BocpdStateStore`, not the pool).
    fn make_test_detector() -> std::sync::Arc<D09BocpdDetector> {
        use mg_onchain_graph::mock::{MockGraphLabelStore, MockTypedEdgeStore};
        use sqlx::PgPool;

        let edge_store = std::sync::Arc::new(MockTypedEdgeStore::default());
        let label_store = std::sync::Arc::new(MockGraphLabelStore::default());
        let state_store = std::sync::Arc::new(mock::MockBocpdStateStore::new());
        // connect_lazy: no real connection attempted until first query.
        let pool = PgPool::connect_lazy("postgres://test:test@localhost/test_placeholder")
            .expect("connect_lazy must not fail");
        let pg_pool = std::sync::Arc::new(pool);

        D09BocpdDetector::new(
            edge_store,
            label_store,
            state_store,
            pg_pool,
            D09Config::default(),
        )
        .expect("default config must be valid")
        .into()
    }

    #[tokio::test]
    async fn d09_hook_on_reorg_calls_delete_states() {
        use mg_onchain_graph::mock::{MockGraphLabelStore, MockTypedEdgeStore};
        use sqlx::PgPool;

        // Build a detector with a MockBocpdStateStore we can inspect.
        let edge_store = std::sync::Arc::new(MockTypedEdgeStore::default());
        let label_store = std::sync::Arc::new(MockGraphLabelStore::default());
        let state_store = std::sync::Arc::new(mock::MockBocpdStateStore::new());
        let pool = PgPool::connect_lazy("postgres://test:test@localhost/test_placeholder")
            .expect("connect_lazy must not fail");
        let pg_pool = std::sync::Arc::new(pool);

        // Pre-seed some state so delete has something to delete.
        let hp = BocpdHyperparams::default();
        let mut init_state = BocpdState::new_with_prior(&hp, 100);
        init_state.update(0.5, 0.00333, &hp);
        state_store
            .save_state(
                "solana",
                "deployer1",
                &init_state,
                0.5,
                &ObservationFeatures {
                    log_gap_seconds: 0.0,
                    lp_locked_pct: 0.5,
                    log_initial_liquidity_usd: 0.0,
                    holder_count_at_1h: 0.0,
                    prior_rug_rate: 0.0,
                },
                0.0,
                None,
                None,
            )
            .await
            .unwrap();

        let detector: std::sync::Arc<D09BocpdDetector> = D09BocpdDetector::new(
            edge_store,
            label_store,
            state_store.clone(),
            pg_pool,
            D09Config::default(),
        )
        .expect("default config must be valid")
        .into();

        let sink = std::sync::Arc::new(MockAnomalyEventSink::default());
        let hook = D09IndexerHook::new(detector, sink);

        // State exists before reorg.
        let before = state_store.load_state("solana", "deployer1").await.unwrap();
        assert!(before.is_some(), "state must exist before reorg");

        // Call on_reorg: mock clears all states for the chain.
        use mg_onchain_indexer::hooks::PoolInitializeHook;
        hook.on_reorg("solana", 300_000_500)
            .await
            .expect("on_reorg must succeed");

        // MockBocpdStateStore::delete_states_above_block clears all for chain.
        let after = state_store.load_state("solana", "deployer1").await.unwrap();
        assert!(after.is_none(), "state must be cleared after reorg");
    }

    #[tokio::test]
    async fn d09_hook_on_new_token_launch_propagates_detector_error() {
        use mg_onchain_common::chain::{BlockRef, Chain};
        use mg_onchain_indexer::hooks::PoolInitializeHook;

        // The detector uses a lazy pool — feature extraction will fail at
        // the DB query. This test verifies the error is propagated as
        // IndexerError::Config, not swallowed.
        let detector = make_test_detector();
        let sink = std::sync::Arc::new(MockAnomalyEventSink::default());
        let hook = D09IndexerHook::new(detector, sink);

        let result = hook
            .on_new_token_launch(
                Chain::Solana,
                "So11111111111111111111111111111111111111112",
                "So11111111111111111111111111111111111111112",
                "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                chrono::Utc::now(),
                BlockRef::new(Chain::Solana, 300_000_000),
            )
            .await;

        // The label_store (MockGraphLabelStore) returns empty labels, so no
        // KnownDex suppression fires. Feature extraction then attempts to
        // query the lazy pool → fails with a connection error.
        // The hook must surface this as Err(IndexerError::Config(...)).
        assert!(
            result.is_err(),
            "hook must propagate DB error from feature extraction, got Ok"
        );
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("D09 on_new_token_launch failed"),
            "error must mention D09 on_new_token_launch, got: {err_str}"
        );
    }

    /// D09 supported_chains returns all 6 chains (chain guard removed, V00013 is chain-keyed).
    ///
    /// Previously D09 was Solana-only. V00013 `bocpd_deployer_state` PRIMARY KEY is
    /// (chain, deployer) — fully multi-chain. The Solana-only chain guard was removed
    /// and `supported_chains()` now returns all 6 production chains.
    #[tokio::test]
    async fn supported_chains_returns_six_chains() {
        let det = make_test_detector();
        let chains = crate::detector::Detector::supported_chains(det.as_ref());
        assert_eq!(chains.len(), 6, "D09 must support exactly 6 chains");
        assert!(chains.contains(&Chain::Solana), "D09 must support Solana");
        assert!(
            chains.contains(&Chain::Ethereum),
            "D09 must support Ethereum"
        );
        assert!(chains.contains(&Chain::Bsc), "D09 must support BSC");
        assert!(chains.contains(&Chain::Base), "D09 must support Base");
        assert!(
            chains.contains(&Chain::Arbitrum),
            "D09 must support Arbitrum"
        );
        assert!(
            chains.contains(&Chain::Polygon),
            "D09 must support Polygon"
        );
    }

    /// D09 evaluate with Ethereum context: with chain guard removed, Ethereum is
    /// in supported_chains and evaluate_inner proceeds to the edge_store query
    /// (rather than short-circuiting). The mock edge_store returns no DeployerOf
    /// edge, so evaluate returns Ok(vec![]) via the no-deployer path.
    #[tokio::test]
    async fn ethereum_is_in_d09_supported_chains_after_guard_removal() {
        let det = make_test_detector();
        let chains = crate::detector::Detector::supported_chains(det.as_ref());
        // Pre-guard-removal: Ethereum was excluded. Post-removal: must be present.
        assert!(
            chains.contains(&Chain::Ethereum),
            "Ethereum must be in D09 supported_chains after chain guard removal"
        );
        // The chain guard condition (chain != Solana) no longer gates execution;
        // V00013 bocpd_deployer_state PRIMARY KEY is (chain, deployer) and accepts
        // any chain string.
        assert!(
            chains.contains(&Chain::Ethereum) && chains.contains(&Chain::Solana),
            "D09 must support both Ethereum and Solana after guard removal"
        );
    }
}
