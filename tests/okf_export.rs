// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for slice `okf-export`.
//!
//! `lattice export` renders the built graph to OKF Markdown under
//! .lattice/export/okf/{services,modules,teams}/; `lattice summarize <path>`
//! returns a human summary for a path. Both derive from the graph only.
//!
//!   1. export writes the three OKF dirs, each with >=1 non-empty .md file that
//!      begins with a Markdown `#` heading; envelope command "export", status
//!      "ok", protocol_version "1.5.0", exit 0.
//!   2. at least one modules/*.md contains the literal string `auth.rs`.
//!   3. summarize auth.rs -> exit 0, envelope command "summarize",
//!      target "auth.rs", non-empty summary string, protocol_version "1.5.0".
//!   4. stream discipline: stdout is exactly one JSON object on both commands.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

/// Copy the rust_basic fixture into a fresh temp dir (no .gitignore, no .lattice/).
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

/// A dir must contain at least one .md file that is non-empty and starts with `#`.
fn has_markdown_page(dir: &Path) -> bool {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            if let Ok(content) = fs::read_to_string(&path) {
                if content.trim_start().starts_with('#') && !content.trim().is_empty() {
                    return true;
                }
            }
        }
    }
    false
}

#[test]
fn okf_export_and_summarize() {
    // Fixture T: rust_basic copied and initialized once.
    let t = fixture_copy();
    let t_root = t.path();
    let init_out = run_in(t_root, &["init"]);
    assert!(
        init_out.status.success(),
        "lattice init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    assert!(
        t_root.join(".lattice").is_dir(),
        ".lattice/ must exist after init"
    );

    // ---- Scenario 1: export writes the three OKF dirs with Markdown ----
    let ex = run_in(t_root, &["export"]);
    assert!(
        ex.status.success(),
        "export must exit 0; stderr: {}",
        String::from_utf8_lossy(&ex.stderr)
    );
    let ee = parse_single_json(&ex);
    assert_eq!(ee["protocol_version"], "1.5.0");
    assert_eq!(ee["command"], "export");
    assert_eq!(ee["status"], "ok");

    let okf = t_root.join(".lattice/export/okf");
    for sub in ["services", "modules", "teams"] {
        let dir = okf.join(sub);
        assert!(dir.is_dir(), "{sub}/ directory must exist under {okf:?}");
        assert!(
            has_markdown_page(&dir),
            "{sub}/ must contain a non-empty .md file beginning with a Markdown # heading"
        );
    }

    // ---- Scenario 2: module pages reflect the graph ----
    let modules_dir = okf.join("modules");
    let mut found_auth = false;
    for entry in fs::read_dir(&modules_dir).expect("read modules dir").flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("md") {
            if let Ok(content) = fs::read_to_string(&p) {
                if content.contains("auth.rs") {
                    found_auth = true;
                    break;
                }
            }
        }
    }
    assert!(
        found_auth,
        "at least one modules/*.md must contain the literal string `auth.rs`"
    );

    // ---- Scenario 3: summarize emits a human summary for a valid path ----
    let sm = run_in(t_root, &["summarize", "auth.rs"]);
    assert!(
        sm.status.success(),
        "summarize must exit 0; stderr: {}",
        String::from_utf8_lossy(&sm.stderr)
    );
    let se = parse_single_json(&sm);
    assert_eq!(se["protocol_version"], "1.5.0");
    assert_eq!(se["command"], "summarize");
    assert_eq!(se["target"], "auth.rs");
    let summary = se["summary"].as_str().expect("summary must be a string");
    assert!(
        !summary.trim().is_empty(),
        "summary must be a non-empty string"
    );

    // ---- Scenario 4: stream discipline already asserted via parse_single_json
    // on both export and summarize invocations above. ----
}
