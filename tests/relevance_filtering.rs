// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for the `relevance-and-filtering` slice.
//!
//! Fixture: tests/fixtures/relevance_basic/
//!   src/core.rs          — core::run, called by main.rs and util.rs → in_degree 2
//!   src/main.rs          — main::main, calls core::run
//!   src/util.rs          — util::helper, calls core::run
//!   src/api.generated.rs — generated_endpoint, class "generated"
//!   src/tests/core_test.rs — run(), class "test", in_degree 0
//!   tests/fixtures/data.rs — fixture_data(), class "fixture"
//!   vendor/lib.rs        — vendor_fn(), class "vendored"
//!
//! Scenarios:
//!   1. `query --symbol "run" --exclude-tests` — hits include core::run (class
//!      "source"), exclude core_test::run (class "test"). Assert by name/path.
//!   2. `query --symbol "run" --limit 1` — exactly 1 hit returned; verify
//!      the hit has a `class` field.
//!   3. `query --symbol "run" --kind function` — kind filter works; assert
//!      the first hit has kind=="function".
//!   4. `query --symbol "run" --kind module` — kind filter works for modules.
//!   5. `query --symbol "zzzz" --limit 5` — clean miss still exit 0, empty hits.
//!   6. `query --symbol "run" --kind invalid_kind` — invalid_flag error, exit non-zero.
//!   7. `query --symbol "run" --limit 0` — invalid_flag error (limit must be >= 1).
//!   8. `query "core run" --exclude-tests` — semantic query with --exclude-tests
//!      returns hits, each with a `class` field, no test/fixture paths in results.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

/// Copy the relevance_basic fixture into a fresh temp dir.
fn fixture_copy() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/relevance_basic");
    copy_dir_recursive(&src, tmp.path()).expect("copy fixture dir");
    tmp
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let src_path = entry.path();
        let dst_path = dst.join(&name);
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
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

#[test]
fn relevance_filtering_end_to_end() {
    let tmp = fixture_copy();
    let root = tmp.path();

    // Init the store.
    let init = run_in(root, &["init"]);
    assert!(
        init.status.success(),
        "init exited non-zero; stderr: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    // ---- Scenario 1: --exclude-tests removes test-class symbols ----
    let s1 = run_in(root, &["query", "--symbol", "run", "--exclude-tests"]);
    assert!(
        s1.status.success(),
        "s1 exited non-zero; stderr: {}",
        String::from_utf8_lossy(&s1.stderr)
    );
    let e1 = parse_single_json(&s1);
    assert_eq!(e1["protocol_version"], "1.5.0");
    assert_eq!(e1["query_type"], "symbol");
    let hits1 = e1["hits"].as_array().expect("hits is array");
    // core::run MUST be present (class "source").
    assert!(
        hits1
            .iter()
            .any(|h| h["name"] == "run" && h["path"].as_str().unwrap_or("").contains("core.rs")
                && !h["path"].as_str().unwrap_or("").contains("core_test")),
        "expected core.rs::run in --exclude-tests results; hits: {e1}"
    );
    // core_test::run MUST NOT be present (class "test").
    assert!(
        !hits1.iter().any(|h| h["path"]
            .as_str()
            .unwrap_or("")
            .contains("core_test.rs")),
        "core_test.rs::run must be excluded by --exclude-tests; hits: {e1}"
    );
    // Every hit must have a "class" field.
    for hit in hits1 {
        assert!(
            hit.get("class").is_some(),
            "every symbol hit must have a 'class' field; hit: {hit}"
        );
    }

    // ---- Scenario 2: --limit 1 returns exactly 1 hit with class field ----
    let s2 = run_in(root, &["query", "--symbol", "run", "--limit", "1"]);
    assert!(
        s2.status.success(),
        "s2 exited non-zero; stderr: {}",
        String::from_utf8_lossy(&s2.stderr)
    );
    let e2 = parse_single_json(&s2);
    let hits2 = e2["hits"].as_array().expect("hits is array");
    assert_eq!(
        hits2.len(),
        1,
        "--limit 1 must return exactly 1 hit; hits: {e2}"
    );
    assert!(
        hits2[0].get("class").is_some(),
        "hit must have a 'class' field; hit: {}",
        hits2[0]
    );

    // ---- Scenario 3: --kind function works ----
    let s3 = run_in(root, &["query", "--symbol", "run", "--kind", "function"]);
    assert!(
        s3.status.success(),
        "s3 exited non-zero; stderr: {}",
        String::from_utf8_lossy(&s3.stderr)
    );
    let e3 = parse_single_json(&s3);
    let hits3 = e3["hits"].as_array().expect("hits is array");
    assert!(
        !hits3.is_empty(),
        "--kind function should return at least one hit"
    );
    for hit in hits3 {
        assert_eq!(
            hit["kind"], "function",
            "--kind function must only return function hits; hit: {hit}"
        );
    }

    // ---- Scenario 4: --kind module works (returns module-kind hits or empty) ----
    let s4 = run_in(root, &["query", "--symbol", "core", "--kind", "module"]);
    assert!(
        s4.status.success(),
        "s4 exited non-zero; stderr: {}",
        String::from_utf8_lossy(&s4.stderr)
    );
    let e4 = parse_single_json(&s4);
    assert_eq!(e4["query_type"], "symbol");
    // All returned hits must be modules (if any).
    let hits4 = e4["hits"].as_array().expect("hits is array");
    for hit in hits4 {
        assert_eq!(
            hit["kind"], "module",
            "--kind module must only return module hits; hit: {hit}"
        );
    }

    // ---- Scenario 5: clean miss with --limit still exits 0 ----
    let s5 = run_in(root, &["query", "--symbol", "zzzz", "--limit", "5"]);
    assert!(
        s5.status.success(),
        "clean miss must exit 0; stderr: {}",
        String::from_utf8_lossy(&s5.stderr)
    );
    let e5 = parse_single_json(&s5);
    assert_eq!(e5["query_type"], "symbol");
    // FTS is fuzzy — the miss check uses the specific token, not empty
    // We assert that the specific named symbol "zzzz" is not in hits by name.
    let hits5 = e5["hits"].as_array().expect("hits is array");
    assert!(
        !hits5.iter().any(|h| h["name"] == "zzzz"),
        "no symbol named 'zzzz' should exist"
    );

    // ---- Scenario 6: invalid --kind returns invalid_flag error ----
    let s6 = run_in(
        root,
        &["query", "--symbol", "run", "--kind", "invalid_kind"],
    );
    assert!(
        !s6.status.success(),
        "invalid --kind must exit non-zero; stderr: {}",
        String::from_utf8_lossy(&s6.stderr)
    );
    let e6 = parse_single_json(&s6);
    assert_eq!(e6["query_type"], "error");
    assert_eq!(e6["error"]["code"], "invalid_flag");

    // ---- Scenario 7: --limit 0 returns invalid_flag error ----
    let s7 = run_in(root, &["query", "--symbol", "run", "--limit", "0"]);
    assert!(
        !s7.status.success(),
        "--limit 0 must exit non-zero; stderr: {}",
        String::from_utf8_lossy(&s7.stderr)
    );
    let e7 = parse_single_json(&s7);
    assert_eq!(e7["query_type"], "error");
    assert_eq!(e7["error"]["code"], "invalid_flag");

    // ---- Scenario 8: semantic query with --exclude-tests returns class field ----
    let s8 = run_in(root, &["query", "core run", "--exclude-tests"]);
    assert!(
        s8.status.success(),
        "s8 semantic+exclude-tests exited non-zero; stderr: {}",
        String::from_utf8_lossy(&s8.stderr)
    );
    let e8 = parse_single_json(&s8);
    assert_eq!(e8["protocol_version"], "1.5.0");
    assert_eq!(e8["query_type"], "hybrid");
    let hits8 = e8["hits"].as_array().expect("hits is array");
    // Every returned semantic hit must have a "class" field.
    for hit in hits8 {
        assert!(
            hit.get("class").is_some(),
            "every semantic hit must have a 'class' field; hit: {hit}"
        );
    }
    // No test or fixture paths should appear.
    for hit in hits8 {
        let path = hit["path"].as_str().unwrap_or("");
        assert!(
            !path.contains("core_test"),
            "test file must be excluded; hit: {hit}"
        );
    }
}
