// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for slice `bm25-symbol-ranking` (S2).
//!
//! Proves that the explicit bm25(symbols, 10.0, 1.0) column weighting is
//! load-bearing: a tokens-only hit (short tokens-col doc, strong BM25 stats)
//! would OUTRANK a name-column hit (long name, poor length-norm) under equal
//! weights (HEAD), but the 10:1 name/tokens weighting FLIPS the order so the
//! name-column hit wins.
//!
//! Fixture: tests/fixtures/rust_ranking/ranking.rs
//!   - validate_all_incoming_user_supplied_request: name-col match, long doc
//!     (6 tokens in name col) → poor length-norm → weak equal-weight score.
//!   - reValidate: tokens-col match only (camelCase name doesn't match "validate"
//!     prefix directly), short tokens-col doc ("re validate", 2 tokens) →
//!     strong equal-weight score.
//!   - foo, bar, baz: non-matching filler to restore IDF.
//!
//! Expected behaviour AFTER the weighting fix:
//!   query --symbol "validate" → top two hits in order:
//!     1. validate_all_incoming_user_supplied_request  (name-col, boosted 10×)
//!     2. reValidate                                   (tokens-col, weight 1×)
//!
//! Under equal weights (HEAD), the order is REVERSED (tokens-only hit wins),
//! so this gate FAILS on HEAD and PASSES only after the bm25(10,1) fix.
//!
//! Tiebreak proof: both symbols have in_degree=0 (equal tiebreak inert).
//! Alphabetical name-ASC would produce ["reValidate", "validate_all…"] —
//! the OPPOSITE of the expected weighted order — so the sort must come from
//! bm25, not the tiebreak chain.
//!
//! Protocol: query_type "symbol", protocol_version from shared PROTOCOL_VERSION
//! const (current: "1.5.0"). No bump — only relative order changes.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

fn fixture_copy() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_ranking");
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
fn bm25_name_weight_ranks_name_col_hit_first() {
    // ---- Setup: init the ranking fixture ----
    let tmp = fixture_copy();
    let root = tmp.path();
    let init = run_in(root, &["init"]);
    assert!(
        init.status.success(),
        "init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    // ---- Scenario 1: envelope shape ----
    let out = run_in(root, &["query", "--symbol", "validate"]);
    assert!(
        out.status.success(),
        "symbol query must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);

    // Protocol version via shared const — must not hard-code divergently.
    // Current value per PROTOCOL_VERSION in src/envelope.rs: "1.5.0".
    assert_eq!(env["protocol_version"], "1.5.0");
    assert_eq!(env["query_type"], "symbol");
    assert_eq!(env["query"], "validate");

    // ---- Scenario 2: BM25 name-weight ordering (the crux assertion) ----
    //
    // After bm25(symbols, 10.0, 1.0): name-col weight = 10×, tokens-col = 1×.
    // validate_all_incoming_user_supplied_request wins (name-col hit, boosted).
    // reValidate comes second (tokens-col only, weight 1×).
    //
    // Under HEAD equal weights: order is REVERSED (tokens-col wins) → this
    // assertion FAILS on HEAD, proving the fix is load-bearing.
    //
    // Tiebreak proof: in_degree=0 for both (equal, inert).
    // name-ASC alone gives ["reValidate", "validate_all…"] — OPPOSITE of expected.
    // So the expected order can ONLY come from bm25 weighting.
    let hits = env["hits"].as_array().expect("hits is an array");
    assert!(
        hits.len() >= 2,
        "expected at least 2 hits for 'validate' (got {}); hits: {hits:?}",
        hits.len()
    );

    let names: Vec<&str> = hits
        .iter()
        .map(|h| h["name"].as_str().expect("hit has name field"))
        .collect();

    assert_eq!(
        names[0], "validate_all_incoming_user_supplied_request",
        "CRUX: name-column hit must rank FIRST after bm25(10,1) weighting. \
         If this fails on HEAD (equal weights), the tokens-only hit (reValidate) \
         incorrectly outranks the name hit. \
         Actual order: {names:?}"
    );
    assert_eq!(
        names[1], "reValidate",
        "tokens-only hit must rank SECOND after bm25(10,1) weighting. \
         Actual order: {names:?}"
    );

    // ---- Scenario 3: both hits are present with correct shape ----
    // Note: `in_degree` is used internally for ordering but is NOT serialised
    // in the symbol envelope (see src/envelope.rs::symbol). Verify the other
    // required fields are present on both discriminating hits.
    let validate_all_hit = hits
        .iter()
        .find(|h| h["name"] == "validate_all_incoming_user_supplied_request")
        .expect("validate_all hit must be present");
    let revalidate_hit = hits
        .iter()
        .find(|h| h["name"] == "reValidate")
        .expect("reValidate hit must be present");
    assert_eq!(
        validate_all_hit["kind"], "function",
        "validate_all… must be a function"
    );
    assert_eq!(
        revalidate_hit["kind"], "function",
        "reValidate must be a function"
    );

    // ---- Scenario 4: determinism — same query yields byte-identical output ----
    let again = run_in(root, &["query", "--symbol", "validate"]);
    assert_eq!(
        again.stdout, out.stdout,
        "identical symbol query must be byte-identical across runs (determinism)"
    );
}
