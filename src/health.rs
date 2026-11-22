// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Pure-Rust analytics over the existing SparrowDB call graph + git churn.
//!
//! All public functions are pure over already-loaded data structures — no I/O
//! beyond the loads in the dispatch functions (`graph::load_call_graph`,
//! `graph::read`, `churn_map`). No new persisted store, no schema change.
//!
//! Exposes: `dead_code`, `file_degrees`, `hotspots`, `coupling`, `god_classes`.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::class::{self, SourceClass};
use crate::graph::{CallGraph, GraphView};

/// God-class threshold: a (path, parent) group with this many methods or more
/// is surfaced. Emitted in the envelope so it is visible, not magic.
pub const GOD_CLASS_THRESHOLD: usize = 12;

/// Number of hotspot entries embedded in the --health envelope (fixed cap;
/// --limit does not resize this sub-list).
pub const HEALTH_HOTSPOT_LIMIT: usize = 5;

// ── Types ─────────────────────────────────────────────────────────────────────

/// A dead-code candidate (zero-caller function surviving all exclusion rules).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeadHit {
    pub path: String,
    pub name: String,
    pub line: usize,
    pub id: String,
}

/// Per-file degree decomposition used by hotspots.
#[derive(Debug, Clone, Default)]
pub struct DegreeParts {
    pub imports_in: usize,
    pub imports_out: usize,
    pub calls_in: usize,
    pub calls_out: usize,
}

impl DegreeParts {
    pub fn degree(&self) -> usize {
        self.imports_in + self.imports_out + self.calls_in + self.calls_out
    }
}

/// A single hotspot entry (one row in the hotspots envelope).
#[derive(Debug, Clone)]
pub struct HotspotEntry {
    pub path: String,
    pub imports_in: usize,
    pub imports_out: usize,
    pub calls_in: usize,
    pub calls_out: usize,
    pub degree: usize,
    pub churn: u64,
    pub score: f64,
}

/// Result of a coupling query for one file.
#[derive(Debug, Clone)]
pub struct Coupling {
    pub afferent_files: Vec<String>,
    pub efferent_files: Vec<String>,
}

// ── Dead code ─────────────────────────────────────────────────────────────────

/// The fixed, ordered list of exclusion-rule names applied to dead-code
/// candidates. Emitted in the envelope so the heuristic version is visible.
pub const DEAD_CODE_EXCLUDED_RULES: &[&str] =
    &["main", "method", "test_name", "test_path", "dunder"];

/// Heuristic documentation string emitted in every dead_code envelope.
pub const DEAD_CODE_NOTE: &str =
    "heuristic: public API, dynamic dispatch, macro/attribute-invoked and \
     reflectively-called functions may be missed or wrongly flagged";

/// Compute dead-code candidates from the call graph.
///
/// Candidate set: every Function node whose in-degree is zero.
/// Then apply exclusion rules in order (R1–R5); a node is dropped at the
/// first matching rule.
///
/// Returns hits sorted by (path, line, name).
pub fn dead_code(cg: &CallGraph) -> Vec<DeadHit> {
    let mut hits: Vec<DeadHit> = Vec::new();

    for (id, node) in &cg.nodes {
        // Must have in-degree zero (no callers in the graph).
        let has_callers = cg
            .inn
            .get(id)
            .map_or(false, |s| !s.is_empty());
        if has_callers {
            continue;
        }

        // R1: main — program entry point.
        if node.name == "main" {
            continue;
        }

        // R2: method — impl fns / methods reached via dyn dispatch or framework
        // callbacks; conservative exclusion avoids false positives.
        if node.parent.is_some() {
            continue;
        }

        // R3: test_name — pytest / Rust test naming convention.
        if node.name.starts_with("test_") {
            continue;
        }

        // R4: test_path — any non-source class (Test, Fixture, Generated, Vendored).
        if class::classify(&node.path) != SourceClass::Source {
            continue;
        }

        // R5: dunder — Python dunder methods (__init__, __str__, etc.).
        if node.name.starts_with("__") && node.name.ends_with("__") {
            continue;
        }

        // R6: mock/stub — test doubles in source files (port of reference's
        // _MOCK_NAME_RE). Case-insensitive prefix/suffix check.
        if is_mock_name(&node.name) {
            continue;
        }

        // R7: framework class — methods of known framework base classes.
        // Redundant with R2 for now (R2 excludes all methods), but future-
        // proofing in case R2 is relaxed. Also catches free functions whose
        // file name matches a framework pattern.
        if let Some(ref parent) = node.parent {
            if is_framework_class(parent) {
                continue;
            }
        }

        // R8: entry_point — functions matching known entry-point conventions
        // (main, run, handler, execute, serve, cmd_*) are never dead code
        // even if nothing in the current graph calls them.
        if crate::analysis::ENTRY_POINT_NAMES.contains(&node.name.as_str())
            || node.name.starts_with(crate::analysis::ENTRY_POINT_PREFIX)
        {
            continue;
        }

        hits.push(DeadHit {
            path: node.path.clone(),
            name: node.name.clone(),
            line: node.line,
            id: node.id.clone(),
        });
    }

    // Sort by (path, line, name) ascending — deterministic output.
    hits.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.name.cmp(&b.name))
    });
    hits
}

// ── Hotspots ──────────────────────────────────────────────────────────────────

/// Compute per-file degree decomposition over the call graph and graph view.
///
/// Seeded with every path in `view.files` so degree-0 files appear.
/// Intra-file edges are excluded; a hotspot is a coordination risk, not
/// internal wiring.
pub fn file_degrees(cg: &CallGraph, view: &GraphView) -> BTreeMap<String, DegreeParts> {
    let mut map: BTreeMap<String, DegreeParts> = BTreeMap::new();

    // Seed every indexed file so degree-0 files (e.g. orphan.rs) appear.
    for (path, _lang) in &view.files {
        map.entry(path.clone()).or_default();
    }

    // Import edges (file → file), self-loops excluded.
    for (from, to) in &view.imports {
        if from == to {
            continue;
        }
        map.entry(from.clone()).or_default().imports_out += 1;
        map.entry(to.clone()).or_default().imports_in += 1;
    }

    // Call edges — count cross-file pairs only.
    // `cg.out` maps caller_id → BTreeSet<callee_id>; each node has a `path`.
    for (caller_id, callee_set) in &cg.out {
        let caller_node = match cg.nodes.get(caller_id) {
            Some(n) => n,
            None => continue,
        };
        let caller_path = &caller_node.path;

        let mut seen_callee_paths: BTreeSet<String> = BTreeSet::new();
        for callee_id in callee_set {
            let callee_node = match cg.nodes.get(callee_id) {
                Some(n) => n,
                None => continue,
            };
            let callee_path = &callee_node.path;
            if callee_path == caller_path {
                // Intra-file: skip.
                continue;
            }
            // Count each distinct cross-file function pair once.
            // The BTreeSet already de-duplicates callee_ids, so we iterate
            // each (caller_id, callee_id) pair exactly once. Two distinct
            // functions in the same caller file calling the same callee file
            // count as two separate cross-file pairs, matching the design's
            // "distinct pairs (a,b) from cg.out with…" spec.
            seen_callee_paths.insert(callee_path.clone());

            map.entry(caller_path.clone()).or_default().calls_out += 1;
            map.entry(callee_path.clone()).or_default().calls_in += 1;
        }
        let _ = seen_callee_paths; // used only to make logic explicit
    }

    map
}

/// Compute scored hotspot entries from the degree map and churn map.
///
/// `score(f) = degree(f) as f64 * (1.0 + churn(f) as f64)`
///
/// Sorted: score descending, path ascending for ties.
/// Truncated to `limit`.
pub fn hotspots(
    degrees: &BTreeMap<String, DegreeParts>,
    churn_map: &HashMap<String, u64>,
    limit: usize,
) -> Vec<HotspotEntry> {
    let mut entries: Vec<HotspotEntry> = degrees
        .iter()
        .map(|(path, dp)| {
            let churn = *churn_map.get(path).unwrap_or(&0);
            let degree = dp.degree();
            let score = degree as f64 * (1.0 + churn as f64);
            HotspotEntry {
                path: path.clone(),
                imports_in: dp.imports_in,
                imports_out: dp.imports_out,
                calls_in: dp.calls_in,
                calls_out: dp.calls_out,
                degree,
                churn,
                score,
            }
        })
        .collect();

    // Sort: score descending, path ascending for ties.
    entries.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });
    entries.truncate(limit);
    entries
}

// ── Coupling ─────────────────────────────────────────────────────────────────

/// Compute afferent/efferent import coupling for a single file path.
///
/// Operates over `view.imports` (already sorted + de-duplicated file-pair
/// edges). Self-loops are excluded. A path not present in the graph naturally
/// yields empty lists and instability 0.0 — the graceful unknown-path case.
pub fn coupling(view: &GraphView, path: &str) -> Coupling {
    // afferent: files that import `path` (in-edges).
    let mut afferent_set: BTreeSet<String> = BTreeSet::new();
    for (from, to) in &view.imports {
        if to == path && from != path {
            afferent_set.insert(from.clone());
        }
    }

    // efferent: files that `path` imports (out-edges) — same as imports_of.
    let efferent_set: BTreeSet<String> = view
        .imports_of(path)
        .into_iter()
        .filter(|p| p != path)
        .collect();

    Coupling {
        afferent_files: afferent_set.into_iter().collect(),
        efferent_files: efferent_set.into_iter().collect(),
    }
}

// ── God classes ───────────────────────────────────────────────────────────────

/// A god-class entry: a (path, class_name) group exceeding the method threshold.
#[derive(Debug, Clone)]
pub struct GodClass {
    pub path: String,
    pub class_name: String,
    pub methods: usize,
}

/// Find god classes in the call graph: (path, parent) groups with method count
/// ≥ threshold.
///
/// Groups only Function nodes with `parent = Some(P)` (methods); free functions
/// are NOT grouped into a pseudo-class per file.
///
/// Returns entries sorted by methods descending, then (path, class_name) ascending.
pub fn god_classes(cg: &CallGraph, threshold: usize) -> Vec<GodClass> {
    // Group by (path, parent) where parent is Some.
    let mut groups: BTreeMap<(String, String), usize> = BTreeMap::new();
    for node in cg.nodes.values() {
        if let Some(parent) = &node.parent {
            *groups
                .entry((node.path.clone(), parent.clone()))
                .or_insert(0) += 1;
        }
    }

    let mut entries: Vec<GodClass> = groups
        .into_iter()
        .filter(|(_, count)| *count >= threshold)
        .map(|((path, class_name), methods)| GodClass {
            path,
            class_name,
            methods,
        })
        .collect();

    // Sort: methods desc, then (path, class_name) asc.
    entries.sort_by(|a, b| {
        b.methods
            .cmp(&a.methods)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.class_name.cmp(&b.class_name))
    });
    entries
}

// ── R6/R7 helpers ─────────────────────────────────────────────────────────────

/// Known framework base-class suffixes (R7). A function whose enclosing class
/// name ends with one of these is excluded from dead-code candidates.
const FRAMEWORK_CLASS_SUFFIXES: &[&str] = &[
    "Stack",
    "Construct",
    "Model",
    "BaseModel",
    "Settings",
    "Config",
    "Handler",
    "Service",
    "Repository",
    "Controller",
];

/// Check if a parent class name matches a known framework base class.
fn is_framework_class(parent: &str) -> bool {
    FRAMEWORK_CLASS_SUFFIXES
        .iter()
        .any(|suffix| parent.ends_with(suffix))
}

/// Check if a function name matches mock/stub patterns (R6).
/// Ported from reference's _MOCK_NAME_RE (case-insensitive):
/// - starts with "mock" followed by underscore or uppercase
/// - ends with "Mock", "Stub", "Fake", or "Spy"
fn is_mock_name(name: &str) -> bool {
    let lower = name.to_lowercase();

    // Prefix: mock_ or mock[A-Z]
    if lower.starts_with("mock") {
        let rest = &name[4..];
        if rest.starts_with('_')
            || rest
                .chars()
                .next()
                .is_some_and(|c| c.is_uppercase())
        {
            return true;
        }
    }

    // Suffix: mock, stub, fake, spy (case-insensitive)
    if lower.ends_with("mock")
        || lower.ends_with("stub")
        || lower.ends_with("fake")
        || lower.ends_with("spy")
    {
        return true;
    }

    false
}
