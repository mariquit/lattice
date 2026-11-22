// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for the cross-module-resolution slice.
//!
//! Tests:
//!   TS (ts_aliases fixture):
//!     T1 — init: 5 files indexed, protocol_version == "1.5.0"
//!     T2 — impact src/core/parse.ts: callers [app,report,view], dependents 4, blast 4
//!     T3 — callers parse (bare name): resolved parse@line6, hit runApp@line5
//!     T4 — callers Parser::parse (qualified): same resolved/hits as T3
//!   Python (py_pkg fixture):
//!     P1 — init: 5 files indexed
//!     P2 — impact pkg/util.py: callers+deps [core,tool], blast 2
//!     P3 — impact pkg/core.py: callers [app], deps [app,__init__], blast 2
//!     P4 — callers handle: resolved handle@line3, hit main@line3
//!   Idempotence: second init produces identical impact envelope
//!   JSONC-strip unit: tested inline in ingest.rs; see strip_jsonc tests

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

/// Recursively copy src dir into dst (creates subdirs as needed).
fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create dst dir");
    for entry in fs::read_dir(src).expect("read src dir") {
        let entry = entry.expect("read dir entry");
        let name = entry.file_name();
        let src_path = entry.path();
        let dst_path = dst.join(&name);
        let ft = entry.file_type().expect("file_type");
        if ft.is_dir() {
            copy_dir_recursive(&src_path, &dst_path);
        } else {
            fs::copy(&src_path, &dst_path).expect("copy file");
        }
    }
}

fn fixture_copy_ts() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ts_aliases");
    copy_dir_recursive(&src, tmp.path());
    tmp
}

fn fixture_copy_py() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/py_pkg");
    copy_dir_recursive(&src, tmp.path());
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
        "stdout is not a single JSON object: {stdout:?}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str::<Value>(trimmed)
        .unwrap_or_else(|e| panic!("stdout is not exactly one JSON object ({e}): {stdout:?}"))
}

fn sorted_strs(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = v
        .as_array()
        .expect("expected array")
        .iter()
        .map(|x| x.as_str().expect("string element").to_string())
        .collect();
    out.sort();
    out
}

// ─────────────────────── TypeScript tests ───────────────────────────────────

#[test]
fn ts_t1_init_five_files() {
    let tmp = fixture_copy_ts();
    let root = tmp.path();
    let init_out = run_in(root, &["init"]);
    assert!(
        init_out.status.success(),
        "lattice init failed; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    let env = parse_single_json(&init_out);
    assert_eq!(env["protocol_version"], "1.5.0", "T1 protocol_version");
    assert_eq!(env["status"], "ok", "T1 status");
    assert_eq!(env["files_indexed"], 5, "T1 files_indexed (tsconfig.json not a source)");
}

#[test]
fn ts_t2_impact_parse_ts() {
    let tmp = fixture_copy_ts();
    let root = tmp.path();
    run_in(root, &["init"]);

    let out = run_in(root, &["query", "--impact", "src/core/parse.ts"]);
    assert!(
        out.status.success(),
        "query --impact failed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);
    assert_eq!(env["query_type"], "impact", "T2 query_type");
    assert_eq!(env["target"], "src/core/parse.ts", "T2 target");

    let callers = sorted_strs(&env["callers"]);
    assert_eq!(
        callers,
        vec!["src/app.ts", "src/report.ts", "src/view.ts"],
        "T2 callers"
    );

    let dependents = sorted_strs(&env["dependents"]);
    assert_eq!(
        dependents,
        vec!["src/app.ts", "src/core/index.ts", "src/report.ts", "src/view.ts"],
        "T2 dependents"
    );

    assert_eq!(env["blast_radius"], 4, "T2 blast_radius");
}

#[test]
fn ts_t3_callers_bare_parse() {
    let tmp = fixture_copy_ts();
    let root = tmp.path();
    run_in(root, &["init"]);

    let out = run_in(root, &["query", "--callers", "parse"]);
    assert!(
        out.status.success(),
        "query --callers parse failed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);
    assert_eq!(env["query_type"], "callers", "T3 query_type");

    // resolved must contain {path:src/core/parse.ts, name:parse, line:6}
    let resolved = env["resolved"].as_array().expect("resolved array");
    assert!(
        !resolved.is_empty(),
        "T3 resolved is empty\nfull envelope: {env}"
    );
    let r0 = &resolved[0];
    assert_eq!(r0["path"], "src/core/parse.ts", "T3 resolved path");
    assert_eq!(r0["name"], "parse", "T3 resolved name");
    assert_eq!(r0["line"], 6, "T3 resolved line");

    // hits must contain {path:src/app.ts, name:runApp, line:5}
    let hits = env["hits"].as_array().expect("hits array");
    let hit = hits
        .iter()
        .find(|h| h["path"] == "src/app.ts" && h["name"] == "runApp")
        .unwrap_or_else(|| panic!("T3 missing hit runApp in src/app.ts; hits: {hits:?}"));
    assert_eq!(hit["line"], 5, "T3 hit line");
}

#[test]
fn ts_t4_callers_qualified_parser_parse() {
    let tmp = fixture_copy_ts();
    let root = tmp.path();
    run_in(root, &["init"]);

    let out = run_in(root, &["query", "--callers", "Parser::parse"]);
    assert!(
        out.status.success(),
        "query --callers Parser::parse failed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);
    assert_eq!(env["query_type"], "callers", "T4 query_type");

    let resolved = env["resolved"].as_array().expect("resolved array");
    assert!(
        !resolved.is_empty(),
        "T4 resolved is empty\nfull envelope: {env}"
    );
    let r0 = &resolved[0];
    assert_eq!(r0["path"], "src/core/parse.ts", "T4 resolved path");
    assert_eq!(r0["name"], "parse", "T4 resolved name");
    assert_eq!(r0["line"], 6, "T4 resolved line");

    let hits = env["hits"].as_array().expect("hits array");
    let hit = hits
        .iter()
        .find(|h| h["path"] == "src/app.ts" && h["name"] == "runApp")
        .unwrap_or_else(|| panic!("T4 missing hit runApp; hits: {hits:?}"));
    assert_eq!(hit["line"], 5, "T4 hit line");
}

#[test]
fn ts_idempotence() {
    let tmp = fixture_copy_ts();
    let root = tmp.path();
    run_in(root, &["init"]);

    let out1 = run_in(root, &["query", "--impact", "src/core/parse.ts"]);
    let env1 = parse_single_json(&out1);

    // Second init — should not change graph.
    let init2 = run_in(root, &["init"]);
    assert!(
        init2.status.success(),
        "second init failed; stderr: {}",
        String::from_utf8_lossy(&init2.stderr)
    );

    let out2 = run_in(root, &["query", "--impact", "src/core/parse.ts"]);
    let env2 = parse_single_json(&out2);

    assert_eq!(
        env1["blast_radius"], env2["blast_radius"],
        "idempotence: blast_radius changed after second init"
    );
    assert_eq!(
        sorted_strs(&env1["callers"]),
        sorted_strs(&env2["callers"]),
        "idempotence: callers changed after second init"
    );
    assert_eq!(
        sorted_strs(&env1["dependents"]),
        sorted_strs(&env2["dependents"]),
        "idempotence: dependents changed after second init"
    );
}

// ─────────────────────── Python tests ───────────────────────────────────────

#[test]
fn py_p1_init_five_files() {
    let tmp = fixture_copy_py();
    let root = tmp.path();
    let init_out = run_in(root, &["init"]);
    assert!(
        init_out.status.success(),
        "lattice init failed; stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    let env = parse_single_json(&init_out);
    assert_eq!(env["protocol_version"], "1.5.0", "P1 protocol_version");
    assert_eq!(env["files_indexed"], 5, "P1 files_indexed");
}

#[test]
fn py_p2_impact_util() {
    let tmp = fixture_copy_py();
    let root = tmp.path();
    run_in(root, &["init"]);

    let out = run_in(root, &["query", "--impact", "pkg/util.py"]);
    assert!(
        out.status.success(),
        "query --impact failed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);
    assert_eq!(env["query_type"], "impact", "P2 query_type");
    assert_eq!(env["target"], "pkg/util.py", "P2 target");

    let callers = sorted_strs(&env["callers"]);
    assert_eq!(callers, vec!["pkg/core.py", "tool.py"], "P2 callers");

    let dependents = sorted_strs(&env["dependents"]);
    assert_eq!(dependents, vec!["pkg/core.py", "tool.py"], "P2 dependents");

    assert_eq!(env["blast_radius"], 2, "P2 blast_radius");
}

#[test]
fn py_p3_impact_core() {
    let tmp = fixture_copy_py();
    let root = tmp.path();
    run_in(root, &["init"]);

    let out = run_in(root, &["query", "--impact", "pkg/core.py"]);
    assert!(
        out.status.success(),
        "query --impact failed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);
    assert_eq!(env["query_type"], "impact", "P3 query_type");
    assert_eq!(env["target"], "pkg/core.py", "P3 target");

    let callers = sorted_strs(&env["callers"]);
    assert_eq!(callers, vec!["app.py"], "P3 callers");

    let dependents = sorted_strs(&env["dependents"]);
    assert_eq!(dependents, vec!["app.py", "pkg/__init__.py"], "P3 dependents");

    assert_eq!(env["blast_radius"], 2, "P3 blast_radius");
}

#[test]
fn py_p4_callers_handle() {
    let tmp = fixture_copy_py();
    let root = tmp.path();
    run_in(root, &["init"]);

    let out = run_in(root, &["query", "--callers", "handle"]);
    assert!(
        out.status.success(),
        "query --callers handle failed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_single_json(&out);
    assert_eq!(env["query_type"], "callers", "P4 query_type");

    let resolved = env["resolved"].as_array().expect("resolved array");
    assert!(
        !resolved.is_empty(),
        "P4 resolved is empty\nfull envelope: {env}"
    );
    let r0 = &resolved[0];
    assert_eq!(r0["path"], "pkg/core.py", "P4 resolved path");
    assert_eq!(r0["name"], "handle", "P4 resolved name");
    assert_eq!(r0["line"], 3, "P4 resolved line");

    let hits = env["hits"].as_array().expect("hits array");
    let hit = hits
        .iter()
        .find(|h| h["path"] == "app.py" && h["name"] == "main")
        .unwrap_or_else(|| panic!("P4 missing hit main in app.py; hits: {hits:?}"));
    assert_eq!(hit["line"], 3, "P4 hit line");
}
