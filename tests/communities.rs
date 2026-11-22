// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Integration tests for community detection (PLAN Feature 2).
//!
//! Test gate: `cargo test --test communities`
//! Fixture: `tests/fixtures/community_basic/` — 2 files, 6 functions, 2 call clusters.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

fn fixture_copy() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/community_basic");
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
    assert!(out.status.success(), "init failed: {}", String::from_utf8_lossy(&out.stderr));
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
fn communities_two_clusters() {
    let tmp = fixture_copy();
    let root = tmp.path();

    let out = run_in(root, &["query", "--communities"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);

    assert_eq!(env["protocol_version"], "1.5.0");
    assert_eq!(env["query_type"], "communities");
    assert_eq!(env["count"], 2);
    assert_eq!(env["resolution"], 1.0);
    let q = env["modularity"].as_f64().unwrap();
    assert!(q > 0.0 && q <= 1.0, "modularity {q} outside (0, 1]");

    let comms = env["communities"].as_array().unwrap();
    assert_eq!(comms.len(), 2);

    // Each community has 3 members.
    for c in comms {
        assert_eq!(c["size"].as_u64().unwrap(), 3, "community should have 3 members");
    }

    // Find the auth community (contains login).
    let auth = comms.iter().find(|c| {
        c["members"].as_array().unwrap().iter()
            .any(|m| m.as_str().unwrap().ends_with("::login"))
    }).unwrap();
    assert_eq!(auth["name"], "auth");
    let mut auth_members: Vec<&str> = auth["members"].as_array().unwrap().iter()
        .map(|m| m.as_str().unwrap().split("::").last().unwrap()).collect();
    auth_members.sort();
    assert_eq!(auth_members, vec!["check_permission", "login", "validate_token"]);

    // Find the payments community (contains charge).
    let payments = comms.iter().find(|c| {
        c["members"].as_array().unwrap().iter()
            .any(|m| m.as_str().unwrap().ends_with("::charge"))
    }).unwrap();
    assert_eq!(payments["name"], "payments");
    let mut pay_members: Vec<&str> = payments["members"].as_array().unwrap().iter()
        .map(|m| m.as_str().unwrap().split("::").last().unwrap()).collect();
    pay_members.sort();
    assert_eq!(pay_members, vec!["charge", "refund", "verify_payment"]);
}

#[test]
fn community_lookup_by_name() {
    let tmp = fixture_copy();
    let root = tmp.path();

    let out = run_in(root, &["query", "--community", "auth"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);

    assert_eq!(env["query_type"], "community");
    assert_eq!(env["name"], "auth");
    assert_eq!(env["size"], 3);
}

#[test]
fn community_not_found() {
    let tmp = fixture_copy();
    let root = tmp.path();

    let out = run_in(root, &["query", "--community", "nonexistent"]);
    assert!(!out.status.success());
    let env = parse_json(&out);
    assert_eq!(env["query_type"], "error");
    assert_eq!(env["error"]["code"], "not_found");
}

#[test]
fn determinism_communities_twice() {
    let tmp = fixture_copy();
    let root = tmp.path();

    let out1 = run_in(root, &["query", "--communities"]);
    let out2 = run_in(root, &["query", "--communities"]);

    let j1 = String::from_utf8_lossy(&out1.stdout);
    let j2 = String::from_utf8_lossy(&out2.stdout);
    assert_eq!(j1, j2, "two --communities invocations should be byte-identical");
}

#[test]
fn community_guard_rejects_kind() {
    let tmp = fixture_copy();
    let root = tmp.path();

    let out = run_in(root, &["query", "--communities", "--kind", "function"]);
    assert!(!out.status.success());
}
