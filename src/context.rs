// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Deterministic repository overview for `lattice context`.
//!
//! Reads the persisted store (graph + meta.db) without triggering any
//! freshness self-heal or re-index. All output is byte-deterministic:
//!
//! - No raw f64 emitted (C1) — hotspot score is dropped; only rank order
//!   (position in the sorted list) is emitted.
//! - Every list is sorted by a total order before emission.
//! - No timestamps, no absolute paths, no env-derived strings.
//! - `freshness` describes the persisted index, not the current working tree.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Value};

use crate::analysis;
use crate::graph;
use crate::health;
use crate::ownership;

/// Default limit for hotspot entries in the stable overview.
const STABLE_HOTSPOT_LIMIT: usize = 10;

/// Default limit for owner-directory rows in the stable overview.
const STABLE_OWNER_LIMIT: usize = 10;

/// Default limit for module rows in the stable overview.
const STABLE_MODULE_LIMIT: usize = 20;

// ── Public API ────────────────────────────────────────────────────────────────

/// Build the stable context envelope value.
///
/// Reads graph + meta.db as-of the last init. No freshness, no re-index.
/// All output is deterministic and byte-identical across runs with no store
/// change (C1).
/// Load and compute all stable data. Returns (repo, modules, hotspots, owners).
///
/// - `repo`     : `{"files": N, "symbols": N, "languages": {...}}`
/// - `modules`  : sorted-asc-by-path Vec of `{"path": ..., "symbols": N, "role_hint": "module"}`
/// - `hotspots` : sorted desc-score-then-asc-path, NO raw f64 score (C1)
/// - `owners`   : sorted asc-by-dir, built via BTreeMap (C1c)
fn load_stable(
    lattice_dir: &Path,
    view: &graph::GraphView,
    call_graph: &graph::CallGraph,
    churn_map: &std::collections::HashMap<String, u64>,
) -> crate::Result<(Value, Vec<Value>, Vec<Value>, Vec<Value>)> {
    let file_count = view.files.len();
    let symbol_count = view.functions.len();

    // Language breakdown: count files per language via BTreeMap for determinism.
    let mut lang_map: BTreeMap<String, u64> = BTreeMap::new();
    for (_path, lang) in &view.files {
        *lang_map.entry(lang.clone()).or_insert(0) += 1;
    }
    let languages: Value = lang_map
        .into_iter()
        .map(|(lang, count)| (lang, Value::Number(count.into())))
        .collect::<serde_json::Map<_, _>>()
        .into();

    let repo = json!({
        "files": file_count,
        "symbols": symbol_count,
        "languages": languages,
    });

    // --- Module map: one entry per file, symbol count per file ---
    // Count symbols per file path using a BTreeMap (sorted, no HashMap hazard).
    let mut sym_per_file: BTreeMap<String, usize> = BTreeMap::new();
    for (path, _lang) in &view.files {
        sym_per_file.entry(path.clone()).or_insert(0);
    }
    for (_name, path) in &view.functions {
        *sym_per_file.entry(path.clone()).or_insert(0) += 1;
    }
    // modules: sorted ascending by path (BTreeMap iteration is already asc).
    let modules: Vec<Value> = sym_per_file
        .iter()
        .take(STABLE_MODULE_LIMIT)
        .map(|(path, &count)| {
            json!({
                "path": path,
                "symbols": count,
                "role_hint": "module",
            })
        })
        .collect();

    // --- Hotspots (no raw f64 — C1) ---
    let degrees = health::file_degrees(call_graph, view);

    // Reuse health::hotspots for the sorted order.
    let hotspot_entries = health::hotspots(&degrees, churn_map, STABLE_HOTSPOT_LIMIT);

    // Build the stable hotspot list: path + degree + churn (NO raw f64 score — C1).
    let hotspots: Vec<Value> = hotspot_entries
        .iter()
        .map(|e| {
            json!({
                "path": e.path,
                "degree": e.degree,
                "churn": e.churn,
                // rank is implicit (position in array); no raw f64
            })
        })
        .collect();

    // --- Owners overview: BTreeMap aggregation of all_file_stats (C1c) ---
    let owners = build_owners_overview(lattice_dir);

    Ok((repo, modules, hotspots, owners))
}

/// Build the owners overview using BTreeMap aggregation (C1c).
///
/// Per directory: total churn + top author by lines-added (desc), name (asc)
/// for ties — deterministic total order, no HashMap iteration.
fn build_owners_overview(lattice_dir: &Path) -> Vec<Value> {
    // all_file_stats returns a HashMap. We must aggregate through a BTreeMap.
    let file_stats = match ownership::all_file_stats(lattice_dir) {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };

    if file_stats.is_empty() {
        return Vec::new();
    }

    // Aggregate per directory (parent path component):
    // dir → BTreeMap<author, lines_added>
    let mut dir_authors: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();

    for (path, (_churn, _last, authors)) in &file_stats {
        // directory = parent of path, or "." for root-level files.
        let dir = std::path::Path::new(path)
            .parent()
            .and_then(|p| p.to_str())
            .map(|s| if s.is_empty() { "." } else { s })
            .unwrap_or(".")
            .to_string();
        let entry = dir_authors.entry(dir).or_default();
        for (author, added, _commits, _last) in authors {
            *entry.entry(author.clone()).or_insert(0) += added;
        }
    }

    // Build sorted rows: for each dir, pick top_author (most lines-added;
    // tie-break by author name asc). Sort dirs ascending (BTreeMap order).
    let mut rows: Vec<Value> = dir_authors
        .iter()
        .filter_map(|(dir, authors)| {
            if authors.is_empty() {
                return None;
            }
            // Top author: max by added desc, then author name asc for ties.
            let top_author = authors
                .iter()
                .max_by(|(a_name, a_added), (b_name, b_added)| {
                    a_added
                        .cmp(b_added)
                        .then_with(|| b_name.cmp(a_name)) // higher added first, then name asc
                })
                .map(|(name, _)| name.clone())
                .unwrap_or_default();
            Some(json!({
                "dir": dir,
                "top_author": top_author,
            }))
        })
        .collect();

    rows.truncate(STABLE_OWNER_LIMIT);
    rows
}

// ── Minimal context ───────────────────────────────────────────────────────────

/// Default limit for communities in the minimal context.
const MINIMAL_COMMUNITY_LIMIT: usize = 3;

/// Default limit for flows in the minimal context.
const MINIMAL_FLOW_LIMIT: usize = 3;

/// Default limit for tool suggestions.
const MINIMAL_SUGGESTION_LIMIT: usize = 3;

/// Build the repository overview from persisted stores without refreshing them.
pub fn overview(
    lattice_dir: &Path,
    task: Option<&str>,
) -> crate::Result<Value> {
    let cg = graph::load_call_graph(lattice_dir)?;
    let view = graph::read(lattice_dir)?;
    let churn_map = ownership::churn_map(lattice_dir)?;
    let (repo, modules, hotspots, owners) = load_stable(lattice_dir, &view, &cg, &churn_map)?;

    // Communities (top N by size, descending).
    let comms = analysis::communities(&cg);
    let mut sorted_comms = comms;
    sorted_comms.sort_by(|a, b| b.members.len().cmp(&a.members.len()));
    let top_communities: Vec<Value> = sorted_comms
        .iter()
        .take(MINIMAL_COMMUNITY_LIMIT)
        .map(|c| {
            json!({
                "name": c.name,
                "size": c.members.len(),
            })
        })
        .collect();

    // Flows (top N by criticality desc).
    let flow_list = analysis::flows(&cg, &view, &churn_map);
    let top_flows: Vec<Value> = flow_list
        .iter()
        .take(MINIMAL_FLOW_LIMIT)
        .map(|f| {
            json!({
                "entry": f.entry.split("::").last().unwrap_or(&f.entry),
                "criticality": f.criticality,
            })
        })
        .collect();

    // Risk label: based on dead-code count + flow criticality.
    let dead = health::dead_code(&cg);
    let max_crit = flow_list.iter().map(|f| f.criticality).max().unwrap_or(0);
    let risk = if dead.len() > 5 || max_crit > 80 {
        "high"
    } else if !dead.is_empty() || max_crit > 30 {
        "medium"
    } else {
        "low"
    };

    // Task-aware suggestions.
    let suggestions = task_suggestions(task);

    Ok(json!({
        "protocol_version": crate::envelope::PROTOCOL_VERSION,
        "query_type": "context_overview",
        "freshness": "as_indexed",
        "repo": repo,
        "modules": modules,
        "hotspots": hotspots,
        "owners": owners,
        "communities": top_communities,
        "flows": top_flows,
        "risk": risk,
        "next_tool_suggestions": suggestions,
    }))
}

/// Derive task-aware next-tool suggestions.
fn task_suggestions(task: Option<&str>) -> Vec<String> {
    let mut suggestions = Vec::new();
    let task_lower = task.unwrap_or("").to_lowercase();

    if task_lower.contains("review") || task_lower.contains("pr") || task_lower.contains("merge") {
        suggestions.push("lattice query --impact <path>".to_string());
        suggestions.push("lattice query --hotspots".to_string());
    } else if task_lower.contains("debug") || task_lower.contains("fix") || task_lower.contains("error") {
        suggestions.push("lattice query --impact <path>".to_string());
        suggestions.push("lattice query --flow <symbol>".to_string());
    } else if task_lower.contains("onboard") || task_lower.contains("explain") || task_lower.contains("overview") {
        suggestions.push("lattice query --communities".to_string());
        suggestions.push("lattice summarize <path>".to_string());
    } else {
        // Default suggestions.
        suggestions.push("lattice query \"<question>\"".to_string());
        suggestions.push("lattice query --hotspots".to_string());
        suggestions.push("lattice query --communities".to_string());
    }

    suggestions.truncate(MINIMAL_SUGGESTION_LIMIT);
    suggestions
}
