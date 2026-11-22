// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Pure-Rust Leiden community detection over the call graph.
//!
//! AGENTS.md forbids C++/cmake/libclang dependencies, so community detection
//! ships in-tree rather than via an external crate. This module implements the
//! three phases of the Leiden algorithm (Traag, Waltman & van Eck, 2019):
//!
//!   1. **Local moving** of nodes to the neighbouring community that yields the
//!      largest modularity gain, iterated to convergence.
//!   2. **Refinement** — within each community, nodes are re-partitioned into
//!      well-connected sub-communities. This is the step that repairs Louvain's
//!      "badly connected communities" defect and is what makes this Leiden and
//!      not Louvain.
//!   3. **Aggregation** — the refined partition is collapsed into a
//!      super-node graph and the process repeats until modularity stops
//!      improving.
//!
//! The call graph is treated as **undirected and weighted**: an edge between two
//! symbols has weight equal to the number of directed calls between them in
//! either direction (a mutual call counts as weight 2). Self-loops (a symbol
//! calling itself) are ignored — they do not affect community membership.
//!
//! # Determinism
//!
//! There is **no randomness**. Every iteration visits nodes in a fixed order
//! (ascending internal index, which mirrors the `BTreeMap` key order of the
//! source [`CallGraph`]) and breaks ties by preferring the lowest community
//! index. The same graph therefore yields byte-identical communities across
//! runs, which is what the exact-equality gate in `tests/leiden.rs` asserts.

use std::collections::BTreeMap;

use crate::graph::CallGraph;

/// Resolution parameter for modularity. 1.0 is the classic Newman-Girvan
/// modularity; higher values bias toward more, smaller communities. Emitted in
/// the query envelope (never a magic constant) by the `--communities` intent.
pub const DEFAULT_RESOLUTION: f64 = 1.0;

/// A detected partition of the call graph.
#[derive(Debug, Clone, PartialEq)]
pub struct Communities {
    /// Resolution the partition was computed at (echoed for reproducibility).
    pub resolution: f64,
    /// Final modularity score of the returned partition.
    pub modularity: f64,
    /// One entry per community, sorted deterministically: by descending size,
    /// then by the lexicographically smallest member id (stable tie-break).
    pub communities: Vec<Community>,
}

/// A single community: a set of symbol ids that call each other densely.
#[derive(Debug, Clone, PartialEq)]
pub struct Community {
    /// Stable id assigned after sorting (0-based, dense).
    pub id: usize,
    /// Member symbol ids, sorted ascending for deterministic output.
    pub members: Vec<String>,
}

impl Community {
    /// Number of members in the community.
    #[allow(dead_code)]
    pub fn size(&self) -> usize {
        self.members.len()
    }
}

/// Fraction of the graph above which a community is considered "oversized" and
/// a caller may choose to recursively re-split it (the `--split` behavior in the
/// community-detection proposal). Exposed so the threshold is not magic.
pub const OVERSIZED_FRACTION: f64 = 0.25;

// --- internal weighted-graph representation ---------------------------------

/// A compact, index-based undirected weighted graph derived from a [`CallGraph`].
/// Nodes are dense `0..n` indices whose order mirrors the source `BTreeMap`
/// keys, so all downstream iteration is deterministic.
struct WeightedGraph {
    /// index -> original symbol id (ascending id order).
    ids: Vec<String>,
    /// adjacency: node -> (neighbour -> edge weight). Sorted keys via BTreeMap.
    adj: Vec<BTreeMap<usize, f64>>,
    /// weighted degree (sum of incident edge weights) per node.
    degree: Vec<f64>,
    /// total edge weight m (sum of all edge weights, each undirected edge once).
    total_weight: f64,
}

impl WeightedGraph {
    /// Build the undirected weighted graph from a call graph. Directed call
    /// edges in either direction are summed onto one undirected edge; self-loops
    /// are dropped.
    fn from_call_graph(cg: &CallGraph) -> Self {
        // Stable dense indexing over the BTreeMap key order.
        let ids: Vec<String> = cg.nodes.keys().cloned().collect();
        let index: BTreeMap<&str, usize> =
            ids.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();

        let n = ids.len();
        let mut adj: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); n];

        // Accumulate each directed edge onto the undirected adjacency. Iterating
        // the BTreeMap keeps caller/callee order deterministic.
        for (caller, callees) in &cg.out {
            let Some(&u) = index.get(caller.as_str()) else { continue };
            for callee in callees {
                let Some(&v) = index.get(callee.as_str()) else { continue };
                if u == v {
                    continue; // ignore self-loops
                }
                *adj[u].entry(v).or_insert(0.0) += 1.0;
                *adj[v].entry(u).or_insert(0.0) += 1.0;
            }
        }

        let degree: Vec<f64> = adj.iter().map(|m| m.values().sum()).collect();
        // m = half the summed degree (each undirected edge counted twice above).
        let total_weight: f64 = degree.iter().sum::<f64>() / 2.0;

        WeightedGraph { ids, adj, degree, total_weight }
    }

    fn node_count(&self) -> usize {
        self.ids.len()
    }
}

// --- modularity local-moving on a weighted graph ----------------------------

/// Run local-moving modularity optimization on `g` starting from the singleton
/// partition. Returns `community[node]` labels (not necessarily dense).
///
/// Node visit order is ascending index (deterministic); ties in gain are broken
/// toward the lowest community label already, because we only *move* on a
/// strictly positive gain and seed `best` with the current community.
fn local_move(g: &WeightedGraph, resolution: f64) -> Vec<usize> {
    let n = g.node_count();
    let mut community: Vec<usize> = (0..n).collect();
    // Sum of weighted degrees of all nodes currently in each community.
    let mut comm_degree: Vec<f64> = g.degree.clone();

    let two_m = 2.0 * g.total_weight;
    if two_m == 0.0 {
        return community; // edgeless graph: every node is its own community
    }

    let mut improved = true;
    while improved {
        improved = false;
        for u in 0..n {
            let cu = community[u];
            let du = g.degree[u];

            // Weight from u into each neighbouring community (deterministic order).
            let mut weight_to: BTreeMap<usize, f64> = BTreeMap::new();
            for (&v, &w) in &g.adj[u] {
                *weight_to.entry(community[v]).or_insert(0.0) += w;
            }

            // Remove u from its own community before evaluating gains.
            comm_degree[cu] -= du;
            let w_to_cu = *weight_to.get(&cu).unwrap_or(&0.0);

            // Baseline: staying in cu. gain(c) = w_to_c - resolution*du*commDeg(c)/(2m)
            let mut best_comm = cu;
            let mut best_gain = w_to_cu - resolution * du * comm_degree[cu] / two_m;

            for (&c, &w_to_c) in &weight_to {
                if c == cu {
                    continue;
                }
                let gain = w_to_c - resolution * du * comm_degree[c] / two_m;
                // Strict `>` keeps the lowest-index community on ties (BTreeMap
                // iterates ascending), so the result is order-stable.
                if gain > best_gain {
                    best_gain = gain;
                    best_comm = c;
                }
            }

            // Re-insert u into the chosen community.
            comm_degree[best_comm] += du;
            if best_comm != cu {
                community[u] = best_comm;
                improved = true;
            }
        }
    }

    community
}

/// Relabel arbitrary community labels to a dense `0..k` range, preserving the
/// order in which labels are first seen over ascending node index (stable).
fn densify(labels: &[usize]) -> (Vec<usize>, usize) {
    let mut map: BTreeMap<usize, usize> = BTreeMap::new();
    let mut next = 0usize;
    let mut dense = vec![0usize; labels.len()];
    for (i, &l) in labels.iter().enumerate() {
        let id = *map.entry(l).or_insert_with(|| {
            let v = next;
            next += 1;
            v
        });
        dense[i] = id;
    }
    (dense, next)
}

/// Refinement: within each community from `partition`, run local-moving again
/// but constrained so nodes may only join sub-communities *within their own
/// community*. This produces well-connected sub-communities (the Leiden
/// guarantee). Implemented by masking cross-community edges, then local-moving.
fn refine(g: &WeightedGraph, partition: &[usize], resolution: f64) -> Vec<usize> {
    let n = g.node_count();
    // Build a subgraph view that only keeps intra-community edges, then reuse
    // the same modularity local move. Cross-community edges are dropped so a
    // node can never escape its coarse community during refinement.
    let mut sub = WeightedGraph {
        ids: g.ids.clone(),
        adj: vec![BTreeMap::new(); n],
        degree: vec![0.0; n],
        total_weight: 0.0,
    };
    let mut summed_degree = 0.0;
    for u in 0..n {
        for (&v, &w) in &g.adj[u] {
            if partition[u] == partition[v] {
                sub.adj[u].insert(v, w);
            }
        }
        sub.degree[u] = sub.adj[u].values().sum();
        summed_degree += sub.degree[u];
    }
    sub.total_weight = summed_degree / 2.0;

    local_move(&sub, resolution)
}

/// Collapse `partition` into an aggregated weighted graph whose nodes are the
/// communities. Edge weights (including self-loops representing intra-community
/// weight) are summed. Returns the aggregated graph and, for bookkeeping, the
/// dense community count.
fn aggregate(g: &WeightedGraph, dense: &[usize], k: usize) -> WeightedGraph {
    let mut adj: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); k];
    for u in 0..g.node_count() {
        let cu = dense[u];
        for (&v, &w) in &g.adj[u] {
            let cv = dense[v];
            // Each undirected edge (u,v) is visited from both ends; adding w
            // from each end reconstructs the correct aggregated undirected
            // weight (and doubles self-loops consistently, matching `degree`).
            *adj[cu].entry(cv).or_insert(0.0) += w;
        }
    }
    // Aggregated ids: synthesize stable labels from the smallest member id.
    let mut smallest: Vec<Option<&str>> = vec![None; k];
    for (u, &c) in dense.iter().enumerate() {
        let id = g.ids[u].as_str();
        smallest[c] = Some(match smallest[c] {
            Some(prev) if prev <= id => prev,
            _ => id,
        });
    }
    let ids: Vec<String> = smallest
        .into_iter()
        .map(|o| o.unwrap_or("").to_string())
        .collect();

    let degree: Vec<f64> = adj.iter().map(|m| m.values().sum()).collect();
    let total_weight: f64 = degree.iter().sum::<f64>() / 2.0;
    WeightedGraph { ids, adj, degree, total_weight }
}

/// Modularity of a dense partition on graph `g` at the given resolution.
fn modularity(g: &WeightedGraph, dense: &[usize], k: usize, resolution: f64) -> f64 {
    let two_m = 2.0 * g.total_weight;
    if two_m == 0.0 {
        return 0.0;
    }
    // internal weight and total degree per community.
    let mut internal = vec![0.0f64; k];
    let mut deg = vec![0.0f64; k];
    for u in 0..g.node_count() {
        let cu = dense[u];
        deg[cu] += g.degree[u];
        for (&v, &w) in &g.adj[u] {
            if dense[v] == cu {
                internal[cu] += w; // each intra edge counted from both ends
            }
        }
    }
    let mut q = 0.0;
    for c in 0..k {
        // internal[c] already double-counts intra edges, so divide by 2m once
        // for the edge term; the degree term uses (deg/2m)^2.
        q += internal[c] / two_m - resolution * (deg[c] / two_m).powi(2);
    }
    q
}

/// Detect communities in the call graph with the Leiden algorithm.
///
/// Returns a deterministic [`Communities`] partition. On an empty or edgeless
/// graph every node is its own singleton community and modularity is 0.
pub fn detect(cg: &CallGraph, resolution: f64) -> Communities {
    let base = WeightedGraph::from_call_graph(cg);
    let n = base.node_count();

    // node -> final community label in terms of the base graph's node indices.
    let mut membership: Vec<usize> = (0..n).collect();

    if n == 0 || base.total_weight == 0.0 {
        return finalize(&base, &membership, resolution);
    }

    // Leiden outer loop over aggregation levels.
    let mut current = WeightedGraph::from_call_graph(cg);
    // maps a base-graph node to its node index in `current` (per level).
    let mut base_to_current: Vec<usize> = (0..n).collect();
    let mut prev_q = f64::NEG_INFINITY;

    loop {
        // Phase 1: local moving on the current (aggregated) graph.
        let moved = local_move(&current, resolution);
        // Phase 2: refinement within the resulting communities.
        let refined = refine(&current, &moved, resolution);
        let (dense, k) = densify(&refined);

        // Propagate current-level labels back to base-graph nodes.
        for b in 0..n {
            membership[b] = dense[base_to_current[b]];
        }

        let q = modularity(&base, &membership, community_count(&membership), resolution);

        // Converged when one full level yields no modularity improvement, or the
        // partition collapsed to a single community, or one-node-per-community.
        if k == current.node_count() || q <= prev_q + f64::EPSILON {
            break;
        }
        prev_q = q;

        // Phase 3: aggregate and remap for the next level.
        let next = aggregate(&current, &dense, k);
        for b in 0..n {
            base_to_current[b] = dense[base_to_current[b]];
        }
        current = next;

        if current.node_count() <= 1 {
            break;
        }
    }

    finalize(&base, &membership, resolution)
}

/// Number of distinct labels present in `membership`.
fn community_count(membership: &[usize]) -> usize {
    let (_, k) = densify(membership);
    k
}

/// Turn a base-graph membership vector into a sorted, dense [`Communities`].
fn finalize(base: &WeightedGraph, membership: &[usize], resolution: f64) -> Communities {
    let (dense, k) = densify(membership);
    let q = modularity(base, &dense, k, resolution);

    // Gather members per community as sorted id lists.
    let mut buckets: Vec<Vec<String>> = vec![Vec::new(); k];
    for (i, &c) in dense.iter().enumerate() {
        buckets[c].push(base.ids[i].clone());
    }
    for b in &mut buckets {
        b.sort();
    }

    // Deterministic community order: largest first, then by smallest member id.
    buckets.sort_by(|a, b| {
        b.len()
            .cmp(&a.len())
            .then_with(|| a.first().cmp(&b.first()))
    });

    let communities: Vec<Community> = buckets
        .into_iter()
        .enumerate()
        .map(|(id, members)| Community { id, members })
        .collect();

    Communities { resolution, modularity: q, communities }
}

/// Modularity of an explicit partition (member-id groups) on `cg`.
/// Used by `--split`, which refines a Leiden partition and needs the score of
/// the partition it actually returns. `resolution` is the same knob `detect` uses.
pub fn modularity_of(cg: &CallGraph, groups: &[Vec<String>], resolution: f64) -> f64 {
    if groups.is_empty() || cg.nodes.is_empty() {
        return 0.0;
    }
    let base = WeightedGraph::from_call_graph(cg);
    let index: BTreeMap<&str, usize> = base
        .ids
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_str(), i))
        .collect();
    let mut dense = vec![0usize; base.node_count()];
    for (c, members) in groups.iter().enumerate() {
        for m in members {
            if let Some(&i) = index.get(m.as_str()) {
                dense[i] = c;
            }
        }
    }
    modularity(&base, &dense, groups.len(), resolution)
}
