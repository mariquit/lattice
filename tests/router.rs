// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for slice `query-router`.
//!
//! One `query` entrypoint routes to graph, hybrid search, or an error with a truthful
//! `query_type` on a common envelope. Four dispatch scenarios plus stream
//! discipline on every path:
//!   1. `query --impact auth.rs`   -> exit 0, query_type "impact"
//!   2. `query "validate ..."`     -> exit 0, query_type "hybrid"
//!   3. `query` (no flag/text)     -> non-zero, query_type "error",
//!                                    error.code "ambiguous_query"
//!   4. `query --impact auth.rs` in an un-initialized dir
//!                                 -> non-zero, query_type "error",
//!                                    error.code "no_lattice"
//!   5. stdout is exactly one JSON object on EVERY path (success and failure).

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

/// Copy the rust_basic fixture into a fresh temp dir (no .gitignore, no .lattice/).
fn fixture_copy() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic");
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

/// Stream discipline: stdout must contain nothing besides a single JSON object
/// (parses with no leading/trailing non-whitespace content).
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
fn router_dispatches_all_four_outcomes() {
    // Fixture T: rust_basic copied and initialized once.
    let t = fixture_copy();
    let t_root = t.path();
    let init_out = run_in(t_root, &["init"]);
    assert!(
        init_out.status.success(),
        "lattice init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    assert!(
        t_root.join(".lattice").is_dir(),
        ".lattice/ must exist after init"
    );

    // Fixture U: a second, empty temp dir that is never initialized.
    let u = tempfile::tempdir().expect("create empty temp dir");
    let u_root = u.path();
    assert!(
        !u_root.join(".lattice").exists(),
        "U must never be initialized"
    );

    // ---- Scenario 1: --impact dispatches to the graph backend ----
    let s1 = run_in(t_root, &["query", "--impact", "auth.rs"]);
    assert!(
        s1.status.success(),
        "scenario 1 must exit 0; stderr: {}",
        String::from_utf8_lossy(&s1.stderr)
    );
    let e1 = parse_single_json(&s1);
    assert_eq!(e1["protocol_version"], "1.5.0");
    assert_eq!(e1["query_type"], "impact");

    // ---- Scenario 2: free text dispatches to the vector backend ----
    let s2 = run_in(t_root, &["query", "validate token signature"]);
    assert!(
        s2.status.success(),
        "scenario 2 must exit 0; stderr: {}",
        String::from_utf8_lossy(&s2.stderr)
    );
    let e2 = parse_single_json(&s2);
    assert_eq!(e2["protocol_version"], "1.5.0");
    assert_eq!(e2["query_type"], "hybrid");

    // ---- Scenario 3: undispatchable intent yields ambiguous_query ----
    let s3 = run_in(t_root, &["query"]);
    assert!(
        !s3.status.success(),
        "scenario 3 must exit non-zero (got success)"
    );
    let e3 = parse_single_json(&s3);
    assert_eq!(e3["protocol_version"], "1.5.0");
    assert_eq!(e3["query_type"], "error");
    assert_eq!(e3["error"]["code"], "ambiguous_query");
    assert!(
        e3["error"]["message"].is_string(),
        "ambiguous_query must carry an error.message string: {e3}"
    );

    // ---- Scenario 4: missing store yields no_lattice before dispatch ----
    let s4 = run_in(u_root, &["query", "--impact", "auth.rs"]);
    assert!(
        !s4.status.success(),
        "scenario 4 must exit non-zero (got success)"
    );
    let e4 = parse_single_json(&s4);
    assert_eq!(e4["protocol_version"], "1.5.0");
    assert_eq!(e4["query_type"], "error");
    assert_eq!(e4["error"]["code"], "no_lattice");

    // ---- Scenario 5: stream discipline already asserted via parse_single_json
    // on all four invocations above (success AND failure paths). ----
}
