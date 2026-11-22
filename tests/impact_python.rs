// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for slice `python-ingestion`.
//!
//! Scenarios:
//!   1. `lattice init` ingests the 4 Python fixture sources.
//!   2. `lattice query --impact auth.py` returns hand-counted ground truth.
//!   3. Stream discipline: stdout is exactly one JSON object per invocation.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

/// Copy the python_basic fixture into a fresh temp dir (no .lattice/).
fn fixture_copy() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python_basic");
    for entry in fs::read_dir(&src).expect("read fixture dir") {
        let entry = entry.expect("fixture dir entry");
        let name = entry.file_name();
        fs::copy(entry.path(), tmp.path().join(&name)).expect("copy fixture file");
    }
    tmp
}

fn run_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn lattice binary")
}

/// Scenario 3 discipline: stdout must contain nothing besides a single JSON
/// object (parses with no leading/trailing non-whitespace content).
fn parse_single_json(output: &Output) -> Value {
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8");
    let trimmed = stdout.trim();
    assert!(
        trimmed.starts_with('{') && trimmed.ends_with('}'),
        "stdout is not a single JSON object: {stdout:?}"
    );
    // serde_json::from_str fails on trailing non-whitespace content, so this
    // also proves there is exactly one JSON value on stdout.
    serde_json::from_str::<Value>(trimmed)
        .unwrap_or_else(|e| panic!("stdout is not exactly one JSON object ({e}): {stdout:?}"))
}

#[test]
fn init_and_python_impact_query_end_to_end() {
    let tmp = fixture_copy();
    let root = tmp.path();
    assert!(!root.join(".lattice").exists());

    // ---- Scenario 1: init ingests Python sources ----
    let init_out = run_in(root, &["init"]);
    assert!(
        init_out.status.success(),
        "lattice init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    let init_env = parse_single_json(&init_out);
    assert_eq!(init_env["protocol_version"], "1.5.0");
    assert_eq!(init_env["status"], "ok");
    assert_eq!(init_env["files_indexed"], 4);

    // ---- Scenario 2: impact query returns hand-counted Python ground truth ----
    let query_out = run_in(root, &["query", "--impact", "auth.py"]);
    assert!(
        query_out.status.success(),
        "lattice query --impact exited non-zero; stderr: {}",
        String::from_utf8_lossy(&query_out.stderr)
    );
    let envelope = parse_single_json(&query_out);
    assert_eq!(envelope["protocol_version"], "1.5.0");
    assert_eq!(envelope["query_type"], "impact");
    assert_eq!(envelope["target"], "auth.py");
    assert_eq!(envelope["callers"], serde_json::json!(["app.py"]));
    assert_eq!(
        envelope["dependents"],
        serde_json::json!(["app.py", "metrics.py"])
    );
    assert_eq!(envelope["blast_radius"], 2);
}
