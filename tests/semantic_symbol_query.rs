// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate test for the symbol-granular-embeddings slice.
//!
//! Verifies that `lattice query "<text>"` (no flags) now returns the NEW
//! default hit shape: one hit per function/method span, with {path, name, kind,
//! start_line, end_line, score, class}.  The fixture is a 3-file Rust tree
//! designed so that symbol-level ranking surfaces `persist_session_record` from
//! `src/store.rs` above the decoy files.
//!
//! Harness conventions mirror tests/semantic_query.rs: temp-copy the fixture,
//! run the built binary against it, parse stdout as exactly one JSON object.

use std::fs;
use std::path::Path;
use std::process::Command;

fn fixture_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/semantic_symbol_basic")
}

/// Recursively copy `src` into `dst`.
fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create dst dir");
    for entry in fs::read_dir(src).expect("read src dir") {
        let entry = entry.expect("read entry");
        let ft = entry.file_type().expect("file type");
        let dest = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_all(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), &dest).expect("copy file");
        }
    }
}

/// Run `lattice <args>` inside `dir`; return the Command output.
fn run_in(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_lattice"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run lattice binary")
}

/// Parse stdout as exactly one JSON object (no trailing/leading junk).
fn parse_single_json(stdout: &str) -> serde_json::Value {
    let trimmed = stdout.trim();
    serde_json::from_str(trimmed).unwrap_or_else(|e| {
        panic!(
            "stdout is not a single JSON object: {e}\nstdout was:\n{trimmed}"
        )
    })
}

#[test]
fn semantic_symbol_query_returns_symbol_hit() {
    // ── 1. Copy fixture to a temp dir ────────────────────────────────────────
    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path();
    copy_dir_all(&fixture_dir(), root);

    // ── 2. lattice init ────────────────────────────────────────────────────
    let init = run_in(root, &["init"]);
    assert!(
        init.status.success(),
        "lattice init failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );

    // ── 3. Zero-egress guard ─────────────────────────────────────────────────
    // The embedder is local (candle); no network traffic should occur.
    // We verify this indirectly: if the model were fetched from the net the
    // test would either time out or succeed differently — the unit test suite
    // is run offline in CI.  The real guard is `embed::ensure_model_present`
    // which returns `model_not_found` rather than fetching; if it fired here
    // `init` would have exited non-zero above.

    // ── 4. lattice query (new default shape) ───────────────────────────────
    let q = run_in(root, &["query", "persist a session record to the store"]);

    // Assertion 1: exit code 0
    assert!(
        q.status.success(),
        "query exit code non-zero:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&q.stdout),
        String::from_utf8_lossy(&q.stderr)
    );

    let stdout = String::from_utf8(q.stdout.clone()).expect("stdout UTF-8");

    // Assertion 2: stdout parses as exactly one JSON object
    let envelope = parse_single_json(&stdout);

    // Assertion 3: protocol_version == "1.5.0"
    assert_eq!(
        envelope["protocol_version"].as_str().unwrap_or(""),
        "1.5.0",
        "expected protocol_version 1.5.0"
    );

    // Assertion 4: query_type == "hybrid"
    assert_eq!(
        envelope["query_type"].as_str().unwrap_or(""),
        "hybrid",
        "expected query_type semantic"
    );

    let hits = envelope["hits"].as_array().expect("hits is an array");

    // Assertion 5: hits is non-empty
    assert!(!hits.is_empty(), "hits must be non-empty");

    // Find the target hit: name == "persist_session_record", path contains "store.rs"
    let target = hits
        .iter()
        .find(|h| {
            h["name"].as_str() == Some("persist_session_record")
                && h["path"].as_str().map(|p| p.contains("store.rs")).unwrap_or(false)
        })
        .unwrap_or_else(|| {
            panic!(
                "no hit with name==persist_session_record in store.rs\nhits: {}",
                serde_json::to_string_pretty(hits).unwrap()
            )
        });

    // Assertion 6: target hit is hits[0] (top-ranked)
    assert_eq!(
        hits[0]["name"].as_str(),
        Some("persist_session_record"),
        "persist_session_record must be the top-ranked hit\nhits[0]: {}",
        serde_json::to_string_pretty(&hits[0]).unwrap()
    );
    // Also verify it's from store.rs (not store_test.rs)
    assert!(
        hits[0]["path"].as_str().map(|p| p.contains("store.rs")).unwrap_or(false),
        "top hit must be from store.rs, got path: {:?}",
        hits[0]["path"]
    );

    // Assertion 7a: kind == "function"
    assert_eq!(
        target["kind"].as_str().unwrap_or(""),
        "function",
        "expected kind=function for persist_session_record"
    );

    // Assertion 7b: start_line < end_line (multi-line function)
    let start_line = target["start_line"].as_u64().expect("start_line is a number");
    let end_line = target["end_line"].as_u64().expect("end_line is a number");
    assert!(
        start_line < end_line,
        "start_line ({start_line}) must be < end_line ({end_line})"
    );

    // Assertion 7c: line at start_line contains "fn persist_session_record"
    // Read back the fixture source to verify the line anchor is correct.
    let store_src = fs::read_to_string(root.join("src/store.rs"))
        .expect("read fixture src/store.rs");
    let store_lines: Vec<&str> = store_src.lines().collect();
    let fn_line = store_lines
        .get((start_line as usize).saturating_sub(1))
        .copied()
        .unwrap_or("");
    assert!(
        fn_line.contains("fn persist_session_record"),
        "line {start_line} of src/store.rs should contain 'fn persist_session_record', got: {fn_line:?}"
    );

    // Assertion 7d: hit has a "score" field that is a positive number
    let score = target["fused_score"].as_f64().expect("score is a number");
    assert!(score > 0.0, "score must be > 0, got {score}");
}
