// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for slice S3 "prompt-cache-aware context split".
//!
//! `lattice context --stable` emits the stable, cacheable repo overview:
//! mental model + module map + top hotspots + ownership overview.
//!
//! Written test-first; all scenarios must FAIL until the implementation is
//! shipped.
//!
//! Fixture: `tests/fixtures/rust_basic/` — 4 source files, hand-countable.
//!
//! Ground truth (hand-counted from fixture):
//!   files     = 4   (auth.rs, db.rs, main.rs, metrics.rs)
//!   symbols   = 5   (authenticate, connect, main, record_request, handle → but
//!                    tree-sitter extracts: auth.rs has `authenticate`+`validate`,
//!                    db.rs has `connect`, main.rs has `main`, metrics.rs has
//!                    `record_request` = 5 total)
//!
//! One command proves the slice: `cargo test --test context_overview`

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
#[path = "support/install.rs"]
mod install_support;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");
const FIXTURE_DIR: &str = "tests/fixtures/rust_basic";

/// Run `lattice <args>` with cwd = `dir`.
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
    serde_json::from_str::<Value>(trimmed)
        .unwrap_or_else(|e| panic!("stdout is not one JSON object ({e}): {stdout:?}"))
}

/// Copy the fixture directory into a fresh temp dir and return (temp, fixture_root).
fn setup_fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("temp dir");
    let fixture_root = tmp.path().join("repo");
    let src_fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR);
    copy_dir_all(&src_fixture, &fixture_root).expect("copy fixture");
    (tmp, fixture_root)
}

/// Recursively copy a directory tree.
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst_path)?;
        } else {
            fs::copy(entry.path(), dst_path)?;
        }
    }
    Ok(())
}

// ── Scenario 1 — no store → no_lattice error envelope ──────────────────────

#[test]
fn scenario_1_no_store_returns_no_lattice_error() {
    let (_tmp, repo) = setup_fixture();

    let out = run_in(&repo, &["context"]);
    assert!(
        !out.status.success(),
        "context --stable must exit non-zero when .lattice/ is absent; \
         exit: {}; stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let env = parse_single_json(&out);
    assert_eq!(
        env["protocol_version"], "1.5.0",
        "protocol_version must be the PROTOCOL_VERSION const value"
    );
    assert_eq!(
        env["error"]["code"], "no_lattice",
        "error code must be no_lattice; got: {env}"
    );
}

// ── Scenario 2 — after init → correct envelope shape and hand-counted values ──

#[test]
fn scenario_2_after_init_correct_envelope() {
    let (_tmp, repo) = setup_fixture();

    // Run init first.
    let init_out = run_in(&repo, &["init"]);
    assert!(
        init_out.status.success(),
        "init failed; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );

    let out = run_in(&repo, &["context"]);
    assert!(
        out.status.success(),
        "context --stable must succeed after init; exit: {}; stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let env = parse_single_json(&out);

    // Protocol discipline.
    assert_eq!(env["protocol_version"], "1.5.0");
    assert_eq!(env["query_type"], "context_overview");
    assert_eq!(env["freshness"], "as_indexed");

    // Hand-counted fixture values.
    let files = env["repo"]["files"].as_u64().expect("repo.files is a number");
    assert_eq!(files, 4, "fixture has exactly 4 source files");

    let symbols = env["repo"]["symbols"].as_u64().expect("repo.symbols is a number");
    // 5 functions: authenticate, validate (auth.rs), connect (db.rs),
    // main (main.rs), record_request (metrics.rs).
    assert_eq!(symbols, 5, "fixture has exactly 5 symbols");

    // modules must be a non-empty array sorted ascending by path.
    let modules = env["modules"].as_array().expect("modules is an array");
    assert!(!modules.is_empty(), "modules must be non-empty after init");
    let paths: Vec<&str> = modules
        .iter()
        .map(|m| m["path"].as_str().expect("module.path is a string"))
        .collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "modules must be sorted ascending by path");

    // hotspots must be a non-empty array.
    let hotspots = env["hotspots"].as_array().expect("hotspots is an array");
    assert!(!hotspots.is_empty(), "hotspots must be non-empty after init");

    // hotspots must NOT contain raw f64 score — only rank (integer index) or omitted.
    for h in hotspots {
        assert!(
            h.get("score").is_none(),
            "stable hotspots must NOT emit raw f64 score (C1); got: {h}"
        );
    }

    // owners may be empty (no git history in fixture) — that is fine.
    let _owners = env["owners"].as_array().expect("owners is an array (possibly empty)");
}

// ── Scenario 3 — byte-identity (the cacheability property, C1) ───────────────

#[test]
fn scenario_3_byte_identical_across_two_runs() {
    let (_tmp, repo) = setup_fixture();

    let init_out = run_in(&repo, &["init"]);
    assert!(init_out.status.success(), "init failed");

    let out1 = run_in(&repo, &["context"]);
    assert!(out1.status.success(), "first context --stable failed");

    let out2 = run_in(&repo, &["context"]);
    assert!(out2.status.success(), "second context --stable failed");

    assert_eq!(
        out1.stdout, out2.stdout,
        "context --stable must be byte-identical across two independent runs \
         (C1: byte-determinism is the whole point of the stable channel)"
    );
}

// ── Scenario 4 — init-only refresh (no freshness self-heal) ──────────────────

#[test]
fn scenario_4_init_only_refresh() {
    let (_tmp, repo) = setup_fixture();

    // Initial init.
    let init_out = run_in(&repo, &["init"]);
    assert!(init_out.status.success(), "init failed");

    let baseline = run_in(&repo, &["context"]);
    assert!(baseline.status.success(), "baseline context --stable failed");
    let baseline_bytes = baseline.stdout.clone();

    // Append a new function to a fixture file WITHOUT re-running init.
    let metrics_file = repo.join("metrics.rs");
    let existing = fs::read_to_string(&metrics_file).unwrap();
    let patched = format!("{existing}\npub fn new_function_for_s3_test() {{}}\n");
    fs::write(&metrics_file, &patched).unwrap();

    // context --stable must still return bytes identical to baseline (no freshness).
    let after_edit = run_in(&repo, &["context"]);
    assert!(after_edit.status.success(), "context --stable after edit failed");
    assert_eq!(
        after_edit.stdout, baseline_bytes,
        "context --stable must NOT reflect source edits until after re-init \
         (init-only refresh — no freshness self-heal)"
    );

    // After re-init the symbol count must increase by exactly 1.
    let reinit_out = run_in(&repo, &["init"]);
    assert!(reinit_out.status.success(), "re-init failed");

    let after_init = run_in(&repo, &["context"]);
    assert!(after_init.status.success(), "context --stable after re-init failed");

    let env_before = serde_json::from_slice::<Value>(&baseline_bytes).unwrap();
    let env_after = serde_json::from_slice::<Value>(&after_init.stdout).unwrap();

    let symbols_before = env_before["repo"]["symbols"].as_u64().unwrap();
    let symbols_after = env_after["repo"]["symbols"].as_u64().unwrap();
    assert_eq!(
        symbols_after,
        symbols_before + 1,
        "re-init must increase symbol count by exactly 1 (the added function)"
    );
}

#[test]
fn install_does_not_embed_repository_context() {
    let (_tmp, repo) = setup_fixture();
    let home = tempfile::tempdir().unwrap();
    assert!(run_in(&repo, &["init"]).status.success());
    assert!(install_support::isolated_install(&repo, home.path()).arg("install").output().unwrap().status.success());
    assert!(!repo.join("AGENTS.md").exists());
    assert!(!home.path().join(".config/opencode/AGENTS.md").exists());
    assert_eq!(fs::read_to_string(home.path().join(".config/opencode/skills/lattice/SKILL.md")).unwrap(),
        include_str!("../skills/lattice/SKILL.md"));
}
