// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for betweenness bridges and execution-flow chains.
//!
//! `cargo test --test centrality_flows`
//!
//! Fixture: `main -> mid -> leaf`. Brandes (directed, unnormalized) gives `mid`
//! betweenness 1: the only shortest path from `main` to `leaf` passes through it.
//! The flow chain is that path, in order. Criticality is the shipped score
//! (fan-in × 2, no security-keyword bonus, no git churn) = 4.

use std::fs;
use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

fn run_in(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN).args(args).current_dir(dir).output().expect("spawn")
}

fn parse_json(output: &std::process::Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("parse: {e}\nstdout: {stdout}\nstderr: {}", String::from_utf8_lossy(&output.stderr))
    })
}

#[test]
fn bridge_first_and_flow_chain() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("lib.rs"),
        "\
pub fn main() { mid(); }
pub fn mid() { leaf(); }
pub fn leaf() {}
",
    )
    .unwrap();
    let init = run_in(root, &["init"]);
    assert!(init.status.success(), "init: {}", String::from_utf8_lossy(&init.stderr));

    let bridges = run_in(root, &["query", "--bridges"]);
    assert!(bridges.status.success(), "{}", String::from_utf8_lossy(&bridges.stderr));
    let benv = parse_json(&bridges);
    assert_eq!(benv["query_type"], "bridges");
    let hits = benv["hits"].as_array().unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0]["name"], "mid");
    assert_eq!(hits[0]["betweenness"].as_f64(), Some(1.0));

    let flows = run_in(root, &["query", "--flows"]);
    assert!(flows.status.success(), "{}", String::from_utf8_lossy(&flows.stderr));
    let fenv = parse_json(&flows);
    let flow = &fenv["flows"].as_array().unwrap()[0];
    let chain: Vec<&str> = flow["chain"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(chain, vec!["main", "mid", "leaf"]);
    assert_eq!(flow["criticality"], 4);
}
