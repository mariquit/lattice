// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for graph-diff over snapshots.
//!
//! `cargo test --test graph_diff`
//!
//! Snapshot a fixture, add one function and one `calls` edge, re-ingest,
//! snapshot again. `--graph-diff a b` reports exactly one added node and one
//! added edge.

use std::fs;
use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

fn run_in(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn lattice")
}

fn parse_json(output: &std::process::Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("parse JSON: {e}\nstdout: {stdout}\nstderr: {}", String::from_utf8_lossy(&output.stderr))
    })
}

#[test]
fn graph_diff_one_added_node_and_edge() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("lib.rs"),
        "fn helper() {}\nfn caller() { helper(); }\n",
    )
    .unwrap();

    let init = run_in(root, &["init"]);
    assert!(init.status.success(), "init: {}", String::from_utf8_lossy(&init.stderr));

    let snap_a = run_in(root, &["snapshot", "--label", "a"]);
    assert!(snap_a.status.success(), "snapshot a: {}", String::from_utf8_lossy(&snap_a.stderr));

    fs::write(
        root.join("lib.rs"),
        "fn helper() {}\nfn caller() { helper(); }\nfn extra() { helper(); }\n",
    )
    .unwrap();
    let init2 = run_in(root, &["init"]);
    assert!(init2.status.success(), "re-init: {}", String::from_utf8_lossy(&init2.stderr));

    let snap_b = run_in(root, &["snapshot", "--label", "b"]);
    assert!(snap_b.status.success(), "snapshot b: {}", String::from_utf8_lossy(&snap_b.stderr));

    let diff1 = run_in(root, &["query", "--graph-diff", "a", "b"]);
    let diff2 = run_in(root, &["query", "--graph-diff", "a", "b"]);
    assert!(diff1.status.success(), "diff: {}", String::from_utf8_lossy(&diff1.stderr));
    assert_eq!(
        diff1.stdout, diff2.stdout,
        "two runs of the same diff must be byte-identical"
    );

    let env = parse_json(&diff1);
    assert_eq!(env["protocol_version"], "1.5.0");
    assert_eq!(env["query_type"], "graph_diff");
    assert_eq!(env["from"], "a");
    assert_eq!(env["to"], "b");

    let added = env["nodes"]["added"].as_array().unwrap();
    assert_eq!(added.len(), 1, "exactly one added node: {added:?}");
    assert!(
        added[0]["id"].as_str().unwrap().ends_with("::extra"),
        "added node should be extra: {added:?}"
    );
    assert_eq!(env["nodes"]["removed"].as_array().unwrap().len(), 0);
    assert_eq!(env["nodes"]["changed"].as_array().unwrap().len(), 0);
    assert_eq!(env["edges"]["added"].as_array().unwrap().len(), 1);
    assert_eq!(env["edges"]["removed"].as_array().unwrap().len(), 0);
}

#[test]
fn snapshot_rejects_working_label() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("lib.rs"), "fn helper() {}\n").unwrap();
    assert!(run_in(root, &["init"]).status.success());
    let out = run_in(root, &["snapshot", "--label", "working"]);
    assert!(!out.status.success(), "working must be rejected");
    let env = parse_json(&out);
    assert_eq!(env["query_type"], "error");
}
