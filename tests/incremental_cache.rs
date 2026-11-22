// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for slice `incremental-graph-cache` (slice 10).
//!
//! The SparrowDB graph is rebuilt on refresh because its checkpointed
//! adjacency index can prevent in-place node deletion. Each file's extracted `FileFacts` are
//! cached in `meta.db` keyed by content hash, so a re-`init` runs tree-sitter
//! only over changed/new files and assembles the resolved graph from the
//! cached union.
//!
//! Observable proof: an additive `files_parsed` counter on the init envelope
//! (protocol stays "1.5.0"). It reports how many files were actually parsed
//! this run. Correctness is proven by reusing the frozen incremental contract's
//! assertions (auth.rs edges update; db.rs impact byte-identical) — the cache
//! path must produce the identical resolved graph as a full parse.
//!
//! Scenarios:
//!   1. Baseline `init` on rust_basic: files_parsed == files_indexed == 4
//!      (nothing cached yet — every file parsed).
//!   2. No-op re-`init` (no edits): files_parsed == 0, files_unchanged == 4
//!      (every file served from the fragment cache; zero tree-sitter work).
//!      This is the assertion the pre-cache implementation fails: it always
//!      re-parses all four.
//!   3. Edit metrics.rs, re-`init`: files_parsed == 1, files_reindexed == 1,
//!      files_unchanged == 3, invariant reindexed + unchanged == indexed.
//!   4. Correctness preserved across the cache path: auth.rs impact now lists
//!      metrics.rs as caller+dependent; db.rs impact stdout byte-identical to
//!      the baseline (unchanged file, deterministic envelope).

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

const EDITED_METRICS_RS: &str = "\
use crate::auth;

pub fn report() -> u32 {
    if auth::validate_token(\"abcdefghi\") { 1 } else { 0 }
}
";

#[test]
fn incremental_cache_skips_unchanged_parses() {
    let tmp = fixture_copy();
    let root = tmp.path();

    // ---- Scenario 1: baseline — everything parsed ----
    let init1 = run_in(root, &["init"]);
    assert!(
        init1.status.success(),
        "baseline init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init1.stderr)
    );
    let env1 = parse_single_json(&init1);
    assert_eq!(env1["protocol_version"], "1.5.0");
    assert_eq!(env1["files_indexed"], 4);
    assert_eq!(
        env1["files_parsed"], 4,
        "first init must parse every file (cache empty)"
    );

    // Baseline db.rs impact bytes (unchanged file, deterministic envelope).
    let db_before = run_in(root, &["query", "--impact", "db.rs"]);
    assert!(db_before.status.success());
    let baseline_db: Vec<u8> = db_before.stdout.clone();

    // ---- Scenario 2: no-op re-init — zero parses, all served from cache ----
    let init2 = run_in(root, &["init"]);
    assert!(
        init2.status.success(),
        "no-op re-init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init2.stderr)
    );
    let env2 = parse_single_json(&init2);
    assert_eq!(env2["files_indexed"], 4);
    assert_eq!(env2["files_unchanged"], 4);
    assert_eq!(
        env2["files_parsed"], 0,
        "no-op re-init must parse nothing — every file's facts come from the cache"
    );

    // ---- Scenario 3: edit one file — only that file is parsed ----
    fs::write(root.join("metrics.rs"), EDITED_METRICS_RS).expect("overwrite metrics.rs");
    let init3 = run_in(root, &["init"]);
    assert!(
        init3.status.success(),
        "re-init after edit exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init3.stderr)
    );
    let env3 = parse_single_json(&init3);
    assert_eq!(env3["files_indexed"], 4);
    assert_eq!(env3["files_reindexed"], 1);
    assert_eq!(env3["files_unchanged"], 3);
    assert_eq!(
        env3["files_parsed"], 1,
        "only the edited file should be re-parsed"
    );
    assert_eq!(
        env3["files_reindexed"].as_u64().unwrap() + env3["files_unchanged"].as_u64().unwrap(),
        env3["files_indexed"].as_u64().unwrap()
    );

    // ---- Scenario 4: correctness across the cache path ----
    let auth_after = run_in(root, &["query", "--impact", "auth.rs"]);
    assert!(auth_after.status.success());
    let auth_env = parse_single_json(&auth_after);
    assert_eq!(
        auth_env["callers"],
        serde_json::json!(["main.rs", "metrics.rs"])
    );
    assert_eq!(
        auth_env["dependents"],
        serde_json::json!(["main.rs", "metrics.rs"])
    );
    assert_eq!(auth_env["blast_radius"], 2);

    let db_after = run_in(root, &["query", "--impact", "db.rs"]);
    assert!(db_after.status.success());
    assert_eq!(
        db_after.stdout, baseline_db,
        "db.rs impact stdout must be byte-identical across the cache-backed refresh"
    );
}
