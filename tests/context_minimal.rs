// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Integration tests for minimal context orientation (PLAN Feature 3).
//!
//! Test gate: `cargo test --test context_overview`

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

fn fixture_copy(fixture: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join(fixture);
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
fn minimal_with_task_review() {
    let tmp = fixture_copy("tests/fixtures/rust_flows");
    let root = tmp.path();

    let out = run_in(root, &["context", "--task", "review PR #42"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);

    assert_eq!(env["protocol_version"], "1.5.0");
    assert_eq!(env["query_type"], "context_overview");
    for key in ["modules", "hotspots", "owners"] {
        assert!(env[key].is_array(), "missing overview section: {key}");
    }

    // Repo stats.
    assert!(env["repo"]["files"].as_u64().unwrap() >= 1);
    assert!(env["repo"]["symbols"].as_u64().unwrap() >= 1);
    assert!(env["repo"]["languages"]["rust"].as_u64().unwrap() >= 1);

    // Communities.
    let comms = env["communities"].as_array().unwrap();
    assert!(comms.len() >= 1);
    assert!(comms.len() <= 3, "max 3 communities");
    for c in comms {
        assert!(c.get("name").is_some(), "community needs name");
        assert!(c.get("size").is_some(), "community needs size");
    }

    // Flows.
    let flows = env["flows"].as_array().unwrap();
    assert!(flows.len() >= 1);
    assert!(flows.len() <= 3, "max 3 flows");
    for f in flows {
        assert!(f.get("entry").is_some(), "flow needs entry");
        assert!(f.get("criticality").is_some(), "flow needs criticality");
    }

    // Risk.
    let risk = env["risk"].as_str().unwrap();
    assert!(
        risk == "low" || risk == "medium" || risk == "high",
        "risk must be low/medium/high, got: {risk}"
    );

    // Review task should suggest impact + hotspots.
    let suggestions = env["next_tool_suggestions"].as_array().unwrap();
    assert!(suggestions.iter().any(|s| s.as_str() == Some("lattice query --impact <path>")));
    assert!(suggestions.iter().any(|s| s.as_str() == Some("lattice query --hotspots")));
}

#[test]
fn minimal_without_task() {
    let tmp = fixture_copy("tests/fixtures/rust_flows");
    let root = tmp.path();

    let out = run_in(root, &["context"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);

    assert_eq!(env["query_type"], "context_overview");

    // Default suggestions (no task).
    let suggestions = env["next_tool_suggestions"].as_array().unwrap();
    assert!(suggestions.len() >= 1, "should have default suggestions");
}

#[test]
fn retired_context_flags_are_rejected() {
    let tmp = fixture_copy("tests/fixtures/rust_flows");
    let root = tmp.path();

    for flag in ["--stable", "--minimal", "--task"] {
        let out = run_in(root, &["context", flag]);
        assert_eq!(out.status.code(), Some(2));
        assert_eq!(parse_json(&out)["error"]["code"], "invalid_flag");
    }
}
