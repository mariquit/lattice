// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Call-edge evidence: `binding` vs `name`, fail-closed ambiguity, and
//! `--min-evidence` / `--with-evidence`.
//!
//! Default envelopes (flag absent) keep the pre-existing hit shape. The flags
//! are opt-in, so protocol stays 1.1.0.

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

fn parse_single_json(out: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    assert!(
        trimmed.starts_with('{') && trimmed.ends_with('}'),
        "stdout is not one JSON object: {stdout:?}"
    );
    serde_json::from_str(trimmed).unwrap_or_else(|e| panic!("json ({e}): {stdout:?}"))
}

fn write(dir: &Path, name: &str, body: &str) {
    fs::write(dir.join(name), body).expect("write fixture");
}

fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("temp");
    let dir = tmp.path();
    write(
        dir,
        "binding.rs",
        "pub fn bound() {}\n",
    );
    write(
        dir,
        "name_only.rs",
        "pub fn unique_leaf() {}\n",
    );
    write(dir, "ambiguous_a.rs", "pub fn shared() {}\n");
    write(dir, "ambiguous_b.rs", "pub fn shared() {}\n");
    write(
        dir,
        "dup.rs",
        "pub fn twin() {}\npub fn twin() {}\nfn calls_twin() { twin(); }\n",
    );
    write(
        dir,
        "caller.rs",
        "mod binding;\nmod name_only;\nmod ambiguous_a;\nmod ambiguous_b;\n\n\
         fn calls_bound() { binding::bound(); }\n\
         fn calls_name() { unique_leaf(); }\n\
         fn calls_ambiguous() { shared(); }\n",
    );
    let out = run_in(dir, &["init"]);
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    tmp
}

fn hit(path: &str, name: &str, line: u64) -> Value {
    serde_json::json!({ "path": path, "name": name, "line": line })
}

#[test]
fn binding_and_name_edges_and_fail_closed() {
    let tmp = fixture();
    let root = tmp.path();

    let bound = parse_single_json(&run_in(
        root,
        &["query", "--callers", "bound", "--with-evidence"],
    ));
    assert!(run_in(root, &["query", "--callers", "bound", "--with-evidence"])
        .status
        .success());
    assert_eq!(bound["query_type"], "callers");
    assert!(bound.get("min_evidence").is_none(), "flag absent → field omitted");
    assert_eq!(
        bound["hits"],
        serde_json::json!([{
            "path": "caller.rs",
            "name": "calls_bound",
            "line": 6,
            "evidence": "binding"
        }])
    );

    let named = parse_single_json(&run_in(
        root,
        &["query", "--callers", "unique_leaf", "--with-evidence"],
    ));
    assert_eq!(
        named["hits"],
        serde_json::json!([{
            "path": "caller.rs",
            "name": "calls_name",
            "line": 7,
            "evidence": "name"
        }])
    );

    // Default shape has no evidence field.
    let plain = parse_single_json(&run_in(root, &["query", "--callers", "unique_leaf"]));
    assert_eq!(
        plain["hits"],
        serde_json::json!([hit("caller.rs", "calls_name", 7)])
    );

    let shared = parse_single_json(&run_in(root, &["query", "--callers", "shared"]));
    assert_eq!(shared["hits"], serde_json::json!([]), "ambiguous bare name: no edge");

    let twin = parse_single_json(&run_in(root, &["query", "--callers", "twin"]));
    assert_eq!(twin["hits"], serde_json::json!([]), "two same-file defs: no edge");
}

#[test]
fn min_evidence_binding_drops_name_edges() {
    let tmp = fixture();
    let root = tmp.path();

    let kept = parse_single_json(&run_in(
        root,
        &["query", "--callers", "bound", "--min-evidence", "binding"],
    ));
    assert_eq!(kept["min_evidence"], "binding");
    assert_eq!(
        kept["hits"],
        serde_json::json!([hit("caller.rs", "calls_bound", 6)])
    );

    let dropped = parse_single_json(&run_in(
        root,
        &[
            "query",
            "--callers",
            "unique_leaf",
            "--min-evidence",
            "binding",
        ],
    ));
    assert_eq!(dropped["hits"], serde_json::json!([]));

    let chain = parse_single_json(&run_in(
        root,
        &[
            "query",
            "--call-chain",
            "calls_name",
            "unique_leaf",
            "--min-evidence",
            "binding",
        ],
    ));
    assert_eq!(chain["paths"], serde_json::json!([]));

    let chain_default = parse_single_json(&run_in(
        root,
        &["query", "--call-chain", "calls_name", "unique_leaf"],
    ));
    assert_eq!(chain_default["paths"].as_array().unwrap().len(), 1);

    // File impact: the name-only call drops; the `mod` import remains a dependent.
    let impact = parse_single_json(&run_in(
        root,
        &[
            "query",
            "--impact",
            "name_only.rs",
            "--min-evidence",
            "binding",
        ],
    ));
    assert_eq!(impact["callers"], serde_json::json!([]));
    assert_eq!(impact["dependents"], serde_json::json!(["caller.rs"]));
    assert_eq!(impact["blast_radius"], 1);
}

#[test]
fn min_evidence_rejected_off_call_queries() {
    let tmp = fixture();
    let root = tmp.path();
    let out = run_in(root, &["query", "--symbol", "bound", "--min-evidence", "binding"]);
    assert!(!out.status.success());
    let env = parse_single_json(&out);
    assert_eq!(env["error"]["code"], "invalid_flag");

    let bad = run_in(root, &["query", "--callers", "bound", "--min-evidence", "type"]);
    assert!(!bad.status.success());
    let env = parse_single_json(&bad);
    assert_eq!(env["error"]["code"], "invalid_flag");
}
