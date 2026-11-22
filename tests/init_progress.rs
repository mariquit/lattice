// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate: `lattice init` progress footer.
//!
//! A long cold index (embedding is seconds-per-file) previously looked hung: the
//! only signal was an `embedded N/M` line every ten files, and nothing at all
//! during discovery/parse/graph/FTS. This adds a single in-place stderr footer
//! naming the current stage and file.
//!
//! The risky part is not the spinner, it is not breaking three contracts:
//!
//!   1. stdout stays exactly one JSON envelope for CLI consumers. Progress
//!      is prose and goes to stderr only.
//!   2. Non-tty stderr (pipes, CI, `$(...)`) must be byte-identical to the
//!      no-progress behavior: no spinner frames, no ANSI escapes.
//!   3. Query-time freshness self-heal re-runs the SAME `index::run` pipeline.
//!      It must stay silent — a spinner must never appear during `query`.
//!
//! `LATTICE_PROGRESS=always|never` overrides the tty sniff so scenario 4 can
//! assert real frames without allocating a pty.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

fn run_in(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.args(args).current_dir(dir);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn lattice binary")
}

fn write(root: &Path, rel: &str, body: &str) {
    let abs = root.join(rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).expect("create parent dir");
    }
    std::fs::write(abs, body).expect("write fixture file");
}

/// A few files so multi-file stages actually have counters to report.
fn fixture(root: &Path) {
    write(root, "src/alpha.rs", "pub fn alpha() -> u8 { 1 }\n");
    write(root, "src/beta.rs", "pub fn beta() -> u8 { 2 }\n");
    write(
        root,
        "src/gamma.rs",
        "pub fn gamma() -> u8 { super::beta::beta() }\n",
    );
}

/// ANSI escape / spinner-frame detection. The footer cannot exist without at
/// least one of these; plain `eprintln!` lines contain none.
fn has_terminal_control(s: &str) -> bool {
    s.contains('\r') || s.contains("\x1b[") || s.chars().any(|c| "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(c))
}

/// Scenario 1+2: with stderr captured (a pipe, i.e. NOT a tty), `init` must
/// behave exactly as before — one JSON envelope on stdout, and stderr free of
/// any spinner frames or cursor-control escapes.
#[test]
fn non_tty_init_emits_no_terminal_control() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let root = tmp.path();
    fixture(root);

    let out = run_in(root, &["init"], &[]);
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // stdout: exactly one JSON envelope, unchanged.
    let stdout = String::from_utf8(out.stdout).expect("stdout UTF-8");
    let env: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not exactly one JSON object ({e}): {stdout:?}"));
    assert_eq!(env["status"], "ok");
    assert_eq!(env["files_indexed"], 3);

    // stderr: prose only, no footer machinery.
    let stderr = String::from_utf8(out.stderr).expect("stderr UTF-8");
    assert!(
        !has_terminal_control(&stderr),
        "non-tty stderr must contain no spinner/ANSI control: {stderr:?}"
    );
}

/// Scenario 3: the footer must never leak into a `query`. Queries re-run the
/// same index pipeline through the freshness self-heal, so a naive "always on"
/// implementation would spin during every query and corrupt tool output.
#[test]
fn query_never_shows_progress_even_when_forced() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let root = tmp.path();
    fixture(root);
    assert!(run_in(root, &["init"], &[]).status.success());

    // Touch a file so the query MUST take the stale path and re-run index::run.
    write(root, "src/alpha.rs", "pub fn alpha() -> u8 { 42 }\n");

    // Force progress on: it must still be absent, because only `cmd_init` opts
    // in — `index::run` itself is always silent.
    let out = run_in(
        root,
        &["query", "--symbol", "alpha"],
        &[("LATTICE_PROGRESS", "always")],
    );
    assert!(out.status.success(), "query failed");

    let stdout = String::from_utf8(out.stdout).expect("stdout UTF-8");
    let env: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("query stdout is not one JSON object ({e}): {stdout:?}"));
    assert_eq!(env["query_type"], "symbol");

    let stderr = String::from_utf8(out.stderr).expect("stderr UTF-8");
    assert!(
        !has_terminal_control(&stderr),
        "query stderr must never contain a progress footer: {stderr:?}"
    );
}

/// Scenario 4: when the footer IS enabled, it actually reports the stages and
/// the file being worked on — otherwise this whole feature is inert and the
/// other three tests would pass trivially.
#[test]
fn forced_progress_reports_stages_and_files() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let root = tmp.path();
    fixture(root);

    let out = run_in(root, &["init"], &[("LATTICE_PROGRESS", "always")]);
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // stdout is STILL exactly one envelope — the footer must not touch it.
    let stdout = String::from_utf8(out.stdout).expect("stdout UTF-8");
    let env: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not exactly one JSON object ({e}): {stdout:?}"));
    assert_eq!(env["status"], "ok");

    let stderr = String::from_utf8(out.stderr).expect("stderr UTF-8");

    // In-place redraw happened (this is what makes it a footer, not a log).
    assert!(
        has_terminal_control(&stderr),
        "forced progress must draw terminal control sequences: {stderr:?}"
    );

    // Each pipeline stage is named.
    for stage in ["scan", "parse", "graph", "embed"] {
        assert!(
            stderr.contains(stage),
            "footer must report the '{stage}' stage; got: {stderr:?}"
        );
    }

    // A concrete file is named, with a counter, so the user can see movement.
    assert!(
        stderr.contains("alpha.rs") || stderr.contains("beta.rs") || stderr.contains("gamma.rs"),
        "footer must name the file being processed; got: {stderr:?}"
    );
    assert!(
        stderr.contains("/3"),
        "footer must show an n/total counter for the 3 fixture files; got: {stderr:?}"
    );

    // The footer is erased at the end: the last non-empty stderr line is the
    // human summary, not a half-drawn spinner frame.
    let last = stderr
        .replace('\r', "\n")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .next_back()
        .unwrap_or_default();
    assert!(
        last.contains("indexed"),
        "footer must be cleared so the summary is last; got last line: {last:?}"
    );
}

/// `LATTICE_PROGRESS=never` forces the footer off even on a tty. Asserted here
/// on a non-tty (where it is off anyway) purely to prove the flag parses and
/// does not accidentally *enable* anything.
#[test]
fn progress_never_is_silent() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let root = tmp.path();
    fixture(root);

    let out = run_in(root, &["init"], &[("LATTICE_PROGRESS", "never")]);
    assert!(out.status.success(), "init failed");
    let stderr = String::from_utf8(out.stderr).expect("stderr UTF-8");
    assert!(
        !has_terminal_control(&stderr),
        "LATTICE_PROGRESS=never must be silent: {stderr:?}"
    );
}
