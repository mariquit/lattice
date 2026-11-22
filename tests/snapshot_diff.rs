// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Integration tests for graph snapshot diffing (PLAN Feature 6).
//!
//! Test gate: `cargo test --test snapshot_diff`

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

fn fixture_copy() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_flows");
    for entry in fs::read_dir(&src).expect("read fixture dir") {
        let entry = entry.expect("fixture dir entry");
        let name = entry.file_name();
        fs::copy(entry.path(), tmp.path().join(&name)).expect("copy fixture file");
    }
    let out = Command::new(BIN)
        .args(["init"])
        .current_dir(tmp.path())
        .output()
        .expect("init");
    assert!(out.status.success());
    tmp
}

fn run_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn lattice")
}

fn parse_json(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.lines().last().unwrap_or("");
    serde_json::from_str(trimmed).expect("parse JSON")
}

#[test]
fn snapshot_stores_a_manifest() {
    let tmp = fixture_copy();
    let root = tmp.path();

    let out = run_in(root, &["snapshot", "baseline"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);
    assert_eq!(env["command"], "snapshot");
    assert_eq!(env["label"], "baseline");
    assert!(env["node_count"].as_u64().unwrap() >= 1);

    let stored = lattice::snapshot::load(&root.join(".lattice"), "baseline").unwrap();
    assert_eq!(stored.nodes.len() as u64, env["node_count"].as_u64().unwrap());
    assert!(!root.join(".lattice/snapshots").exists());
}

#[test]
fn diff_with_no_changes() {
    let tmp = fixture_copy();
    let root = tmp.path();

    assert!(run_in(root, &["snapshot", "--label", "baseline"]).status.success());

    let out = run_in(root, &["query", "--graph-diff", "baseline", "working"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);

    assert_eq!(env["query_type"], "graph_diff");
    assert_eq!(env["from"], "baseline");
    assert_eq!(env["nodes"]["added"].as_array().unwrap().len(), 0);
    assert_eq!(env["nodes"]["removed"].as_array().unwrap().len(), 0);
}

#[test]
fn diff_detects_added_node() {
    let tmp = fixture_copy();
    let root = tmp.path();

    assert!(run_in(root, &["snapshot", "before"]).status.success());

    // Add a new file.
    fs::write(root.join("extra.rs"), "pub fn extra_fn() { }\n").unwrap();
    // Re-init to pick up the new file.
    let init_out = run_in(root, &["init"]);
    assert!(init_out.status.success());

    let out = run_in(root, &["query", "--graph-diff", "before", "working"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);

    let added = env["nodes"]["added"].as_array().unwrap();
    assert_eq!(added.len(), 1, "should detect 1 added node");
    assert_eq!(added[0]["id"], "extra.rs::extra_fn");
}

#[test]
fn diff_not_found() {
    let tmp = fixture_copy();
    let root = tmp.path();

    let out = run_in(root, &["query", "--graph-diff", "nonexistent", "working"]);
    assert!(!out.status.success());
    assert_eq!(parse_json(&out)["error"]["code"], "not_found");
}

#[test]
fn determinism_snapshot_twice() {
    let tmp = fixture_copy();
    let root = tmp.path();

    assert!(run_in(root, &["snapshot", "snap1"]).status.success());
    let out1 = run_in(root, &["query", "--graph-diff", "snap1", "working"]);
    let out2 = run_in(root, &["query", "--graph-diff", "snap1", "working"]);
    assert!(out1.status.success() && out2.status.success());

    let j1 = String::from_utf8_lossy(&out1.stdout);
    let j2 = String::from_utf8_lossy(&out2.stdout);
    assert_eq!(j1, j2, "two diff invocations should be byte-identical");
}

#[test]
fn retired_snapshot_commands_leave_legacy_files_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let dir = root.join(".lattice/snapshots");
    fs::create_dir_all(&dir).unwrap();
    let existing = dir.join("baseline.json");
    fs::write(&existing, "legacy snapshot").unwrap();
    let save = run_in(root, &["snapshot", "--name", "baseline"]);
    let diff = run_in(root, &["diff", "baseline"]);
    assert_eq!(parse_json(&save)["error"]["code"], "invalid_flag");
    assert_eq!(parse_json(&diff)["error"]["code"], "unknown_command");
    assert_eq!(fs::read_to_string(existing).unwrap(), "legacy snapshot");
}
