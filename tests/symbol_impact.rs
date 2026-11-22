// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for slice `symbol-impact`.
//!
//! Nine scenarios over `tests/fixtures/rust_basic/` plus the new
//! `tests/fixtures/rust_dup/` ambiguity fixture. Every stdout must be exactly
//! one JSON object (Scenario-4 discipline).
//!
//! Fixture ground truth:
//!   - 4 files, 5 functions, exactly 3 `calls` edges.
//!   - Chain: main → validate_token → check_signature (unique).
//!   - `report()` has no outgoing calls edges.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

/// Copy the rust_basic fixture into a fresh temp dir.
fn fixture_copy_rust_basic() -> tempfile::TempDir {
    fixture_copy_dir("rust_basic")
}

/// Copy the rust_dup fixture into a fresh temp dir.
fn fixture_copy_rust_dup() -> tempfile::TempDir {
    fixture_copy_dir("rust_dup")
}

fn fixture_copy_dir(name: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    for entry in fs::read_dir(&src).expect("read fixture dir") {
        let entry = entry.expect("fixture dir entry");
        let file_name = entry.file_name();
        fs::copy(entry.path(), tmp.path().join(&file_name)).expect("copy fixture file");
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

/// Stream discipline: stdout must be exactly one JSON object.
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

fn init_fixture(dir: &Path) {
    let out = run_in(dir, &["init"]);
    assert!(
        out.status.success(),
        "lattice init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Build a hit object `{path, name, line}` for comparison.
fn hit(path: &str, name: &str, line: u64) -> Value {
    serde_json::json!({ "path": path, "name": name, "line": line })
}

#[test]
fn s1_callers_validate_token() {
    let tmp = fixture_copy_rust_basic();
    let root = tmp.path();
    init_fixture(root);

    let out = run_in(root, &["query", "--callers", "validate_token"]);
    assert!(
        out.status.success(),
        "S1 must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);
    assert_eq!(env["protocol_version"], "1.5.0", "S1 protocol_version");
    assert_eq!(env["query_type"], "callers", "S1 query_type");
    assert_eq!(env["symbol"], "validate_token", "S1 symbol");
    assert_eq!(
        env["resolved"],
        serde_json::json!([hit("auth.rs", "validate_token", 1)]),
        "S1 resolved"
    );
    assert_eq!(
        env["hits"],
        serde_json::json!([hit("main.rs", "main", 5)]),
        "S1 hits"
    );
    assert!(env["freshness"].is_string(), "S1 freshness present");
}

#[test]
fn s2_callees_main() {
    let tmp = fixture_copy_rust_basic();
    let root = tmp.path();
    init_fixture(root);

    let out = run_in(root, &["query", "--callees", "main"]);
    assert!(
        out.status.success(),
        "S2 must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);
    assert_eq!(env["protocol_version"], "1.5.0", "S2 protocol_version");
    assert_eq!(env["query_type"], "callees", "S2 query_type");
    // sorted: auth.rs < db.rs
    assert_eq!(
        env["hits"],
        serde_json::json!([
            hit("auth.rs", "validate_token", 1),
            hit("db.rs", "save_session", 1)
        ]),
        "S2 hits"
    );
    assert!(env["freshness"].is_string(), "S2 freshness present");
}

#[test]
fn s3_call_chain_main_to_check_signature() {
    let tmp = fixture_copy_rust_basic();
    let root = tmp.path();
    init_fixture(root);

    let out = run_in(root, &["query", "--call-chain", "main", "check_signature"]);
    assert!(
        out.status.success(),
        "S3 must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);
    assert_eq!(env["protocol_version"], "1.5.0", "S3 protocol_version");
    assert_eq!(env["query_type"], "call_chain", "S3 query_type");
    assert_eq!(env["max_depth"], 8, "S3 max_depth");
    assert_eq!(
        env["paths"],
        serde_json::json!([[
            hit("main.rs", "main", 5),
            hit("auth.rs", "validate_token", 1),
            hit("auth.rs", "check_signature", 5)
        ]]),
        "S3 paths"
    );
    assert!(env["freshness"].is_string(), "S3 freshness present");
}

#[test]
fn s4_symbol_impact_check_signature() {
    let tmp = fixture_copy_rust_basic();
    let root = tmp.path();
    init_fixture(root);

    let out = run_in(root, &["query", "--impact", "--symbol", "check_signature"]);
    assert!(
        out.status.success(),
        "S4 must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);
    assert_eq!(env["protocol_version"], "1.5.0", "S4 protocol_version");
    assert_eq!(env["query_type"], "impact", "S4 query_type");
    assert_eq!(env["scope"], "symbol", "S4 scope");
    assert_eq!(env["symbol"], "check_signature", "S4 symbol");
    assert_eq!(
        env["resolved"],
        serde_json::json!([hit("auth.rs", "check_signature", 5)]),
        "S4 resolved"
    );
    assert_eq!(
        env["callers"],
        serde_json::json!([hit("auth.rs", "validate_token", 1)]),
        "S4 callers"
    );
    // transitive callers: validate_token and main (sorted path,line,name)
    assert_eq!(
        env["transitive_callers"],
        serde_json::json!([
            hit("auth.rs", "validate_token", 1),
            hit("main.rs", "main", 5)
        ]),
        "S4 transitive_callers"
    );
    assert_eq!(env["blast_radius"], 2, "S4 blast_radius = transitive_callers.len()");
    assert!(env["freshness"].is_string(), "S4 freshness present");
}

#[test]
fn s5_clean_miss() {
    let tmp = fixture_copy_rust_basic();
    let root = tmp.path();
    init_fixture(root);

    let out = run_in(root, &["query", "--callers", "no_such_fn"]);
    assert!(
        out.status.success(),
        "S5 clean miss must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);
    assert_eq!(env["protocol_version"], "1.5.0", "S5 protocol_version");
    assert_eq!(env["query_type"], "callers", "S5 query_type");
    assert_eq!(env["resolved"], serde_json::json!([]), "S5 resolved empty");
    assert_eq!(env["hits"], serde_json::json!([]), "S5 hits empty");
}

#[test]
fn s6_ambiguity_rust_dup() {
    let tmp = fixture_copy_rust_dup();
    let root = tmp.path();
    init_fixture(root);

    // First: bare name resolves to both parse functions
    let out = run_in(root, &["query", "--callers", "parse"]);
    assert!(
        out.status.success(),
        "S6a must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);
    assert_eq!(env["protocol_version"], "1.5.0", "S6a protocol_version");
    assert_eq!(env["query_type"], "callers", "S6a query_type");
    // Both parse functions in resolved, sorted by (path, line, name)
    assert_eq!(
        env["resolved"],
        serde_json::json!([
            hit("one.rs", "parse", 1),
            hit("two.rs", "parse", 1)
        ]),
        "S6a resolved has both candidates"
    );
    // Union of hits: main calls one::parse only
    assert_eq!(
        env["hits"],
        serde_json::json!([hit("main.rs", "main", 4)]),
        "S6a hits = callers of any parse"
    );

    // Second: qualified name two::parse resolves to exactly two.rs::parse
    let out2 = run_in(root, &["query", "--callers", "two::parse"]);
    assert!(
        out2.status.success(),
        "S6b must exit 0; stderr: {}",
        String::from_utf8_lossy(&out2.stderr)
    );
    let env2 = parse_single_json(&out2);
    assert_eq!(env2["protocol_version"], "1.5.0", "S6b protocol_version");
    assert_eq!(
        env2["resolved"],
        serde_json::json!([hit("two.rs", "parse", 1)]),
        "S6b resolved = two::parse only"
    );
    assert_eq!(
        env2["hits"],
        serde_json::json!([]),
        "S6b hits = empty (nobody calls two::parse)"
    );
}

#[test]
fn s7_call_chain_miss() {
    let tmp = fixture_copy_rust_basic();
    let root = tmp.path();
    init_fixture(root);

    // report() calls nothing, so no chain to main
    let out = run_in(root, &["query", "--call-chain", "report", "main"]);
    assert!(
        out.status.success(),
        "S7 must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);
    assert_eq!(env["protocol_version"], "1.5.0", "S7 protocol_version");
    assert_eq!(env["query_type"], "call_chain", "S7 query_type");
    // resolved_from and resolved_to must be non-empty (symbols exist)
    assert!(
        !env["resolved_from"].as_array().unwrap().is_empty(),
        "S7 resolved_from must be non-empty (report exists)"
    );
    assert!(
        !env["resolved_to"].as_array().unwrap().is_empty(),
        "S7 resolved_to must be non-empty (main exists)"
    );
    assert_eq!(
        env["paths"],
        serde_json::json!([]),
        "S7 paths empty (no chain from report to main)"
    );
}

#[test]
fn s8_malformed_call_chain() {
    let tmp = fixture_copy_rust_basic();
    let root = tmp.path();
    init_fixture(root);

    // --call-chain with only one arg → ambiguous_query
    let out = run_in(root, &["query", "--call-chain", "main"]);
    assert!(
        !out.status.success(),
        "S8 must exit non-zero (got success)"
    );
    let env = parse_single_json(&out);
    assert_eq!(env["protocol_version"], "1.5.0", "S8 protocol_version");
    assert_eq!(env["query_type"], "error", "S8 query_type");
    assert_eq!(env["error"]["code"], "ambiguous_query", "S8 error.code");
}

#[test]
fn s8b_malformed_callers_no_symbol() {
    let tmp = fixture_copy_rust_basic();
    let root = tmp.path();
    init_fixture(root);

    // --callers with no symbol → ambiguous_query (guard for free-text arm)
    let out = run_in(root, &["query", "--callers"]);
    assert!(
        !out.status.success(),
        "S8b must exit non-zero (got success)"
    );
    let env = parse_single_json(&out);
    assert_eq!(env["protocol_version"], "1.5.0", "S8b protocol_version");
    assert_eq!(env["query_type"], "error", "S8b query_type");
    assert_eq!(env["error"]["code"], "ambiguous_query", "S8b error.code");
}

#[test]
fn s9_file_impact_regression() {
    let tmp = fixture_copy_rust_basic();
    let root = tmp.path();
    init_fixture(root);

    // File-scope impact must be byte-for-byte unchanged
    let out = run_in(root, &["query", "--impact", "auth.rs"]);
    assert!(
        out.status.success(),
        "S9 must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);
    assert_eq!(env["protocol_version"], "1.5.0", "S9 protocol_version");
    assert_eq!(env["query_type"], "impact", "S9 query_type");
    assert_eq!(env["target"], "auth.rs", "S9 target");
    assert_eq!(env["callers"], serde_json::json!(["main.rs"]), "S9 callers");
    assert_eq!(
        env["dependents"],
        serde_json::json!(["main.rs", "metrics.rs"]),
        "S9 dependents"
    );
    assert_eq!(env["blast_radius"], 2, "S9 blast_radius");
    // No "scope" field on file-scope impact
    assert!(env.get("scope").is_none(), "S9 no scope field on file-scope impact");
}
