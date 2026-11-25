// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! JSON envelope discipline: every machine-facing response is exactly ONE JSON
//! object on stdout carrying a top-level `protocol_version`. Human prose goes
//! to stderr only. This envelope is the contract for CLI consumers.

use serde_json::{json, Value};

use crate::fts;
use crate::graph;

/// Protocol 1.5.0 consolidates install, context, and snapshot commands.
pub const PROTOCOL_VERSION: &str = "1.5.0";

/// Print the single JSON envelope to stdout (nothing else may go to stdout).
pub fn emit(envelope: &Value) {
    println!("{envelope}");
}

/// `lattice init` success envelope. Counters are additive (protocol stays
/// "1.0.0"): `files_indexed` = total eligible source files this run;
/// `files_reindexed` = changed + new files re-parsed; `files_unchanged` =
/// files skipped via content-hash match; `files_parsed` = files actually run
/// through tree-sitter this run (the rest are served from the meta.db parse
/// fragment cache — slice 10). Invariant:
/// `files_reindexed + files_unchanged == files_indexed`; and
/// `files_parsed <= files_indexed` (equal only when the cache is cold).
pub fn init_ok(
    files_indexed: usize,
    files_reindexed: usize,
    files_unchanged: usize,
    files_parsed: usize,
) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "command": "init",
        "status": "ok",
        "files_indexed": files_indexed,
        "files_reindexed": files_reindexed,
        "files_unchanged": files_unchanged,
        "files_parsed": files_parsed,
    })
}

/// `lattice query --impact <path>` success envelope. `freshness` is the
/// additive slice-12 field: `"refreshed"` if this invocation re-indexed >= 1
/// stale file before answering, else `"current"` (protocol stays "1.0.0").
pub fn impact(
    target: &str,
    callers: &[String],
    dependents: &[String],
    blast_radius: usize,
    freshness: &str,
) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "impact",
        "target": target,
        "callers": callers,
        "dependents": dependents,
        "blast_radius": blast_radius,
        "freshness": freshness,
    })
}

/// Default semantic envelope (slice symbol-granular-embeddings, protocol 1.1.0).
/// Each hit is a function/method span: `{path, name, kind, start_line, end_line, score, class}`.
/// `hits` = (path, name, kind, start_line, end_line, score, class).
pub fn semantic_symbols(
    query: &str,
    hits: &[(String, String, String, usize, usize, f32, String)],
    freshness: &str,
) -> Value {
    let hit_values: Vec<Value> = hits
        .iter()
        .map(|(path, name, kind, start_line, end_line, score, class)| {
            json!({
                "path": path,
                "name": name,
                "kind": kind,
                "start_line": start_line,
                "end_line": end_line,
                "score": score,
                "class": class,
            })
        })
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "semantic",
        "query": query,
        "hits": hit_values,
        "freshness": freshness,
    })
}

/// File-level semantic envelope (`--file-level` flag, back-compat with pre-1.1.0 shape).
/// Each hit is: `{path, score, class}`.
/// `hits` = (path, score, class). Used when --exclude-tests or filtering
/// is in effect; `--file-level` is the opt-in back-compat flag.
pub fn semantic_with_class(query: &str, hits: &[(String, f32, String)], freshness: &str) -> Value {
    let hit_values: Vec<Value> = hits
        .iter()
        .map(|(path, score, class)| json!({ "path": path, "score": score, "class": class }))
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "semantic",
        "query": query,
        "hits": hit_values,
        "freshness": freshness,
    })
}

/// Symbol-granular semantic envelope for the empty-store case.
///
/// Emits the same shape as `semantic_symbols` with `hits: []` PLUS the
/// additive field `"vectors_empty": true` (omitted in the normal path so
/// existing byte-exact test expectations are untouched). Per the versioning
/// policy (additive field = no bump), `PROTOCOL_VERSION` stays `1.1.0`.
pub fn semantic_symbols_empty(query: &str, freshness: &str) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "semantic",
        "query": query,
        "hits": [],
        "freshness": freshness,
        "vectors_empty": true,
    })
}

/// File-level semantic envelope for the empty-store case.
///
/// Same as `semantic_with_class` with `hits: []` and `"vectors_empty": true`.
pub fn semantic_with_class_empty(query: &str, freshness: &str) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "semantic",
        "query": query,
        "hits": [],
        "freshness": freshness,
        "vectors_empty": true,
    })
}

/// `lattice query --symbol "<text>"` success envelope: lexical (FTS5) symbol
/// search. `hits` are `fts::SymbolHit` values, ordered by relevance then a
/// stable key. The `class` field is additive (slice 14). An empty `hits` array
/// is a valid success (clean miss).
pub fn symbol(
    query: &str,
    hits: &[fts::SymbolHit],
    freshness: &str,
) -> Value {
    let hit_values: Vec<Value> = hits
        .iter()
        .map(|h| {
            json!({
                "path": h.path,
                "name": h.name,
                "kind": h.kind,
                "line": h.line,
                "class": h.class,
            })
        })
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "symbol",
        "query": query,
        "hits": hit_values,
        "freshness": freshness,
    })
}

/// `path` is the model cache directory; `cached` indicates no download was needed.
pub fn install_ok(model: &str, path: &str, files: usize, cached: bool, agent: &str, wrote: &[String]) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "command": "install",
        "status": "ok",
        "model": model,
        "path": path,
        "files": files,
        "cached": cached,
        "agent": agent,
        "wrote": wrote,
    })
}

/// `lattice export` success envelope. `root` is the repo-relative OKF root;
/// `files_written` counts generated .md files (not frozen; grows with fixture).
pub fn export_ok(root: &str, files_written: usize) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "command": "export",
        "status": "ok",
        "root": root,
        "files_written": files_written,
    })
}

/// `lattice summarize <path>` success envelope; `summary` is non-empty prose.
pub fn summarize_ok(target: &str, summary: &str) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "command": "summarize",
        "target": target,
        "summary": summary,
    })
}

// ── Symbol-level structural query envelopes ───────────────────────────────────

/// A single hit/resolved entry for symbol-level queries: `{path, name, line}`.
fn symbol_node_json(path: &str, name: &str, line: usize) -> Value {
    json!({ "path": path, "name": name, "line": line })
}

fn nodes_to_json(nodes: &[(String, String, usize)]) -> Vec<Value> {
    nodes
        .iter()
        .map(|(path, name, line)| symbol_node_json(path, name, *line))
        .collect()
}

/// `query --callers "<symbol>"` success envelope.
pub fn callers(
    symbol: &str,
    resolved: &[(String, String, usize)],
    hits: &[(String, String, usize)],
    freshness: &str,
) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "callers",
        "symbol": symbol,
        "resolved": nodes_to_json(resolved),
        "hits": nodes_to_json(hits),
        "freshness": freshness,
    })
}

/// `query --callees "<symbol>"` success envelope.
pub fn callees(
    symbol: &str,
    resolved: &[(String, String, usize)],
    hits: &[(String, String, usize)],
    freshness: &str,
) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "callees",
        "symbol": symbol,
        "resolved": nodes_to_json(resolved),
        "hits": nodes_to_json(hits),
        "freshness": freshness,
    })
}

/// `query --call-chain "<from>" "<to>"` success envelope.
pub fn call_chain(
    from: &str,
    to: &str,
    resolved_from: &[(String, String, usize)],
    resolved_to: &[(String, String, usize)],
    paths: &[Vec<(String, String, usize)>],
    freshness: &str,
) -> Value {
    let path_values: Vec<Value> = paths
        .iter()
        .map(|path| {
            let nodes: Vec<Value> = path
                .iter()
                .map(|(p, n, l)| symbol_node_json(p, n, *l))
                .collect();
            Value::Array(nodes)
        })
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "call_chain",
        "from": from,
        "to": to,
        "resolved_from": nodes_to_json(resolved_from),
        "resolved_to": nodes_to_json(resolved_to),
        "max_depth": graph::MAX_CALL_CHAIN_DEPTH,
        "paths": path_values,
        "freshness": freshness,
    })
}

/// `query --impact --symbol "<name>"` success envelope.
pub fn impact_symbol(
    symbol: &str,
    resolved: &[(String, String, usize)],
    direct_callers: &[(String, String, usize)],
    transitive: &[(String, String, usize)],
    blast_radius: usize,
    freshness: &str,
) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "impact",
        "scope": "symbol",
        "symbol": symbol,
        "resolved": nodes_to_json(resolved),
        "callers": nodes_to_json(direct_callers),
        "transitive_callers": nodes_to_json(transitive),
        "blast_radius": blast_radius,
        "freshness": freshness,
    })
}

/// `lattice query --owners <path>` success envelope.
/// `owner_list` is sorted by share desc (by the caller); each entry is
/// `(author, share, last_touched)`. Non-git tree or unknown path →
/// empty lists, `churn:0`, exit 0 (never an error envelope).
pub fn owners(
    path: &str,
    owner_list: &[(String, f64, String)],
    churn: u64,
    co_changed: &[String],
    freshness: &str,
) -> Value {
    let owner_values: Vec<Value> = owner_list
        .iter()
        .map(|(author, share, last_touched)| {
            json!({
                "author": author,
                "share": share,
                "last_touched": last_touched,
            })
        })
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "owners",
        "path": path,
        "owners": owner_values,
        "churn": churn,
        "co_changed": co_changed,
        "freshness": freshness,
    })
}

// ── Code-health analytics envelopes ──────────────────────────────────────────

/// `lattice query --dead-code` success envelope.
///
/// `hits`: (path, name, line, id) tuples, sorted by (path, line, name).
/// `count`: the TRUE untruncated dead-function count (hits may be truncated).
/// The `excluded_rules` array and `note` string are protocol constants
/// (emitted inline so consumers see the heuristic version).
pub fn dead_code(
    hits: &[(String, String, usize, String)],
    count: usize,
    freshness: &str,
) -> Value {
    use crate::health::{DEAD_CODE_EXCLUDED_RULES, DEAD_CODE_NOTE};
    let hit_values: Vec<Value> = hits
        .iter()
        .map(|(path, name, line, id)| {
            json!({
                "path": path,
                "name": name,
                "line": line,
                "id": id,
            })
        })
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "dead_code",
        "hits": hit_values,
        "count": count,
        "excluded_rules": DEAD_CODE_EXCLUDED_RULES,
        "note": DEAD_CODE_NOTE,
        "freshness": freshness,
    })
}

/// `lattice query --hotspots` success envelope.
///
/// `entries`: pre-sorted, pre-truncated hotspot entries from `health::hotspots`.
/// `churn_weighted`: true iff the git_file_stats table exists with ≥1 row.
pub fn hotspots(
    hotspot_values: &[Value],
    churn_weighted: bool,
    freshness: &str,
) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "hotspots",
        "unit": "file",
        "churn_weighted": churn_weighted,
        "hotspots": hotspot_values,
        "freshness": freshness,
    })
}

/// `lattice query --coupling <path>` success envelope.
///
/// Counts and instability are derived from the list lengths (single source of
/// truth — counts cannot disagree with lists).
pub fn coupling(
    path: &str,
    afferent_files: &[String],
    efferent_files: &[String],
    freshness: &str,
) -> Value {
    let aff = afferent_files.len();
    let eff = efferent_files.len();
    let instability: f64 = if aff + eff == 0 {
        0.0
    } else {
        eff as f64 / (aff + eff) as f64
    };
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "coupling",
        "path": path,
        "afferent": aff,
        "efferent": eff,
        "afferent_files": afferent_files,
        "efferent_files": efferent_files,
        "instability": instability,
        "freshness": freshness,
    })
}

/// `lattice query --health` success envelope.
///
/// Composes hotspots (top HEALTH_HOTSPOT_LIMIT), dead-code (hits capped at
/// DEFAULT_LIMIT, true count), and god-classes into one summary.
#[allow(clippy::too_many_arguments)]
pub fn health(
    files: usize,
    functions: usize,
    call_edges: usize,
    import_edges: usize,
    avg_coupling: f64,
    hotspot_entries: &[crate::health::HotspotEntry],
    dead_hits: &[(String, String, usize, String)],
    dead_count: usize,
    god_threshold: usize,
    god_entries: &[(String, String, usize)],
    churn_weighted: bool,
    freshness: &str,
) -> Value {
    let hotspot_values: Vec<Value> = hotspot_entries
        .iter()
        .map(|e| {
            json!({
                "path": e.path,
                "imports_in": e.imports_in,
                "imports_out": e.imports_out,
                "calls_in": e.calls_in,
                "calls_out": e.calls_out,
                "degree": e.degree,
                "churn": e.churn,
                "score": e.score,
            })
        })
        .collect();

    let dead_hit_values: Vec<Value> = dead_hits
        .iter()
        .map(|(path, name, line, id)| {
            json!({
                "path": path,
                "name": name,
                "line": line,
                "id": id,
            })
        })
        .collect();

    let god_entry_values: Vec<Value> = god_entries
        .iter()
        .map(|(path, class, methods)| {
            json!({
                "path": path,
                "class": class,
                "methods": methods,
            })
        })
        .collect();

    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "health",
        "files": files,
        "functions": functions,
        "call_edges": call_edges,
        "import_edges": import_edges,
        "avg_coupling": avg_coupling,
        "hotspots": hotspot_values,
        "dead_code": {
            "count": dead_count,
            "hits": dead_hit_values,
        },
        "god_classes": {
            "threshold": god_threshold,
            "entries": god_entry_values,
        },
        "churn_weighted": churn_weighted,
        "freshness": freshness,
    })
}

/// `lattice query "<text>"` success envelope.
///
/// Hits carry fused RRF scores plus per-backend rank/score fields so consumers
/// can reproduce the ordering. Empty `hits` is a valid success (clean miss).
///
/// Each hit in `hits` is:
/// `(path, name, kind, start_line, end_line_opt, class, fused_score,
///   sources, fts_rank_opt, vector_rank_opt, vector_score_opt, in_degree)`
#[allow(clippy::too_many_arguments)]
pub fn hybrid(
    query: &str,
    hits: &[(
        String,             // path
        String,             // name
        String,             // kind
        usize,              // start_line
        Option<usize>,      // end_line (None for FTS-only hits)
        String,             // class
        f64,                // fused_score
        Vec<String>,        // sources: subset of ["fts", "vector"]
        Option<usize>,      // fts_rank (1-based; None if FTS didn't return this)
        Option<usize>,      // vector_rank (1-based; None if vector didn't return this)
        Option<f32>,        // vector_score (None if vector didn't return this)
        usize,              // in_degree
    )],
    freshness: &str,
) -> Value {
    let hit_values: Vec<Value> = hits
        .iter()
        .map(
            |(
                path,
                name,
                kind,
                start_line,
                end_line,
                class,
                fused_score,
                sources,
                fts_rank,
                vector_rank,
                vector_score,
                in_degree,
            )| {
                json!({
                    "path": path,
                    "name": name,
                    "kind": kind,
                    "start_line": start_line,
                    "end_line": end_line,
                    "class": class,
                    "fused_score": fused_score,
                    "sources": sources,
                    "fts_rank": fts_rank,
                    "vector_rank": vector_rank,
                    "vector_score": vector_score,
                    "in_degree": in_degree,
                })
            },
        )
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "hybrid",
        "query": query,
        "k": crate::fuse::RRF_K,
        "hits": hit_values,
        "freshness": freshness,
    })
}

/// Structured error envelope; caller pairs this with a non-zero exit code.
pub fn error(code: &str, message: &str) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "error",
        "error": { "code": code, "message": message },
    })
}

// ── Flow envelopes ────────────────────────────────────────────────────────────

/// `lattice query --flows` success envelope: list of execution flows.
pub fn flows(flows: &[crate::analysis::Flow], freshness: &str) -> Value {
    let flow_values: Vec<Value> = flows
        .iter()
        .map(|f| {
            let members: Vec<&str> = f.members.iter().map(|s| s.as_str()).collect();
            let chain: Vec<Value> = f
                .chain
                .iter()
                .map(|(path, name)| json!({"path": path, "name": name}))
                .collect();
            json!({
                "entry": f.entry,
                "members": members,
                "chain": chain,
                "criticality": f.criticality,
            })
        })
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "flows",
        "flows": flow_values,
        "count": flow_values.len(),
        "freshness": freshness,
    })
}

/// `lattice query --flow <sym>` success envelope: one traced flow.
pub fn flow(flow: &crate::analysis::Flow, freshness: &str) -> Value {
    let members: Vec<&str> = flow.members.iter().map(|s| s.as_str()).collect();
    let chain: Vec<Value> = flow
        .chain
        .iter()
        .map(|(path, name)| json!({"path": path, "name": name}))
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "flow",
        "entry": flow.entry,
        "members": members,
        "chain": chain,
        "criticality": flow.criticality,
        "freshness": freshness,
    })
}

// ── Community envelopes ───────────────────────────────────────────────────────

/// `lattice query --communities` success envelope: list of communities.
pub fn communities(
    report: &crate::analysis::CommunityReport,
    freshness: &str,
) -> Value {
    let comm_values: Vec<Value> = report
        .communities
        .iter()
        .map(|c| {
            let members: Vec<&str> = c.members.iter().map(|s| s.as_str()).collect();
            json!({
                "id": c.id,
                "name": c.name,
                "label": c.name,
                "members": members,
                "size": members.len(),
            })
        })
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "communities",
        "resolution": report.resolution,
        "modularity": report.modularity,
        "communities": comm_values,
        "count": comm_values.len(),
        "freshness": freshness,
    })
}

/// `lattice query --community <id>` success envelope: one community's members.
pub fn community(comm: &crate::analysis::Community, freshness: &str) -> Value {
    let members: Vec<&str> = comm.members.iter().map(|s| s.as_str()).collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "community",
        "id": comm.id,
        "name": comm.name,
        "members": members,
        "size": members.len(),
        "freshness": freshness,
    })
}

// ── Snapshot envelopes ────────────────────────────────────────────────────────

/// `lattice snapshot --label <name>` success envelope. `command` rather than
/// `query_type`: snapshot is not a query dispatch.
pub fn snapshot_label_ok(label: &str, nodes: usize, edges: usize) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "command": "snapshot",
        "label": label,
        "node_count": nodes,
        "edge_count": edges,
        "status": "ok",
    })
}

fn snap_node(n: &crate::snapshot::SnapNode) -> Value {
    json!({"id": n.id, "kind": n.kind, "path": n.path})
}

fn snap_edge(e: &crate::snapshot::SnapEdge) -> Value {
    json!({"from": e.from, "to": e.to, "kind": e.kind})
}

/// `lattice query --graph-diff <a> <b>`.
pub fn graph_diff_query(from: &str, to: &str, diff: &crate::snapshot::GraphDiff, freshness: &str) -> Value {
    let added: Vec<Value> = diff.nodes_added.iter().map(snap_node).collect();
    let removed: Vec<Value> = diff.nodes_removed.iter().map(snap_node).collect();
    let changed: Vec<Value> = diff
        .nodes_changed
        .iter()
        .map(|c| json!({"id": c.id, "field": c.field, "before": c.before, "after": c.after}))
        .collect();
    let edges_added: Vec<Value> = diff.edges_added.iter().map(snap_edge).collect();
    let edges_removed: Vec<Value> = diff.edges_removed.iter().map(snap_edge).collect();
    let merged: Vec<Value> = diff
        .communities_merged
        .iter()
        .map(|(a, b)| json!([a, b]))
        .collect();
    let split: Vec<Value> = diff
        .communities_split
        .iter()
        .map(|(a, b)| json!([a, b]))
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "graph_diff",
        "from": from,
        "to": to,
        "nodes": { "added": added, "removed": removed, "changed": changed },
        "edges": { "added": edges_added, "removed": edges_removed },
        "communities": {
            "added": diff.communities_added,
            "removed": diff.communities_removed,
            "merged": merged,
            "split": split,
        },
        "freshness": freshness,
    })
}

/// `lattice query --review <ref>`.
pub fn review(spec: &str, report: &crate::review::Review, freshness: &str) -> Value {
    let changed: Vec<Value> = report
        .changed
        .iter()
        .map(|c| json!({"path": c.path, "symbols": c.symbols}))
        .collect();
    let affected: Vec<Value> = report
        .affected
        .iter()
        .map(|a| json!({
            "symbol": a.symbol,
            "path": a.path,
            "callers": a.callers,
            "transitive_callers": a.transitive_callers,
            "risk": a.risk,
        }))
        .collect();
    let test_gaps: Vec<Value> = report
        .test_gaps
        .iter()
        .map(|g| json!({"symbol": g.symbol, "path": g.path}))
        .collect();
    let owners: Vec<Value> = report
        .owners
        .iter()
        .map(|o| json!({"path": o.path, "owners": o.owners}))
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "review",
        "ref": spec,
        "changed": changed,
        "affected": affected,
        "test_gaps": test_gaps,
        "owners": owners,
        "freshness": freshness,
    })
}

/// `lattice query --similar <symbol>`.
pub fn similar(
    symbol: &str,
    hits: &[(String, String, String, usize, usize, f32, String)],
    freshness: &str,
) -> Value {
    let hit_values: Vec<Value> = hits
        .iter()
        .map(|(path, name, kind, start_line, end_line, score, class)| {
            json!({
                "path": path,
                "name": name,
                "kind": kind,
                "start_line": start_line,
                "end_line": end_line,
                "score": score,
                "class": class,
            })
        })
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "similar",
        "symbol": symbol,
        "hits": hit_values,
        "freshness": freshness,
    })
}

/// `lattice query --peek "<text>"`. Locator-only: no body span.
pub fn peek(
    hits: &[(String, String, String, usize, String)],
    freshness: &str,
) -> Value {
    let hit_values: Vec<Value> = hits
        .iter()
        .map(|(path, name, kind, line, class)| {
            json!({
                "path": path,
                "name": name,
                "kind": kind,
                "line": line,
                "class": class,
            })
        })
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "peek",
        "hits": hit_values,
        "freshness": freshness,
    })
}

/// One `--context` hit. `matched_by` is a sorted subset of
/// `fts` / `vector` / `graph`. `evidence` is present only when `graph` matched.
pub struct ContextHit {
    pub id: String,
    pub path: String,
    pub name: String,
    pub kind: String,
    pub line: usize,
    pub matched_by: Vec<String>,
    pub evidence: Option<crate::ingest::CallSite>,
}

pub fn show(
    id: &str,
    path: &str,
    start_line: usize,
    end_line: usize,
    source: &str,
    freshness: &str,
) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "show",
        "id": id,
        "path": path,
        "start_line": start_line,
        "end_line": end_line,
        "source": source,
        "freshness": freshness,
    })
}

/// `lattice query --context "<text>"`. Additive query type; no protocol bump.
pub fn context(hits: &[ContextHit], freshness: &str) -> Value {
    let hit_values: Vec<Value> = hits
        .iter()
        .map(|h| {
            let mut obj = json!({
                "id": h.id,
                "path": h.path,
                "name": h.name,
                "kind": h.kind,
                "line": h.line,
                "matched_by": h.matched_by,
            });
            if let Some(site) = &h.evidence {
                obj.as_object_mut().unwrap().insert(
                    "evidence".to_string(),
                    json!({
                        "path": site.path,
                        "start_line": site.start_line,
                        "end_line": site.end_line,
                    }),
                );
            }
            obj
        })
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "context",
        "hits": hit_values,
        "freshness": freshness,
    })
}

/// One `lattice bench` question result.
pub struct BenchResult {
    pub question: String,
    pub naive_tokens: usize,
    pub query_tokens: usize,
    pub ratio: f64,
}

/// `lattice bench`. New query type, no protocol bump of its own.
pub fn bench(
    model: &str,
    results: &[BenchResult],
    average_ratio: f64,
    freshness: &str,
) -> Value {
    let rows: Vec<Value> = results
        .iter()
        .map(|r| {
            json!({
                "question": r.question,
                "naive_tokens": r.naive_tokens,
                "query_tokens": r.query_tokens,
                "ratio": r.ratio,
            })
        })
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "bench",
        "model": model,
        "results": rows,
        "average_ratio": average_ratio,
        "freshness": freshness,
    })
}

/// `lattice query --bridges`.
pub fn bridges(hits: &[crate::centrality::BridgeHit], freshness: &str) -> Value {
    let hit_values: Vec<Value> = hits
        .iter()
        .map(|h| json!({"path": h.path, "name": h.name, "betweenness": h.betweenness}))
        .collect();
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "query_type": "bridges",
        "hits": hit_values,
        "freshness": freshness,
    })
}
