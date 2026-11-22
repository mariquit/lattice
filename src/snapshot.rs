// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Content-addressed graph snapshots in `meta.db`, and the diff between two.
//!
//! A snapshot is a manifest (per-node id+hash, per-edge from/to/kind, plus
//! community member sets) — never a second copy of the graph. The label
//! `"working"` is reserved for the live graph and is rejected as a saved name.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::graph::CallGraph;
use crate::leiden;
use crate::meta::{self, META_DB};
use crate::Result;

/// Reserved label for the live graph. Never a saved snapshot.
pub const WORKING: &str = "working";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapNode {
    pub id: String,
    pub kind: String,
    pub path: String,
    pub name: String,
    pub line: usize,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapEdge {
    pub from: String,
    pub to: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapCommunity {
    pub id: usize,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub nodes: Vec<SnapNode>,
    pub edges: Vec<SnapEdge>,
    pub communities: Vec<SnapCommunity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeChange {
    pub id: String,
    pub field: String,
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphDiff {
    pub nodes_added: Vec<SnapNode>,
    pub nodes_removed: Vec<SnapNode>,
    pub nodes_changed: Vec<NodeChange>,
    pub edges_added: Vec<SnapEdge>,
    pub edges_removed: Vec<SnapEdge>,
    pub communities_added: Vec<usize>,
    pub communities_removed: Vec<usize>,
    pub communities_merged: Vec<(usize, usize)>,
    pub communities_split: Vec<(usize, usize)>,
}

/// Freeze the live call graph into a deterministic manifest.
pub fn capture(cg: &CallGraph) -> Manifest {
    let mut nodes: Vec<SnapNode> = cg
        .nodes
        .values()
        .map(|n| {
            let kind = "function".to_string();
            let hash = meta::content_hash(
                format!("{kind}|{}|{}|{}", n.path, n.name, n.line).as_bytes(),
            );
            SnapNode {
                id: n.id.clone(),
                kind,
                path: n.path.clone(),
                name: n.name.clone(),
                line: n.line,
                hash,
            }
        })
        .collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));

    let mut edges: Vec<SnapEdge> = Vec::new();
    for (from, callees) in &cg.out {
        for to in callees {
            edges.push(SnapEdge {
                from: from.clone(),
                to: to.clone(),
                kind: "calls".to_string(),
            });
        }
    }
    edges.sort_by(|a, b| {
        a.from
            .cmp(&b.from)
            .then(a.to.cmp(&b.to))
            .then(a.kind.cmp(&b.kind))
    });
    edges.dedup();

    let detected = leiden::detect(cg, leiden::DEFAULT_RESOLUTION);
    let communities = detected
        .communities
        .into_iter()
        .map(|c| SnapCommunity {
            id: c.id,
            members: c.members,
        })
        .collect();

    Manifest {
        nodes,
        edges,
        communities,
    }
}

fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS graph_snapshots (
            label TEXT PRIMARY KEY,
            manifest TEXT NOT NULL
        );",
    )
    .map_err(|e| format!("create graph_snapshots: {e}"))?;
    Ok(())
}

/// Persist a manifest under `label`. Rejects the reserved label `"working"`.
pub fn save(lattice_dir: &Path, label: &str, manifest: &Manifest) -> Result<()> {
    if label == WORKING {
        return Err(format!("label {WORKING:?} is reserved for the live graph").into());
    }
    if label.is_empty() || label.contains('/') || label.contains('\\') {
        return Err(format!("invalid snapshot label {label:?}").into());
    }
    let conn = Connection::open(lattice_dir.join(META_DB))
        .map_err(|e| format!("open meta.db: {e}"))?;
    ensure_schema(&conn)?;
    let json = serde_json::to_string(manifest).map_err(|e| format!("encode snapshot: {e}"))?;
    conn.execute(
        "INSERT OR REPLACE INTO graph_snapshots (label, manifest) VALUES (?1, ?2)",
        params![label, json],
    )
    .map_err(|e| format!("write snapshot: {e}"))?;
    Ok(())
}

pub fn load(lattice_dir: &Path, label: &str) -> Result<Manifest> {
    let conn = Connection::open(lattice_dir.join(META_DB))
        .map_err(|e| format!("open meta.db: {e}"))?;
    ensure_schema(&conn)?;
    let json: String = conn
        .query_row(
            "SELECT manifest FROM graph_snapshots WHERE label = ?1",
            [label],
            |row| row.get(0),
        )
        .map_err(|_| format!("snapshot {label:?} not found"))?;
    serde_json::from_str(&json).map_err(|e| format!("decode snapshot {label:?}: {e}").into())
}

/// Resolve a label to a manifest. `"working"` is the live graph.
pub fn resolve(lattice_dir: &Path, cg: &CallGraph, label: &str) -> Result<Manifest> {
    if label == WORKING {
        Ok(capture(cg))
    } else {
        load(lattice_dir, label)
    }
}

fn edge_key(e: &SnapEdge) -> String {
    format!("{}->{}:{}", e.from, e.to, e.kind)
}

/// Set-difference over ids / edge keys, plus a field-level compare for nodes
/// present in both. Output is sorted (id, then field).
pub fn diff(from: &Manifest, to: &Manifest) -> GraphDiff {
    let from_nodes: BTreeMap<&str, &SnapNode> =
        from.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let to_nodes: BTreeMap<&str, &SnapNode> = to.nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    let mut nodes_added: Vec<SnapNode> = to_nodes
        .iter()
        .filter(|(id, _)| !from_nodes.contains_key(*id))
        .map(|(_, n)| (*n).clone())
        .collect();
    nodes_added.sort_by(|a, b| a.id.cmp(&b.id));

    let mut nodes_removed: Vec<SnapNode> = from_nodes
        .iter()
        .filter(|(id, _)| !to_nodes.contains_key(*id))
        .map(|(_, n)| (*n).clone())
        .collect();
    nodes_removed.sort_by(|a, b| a.id.cmp(&b.id));

    let mut nodes_changed: Vec<NodeChange> = Vec::new();
    for (id, before) in &from_nodes {
        let Some(after) = to_nodes.get(id) else {
            continue;
        };
        if before.hash == after.hash {
            continue;
        }
        let fields = [
            ("kind", before.kind.as_str(), after.kind.as_str()),
            ("path", before.path.as_str(), after.path.as_str()),
            ("name", before.name.as_str(), after.name.as_str()),
            ("line", &before.line.to_string(), &after.line.to_string()),
        ];
        for (field, b, a) in fields {
            if b != a {
                nodes_changed.push(NodeChange {
                    id: (*id).to_string(),
                    field: field.to_string(),
                    before: b.to_string(),
                    after: a.to_string(),
                });
            }
        }
    }
    nodes_changed.sort_by(|a, b| a.id.cmp(&b.id).then(a.field.cmp(&b.field)));

    let from_edges: BTreeMap<String, &SnapEdge> =
        from.edges.iter().map(|e| (edge_key(e), e)).collect();
    let to_edges: BTreeMap<String, &SnapEdge> = to.edges.iter().map(|e| (edge_key(e), e)).collect();

    let mut edges_added: Vec<SnapEdge> = to_edges
        .iter()
        .filter(|(k, _)| !from_edges.contains_key(*k))
        .map(|(_, e)| (*e).clone())
        .collect();
    edges_added.sort_by(|a, b| edge_key(a).cmp(&edge_key(b)));
    let mut edges_removed: Vec<SnapEdge> = from_edges
        .iter()
        .filter(|(k, _)| !to_edges.contains_key(*k))
        .map(|(_, e)| (*e).clone())
        .collect();
    edges_removed.sort_by(|a, b| edge_key(a).cmp(&edge_key(b)));

    let (communities_added, communities_removed, communities_merged, communities_split) =
        diff_communities(&from.communities, &to.communities);

    GraphDiff {
        nodes_added,
        nodes_removed,
        nodes_changed,
        edges_added,
        edges_removed,
        communities_added,
        communities_removed,
        communities_merged,
        communities_split,
    }
}

fn signature(members: &[String]) -> String {
    members.join("\n")
}

fn diff_communities(
    from: &[SnapCommunity],
    to: &[SnapCommunity],
) -> (Vec<usize>, Vec<usize>, Vec<(usize, usize)>, Vec<(usize, usize)>) {
    let from_sig: BTreeMap<String, usize> = from
        .iter()
        .map(|c| (signature(&c.members), c.id))
        .collect();
    let to_sig: BTreeMap<String, usize> = to.iter().map(|c| (signature(&c.members), c.id)).collect();

    let mut added: Vec<usize> = to
        .iter()
        .filter(|c| !from_sig.contains_key(&signature(&c.members)))
        .map(|c| c.id)
        .collect();
    added.sort_unstable();
    let mut removed: Vec<usize> = from
        .iter()
        .filter(|c| !to_sig.contains_key(&signature(&c.members)))
        .map(|c| c.id)
        .collect();
    removed.sort_unstable();

    // merged: two or more `from` communities whose member union equals one `to`
    // community. split: one `from` community partitioned across two or more `to`
    // communities. Ids are the snapshot-local ids (from-ids for merged pairs,
    // to-ids for split pairs).
    let mut merged: Vec<(usize, usize)> = Vec::new();
    let mut split: Vec<(usize, usize)> = Vec::new();

    let from_sets: Vec<(usize, BTreeSet<&str>)> = from
        .iter()
        .map(|c| (c.id, c.members.iter().map(|s| s.as_str()).collect()))
        .collect();
    let to_sets: Vec<(usize, BTreeSet<&str>)> = to
        .iter()
        .map(|c| (c.id, c.members.iter().map(|s| s.as_str()).collect()))
        .collect();

    for (_to_id, to_members) in &to_sets {
        let parts: Vec<usize> = from_sets
            .iter()
            .filter(|(_, members)| !members.is_empty() && members.is_subset(to_members))
            .map(|(id, _)| *id)
            .collect();
        if parts.len() >= 2 {
            let mut union: BTreeSet<&str> = BTreeSet::new();
            for id in &parts {
                if let Some((_, members)) = from_sets.iter().find(|(i, _)| i == id) {
                    union.extend(members.iter().copied());
                }
            }
            if union == *to_members {
                for pair in parts.windows(2) {
                    merged.push((pair[0], pair[1]));
                }
            }
        }
    }
    for (_from_id, from_members) in &from_sets {
        let parts: Vec<usize> = to_sets
            .iter()
            .filter(|(_, members)| !members.is_empty() && members.is_subset(from_members))
            .map(|(id, _)| *id)
            .collect();
        if parts.len() >= 2 {
            let mut union: BTreeSet<&str> = BTreeSet::new();
            for id in &parts {
                if let Some((_, members)) = to_sets.iter().find(|(i, _)| i == id) {
                    union.extend(members.iter().copied());
                }
            }
            if union == *from_members {
                for pair in parts.windows(2) {
                    split.push((pair[0], pair[1]));
                }
            }
        }
    }
    merged.sort_unstable();
    merged.dedup();
    split.sort_unstable();
    split.dedup();

    (added, removed, merged, split)
}
