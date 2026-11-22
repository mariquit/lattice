// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Diff review scoring: compose impact, source-class, and ownership against a
//! git range.
//!
//! `risk = transitive_caller_count × file_churn`. Both factors are emitted
//! (`transitive_callers`, and churn via the ownership layer) so the product is
//! tunable, not magic. Read-only: this module never writes the store.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::Path;

use gix::bstr::ByteSlice;
use imara_diff::intern::InternedInput;
use imara_diff::{diff, Algorithm, Sink};

use crate::class::{self, SourceClass};
use crate::graph::{self, CallGraph};
use crate::ingest::{self, CallEvidence};
use crate::languages::{self, LanguageRegistry};
use crate::ownership;
use crate::Result;

/// Documented risk: transitive caller count times the file's git churn.
pub fn risk(transitive_callers: usize, churn: u64) -> u64 {
    (transitive_callers as u64).saturating_mul(churn)
}

#[derive(Debug, Clone)]
pub struct ChangedFile {
    pub path: String,
    pub symbols: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Affected {
    pub symbol: String,
    pub path: String,
    pub callers: usize,
    pub transitive_callers: usize,
    pub risk: u64,
}

#[derive(Debug, Clone)]
pub struct TestGap {
    pub symbol: String,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct PathOwners {
    pub path: String,
    pub owners: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Review {
    pub changed: Vec<ChangedFile>,
    pub affected: Vec<Affected>,
    pub test_gaps: Vec<TestGap>,
    pub owners: Vec<PathOwners>,
}

#[derive(Default)]
struct LineSink {
    before: Vec<Range<u32>>,
    after: Vec<Range<u32>>,
}

impl Sink for LineSink {
    type Out = Self;

    fn process_change(&mut self, before: Range<u32>, after: Range<u32>) {
        self.before.push(before);
        self.after.push(after);
    }

    fn finish(self) -> Self::Out {
        self
    }
}

fn changed_lines(old_bytes: &[u8], new_bytes: &[u8]) -> LineSink {
    let input = InternedInput::new(old_bytes, new_bytes);
    diff(
        Algorithm::Histogram,
        &input,
        LineSink::default(),
    )
}

fn collect_changed_symbols(
    path: &str,
    bytes: &[u8],
    hunks: &[Range<u32>],
    symbols: &mut BTreeSet<String>,
) -> Result<()> {
    if hunks.iter().all(Range::is_empty) {
        return Ok(());
    }
    // Match ingestion's lossy UTF-8 decoding and convert the parser's inclusive
    // one-based spans to half-open zero-based ranges for the diff.
    let facts = ingest::parse_file(path, &String::from_utf8_lossy(bytes))?;
    for f in facts.functions {
        let start = f.start_line.saturating_sub(1);
        let end = f.end_line.max(f.start_line);
        if hunks.iter().any(|h| {
            !h.is_empty() && (h.start as usize) < end && start < (h.end as usize)
        }) {
            symbols.insert(f.name);
        }
    }
    Ok(())
}

fn blob_bytes(repo: &gix::Repository, id: Option<gix::ObjectId>) -> Result<Vec<u8>> {
    let Some(id) = id else {
        return Ok(Vec::new());
    };
    repo.find_object(id)
        .map(|o| o.data.to_vec())
        .map_err(|e| format!("read diff blob {id}: {e}").into())
}

fn commit_tree<'a>(
    repo: &'a gix::Repository,
    spec: &str,
) -> Result<gix::ObjectId> {
    let id = repo
        .rev_parse_single(spec)
        .map_err(|e| format!("unresolved ref {spec:?}: {e}"))?
        .detach();
    let obj = repo
        .find_object(id)
        .map_err(|e| format!("resolve {spec:?}: {e}"))?;
    let commit = obj
        .peel_to_commit()
        .map_err(|e| format!("peel {spec:?}: {e}"))?;
    let decoded = commit
        .decode()
        .map_err(|e| format!("decode {spec:?}: {e}"))?;
    Ok(decoded.tree())
}

/// Parse `ref` as `A..B` or a single commit (that commit vs its first parent).
fn range_trees(repo: &gix::Repository, spec: &str) -> Result<(gix::ObjectId, gix::ObjectId)> {
    if let Some((from, to)) = spec.split_once("..") {
        let from = if from.is_empty() {
            gix::hash::ObjectId::empty_tree(repo.object_hash())
        } else {
            commit_tree(repo, from)?
        };
        let to = if to.is_empty() {
            commit_tree(repo, "HEAD")?
        } else {
            commit_tree(repo, to)?
        };
        return Ok((from, to));
    }
    let id = repo
        .rev_parse_single(spec)
        .map_err(|e| format!("unresolved ref {spec:?}: {e}"))?
        .detach();
    let obj = repo
        .find_object(id)
        .map_err(|e| format!("resolve {spec:?}: {e}"))?;
    let commit = obj
        .peel_to_commit()
        .map_err(|e| format!("peel {spec:?}: {e}"))?;
    let decoded = commit
        .decode()
        .map_err(|e| format!("decode {spec:?}: {e}"))?;
    let to = decoded.tree();
    let parent_tree = decoded.parents().next().map(|p| p.into()).and_then(|pid: gix::ObjectId| {
        let obj = repo.find_object(pid).ok()?;
        let commit = obj.peel_to_commit().ok()?;
        let decoded = commit.decode().ok()?;
        Some(decoded.tree())
    });
    let from = parent_tree.unwrap_or_else(|| gix::hash::ObjectId::empty_tree(repo.object_hash()));
    Ok((from, to))
}

fn changed_files(
    repo: &gix::Repository,
    from: gix::ObjectId,
    to: gix::ObjectId,
    registry: &LanguageRegistry,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    if from == to {
        return Ok(BTreeMap::new());
    }
    let from_tree = repo
        .find_object(from)
        .map_err(|e| format!("from tree: {e}"))?
        .into_tree();
    let to_tree = repo
        .find_object(to)
        .map_err(|e| format!("to tree: {e}"))?
        .into_tree();

    let mut files: Vec<(String, Option<gix::ObjectId>, Option<gix::ObjectId>)> = Vec::new();
    from_tree.changes()?
        // Treat renames as a deletion and addition so both paths are reported.
        .options(|opts| {
            opts.track_rewrites(None);
        })
        .for_each_to_obtain_tree(&to_tree, |change| {
            use gix::object::tree::diff::Change;
            match &change {
                Change::Addition {
                    entry_mode,
                    id,
                    location,
                    ..
                } if entry_mode.is_blob() => {
                    files.push((
                        location.to_str_lossy().into_owned(),
                        None,
                        Some(id.detach()),
                    ));
                }
                Change::Modification {
                    entry_mode,
                    id,
                    previous_id,
                    location,
                    ..
                } if entry_mode.is_blob() => {
                    files.push((
                        location.to_str_lossy().into_owned(),
                        Some(previous_id.detach()),
                        Some(id.detach()),
                    ));
                }
                Change::Deletion {
                    entry_mode,
                    id,
                    location,
                    ..
                } if entry_mode.is_blob() => {
                    files.push((
                        location.to_str_lossy().into_owned(),
                        Some(id.detach()),
                        None,
                    ));
                }
                _ => {}
            }
            Ok::<_, std::convert::Infallible>(gix::object::tree::diff::Action::Continue)
        })?;

    let _active = languages::activate(registry);
    let mut changed = BTreeMap::new();
    for (path, old_id, new_id) in files {
        let mut symbols = BTreeSet::new();
        if ingest::is_builtin_ext(&path) || registry.for_path(&path).is_some() {
            let old = blob_bytes(repo, old_id)?;
            let new = blob_bytes(repo, new_id)?;
            let hunks = changed_lines(&old, &new);
            collect_changed_symbols(&path, &old, &hunks.before, &mut symbols)?;
            collect_changed_symbols(&path, &new, &hunks.after, &mut symbols)?;
        }
        changed.insert(path, symbols);
    }
    Ok(changed)
}

fn is_test_class(path: &str) -> bool {
    matches!(
        class::classify(path),
        SourceClass::Test | SourceClass::Fixture
    )
}

/// Score `spec` (`commit`, `branch`, or `A..B`) against the indexed graph.
pub fn review(root: &Path, lattice_dir: &Path, cg: &CallGraph, spec: &str) -> Result<Review> {
    if !root.join(".git").exists() {
        return Err("review requires a git repository".into());
    }
    let repo = gix::open(root).map_err(|e| format!("open git repo: {e}"))?;
    let (from, to) = range_trees(&repo, spec)?;
    let registry = languages::load_stored(lattice_dir)?;
    let changed_files = changed_files(&repo, from, to, &registry)?;
    let churn = ownership::churn_map(lattice_dir).unwrap_or_default();

    let mut changed: Vec<ChangedFile> = Vec::new();
    let mut affected: Vec<Affected> = Vec::new();
    let mut test_gaps: Vec<TestGap> = Vec::new();
    let mut owners_out: Vec<PathOwners> = Vec::new();

    for (path, symbols) in changed_files {
        // Scoring still uses the live graph. A historical symbol may have no
        // current node, but it still belongs in the changed list.
        for symbol in &symbols {
            let id = format!("{path}::{symbol}");
            let prefix = format!("{path}::");
            let suffix = format!("::{symbol}");
            let ids = cg
                .nodes
                .keys()
                .filter(|k| *k == &id || k.ends_with(&suffix) && k.starts_with(&prefix))
                .cloned()
                .collect::<BTreeSet<_>>();
            let target_ids = if ids.is_empty() {
                // Fall back to any node with this path+name.
                cg.nodes
                    .values()
                    .filter(|n| n.path == path && n.name == *symbol)
                    .map(|n| n.id.clone())
                    .collect()
            } else {
                ids
            };
            if target_ids.is_empty() {
                continue;
            }
            let direct = graph::callers_min(cg, &target_ids, CallEvidence::Name);
            let trans = graph::transitive_callers_min(cg, &target_ids, CallEvidence::Name);
            let file_churn = churn.get(&path).copied().unwrap_or(0);
            let trans_n = trans.len();
            affected.push(Affected {
                symbol: symbol.clone(),
                path: path.clone(),
                callers: direct.len(),
                transitive_callers: trans_n,
                risk: risk(trans_n, file_churn),
            });
            let tested = direct.iter().any(|(node, _)| is_test_class(&node.path))
                || trans.iter().any(|node| is_test_class(&node.path));
            if !tested {
                test_gaps.push(TestGap {
                    symbol: symbol.clone(),
                    path: path.clone(),
                });
            }
        }
        changed.push(ChangedFile {
            path: path.clone(),
            symbols: symbols.into_iter().collect(),
        });
        let report = ownership::owners(lattice_dir, &path).unwrap_or(ownership::OwnersReport {
            owners: Vec::new(),
            churn: 0,
            co_changed: Vec::new(),
        });
        let mut authors: Vec<String> = report.owners.into_iter().map(|o| o.author).collect();
        authors.sort();
        authors.dedup();
        owners_out.push(PathOwners {
            path: path.clone(),
            owners: authors,
        });
    }

    changed.sort_by(|a, b| a.path.cmp(&b.path));
    affected.sort_by(|a, b| {
        b.risk
            .cmp(&a.risk)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    test_gaps.sort_by(|a, b| a.path.cmp(&b.path).then(a.symbol.cmp(&b.symbol)));
    owners_out.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(Review {
        changed,
        affected,
        test_gaps,
        owners: owners_out,
    })
}
