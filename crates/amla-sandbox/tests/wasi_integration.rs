//! WASI Integration Tests
//!
//! These tests verify the WASM runtime loads correctly via Node.js WASI.
//! Tests are skipped gracefully if Node.js is not installed or WASM not built.
//!
//! To run these tests:
//!   1. Build the WASM: `cargo build --release -p amla-sandbox --target wasm32-wasip1`
//!   2. Run tests: `cargo test -p amla-sandbox --test wasi_integration`

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

/// Get the path to the test harness
fn harness_path() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir)
        .join("tests")
        .join("wasi_harness.mjs")
}

#[test]
fn test_wasi_harness() {
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
    assert!(harness.exists(), "Test harness not found at {harness:?}");

    let output = Command::new("node")
        .arg(&harness)
        .arg(&wasm)
        .output()
        .expect("Failed to execute node");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "WASI harness failed with exit code {:?}\n\nSTDOUT:\n{stdout}\n\nSTDERR:\n{stderr}",
        output.status.code(),
    );

    // Verify expected output
    assert!(
        stdout.contains("WASI Test Harness"),
        "Missing harness header in output"
    );
    assert!(
        stdout.contains("WASI Test Passed"),
        "Test did not pass:\n{stdout}"
    );

    println!("WASI integration test passed:\n{stdout}");
}
