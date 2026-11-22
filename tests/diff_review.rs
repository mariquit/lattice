// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for diff review scoring.
//!
//! `cargo test --test diff_review`
//!
//! A range edits one function with two transitive callers and no test caller.
//! `--review` lists that function with the exact caller counts, flags the test
//! gap, and returns the owner. An empty range returns empty sets.

use std::fs;
use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(dir).output().expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

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
fn review_matches_each_hunk_against_its_own_revision() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.email", "ada@example.com"]);
    git(root, &["config", "user.name", "Ada"]);
    fs::write(root.join("lib.rs"), "pub fn surviving() {\n    let _deleted = 1;\n    let _kept = 2;\n}\n\npub fn edited() {\n    let _value = 3;\n}\n").unwrap();
    fs::write(root.join("gone.rs"), "pub fn removed() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "base"]);
    git(root, &["tag", "before"]);
    fs::write(root.join("lib.rs"), "pub fn surviving() {\n    let _kept = 2;\n}\n\npub fn edited() {\n    let _value = 4;\n}\n").unwrap();
    git(root, &["rm", "gone.rs"]);
    git(root, &["add", "lib.rs"]);
    git(root, &["commit", "-m", "delete and edit"]);
    git(root, &["tag", "after"]);

    // The current checkout has unrelated functions at the old and new hunk lines.
    fs::write(root.join("lib.rs"), "pub fn unrelated() {\n    let _a = 0;\n    let _b = 0;\n    let _c = 0;\n    let _d = 0;\n    let _e = 0;\n}\n").unwrap();
    let init = run_in(root, &["init"]);
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    let out = run_in(root, &["query", "--review", "before..after"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);
    assert_eq!(env["changed"], serde_json::json!([
        {"path": "gone.rs", "symbols": ["removed"]},
        {"path": "lib.rs", "symbols": ["edited", "surviving"]}
    ]));
    assert_eq!(env["affected"], serde_json::json!([]));
}

#[test]
fn review_scores_edited_function() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.email", "ada@example.com"]);
    git(root, &["config", "user.name", "Ada"]);

    fs::write(
        root.join("lib.rs"),
        "\
pub fn caller_a() { caller_b(); }
pub fn caller_b() { target(); }
pub fn target() { let _ = 1; }
",
    )
    .unwrap();
    git(root, &["add", "lib.rs"]);
    git(root, &["commit", "-m", "base"]);

    fs::write(
        root.join("lib.rs"),
        "\
pub fn caller_a() { caller_b(); }
pub fn caller_b() { target(); }
pub fn target() { let _ = 2; }
",
    )
    .unwrap();
    git(root, &["add", "lib.rs"]);
    git(root, &["commit", "-m", "edit target"]);

    let init = run_in(root, &["init"]);
    assert!(init.status.success(), "init: {}", String::from_utf8_lossy(&init.stderr));

    let out = run_in(root, &["query", "--review", "HEAD~1..HEAD"]);
    assert!(out.status.success(), "review: {}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);
    assert_eq!(env["protocol_version"], "1.5.0");
    assert_eq!(env["query_type"], "review");

    let affected = env["affected"].as_array().unwrap();
    assert_eq!(affected.len(), 1, "affected: {affected:?}");
    assert_eq!(affected[0]["symbol"], "target");
    assert_eq!(affected[0]["path"], "lib.rs");
    assert_eq!(affected[0]["callers"], 1);
    assert_eq!(affected[0]["transitive_callers"], 2);

    let owners = run_in(root, &["query", "--owners", "lib.rs"]);
    let own = parse_json(&owners);
    let churn = own["churn"].as_u64().unwrap();
    assert_eq!(
        affected[0]["risk"].as_u64().unwrap(),
        2 * churn,
        "risk = transitive_callers * churn"
    );

    let gaps = env["test_gaps"].as_array().unwrap();
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0]["symbol"], "target");

    let owner_rows = env["owners"].as_array().unwrap();
    let authors = owner_rows[0]["owners"].as_array().unwrap();
    assert!(
        authors.iter().any(|a| a.as_str().unwrap().contains("Ada")),
        "owners: {authors:?}"
    );

    let empty = run_in(root, &["query", "--review", "HEAD..HEAD"]);
    assert!(empty.status.success(), "empty range: {}", String::from_utf8_lossy(&empty.stderr));
    let empty_env = parse_json(&empty);
    assert_eq!(empty_env["changed"].as_array().unwrap().len(), 0);
    assert_eq!(empty_env["affected"].as_array().unwrap().len(), 0);
    assert_eq!(empty_env["test_gaps"].as_array().unwrap().len(), 0);
    assert_eq!(empty_env["owners"].as_array().unwrap().len(), 0);
}
