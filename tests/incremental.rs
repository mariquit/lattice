// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for slice `incremental-reindex`.
//!
//! Scenarios:
//!   1. Baseline: `init` on rust_basic emits files_indexed == 4; capture the
//!      raw stdout bytes of `query --impact db.rs` and assert auth.rs impact.
//!   2. Edit metrics.rs (report() now calls auth::validate_token), re-`init`:
//!      counters files_indexed 4 / files_reindexed 1 / files_unchanged 3,
//!      protocol_version byte-equal "1.5.0".
//!   3. The changed file's edges update: auth.rs callers/dependents include
//!      metrics.rs.
//!   4. Unchanged file db.rs impact stdout is BYTE-IDENTICAL to the baseline.
//!   5. Stream discipline: stdout is exactly one JSON object per invocation.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

/// Copy the rust_basic fixture into a fresh temp dir (no .gitignore, no
/// .lattice/). The edit in Scenario 2 mutates only this temp copy.
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

/// Stream discipline (Scenario 5): stdout is exactly one JSON object and
/// nothing else. serde_json::from_str fails on trailing non-whitespace, so a
/// successful parse of the trimmed stdout also proves there is a single value.
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

/// The edited metrics.rs: report() now *calls*
/// auth::validate_token in addition to importing auth.
const EDITED_METRICS_RS: &str = "\
use crate::auth;

pub fn report() -> u32 {
    if auth::validate_token(\"abcdefghi\") { 1 } else { 0 }
}
";

#[test]
fn incremental_reindex_end_to_end() {
    let tmp = fixture_copy();
    let root = tmp.path();

    // ---- Scenario 1: baseline ----
    let init_out = run_in(root, &["init"]);
    assert!(
        init_out.status.success(),
        "baseline lattice init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    let env1 = parse_single_json(&init_out);
    assert_eq!(env1["protocol_version"], "1.5.0");
    assert_eq!(env1["files_indexed"], 4);

    // Capture the raw stdout bytes of `query --impact db.rs` as the baseline.
    let db_before = run_in(root, &["query", "--impact", "db.rs"]);
    assert!(
        db_before.status.success(),
        "baseline query --impact db.rs exited non-zero; stderr: {}",
        String::from_utf8_lossy(&db_before.stderr)
    );
    let baseline_db: Vec<u8> = db_before.stdout.clone();
    let db_env = parse_single_json(&db_before);
    assert_eq!(db_env["callers"], serde_json::json!(["main.rs"]));
    assert_eq!(db_env["dependents"], serde_json::json!(["main.rs"]));
    assert_eq!(db_env["blast_radius"], 1);

    // auth.rs baseline: callers ["main.rs"], blast_radius 2.
    let auth_before = run_in(root, &["query", "--impact", "auth.rs"]);
    assert!(auth_before.status.success());
    let auth_env = parse_single_json(&auth_before);
    assert_eq!(auth_env["callers"], serde_json::json!(["main.rs"]));
    assert_eq!(auth_env["blast_radius"], 2);

    // ---- Scenario 2: edit one file, re-init re-ingests only that file ----
    fs::write(root.join("metrics.rs"), EDITED_METRICS_RS).expect("overwrite metrics.rs");

    let reinit_out = run_in(root, &["init"]);
    assert!(
        reinit_out.status.success(),
        "re-init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&reinit_out.stderr)
    );
    let env2 = parse_single_json(&reinit_out);
    assert_eq!(env2["protocol_version"], "1.5.0");
    assert_eq!(env2["files_indexed"], 4);
    assert_eq!(env2["files_reindexed"], 1);
    assert_eq!(env2["files_unchanged"], 3);
    // The unaffected file's impact report is unchanged.
    assert_eq!(
        env2["files_reindexed"].as_u64().unwrap() + env2["files_unchanged"].as_u64().unwrap(),
        env2["files_indexed"].as_u64().unwrap()
    );

    // ---- Scenario 3: the changed file's edges updated ----
    let auth_after = run_in(root, &["query", "--impact", "auth.rs"]);
    assert!(
        auth_after.status.success(),
        "post-refresh query --impact auth.rs exited non-zero; stderr: {}",
        String::from_utf8_lossy(&auth_after.stderr)
    );
    let auth_env2 = parse_single_json(&auth_after);
    assert_eq!(
        auth_env2["callers"],
        serde_json::json!(["main.rs", "metrics.rs"])
    );
    assert_eq!(
        auth_env2["dependents"],
        serde_json::json!(["main.rs", "metrics.rs"])
    );
    assert_eq!(auth_env2["blast_radius"], 2);

    // ---- Scenario 4: unchanged file's state intact, envelope byte-stable ----
    let db_after = run_in(root, &["query", "--impact", "db.rs"]);
    assert!(
        db_after.status.success(),
        "post-refresh query --impact db.rs exited non-zero; stderr: {}",
        String::from_utf8_lossy(&db_after.stderr)
    );
    assert_eq!(
        db_after.stdout, baseline_db,
        "db.rs impact stdout must be byte-identical across the refresh"
    );
}
