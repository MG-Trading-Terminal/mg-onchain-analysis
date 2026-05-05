//! `onchain-score-server` — thin HTTP wrapper around `onchain-check-token`.
//!
//! Sprint 27 T27-35: when a consumer (bot-trader / mg-custody / market
//! maker / exchange) needs the on-chain risk verdict over HTTP rather than
//! linking the Rust crate, this binary exposes:
//!
//!   `GET /v1/score?chain=ethereum&token=0xdAC17F958D…`
//!
//! Internally it spawns `onchain-check-token <token> --chain <chain>` as a
//! subprocess, captures stdout, and parses the composite verdict +
//! driving signals into a JSON response. Single binary, no Postgres, no
//! ClickHouse — exactly the consumer-facing surface from
//! `feedback_cli_first_product` memory ("wrap the CLI in a REST endpoint").
//!
//! Run:
//!   cargo run --release --bin onchain-score-server -- --bind 0.0.0.0:8080

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::Context;
use axum::{
    Json, Router,
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

#[derive(Parser, Debug)]
#[command(name = "onchain-score-server", about = "HTTP JSON wrapper around onchain-check-token")]
struct Args {
    /// Address+port to bind. Default `127.0.0.1:8080`.
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: String,

    /// Optional explicit path to the `onchain-check-token` binary. By
    /// default the server walks `current_exe()` and looks in the same
    /// directory.
    #[arg(long)]
    cli_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct AppState {
    cli_path: PathBuf,
}

#[derive(Deserialize, Debug)]
struct ScoreQuery {
    /// Solana base58 mint, EVM 0x-address, or symbol like "USDT".
    token: String,
    /// Optional `solana` / `ethereum` / `bsc`. Auto-detected when omitted.
    #[serde(default)]
    chain: Option<String>,
}

#[derive(Serialize, Debug)]
struct ScoreResponse {
    /// The token argument we received, after normalisation.
    token: String,
    /// Resolved chain (`solana` / `ethereum` / `bsc`) — when the CLI
    /// detected one. `null` when the lookup failed.
    chain: Option<String>,
    /// `INFO / clean` / `LOW` / `MEDIUM` / `HIGH` / `CRITICAL`.
    verdict: String,
    /// Composite confidence ∈ [0, 1].
    confidence: f64,
    /// Top-3 detector signals contributing to the composite (highest
    /// confidence first), each `{label, detector_id, confidence,
    /// rationale}`.
    driving_signals: Vec<DrivingSignal>,
    /// Detector IDs that did not fire (RPC throttle, missing infra,
    /// not yet wired) with the reason given by the CLI.
    coverage_gap: Vec<CoverageGapEntry>,
    /// Verbatim recommendation line emitted by the CLI.
    recommendation: String,
    /// Wall-clock wall time the analysis took (seconds).
    duration_secs: f64,
}

#[derive(Serialize, Debug)]
struct DrivingSignal {
    label: String,
    detector_id: String,
    confidence: f64,
    rationale: String,
}

#[derive(Serialize, Debug)]
struct CoverageGapEntry {
    detector_id: String,
    reason: String,
}

#[derive(Serialize, Debug)]
struct ErrorResponse {
    error: String,
    detail: Option<String>,
}

enum AppError {
    BadRequest(String),
    Upstream(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            AppError::BadRequest(e) => (StatusCode::BAD_REQUEST, e),
            AppError::Upstream(e) => (StatusCode::BAD_GATEWAY, e),
        };
        (
            status,
            Json(ErrorResponse {
                error,
                detail: None,
            }),
        )
            .into_response()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cli_path = match args.cli_path {
        Some(p) => p,
        None => {
            // Default: same directory as the score-server itself.
            let mut p = std::env::current_exe().context("resolve current_exe")?;
            p.pop();
            p.push("onchain-check-token");
            p
        }
    };
    if !cli_path.exists() {
        anyhow::bail!(
            "onchain-check-token binary not found at {}; pass --cli-path explicitly",
            cli_path.display()
        );
    }
    let state = AppState { cli_path };

    let app = Router::new()
        .route("/v1/score", get(score_handler))
        .route("/healthz", get(health_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("bind {}", args.bind))?;
    eprintln!("[score-server] listening on http://{}", args.bind);
    eprintln!("[score-server] try: curl 'http://{}/v1/score?token=USDT'", args.bind);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health_handler() -> &'static str {
    "ok"
}

async fn score_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    Query(q): Query<ScoreQuery>,
) -> Result<Json<ScoreResponse>, AppError> {
    if q.token.is_empty() {
        return Err(AppError::BadRequest("missing 'token' query parameter".to_owned()));
    }
    if q.token.len() > 80 {
        return Err(AppError::BadRequest(
            "'token' parameter too long (max 80 chars)".to_owned(),
        ));
    }
    if let Some(ref c) = q.chain {
        if !matches!(c.as_str(), "solana" | "ethereum" | "bsc") {
            return Err(AppError::BadRequest(format!(
                "unsupported chain: {c} (allowed: solana, ethereum, bsc)"
            )));
        }
    }

    let started = std::time::Instant::now();
    let mut cmd = Command::new(&state.cli_path);
    cmd.arg(&q.token);
    if let Some(ref c) = q.chain {
        cmd.arg("--chain").arg(c);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let output = cmd
        .output()
        .await
        .map_err(|e| AppError::Upstream(format!("spawn CLI: {e}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        // Non-zero exit usually means CLI couldn't resolve token or RPC
        // failed. Return the upstream message so the caller knows what
        // happened.
        let detail = if !stderr.is_empty() {
            stderr.to_string()
        } else {
            stdout.to_string()
        };
        return Err(AppError::Upstream(format!(
            "onchain-check-token failed: {detail}"
        )));
    }

    let parsed = parse_cli_output(&q.token, &stdout);
    let duration = started.elapsed().as_secs_f64();

    Ok(Json(ScoreResponse {
        token: parsed.token,
        chain: parsed.chain,
        verdict: parsed.verdict,
        confidence: parsed.confidence,
        driving_signals: parsed.driving_signals,
        coverage_gap: parsed.coverage_gap,
        recommendation: parsed.recommendation,
        duration_secs: duration,
    }))
}

struct Parsed {
    token: String,
    chain: Option<String>,
    verdict: String,
    confidence: f64,
    driving_signals: Vec<DrivingSignal>,
    coverage_gap: Vec<CoverageGapEntry>,
    recommendation: String,
}

fn parse_cli_output(input_token: &str, stdout: &str) -> Parsed {
    let mut token = input_token.to_owned();
    let mut chain: Option<String> = None;
    let mut verdict = "UNKNOWN".to_owned();
    let mut confidence = 0.0_f64;
    let mut driving_signals: Vec<DrivingSignal> = Vec::new();
    let mut coverage_gap: Vec<CoverageGapEntry> = Vec::new();
    let mut recommendation = String::new();

    let mut in_driving = false;
    let mut in_coverage = false;

    for raw_line in stdout.lines() {
        let line = raw_line.trim_end();
        let trimmed = line.trim_start();

        if let Some(rest) = trimmed.strip_prefix("token: ") {
            token = rest.trim().to_owned();
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("chain: ") {
            chain = Some(rest.trim().to_owned());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("composite verdict: ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if !parts.is_empty() {
                verdict = parts[0].to_owned();
                if parts.len() >= 3 && parts[0] == "INFO" && parts[1] == "/" {
                    verdict = "INFO".to_owned();
                }
            }
            if let Some(idx) = rest.find("confidence ") {
                let tail = &rest[idx + "confidence ".len()..];
                let num: String = tail
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                confidence = num.parse().unwrap_or(0.0);
            }
            continue;
        }
        if trimmed.starts_with("driving signals (highest first):") {
            in_driving = true;
            in_coverage = false;
            continue;
        }
        if trimmed.starts_with("coverage gap") {
            in_driving = false;
            in_coverage = true;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("RECOMMENDATION:") {
            recommendation = rest.trim().to_owned();
            in_driving = false;
            in_coverage = false;
            continue;
        }
        if in_driving
            && (trimmed.starts_with("HIGH ")
                || trimmed.starts_with("MEDIUM ")
                || trimmed.starts_with("LOW ")
                || trimmed.starts_with("INFO "))
        {
            // Format: "HIGH d10_launch_audit  conf 1.00  — rationale"
            let parts: Vec<&str> = trimmed.splitn(2, "  ").collect();
            let head = parts[0];
            let head_parts: Vec<&str> = head.split_whitespace().collect();
            let label = head_parts.first().copied().unwrap_or("?").to_owned();
            let detector_id = head_parts.get(1).copied().unwrap_or("?").to_owned();
            let mut conf = 0.0_f64;
            let mut rationale = String::new();
            for piece in trimmed.split("  ").skip(1) {
                if let Some(after) = piece.trim().strip_prefix("conf ") {
                    let num: String = after
                        .chars()
                        .take_while(|c| c.is_ascii_digit() || *c == '.')
                        .collect();
                    conf = num.parse().unwrap_or(0.0);
                } else if let Some(after) = piece.trim().strip_prefix("— ") {
                    rationale = after.to_owned();
                }
            }
            driving_signals.push(DrivingSignal {
                label,
                detector_id,
                confidence: conf,
                rationale,
            });
        }
        if in_coverage
            && let Some(rest) = trimmed.strip_prefix("- ")
        {
            let mut parts = rest.splitn(2, " — ");
            let detector_id = parts.next().unwrap_or("?").trim().to_owned();
            let reason = parts.next().unwrap_or("").trim().to_owned();
            coverage_gap.push(CoverageGapEntry {
                detector_id,
                reason,
            });
        }
    }

    Parsed {
        token,
        chain,
        verdict,
        confidence,
        driving_signals,
        coverage_gap,
        recommendation,
    }
}
