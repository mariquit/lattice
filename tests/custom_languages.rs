// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for custom languages via `languages.toml`.
//!
//! `cargo test --test custom_languages`

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

const WIDGET: &str = "\
fn alpha() {
    beta();
}
fn beta() -> bool {
    true
}
";

const TOML_OK: &str = "\
[[language]]
extensions = [\"ktsub\"]
grammar = \"rust\"
functions = [\"function_item\"]
classes = [\"impl_item\"]
imports = [\"use_declaration\"]
calls = [\"call_expression\"]
";

#[test]
fn custom_extension_ingests_functions_and_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".lattice")).unwrap();
    fs::write(root.join(".lattice/languages.toml"), TOML_OK).unwrap();
    fs::write(root.join("widget.ktsub"), WIDGET).unwrap();

    let init = run_in(root, &["init"]);
    assert!(init.status.success(), "init: {}", String::from_utf8_lossy(&init.stderr));

    let callees = run_in(root, &["query", "--callees", "alpha"]);
    assert!(callees.status.success(), "{}", String::from_utf8_lossy(&callees.stderr));
    let env = parse_json(&callees);
    let names: Vec<&str> = env["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["beta"]);

    let alpha = run_in(root, &["query", "--symbol", "alpha"]);
    let beta = run_in(root, &["query", "--symbol", "beta"]);
    assert_eq!(parse_json(&alpha)["hits"].as_array().unwrap().len(), 1);
    assert_eq!(parse_json(&beta)["hits"].as_array().unwrap().len(), 1);
}

#[test]
fn unknown_grammar_errors_without_corrupting() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("lib.rs"), "pub fn kept() {}\n").unwrap();
    assert!(run_in(root, &["init"]).status.success());

    fs::create_dir_all(root.join(".lattice")).unwrap();
    fs::write(
        root.join(".lattice/languages.toml"),
        "[[language]]\nextensions = [\"zzz\"]\ngrammar = \"cobol\"\nfunctions = [\"function_item\"]\n",
    )
    .unwrap();
    let bad = run_in(root, &["init"]);
    assert!(!bad.status.success(), "unknown grammar must fail");
    let env = parse_json(&bad);
    assert_eq!(env["error"]["code"], "unknown_grammar");

    let kept = run_in(root, &["query", "--symbol", "kept"]);
    assert!(kept.status.success(), "{}", String::from_utf8_lossy(&kept.stderr));
    assert_eq!(parse_json(&kept)["hits"].as_array().unwrap().len(), 1);
}

const KOTLIN: &str = "\
fun alpha() {
    beta()
}
fun beta(): Boolean {
    return true
}
";

const KOTLIN_TOML: &str = "\
[[language]]
extensions = [\"kt\"]
grammar = \"kotlin\"
functions = [\"function_declaration\"]
imports = [\"import_header\"]
calls = [\"call_expression\"]
";

#[test]
fn kotlin_grammar_ingests_functions_and_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".lattice")).unwrap();
    fs::write(root.join(".lattice/languages.toml"), KOTLIN_TOML).unwrap();
    fs::write(root.join("Widget.kt"), KOTLIN).unwrap();

    let init = run_in(root, &["init"]);
    assert!(
        init.status.success(),
        "init: {}\n{}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );

    let callees = run_in(root, &["query", "--callees", "alpha"]);
    assert!(
        callees.status.success(),
        "{}",
        String::from_utf8_lossy(&callees.stderr)
    );
    let parsed = parse_json(&callees);
    let names: Vec<&str> = parsed["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["beta"]);
}

#[test]
fn no_config_ignores_custom_extension() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("lib.rs"), "pub fn kept() {}\n").unwrap();
    fs::write(root.join("widget.ktsub"), WIDGET).unwrap();
    assert!(run_in(root, &["init"]).status.success());
    let alpha = run_in(root, &["query", "--symbol", "alpha"]);
    assert!(alpha.status.success());
    assert_eq!(
        parse_json(&alpha)["hits"].as_array().unwrap().len(),
        0,
        "without languages.toml the custom extension must not be ingested"
    );
    let kept = run_in(root, &["query", "--symbol", "kept"]);
    assert_eq!(parse_json(&kept)["hits"].as_array().unwrap().len(), 1);
}
