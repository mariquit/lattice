// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for slice `fts-lexical-search` (slice 8).
//!
//! lattice's semantic query (feature-hash vectors) ranks whole files by
//! bag-of-words similarity — good for "what is X" but weak at exact symbol
//! lookup, the thing tokensave's FTS5 search does well. This slice adds a
//! lexical symbol index (FTS5 over symbol names, tokenized with the same
//! snake/camelCase splitter the embedder uses) queried via a new additive
//! router intent `query --symbol "<text>"`.
//!
//! Envelope: `{ protocol_version, query_type: "symbol", query,
//!             hits: [{ path, name, kind, line }] }`, ordered deterministically.
//!
//! Ground truth (tests/fixtures/rust_basic):
//!   auth.rs    -> validate_token (line 1), check_signature (line 5)
//!   db.rs      -> save_session (line 1)
//!   metrics.rs -> report (line 3)
//!   main.rs    -> main (line 5)
//!
//! Scenarios:
//!   1. `query --symbol "validate"` matches validate_token (snake-split: the
//!      "validate" prefix hits the "validate" sub-token of validate_token),
//!      query_type "symbol", exit 0, top hit path auth.rs / kind function /
//!      line 1.
//!   2. `query --symbol "signature"` matches check_signature in auth.rs line 5.
//!   3. `query --symbol "zzzznomatch"` -> exit 0, query_type "symbol", empty
//!      hits (a clean miss is not an error).
//!   4. Determinism: identical query yields byte-identical stdout across runs.
//!   5. Stream discipline: exactly one JSON object on stdout.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

fn fixture_copy() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic");
    for entry in fs::read_dir(&src).expect("read fixture dir") {
        let entry = entry.expect("fixture dir entry");
        let name = entry.file_name();
        fs::copy(entry.path(), tmp.path().join(&name)).expect("copy fixture file");
    }
    tmp
}

fn run_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn lattice binary")
}

fn parse_single_json(output: &Output) -> Value {
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8");
    let trimmed = stdout.trim();
    assert!(
        trimmed.starts_with('{') && trimmed.ends_with('}'),
        "stdout is not a single JSON object: {stdout:?}"
    );
    serde_json::from_str::<Value>(trimmed)
        .unwrap_or_else(|e| panic!("stdout is not exactly one JSON object ({e}): {stdout:?}"))
}

#[test]
fn symbol_search_finds_symbols_by_name() {
    let tmp = fixture_copy();
    let root = tmp.path();
    let init = run_in(root, &["init"]);
    assert!(
        init.status.success(),
        "init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    // ---- Scenario 1: prefix of a snake-cased symbol ----
    let s1 = run_in(root, &["query", "--symbol", "validate"]);
    assert!(
        s1.status.success(),
        "symbol query must exit 0; stderr: {}",
        String::from_utf8_lossy(&s1.stderr)
    );
    let e1 = parse_single_json(&s1);
    assert_eq!(e1["protocol_version"], "1.5.0");
    assert_eq!(e1["query_type"], "symbol");
    assert_eq!(e1["query"], "validate");
    let hits = e1["hits"].as_array().expect("hits is an array");
    assert!(!hits.is_empty(), "expected at least one hit for 'validate'");
    let top = &hits[0];
    assert_eq!(top["name"], "validate_token");
    assert_eq!(top["path"], "auth.rs");
    assert_eq!(top["kind"], "function");
    assert_eq!(top["line"], 1);

    // ---- Scenario 2: a different symbol in the same file ----
    let s2 = run_in(root, &["query", "--symbol", "signature"]);
    assert!(s2.status.success());
    let e2 = parse_single_json(&s2);
    let hits2 = e2["hits"].as_array().expect("hits is an array");
    assert!(
        hits2.iter().any(|h| h["name"] == "check_signature"
            && h["path"] == "auth.rs"
            && h["line"] == 5),
        "expected check_signature@auth.rs:5 among hits: {e2}"
    );

    // ---- Scenario 3: a clean miss is empty, not an error ----
    let s3 = run_in(root, &["query", "--symbol", "zzzznomatch"]);
    assert!(
        s3.status.success(),
        "a no-match symbol query must still exit 0"
    );
    let e3 = parse_single_json(&s3);
    assert_eq!(e3["query_type"], "symbol");
    assert_eq!(
        e3["hits"].as_array().expect("hits is an array").len(),
        0,
        "no matches must yield an empty hits array"
    );

    // ---- Scenario 4: deterministic byte-stable output ----
    let again = run_in(root, &["query", "--symbol", "validate"]);
    assert_eq!(
        again.stdout, s1.stdout,
        "identical symbol query must be byte-identical across runs"
    );
}
