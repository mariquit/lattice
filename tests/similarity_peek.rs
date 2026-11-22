// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for similarity search and low-token peek.
//!
//! `cargo test --test similarity_peek`

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
fn similar_ranks_duplicate_and_excludes_self() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("one.rs"),
        "\
pub fn parse_token(input: &str) -> u32 {
    // validate the incoming token string and return its trimmed length
    input.trim().len() as u32
}
",
    )
    .unwrap();
    fs::write(
        root.join("two.rs"),
        "\
pub fn parse_token_copy(input: &str) -> u32 {
    // validate the incoming token string and return its trimmed length
    input.trim().len() as u32
}
",
    )
    .unwrap();
    fs::write(
        root.join("other.rs"),
        "pub fn render_dashboard() -> &'static str { \"widgets charts layout pixels canvas\" }\n",
    )
    .unwrap();

    let init = run_in(root, &["init"]);
    assert!(init.status.success(), "init: {}", String::from_utf8_lossy(&init.stderr));

    let out = run_in(root, &["query", "--similar", "parse_token"]);
    assert!(out.status.success(), "similar: {}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);
    assert_eq!(env["query_type"], "similar");
    assert_eq!(env["symbol"], "parse_token");
    let hits = env["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "expected neighbors");
    assert_eq!(hits[0]["name"], "parse_token_copy");
    assert!(
        hits.iter().all(|h| h["name"] != "parse_token"),
        "must not return itself: {hits:?}"
    );

    let peek = run_in(root, &["query", "--peek", "validate the incoming token string"]);
    assert!(peek.status.success(), "peek: {}", String::from_utf8_lossy(&peek.stderr));
    let penv = parse_json(&peek);
    assert_eq!(penv["query_type"], "peek");
    let phits = penv["hits"].as_array().unwrap();
    assert!(!phits.is_empty(), "peek should hit the token functions");
    for hit in phits {
        assert!(hit.get("start_line").is_none(), "peek must not carry start_line: {hit}");
        assert!(hit.get("end_line").is_none(), "peek must not carry end_line: {hit}");
        assert!(hit.get("path").is_some());
        assert!(hit.get("name").is_some());
        assert!(hit.get("line").is_some());
        assert!(hit.get("class").is_some());
    }
}
