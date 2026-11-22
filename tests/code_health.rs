// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for the code-health-analytics slice.
//!
//! Tests four new query intents: --dead-code, --hotspots, --coupling, --health.
//! The health_basic fixture has hand-counted expectations for these queries.
//!
//! Run: `cargo test --test code_health`

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

/// Copy the health_basic fixture into a fresh temp dir (no .git, no .lattice/).
/// The absence of .git is deliberate: exercises churn-degradation ladder rung 1
/// (churn_weighted: false, all churn values 0).
fn fixture_copy() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/health_basic");
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

/// Initialize the fixture and return the tempdir (must be kept alive).
fn init_fixture() -> tempfile::TempDir {
    let tmp = fixture_copy();
    let init_out = run_in(tmp.path(), &["init"]);
    assert!(
        init_out.status.success(),
        "lattice init failed; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    tmp
}

// ── 1. dead_code ─────────────────────────────────────────────────────────────

#[test]
fn dead_code_exact_hit() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--dead-code"]);
    assert!(
        out.status.success(),
        "exit non-zero; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);

    assert_eq!(env["protocol_version"], "1.5.0");
    assert_eq!(env["query_type"], "dead_code");
    assert_eq!(env["count"], 1);

    let hits = env["hits"].as_array().expect("hits is array");
    assert_eq!(hits.len(), 1, "expected exactly one dead-code hit");

    let hit = &hits[0];
    assert_eq!(hit["path"], "orphan.rs");
    assert_eq!(hit["name"], "unused_helper");
    assert_eq!(hit["line"], 1);
    assert_eq!(hit["id"], "orphan.rs::unused_helper");
}

// ── 1b. dead_code: R6 mock/stub exclusion ─────────────────────────────────────

#[test]
fn dead_code_r6_mock_excluded() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--dead-code"]);
    assert!(
        out.status.success(),
        "exit non-zero; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);

    let hits = env["hits"].as_array().expect("hits is array");
    let names: Vec<&str> = hits.iter().map(|h| h["name"].as_str().unwrap()).collect();

    // R6: mock_service and dbStub should be excluded.
    assert!(
        !names.contains(&"mock_service"),
        "mock_service should be excluded by R6, got: {names:?}"
    );
    assert!(
        !names.contains(&"dbStub"),
        "dbStub should be excluded by R6, got: {names:?}"
    );

    // unused_helper should still be there (not a mock).
    assert!(
        names.contains(&"unused_helper"),
        "unused_helper should still be a dead-code hit"
    );
}

// ── 1c. dead_code: R7 framework class exclusion ───────────────────────────────

#[test]
fn dead_code_r7_framework_class_excluded() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--dead-code"]);
    assert!(
        out.status.success(),
        "exit non-zero; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);

    let hits = env["hits"].as_array().expect("hits is array");
    let names: Vec<&str> = hits.iter().map(|h| h["name"].as_str().unwrap()).collect();

    // R7: framework class methods should be excluded (setup, validate).
    // Note: R2 already excludes methods with parents, so R7 is a
    // redundant but future-proofing check.
    assert!(
        !names.contains(&"setup"),
        "setup should be excluded (R2 or R7), got: {names:?}"
    );
    assert!(
        !names.contains(&"validate"),
        "validate should be excluded (R2 or R7), got: {names:?}"
    );
}

// ── 2. hotspots ──────────────────────────────────────────────────────────────

#[test]
fn hotspots_order_and_fields() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--hotspots"]);
    assert!(
        out.status.success(),
        "exit non-zero; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);

    assert_eq!(env["protocol_version"], "1.5.0");
    assert_eq!(env["query_type"], "hotspots");
    assert_eq!(env["unit"], "file");
    assert_eq!(env["churn_weighted"], false);

    let hotspots = env["hotspots"].as_array().expect("hotspots is array");
    assert_eq!(hotspots.len(), 7, "expected 7 hotspot entries (one per file)");

    // Rank order: score desc, path asc for ties.
    let paths: Vec<&str> = hotspots
        .iter()
        .map(|h| h["path"].as_str().expect("path is string"))
        .collect();
    assert_eq!(
        paths,
        vec!["hub.rs", "a.rs", "main.rs", "b.rs", "framework_class.rs", "mock_fns.rs", "orphan.rs"],
        "hotspot rank order"
    );

    // hub.rs entry exact field check.
    let hub = &hotspots[0];
    assert_eq!(hub["path"], "hub.rs");
    assert_eq!(hub["imports_in"], 3);
    assert_eq!(hub["imports_out"], 0);
    assert_eq!(hub["calls_in"], 3);
    assert_eq!(hub["calls_out"], 0);
    assert_eq!(hub["degree"], 6);
    assert_eq!(hub["churn"], 0);
    assert_eq!(hub["score"], 6.0_f64);
}

// ── 3. hotspots --limit ───────────────────────────────────────────────────────

#[test]
fn hotspots_limit() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--hotspots", "--limit", "2"]);
    assert!(
        out.status.success(),
        "exit non-zero; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);

    let hotspots = env["hotspots"].as_array().expect("hotspots is array");
    assert_eq!(hotspots.len(), 2, "expected exactly 2 entries after --limit 2");

    let paths: Vec<&str> = hotspots
        .iter()
        .map(|h| h["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["hub.rs", "a.rs"]);
}

// ── 4. coupling (hub.rs) ──────────────────────────────────────────────────────

#[test]
fn coupling_hub() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--coupling", "hub.rs"]);
    assert!(
        out.status.success(),
        "exit non-zero; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);

    assert_eq!(env["protocol_version"], "1.5.0");
    assert_eq!(env["query_type"], "coupling");
    assert_eq!(env["path"], "hub.rs");
    assert_eq!(env["afferent"], 3);
    assert_eq!(env["efferent"], 0);
    assert_eq!(env["instability"], 0.0_f64);

    let aff: Vec<&str> = env["afferent_files"]
        .as_array()
        .expect("afferent_files is array")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(aff, vec!["a.rs", "b.rs", "main.rs"]);

    let eff: Vec<&str> = env["efferent_files"]
        .as_array()
        .expect("efferent_files is array")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(eff, Vec::<&str>::new());
}

// ── 5a. coupling (a.rs — mid) ────────────────────────────────────────────────

#[test]
fn coupling_a_mid() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--coupling", "a.rs"]);
    assert!(
        out.status.success(),
        "exit non-zero; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);

    assert_eq!(env["afferent"], 1);
    assert_eq!(env["efferent"], 1);
    assert_eq!(env["instability"], 0.5_f64);

    let aff: Vec<&str> = env["afferent_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(aff, vec!["main.rs"]);

    let eff: Vec<&str> = env["efferent_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(eff, vec!["hub.rs"]);
}

// ── 5b. coupling (unknown path — graceful empty result) ───────────────────────

#[test]
fn coupling_unknown_path() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--coupling", "no_such.rs"]);
    assert!(
        out.status.success(),
        "exit non-zero for unknown path; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);

    assert_eq!(env["afferent"], 0);
    assert_eq!(env["efferent"], 0);
    assert_eq!(env["instability"], 0.0_f64);
    assert_eq!(env["afferent_files"].as_array().unwrap().len(), 0);
    assert_eq!(env["efferent_files"].as_array().unwrap().len(), 0);
}

// ── 6. health ─────────────────────────────────────────────────────────────────

#[test]
fn health_summary() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--health"]);
    assert!(
        out.status.success(),
        "exit non-zero; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);

    assert_eq!(env["protocol_version"], "1.5.0");
    assert_eq!(env["query_type"], "health");

    assert_eq!(env["files"], 7);
    assert_eq!(env["functions"], 10);
    assert_eq!(env["call_edges"], 5);
    assert_eq!(env["import_edges"], 5);
    assert!((env["avg_coupling"].as_f64().unwrap() - 10.0_f64 / 7.0_f64).abs() < 1e-10);

    assert_eq!(env["dead_code"]["count"], 1);
    assert_eq!(
        env["dead_code"]["hits"][0]["name"],
        "unused_helper"
    );

    assert_eq!(env["god_classes"]["threshold"], 12);
    let god_entries = env["god_classes"]["entries"].as_array().unwrap();
    assert_eq!(god_entries.len(), 0, "no god classes in health_basic");

    let hotspots = env["hotspots"].as_array().unwrap();
    assert_eq!(hotspots.len(), 5, "health embeds top 5 hotspots (all 5 files)");
    assert_eq!(hotspots[0]["path"], "hub.rs");

    assert_eq!(env["churn_weighted"], false);
}

// ── 7. flag guards ────────────────────────────────────────────────────────────

#[test]
fn dead_code_rejects_kind_flag() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--dead-code", "--kind", "function"]);
    assert!(
        !out.status.success(),
        "expected non-zero exit for --dead-code --kind"
    );
    let env = parse_single_json(&out);
    assert_eq!(env["error"]["code"], "invalid_flag");
}

#[test]
fn hotspots_rejects_file_level_flag() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--hotspots", "--file-level"]);
    assert!(
        !out.status.success(),
        "expected non-zero exit for --hotspots --file-level"
    );
    let env = parse_single_json(&out);
    assert_eq!(env["error"]["code"], "invalid_flag");
}

#[test]
fn coupling_no_path_is_ambiguous() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--coupling"]);
    assert!(
        !out.status.success(),
        "expected non-zero exit for bare --coupling"
    );
    let env = parse_single_json(&out);
    assert_eq!(env["error"]["code"], "ambiguous_query");
}
