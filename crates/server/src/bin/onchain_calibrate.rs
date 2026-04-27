//! `onchain-calibrate` — calibration metrics pass for real-incident fixtures.
//!
//! Loads `tests/fixtures/calibration/evm_real_incidents.json` (7 incidents
//! sourced from rekt.news, Sprint 47), runs a synthetic signal-mapping pass
//! against each incident, and computes detector-level precision / recall metrics.
//!
//! # Purpose
//!
//! Provides a closed-loop calibration signal: given what each incident SHOULD have
//! triggered, does the current detector configuration produce a matching result?
//!
//! # Synthetic replay vs. live replay
//!
//! The incidents in `evm_real_incidents.json` each document a `research_gaps` field
//! that lists missing data (LP pair addresses, full TX sequences, unsupported chains).
//! A full live replay against on-chain data is deferred to Sprint 25+ when the EVM
//! indexer is wired. This pass performs synthetic signal mapping: given the incident
//! descriptor (chain, exploit type, liquidity drain pattern), compute what confidence
//! and severity the detector WOULD emit if it had full data. This lets us:
//!
//! 1. Validate that the detector formula parameters (confidence formula, thresholds)
//!    can theoretically produce the expected confidence.
//! 2. Document incidents where the current detector architecture has structural gaps
//!    (e.g. bridge exploits that bypass token-level LP drain).
//! 3. Emit threshold-adjustment recommendations when the formula undershoots or overshoots.
//!
//! # Output
//!
//! Prints to stdout and saves `tests/fixtures/calibration/calibration_report_{date}.md`.
//!
//! # Exit codes
//!
//! | Code | Meaning                                 |
//! |------|-----------------------------------------|
//! |  0   | Report generated (some mismatches OK)   |
//! |  2   | Fixture load / parse failure            |
//!
//! # Gotcha #22
//!
//! The calibration uses `Utc::now()` ONLY for the output filename (wall-clock date).
//! All per-incident timestamps come from `incident_date` in the fixture JSON.
//! This is the documented exception for batch / reporting tools.

use std::path::PathBuf;

use anyhow::Context as _;
use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::info;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Calibration metrics pass against real-incident fixtures.
///
/// Loads `tests/fixtures/calibration/evm_real_incidents.json`, applies synthetic
/// signal mapping for each detector target, and emits a precision/recall report.
#[derive(Parser)]
#[command(name = "onchain-calibrate", author, version, about)]
struct Cli {
    /// Path to the calibration incident fixture JSON.
    #[arg(
        long,
        default_value = "tests/fixtures/calibration/evm_real_incidents.json"
    )]
    incidents: PathBuf,

    /// Output path for the calibration report markdown file.
    ///
    /// If not set, defaults to `tests/fixtures/calibration/calibration_report_{date}.md`.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Verbose: print per-incident evidence for each detector.
    #[arg(long, short)]
    verbose: bool,
}

// ---------------------------------------------------------------------------
// Fixture types
// ---------------------------------------------------------------------------

/// A single real-incident record from evm_real_incidents.json.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IncidentRecord {
    pub incident_id: String,
    pub chain: String,
    pub name: String,
    pub victim_token: String,
    pub attacker_address: String,
    pub incident_tx_hash: String,
    pub incident_date: String,
    pub usd_lost: u64,
    #[serde(default)]
    pub detector_target: String,
    pub source: String,
    pub expected_severity: String,
    pub expected_confidence_min: f64,
    pub notes: String,
    #[serde(default)]
    pub research_gaps: String,
}

// ---------------------------------------------------------------------------
// Signal mapping
// ---------------------------------------------------------------------------

/// Result of the synthetic signal mapping for one incident.
#[derive(Debug)]
pub struct SignalMappingResult {
    /// Detector ID evaluated.
    pub detector_id: String,
    /// Synthetic actual confidence (what the detector formula produces for this type).
    pub actual_confidence: f64,
    /// Synthetic actual severity.
    pub actual_severity: String,
    /// Whether the actual severity matches expected.
    pub severity_match: bool,
    /// Confidence gap: actual - expected_min. Negative = undershoots.
    pub confidence_gap: f64,
    /// Whether this incident has structural gaps (unsupported chain, missing TX data).
    pub has_structural_gap: bool,
    /// Short note explaining the synthetic mapping.
    pub note: String,
}

/// Map a real incident to a synthetic detector signal result.
///
/// This is the core of the calibration pass. For each incident, we compute
/// what confidence/severity the detector WOULD produce given the described
/// exploit pattern. This is NOT a live detector run — it is a formula-level
/// analysis that exercises the same mathematical relationships.
///
/// # Methodology
///
/// Each detector has a documented confidence formula (see module doc in its
/// source file). We apply the formula with known inputs derived from the
/// incident description:
///
/// - D02 (LP rug pull): confidence_A from LP drain % → classic SafeMoon/Merlin pattern.
/// - D01 (honeypot): Wormhole minting exploit — not a honeypot; structural gap.
/// - D13 (sandwich MEV): Harvest Finance price manipulation — price-manip analog.
/// - D12 (permit2 drainer): Euler / Ronin — bridge/flash-loan; structural gap.
pub fn map_incident_to_signal(incident: &IncidentRecord) -> SignalMappingResult {
    let has_structural_gap = detect_structural_gap(incident);

    match incident.detector_target.as_str() {
        "d02_lp_rug_pull" => map_d02_signal(incident, has_structural_gap),
        "d01_honeypot" => map_d01_signal(incident, has_structural_gap),
        "d13_sandwich_mev" => map_d13_signal(incident, has_structural_gap),
        "d12_permit2_drainer" => map_d12_signal(incident, has_structural_gap),
        _ => SignalMappingResult {
            detector_id: incident.detector_target.clone(),
            actual_confidence: 0.0,
            actual_severity: "Unknown".to_string(),
            severity_match: false,
            confidence_gap: -incident.expected_confidence_min,
            has_structural_gap: true,
            note: format!("Unknown detector_target: '{}'", incident.detector_target),
        },
    }
}

/// Detect whether an incident has a structural gap (unsupported chain, missing TX, etc.).
fn detect_structural_gap(incident: &IncidentRecord) -> bool {
    // Chain not yet supported.
    if incident.chain == "zksync" {
        return true;
    }
    // Victim token is zero address — token contract not captured.
    if incident.victim_token == "0x0000000000000000000000000000000000000000" {
        return true;
    }
    // TX hash is SPEC-NOTE placeholder.
    if incident.incident_tx_hash.starts_with("SPEC-NOTE") {
        return true;
    }
    false
}

/// D02 synthetic signal mapping.
///
/// D02 Signal A fires when LP drain >= 65% in the drain window.
/// For classic rug/drain incidents: assume 100% drain → confidence_A ≈ 0.92.
/// For governance/bridge exploits with LP-drain component: confidence depends on
/// whether the LP drain was token-level vs. bridge-level.
fn map_d02_signal(incident: &IncidentRecord, has_structural_gap: bool) -> SignalMappingResult {
    if has_structural_gap {
        return SignalMappingResult {
            detector_id: "rug_pull_lp_drain".to_string(),
            actual_confidence: 0.0,
            actual_severity: "Undetectable".to_string(),
            severity_match: false,
            confidence_gap: -incident.expected_confidence_min,
            has_structural_gap: true,
            note: "Structural gap: unsupported chain / missing TX / bridge exploit path not LP-level"
                .to_string(),
        };
    }

    // Compute D02 Signal A confidence for 100% LP drain (worst case for all listed incidents).
    // Formula from d02_rug_pull.rs:
    //   lp_removal_threshold = 0.65 (config default)
    //   raw_conf = (drain_pct - threshold) / (1.0 - threshold)  → at 100%: (1.0 - 0.65) / (1.0 - 0.65) = 1.0
    //   confidence_A = clamp(sigmoid(raw_conf * 4.0 - 1.5), 0.75, 1.0)
    //                = sigmoid(4.0 - 1.5) = sigmoid(2.5) ≈ 0.924
    let drain_pct = if incident.usd_lost > 1_000_000 {
        1.0_f64 // 100% drain for large incidents
    } else {
        0.80_f64 // 80% for smaller incidents
    };
    let threshold = 0.65_f64;
    let raw_conf = (drain_pct - threshold) / (1.0 - threshold);
    let sigmoid_arg = raw_conf * 4.0 - 1.5;
    let sigmoid_val = 1.0 / (1.0 + (-sigmoid_arg).exp());
    let actual_confidence = sigmoid_val.clamp(0.75, 1.0);

    let actual_severity = severity_from_confidence(actual_confidence);
    let severity_match = actual_severity.eq_ignore_ascii_case(&incident.expected_severity);
    let confidence_gap = actual_confidence - incident.expected_confidence_min;

    let drain_note = if incident.notes.contains("flash loan")
        || incident.notes.contains("governance")
    {
        "Flash-loan / governance LP drain: D02 Signal A covers LP removal but not governance vector"
    } else {
        "Classic LP drain: D02 Signal A fires at 100% drain threshold"
    };

    SignalMappingResult {
        detector_id: "rug_pull_lp_drain".to_string(),
        actual_confidence,
        actual_severity,
        severity_match,
        confidence_gap,
        has_structural_gap: false,
        note: drain_note.to_string(),
    }
}

/// D01 synthetic signal mapping.
///
/// D01 (honeypot) detects tokens that revert on sell simulation.
/// Wormhole exploit was a fraudulent mint — not a honeypot pattern.
fn map_d01_signal(incident: &IncidentRecord, has_structural_gap: bool) -> SignalMappingResult {
    SignalMappingResult {
        detector_id: "honeypot_sim".to_string(),
        actual_confidence: 0.0,
        actual_severity: if has_structural_gap {
            "Undetectable".to_string()
        } else {
            "Low".to_string()
        },
        severity_match: false, // D01 cannot detect bridge mint exploits
        confidence_gap: -incident.expected_confidence_min,
        has_structural_gap,
        note: "D01 honeypot simulation cannot detect bridge signature-bypass exploit. \
               Structural mismatch: incident is a fraudulent mint, not a sell-revert pattern. \
               D01 architecture gap — would require a separate 'mint_cap_exceeded' signal."
            .to_string(),
    }
}

/// D13 synthetic signal mapping.
///
/// D13 (sandwich MEV) detects sandwich patterns around swaps.
/// Harvest Finance used repeated price manipulation (32 rounds) — this is the
/// closest analog in our detector set, though D13 targets mempool-level sandwiches.
fn map_d13_signal(incident: &IncidentRecord, has_structural_gap: bool) -> SignalMappingResult {
    if has_structural_gap {
        return SignalMappingResult {
            detector_id: "sandwich_mev_v1".to_string(),
            actual_confidence: 0.0,
            actual_severity: "Undetectable".to_string(),
            severity_match: false,
            confidence_gap: -incident.expected_confidence_min,
            has_structural_gap: true,
            note: "Structural gap".to_string(),
        };
    }

    // D13 detects price-impact anomalies in swap sequences.
    // Harvest Finance: 32 rounds of Curve/USDC manipulation → high repeated-swap anomaly.
    // D13 confidence for 32-round price manipulation with $33.8M loss:
    // Estimate: confidence ≈ 0.72 (High) — D13 would fire on repeated large-impact swaps
    // but not at Critical because it's a flash-loan vault exploit, not a pure sandwich.
    let actual_confidence = 0.72_f64;
    let actual_severity = severity_from_confidence(actual_confidence);
    let severity_match = actual_severity.eq_ignore_ascii_case(&incident.expected_severity);
    let confidence_gap = actual_confidence - incident.expected_confidence_min;

    SignalMappingResult {
        detector_id: "sandwich_mev_v1".to_string(),
        actual_confidence,
        actual_severity,
        severity_match,
        confidence_gap,
        has_structural_gap: false,
        note: "Price manipulation sandwich analog: D13 would detect 32-round swap anomaly. \
               Confidence ≈ 0.72 (High) — flash-loan vault exploit, not pure mempool sandwich. \
               D13 sandwich confidence cap limits detection of vault-level exploits. \
               Recommendation: add 'repeated_price_impact' sub-signal to D13."
            .to_string(),
    }
}

/// D12 synthetic signal mapping.
///
/// D12 (permit2 drainer) detects Permit2-based token drains.
/// Euler Finance and Ronin are bridge/governance exploits, not Permit2 drains.
fn map_d12_signal(incident: &IncidentRecord, has_structural_gap: bool) -> SignalMappingResult {
    SignalMappingResult {
        detector_id: "permit2_drainer_v1".to_string(),
        actual_confidence: 0.0,
        actual_severity: "Undetectable".to_string(),
        severity_match: false,
        confidence_gap: -incident.expected_confidence_min,
        has_structural_gap,
        note: "D12 requires Permit2 allowance + transfer sequence. Euler/Ronin are \
               flash-loan / validator-key exploits with no Permit2 signature path. \
               Structural mismatch: these incidents require a dedicated 'flash_loan_governance' \
               detector to reach expected confidence."
            .to_string(),
    }
}

/// Map confidence to severity string (mirrors `severity_from_confidence` in detectors).
fn severity_from_confidence(confidence: f64) -> String {
    if confidence >= 0.85 {
        "Critical".to_string()
    } else if confidence >= 0.65 {
        "High".to_string()
    } else if confidence >= 0.40 {
        "Medium".to_string()
    } else {
        "Low".to_string()
    }
}

// ---------------------------------------------------------------------------
// Metrics aggregation
// ---------------------------------------------------------------------------

/// Per-detector calibration metrics.
#[derive(Debug, Default)]
pub struct DetectorMetrics {
    pub detector_id: String,
    pub sample_count: usize,
    pub severity_matches: usize,
    pub confidence_gap_sum: f64,
    pub structural_gap_count: usize,
}

impl DetectorMetrics {
    pub fn precision(&self) -> f64 {
        if self.sample_count == 0 {
            return 0.0;
        }
        self.severity_matches as f64 / self.sample_count as f64
    }

    pub fn avg_confidence_gap(&self) -> f64 {
        if self.sample_count == 0 {
            return 0.0;
        }
        self.confidence_gap_sum / self.sample_count as f64
    }
}

// ---------------------------------------------------------------------------
// Report generation
// ---------------------------------------------------------------------------

pub fn generate_report(
    incidents: &[IncidentRecord],
    results: &[(IncidentRecord, SignalMappingResult)],
    report_date: &str,
) -> String {
    let mut out = String::new();

    out.push_str(&format!("# Calibration Report — {report_date}\n\n"));
    out.push_str(&format!(
        "**Incidents loaded:** {} from `tests/fixtures/calibration/evm_real_incidents.json`\n\n",
        incidents.len()
    ));
    out.push_str("**Method:** Synthetic signal mapping (live-replay deferred to Sprint 25+).\n");
    out.push_str("Confidence values are formula-level estimates, NOT live detector runs.\n");
    out.push_str("Incidents with `research_gaps` or unsupported chains are flagged as structural gaps.\n\n");
    out.push_str("---\n\n");

    // Per-detector table.
    out.push_str("## Per-Detector Results\n\n");

    // Collect metrics by detector.
    use std::collections::BTreeMap;
    let mut metrics_map: BTreeMap<String, DetectorMetrics> = BTreeMap::new();

    for (incident, result) in results {
        let m = metrics_map
            .entry(result.detector_id.clone())
            .or_insert_with(|| DetectorMetrics {
                detector_id: result.detector_id.clone(),
                ..Default::default()
            });
        m.sample_count += 1;
        if result.severity_match {
            m.severity_matches += 1;
        }
        m.confidence_gap_sum += result.confidence_gap;
        if result.has_structural_gap {
            m.structural_gap_count += 1;
        }

        // Per-incident detail.
        let status = if result.has_structural_gap {
            "STRUCTURAL_GAP"
        } else if result.severity_match {
            "MATCH"
        } else {
            "MISMATCH"
        };

        out.push_str(&format!(
            "### [{status}] {} — {}\n\n",
            incident.incident_id, incident.name
        ));
        out.push_str(&format!("- **Chain:** {}\n", incident.chain));
        out.push_str(&format!(
            "- **Detector:** {}\n",
            result.detector_id
        ));
        out.push_str(&format!(
            "- **Expected:** severity={} confidence_min={:.2}\n",
            incident.expected_severity, incident.expected_confidence_min
        ));
        out.push_str(&format!(
            "- **Actual (synthetic):** severity={} confidence={:.2}\n",
            result.actual_severity, result.actual_confidence
        ));
        out.push_str(&format!(
            "- **Confidence gap:** {:.2} (positive = overshoots, negative = undershoots)\n",
            result.confidence_gap
        ));
        if result.has_structural_gap {
            out.push_str("- **Structural gap:** YES — live replay not possible with current data\n");
        }
        out.push_str(&format!("- **Note:** {}\n\n", result.note));
        out.push_str(&format!("- **Research gaps:** {}\n\n", incident.research_gaps));
        out.push_str("---\n\n");
    }

    // Summary table.
    out.push_str("## Summary by Detector\n\n");
    out.push_str("| Detector | Samples | Severity Match | Avg Confidence Gap | Structural Gaps | Recommendation |\n");
    out.push_str("|----------|---------|----------------|--------------------|-----------------|----------------|\n");

    for (det_id, m) in &metrics_map {
        let precision_str = format!("{}/{} ({:.0}%)", m.severity_matches, m.sample_count, m.precision() * 100.0);
        let gap_str = format!("{:+.2}", m.avg_confidence_gap());
        let recommendation = generate_recommendation(det_id, m);
        out.push_str(&format!(
            "| {det_id} | {} | {precision_str} | {gap_str} | {} | {recommendation} |\n",
            m.sample_count, m.structural_gap_count
        ));
    }

    out.push('\n');

    // Global summary.
    let total = results.len();
    let total_match = results.iter().filter(|(_, r)| r.severity_match).count();
    let total_structural_gap = results.iter().filter(|(_, r)| r.has_structural_gap).count();

    out.push_str("## Global Metrics\n\n");
    out.push_str(&format!("- **Total incidents:** {total}\n"));
    out.push_str(&format!(
        "- **Severity match (synthetic):** {total_match}/{total}\n"
    ));
    out.push_str(&format!(
        "- **Structural gaps (live replay impossible):** {total_structural_gap}/{total}\n"
    ));
    out.push_str(&format!(
        "- **Evaluable incidents (no structural gap):** {}/{total}\n\n",
        total - total_structural_gap
    ));

    out.push_str("## Threshold Adjustment Recommendations\n\n");
    out.push_str(
        "**NOTE:** These are recommendations only. Do NOT modify `config/detectors.toml` \
         thresholds until live-replay data is available (Sprint 25+).\n\n",
    );
    out.push_str(
        "1. **D02 (rug_pull_lp_drain):** Formula produces correct severity for 100% LP drain \
         incidents. Flash-loan governance vectors (Euler, Beanstalk) require a separate \
         `flash_loan_governance` signal — D02 Signal A alone undershoots for these patterns \
         because the drain originates from a governance call, not a direct LP remove.\n\n",
    );
    out.push_str(
        "2. **D01 (honeypot_sim):** Bridge signature-bypass exploits (Wormhole) are structurally \
         undetectable by D01's sell-simulation path. A dedicated `mint_anomaly_rapid` sub-signal \
         or integration with D06 (MintBurnAnomaly) would be needed.\n\n",
    );
    out.push_str(
        "3. **D13 (sandwich_mev_v1):** Vault-level price-manipulation exploits (Harvest Finance) \
         partially map to D13's swap anomaly signals. Adding a `repeated_price_impact` sub-signal \
         with a count threshold (≥5 identical pools in same block) would improve detection. \
         Confidence cap (currently ~0.72 for this class) may need an upward adjustment to \
         Critical range once the sub-signal is added.\n\n",
    );
    out.push_str(
        "4. **D12 (permit2_drainer_v1):** Euler and Ronin exploits bypass the Permit2 path \
         entirely. D12 is correctly scoped to Permit2-based drains. No threshold change needed. \
         These incidents require bridge-level monitoring outside D12's scope.\n\n",
    );

    out.push_str("---\n\n");
    out.push_str(&format!(
        "*Generated by `onchain-calibrate` on {report_date}. \
         Synthetic methodology — live replay pending Sprint 25 EVM indexer integration.*\n"
    ));

    out
}

/// Generate a short recommendation string for the summary table.
fn generate_recommendation(detector_id: &str, m: &DetectorMetrics) -> &'static str {
    if m.structural_gap_count == m.sample_count {
        return "Structural gaps only — live replay needed";
    }
    if m.precision() >= 1.0 {
        return "Thresholds correct (formula match)";
    }
    match detector_id {
        "rug_pull_lp_drain" => {
            "Add flash_loan_governance sub-signal to cover governance vectors"
        }
        "honeypot_sim" => "Add mint_anomaly_rapid sub-signal or integrate with D06",
        "sandwich_mev_v1" => "Add repeated_price_impact sub-signal; raise confidence cap",
        "permit2_drainer_v1" => "Thresholds correct — bridge exploits are out of scope",
        _ => "No recommendation available",
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RUST_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let code = match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("FATAL: {e:#}");
            2
        }
    };

    std::process::exit(code);
}

async fn run(cli: Cli) -> anyhow::Result<i32> {
    info!("onchain-calibrate starting");

    // Load incidents.
    let incidents = load_incidents(&cli.incidents)
        .with_context(|| format!("failed to load incidents from {}", cli.incidents.display()))?;

    info!(count = incidents.len(), "incidents loaded");

    // Apply synthetic signal mapping.
    let results: Vec<(IncidentRecord, SignalMappingResult)> = incidents
        .iter()
        .map(|inc| {
            let result = map_incident_to_signal(inc);
            if cli.verbose {
                println!(
                    "[{}] {} → {} (actual_confidence={:.2}, severity_match={})",
                    if result.severity_match { "MATCH" } else if result.has_structural_gap { "GAP  " } else { "MISS " },
                    inc.incident_id,
                    result.detector_id,
                    result.actual_confidence,
                    result.severity_match
                );
            }
            (inc.clone(), result)
        })
        .collect();

    // Determine output path.
    // NOTE: Utc::now() is used here ONLY for the output filename (gotcha #22 exception:
    // batch/reporting tools may use wall-clock for naming outputs).
    let now = chrono::Utc::now();
    let report_date = now.format("%Y-%m-%d").to_string();
    let output_path = cli.output.unwrap_or_else(|| {
        PathBuf::from(format!(
            "tests/fixtures/calibration/calibration_report_{report_date}.md"
        ))
    });

    // Generate report.
    let report = generate_report(&incidents, &results, &report_date);

    // Print to stdout.
    println!("{report}");

    // Save to file.
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create output directory: {}", parent.display()))?;
    }
    std::fs::write(&output_path, &report)
        .with_context(|| format!("failed to write report to {}", output_path.display()))?;

    info!(path = %output_path.display(), "calibration report written");
    println!("\nReport saved to: {}", output_path.display());

    Ok(0)
}

// ---------------------------------------------------------------------------
// Fixture loading
// ---------------------------------------------------------------------------

fn load_incidents(path: &PathBuf) -> anyhow::Result<Vec<IncidentRecord>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read incident file {}", path.display()))?;
    let incidents: Vec<IncidentRecord> = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse incident JSON from {}", path.display()))?;
    anyhow::ensure!(!incidents.is_empty(), "incident file must contain at least one record");
    Ok(incidents)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_d02_incident(chain: &str, usd_lost: u64, victim_token: &str, tx_hash: &str) -> IncidentRecord {
        IncidentRecord {
            incident_id: "test-d02-001".to_string(),
            chain: chain.to_string(),
            name: "Test D02 LP Drain".to_string(),
            victim_token: victim_token.to_string(),
            attacker_address: "0x1234000000000000000000000000000000000001".to_string(),
            incident_tx_hash: tx_hash.to_string(),
            incident_date: "2023-01-01T00:00:00Z".to_string(),
            usd_lost,
            detector_target: "d02_lp_rug_pull".to_string(),
            source: "rekt.news/test".to_string(),
            expected_severity: "Critical".to_string(),
            expected_confidence_min: 0.85,
            notes: "Classic LP drain.".to_string(),
            research_gaps: "".to_string(),
        }
    }

    /// D02 large drain (>$1M) → Critical severity via formula.
    #[test]
    fn d02_large_incident_maps_to_critical() {
        let incident = make_d02_incident(
            "ethereum",
            197_000_000,
            "0xd9fcd98c322942075a5c3860693e9f4f03aae07b",
            "0xc310a0affe2169d1f6feec1c63dbc7f7c62a887fa48795d327d4d2da2d6b111d",
        );
        let result = map_d02_signal(&incident, false);
        assert_eq!(result.detector_id, "rug_pull_lp_drain");
        // 100% drain → confidence ≈ 0.924 → Critical (≥ 0.85)
        assert!(
            result.actual_confidence >= 0.85,
            "D02 100% drain should produce Critical confidence, got {:.2}",
            result.actual_confidence
        );
        assert_eq!(result.actual_severity, "Critical");
    }

    /// D02 structural gap (zksync) → Undetectable.
    #[test]
    fn d02_structural_gap_returns_undetectable() {
        let mut incident = make_d02_incident(
            "zksync",
            1_800_000,
            "0x0000000000000000000000000000000000000000",
            "SPEC-NOTE:not-captured",
        );
        incident.chain = "zksync".to_string();
        let has_gap = detect_structural_gap(&incident);
        assert!(has_gap, "zksync incident must be detected as structural gap");
        let result = map_d02_signal(&incident, has_gap);
        assert_eq!(result.actual_severity, "Undetectable");
        assert!(result.has_structural_gap);
    }

    /// D01 bridge exploit → structural mismatch (not a honeypot).
    #[test]
    fn d01_bridge_exploit_cannot_be_detected_by_honeypot() {
        let incident = IncidentRecord {
            incident_id: "test-d01-wormhole".to_string(),
            chain: "solana".to_string(),
            name: "Wormhole bridge exploit".to_string(),
            victim_token: "0x0000000000000000000000000000000000000000".to_string(),
            attacker_address: "0x629e7da20197a5429d30da36e77d06cdf796b71a".to_string(),
            incident_tx_hash: "0x4d5201dd4a377f20e61fb8f42e6f929ec16bcec918f0584e39241d15b254a80f".to_string(),
            incident_date: "2022-02-02T00:00:00Z".to_string(),
            usd_lost: 326_000_000,
            detector_target: "d01_honeypot".to_string(),
            source: "rekt.news/wormhole-rekt/".to_string(),
            expected_severity: "Critical".to_string(),
            expected_confidence_min: 0.75,
            notes: "Fraudulent minting of 120k whETH on Solana.".to_string(),
            research_gaps: "Solana-side attacker address not captured.".to_string(),
        };
        let has_gap = detect_structural_gap(&incident);
        // Victim token is 0x0000... → structural gap.
        assert!(has_gap);
        let result = map_d01_signal(&incident, has_gap);
        // D01 cannot detect this → confidence = 0.
        assert_eq!(result.actual_confidence, 0.0);
        assert!(!result.severity_match);
    }

    /// severity_from_confidence: thresholds match detector output conventions.
    #[test]
    fn severity_thresholds_match_detector_conventions() {
        assert_eq!(severity_from_confidence(0.92), "Critical");
        assert_eq!(severity_from_confidence(0.85), "Critical");
        assert_eq!(severity_from_confidence(0.72), "High");
        assert_eq!(severity_from_confidence(0.65), "High");
        assert_eq!(severity_from_confidence(0.50), "Medium");
        assert_eq!(severity_from_confidence(0.39), "Low");
    }

    /// Report generation: does not panic for 0 incidents.
    #[test]
    fn generate_report_empty_does_not_panic() {
        let report = generate_report(&[], &[], "2026-04-24");
        assert!(report.contains("Calibration Report"));
        assert!(report.contains("0 from"));
    }

    /// Incident loading: fixture file round-trip (if file exists).
    #[test]
    fn incident_fixture_round_trip() {
        let json = r#"[{
            "incident_id": "test-001",
            "chain": "ethereum",
            "name": "Test Incident",
            "victim_token": "0x1234000000000000000000000000000000000001",
            "attacker_address": "0x5678000000000000000000000000000000000002",
            "incident_tx_hash": "0xabcd000000000000000000000000000000000000000000000000000000000003",
            "incident_date": "2023-01-01T00:00:00Z",
            "usd_lost": 1000000,
            "detector_target": "d02_lp_rug_pull",
            "source": "rekt.news/test/",
            "expected_severity": "Critical",
            "expected_confidence_min": 0.85,
            "notes": "Test.",
            "research_gaps": ""
        }]"#;
        let incidents: Vec<IncidentRecord> = serde_json::from_str(json).expect("must parse");
        assert_eq!(incidents.len(), 1);
        assert_eq!(incidents[0].expected_severity, "Critical");
        assert!(incidents[0].expected_confidence_min > 0.8 - f64::EPSILON);
    }
}
