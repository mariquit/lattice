// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for S0: vector-index resumability + non-silent empty-semantic.
//!
//! Tests:
//!   1. Interrupted-init repro + query-time heal (the main bug): reproduce the
//!      tencentdb state (markers durable, documents=0), run a semantic query,
//!      assert freshness=="refreshed" and documents returns to D=5.
//!   2. Per-file resumability (convergence proof): init, record per-path id
//!      sets, delete one file's rows + embedded entry, re-init, assert
//!      files_reindexed==0 (graph untouched), documents==D, and OTHER files'
//!      id sets are byte-identical (only the pending file was re-embedded).
//!   3. Non-silent empty-semantic: init an empty fixture dir, run a query,
//!      assert vectors_empty==true in the envelope + stderr note; also assert
//!      the field is ABSENT on a populated fixture.
//!   4. Regression guard (schema migration): write schema_version='3' directly,
//!      run a query (not init), assert heal to documents==D and schema_version=='4'.
//!
//! Ground truth D=5 (hand-counted):
//!   auth.rs:    validate_token, check_signature  → 2 spans
//!   db.rs:      save_session                     → 1 span
//!   main.rs:    main                             → 1 span
//!   metrics.rs: report                           → 1 span
//!                                                  ------
//!                                                  D = 5

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use rusqlite::Connection;
use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");
const SEEDED_QUERY: &str = "validate token signature";

/// Hand-counted expected document rows for the rust_basic fixture.
const D: usize = 5;

// ── Test helpers ─────────────────────────────────────────────────────────────

/// Copy rust_basic fixture into a fresh temp dir.
fn fixture_copy() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic");
    for entry in fs::read_dir(&src).expect("read fixture dir") {
        let entry = entry.expect("fixture dir entry");
        let name = entry.file_name();
        fs::copy(entry.path(), tmp.path().join(&name)).expect("copy fixture file");
    }
    tmp
}

/// Create an empty temp dir (no source files) for testing zero-document case.
fn empty_fixture() -> tempfile::TempDir {
    tempfile::tempdir().expect("create empty temp dir")
}

fn run_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn lattice binary")
}

fn parse_single_json(output: &Output) -> Value {
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout UTF-8");
    let trimmed = stdout.trim();
    let lines: Vec<&str> = trimmed.lines().collect();
    assert_eq!(lines.len(), 1, "stdout must be exactly one line (one JSON object); got:\n{stdout}");
    serde_json::from_str(lines[0]).expect("parse JSON")
}

/// Open vectors.db in the .lattice dir, registering sqlite-vec extension.
fn open_vectors_db(root: &Path) -> Connection {
    let db_path = root.join(".lattice").join("vectors.db");
    // Register sqlite-vec so vec0 virtual table queries work.
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *const std::os::raw::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> std::os::raw::c_int,
        >(sqlite_vec::sqlite3_vec_init as *const ())));
    }
    Connection::open(&db_path).expect("open vectors.db")
}

/// Count rows in `documents` table.
fn doc_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
        .expect("count documents")
}

/// Read path → set-of-rowids from `documents`.
fn doc_ids_by_path(conn: &Connection) -> BTreeMap<String, BTreeSet<i64>> {
    let mut stmt = conn
        .prepare("SELECT path, id FROM documents ORDER BY path, id")
        .expect("prepare");
    let rows = stmt
        .query_map([], |row| {
            let path: String = row.get(0)?;
            let id: i64 = row.get(1)?;
            Ok((path, id))
        })
        .expect("query");
    let mut map: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
    for row in rows {
        let (path, id) = row.expect("read row");
        map.entry(path).or_default().insert(id);
    }
    map
}

/// Read vec_meta schema_version.
fn schema_version(conn: &Connection) -> String {
    conn.query_row(
        "SELECT value FROM vec_meta WHERE key = 'schema_version'",
        [],
        |row| row.get(0),
    )
    .expect("read schema_version")
}

// ── Scenario 1: interrupted-init repro + query-time heal ─────────────────────

#[test]
fn s1_query_time_heal_after_interrupted_init() {
    let tmp = fixture_copy();
    let root = tmp.path();

    // Full init: graph/FTS/meta all populated, D documents embedded.
    let init_out = run_in(root, &["init"]);
    assert!(init_out.status.success(), "init failed: {:?}", init_out);

    // Reproduce the tencentdb bug state: schema marker + meta hashes are
    // durable (kept), but documents and vec_documents are wiped (as if
    // embedding was killed mid-transaction in the old code path).
    {
        let conn = open_vectors_db(root);
        // Also delete embedded_files rows so has_pending() fires.
        conn.execute("DELETE FROM documents", []).expect("delete documents");
        conn.execute("DELETE FROM vec_documents", []).expect("delete vec_documents");
        conn.execute("DELETE FROM embedded_files", []).expect("delete embedded_files");
        // vec_meta.schema_version row is KEPT — exact tencentdb state.
        let count = doc_count(&conn);
        assert_eq!(count, 0, "setup: documents should be empty now");
    }

    // Run a semantic query — should self-heal (freshness="refreshed") and
    // return non-empty hits.
    let q_out = run_in(root, &["query", SEEDED_QUERY]);
    assert!(q_out.status.success(), "query failed after simulated interrupted init: {:?}", q_out);

    let env = parse_single_json(&q_out);
    assert_eq!(
        env["query_type"].as_str().unwrap_or(""),
        "hybrid",
        "query_type must be 'hybrid'"
    );
    assert_eq!(
        env["freshness"].as_str().unwrap_or(""),
        "refreshed",
        "freshness must be 'refreshed' — self-heal must have run"
    );

    // hits must be non-empty and auth.rs must be the top hit.
    let hits = env["hits"].as_array().expect("hits array");
    assert!(
        !hits.is_empty(),
        "hits must not be empty after self-heal (docs were re-embedded)"
    );
    assert_eq!(
        hits[0]["path"].as_str().unwrap_or(""),
        "auth.rs",
        "top hit must be auth.rs (seeded query: 'validate token signature')"
    );

    // Verify documents were fully restored.
    let conn = open_vectors_db(root);
    let count = doc_count(&conn);
    assert_eq!(count, D as i64, "documents must return to D={D} after self-heal");
}

// ── Scenario 2: per-file resumability (convergence proof) ─────────────────────

#[test]
fn s2_per_file_resumability_convergence() {
    let tmp = fixture_copy();
    let root = tmp.path();

    // Full init.
    let init1 = run_in(root, &["init"]);
    assert!(init1.status.success(), "init failed: {:?}", init1);

    // Record per-path id sets BEFORE deletion.
    let ids_before: BTreeMap<String, BTreeSet<i64>>;
    {
        let conn = open_vectors_db(root);
        assert_eq!(
            doc_count(&conn),
            D as i64,
            "initial documents count must be D={D}"
        );
        ids_before = doc_ids_by_path(&conn);
    }

    // Delete documents + vec_documents + embedded_files row for ONE file only.
    // This simulates a per-file resume scenario: one file lost, rest intact.
    let target_file = "auth.rs";
    {
        let conn = open_vectors_db(root);
        // Get ids for target file.
        let ids: Vec<i64> = {
            let mut stmt = conn
                .prepare("SELECT id FROM documents WHERE path = ?1")
                .expect("prepare");
            stmt.query_map(rusqlite::params![target_file], |row| row.get(0))
                .expect("query")
                .filter_map(|r| r.ok())
                .collect()
        };
        assert!(
            !ids.is_empty(),
            "target file {target_file} must have document rows"
        );
        for id in &ids {
            conn.execute(
                "DELETE FROM vec_documents WHERE rowid = ?1",
                rusqlite::params![id],
            )
            .expect("delete vec row");
        }
        conn.execute(
            "DELETE FROM documents WHERE path = ?1",
            rusqlite::params![target_file],
        )
        .expect("delete document rows");
        conn.execute(
            "DELETE FROM embedded_files WHERE path = ?1",
            rusqlite::params![target_file],
        )
        .expect("delete embedded_files row");
    }

    // Run init again — should NOT re-parse/re-graph (graph path unchanged),
    // but SHOULD re-embed only the pending file.
    let init2 = run_in(root, &["init"]);
    assert!(init2.status.success(), "second init failed: {:?}", init2);

    let init2_json = parse_single_json(&init2);
    // Graph path must report files_reindexed==0 (no source file changed).
    assert_eq!(
        init2_json["files_reindexed"].as_u64().unwrap_or(999),
        0,
        "files_reindexed must be 0 — no source file changed, only embedding was pending"
    );
    // files_unchanged must be 4 (all fixture files).
    assert_eq!(
        init2_json["files_unchanged"].as_u64().unwrap_or(999),
        4,
        "files_unchanged must be 4 (all rust_basic files)"
    );

    // documents must be back to D.
    let ids_after: BTreeMap<String, BTreeSet<i64>>;
    {
        let conn = open_vectors_db(root);
        let count = doc_count(&conn);
        assert_eq!(
            count,
            D as i64,
            "documents must return to D={D} after second init"
        );
        ids_after = doc_ids_by_path(&conn);
    }

    // CONVERGENCE PROOF: every NON-target file's id set must be byte-identical.
    // If any other file's ids changed, the "resume" was actually a full re-embed.
    for (path, before_ids) in &ids_before {
        if path == target_file {
            continue; // target was deliberately deleted and re-embedded
        }
        let after_ids = ids_after
            .get(path)
            .unwrap_or_else(|| panic!("path {path} missing after second init"));
        assert_eq!(
            before_ids, after_ids,
            "id set for {path} must be byte-identical (only {target_file} was re-embedded, not a full re-embed)"
        );
    }
    // Target file must now have its rows (whatever new ids are fine).
    assert!(
        ids_after.contains_key(target_file),
        "{target_file} must have document rows after second init"
    );
}

// ── Scenario 3: non-silent empty-semantic ─────────────────────────────────────

#[test]
fn s3_non_silent_empty_semantic() {
    // 3a. Empty fixture: zero source files → vectors_empty must be true.
    let empty = empty_fixture();
    let root = empty.path();

    let init_out = run_in(root, &["init"]);
    assert!(init_out.status.success(), "init on empty dir failed: {:?}", init_out);

    let q_out = run_in(root, &["query", "anything"]);
    assert!(q_out.status.success(), "query on empty dir failed: {:?}", q_out);

    let env = parse_single_json(&q_out);
    assert_eq!(
        env["query_type"].as_str().unwrap_or(""),
        "hybrid",
        "query_type must be 'hybrid'"
    );
    let hits = env["hits"].as_array().expect("hits array");
    assert!(hits.is_empty(), "hits must be empty for empty fixture");
    assert_eq!(
        env["vectors_empty"].as_bool(),
        Some(true),
        "vectors_empty must be true for 0-document store"
    );

    // Check stderr note.
    let stderr = String::from_utf8(q_out.stderr.clone()).expect("stderr UTF-8");
    assert!(
        stderr.contains("semantic index is empty"),
        "stderr must contain 'semantic index is empty'; got: {stderr:?}"
    );

    // 3b. Populated fixture: vectors_empty must be ABSENT (not false, ABSENT).
    let tmp = fixture_copy();
    let root2 = tmp.path();
    let init2 = run_in(root2, &["init"]);
    assert!(init2.status.success(), "init on populated dir failed: {:?}", init2);

    let q2 = run_in(root2, &["query", SEEDED_QUERY]);
    assert!(q2.status.success(), "query on populated dir failed: {:?}", q2);

    let env2 = parse_single_json(&q2);
    assert!(
        env2.get("vectors_empty").is_none(),
        "vectors_empty must be ABSENT (not false, not null) on a populated store; \
         found: {:?}",
        env2.get("vectors_empty")
    );
}

// ── Scenario 4: schema migration regression guard ────────────────────────────

#[test]
fn s4_v3_to_v4_schema_migration_via_query() {
    let tmp = fixture_copy();
    let root = tmp.path();

    // Full init to populate everything with schema v4.
    let init_out = run_in(root, &["init"]);
    assert!(init_out.status.success(), "init failed: {:?}", init_out);

    // Downgrade schema_version to '3' directly, simulating a wild v3 store.
    // Keep all document rows so we can detect whether a re-embed happened.
    {
        let conn = open_vectors_db(root);
        conn.execute(
            "UPDATE vec_meta SET value = '3' WHERE key = 'schema_version'",
            [],
        )
        .expect("downgrade schema_version to 3");
        // Also clear embedded_files (they won't exist in a real v3 store).
        // The table exists because we created the store with v4 schema, but
        // a real v3 store wouldn't have it. We can't drop it here easily without
        // sqlite's limited ALTER TABLE, so instead we rely on is_current_schema()
        // returning false → delete_store → full re-embed path.
    }

    // Run a QUERY (not init) — must self-heal: schema mismatch detected by
    // has_pending (which calls is_current_schema → false for v3) → stale →
    // index::run → delete_store + full re-embed.
    let q_out = run_in(root, &["query", SEEDED_QUERY]);
    assert!(
        q_out.status.success(),
        "query after v3 schema downgrade failed: {:?}",
        q_out
    );

    let env = parse_single_json(&q_out);
    assert_eq!(
        env["freshness"].as_str().unwrap_or(""),
        "refreshed",
        "freshness must be 'refreshed' — schema v3 must trigger a self-heal"
    );

    // Verify store is back on the current schema (v5) with D documents.
    // SCHEMA_VERSION in vectors.rs is "5"; a v3 store is deleted and rewritten.
    let conn = open_vectors_db(root);
    let sv = schema_version(&conn);
    assert_eq!(sv, "5", "schema_version must be the current schema after migration");

    let count = doc_count(&conn);
    assert_eq!(count, D as i64, "documents must be D={D} after v3→v4 migration");
}
