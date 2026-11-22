// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for schema-version staleness detection in `freshness::ensure_fresh`.
//!
//! A binary upgrade may widen the ingestion/resolution schema without touching
//! any source file. `ensure_fresh` must treat a `facts_schema` version mismatch
//! as a staleness trigger — identical to a changed file — and fire `index::run`
//! so the full reparse runs. After the refresh the schema is current again and
//! subsequent queries report `freshness: "current"`.
//!
//! Scenarios:
//!   1. Baseline `lattice init` on rust_basic fixture → `facts_schema` is
//!      current; first query reports `freshness: "current"`.
//!   2. Downgrade stored schema to "1" via `lattice::meta::write_kv` (simulates
//!      a binary upgrade with no file changes); next query reports
//!      `freshness: "refreshed"` and exits 0 with a valid envelope.
//!   3. Immediate second query (schema is now current) reports
//!      `freshness: "current"` again — idempotent.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

/// Copy the rust_basic fixture into a fresh temp dir.
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
fn freshness_schema_version_mismatch_triggers_refresh() {
    let tmp = fixture_copy();
    let root = tmp.path();
    let lattice_dir = root.join(".lattice");

    // ---- Scenario 1: baseline init → schema current → query is `current` ----
    let init_out = run_in(root, &["init"]);
    assert!(
        init_out.status.success(),
        "baseline lattice init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );

    let s1 = run_in(root, &["query", "--impact", "auth.rs"]);
    assert!(
        s1.status.success(),
        "baseline query exited non-zero; stderr: {}",
        String::from_utf8_lossy(&s1.stderr)
    );
    let e1 = parse_single_json(&s1);
    assert_eq!(e1["protocol_version"], "1.5.0");
    assert_eq!(e1["query_type"], "impact");
    assert_eq!(
        e1["freshness"], "current",
        "store is fresh after init; no re-index expected"
    );

    // ---- Scenario 2: simulate a binary upgrade — downgrade stored schema ----
    // Overwrite the stored facts_schema to an old value. No source file changes.
    lattice::meta::write_kv(&lattice_dir, "facts_schema", "1")
        .expect("downgrade stored facts_schema");

    let s2 = run_in(root, &["query", "--impact", "auth.rs"]);
    assert!(
        s2.status.success(),
        "post-schema-downgrade query exited non-zero; stderr: {}",
        String::from_utf8_lossy(&s2.stderr)
    );
    let e2 = parse_single_json(&s2);
    assert_eq!(e2["protocol_version"], "1.5.0");
    assert_eq!(e2["query_type"], "impact");
    assert_eq!(
        e2["freshness"], "refreshed",
        "schema version mismatch must trigger a refresh even with no file changes"
    );

    // ---- Scenario 3: schema is now current → immediate repeat is `current` ----
    let s3 = run_in(root, &["query", "--impact", "auth.rs"]);
    assert!(
        s3.status.success(),
        "repeat query after schema refresh exited non-zero; stderr: {}",
        String::from_utf8_lossy(&s3.stderr)
    );
    let e3 = parse_single_json(&s3);
    assert_eq!(e3["protocol_version"], "1.5.0");
    assert_eq!(e3["query_type"], "impact");
    assert_eq!(
        e3["freshness"], "current",
        "after schema refresh, a second query must be current (idempotent)"
    );
}
