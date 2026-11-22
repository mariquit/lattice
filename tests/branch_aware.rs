// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for branch-aware indexing.
//!
//! `cargo test --test branch_aware`
//!
//! A symbol added only on branch `feature` is absent from a query on `main`,
//! present exactly once on `feature`, and present from `main` under
//! `--all-branches`. The envelope `branch` field names the scope.

use std::fs;
use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

#[test]
fn non_symbol_queries_reject_branch_flags_before_refresh() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(".lattice")).unwrap();
    for query in [&["--impact", "lib.rs"][..], &["find retry handling"][..], &["--context", "retry"][..]] {
        for scope in [&["--branch", "other"][..], &["--all-branches"][..]] {
            let args: Vec<_> = ["query"].into_iter().chain(query.iter().copied()).chain(scope.iter().copied()).collect();
            let out = run_in(tmp.path(), &args);
            let env = parse_json(&out);
            assert_eq!(out.status.code(), Some(2), "{args:?}: {env}");
            assert_eq!(env["error"]["code"], "invalid_flag");
            assert!(env["error"]["message"].as_str().unwrap().contains("--symbol"));
            assert!(env.get("branch").is_none());
            assert!(!tmp.path().join(".lattice/meta.db").exists());
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

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
        panic!(
            "parse JSON: {e}\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn hit_names(env: &serde_json::Value) -> Vec<String> {
    env["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn symbol_scoped_to_active_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    fs::write(root.join("base.rs"), "pub fn base() {}\n").unwrap();
    git(root, &["add", "base.rs"]);
    git(root, &["commit", "-m", "base"]);

    let init = run_in(root, &["init"]);
    assert!(init.status.success(), "init: {}", String::from_utf8_lossy(&init.stderr));

    git(root, &["checkout", "-b", "feature"]);
    fs::write(root.join("feature.rs"), "pub fn zxqv_only() {}\n").unwrap();
    git(root, &["add", "feature.rs"]);
    git(root, &["commit", "-m", "feature symbol"]);

    let on_feature = run_in(root, &["query", "--symbol", "zxqv_only"]);
    assert!(on_feature.status.success(), "{}", String::from_utf8_lossy(&on_feature.stderr));
    let feat = parse_json(&on_feature);
    assert_eq!(feat["protocol_version"], "1.5.0");
    assert_eq!(feat["branch"], "feature");
    assert_eq!(hit_names(&feat), vec!["zxqv_only".to_string()]);

    git(root, &["checkout", "main"]);
    let on_main = run_in(root, &["query", "--symbol", "zxqv_only"]);
    assert!(on_main.status.success(), "{}", String::from_utf8_lossy(&on_main.stderr));
    let main = parse_json(&on_main);
    assert_eq!(main["branch"], "main");
    assert_eq!(hit_names(&main), Vec::<String>::new(), "main must not see the feature-only symbol");

    let all = run_in(root, &["query", "--symbol", "zxqv_only", "--all-branches"]);
    assert!(all.status.success(), "{}", String::from_utf8_lossy(&all.stderr));
    let all_env = parse_json(&all);
    assert_eq!(all_env["branch"], "*");
    assert_eq!(hit_names(&all_env), vec!["zxqv_only".to_string()]);

    let explicit = run_in(root, &["query", "--symbol", "zxqv_only", "--branch", "feature"]);
    assert!(explicit.status.success(), "{}", String::from_utf8_lossy(&explicit.stderr));
    let exp = parse_json(&explicit);
    assert_eq!(exp["branch"], "feature");
    assert_eq!(hit_names(&exp), vec!["zxqv_only".to_string()]);
}
