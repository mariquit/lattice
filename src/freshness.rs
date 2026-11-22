// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Query-time staleness check.
//!
//! Runs inside `router::route` after the `.lattice/` guard and BEFORE any
//! dispatch, for every query variant. It compares each source file's
//! filesystem mtime against its stored `indexed_at` (whole-second, INCLUSIVE:
//! `mtime >= indexed_at`), content-hashes only the suspicious files, and — if
//! any file is changed / added / removed — synchronously re-indexes through the
//! SAME `index::run` pipeline `lattice init` uses. No background process, no
//! poll (B12.1).
//!
//! Why `>=` and not `>` (correctness, not taste): `indexed_at` and mtime only
//! agree to the second once truncated. With strict `>`, a file mutated within
//! the same second as its indexing would be silently missed — an unfixable
//! flake in any fast test and a real race. The inclusive rule can only produce
//! false *suspicions*, resolved by one cheap hash; it can never produce a false
//! *trust*. So immediately after a refresh the mutated file may stay suspicious
//! (its mtime equals its new `indexed_at` second) — the hash then matches, and
//! the next query correctly reports `current`. Do NOT "optimize" to strict `>`.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::index;
use crate::meta;
use crate::vectors;
use crate::Result;

/// Result of the freshness check: what the query envelope reports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Freshness {
    /// The store was already up to date; nothing was re-indexed.
    Current,
    /// This invocation re-indexed >= 1 changed / added / removed file.
    Refreshed,
}

impl Freshness {
    /// The additive envelope string value.
    pub fn as_str(self) -> &'static str {
        match self {
            Freshness::Current => "current",
            Freshness::Refreshed => "refreshed",
        }
    }
}

/// Whole-second UNIX timestamp of a filesystem mtime (truncated, floor).
fn mtime_secs(meta: &std::fs::Metadata) -> Option<i64> {
    let mtime = meta.modified().ok()?;
    match mtime.duration_since(UNIX_EPOCH) {
        Ok(d) => Some(d.as_secs() as i64),
        // mtime before the epoch: represent as a negative whole second (floor).
        Err(e) => Some(-(e.duration().as_secs() as i64)),
    }
}

/// Whole-second UNIX timestamp of an RFC 3339 `indexed_at` string (floor).
fn indexed_secs(indexed_at: &str) -> Option<i64> {
    let dt = chrono::DateTime::parse_from_rfc3339(indexed_at).ok()?;
    Some(dt.timestamp())
}

/// Ensure the store is fresh before a query answers, self-healing any changed /
/// added / removed source file synchronously. Returns whether a refresh ran.
///
/// A refresh failure is an error (fail loud — never answer from a store known
/// to be stale); the router maps it to `Routed::Internal`.
pub fn ensure_fresh(lattice_dir: &Path, root: &Path) -> Result<Freshness> {
    // Bench measures an already-fresh store. Skipping here avoids re-embedding
    // the corpus once per question. Never set by `query` itself.
    if std::env::var("LATTICE_SKIP_FRESHNESS").is_ok() {
        return Ok(Freshness::Current);
    }
    // 0. Schema-version check: a binary upgrade may widen the ingestion/resolution
    // schema without touching any source file. If the stored `facts_schema` key
    // does not match the current binary's version, the graph is under-resolved —
    // treat it as stale and force a full reparse through `index::run` immediately.
    // One-time cost per store per binary upgrade; subsequent queries see `current`.
    let stored_schema = meta::read_kv(lattice_dir, "facts_schema").unwrap_or(None);
    if stored_schema.as_deref() != Some(index::FACTS_SCHEMA_VERSION) {
        match index::run(root, lattice_dir) {
            Ok(_) => {
                eprintln!("lattice: refreshed stale schema before answering");
                return Ok(Freshness::Refreshed);
            }
            Err(index::IndexError::ModelNotFound(m)) => {
                return Err(format!("cannot refresh stale index: {m}").into());
            }
            Err(index::IndexError::Internal(m)) => {
                return Err(format!("refresh failed: {m}").into());
            }
        }
    }

    // 1. Enumerate source files on disk (same discovery walk init uses).
    let mut sources = Vec::new();
    let registry = crate::languages::load_stored(lattice_dir).unwrap_or_default();
    index::collect_source_files(root, root, &registry.extensions(), &mut sources)
        .map_err(|e| format!("walk tree: {e}"))?;

    // The stored rows: path -> (content_hash, indexed_at).
    let indexed = meta::read_index(lattice_dir)?;

    let mut on_disk: BTreeSet<String> = BTreeSet::new();
    let mut stale = false;

    // 2/3/4. Diff sets, mtime screen, hash-on-suspicion.
    for abs in &sources {
        let rel = abs
            .strip_prefix(root)
            .unwrap_or(abs)
            .to_string_lossy()
            .replace('\\', "/");
        on_disk.insert(rel.clone());

        match indexed.get(&rel) {
            // On disk but no row → added (stale, no hashing needed).
            None => {
                stale = true;
            }
            Some((stored_hash, indexed_at)) => {
                // mtime screen: a file is suspicious iff mtime >= indexed_at at
                // whole-second granularity. Files with mtime < indexed_at are
                // trusted with zero I/O beyond the stat (the common case).
                let meta = match std::fs::metadata(abs) {
                    Ok(m) => m,
                    // If we can't stat it, be conservative and treat it as
                    // suspicious; the re-index will surface any real problem.
                    Err(_) => {
                        stale = true;
                        continue;
                    }
                };
                let suspicious = match (mtime_secs(&meta), indexed_secs(indexed_at)) {
                    (Some(m), Some(i)) => m >= i,
                    // Unparseable timestamp / no mtime → be conservative.
                    _ => true,
                };
                if suspicious {
                    // Hash on suspicion only: changed iff the digest differs.
                    let bytes = std::fs::read(abs).map_err(|e| format!("read {rel}: {e}"))?;
                    if meta::content_hash(&bytes) != *stored_hash {
                        stale = true;
                    }
                    // Suspicious-but-identical is NOT stale.
                }
            }
        }
    }

    // Row but not on disk → removed (stale, nothing to hash).
    if !stale {
        for path in indexed.keys() {
            if !on_disk.contains(path) {
                stale = true;
                break;
            }
        }
    }

    // Pending-embed check (gate #2 / design 2d): even when no source file
    // changed, the vector store may have un-embedded files from a prior
    // interrupted init. This covers:
    //   a) Partial embed: embedded_files has gaps vs meta.db hashes.
    //   b) v3→v4 schema migration: is_current_schema() false with a non-empty
    //      index → vectors::has_pending returns true → triggers index::run which
    //      deletes the old store and fully re-embeds.
    //
    // This check is pure SQLite reads (no file I/O, no embedding, no git scan)
    // so it is safe to call on every query. It deliberately lives here (shared
    // pipeline) and NOT in main.rs::cmd_init — query-time self-heal is the point.
    if !stale {
        // Convert indexed (path → (hash, indexed_at)) to path → hash for has_pending.
        let indexed_hashes: std::collections::HashMap<String, String> = indexed
            .iter()
            .map(|(path, (hash, _))| (path.clone(), hash.clone()))
            .collect();
        if vectors::has_pending(lattice_dir, &indexed_hashes) {
            stale = true;
        }
    }

    // A branch with no catalog yet must be recorded even when every file hash
    // matches (first checkout of a branch whose tree equals another). Non-git
    // trees have no active branch and skip this.
    if !stale {
        if let Some(name) = crate::branch::active_branch(root) {
            if !crate::branch::has_symbols(lattice_dir, &name) {
                stale = true;
            }
        }
    }

    if !stale {
        return Ok(Freshness::Current);
    }

    // 5. Refresh synchronously through the shared incremental pipeline — same
    // hash-diff, same fragment cache, same graph/FTS/vector/meta writes as
    // `lattice init`. Fail loud on error.
    match index::run(root, lattice_dir) {
        Ok(_) => {
            eprintln!("lattice: refreshed stale files before answering");
            Ok(Freshness::Refreshed)
        }
        Err(index::IndexError::ModelNotFound(m)) => {
            Err(format!("cannot refresh stale index: {m}").into())
        }
        Err(index::IndexError::Internal(m)) => {
            Err(format!("refresh failed: {m}").into())
        }
    }
}
