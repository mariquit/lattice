// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for slice `typescript-ingestion`.
//!
//! Scenarios:
//!   1. `lattice init` ingests the 5 TypeScript/TSX fixture sources
//!      (4 .ts + 1 mandatory .tsx).
//!   2. `lattice query --impact auth.ts` returns hand-counted ground truth,
//!      proving a `.tsx` caller resolves and import-without-call counts as a
//!      dependent only.
//!   3. Stream discipline: stdout is exactly one JSON object per invocation.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

/// Copy the typescript_basic fixture into a fresh temp dir (no .lattice/).
fn fixture_copy() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/typescript_basic");
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
    serde_json::from_str::<Value>(trimmed)
        .unwrap_or_else(|e| panic!("stdout is not exactly one JSON object ({e}): {stdout:?}"))
}

#[test]
fn init_and_typescript_impact_query_end_to_end() {
    let tmp = fixture_copy();
    let root = tmp.path();
    assert!(!root.join(".lattice").exists());

    // ---- Scenario 1: init ingests TS and TSX sources ----
    let init_out = run_in(root, &["init"]);
    assert!(
        init_out.status.success(),
        "lattice init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    let init_env = parse_single_json(&init_out);
    assert_eq!(init_env["protocol_version"], "1.5.0");
    assert_eq!(init_env["status"], "ok");
    assert_eq!(init_env["files_indexed"], 5);

    // ---- Scenario 2: impact query returns hand-counted TS ground truth ----
    let query_out = run_in(root, &["query", "--impact", "auth.ts"]);
    assert!(
        query_out.status.success(),
        "lattice query --impact exited non-zero; stderr: {}",
        String::from_utf8_lossy(&query_out.stderr)
    );
    let envelope = parse_single_json(&query_out);
    assert_eq!(envelope["protocol_version"], "1.5.0");
    assert_eq!(envelope["query_type"], "impact");
    assert_eq!(envelope["target"], "auth.ts");
    assert_eq!(
        envelope["callers"],
        serde_json::json!(["app.ts", "view.tsx"])
    );
    assert_eq!(
        envelope["dependents"],
        serde_json::json!(["app.ts", "metrics.ts", "view.tsx"])
    );
    assert_eq!(envelope["blast_radius"], 3);
}
