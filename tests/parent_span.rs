// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Split Markdown hits carry the parent section span. Code hits and unsplit
//! sections do not, so their envelopes stay unchanged.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

fn run_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn lattice")
}

fn parse_json(out: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("json ({e}): {stdout:?}"))
}

#[test]
fn split_section_hit_carries_parent_span() {
    let tmp = tempfile::tempdir().expect("temp");
    let root = tmp.path();

    let mut long = String::from("# Zephyr Protocol\n");
    for i in 0..150 {
        long.push_str(&format!("zephyr line {i}\n"));
    }
    long.push_str("# Short Note\n\nhello\n");
    fs::write(root.join("notes.md"), long).unwrap();
    fs::write(root.join("lib.rs"), "fn keep() {}\n").unwrap();

    let init = run_in(root, &["init"]);
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let out = run_in(root, &["query", "--symbol", "Zephyr", "--limit", "20"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);
    let hits = env["hits"].as_array().expect("hits");
    let parts: Vec<&Value> = hits
        .iter()
        .filter(|h| h["kind"] == "section" && h["path"] == "notes.md")
        .filter(|h| {
            h["name"]
                .as_str()
                .unwrap_or("")
                .starts_with("Zephyr Protocol")
        })
        .collect();
    assert!(parts.len() >= 2, "expected split parts, got {hits:?}");
    for hit in &parts {
        assert_eq!(hit["parent"]["name"], "Zephyr Protocol");
        assert_eq!(hit["parent"]["start_line"], 1);
        assert!(hit["parent"]["end_line"].as_u64().unwrap() > 150);
    }

    let short = run_in(root, &["query", "--symbol", "Short", "--limit", "20"]);
    let short_env = parse_json(&short);
    let short_hit = short_env["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["name"] == "Short Note")
        .expect("unsplit section");
    assert!(short_hit.get("parent").is_none(), "unsplit section has no parent");

    let code = run_in(root, &["query", "--symbol", "keep", "--limit", "20"]);
    let code_env = parse_json(&code);
    for hit in code_env["hits"].as_array().unwrap() {
        if hit["kind"] != "section" {
            assert!(hit.get("parent").is_none(), "code hit must not gain parent");
        }
    }
}
