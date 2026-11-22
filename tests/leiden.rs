// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Exact-partition gate for the pure-Rust Leiden implementation (`src/leiden.rs`).
//!
//! Test gate: `cargo test --test leiden`
//!
//! Ground truth is hand-counted and asserted with `assert_eq!` (never `>=`),
//! per the quality bar in AGENTS.md. Every graph here is built in-memory from
//! `graph::CallGraph`'s public fields, so the test has no filesystem or store
//! dependency and exercises `leiden::detect` directly.

use std::collections::{BTreeMap, BTreeSet};

use lattice::graph::{CallGraph, SymbolNode};
use lattice::leiden::{self, DEFAULT_RESOLUTION};

/// Build a `CallGraph` from a list of directed `(caller, callee)` edges.
/// Node ids double as names; path/line are synthesized deterministically so the
/// nodes are well-formed without affecting clustering (Leiden only reads edges).
fn call_graph(edges: &[(&str, &str)]) -> CallGraph {
    let mut names: BTreeSet<String> = BTreeSet::new();
    for (a, b) in edges {
        names.insert((*a).to_string());
        names.insert((*b).to_string());
    }

    let mut nodes: BTreeMap<String, SymbolNode> = BTreeMap::new();
    for (i, name) in names.iter().enumerate() {
        nodes.insert(
            name.clone(),
            SymbolNode {
                id: name.clone(),
                name: name.clone(),
                path: format!("src/{name}.rs"),
                line: i + 1,
                parent: None,
            },
        );
    }

    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut inn: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (a, b) in edges {
        out.entry((*a).to_string()).or_default().insert((*b).to_string());
        inn.entry((*b).to_string()).or_default().insert((*a).to_string());
    }

    CallGraph {
        nodes,
        out,
        inn,
        evidence: BTreeMap::new(),
        sites: BTreeMap::new(),
    }
}

/// Return community membership as a set of sorted member-id vectors, so the
/// assertion is independent of the community ordering/id assignment.
fn membership_sets(c: &leiden::Communities) -> BTreeSet<Vec<String>> {
    c.communities.iter().map(|com| com.members.clone()).collect()
}

/// Two dense triangles joined by a single bridge edge → exactly two
/// communities, each the hand-counted triangle.
#[test]
fn two_clusters_one_bridge() {
    // Cluster A: a1,a2,a3 mutually call (triangle). Cluster B likewise.
    // Single bridge a1 -> b1 is the only inter-cluster edge.
    let cg = call_graph(&[
        // cluster A triangle (both directions for strong intra weight)
        ("a1", "a2"), ("a2", "a1"),
        ("a2", "a3"), ("a3", "a2"),
        ("a3", "a1"), ("a1", "a3"),
        // cluster B triangle
        ("b1", "b2"), ("b2", "b1"),
        ("b2", "b3"), ("b3", "b2"),
        ("b3", "b1"), ("b1", "b3"),
        // the bridge
        ("a1", "b1"),
    ]);

    let result = leiden::detect(&cg, DEFAULT_RESOLUTION);

    // Exactly two communities.
    assert_eq!(result.communities.len(), 2, "expected exactly two communities");

    // Exact membership (order-independent).
    let expected: BTreeSet<Vec<String>> = [
        vec!["a1".to_string(), "a2".to_string(), "a3".to_string()],
        vec!["b1".to_string(), "b2".to_string(), "b3".to_string()],
    ]
    .into_iter()
    .collect();
    assert_eq!(membership_sets(&result), expected);

    // Sizes are hand-counted 3 and 3.
    let mut sizes: Vec<usize> = result.communities.iter().map(|c| c.size()).collect();
    sizes.sort();
    assert_eq!(sizes, vec![3, 3]);

    // Modularity of a clean two-cluster split is strictly positive.
    assert!(result.modularity > 0.0, "modularity should be positive, got {}", result.modularity);

    // Resolution is echoed exactly.
    assert_eq!(result.resolution, DEFAULT_RESOLUTION);
}

/// Determinism: the same graph yields byte-identical `Communities` across two
/// independent calls (no randomness, fixed iteration order).
#[test]
fn deterministic_across_runs() {
    let cg = call_graph(&[
        ("a1", "a2"), ("a2", "a1"),
        ("a2", "a3"), ("a3", "a2"),
        ("b1", "b2"), ("b2", "b1"),
        ("b2", "b3"), ("b3", "b2"),
        ("a1", "b1"),
    ]);

    let first = leiden::detect(&cg, DEFAULT_RESOLUTION);
    let second = leiden::detect(&cg, DEFAULT_RESOLUTION);

    // `Communities` derives PartialEq; identical input ⇒ identical output.
    assert_eq!(first, second);
}

/// A single node with no edges is its own singleton community, modularity 0.
#[test]
fn singleton_no_edges() {
    let mut nodes: BTreeMap<String, SymbolNode> = BTreeMap::new();
    nodes.insert(
        "lonely".to_string(),
        SymbolNode {
            id: "lonely".to_string(),
            name: "lonely".to_string(),
            path: "src/lonely.rs".to_string(),
            line: 1,
            parent: None,
        },
    );
    let cg = CallGraph {
        nodes,
        out: BTreeMap::new(),
        inn: BTreeMap::new(),
        evidence: BTreeMap::new(),
        sites: BTreeMap::new(),
    };

    let result = leiden::detect(&cg, DEFAULT_RESOLUTION);

    assert_eq!(result.communities.len(), 1);
    assert_eq!(result.communities[0].members, vec!["lonely".to_string()]);
    assert_eq!(result.modularity, 0.0);
}

/// An empty graph yields no communities and modularity 0 (no panic).
#[test]
fn empty_graph() {
    let cg = CallGraph {
        nodes: BTreeMap::new(),
        out: BTreeMap::new(),
        inn: BTreeMap::new(),
        evidence: BTreeMap::new(),
        sites: BTreeMap::new(),
    };

    let result = leiden::detect(&cg, DEFAULT_RESOLUTION);

    assert_eq!(result.communities.len(), 0);
    assert_eq!(result.modularity, 0.0);
}

/// Three clusters, each a triangle, chained by two single bridge edges →
/// exactly three communities with the hand-counted membership.
#[test]
fn three_clusters_chained() {
    let cg = call_graph(&[
        ("a1", "a2"), ("a2", "a1"), ("a2", "a3"), ("a3", "a2"), ("a3", "a1"), ("a1", "a3"),
        ("b1", "b2"), ("b2", "b1"), ("b2", "b3"), ("b3", "b2"), ("b3", "b1"), ("b1", "b3"),
        ("c1", "c2"), ("c2", "c1"), ("c2", "c3"), ("c3", "c2"), ("c3", "c1"), ("c1", "c3"),
        // bridges chaining A-B-C
        ("a1", "b1"),
        ("b3", "c1"),
    ]);

    let result = leiden::detect(&cg, DEFAULT_RESOLUTION);

    assert_eq!(result.communities.len(), 3, "expected exactly three communities");

    let expected: BTreeSet<Vec<String>> = [
        vec!["a1".to_string(), "a2".to_string(), "a3".to_string()],
        vec!["b1".to_string(), "b2".to_string(), "b3".to_string()],
        vec!["c1".to_string(), "c2".to_string(), "c3".to_string()],
    ]
    .into_iter()
    .collect();
    assert_eq!(membership_sets(&result), expected);
}
