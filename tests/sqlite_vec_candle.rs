// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for slice `sqlite-vec-plus-candle` (slice 9).
//!
//! Scenarios:
//!   1. `lattice init` builds the sqlite-vec store with candle embeddings
//!      (vec0 float[384] virtual table; 4 documents).
//!   2. `lattice query "validate token signature"` ranks the auth module
//!      first, with fused scores in descending order.
//!   3. Model absent (LATTICE_MODEL_DIR -> empty dir) yields the
//!      `model_not_found` error envelope, non-zero exit, zero egress.
//!
//! The tests exercise the compiled binary via env!("CARGO_BIN_EXE_lattice")
//! — the only entry points to vectors::refresh / vectors::query in this
//! bin-only crate are `lattice init` / `lattice query`, which is exactly
//! how the trait boundary (O2) is proven without touching the embedder.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use rusqlite::Connection;
use serde_json::Value;
#[path = "support/install.rs"]
mod install_support;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");
const SEEDED_QUERY: &str = "validate token signature";

/// Copy the rust_basic fixture (auth.rs, db.rs, main.rs, metrics.rs) into a
/// fresh temp dir with no .lattice/ and no .gitignore.
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

fn run_in_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.args(args).current_dir(dir);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn lattice binary")
}

/// Stream discipline: stdout must contain nothing besides a single JSON object.
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

/// Verify that `install` uses an existing local model cache.
fn ensure_model(dir: &Path) {
    let home = tempfile::tempdir().unwrap();
    let out = install_support::isolated_install(dir, home.path()).arg("install").output().unwrap();
    assert!(
        out.status.success(),
        "lattice install failed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);
    assert_eq!(env["protocol_version"], "1.5.0");
    assert_eq!(env["command"], "install");
    assert_eq!(env["status"], "ok");
    assert_eq!(env["model"], "all-MiniLM-L6-v2");
    assert_eq!(env["files"], 3);
    assert_eq!(env["cached"], true);
}

#[test]
fn sqlite_vec_candle_end_to_end() {
    let tmp = fixture_copy();
    let root = tmp.path();
    assert!(!root.join(".lattice").exists());

    // Shared setup: model weights must be present for init/query to embed.
    ensure_model(root);

    // ---- Scenario 1: init builds the sqlite-vec store with candle embeddings ----
    let init_out = run_in(root, &["init"]);
    assert!(
        init_out.status.success(),
        "lattice init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    let init_env = parse_single_json(&init_out);
    assert_eq!(init_env["protocol_version"], "1.5.0");
    assert_eq!(init_env["command"], "init");
    assert_eq!(init_env["status"], "ok");
    assert_eq!(init_env["files_indexed"], 4);

    let vec_db = root.join(".lattice/vectors.db");
    assert!(vec_db.is_file(), ".lattice/vectors.db must exist after init");

    // Open the store read-only and assert the sqlite-vec virtual table replaced
    // the BLOB schema (O2): the DDL contains both `vec0` and `float[384]`.
    {
        let conn = Connection::open_with_flags(
            &vec_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open vectors.db read-only");
        let ddl: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'vec_documents'",
                [],
                |row| row.get(0),
            )
            .expect("vec_documents table must exist");
        assert!(
            ddl.contains("vec0"),
            "vec_documents DDL must contain vec0: {ddl}"
        );
        assert!(
            ddl.contains("float[384]"),
            "vec_documents DDL must contain float[384]: {ddl}"
        );

        let doc_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
            .expect("documents table must exist");
        // Schema v3: one row per function/method span (no whole-file fallback,
        // since every fixture file has functions). rust_basic ground truth:
        // auth.rs=2 (validate_token, check_signature), db.rs=1 (save_session),
        // main.rs=1 (main), metrics.rs=1 (report) = 5 spans exactly.
        assert_eq!(
            doc_count, 5,
            "documents must hold exactly one row per function span (5), got {doc_count}"
        );
    }

    // ---- Scenario 2: semantic query ranks the auth module first ----
    let q = run_in(root, &["query", SEEDED_QUERY]);
    assert!(
        q.status.success(),
        "lattice query exited non-zero; stderr: {}",
        String::from_utf8_lossy(&q.stderr)
    );
    let env = parse_single_json(&q);
    assert_eq!(env["protocol_version"], "1.5.0");
    assert_eq!(env["query_type"], "hybrid");
    assert_eq!(env["query"], SEEDED_QUERY);

    let hits = env["hits"].as_array().expect("hits is an array");
    assert!(!hits.is_empty(), "hits must be a non-empty array");

    // Frozen assertion: hits[0].path contains "auth".
    let top_path = hits[0]["path"].as_str().expect("hit.path is a string");
    assert!(
        top_path.contains("auth"),
        "top hit path must contain \"auth\": {top_path}"
    );

    // Scores descending; asserted by rank only, never by value.
    let mut prev: Option<f64> = None;
    for hit in hits {
        assert!(hit["path"].is_string(), "hit.path must be a string: {hit}");
        let score = hit["fused_score"].as_f64().expect("hit.score must be a number");
        if let Some(p) = prev {
            assert!(
                score <= p,
                "hits must be ordered by score descending: {score} !<= {p}"
            );
        }
        prev = Some(score);
    }

    // ---- Scenario 3: model absent yields model_not_found, zero egress ----
    let empty = tempfile::tempdir().expect("create empty model dir");
    let empty_dir = empty.path().to_string_lossy().into_owned();
    let bad = run_in_env(
        root,
        &["query", "some text"],
        &[("LATTICE_MODEL_DIR", empty_dir.as_str())],
    );
    assert!(
        !bad.status.success(),
        "query with an empty model dir must exit non-zero"
    );
    let bad_env = parse_single_json(&bad);
    assert_eq!(bad_env["protocol_version"], "1.5.0");
    assert_eq!(bad_env["query_type"], "error");
    assert_eq!(
        bad_env["error"]["code"], "model_not_found",
        "model-absent error code must be model_not_found"
    );
}
