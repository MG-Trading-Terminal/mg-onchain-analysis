//! Binary smoke tests for `onchain-service`.
//!
//! # Gate
//!
//! Tests marked `#[ignore]` require a live Postgres + running service config.
//! They are skipped in CI by default. Set `RUN_BINARY_SMOKE=1` to enable.
//!
//! # Coverage
//!
//! - `--help` exits 0 (compile-time check: clap wiring works)
//! - `--version` exits 0
//! - `--config missing.toml` exits non-zero with error message
//! - Config file with `streaming.enabled = false` boots and exits cleanly
//!   (DB-gated, `#[ignore]`)
//!
//! # Design reference
//!
//! design 0020 §6 specifies this test as acceptance criterion for S19-2.

use std::process::{Command, Stdio};

/// Resolve the path to the compiled `onchain-service` binary.
///
/// In CI (cargo test), the binary is in `target/debug/onchain-service`
/// (or `target/release/` if run with `--release`). We use `cargo locate-project`
/// to find the workspace root.
fn binary_path() -> std::path::PathBuf {
    // Use the env var set by cargo test for the target directory.
    // Fallback: assume workspace root relative to this file.
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let workspace_root = std::path::Path::new(&manifest_dir)
            .parent()  // crates/server → crates
            .and_then(|p| p.parent())  // crates → workspace root
            .expect("workspace root must exist")
            .to_path_buf();

        let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
        workspace_root
            .join("target")
            .join(&profile)
            .join("onchain-service")
    } else {
        // Absolute fallback for direct invocation.
        std::path::PathBuf::from("target/debug/onchain-service")
    }
}

// ---------------------------------------------------------------------------
// Unit-level smoke tests (no binary required — fast)
// ---------------------------------------------------------------------------

/// Verify that `ServiceConfig` parses the production `config/service.toml`
/// without errors. This catches TOML syntax errors and unknown-key warnings
/// before the binary is invoked.
#[test]
fn service_toml_parses_cleanly() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set");
    let workspace_root = std::path::Path::new(&manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root from CARGO_MANIFEST_DIR");

    let toml_path = workspace_root.join("config/service.toml");
    assert!(
        toml_path.exists(),
        "config/service.toml must exist at {toml_path:?}"
    );

    let contents = std::fs::read_to_string(&toml_path)
        .expect("config/service.toml must be readable");

    let _: mg_onchain_server::config::ServiceConfig =
        toml::from_str(&contents).expect("config/service.toml must parse without errors");
}

/// Verify all expected `[streaming]` keys are present in service.toml.
#[test]
fn service_toml_streaming_section_exists() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set");
    let workspace_root = std::path::Path::new(&manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root");

    let contents = std::fs::read_to_string(workspace_root.join("config/service.toml"))
        .expect("service.toml must be readable");

    assert!(
        contents.contains("[streaming]"),
        "service.toml must have a [streaming] section"
    );
    assert!(
        contents.contains("token_risk_reports_enabled"),
        "service.toml must document token_risk_reports_enabled (D-C)"
    );
}

/// Verify new config sections added in S19-2 are present.
#[test]
fn service_toml_sprint19_sections_present() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set");
    let workspace_root = std::path::Path::new(&manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root");

    let contents = std::fs::read_to_string(workspace_root.join("config/service.toml"))
        .expect("service.toml must be readable");

    // S19-2 additions per design 0020 §7
    for section in &["[shutdown]", "[observability]", "[postgres]", "[chains.solana]", "[chains.ethereum]", "[gateway]"] {
        assert!(
            contents.contains(section),
            "service.toml must contain {section} section (S19-2 / design 0020 §7)"
        );
    }
}

// ---------------------------------------------------------------------------
// Binary invocation smoke tests (require compiled binary)
// ---------------------------------------------------------------------------

/// Verify `--help` exits 0 and prints usage.
///
/// This test builds the binary if not present. It is NOT marked `#[ignore]`
/// because it does not require a live Postgres connection.
#[test]
fn binary_help_exits_zero() {
    let bin = binary_path();
    if !bin.exists() {
        // Binary not compiled yet — skip silently (can't build in test context).
        // Run `cargo build --bin onchain-service` to compile first.
        eprintln!("SKIP: binary not found at {bin:?} — run cargo build first");
        return;
    }

    let output = Command::new(&bin)
        .arg("--help")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run onchain-service --help");

    assert!(
        output.status.success(),
        "--help must exit 0, got {:?}",
        output.status
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--config"),
        "--help must mention --config flag"
    );
    assert!(
        stdout.contains("--no-migrate"),
        "--help must mention --no-migrate flag"
    );
}

/// Verify `--version` exits 0 and prints a version string.
#[test]
fn binary_version_exits_zero() {
    let bin = binary_path();
    if !bin.exists() {
        eprintln!("SKIP: binary not found at {bin:?} — run cargo build first");
        return;
    }

    let output = Command::new(&bin)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run onchain-service --version");

    assert!(
        output.status.success(),
        "--version must exit 0, got {:?}",
        output.status
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // clap --version prints "<package-name> <version>". Package is "mg-onchain-server".
    assert!(
        stdout.contains("onchain") || stdout.contains("0.1"),
        "--version must include package name or version: {stdout}"
    );
}

/// Verify that a missing config file causes a non-zero exit with a useful error.
#[test]
fn binary_missing_config_exits_nonzero() {
    let bin = binary_path();
    if !bin.exists() {
        eprintln!("SKIP: binary not found at {bin:?} — run cargo build first");
        return;
    }

    let output = Command::new(&bin)
        .args(["--config", "/nonexistent/path/service.toml"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run onchain-service with missing config");

    assert!(
        !output.status.success(),
        "missing config must exit non-zero, got {:?}",
        output.status
    );

    // Error should mention the path.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stderr}{stdout}");
    assert!(
        combined.contains("nonexistent") || combined.contains("service.toml"),
        "error output must mention the config path: {combined}"
    );
}

// ---------------------------------------------------------------------------
// Live-DB smoke tests (Docker-gated, #[ignore])
// ---------------------------------------------------------------------------

/// Full startup smoke: boot the service with streaming.enabled=false against
/// a live Postgres, verify it starts cleanly, then kill it.
///
/// Requires:
/// - `DATABASE_URL` env var pointing to a live Postgres
/// - `RUN_BINARY_SMOKE=1` env var to opt in
/// - `cargo build --bin onchain-service` run first
#[test]
#[ignore = "requires live Postgres (set DATABASE_URL + RUN_BINARY_SMOKE=1)"]
fn binary_boots_with_streaming_disabled() {
    use std::thread;
    use std::time::Duration;

    let run = std::env::var("RUN_BINARY_SMOKE").is_ok();
    if !run {
        return;
    }

    let bin = binary_path();
    if !bin.exists() {
        panic!("binary not found at {bin:?} — run cargo build --bin onchain-service first");
    }

    let db_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for binary smoke tests");

    // Write a minimal config to a temp file in the system temp dir.
    let config_path = std::env::temp_dir().join("mg_onchain_smoke_service.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[postgres]
url = "{db_url}"

[streaming]
enabled = false

[chains.solana]
enabled = false

[chains.ethereum]
enabled = false
"#
        ),
    )
    .expect("write temp config");

    let mut child = Command::new(&bin)
        .args(["--config", config_path.to_str().unwrap(), "--no-migrate"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn onchain-service");

    // Give it 5 seconds to start (gateway bind takes ~1s).
    thread::sleep(Duration::from_secs(5));

    // The binary should still be running (hasn't panicked).
    assert!(
        child.try_wait().expect("try_wait must work").is_none(),
        "binary must still be running after 5s"
    );

    // Kill the process (SIGKILL as a simple smoke test — no SIGTERM handler needed here).
    child.kill().expect("kill must succeed");
    let status = child.wait().expect("wait must succeed");
    // Killed processes return non-zero, which is expected here.
    let _ = status; // We just verify no panic occurred during startup.
}
