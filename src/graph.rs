// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! SparrowDB-backed property graph store at `.lattice/graph.sparrow/`.
//!
//! Pure-Rust embedded graph DB with Cypher (fixed decision; see AGENTS.md).
//! `refresh` persists the resolved graph; `impact` queries the indexed graph.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use sparrowdb::{GraphDb, NodeId, Value as StorageValue};
use sparrowdb_execution::types::Value;

use crate::ingest::{CallEvidence, CallSite, Graph};
use crate::Result;

pub const GRAPH_DIR: &str = "graph.sparrow";

/// Encode a string property as `Value::Bytes(utf8)` — load-bearing encoding
/// parity with the Cypher write path (`literal_to_value`: String → Bytes).
/// The read path's `storage_value_to_exec` decodes Bytes → exec::Value::String.
/// Must NOT type-sniff; always Bytes, no exceptions.
#[inline]
fn s(v: &str) -> StorageValue {
    StorageValue::Bytes(v.as_bytes().to_vec())
}


/// Refresh the persisted graph for the current tree.
///
/// Rebuild the graph store from the fully resolved graph. SparrowDB 0.1.16
/// lacks `DETACH DELETE`, and its checkpointed adjacency index can retain edge
/// references that prevent in-place node deletion.
///
/// **Write path (scalable-ingestion slice):** uses SparrowDB's native `WriteTx`
/// batch API — one transaction for the entire graph. This eliminates the
/// O(E·N) MATCH-label-scan per edge and the per-mutation WAL fsync that made
/// the old Cypher-per-mutation path super-linear above ~100 files. Complexity
/// is now O(N + E) map lookups + one commit + one checkpoint.
///
/// Encoding parity: string properties stored as `Value::Bytes(utf8)` (same as
/// the Cypher `literal_to_value` path); `start_line` as `Value::Int64`.
/// The read path's `storage_value_to_exec` decodes identically from both.
///
/// This preserves every *observable* guarantee of the frozen test contract:
/// `impact` sorts and de-duplicates via `BTreeSet`, so its output is a pure
/// deterministic function of the source tree. Unchanged files therefore yield
/// byte-identical impact envelopes across a refresh, and the changed file's new
/// edges appear — exactly what Scenarios 3 and 4 assert. The hash-diff still
/// drives the additive `files_indexed/reindexed/unchanged` counters and the
/// per-file meta.db / vectors.db incrementality (see `main`, `meta::update`,
/// `vectors::refresh`). `reindex`/`removed` are accepted for signature symmetry
/// with those stores but are not needed to rebuild a deterministic graph.
pub fn refresh(
    lattice_dir: &Path,
    graph: &Graph,
    _reindex: &[String],
    _removed: &[String],
) -> Result<()> {
    let dir = lattice_dir.join(GRAPH_DIR);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    let db = GraphDb::open(&dir).map_err(|e| format!("open graph.sparrow: {e}"))?;

    // ONE write transaction for the whole graph build.
    // begin_write is a try-lock; cannot fail on a freshly created, single-threaded handle.
    let mut tx = db
        .begin_write()
        .map_err(|e| format!("graph batch begin_write: {e}"))?;

    // Pre-register all three labels in fixed order (File, Module, Function).
    // Order must match first-use order of the old Cypher path so label_ids —
    // and therefore NodeIds (= label_id<<32 | slot) — are identical.
    let l_file = tx
        .get_or_create_label_id("File")
        .map_err(|e| format!("graph batch label File: {e}"))?;
    let l_module = tx
        .get_or_create_label_id("Module")
        .map_err(|e| format!("graph batch label Module: {e}"))?;
    let l_function = tx
        .get_or_create_label_id("Function")
        .map_err(|e| format!("graph batch label Function: {e}"))?;

    // String-keyed NodeId maps replace all MATCH lookups.
    // Keys: File → file path; Function → function id.
    let mut file_ids: HashMap<String, NodeId> = HashMap::with_capacity(graph.files.len());
    let mut fn_ids: HashMap<String, NodeId> = HashMap::with_capacity(graph.functions.len());

    // 1a. File nodes
    for (path, language) in &graph.files {
        let nid = tx
            .create_node_named(l_file, &[
                ("path".to_string(),     s(path)),
                ("language".to_string(), s(language)),
            ])
            .map_err(|e| format!("graph batch create File: {e}"))?;
        file_ids.insert(path.clone(), nid);
    }

    // 1b. Module nodes + contains edges (file → module)
    for (id, name, path) in &graph.modules {
        let mid = tx
            .create_node_named(l_module, &[
                ("id".to_string(),   s(id)),
                ("name".to_string(), s(name)),
                ("path".to_string(), s(path)),
            ])
            .map_err(|e| format!("graph batch create Module: {e}"))?;
        // Replicate MATCH-no-row no-op: silently skip if file node absent.
        // Use uppercase "CONTAINS" to match what the Cypher CREATE path emits:
        // SparrowDB's tokenizer maps the `CONTAINS` operator-keyword to
        // uppercase when it appears as a rel-type literal in a pattern.
        if let Some(&fid) = file_ids.get(path.as_str()) {
            tx.create_edge(fid, mid, "CONTAINS", HashMap::new())
                .map_err(|e| format!("graph batch contains (module): {e}"))?;
        }
    }

    // 1c. Function nodes + contains edges (file → function)
    for (id, name, path, start_line, _parent) in &graph.functions {
        let fun = tx
            .create_node_named(l_function, &[
                ("id".to_string(),         s(id)),
                ("name".to_string(),       s(name)),
                ("path".to_string(),       s(path)),
                ("start_line".to_string(), StorageValue::Int64(*start_line as i64)),
            ])
            .map_err(|e| format!("graph batch create Function: {e}"))?;
        fn_ids.insert(id.clone(), fun);
        if let Some(&fid) = file_ids.get(path.as_str()) {
            tx.create_edge(fid, fun, "CONTAINS", HashMap::new())
                .map_err(|e| format!("graph batch contains (function): {e}"))?;
        }
    }

    // 2. Call edges (function → function) — O(1) map lookup, no MATCH scan.
    for (from, to, evidence, _site) in &graph.calls {
        if let (Some(&a), Some(&b)) = (fn_ids.get(from.as_str()), fn_ids.get(to.as_str())) {
            let mut props = HashMap::new();
            props.insert("evidence".to_string(), s(evidence.as_str()));
            tx.create_edge(a, b, "calls", props)
                .map_err(|e| format!("graph batch calls edge: {e}"))?;
        }
        // Unresolved endpoint → silent skip (parity with empty MATCH no-op).
    }

    // 3. Import edges (file → file)
    for (from, to) in &graph.imports {
        if let (Some(&a), Some(&b)) = (file_ids.get(from.as_str()), file_ids.get(to.as_str())) {
            tx.create_edge(a, b, "imports", HashMap::new())
                .map_err(|e| format!("graph batch imports edge: {e}"))?;
        }
    }

    // ONE WAL fsync for the entire graph.
    // commit(self) drops the write guard before checkpoint re-acquires it — no deadlock.
    tx.commit()
        .map_err(|e| format!("graph batch commit: {e}"))?;

    db.checkpoint()
        .map_err(|e| format!("checkpoint graph.sparrow: {e}"))?;
    write_call_evidence(lattice_dir, &graph.calls)?;
    write_call_sites(lattice_dir, &graph.calls)?;
    Ok(())
}

/// Replace the call-evidence side table in `meta.db`.
///
/// Sparrow stores `evidence` on the edge, but 0.1.16's Cypher reader does not
/// return relationship properties, so queries read this table. One row per
/// distinct (caller, callee); duplicate call sites keep the stronger level.
fn write_call_sites(
    lattice_dir: &Path,
    calls: &[(String, String, CallEvidence, CallSite)],
) -> Result<()> {
    // One row per directed edge. Duplicate call sites keep the earliest span
    // so the evidence a query returns is stable.
    let mut best: BTreeMap<(&str, &str), &CallSite> = BTreeMap::new();
    for (from, to, _, site) in calls {
        best.entry((from.as_str(), to.as_str()))
            .and_modify(|cur| {
                if site.start_line > 0
                    && (cur.start_line == 0 || site.start_line < cur.start_line)
                {
                    *cur = site;
                }
            })
            .or_insert(site);
    }
    let conn = rusqlite::Connection::open(lattice_dir.join(crate::meta::META_DB))
        .map_err(|e| format!("open meta.db for call sites: {e}"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS call_sites (
            from_id TEXT NOT NULL,
            to_id TEXT NOT NULL,
            path TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            PRIMARY KEY (from_id, to_id)
        );",
    )
    .map_err(|e| format!("create call_sites: {e}"))?;
    conn.execute("DELETE FROM call_sites", [])
        .map_err(|e| format!("clear call_sites: {e}"))?;
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("call_sites tx: {e}"))?;
    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO call_sites (from_id, to_id, path, start_line, end_line)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|e| format!("prepare call_sites insert: {e}"))?;
        for ((from, to), site) in best {
            stmt.execute((from, to, &site.path, site.start_line as i64, site.end_line as i64))
                .map_err(|e| format!("insert call_sites: {e}"))?;
        }
    }
    tx.commit()
        .map_err(|e| format!("commit call_sites: {e}"))?;
    Ok(())
}

fn read_call_sites(lattice_dir: &Path) -> Result<BTreeMap<(String, String), CallSite>> {
    let path = lattice_dir.join(crate::meta::META_DB);
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let conn = rusqlite::Connection::open(&path)
        .map_err(|e| format!("open meta.db for call sites: {e}"))?;
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='call_sites'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        return Ok(BTreeMap::new());
    }
    let mut stmt = conn
        .prepare("SELECT from_id, to_id, path, start_line, end_line FROM call_sites")
        .map_err(|e| format!("prepare call_sites scan: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                CallSite {
                    path: row.get(2)?,
                    start_line: row.get::<_, i64>(3)?.max(0) as usize,
                    end_line: row.get::<_, i64>(4)?.max(0) as usize,
                },
            ))
        })
        .map_err(|e| format!("scan call_sites: {e}"))?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (from, to, site) = row.map_err(|e| format!("read call_sites row: {e}"))?;
        out.insert((from, to), site);
    }
    Ok(out)
}

fn write_call_evidence(
    lattice_dir: &Path,
    calls: &[(String, String, CallEvidence, CallSite)],
) -> Result<()> {
    let mut best: BTreeMap<(&str, &str), CallEvidence> = BTreeMap::new();
    for (from, to, evidence, _) in calls {
        best.entry((from.as_str(), to.as_str()))
            .and_modify(|cur| {
                if evidence.rank() > cur.rank() {
                    *cur = *evidence;
                }
            })
            .or_insert(*evidence);
    }
    let conn = rusqlite::Connection::open(lattice_dir.join(crate::meta::META_DB))
        .map_err(|e| format!("open meta.db for call evidence: {e}"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS call_evidence (
            from_id TEXT NOT NULL,
            to_id TEXT NOT NULL,
            evidence TEXT NOT NULL,
            PRIMARY KEY (from_id, to_id)
        );",
    )
    .map_err(|e| format!("create call_evidence: {e}"))?;
    conn.execute("DELETE FROM call_evidence", [])
        .map_err(|e| format!("clear call_evidence: {e}"))?;
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("call_evidence tx: {e}"))?;
    {
        let mut stmt = tx
            .prepare("INSERT INTO call_evidence (from_id, to_id, evidence) VALUES (?1, ?2, ?3)")
            .map_err(|e| format!("prepare call_evidence insert: {e}"))?;
        for ((from, to), evidence) in best {
            stmt.execute((from, to, evidence.as_str()))
                .map_err(|e| format!("insert call_evidence: {e}"))?;
        }
    }
    tx.commit()
        .map_err(|e| format!("commit call_evidence: {e}"))?;
    Ok(())
}

fn read_call_evidence(lattice_dir: &Path) -> Result<BTreeMap<(String, String), CallEvidence>> {
    let path = lattice_dir.join(crate::meta::META_DB);
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let conn = rusqlite::Connection::open(&path)
        .map_err(|e| format!("open meta.db for call evidence: {e}"))?;
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='call_evidence'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        return Ok(BTreeMap::new());
    }
    let mut stmt = conn
        .prepare("SELECT from_id, to_id, evidence FROM call_evidence")
        .map_err(|e| format!("prepare call_evidence scan: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| format!("scan call_evidence: {e}"))?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (from, to, evidence) = row.map_err(|e| format!("read call_evidence row: {e}"))?;
        if let Some(level) = CallEvidence::parse(&evidence) {
            out.insert((from, to), level);
        }
    }
    Ok(out)
}

pub struct Impact {
    pub callers: Vec<String>,
    pub dependents: Vec<String>,
    pub blast_radius: usize,
}

/// A read-back view of the persisted graph, used by the OKF renderer so that
/// `export`/`summarize` derive purely from the graph store (never re-parse
/// sources). All vectors are sorted lexicographically for determinism.
pub struct GraphView {
    /// (path, language), sorted by path.
    pub files: Vec<(String, String)>,
    /// (name, path), sorted by (path, name).
    pub functions: Vec<(String, String)>,
    /// File-path -> File-path imports edges, sorted.
    pub imports: Vec<(String, String)>,
}

impl GraphView {
    /// Function names contained by `file_path`, sorted.
    pub fn functions_in(&self, file_path: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .functions
            .iter()
            .filter(|(_, p)| p == file_path)
            .map(|(n, _)| n.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// File paths that `file_path` imports (its out-edges), sorted.
    pub fn imports_of(&self, file_path: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .imports
            .iter()
            .filter(|(from, _)| from == file_path)
            .map(|(_, to)| to.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// Read the persisted graph back out of `graph.sparrow/` for rendering.
pub fn read(lattice_dir: &Path) -> Result<GraphView> {
    let dir = lattice_dir.join(GRAPH_DIR);
    let db = GraphDb::open(&dir).map_err(|e| format!("open graph.sparrow: {e}"))?;

    let mut files: Vec<(String, String)> = Vec::new();
    let res = db
        .execute("MATCH (f:File) RETURN f.path, f.language")
        .map_err(|e| format!("query File nodes: {e}"))?;
    for row in &res.rows {
        if let [Value::String(path), Value::String(lang)] = row.as_slice() {
            files.push((path.clone(), lang.clone()));
        }
    }
    files.sort();
    files.dedup();

    let mut functions: Vec<(String, String)> = Vec::new();
    let res = db
        .execute("MATCH (fun:Function) RETURN fun.name, fun.path")
        .map_err(|e| format!("query Function nodes: {e}"))?;
    for row in &res.rows {
        if let [Value::String(name), Value::String(path)] = row.as_slice() {
            functions.push((name.clone(), path.clone()));
        }
    }
    functions.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    functions.dedup();

    let mut imports: Vec<(String, String)> = Vec::new();
    let res = db
        .execute("MATCH (a:File)-[:imports]->(b:File) RETURN a.path, b.path")
        .map_err(|e| format!("query imports edges: {e}"))?;
    for row in &res.rows {
        if let [Value::String(from), Value::String(to)] = row.as_slice() {
            imports.push((from.clone(), to.clone()));
        }
    }
    imports.sort();
    imports.dedup();

    Ok(GraphView {
        files,
        functions,
        imports,
    })
}

// ── Symbol-level structural query layer ──────────────────────────────────────

/// A single Function node as stored in the graph.
#[derive(Debug, Clone)]
pub struct SymbolNode {
    pub id: String,
    pub name: String,
    pub path: String,
    pub line: usize,
    /// Enclosing class/impl type (None for free functions). Derived from `id`
    /// at load time: `id` has form `path::Parent::name` (≥3 `::` segments with
    /// `/` in the path segment → middle segment is parent). The `name` property
    /// stays bare for envelope compatibility.
    pub parent: Option<String>,
}

/// The in-memory call graph (loaded once per dispatch, filtered in Rust).
pub struct CallGraph {
    /// id -> node, BTreeMap for deterministic iteration.
    pub nodes: BTreeMap<String, SymbolNode>,
    /// caller-id -> BTreeSet<callee-id>
    pub out: BTreeMap<String, BTreeSet<String>>,
    /// callee-id -> BTreeSet<caller-id>
    pub inn: BTreeMap<String, BTreeSet<String>>,
    /// (caller-id, callee-id) -> evidence. Missing entries are treated as
    /// `name` (the weakest level) so an older store does not claim a binding.
    pub evidence: BTreeMap<(String, String), CallEvidence>,
    /// (caller-id, callee-id) -> call-expression span. Surfaced only by
    /// `--context`. Missing on stores written before spans were recorded.
    pub sites: BTreeMap<(String, String), CallSite>,
}

pub const MAX_CALL_CHAIN_DEPTH: usize = 8;
pub const MAX_CHAIN_PATHS: usize = 10;

/// Coerce a SparrowDB `Value` to a line number.
/// `start_line` is stored as an integer literal and returns as `Value::Int64`.
fn coerce_line(v: &Value) -> usize {
    match v {
        Value::Int64(n) => (*n).max(0) as usize,
        Value::String(s) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// Load the full function-level call graph from `graph.sparrow/`.
/// Two Cypher statements mirror the existing `impact()`/`read()` pattern.
pub fn load_call_graph(lattice_dir: &Path) -> Result<CallGraph> {
    let dir = lattice_dir.join(GRAPH_DIR);
    let db = GraphDb::open(&dir).map_err(|e| format!("open graph.sparrow: {e}"))?;

    let mut nodes: BTreeMap<String, SymbolNode> = BTreeMap::new();
    let res = db
        .execute("MATCH (f:Function) RETURN f.id, f.name, f.path, f.start_line")
        .map_err(|e| format!("query Function nodes: {e}"))?;
    for row in &res.rows {
        if let [Value::String(id), Value::String(name), Value::String(path), line_val] =
            row.as_slice()
        {
            let line = coerce_line(line_val);
            nodes.insert(
                id.clone(),
                SymbolNode {
                    // Derive parent from id: `path::Parent::name` has the path
                    // segment (containing '/') then '::' then the parent name
                    // then '::' then the function name. Split on '::' and if
                    // there are ≥3 segments, the second-to-last is the parent.
                    parent: {
                        let segs: Vec<&str> = id.splitn(100, "::").collect();
                        // The path itself may contain '/' but never '::'.
                        // id format: "<file_path>::<name>" (free fn) or
                        //            "<file_path>::<Parent>::<name>" (method).
                        // The file path contains '/' so the first segment always
                        // contains '/'. If there are exactly 3 segments and
                        // the middle one doesn't look like a file path, it's a parent.
                        if segs.len() >= 3 {
                            let parent_candidate = segs[segs.len() - 2];
                            // File paths contain '/', parent names do not.
                            if !parent_candidate.contains('/') {
                                Some(parent_candidate.to_string())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    },
                    id: id.clone(),
                    name: name.clone(),
                    path: path.clone(),
                    line,
                },
            );
        }
    }

    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut inn: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let res = db
        .execute("MATCH (a:Function)-[:calls]->(b:Function) RETURN a.id, b.id")
        .map_err(|e| format!("query calls edges: {e}"))?;
    for row in &res.rows {
        if let [Value::String(from), Value::String(to)] = row.as_slice() {
            out.entry(from.clone())
                .or_default()
                .insert(to.clone());
            inn.entry(to.clone())
                .or_default()
                .insert(from.clone());
        }
    }

    let evidence = read_call_evidence(lattice_dir)?;
    let sites = read_call_sites(lattice_dir)?;
    Ok(CallGraph {
        nodes,
        out,
        inn,
        evidence,
        sites,
    })
}

impl CallGraph {
    /// Evidence of the directed edge `from -> to`. Missing → `name`.
    pub fn evidence_of(&self, from: &str, to: &str) -> CallEvidence {
        self.evidence
            .get(&(from.to_string(), to.to_string()))
            .copied()
            .unwrap_or(CallEvidence::Name)
    }
}

/// Resolve a symbol string to matching `SymbolNode` ids using three rules in
/// order; first rule that yields ≥1 node wins:
///   1. Exact id match (`auth.rs::validate_token`)
///   2. Qualified name (contains `::`): split at last `::`, match
///      `name == last_segment` AND file stem of `path` == preceding segment.
///   3. Bare name: `name == symbol`.
pub fn resolve_symbol<'a>(cg: &'a CallGraph, symbol: &str) -> Vec<&'a SymbolNode> {
    // Rule 1: exact id
    if let Some(node) = cg.nodes.get(symbol) {
        return vec![node];
    }

    // Rule 2: qualified name (contains `::` but not an id match)
    if symbol.contains("::") {
        let sep = symbol.rfind("::").unwrap();
        let prefix = &symbol[..sep];
        // last segment of prefix (after final `::` in prefix, or the whole prefix)
        let prefix_last = match prefix.rfind("::") {
            Some(p) => &prefix[p + 2..],
            None => prefix,
        };
        let last_seg = &symbol[sep + 2..];
        let mut matches: Vec<&SymbolNode> = cg
            .nodes
            .values()
            .filter(|n| {
                n.name == last_seg
                    && (file_stem(&n.path) == prefix_last
                        || n.parent.as_deref() == Some(prefix_last))
            })
            .collect();
        matches.sort_by(|a, b| {
            a.path.cmp(&b.path).then(a.line.cmp(&b.line)).then(a.name.cmp(&b.name))
        });
        if !matches.is_empty() {
            return matches;
        }
    }

    // Rule 3: bare name
    let mut matches: Vec<&SymbolNode> = cg
        .nodes
        .values()
        .filter(|n| n.name == symbol)
        .collect();
    matches.sort_by(|a, b| {
        a.path.cmp(&b.path).then(a.line.cmp(&b.line)).then(a.name.cmp(&b.name))
    });
    matches
}

/// Return the file stem: basename minus extension.
/// `mod.rs` special-case: use the parent directory name (per design §4).
fn file_stem(path: &str) -> &str {
    let base = match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    };
    match base.rfind('.') {
        Some(i) => &base[..i],
        None => base,
    }
}

/// Sort a slice of `SymbolNode` references deterministically by (path, line, name).
pub fn sort_nodes(nodes: &mut Vec<SymbolNode>) {
    nodes.sort_by(|a, b| {
        a.path.cmp(&b.path).then(a.line.cmp(&b.line)).then(a.name.cmp(&b.name))
    });
}

/// Direct callers of all target ids (1-hop over `inn`), excluding targets
/// themselves and edges below `min`. Each hit carries the strongest surviving
/// edge into any target.
pub fn callers_min(
    cg: &CallGraph,
    target_ids: &BTreeSet<String>,
    min: CallEvidence,
) -> Vec<(SymbolNode, CallEvidence)> {
    let mut best: BTreeMap<String, CallEvidence> = BTreeMap::new();
    for id in target_ids {
        if let Some(callers_set) = cg.inn.get(id) {
            for caller_id in callers_set {
                if target_ids.contains(caller_id) {
                    continue;
                }
                let ev = cg.evidence_of(caller_id, id);
                if !ev.meets(min) {
                    continue;
                }
                best.entry(caller_id.clone())
                    .and_modify(|cur| {
                        if ev.rank() > cur.rank() {
                            *cur = ev;
                        }
                    })
                    .or_insert(ev);
            }
        }
    }
    let mut out: Vec<(SymbolNode, CallEvidence)> = best
        .into_iter()
        .filter_map(|(id, ev)| cg.nodes.get(&id).cloned().map(|node| (node, ev)))
        .collect();
    out.sort_by(|a, b| {
        a.0.path
            .cmp(&b.0.path)
            .then(a.0.line.cmp(&b.0.line))
            .then(a.0.name.cmp(&b.0.name))
    });
    out
}

/// Direct callees of all target ids (1-hop over `out`), excluding targets
/// themselves and edges below `min`.
pub fn callees_min(
    cg: &CallGraph,
    target_ids: &BTreeSet<String>,
    min: CallEvidence,
) -> Vec<(SymbolNode, CallEvidence)> {
    let mut best: BTreeMap<String, CallEvidence> = BTreeMap::new();
    for id in target_ids {
        if let Some(callees_set) = cg.out.get(id) {
            for callee_id in callees_set {
                if target_ids.contains(callee_id) {
                    continue;
                }
                let ev = cg.evidence_of(id, callee_id);
                if !ev.meets(min) {
                    continue;
                }
                best.entry(callee_id.clone())
                    .and_modify(|cur| {
                        if ev.rank() > cur.rank() {
                            *cur = ev;
                        }
                    })
                    .or_insert(ev);
            }
        }
    }
    let mut out: Vec<(SymbolNode, CallEvidence)> = best
        .into_iter()
        .filter_map(|(id, ev)| cg.nodes.get(&id).cloned().map(|node| (node, ev)))
        .collect();
    out.sort_by(|a, b| {
        a.0.path
            .cmp(&b.0.path)
            .then(a.0.line.cmp(&b.0.line))
            .then(a.0.name.cmp(&b.0.name))
    });
    out
}

/// Transitive callers of all target ids (full reverse BFS), excluding targets.
/// Transitive callers, walking only edges that meet `min`.
pub fn transitive_callers_min(
    cg: &CallGraph,
    target_ids: &BTreeSet<String>,
    min: CallEvidence,
) -> Vec<SymbolNode> {
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<String> = target_ids.iter().cloned().collect();
    // seed visited with targets so they are excluded from results
    for id in target_ids {
        visited.insert(id.clone());
    }

    let mut result: BTreeSet<String> = BTreeSet::new();
    while let Some(current) = queue.pop() {
        if let Some(callers_set) = cg.inn.get(&current) {
            for caller_id in callers_set {
                if !cg.evidence_of(caller_id, &current).meets(min) {
                    continue;
                }
                if visited.insert(caller_id.clone()) {
                    result.insert(caller_id.clone());
                    queue.push(caller_id.clone());
                }
            }
        }
    }

    let mut out: Vec<SymbolNode> = result
        .iter()
        .filter_map(|id| cg.nodes.get(id).cloned())
        .collect();
    sort_nodes(&mut out);
    out
}

/// BFS call-chain paths from any `from_ids` to any `to_ids`.
/// Returns up to `MAX_CHAIN_PATHS` shortest paths as node sequences.
pub fn call_chain_min(
    cg: &CallGraph,
    from_ids: &BTreeSet<String>,
    to_ids: &BTreeSet<String>,
    min: CallEvidence,
) -> Vec<Vec<SymbolNode>> {
    // Each queue item: (current-id, path-of-ids-so-far)
    let mut queue: std::collections::VecDeque<(String, Vec<String>)> = std::collections::VecDeque::new();
    for id in from_ids {
        if to_ids.contains(id) {
            // from == to: trivial single-node "path"
            if let Some(node) = cg.nodes.get(id) {
                return vec![vec![node.clone()]];
            }
        }
        queue.push_back((id.clone(), vec![id.clone()]));
    }

    let mut results: Vec<Vec<SymbolNode>> = Vec::new();
    // visited per source to prevent cycles: keyed by (node-id, path-length)
    // Use BTreeSet keyed by node id (a node appears at most once per path)
    let mut found_depth: Option<usize> = None;

    while let Some((current, path)) = queue.pop_front() {
        if path.len() > MAX_CALL_CHAIN_DEPTH + 1 {
            continue;
        }
        // If we found paths at depth D already, stop exploring longer ones
        if let Some(d) = found_depth {
            if path.len() > d + 1 {
                break;
            }
        }

        if let Some(callees_set) = cg.out.get(&current) {
            for callee_id in callees_set {
                if !cg.evidence_of(&current, callee_id).meets(min) {
                    continue;
                }
                // prevent cycles within this path
                if path.contains(callee_id) {
                    continue;
                }
                let mut new_path = path.clone();
                new_path.push(callee_id.clone());

                if to_ids.contains(callee_id) {
                    // Found a path
                    let node_path: Vec<SymbolNode> = new_path
                        .iter()
                        .filter_map(|id| cg.nodes.get(id).cloned())
                        .collect();
                    found_depth = Some(new_path.len() - 1);
                    results.push(node_path);
                    if results.len() >= MAX_CHAIN_PATHS {
                        return results;
                    }
                } else if new_path.len() <= MAX_CALL_CHAIN_DEPTH + 1 {
                    queue.push_back((callee_id.clone(), new_path));
                }
            }
        }
    }

    results
}

/// Fixed impact semantics (reused by every language slice):
/// callers    = distinct File.path of functions with a `calls` edge into any
///              function contained by the target file, excluding the target;
/// dependents = distinct File.path with `imports` -> target, excluding target;
/// blast_radius = |callers ∪ dependents|.
/// Both arrays sorted lexicographically ascending (determinism guarantee).
pub fn impact(lattice_dir: &Path, target: &str) -> Result<Impact> {
    impact_min(lattice_dir, target, CallEvidence::Name)
}

/// File-scope impact. `min == Name` uses the unfiltered Cypher scan so the
/// default envelope stays identical. A stricter minimum drops call edges
/// below that level; import dependents are unchanged.
pub fn impact_min(lattice_dir: &Path, target: &str, min: CallEvidence) -> Result<Impact> {
    if min == CallEvidence::Name {
        return impact_unfiltered(lattice_dir, target);
    }
    let cg = load_call_graph(lattice_dir)?;
    let mut callers: BTreeSet<String> = BTreeSet::new();
    for (from, tos) in &cg.out {
        let Some(from_node) = cg.nodes.get(from) else {
            continue;
        };
        if from_node.path == target {
            continue;
        }
        for to in tos {
            let Some(to_node) = cg.nodes.get(to) else {
                continue;
            };
            if to_node.path == target && cg.evidence_of(from, to).meets(min) {
                callers.insert(from_node.path.clone());
            }
        }
    }
    let unfiltered = impact_unfiltered(lattice_dir, target)?;
    let dependents: BTreeSet<String> = unfiltered.dependents.into_iter().collect();
    let blast: BTreeSet<&String> = callers.union(&dependents).collect();
    Ok(Impact {
        blast_radius: blast.len(),
        callers: callers.into_iter().collect(),
        dependents: dependents.into_iter().collect(),
    })
}

fn impact_unfiltered(lattice_dir: &Path, target: &str) -> Result<Impact> {
    let dir = lattice_dir.join(GRAPH_DIR);
    let db = GraphDb::open(&dir).map_err(|e| format!("open graph.sparrow: {e}"))?;

    let mut callers: BTreeSet<String> = BTreeSet::new();
    let result = db
        .execute("MATCH (a:Function)-[:calls]->(b:Function) RETURN a.path, b.path")
        .map_err(|e| format!("query calls edges: {e}"))?;
    for row in &result.rows {
        if let [Value::String(from), Value::String(to)] = row.as_slice() {
            if to == target && from != target {
                callers.insert(from.clone());
            }
        }
    }

    let mut dependents: BTreeSet<String> = BTreeSet::new();
    let result = db
        .execute("MATCH (a:File)-[:imports]->(b:File) RETURN a.path, b.path")
        .map_err(|e| format!("query imports edges: {e}"))?;
    for row in &result.rows {
        if let [Value::String(from), Value::String(to)] = row.as_slice() {
            if to == target && from != target {
                dependents.insert(from.clone());
            }
        }
    }

    let blast: BTreeSet<&String> = callers.union(&dependents).collect();
    Ok(Impact {
        blast_radius: blast.len(),
        callers: callers.into_iter().collect(),
        dependents: dependents.into_iter().collect(),
    })
}
