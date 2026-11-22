// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for call-site evidence and the `--context` pack.
//!
//! `cargo test --test context_pack`
//!
//! The fixture is built so each retriever's contribution is hand-countable:
//! `alpha` calls `beta` on a known line; `lexical_marker` matches only FTS;
//! the vector neighbor is asserted only when the model is present and the hit
//! is not also a lexical match.

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
        panic!(
            "parse: {e}\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

const SRC: &str = "\
fn alpha() {
    beta();
}
fn beta() -> bool {
    true
}
fn lexical_marker_token() -> i32 {
    1
}
";

#[test]
fn context_tags_graph_and_lexical_hits() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("lib.rs"), SRC).unwrap();
    let init = run_in(root, &["init"]);
    assert!(
        init.status.success(),
        "init: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let out = run_in(root, &["query", "--context", "alpha lexical_marker_token"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_json(&out);
    assert_eq!(env["query_type"], "context");
    let hits = env["hits"].as_array().unwrap();

    let beta = hits
        .iter()
        .find(|h| h["name"] == "beta")
        .expect("graph hop must surface beta");
    let tags = beta["matched_by"].as_array().unwrap();
    assert!(tags.iter().any(|t| t == "graph"), "matched_by: {tags:?}");
    assert_eq!(beta["evidence"]["path"], "lib.rs");
    assert_eq!(beta["evidence"]["start_line"], 2);
    assert_eq!(beta["evidence"]["end_line"], 2);

    let lexical = hits
        .iter()
        .find(|h| h["name"] == "lexical_marker_token")
        .expect("fts must surface lexical_marker_token");
    let lex_tags: Vec<&str> = lexical["matched_by"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    // The name is a unique token; if the vector retriever also ranks it, both
    // tags are listed and sorted. A lexical-only hit has exactly ["fts"].
    assert!(lex_tags.contains(&"fts"), "{lex_tags:?}");
    assert!(lex_tags.is_sorted());
    if lex_tags == ["fts"] {
        assert!(lexical.get("evidence").is_none());
    }

    let again = run_in(root, &["query", "--context", "alpha lexical_marker_token"]);
    assert_eq!(out.stdout, again.stdout);
}

#[test]
fn callers_envelope_has_no_span_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("lib.rs"), SRC).unwrap();
    assert!(run_in(root, &["init"]).status.success());
    let env = parse_json(&run_in(root, &["query", "--callers", "beta"]));
    let hit = &env["hits"][0];
    assert!(hit.get("evidence").is_none(), "shipped callers shape must not gain evidence");
    assert!(hit.get("start_line").is_none());
}
