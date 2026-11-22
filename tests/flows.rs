// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Integration tests for execution flow detection (PLAN Feature 1).
//!
//! Test gate: `cargo test --test flows`
//! Fixture: `tests/fixtures/rust_flows/` — 7 .rs files, hand-countable call graph.

use std::process::Command;

fn lattice_bin() -> String {
    let mut path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    path.push("lattice");
    path.to_string_lossy().to_string()
}

fn run_in(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(lattice_bin())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to run lattice")
}

fn init_fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let fixture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rust_flows");
    for entry in std::fs::read_dir(&fixture_dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            std::fs::copy(entry.path(), tmp.path().join(entry.file_name())).unwrap();
        }
    }
    let init_out = run_in(tmp.path(), &["init"]);
    assert!(
        init_out.status.success(),
        "lattice init failed; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    tmp
}

fn parse_single_json(out: &std::process::Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.lines().last().unwrap_or("");
    serde_json::from_str(trimmed).unwrap_or_else(|e| {
        panic!("failed to parse JSON: {e}\nstdout: {stdout}");
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[test]
fn flows_entry_points_and_members() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--flows"]);
    assert!(
        out.status.success(),
        "exit non-zero; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);

    assert_eq!(env["protocol_version"], "1.5.0");
    assert_eq!(env["query_type"], "flows");

    let flows = env["flows"].as_array().expect("flows is array");
    assert_eq!(flows.len(), 4, "expected 4 flows");

    // Extract entry names for sorted comparison.
    let entries: Vec<&str> = flows
        .iter()
        .map(|f| {
            f["entry"]
                .as_str()
                .unwrap()
                .split("::")
                .last()
                .unwrap()
        })
        .collect();
    let mut sorted_entries = entries.clone();
    sorted_entries.sort();
    assert_eq!(
        sorted_entries,
        vec!["cmd_start", "handler", "main", "run"],
        "entry point names"
    );

    // main's flow: {compute, helper, main, process} (sorted by file::name)
    let main_flow = flows.iter().find(|f| {
        f["entry"]
            .as_str()
            .unwrap()
            .ends_with("::main")
    }).unwrap();
    let main_members: Vec<&str> = main_flow["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let mut sorted_main = main_members.clone();
    sorted_main.sort();
    assert_eq!(
        sorted_main,
        vec![
            "leaf.rs::compute",
            "lib.rs::process",
            "main.rs::helper",
            "main.rs::main",
        ],
        "main flow members"
    );

    // main's criticality > 0 (has members contributing degree).
    assert!(
        main_flow["criticality"].as_i64().unwrap() > 0,
        "main flow should have non-zero criticality"
    );
}

#[test]
fn flow_single_trace() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--flow", "main"]);
    assert!(
        out.status.success(),
        "exit non-zero; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);

    assert_eq!(env["query_type"], "flow");
    assert_eq!(env["entry"], "main.rs::main");

    let members: Vec<&str> = env["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(members.len(), 4, "main flow has 4 members");
}

#[test]
fn flow_orphan_entry() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--flow", "run"]);
    assert!(
        out.status.success(),
        "exit non-zero; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);

    assert_eq!(env["query_type"], "flow");
    let members = env["members"].as_array().unwrap();
    assert_eq!(members.len(), 1, "run flow has 1 member (orphan)");
    assert_eq!(members[0], "run.rs::run");
}

#[test]
fn flow_not_found() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--flow", "nonexistent"]);
    assert!(
        !out.status.success(),
        "should exit non-zero for unknown symbol"
    );
    let env = parse_single_json(&out);
    assert_eq!(env["query_type"], "error");
    assert_eq!(env["error"]["code"], "not_found");
}

#[test]
fn flows_guard_rejects_kind() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--flows", "--kind", "function"]);
    assert!(
        !out.status.success(),
        "--kind should be rejected with --flows"
    );
}

#[test]
fn flows_guard_rejects_file_level() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--flows", "--file-level"]);
    assert!(
        !out.status.success(),
        "--file-level should be rejected with --flows"
    );
}

#[test]
fn communities_detected() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--communities"]);
    assert!(
        out.status.success(),
        "exit non-zero; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);

    assert_eq!(env["query_type"], "communities");

    let comms = env["communities"].as_array().expect("communities is array");
    assert!(comms.len() >= 2, "expected at least 2 communities");

    // Leiden (not the old single-pass Louvain) keeps the main.rs pair together
    // and the process/compute/handler triangle separate. Hand-counted.
    let main_comm = comms
        .iter()
        .find(|c| {
            c["members"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m.as_str().unwrap().ends_with("::main"))
        })
        .expect("community containing main");
    let mut main_members: Vec<&str> = main_comm["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap().rsplit("::").next().unwrap())
        .collect();
    main_members.sort();
    assert_eq!(main_members, vec!["helper", "main"]);
}

#[test]
fn community_lookup_by_name() {
    let tmp = init_fixture();
    let root = tmp.path();

    // First get the community name from --communities.
    let out = run_in(root, &["query", "--communities"]);
    let env = parse_single_json(&out);
    let comms = env["communities"].as_array().unwrap();
    let main_comm = comms
        .iter()
        .find(|c| {
            c["members"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m.as_str().unwrap().ends_with("::main"))
        })
        .unwrap();
    let name = main_comm["name"].as_str().unwrap().to_string();

    // Now look up by name.
    let out = run_in(root, &["query", "--community", &name]);
    assert!(
        out.status.success(),
        "exit non-zero; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);
    assert_eq!(env["query_type"], "community");
    assert_eq!(
        env["members"].as_array().unwrap().len(),
        2,
        "main community is the main/helper pair"
    );
}

#[test]
fn community_not_found() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out = run_in(root, &["query", "--community", "nonexistent"]);
    assert!(
        !out.status.success(),
        "should exit non-zero for unknown community"
    );
    let env = parse_single_json(&out);
    assert_eq!(env["query_type"], "error");
    assert_eq!(env["error"]["code"], "not_found");
}

#[test]
fn determinism_flows_twice() {
    let tmp = init_fixture();
    let root = tmp.path();

    let out1 = run_in(root, &["query", "--flows"]);
    let out2 = run_in(root, &["query", "--flows"]);

    let json1 = String::from_utf8_lossy(&out1.stdout);
    let json2 = String::from_utf8_lossy(&out2.stdout);
    assert_eq!(json1, json2, "two --flows invocations should be byte-identical");
}
