//! JQ Integration Tests
//!
//! Comprehensive end-to-end tests for the jq command implementation.
//! Tests cover basic filters, pipes, options, and complex expressions.
//!
//! To run these tests:
//!   1. Build the WASM: `cargo build --release -p amla-sandbox --target wasm32-wasip1`
//!   2. Run tests: `cargo test -p amla-sandbox --test jq_integration`

use std::path::PathBuf;
use std::process::Command;

/// Check if Node.js is available
fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Get the path to the WASI WASM binary
fn wasm_path() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir)
        .parent()
        .unwrap()
        .join("target")
        .join("wasm32-wasip1")
        .join("release")
        .join("amla_sandbox.wasm")
}

/// Get the path to the JQ test harness
fn harness_path() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir)
        .join("tests")
        .join("jq_harness.mjs")
}

#[test]
fn test_jq_e2e() {
    if !node_available() {
        eprintln!("SKIP: Node.js not available");
        return;
    }

    let wasm = wasm_path();
    if !wasm.exists() {
        eprintln!(
            "SKIP: WASM binary not found at {wasm:?}\n\
             Build with: cargo build --release -p amla-sandbox --target wasm32-wasip1"
        );
        return;
    }

    let harness = harness_path();
    assert!(harness.exists(), "JQ test harness not found at {harness:?}");

    let output = Command::new("node")
        .arg(&harness)
        .arg(&wasm)
        .output()
        .expect("Failed to execute node");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Print output for visibility
    println!("STDOUT:\n{stdout}");
    if !stderr.is_empty() {
        eprintln!("STDERR:\n{stderr}");
    }

    assert!(
        output.status.success(),
        "JQ test harness failed with exit code {:?}\n\nSTDOUT:\n{stdout}\n\nSTDERR:\n{stderr}",
        output.status.code(),
    );

    // Verify expected output
    assert!(
        stdout.contains("JQ E2E Test Harness"),
        "Missing harness header in output"
    );
    assert!(
        stdout.contains("All JQ Tests Passed"),
        "JQ tests did not all pass:\n{stdout}"
    );
}
