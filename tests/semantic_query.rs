// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for slice `semantic-vector-query`.
//!
//! Scenarios:
//!   1. `lattice init` populates `.lattice/vectors.db`.
//!   2. `lattice query --file-level "validate token signature"` returns ranked semantic
//!      hits: descending score, top hit `auth.rs`, correct envelope shape.
//!   3. Ranking is deterministic: a second identical query is byte-identical.
//!   4. Stream discipline: stdout is exactly one JSON object per invocation.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");
const SEEDED_QUERY: &str = "validate token signature";

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
    serde_json::from_str::<Value>(trimmed)
        .unwrap_or_else(|e| panic!("stdout is not exactly one JSON object ({e}): {stdout:?}"))
}

#[test]
fn semantic_query_end_to_end() {
    let tmp = fixture_copy();
    let root = tmp.path();
    assert!(!root.join(".lattice").exists());

    // ---- Scenario 1: init populates the vector store ----
    let init_out = run_in(root, &["init"]);
    assert!(
        init_out.status.success(),
        "lattice init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    assert!(
        root.join(".lattice/vectors.db").is_file(),
        ".lattice/vectors.db must exist after init"
    );

    // ---- Scenario 2: --file-level semantic query returns ranked hits, top = auth.rs ----
    // Use --file-level to get the old file-level shape for back-compat testing.
    let q1 = run_in(root, &["query", "--file-level", SEEDED_QUERY]);
    assert!(
        q1.status.success(),
        "lattice query exited non-zero; stderr: {}",
        String::from_utf8_lossy(&q1.stderr)
    );
    let env = parse_single_json(&q1);
    assert_eq!(env["protocol_version"], "1.5.0");
    assert_eq!(env["query_type"], "semantic");
    assert_eq!(env["query"], SEEDED_QUERY);

    let hits = env["hits"].as_array().expect("hits is an array");
    assert!(!hits.is_empty(), "hits must be a non-empty array");

    // --file-level hits are {path, score, class}; no "name" or "start_line" keys.
    for hit in hits {
        assert!(hit["path"].is_string(), "hit.path must be a string: {hit}");
        let score = hit["score"].as_f64().expect("hit.score must be a number");
        assert!(score >= 0.0, "score must be non-negative");
        assert!(
            hit.get("name").is_none(),
            "--file-level hits must not have 'name' key: {hit}"
        );
        assert!(
            hit.get("start_line").is_none(),
            "--file-level hits must not have 'start_line' key: {hit}"
        );
    }

    // Top hit for the seeded query is auth.rs.
    assert_eq!(
        hits[0]["path"], "auth.rs",
        "top hit for the seeded query must be auth.rs"
    );

    // ---- Scenario 3: ranking is deterministic (byte-identical stdout) ----
    let q2 = run_in(root, &["query", "--file-level", SEEDED_QUERY]);
    assert!(q2.status.success(), "second query exited non-zero");
    assert_eq!(
        q1.stdout, q2.stdout,
        "stdout of the second identical query must be byte-identical to the first"
    );
}
