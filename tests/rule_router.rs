// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for default hybrid search and explicit query intents.
//!
//! `cargo test --test rule_router`

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

fn init() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("lib.rs"),
        "fn retry_handler() { helper(); }\nfn helper() {}\nfn exact_words() {}\n",
    )
    .unwrap();
    let init = run_in(tmp.path(), &["init"]);
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    tmp
}

#[test]
fn quoted_phrase_uses_hybrid_search() {
    let tmp = init();
    let env = parse_json(&run_in(tmp.path(), &["query", "\"exact_words\""]));
    assert_eq!(env["query_type"], "hybrid");
}

#[test]
fn symbol_shaped_text_uses_hybrid_search() {
    let tmp = init();
    let env = parse_json(&run_in(tmp.path(), &["query", "retry_handler"]));
    assert_eq!(env["query_type"], "hybrid");
    assert!(!env["hits"].as_array().unwrap().is_empty());
}

#[test]
fn free_text_uses_hybrid_search() {
    let tmp = init();
    let env = parse_json(&run_in(tmp.path(), &["query", "where is retry handled"]));
    assert_eq!(env["query_type"], "hybrid");
}

#[test]
fn explicit_flag_is_not_rewritten() {
    let tmp = init();
    let env = parse_json(&run_in(tmp.path(), &["query", "--similar", "retry_handler"]));
    // similar may error (ambiguous/not embedded uniquely) — either way the
    // router must not rewrite the intent. Success stamps routed_by explicit.
    if env["query_type"] != "error" {
        assert_eq!(env["query_type"], "similar");
        assert_eq!(env["routed_by"], "explicit");
    } else {
        assert_ne!(env["query_type"], "context");
        assert_ne!(env["query_type"], "symbol");
    }
}

#[test]
fn retired_flags_are_invalid() {
    let tmp = init();
    for flag in ["--no-route", "--hybrid"] {
        let out = run_in(tmp.path(), &["query", "where is retry handled", flag]);
        assert!(!out.status.success());
        assert_eq!(parse_json(&out)["error"]["code"], "invalid_flag");
    }
}
