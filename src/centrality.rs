// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Brandes betweenness centrality over the call graph.
//!
//! Directed, unnormalized: for every ordered pair of distinct nodes, a node
//! scores the fraction of shortest paths between them that pass through it
//! (endpoints excluded). Iteration is BTreeMap / sorted-neighbour order, so
//! the same graph yields the same scores. Distinct from degree-based
//! `--hotspots`.

use std::collections::{BTreeMap, VecDeque};

use crate::graph::CallGraph;

/// A chokepoint: a node whose removal would sit on shortest paths between others.
#[derive(Debug, Clone)]
pub struct BridgeHit {
    pub path: String,
    pub name: String,
    pub betweenness: f64,
}

/// Brandes' algorithm. Nodes with betweenness 0 are omitted (they are not
/// chokepoints). Sorted by betweenness desc, then path, then name.
pub fn bridges(cg: &CallGraph, limit: usize) -> Vec<BridgeHit> {
    let ids: Vec<&str> = cg.nodes.keys().map(|s| s.as_str()).collect();
    let index: BTreeMap<&str, usize> = ids.iter().copied().enumerate().map(|(i, s)| (s, i)).collect();
    let n = ids.len();
    if n == 0 || limit == 0 {
        return Vec::new();
    }

    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (caller, callees) in &cg.out {
        let Some(&u) = index.get(caller.as_str()) else {
            continue;
        };
        let mut vs: Vec<usize> = callees
            .iter()
            .filter_map(|c| index.get(c.as_str()).copied())
            .filter(|v| *v != u)
            .collect();
        vs.sort_unstable();
        vs.dedup();
        adj[u] = vs;
    }

    let mut cb = vec![0.0f64; n];
    for s in 0..n {
        let mut stack: Vec<usize> = Vec::new();
        let mut pred: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut sigma = vec![0.0f64; n];
        let mut dist = vec![-1i32; n];
        sigma[s] = 1.0;
        dist[s] = 0;
        let mut queue = VecDeque::new();
        queue.push_back(s);
        while let Some(v) = queue.pop_front() {
            stack.push(v);
            for &w in &adj[v] {
                if dist[w] < 0 {
                    dist[w] = dist[v] + 1;
                    queue.push_back(w);
                }
                if dist[w] == dist[v] + 1 {
                    sigma[w] += sigma[v];
                    pred[w].push(v);
                }
            }
        }
        let mut delta = vec![0.0f64; n];
        while let Some(w) = stack.pop() {
            for &v in &pred[w] {
                if sigma[w] > 0.0 {
                    delta[v] += (sigma[v] / sigma[w]) * (1.0 + delta[w]);
                }
            }
            if w != s {
                cb[w] += delta[w];
            }
        }
    }

    let mut hits: Vec<BridgeHit> = cb
        .into_iter()
        .enumerate()
        .filter(|(_, score)| *score > 0.0)
        .filter_map(|(i, score)| {
            let node = cg.nodes.get(ids[i])?;
            Some(BridgeHit {
                path: node.path.clone(),
                name: node.name.clone(),
                betweenness: score,
            })
        })
        .collect();
    hits.sort_by(|a, b| {
        b.betweenness
            .partial_cmp(&a.betweenness)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.name.cmp(&b.name))
    });
    hits.truncate(limit);
    hits
}
