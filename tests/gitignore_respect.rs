// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for BUG #1: `lattice init` must respect `.gitignore`.
//!
//! Discovery (shared by `init` and query-time refresh via
//! `index::collect_source_files`) previously skipped only dot-dirs and a
//! top-level `target/`. It now honors `.gitignore` (and `.ignore`) via the
//! pure-Rust `ignore` walker, so gitignored source files never enter any store.
//!
//! Scenarios:
//!   1. A gitignored file and a gitignored directory are excluded from the
//!      index (`files_indexed` counts only the tracked source file).
//!   2. A gitignored symbol is not findable via `query --symbol`, while a
//!      tracked symbol is — proof the exclusion reaches every store.
//!   3. Nested `.gitignore` deeper in the tree is honored too.

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
        .expect("spawn lattice binary")
}

fn parse_single_json(output: &Output) -> Value {
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8");
    let trimmed = stdout.trim();
    serde_json::from_str::<Value>(trimmed)
        .unwrap_or_else(|e| panic!("stdout is not exactly one JSON object ({e}): {stdout:?}"))
}

fn write(root: &Path, rel: &str, body: &str) {
    let abs = root.join(rel);
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(abs, body).expect("write fixture file");
}

#[test]
fn init_respects_gitignore() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let root = tmp.path();

    // One tracked source file with a distinctive symbol.
    write(root, "keep.rs", "pub fn kept_symbol() -> u8 { 1 }\n");
    // A gitignored file and a gitignored directory, each with a distinctive
    // symbol that must NOT end up in any store.
    write(root, "skip.rs", "pub fn skipped_symbol() -> u8 { 2 }\n");
    write(root, "vendor/dep.rs", "pub fn vendored_symbol() -> u8 { 3 }\n");
    // A nested gitignore excluding a file only within its own subtree.
    write(root, "sub/nested.rs", "pub fn nested_kept() -> u8 { 4 }\n");
    write(root, "sub/nested_skip.rs", "pub fn nested_skipped() -> u8 { 5 }\n");

    write(root, ".gitignore", "skip.rs\nvendor/\n");
    write(root, "sub/.gitignore", "nested_skip.rs\n");

    // ---- Scenario 1: gitignored paths are excluded from the index ----
    let init_out = run_in(root, &["init"]);
    assert!(
        init_out.status.success(),
        "lattice init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    let env = parse_single_json(&init_out);
    assert_eq!(env["status"], "ok");
    assert_eq!(
        env["files_indexed"], 2,
        "only keep.rs and sub/nested.rs should be indexed; got {}",
        env["files_indexed"]
    );

    // ---- Scenario 2 & 3: tracked symbols indexed, gitignored ones absent ----
    // `--symbol` is a fuzzy FTS query and always returns nearest neighbors, so
    // presence/absence is decided by whether the EXACT symbol name appears in
    // the hits — not by whether the hit list is empty.
    let names_for = |sym: &str| -> Vec<String> {
        let out = run_in(root, &["query", "--symbol", sym]);
        assert!(out.status.success(), "query --symbol {sym} failed");
        let v = parse_single_json(&out);
        v["hits"]
            .as_array()
            .map(|hits| {
                hits.iter()
                    .filter_map(|h| h["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let indexed = |sym: &str| names_for(sym).iter().any(|n| n == sym);

    assert!(indexed("kept_symbol"), "tracked symbol must be indexed");
    assert!(indexed("nested_kept"), "tracked nested symbol must be indexed");
    assert!(
        !indexed("skipped_symbol"),
        "gitignored file's symbol must NOT be indexed"
    );
    assert!(
        !indexed("vendored_symbol"),
        "gitignored directory's symbol must NOT be indexed"
    );
    assert!(
        !indexed("nested_skipped"),
        "nested-gitignored file's symbol must NOT be indexed"
    );
}

/// Gate for BUG #2 (the ownership-layer half of "init respects .gitignore").
///
/// `ownership::scan` walks git HISTORY, not the working tree, and originally
/// filtered candidates on file extension only. A path that was committed and
/// only later gitignored (the classic `target/` un-tracking) therefore stayed in
/// `git_file_authors` / `git_file_stats` / `git_co_changes` forever, and leaked
/// back out through `query --owners` (including its `co_changed` list) and the
/// churn weighting behind `--hotspots`.
///
/// Fixture: two commits. C1 commits BOTH a real source file and a build
/// artifact. C2 adds a `.gitignore` for the artifact dir and untracks it —
/// exactly the shape of this repo's own `chore: untrack target/` commit.
///
/// Assertions are exact: the ignored path must be wholly absent from the
/// ownership envelope, while the tracked path keeps its full, unchanged stats.
#[test]
fn ownership_pass_respects_gitignore() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let root = tmp.path();

    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Alice")
            .env("GIT_AUTHOR_EMAIL", "alice@example.com")
            .env("GIT_AUTHOR_DATE", "2024-01-01T00:00:00Z")
            .env("GIT_COMMITTER_NAME", "Alice")
            .env("GIT_COMMITTER_EMAIL", "alice@example.com")
            .env("GIT_COMMITTER_DATE", "2024-01-01T00:00:00Z")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };

    // ---- C1: commit a tracked source file AND a build artifact ----
    write(root, "src/keep.rs", "fn tracked_symbol() {}\n");
    write(root, "target/debug/gen.rs", "fn artifact_symbol() {}\n");
    // Subtree-scoping probe: `sub/.gitignore` will ignore `dropme.rs`, but this
    // ROOT-level file of the same basename must survive (git scopes a nested
    // .gitignore to its own subtree; a single root-anchored matcher would
    // wrongly ignore both).
    write(root, "dropme.rs", "fn root_level_symbol() {}\n");
    write(root, "sub/dropme.rs", "fn subtree_symbol() {}\n");
    git(&["init", "-q", "--initial-branch=main"]);
    git(&[
        "add",
        "src/keep.rs",
        "target/debug/gen.rs",
        "dropme.rs",
        "sub/dropme.rs",
    ]);
    git(&["-c", "commit.gpgsign=false", "commit", "-q", "-m", "C1"]);

    // ---- C2: gitignore + untrack the artifact (the `chore: untrack` shape) ----
    write(root, ".gitignore", "target/\n");
    write(root, "sub/.gitignore", "dropme.rs\n");
    git(&["rm", "-q", "--cached", "target/debug/gen.rs"]);
    git(&["rm", "-q", "--cached", "sub/dropme.rs"]);
    git(&["add", ".gitignore", "sub/.gitignore"]);
    git(&["-c", "commit.gpgsign=false", "commit", "-q", "-m", "C2"]);

    let init_out = run_in(root, &["init"]);
    assert!(
        init_out.status.success(),
        "lattice init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );

    let owners_of = |path: &str| -> Value {
        let out = run_in(root, &["query", "--owners", path]);
        assert!(out.status.success(), "query --owners {path} failed");
        parse_single_json(&out)
    };

    // The tracked file keeps its real ownership data (guards over-filtering).
    let kept = owners_of("src/keep.rs");
    assert_eq!(kept["churn"].as_u64().unwrap(), 1, "tracked file churn != 1");
    assert_eq!(
        kept["owners"].as_array().map(Vec::len).unwrap_or(0),
        1,
        "tracked file must have exactly one owner"
    );

    // The now-gitignored artifact must be wholly absent from the ownership store.
    let ignored = owners_of("target/debug/gen.rs");
    assert_eq!(
        ignored["churn"].as_u64().unwrap(),
        0,
        "gitignored path must have no churn recorded"
    );
    assert_eq!(
        ignored["owners"].as_array().map(Vec::len).unwrap_or(0),
        0,
        "gitignored path must have no owners recorded"
    );

    // Nested-gitignore scoping, both directions. `sub/dropme.rs` is ignored by
    // `sub/.gitignore`; the root-level `dropme.rs` is NOT (git agrees), so a
    // matcher that merges nested patterns into the root would over-filter here.
    assert_eq!(
        owners_of("sub/dropme.rs")["churn"].as_u64().unwrap(),
        0,
        "nested-gitignored path must have no churn recorded"
    );
    assert_eq!(
        owners_of("dropme.rs")["churn"].as_u64().unwrap(),
        1,
        "root-level file must NOT be filtered by a nested .gitignore of the \
         same basename (nested rules are subtree-scoped)"
    );

    // And it must not leak in via any OTHER file's co_changed list. In C1 both
    // files were added together, so a co-change edge exists in history.
    let co: Vec<String> = kept["co_changed"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !co.iter().any(|p| p.starts_with("target/")),
        "gitignored path leaked into co_changed: {co:?}"
    );
}
