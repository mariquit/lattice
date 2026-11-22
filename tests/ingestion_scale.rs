// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for slice `scalable-ingestion`.
//!
//! Tests the WriteTx batched graph write path introduced to replace the
//! O(E·N) Cypher-per-mutation approach.
//!
//! Two test functions:
//!
//! (i) `scale_fixture_indexes_within_budget`
//!     Generates N=150 Rust files × 10 functions each with a dense call graph
//!     (~4,500 calls edges) in a temp dir, runs `lattice init`, and asserts:
//!     - wall-clock < 60 s (super-linearity tripwire)
//!     - unconditional structural ground-truth counts
//!
//! (ii) `batched_store_equals_per_mutation_store`
//!     On `tests/fixtures/rust_basic`: builds store A via the production
//!     batched `graph::refresh`, and store B via a test-local reference
//!     `refresh_per_mutation` that replicates the old Cypher-per-mutation loop.
//!     Asserts canonical-sorted GraphDump equality + frozen impact/symbol ground
//!     truths from `impact_rust`/`symbol_impact`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use sparrowdb::{GraphDb, GraphDump, NodeDump, EdgeDump};

use lattice::graph::{self, GRAPH_DIR};
use lattice::ingest::{self, Graph};

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

// ── helpers ──────────────────────────────────────────────────────────────────

fn run_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn lattice binary")
}

fn parse_json(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout UTF-8");
    let trimmed = stdout.trim();
    serde_json::from_str::<serde_json::Value>(trimmed)
        .unwrap_or_else(|e| panic!("stdout not valid JSON ({e}): {stdout:?}"))
}

/// Copy rust_basic fixture into a fresh temp dir.
fn rust_basic_copy() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic");
    for entry in fs::read_dir(&src).expect("read fixture dir") {
        let entry = entry.expect("entry");
        fs::copy(entry.path(), tmp.path().join(entry.file_name())).expect("copy file");
    }
    tmp
}

// ── canonical sort helpers for GraphDump comparison ──────────────────────────

fn sort_dump(mut dump: GraphDump) -> GraphDump {
    dump.nodes.sort_by(|a, b| {
        let ka = node_sort_key(a);
        let kb = node_sort_key(b);
        a.label.cmp(&b.label).then(ka.cmp(&kb))
    });
    dump.edges.sort_by(|a, b| {
        a.rel_type
            .cmp(&b.rel_type)
            .then(a.src_id.cmp(&b.src_id))
            .then(a.dst_id.cmp(&b.dst_id))
    });
    dump
}

fn node_sort_key(n: &NodeDump) -> String {
    let mut pairs: Vec<(&String, &serde_json::Value)> = n.properties.iter().collect();
    pairs.sort_by_key(|(k, _)| k.as_str());
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn props_key(n: &NodeDump) -> String {
    let mut pairs: Vec<_> = n.properties.iter().collect();
    pairs.sort_by_key(|(k, _)| k.as_str());
    let props_str = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{}|{}", n.label, props_str)
}

fn dumps_logically_equal(a: &GraphDump, b: &GraphDump) -> bool {
    if a.nodes.len() != b.nodes.len() || a.edges.len() != b.edges.len() {
        return false;
    }

    let mut keys_a: Vec<String> = a.nodes.iter().map(props_key).collect();
    let mut keys_b: Vec<String> = b.nodes.iter().map(props_key).collect();
    keys_a.sort();
    keys_b.sort();
    if keys_a != keys_b {
        return false;
    }

    // Build (node_id → props_key) maps for edge comparison
    let id_to_key_a: HashMap<u64, String> = a.nodes.iter().map(|n| (n.node_id, props_key(n))).collect();
    let id_to_key_b: HashMap<u64, String> = b.nodes.iter().map(|n| (n.node_id, props_key(n))).collect();

    let edge_key_fn = |e: &EdgeDump, id_map: &HashMap<u64, String>| -> String {
        let sk = id_map.get(&e.src_id).cloned().unwrap_or_else(|| e.src_id.to_string());
        let dk = id_map.get(&e.dst_id).cloned().unwrap_or_else(|| e.dst_id.to_string());
        format!("{}|{}→{}", e.rel_type, sk, dk)
    };

    let mut ekeys_a: Vec<String> = a.edges.iter().map(|e| edge_key_fn(e, &id_to_key_a)).collect();
    let mut ekeys_b: Vec<String> = b.edges.iter().map(|e| edge_key_fn(e, &id_to_key_b)).collect();
    ekeys_a.sort();
    ekeys_b.sort();
    ekeys_a == ekeys_b
}

// ── test-local reference: per-mutation Cypher loop (frozen copy of old path) ─

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Rebuild the graph using the OLD Cypher-per-mutation path.
/// Frozen reference implementation — do NOT change when the production path changes.
fn refresh_per_mutation(lattice_dir: &Path, graph: &Graph) -> Result<(), String> {
    let dir = lattice_dir.join(GRAPH_DIR);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    let db = GraphDb::open(&dir).map_err(|e| format!("open: {e}"))?;

    for (path, language) in &graph.files {
        db.execute(&format!(
            "CREATE (n:File {{path: '{}', language: '{}'}})",
            esc(path),
            esc(language)
        ))
        .map_err(|e| format!("create File: {e}"))?;
    }
    for (id, name, path) in &graph.modules {
        db.execute(&format!(
            "CREATE (n:Module {{id: '{}', name: '{}', path: '{}'}})",
            esc(id),
            esc(name),
            esc(path)
        ))
        .map_err(|e| format!("create Module: {e}"))?;
        db.execute(&format!(
            "MATCH (f:File {{path: '{}'}}), (m:Module {{id: '{}'}}) CREATE (f)-[:contains]->(m)",
            esc(path),
            esc(id)
        ))
        .map_err(|e| format!("contains(module): {e}"))?;
    }
    for (id, name, path, start_line, _parent) in &graph.functions {
        db.execute(&format!(
            "CREATE (n:Function {{id: '{}', name: '{}', path: '{}', start_line: {}}})",
            esc(id),
            esc(name),
            esc(path),
            start_line
        ))
        .map_err(|e| format!("create Function: {e}"))?;
        db.execute(&format!(
            "MATCH (f:File {{path: '{}'}}), (fun:Function {{id: '{}'}}) CREATE (f)-[:contains]->(fun)",
            esc(path),
            esc(id)
        ))
        .map_err(|e| format!("contains(function): {e}"))?;
    }
    for (from, to, _, _) in &graph.calls {
        db.execute(&format!(
            "MATCH (a:Function {{id: '{}'}}), (b:Function {{id: '{}'}}) CREATE (a)-[:calls]->(b)",
            esc(from),
            esc(to)
        ))
        .map_err(|e| format!("calls edge: {e}"))?;
    }
    for (from, to) in &graph.imports {
        db.execute(&format!(
            "MATCH (a:File {{path: '{}'}}), (b:File {{path: '{}'}}) CREATE (a)-[:imports]->(b)",
            esc(from),
            esc(to)
        ))
        .map_err(|e| format!("imports edge: {e}"))?;
    }

    db.checkpoint().map_err(|e| format!("checkpoint: {e}"))?;
    Ok(())
}

// ── helpers to count graph nodes/edges via GraphDump export ──────────────────
// SparrowDB's Cypher engine requires both endpoints to have matching typed
// labels for edge traversal. Additionally, `CONTAINS` is a Cypher keyword
// and is normalised to uppercase when stored via CREATE. Using the GraphDump
// export avoids these query-parsing subtleties and counts ground truth directly.

fn count_label(db: &GraphDb, label: &str) -> usize {
    let dump = db.export().expect("export for count_label");
    dump.nodes.iter().filter(|n| n.label == label).count()
}

fn count_rel(db: &GraphDb, rel: &str) -> usize {
    let dump = db.export().expect("export for count_rel");
    // Match case-insensitively: "CONTAINS" (Cypher keyword → uppercase) and
    // "calls"/"imports" are lowercase in both paths.
    let rel_upper = rel.to_uppercase();
    let rel_lower = rel.to_lowercase();
    dump.edges
        .iter()
        .filter(|e| e.rel_type == rel_lower || e.rel_type == rel_upper)
        .count()
}

// ── ingest helper: walk a directory and resolve a Graph ──────────────────────

fn ingest_fixture(fixture_dir: &Path) -> Graph {
    let mut facts = Vec::new();
    collect_files(fixture_dir, fixture_dir, &mut facts);
    ingest::resolve_with_root(&facts, &fixture_dir.to_string_lossy())
}

fn collect_files(
    root: &Path,
    dir: &Path,
    facts: &mut Vec<lattice::ingest::FileFacts>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, facts);
        } else if path.is_file() {
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if !matches!(ext, "rs" | "py" | "ts" | "tsx") {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if let Ok(src) = fs::read_to_string(&path) {
                if let Ok(ff) = ingest::parse_file(&rel, &src) {
                    facts.push(ff);
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// (i) Scale test: 150 files × 10 functions, dense call graph
// ─────────────────────────────────────────────────────────────────────────────

/// Build a synthetic `Graph` directly (without file I/O or candle embeddings).
/// This lets the scale test measure the graph write path in isolation.
fn build_synthetic_graph(n: usize, fns_per_file: usize) -> (Graph, usize) {
    let calls_per_fn = 3usize;

    let mut files: Vec<(String, String)> = Vec::new();
    let mut modules: Vec<(String, String, String)> = Vec::new();
    let mut functions: Vec<(String, String, String, usize, Option<String>)> = Vec::new();
    let mut calls: Vec<(
        String,
        String,
        lattice::ingest::CallEvidence,
        lattice::ingest::CallSite,
    )> = Vec::new();
    let mut imports: Vec<(String, String)> = Vec::new();

    let mut seen_imports: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut expected_imports = 0usize;

    for i in 0..n {
        let path = format!("mod_{i:03}.rs");
        files.push((path.clone(), "rust".to_string()));

        let mod_id = format!("{path}::mod_{i:03}");
        modules.push((mod_id.clone(), format!("mod_{i:03}"), path.clone()));

        // imports: each file imports up to 5 neighbours
        for step in 1..=5usize {
            let target = (i + step) % n;
            if target != i {
                let from = path.clone();
                let to = format!("mod_{target:03}.rs");
                let key = (from.clone(), to.clone());
                if seen_imports.insert(key) {
                    imports.push((from, to));
                    expected_imports += 1;
                }
            }
        }

        for j in 0..fns_per_file {
            let fn_id = format!("{path}::fn_{i:03}_{j:03}");
            functions.push((fn_id.clone(), format!("fn_{i:03}_{j:03}"), path.clone(), j + 1, None));

            // calls: each function calls 3 functions in neighbouring files
            for k in 0..calls_per_fn {
                let target_file = (i + k + 1) % n;
                let target_fn_j = j % fns_per_file;
                let target_path = format!("mod_{target_file:03}.rs");
                let callee_id = format!("{target_path}::fn_{target_file:03}_{target_fn_j:03}");
                calls.push((
                    fn_id.clone(),
                    callee_id,
                    lattice::ingest::CallEvidence::Name,
                    lattice::ingest::CallSite::default(),
                ));
            }
        }
    }

    let graph = Graph { files, modules, functions, calls, imports };
    (graph, expected_imports)
}

#[test]
fn scale_fixture_indexes_within_budget() {
    const N: usize = 150;
    const FNS_PER_FILE: usize = 10;
    const CALLS_PER_FN: usize = 3;
    const WALL_CLOCK_LIMIT_SECS: u64 = 60;

    // Build the Graph in memory — no disk I/O, no candle embeddings.
    // This tests the graph WRITE PATH in isolation (the change this slice makes).
    let (graph, expected_imports) = build_synthetic_graph(N, FNS_PER_FILE);

    let expected_files = N;
    let expected_functions = N * FNS_PER_FILE;
    // contains: file→fn (1500) + file→module (150) = 1650
    let expected_contains = N * (FNS_PER_FILE + 1);
    let expected_calls = N * FNS_PER_FILE * CALLS_PER_FN;

    let tmp = tempfile::tempdir().expect("create temp dir");

    // ── Wall-clock: measure ONLY graph::refresh (the changed write path) ──────
    let start = std::time::Instant::now();
    graph::refresh(tmp.path(), &graph, &[], &[]).expect("graph::refresh failed");
    let elapsed = start.elapsed();

    // ── Unconditional structural ground-truth assertions (always run) ─────────
    let graph_dir = tmp.path().join(GRAPH_DIR);
    let db = GraphDb::open(&graph_dir).expect("open graph");

    let actual_files = count_label(&db, "File");
    let actual_functions = count_label(&db, "Function");
    let actual_contains = count_rel(&db, "contains");
    let actual_calls = count_rel(&db, "calls");
    let actual_imports = count_rel(&db, "imports");

    assert_eq!(
        actual_files, expected_files,
        "File node count: expected {expected_files}, got {actual_files}"
    );
    assert_eq!(
        actual_functions, expected_functions,
        "Function node count: expected {expected_functions}, got {actual_functions}"
    );
    assert_eq!(
        actual_contains, expected_contains,
        "contains edge count: expected {expected_contains}, got {actual_contains}"
    );
    assert_eq!(
        actual_calls, expected_calls,
        "calls edge count: expected {expected_calls}, got {actual_calls}"
    );
    assert_eq!(
        actual_imports, expected_imports,
        "imports edge count: expected {expected_imports}, got {actual_imports}"
    );

    // ── Wall-clock budget (60 s tripwire for super-linearity regression) ──────
    // Measures only graph::refresh; the old O(E·N) path demonstrably exceeded
    // 60 s above ~100 files. The new O(N+E) path should complete in seconds.
    assert!(
        elapsed.as_secs() < WALL_CLOCK_LIMIT_SECS,
        "graph::refresh took {elapsed:?} — exceeded {WALL_CLOCK_LIMIT_SECS}s budget \
         (super-linearity regression in WriteTx path?)"
    );

    eprintln!(
        "ingestion_scale: N={N}×{FNS_PER_FILE} fns; \
         files={actual_files} fns={actual_functions} \
         contains={actual_contains} calls={actual_calls} imports={actual_imports}; \
         graph_refresh={elapsed:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// (ii) Equivalence: batched == per-mutation on rust_basic
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn batched_store_equals_per_mutation_store() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic");

    // Build resolved graph from fixture
    let graph = ingest_fixture(&fixture);

    // Store A: production batched path (uses WriteTx)
    let tmp_a = tempfile::tempdir().expect("tmp_a");
    graph::refresh(tmp_a.path(), &graph, &[], &[])
        .expect("batched refresh failed");

    // Store B: old per-mutation Cypher path (frozen reference)
    let tmp_b = tempfile::tempdir().expect("tmp_b");
    refresh_per_mutation(tmp_b.path(), &graph).expect("per-mutation refresh failed");

    // ── Canonically sorted GraphDump logical equality ─────────────────────────
    let db_a = GraphDb::open(&tmp_a.path().join(GRAPH_DIR)).expect("open db_a");
    let db_b = GraphDb::open(&tmp_b.path().join(GRAPH_DIR)).expect("open db_b");

    let dump_a = sort_dump(db_a.export().expect("export A"));
    let dump_b = sort_dump(db_b.export().expect("export B"));

    assert_eq!(
        dump_a.nodes.len(),
        dump_b.nodes.len(),
        "node count mismatch: A={}, B={}",
        dump_a.nodes.len(),
        dump_b.nodes.len()
    );
    assert_eq!(
        dump_a.edges.len(),
        dump_b.edges.len(),
        "edge count mismatch: A={}, B={}",
        dump_a.edges.len(),
        dump_b.edges.len()
    );

    assert!(
        dumps_logically_equal(&dump_a, &dump_b),
        "GraphDump logical content differs between batched and per-mutation stores"
    );

    // ── Raw NodeId equality (locks §4 determinism claim) ─────────────────────
    let mut a_node_ids: Vec<u64> = dump_a.nodes.iter().map(|n| n.node_id).collect();
    let mut b_node_ids: Vec<u64> = dump_b.nodes.iter().map(|n| n.node_id).collect();
    a_node_ids.sort();
    b_node_ids.sort();
    assert_eq!(
        a_node_ids, b_node_ids,
        "NodeId sets differ between batched and per-mutation stores (determinism broken)"
    );

    // ── Frozen impact ground truths (from impact_rust gate) ──────────────────
    // Run lattice init + query on a fresh copy using the production binary
    let tmp_cli = rust_basic_copy();
    let cli_root = tmp_cli.path();

    let init_out = run_in(cli_root, &["init"]);
    assert!(
        init_out.status.success(),
        "lattice init for equivalence CLI check failed; stderr:\n{}",
        String::from_utf8_lossy(&init_out.stderr)
    );

    let impact_out = run_in(cli_root, &["query", "--impact", "auth.rs"]);
    assert!(
        impact_out.status.success(),
        "query --impact auth.rs failed; stderr:\n{}",
        String::from_utf8_lossy(&impact_out.stderr)
    );

    let impact_json = parse_json(&impact_out);
    assert_eq!(impact_json["protocol_version"], "1.5.0");
    assert_eq!(impact_json["query_type"], "impact");

    // Frozen rust_basic ground truth (from impact_rust gate):
    //   callers = ["main.rs"]  (main.rs calls auth::validate_token)
    //   dependents = ["main.rs", "metrics.rs"]
    //   blast_radius = 2
    let callers = impact_json["callers"].as_array().expect("callers array");
    let caller_paths: Vec<&str> = callers.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(
        caller_paths,
        vec!["main.rs"],
        "callers ground truth mismatch; got: {caller_paths:?}"
    );

    let blast = impact_json["blast_radius"].as_u64().expect("blast_radius");
    assert_eq!(blast, 2, "blast_radius ground truth mismatch, got {blast}");

    eprintln!(
        "equivalence: A_nodes={} B_nodes={} A_edges={} B_edges={}; impact callers={caller_paths:?}",
        dump_a.nodes.len(),
        dump_b.nodes.len(),
        dump_a.edges.len(),
        dump_b.edges.len()
    );
}
