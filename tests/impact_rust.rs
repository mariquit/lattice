// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for slice `walking-skeleton-rust-impact`.
//!
//! Scenarios:
//!   1. `lattice init` builds the store and emits the init envelope.
//!   2. The `.gitignore` guard is created and idempotent across re-runs.
//!   3. `lattice query --impact auth.rs` returns hand-counted ground truth.
//!   4. Stream discipline: stdout is exactly one JSON object per invocation.

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

/// Scenario 4 discipline: stdout must contain nothing besides a single JSON
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

fn gitignore_lattice_lines(dir: &Path) -> usize {
    let content = fs::read_to_string(dir.join(".gitignore")).expect(".gitignore exists");
    content
        .lines()
        .filter(|line| line.trim() == ".lattice/")
        .count()
}

#[test]
fn init_and_impact_query_end_to_end() {
    let tmp = fixture_copy();
    let root = tmp.path();
    assert!(!root.join(".lattice").exists());
    assert!(!root.join(".gitignore").exists());

    // ---- Scenario 1: init builds the store and the envelope ----
    let init_out = run_in(root, &["init"]);
    assert!(
        init_out.status.success(),
        "lattice init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    let env1 = parse_single_json(&init_out);
    assert_eq!(env1["protocol_version"], "1.5.0");
    assert_eq!(env1["command"], "init");
    assert_eq!(env1["status"], "ok");
    assert_eq!(env1["files_indexed"], 4);
    assert!(
        root.join(".lattice/graph.sparrow").is_dir(),
        ".lattice/graph.sparrow/ must exist after init"
    );
    assert!(
        root.join(".lattice/meta.db").is_file(),
        ".lattice/meta.db must exist after init"
    );

    // ---- Scenario 2: .gitignore guard created and idempotent ----
    assert_eq!(
        gitignore_lattice_lines(root),
        1,
        ".gitignore must contain exactly one `.lattice/` line after first init"
    );
    let reinit_out = run_in(root, &["init"]);
    assert!(
        reinit_out.status.success(),
        "second lattice init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&reinit_out.stderr)
    );
    parse_single_json(&reinit_out); // stream discipline holds on re-run too
    assert_eq!(
        gitignore_lattice_lines(root),
        1,
        ".gitignore must still contain exactly one `.lattice/` line after re-init"
    );

    // ---- Scenario 3: impact query returns hand-counted ground truth ----
    let query_out = run_in(root, &["query", "--impact", "auth.rs"]);
    assert!(
        query_out.status.success(),
        "lattice query --impact exited non-zero; stderr: {}",
        String::from_utf8_lossy(&query_out.stderr)
    );
    let envelope = parse_single_json(&query_out);
    assert_eq!(envelope["protocol_version"], "1.5.0");
    assert_eq!(envelope["query_type"], "impact");
    assert_eq!(envelope["target"], "auth.rs");
    assert_eq!(envelope["callers"], serde_json::json!(["main.rs"]));
    assert_eq!(
        envelope["dependents"],
        serde_json::json!(["main.rs", "metrics.rs"])
    );
    assert_eq!(envelope["blast_radius"], 2);
}
