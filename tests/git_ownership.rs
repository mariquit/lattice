// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate: git-ownership-layer slice.
//!
//! Constructs a deterministic two-commit git fixture, runs `lattice init` and
//! `lattice query --owners`, and verifies the exact ownership envelope shape.
//!
//! All assertions are exact (no `>=` to dodge bugs). Test-first: this file is
//! written before the implementation; it must compile and FAIL before any
//! `src/ownership.rs` exists, then go GREEN once the slice is complete.

use serde_json::Value;
use std::process::Command;

// Path to the lattice binary built by cargo.
const BIN: &str = env!("CARGO_BIN_EXE_lattice");

/// Build the two-commit fixture in `dir` with deterministic authorship + dates.
///
/// C1: Alice creates `src/auth.rs` = 30 lines.
/// C2: Bob appends 10 lines to `src/auth.rs` AND creates `src/token.rs` = 5 lines.
///
/// Isolated from user git config via `GIT_CONFIG_NOSYSTEM=1` and
/// `GIT_CONFIG_GLOBAL=/dev/null`. Signing disabled via `-c commit.gpgsign=false`.
fn build_git_fixture(dir: &std::path::Path) {
    // Create src/ subdirectory.
    std::fs::create_dir_all(dir.join("src")).unwrap();

    // --- Commit C1: Alice adds src/auth.rs (30 lines) ---
    let auth_rs = dir.join("src/auth.rs");
    let alice_content: String = (1..=30).map(|i| format!("fn a{i}() {{}}\n")).collect();
    std::fs::write(&auth_rs, &alice_content).unwrap();

    let token_rs = dir.join("src/token.rs");
    // Create a minimal placeholder so the directory listing exists.
    // (token.rs is created in C2; don't create it in C1)

    // git init
    git(dir, &["init", "-q", "--initial-branch=main"]);

    // C1: Alice commits auth.rs
    git_env(
        dir,
        &["add", "src/auth.rs"],
        &[],
    );
    git_env(
        dir,
        &["commit", "-q", "-m", "C1: add auth.rs"],
        &[
            ("GIT_AUTHOR_NAME", "Alice"),
            ("GIT_AUTHOR_EMAIL", "alice@example.com"),
            ("GIT_AUTHOR_DATE", "2024-01-01T00:00:00Z"),
            ("GIT_COMMITTER_NAME", "Alice"),
            ("GIT_COMMITTER_EMAIL", "alice@example.com"),
            ("GIT_COMMITTER_DATE", "2024-01-01T00:00:00Z"),
            // Disable GPG signing to avoid prompts or failures in CI.
            ("GIT_CONFIG_PARAMETERS", "'commit.gpgsign=false'"),
        ],
    );

    // --- Commit C2: Bob appends 10 lines to auth.rs AND creates token.rs (5 lines) ---
    let bob_extra: String = (1..=10).map(|i| format!("fn b{i}() {{}}\n")).collect();
    let mut full_auth = alice_content.clone();
    full_auth.push_str(&bob_extra);
    std::fs::write(&auth_rs, &full_auth).unwrap();

    let token_content: String = (1..=5).map(|i| format!("fn t{i}() {{}}\n")).collect();
    std::fs::write(&token_rs, token_content).unwrap();

    git_env(dir, &["add", "src/auth.rs", "src/token.rs"], &[]);
    git_env(
        dir,
        &["commit", "-q", "-m", "C2: Bob extends auth.rs and adds token.rs"],
        &[
            ("GIT_AUTHOR_NAME", "Bob"),
            ("GIT_AUTHOR_EMAIL", "bob@example.com"),
            ("GIT_AUTHOR_DATE", "2024-01-02T00:00:00Z"),
            ("GIT_COMMITTER_NAME", "Bob"),
            ("GIT_COMMITTER_EMAIL", "bob@example.com"),
            ("GIT_COMMITTER_DATE", "2024-01-02T00:00:00Z"),
            ("GIT_CONFIG_PARAMETERS", "'commit.gpgsign=false'"),
        ],
    );
}

/// Run a git command without extra env; panics on non-zero exit.
fn git(dir: &std::path::Path, args: &[&str]) {
    git_env(dir, args, &[]);
}

/// Run a git command with extra env vars; panics on non-zero exit.
fn git_env(dir: &std::path::Path, args: &[&str], env: &[(&str, &str)]) {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let status = cmd.output().unwrap();
    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        let stdout = String::from_utf8_lossy(&status.stdout);
        panic!(
            "git {:?} failed:\nstdout: {stdout}\nstderr: {stderr}",
            args
        );
    }
}

/// Run `lattice init` in `dir`; assert exit 0. Return parsed stdout envelope.
fn run_init(dir: &std::path::Path) -> Value {
    let out = Command::new(BIN)
        .arg("init")
        .current_dir(dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "lattice init failed (exit {:?}):\nstdout: {stdout}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(&stdout).expect("init stdout is not valid JSON")
}

/// Run `lattice query --owners <path>` in `dir`; return (exit_code, parsed envelope).
fn run_owners(dir: &std::path::Path, path: &str) -> (i32, Value) {
    let out = Command::new(BIN)
        .args(["query", "--owners", path])
        .current_dir(dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let code = out.status.code().unwrap_or(-1);
    let val: Value = serde_json::from_str(&stdout).unwrap_or_else(|_| {
        panic!(
            "lattice query --owners {path} stdout is not valid JSON:\n{stdout}"
        )
    });
    (code, val)
}

// ─── Test 1: two-author split ────────────────────────────────────────────────

#[test]
fn owners_two_author_split() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();

    build_git_fixture(dir);

    // lattice init must succeed
    let init_val = run_init(dir);
    assert_eq!(init_val["status"], "ok", "init envelope status != ok");

    // Query owners of src/auth.rs
    let (code, val) = run_owners(dir, "src/auth.rs");
    assert_eq!(code, 0, "lattice query --owners exited non-zero:\n{val}");

    // Protocol version stays 1.1.0 (MV5 — no bump).
    assert_eq!(
        val["protocol_version"], "1.5.0",
        "protocol_version mismatch"
    );
    assert_eq!(val["query_type"], "owners", "query_type mismatch");
    assert_eq!(val["path"], "src/auth.rs", "path field mismatch");

    // churn = 2 commits touched src/auth.rs
    assert_eq!(val["churn"].as_u64().unwrap(), 2, "churn != 2");

    // co_changed = ["src/token.rs"]
    let co = val["co_changed"].as_array().unwrap();
    assert_eq!(co.len(), 1, "co_changed.len() != 1");
    assert_eq!(co[0], "src/token.rs", "co_changed[0] != src/token.rs");

    // owners: 2 entries, sorted by share desc
    let owners = val["owners"].as_array().unwrap();
    assert_eq!(owners.len(), 2, "owners.len() != 2");

    // owners[0] = Alice, share 0.75
    let alice = &owners[0];
    assert_eq!(alice["author"], "Alice <alice@example.com>", "owners[0].author");
    let alice_share = alice["share"].as_f64().unwrap();
    assert!(
        (alice_share - 0.75).abs() < 1e-9,
        "Alice share = {alice_share}, expected 0.75"
    );
    assert_eq!(
        alice["last_touched"], "2024-01-01T00:00:00+00:00",
        "Alice last_touched"
    );

    // owners[1] = Bob, share 0.25
    let bob = &owners[1];
    assert_eq!(bob["author"], "Bob <bob@example.com>", "owners[1].author");
    let bob_share = bob["share"].as_f64().unwrap();
    assert!(
        (bob_share - 0.25).abs() < 1e-9,
        "Bob share = {bob_share}, expected 0.25"
    );
    assert_eq!(
        bob["last_touched"], "2024-01-02T00:00:00+00:00",
        "Bob last_touched"
    );
}

// ─── Test 2: non-git tree returns empty, not error ───────────────────────────

#[test]
fn owners_non_git_tree_empty() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();

    // Create a minimal source file but NO git init.
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn hello() {}\n").unwrap();

    // lattice init must succeed (git pass skips cleanly when no .git).
    let init_val = run_init(dir);
    assert_eq!(init_val["status"], "ok");

    // --owners must exit 0 with an empty-data owners envelope (not an error).
    let (code, val) = run_owners(dir, "src/lib.rs");
    assert_eq!(code, 0, "non-git --owners exited non-zero:\n{val}");

    assert_eq!(val["protocol_version"], "1.5.0");
    assert_eq!(val["query_type"], "owners");
    assert_eq!(
        val["owners"].as_array().unwrap().len(),
        0,
        "expected empty owners"
    );
    assert_eq!(val["churn"].as_u64().unwrap(), 0);
    assert_eq!(
        val["co_changed"].as_array().unwrap().len(),
        0,
        "expected empty co_changed"
    );
}

// ─── Test 3: unknown path returns empty, not error ───────────────────────────

#[test]
fn owners_unknown_path_empty() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();

    build_git_fixture(dir);
    run_init(dir); // establish the store

    // Query a path that was never committed.
    let (code, val) = run_owners(dir, "src/nope.rs");
    assert_eq!(code, 0, "unknown-path --owners exited non-zero:\n{val}");
    assert_eq!(val["query_type"], "owners");
    assert_eq!(val["owners"].as_array().unwrap().len(), 0);
    assert_eq!(val["churn"].as_u64().unwrap(), 0);
    assert_eq!(val["co_changed"].as_array().unwrap().len(), 0);
}
