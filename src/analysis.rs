// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Pure-Rust graph analytics: flow detection, community detection, criticality
//! scoring, and static hints. All public functions are pure over already-loaded
//! data structures — no I/O. Mirrors `health.rs`'s contract.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::graph::{CallGraph, GraphView};
use crate::leiden;

// ── Constants ────────────────────────────────────────────────────────────────

/// Security-sensitive name substrings for criticality scoring.
pub const SECURITY_KEYWORDS: &[&str] = &[
    "auth",
    "login",
    "token",
    "password",
    "secret",
    "crypto",
    "verify",
    "permission",
    "admin",
    "sql",
    "exec",
    "payment",
];

/// Entry-point name conventions: functions matching these names are considered
/// potential entry points if they also have zero incoming callers.
pub const ENTRY_POINT_NAMES: &[&str] = &[
    "main",
    "run",
    "handler",
    "lambda_handler",
    "execute",
    "serve",
];

/// Prefix for CLI entry-point detection (e.g. `cmd_start`, `cmd_run`).
pub const ENTRY_POINT_PREFIX: &str = "cmd_";

// ── Types ─────────────────────────────────────────────────────────────────────

/// A detected entry point (zero callers, matches name convention or prefix).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntryPoint {
    pub id: String,
    pub name: String,
    pub path: String,
    pub line: usize,
}

/// An execution flow: entry point + all reachable functions via forward BFS.
#[derive(Debug, Clone)]
pub struct Flow {
    pub entry: String,
    pub members: BTreeSet<String>,
    /// BFS visit order as `(path, name)`, bounded by `graph::MAX_CALL_CHAIN_DEPTH`.
    /// Additive on the flows envelope (the member set is unchanged).
    pub chain: Vec<(String, String)>,
    pub criticality: i64,
}

/// A community of related functions detected by the Leiden algorithm.
#[derive(Debug, Clone)]
pub struct Community {
    pub id: usize,
    pub name: String,
    pub members: BTreeSet<String>,
}

/// A suggestion for the next tool to call (used by the hints system).
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub tool: String,
    pub description: String,
}

// ── Entry-point detection ─────────────────────────────────────────────────────

/// Detect entry points: functions with zero incoming callers whose name matches
/// a convention (ENTRY_POINT_NAMES) or prefix (ENTRY_POINT_PREFIX).
///
/// Test functions (name starts with `test_`) are excluded.
/// Deterministic: iterates BTreeMap nodes in key order.
pub fn entry_points(cg: &CallGraph, _view: &GraphView) -> Vec<EntryPoint> {
    let mut entries = Vec::new();

    for (id, node) in &cg.nodes {
        // Must have zero callers.
        let has_callers = cg.inn.get(id).map_or(false, |s| !s.is_empty());
        if has_callers {
            continue;
        }

        // Exclude test functions (name starts with `test_`).
        if node.name.starts_with("test_") {
            continue;
        }

        // Match name convention or prefix.
        let is_entry_name = ENTRY_POINT_NAMES.contains(&node.name.as_str());
        let is_entry_prefix = node.name.starts_with(ENTRY_POINT_PREFIX);

        if is_entry_name || is_entry_prefix {
            entries.push(EntryPoint {
                id: id.clone(),
                name: node.name.clone(),
                path: node.path.clone(),
                line: node.line,
            });
        }
    }

    // Sort by name ascending for determinism.
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

// ── Flow tracing ──────────────────────────────────────────────────────────────

/// Trace an execution flow from an entry point via forward BFS over call edges.
/// Returns the set of reachable function IDs (sorted for determinism).
/// BFS is bounded by `graph::MAX_CALL_CHAIN_DEPTH` to handle cycles.
pub fn trace_flow(cg: &CallGraph, entry_id: &str) -> BTreeSet<String> {
    let mut members = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut frontier = BTreeSet::new();

    frontier.insert(entry_id.to_string());

    for _ in 0..crate::graph::MAX_CALL_CHAIN_DEPTH {
        if frontier.is_empty() {
            break;
        }
        let mut next_frontier = BTreeSet::new();
        for current in &frontier {
            if visited.contains(current) {
                continue;
            }
            visited.insert(current.clone());
            members.insert(current.clone());

            if let Some(callees) = cg.out.get(current) {
                for callee in callees {
                    if !visited.contains(callee) {
                        next_frontier.insert(callee.clone());
                    }
                }
            }
        }
        frontier = next_frontier;
    }

    members
}

/// Ordered call chain from `entry_id`: BFS visit order, callees sorted, bounded
/// by [`crate::graph::MAX_CALL_CHAIN_DEPTH`]. Each step is `(path, name)`.
pub fn trace_chain(cg: &CallGraph, entry_id: &str) -> Vec<(String, String)> {
    let mut chain = Vec::new();
    let mut visited = BTreeSet::new();
    let mut frontier = vec![entry_id.to_string()];
    for _ in 0..crate::graph::MAX_CALL_CHAIN_DEPTH {
        if frontier.is_empty() {
            break;
        }
        frontier.sort();
        let mut next = Vec::new();
        for current in frontier {
            if !visited.insert(current.clone()) {
                continue;
            }
            if let Some(node) = cg.nodes.get(&current) {
                chain.push((node.path.clone(), node.name.clone()));
            }
            if let Some(callees) = cg.out.get(&current) {
                let mut kids: Vec<String> = callees.iter().cloned().collect();
                kids.sort();
                for callee in kids {
                    if !visited.contains(&callee) {
                        next.push(callee);
                    }
                }
            }
        }
        frontier = next;
    }
    chain
}

// ── Flow list ─────────────────────────────────────────────────────────────────

/// Detect all execution flows: one per entry point, traced via BFS.
/// Each flow is scored for criticality. Sorted by (criticality desc, entry path asc, entry name asc).
pub fn flows(
    cg: &CallGraph,
    view: &GraphView,
    churn_map: &HashMap<String, u64>,
) -> Vec<Flow> {
    let entries = entry_points(cg, view);
    let mut result: Vec<Flow> = entries
        .iter()
        .map(|entry| {
            let members = trace_flow(cg, &entry.id);
            let crit = score_criticality(cg, churn_map, &members);
            Flow {
                entry: entry.id.clone(),
                chain: trace_chain(cg, &entry.id),
                members,
                criticality: crit,
            }
        })
        .collect();

    // Sort by criticality descending, then entry path ascending, then entry name ascending.
    result.sort_by(|a, b| {
        b.criticality
            .cmp(&a.criticality)
            .then_with(|| {
                // Look up paths from cg.nodes
                let path_a = cg.nodes.get(&a.entry).map(|n| n.path.as_str()).unwrap_or("");
                let path_b = cg.nodes.get(&b.entry).map(|n| n.path.as_str()).unwrap_or("");
                path_a.cmp(path_b)
            })
            .then_with(|| a.entry.cmp(&b.entry))
    });

    result
}

// ── Criticality scoring ───────────────────────────────────────────────────────

/// Score a flow's criticality based on:
/// 1. Security keyword matches in member names (high weight)
/// 2. Total fan-in across all members (medium weight)
/// 3. Total churn across all members (low weight)
///
/// Returns a deterministic integer score. Higher = more critical.
pub fn score_criticality(
    cg: &CallGraph,
    churn_map: &HashMap<String, u64>,
    members: &BTreeSet<String>,
) -> i64 {
    let mut has_security_name = false;
    let mut total_fan_in = 0usize;
    let mut total_churn = 0u64;

    for member_id in members {
        // Security keyword check on function name.
        if let Some(node) = cg.nodes.get(member_id) {
            let name_lower = node.name.to_lowercase();
            if SECURITY_KEYWORDS.iter().any(|kw| name_lower.contains(kw)) {
                has_security_name = true;
            }
        }

        // Fan-in: number of callers of this member.
        if let Some(callers) = cg.inn.get(member_id) {
            total_fan_in += callers.len();
        }

        // Churn: look up by path (the churn_map keys are file paths).
        if let Some(node) = cg.nodes.get(member_id) {
            if let Some(&churn) = churn_map.get(&node.path) {
                total_churn += churn;
            }
        }
    }

    // Score formula: security_name (50) + fan_in (2 each) + churn (1 each, capped at 100).
    let security_bonus = if has_security_name { 50 } else { 0 };
    let fan_in_score = (total_fan_in as i64) * 2;
    let churn_score = (total_churn.min(100)) as i64;

    security_bonus + fan_in_score + churn_score
}

// ── Community detection (Leiden) ──────────────────────────────────────────────

/// A Leiden partition plus the resolution/modularity the envelope echoes.
pub struct CommunityReport {
    pub resolution: f64,
    pub modularity: f64,
    pub communities: Vec<Community>,
}

/// Leiden communities at the default resolution, no recursive split.
/// Kept as the convenience used by context / OKF.
pub fn communities(cg: &CallGraph) -> Vec<Community> {
    communities_report(cg, leiden::DEFAULT_RESOLUTION, false).communities
}

/// Swap the clustering engine from single-pass Louvain to multi-level Leiden.
/// `resolution` is echoed (default [`leiden::DEFAULT_RESOLUTION`]). `--split`
/// re-runs Leiden on any community larger than [`leiden::OVERSIZED_FRACTION`]
/// of the graph.
pub fn communities_report(cg: &CallGraph, resolution: f64, split: bool) -> CommunityReport {
    let detected = leiden::detect(cg, resolution);
    let groups = if split {
        split_oversized(cg, &detected, resolution)
    } else {
        detected.communities.iter().map(|c| c.members.clone()).collect()
    };
    let modularity = if split {
        leiden::modularity_of(cg, &groups, resolution)
    } else {
        detected.modularity
    };
    let mut communities: Vec<Community> = groups
        .into_iter()
        .map(|members| {
            let set: BTreeSet<String> = members.into_iter().collect();
            let name = name_community(cg, &set);
            Community {
                id: 0,
                name,
                members: set,
            }
        })
        .collect();
    // Largest first, then by name, then reassign dense ids so output is stable.
    communities.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.members.iter().next().cmp(&b.members.iter().next()))
    });
    for (i, c) in communities.iter_mut().enumerate() {
        c.id = i;
    }
    CommunityReport {
        resolution,
        modularity,
        communities,
    }
}

/// Recursively re-cluster any community exceeding 25% of the graph.
fn split_oversized(cg: &CallGraph, detected: &leiden::Communities, resolution: f64) -> Vec<Vec<String>> {
    let n = cg.nodes.len();
    let mut out = Vec::new();
    for comm in &detected.communities {
        let oversized = n > 0
            && (comm.members.len() as f64) > (n as f64) * leiden::OVERSIZED_FRACTION
            && comm.members.len() > 1;
        if !oversized {
            out.push(comm.members.clone());
            continue;
        }
        let sub = subgraph(cg, &comm.members);
        let refined = leiden::detect(&sub, resolution);
        if refined.communities.len() <= 1 {
            out.push(comm.members.clone());
            continue;
        }
        let nested = split_oversized(&sub, &refined, resolution);
        out.extend(nested);
    }
    out
}

fn subgraph(cg: &CallGraph, members: &[String]) -> CallGraph {
    let keep: BTreeSet<&str> = members.iter().map(|s| s.as_str()).collect();
    let mut nodes = BTreeMap::new();
    for id in &keep {
        if let Some(node) = cg.nodes.get(*id) {
            nodes.insert((*id).to_string(), node.clone());
        }
    }
    let mut out_edges = BTreeMap::new();
    let mut inn = BTreeMap::new();
    for (caller, callees) in &cg.out {
        if !keep.contains(caller.as_str()) {
            continue;
        }
        for callee in callees {
            if !keep.contains(callee.as_str()) {
                continue;
            }
            out_edges
                .entry(caller.clone())
                .or_insert_with(BTreeSet::new)
                .insert(callee.clone());
            inn.entry(callee.clone())
                .or_insert_with(BTreeSet::new)
                .insert(caller.clone());
        }
    }
    CallGraph {
        nodes,
        out: out_edges,
        inn,
        evidence: BTreeMap::new(),
        sites: BTreeMap::new(),
    }
}

/// Derive a community name from the most common path prefix among members.
fn name_community(cg: &CallGraph, members: &BTreeSet<String>) -> String {
    let mut prefix_counts: BTreeMap<String, usize> = BTreeMap::new();
    for member_id in members {
        if let Some(node) = cg.nodes.get(member_id) {
            let prefix = last_module_segment(&node.path);
            *prefix_counts.entry(prefix).or_insert(0) += 1;
        }
    }

    prefix_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(name, _)| name)
        .unwrap_or_else(|| "unnamed".to_string())
}

/// Extract the last meaningful segment from a file path (without extension).
fn last_module_segment(path: &str) -> String {
    let stem = std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string());
    stem
}

// ── Static hints / suggestions ────────────────────────────────────────────────

/// Return suggested next tools for a given query type.
/// Deterministic static table — no state, no I/O.
pub fn next_tools(query_type: &str) -> Vec<Suggestion> {
    match query_type {
        "impact" => vec![
            Suggestion { tool: "lattice query --callers <symbol>".into(), description: "Find direct callers of a symbol".into() },
            Suggestion { tool: "lattice query --callees <symbol>".into(), description: "Find direct callees of a symbol".into() },
            Suggestion { tool: "lattice query --flow <entry>".into(), description: "Trace the execution flow from an entry point".into() },
        ],
        "flows" | "flow" => vec![
            Suggestion { tool: "lattice query --flow <entry>".into(), description: "Drill into a specific flow".into() },
            Suggestion { tool: "lattice query --communities".into(), description: "See the architecture overview".into() },
            Suggestion { tool: "lattice query --hotspots".into(), description: "Find high-churn, high-coupling files".into() },
        ],
        "hotspots" => vec![
            Suggestion { tool: "lattice query --impact <path>".into(), description: "Check blast radius of a hot file".into() },
            Suggestion { tool: "lattice query --flow <entry>".into(), description: "Trace execution flows through hotspots".into() },
            Suggestion { tool: "lattice query --owners <path>".into(), description: "See who owns the hotspot files".into() },
        ],
        "dead_code" => vec![
            Suggestion { tool: "lattice query --impact --symbol <symbol>".into(), description: "Check whether an apparently unused function has callers".into() },
            Suggestion { tool: "lattice query --coupling <path>".into(), description: "Check coupling of files with dead code".into() },
        ],
        "communities" | "community" => vec![
            Suggestion { tool: "lattice query --community <id>".into(), description: "Drill into a specific community".into() },
            Suggestion { tool: "lattice summarize <path>".into(), description: "Get a prose summary of a community file".into() },
            Suggestion { tool: "lattice query --flows".into(), description: "See execution flows across communities".into() },
        ],
        "semantic" | "hybrid" => vec![
            Suggestion { tool: "lattice query --symbol <symbol>".into(), description: "Look up a known symbol by name".into() },
            Suggestion { tool: "lattice query --impact <path>".into(), description: "Check blast radius of a matching file".into() },
        ],
        "symbol" => vec![
            Suggestion { tool: "lattice query --impact --symbol <symbol>".into(), description: "Check impact of this symbol".into() },
            Suggestion { tool: "lattice query --callees <symbol>".into(), description: "See what this symbol calls".into() },
            Suggestion { tool: "lattice query --callers <symbol>".into(), description: "See who calls this symbol".into() },
        ],
        "owners" => vec![
            Suggestion { tool: "lattice query --impact <path>".into(), description: "Check impact of owned files".into() },
            Suggestion { tool: "lattice query --hotspots".into(), description: "See churn-weighted hotspots".into() },
        ],
        "coupling" => vec![
            Suggestion { tool: "lattice query --impact <path>".into(), description: "Check blast radius of this file".into() },
            Suggestion { tool: "lattice query --dead-code".into(), description: "Find potentially unused functions".into() },
        ],
        "health" => vec![
            Suggestion { tool: "lattice query --hotspots".into(), description: "See the highest-risk files".into() },
            Suggestion { tool: "lattice query --dead-code".into(), description: "Find potentially unused functions".into() },
            Suggestion { tool: "lattice query --coupling <path>".into(), description: "Check coupling metrics".into() },
        ],
        _ => vec![
            Suggestion { tool: "lattice query \"<question>\"".into(), description: "Search by natural language".into() },
            Suggestion { tool: "lattice query --hotspots".into(), description: "Find high-risk areas".into() },
            Suggestion { tool: "lattice query --communities".into(), description: "Explore the architecture".into() },
        ],
    }
}
