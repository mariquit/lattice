// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Integration tests for static hints/suggestions (PLAN Feature 4).
//!
//! Test gate: `cargo test --test hints`

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
fn default_envelope_unchanged_without_hints() {
    let tmp = fixture_copy();
    let root = tmp.path();

    let out = run_in(root, &["query", "--impact", "main.rs"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);

    assert_eq!(env["protocol_version"], "1.5.0");
    assert!(env.get("next_tool_suggestions").is_none(), "should NOT have next_tool_suggestions without --hints");
}

#[test]
fn hints_flag_adds_suggestions() {
    let tmp = fixture_copy();
    let root = tmp.path();

    let out = run_in(root, &["query", "--impact", "main.rs", "--hints"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);

    let suggestions = env["next_tool_suggestions"].as_array().expect("next_tool_suggestions is array");
    assert!(suggestions.len() >= 1, "should have at least 1 suggestion");

    for s in suggestions {
        assert!(s.get("tool").is_some(), "suggestion needs tool");
        assert!(s.get("description").is_some(), "suggestion needs description");
    }

    // Impact should suggest callers/callees.
    let tools: Vec<&str> = suggestions.iter().map(|s| s["tool"].as_str().unwrap()).collect();
    assert!(tools.contains(&"lattice query --callers <symbol>"), "impact should suggest callers");
}

#[test]
fn hinted_commands_execute_with_fixture_arguments() {
    let tmp = fixture_copy();
    let root = tmp.path();
    let symbol = run_in(root, &["query", "--symbol", "main"]);
    let symbol = parse_json(&symbol);
    let name = symbol["hits"].as_array().unwrap().iter()
        .find(|hit| hit["kind"] == "function").unwrap()["name"].as_str().unwrap();
    let communities = parse_json(&run_in(root, &["query", "--communities"]));
    let id = communities["communities"][0]["id"].to_string();
    let mut seen = std::collections::BTreeSet::new();
    for kind in ["impact", "flow", "hotspots", "dead_code", "communities", "semantic", "hybrid", "symbol", "owners", "coupling", "health", "review"] {
        for hint in lattice::analysis::next_tools(kind) {
            if !seen.insert(hint.tool.clone()) { continue; }
            let parts: Vec<_> = hint.tool.split_whitespace().collect();
            assert_eq!(parts.first(), Some(&"lattice"), "{}", hint.tool);
            let args: Vec<_> = parts[1..].iter().map(|part| match *part {
                "<symbol>" | "<entry>" => name,
                "<path>" => "main.rs",
                "<id>" => &id,
                "\"<question>\"" => "find retry handling",
                other => other,
            }).collect();
            let out = run_in(root, &args);
            assert!(out.status.success(), "{}: {} {}", hint.tool, String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        }
    }
}

#[test]
fn hints_works_on_multiple_intents() {
    let tmp = fixture_copy();
    let root = tmp.path();

    // --dead-code --hints
    let out = run_in(root, &["query", "--dead-code", "--hints"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);
    let suggestions = env["next_tool_suggestions"].as_array().unwrap();
    assert!(!suggestions.is_empty(), "dead-code should have suggestions");

    // --flows --hints
    let out = run_in(root, &["query", "--flows", "--hints"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);
    let suggestions = env["next_tool_suggestions"].as_array().unwrap();
    assert!(!suggestions.is_empty(), "flows should have suggestions");
}
