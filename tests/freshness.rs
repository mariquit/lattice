// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for slice `freshness` (slice 12).
//!
//! The query path self-refreshes changed files before answering (no background
//! process). Every query SUCCESS envelope carries an additive `freshness`
//! field ("refreshed" | "current"); error envelopes carry none.
//!
//! No sleeps: the inclusive mtime rule (`mtime >= indexed_at`, whole-second)
//! plus hash-on-suspicion makes every scenario deterministic even when init,
//! mutation, and query land in the same second.
//!
//! Scenarios:
//!   1. Baseline `init` then `query --impact auth.rs` untouched → `current`
//!      with the baseline ground truth (callers ["main.rs"], dependents
//!      ["main.rs","metrics.rs"], blast_radius 2).
//!   2. CORE: overwrite db.rs to import+call auth (no re-init); the impact
//!      query heals it → `refreshed`, callers ["db.rs","main.rs"],
//!      dependents ["db.rs","main.rs","metrics.rs"], blast_radius 3.
//!   3. CORE: immediate second identical call → `current`, same answer.
//!   4. The check runs for every query variant: touch db.rs again, free-text
//!      query → semantic envelope + `refreshed`; repeat → `current`.
//!   5. Errors carry no freshness field (no_lattice).

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

/// Copy the rust_basic fixture into a fresh temp dir (no .gitignore, no
/// .lattice/). Mutations touch only this temp copy; the repo is never
/// mutated.
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

/// stdout is exactly one JSON object and nothing else. serde_json::from_str
/// fails on trailing non-whitespace, so a successful parse of the trimmed
/// stdout also proves a single value.
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

/// The Scenario 2 mutation: db.rs now imports and calls auth.
const MUTATED_DB_RS: &str = "\
use crate::auth;

pub fn save_session(id: u32) -> u32 {
    if auth::validate_token(\"abcdefghi\") { id } else { 0 }
}
";

/// The Scenario 4 mutation: a third distinct content (comment appended to the
/// already-mutated db.rs), still importing+calling auth.
const MUTATED_DB_RS_V3: &str = "\
use crate::auth;

pub fn save_session(id: u32) -> u32 {
    if auth::validate_token(\"abcdefghi\") { id } else { 0 }
}
// touched a third time
";

#[test]
fn freshness_self_heals_query_path() {
    let tmp = fixture_copy();
    let root = tmp.path();

    // ---- Scenario 1: baseline index and a `current` answer (B12.2) ----
    let init_out = run_in(root, &["init"]);
    assert!(
        init_out.status.success(),
        "baseline lattice init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );

    let s1 = run_in(root, &["query", "--impact", "auth.rs"]);
    assert!(
        s1.status.success(),
        "baseline query --impact auth.rs exited non-zero; stderr: {}",
        String::from_utf8_lossy(&s1.stderr)
    );
    let e1 = parse_single_json(&s1);
    assert_eq!(e1["protocol_version"], "1.5.0");
    assert_eq!(e1["query_type"], "impact");
    assert_eq!(e1["target"], "auth.rs");
    assert_eq!(e1["callers"], serde_json::json!(["main.rs"]));
    assert_eq!(
        e1["dependents"],
        serde_json::json!(["main.rs", "metrics.rs"])
    );
    assert_eq!(e1["blast_radius"], 2);
    assert_eq!(e1["freshness"], "current", "nothing was re-indexed");

    // ---- Scenario 2: mutation is healed by the query itself (B12.1) CORE ----
    fs::write(root.join("db.rs"), MUTATED_DB_RS).expect("overwrite db.rs");

    let s2 = run_in(root, &["query", "--impact", "auth.rs"]);
    assert!(
        s2.status.success(),
        "post-mutation query --impact auth.rs exited non-zero; stderr: {}",
        String::from_utf8_lossy(&s2.stderr)
    );
    let e2 = parse_single_json(&s2);
    assert_eq!(e2["protocol_version"], "1.5.0");
    assert_eq!(e2["query_type"], "impact");
    assert_eq!(
        e2["callers"],
        serde_json::json!(["db.rs", "main.rs"]),
        "the new db.rs -> auth call edge is visible"
    );
    assert_eq!(
        e2["dependents"],
        serde_json::json!(["db.rs", "main.rs", "metrics.rs"])
    );
    assert_eq!(e2["blast_radius"], 3);
    assert_eq!(
        e2["freshness"], "refreshed",
        "the answer was produced only after a synchronous partial re-index"
    );

    // ---- Scenario 3: immediate second call is `current` (B12.2) CORE ----
    let s3 = run_in(root, &["query", "--impact", "auth.rs"]);
    assert!(
        s3.status.success(),
        "second query --impact auth.rs exited non-zero; stderr: {}",
        String::from_utf8_lossy(&s3.stderr)
    );
    let e3 = parse_single_json(&s3);
    assert_eq!(e3["callers"], serde_json::json!(["db.rs", "main.rs"]));
    assert_eq!(
        e3["dependents"],
        serde_json::json!(["db.rs", "main.rs", "metrics.rs"])
    );
    assert_eq!(e3["blast_radius"], 3);
    assert_eq!(
        e3["freshness"], "current",
        "no second re-index happened; the mutated file hash-matches now"
    );

    // ---- Scenario 4: the check runs for every query variant (B12.1 scope) ----
    fs::write(root.join("db.rs"), MUTATED_DB_RS_V3).expect("overwrite db.rs a third time");

    let s4 = run_in(root, &["query", "save session"]);
    assert!(
        s4.status.success(),
        "free-text query exited non-zero; stderr: {}",
        String::from_utf8_lossy(&s4.stderr)
    );
    let e4 = parse_single_json(&s4);
    assert_eq!(e4["protocol_version"], "1.5.0");
    assert_eq!(e4["query_type"], "hybrid");
    assert_eq!(
        e4["freshness"], "refreshed",
        "the staleness check sits ahead of ALL dispatch, not only --impact"
    );

    let s4b = run_in(root, &["query", "save session"]);
    assert!(s4b.status.success());
    let e4b = parse_single_json(&s4b);
    assert_eq!(e4b["query_type"], "hybrid");
    assert_eq!(
        e4b["freshness"], "current",
        "a repeat of the same call yields current"
    );
}

#[test]
fn freshness_absent_on_error_envelope() {
    // ---- Scenario 5: errors carry no freshness field ----
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path();

    let out = run_in(root, &["query", "--impact", "auth.rs"]);
    assert!(
        !out.status.success(),
        "query with no .lattice/ must exit non-zero"
    );
    let env = parse_single_json(&out);
    assert_eq!(env["protocol_version"], "1.5.0");
    assert_eq!(env["query_type"], "error");
    assert_eq!(env["error"]["code"], "no_lattice");
    assert!(
        env.get("freshness").is_none(),
        "error envelopes must not carry a freshness field: {env}"
    );
}
