// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! The incremental (re)index core, extracted from `cmd_init` so that
//! `lattice init` and the query-time `freshness::ensure_fresh` share ONE
//! identical pipeline. A query-time refresh can therefore never drift from what
//! `init` would have done: same discovery walk, same hash-diff, same fragment
//! cache, same graph / FTS / vector / meta writes.
//!
//! `run` performs steps 2–3 of the store build (discovery + hash-diff refresh
//! of every store under `.lattice/`). It does NOT emit the JSON envelope and
//! does NOT touch `.gitignore` — those stay in `main::cmd_init`, which is the
//! only caller that speaks the `init` envelope.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::{embed, fts, graph, ingest, meta, progress, vectors, LATTICE_DIR};

/// Failure mode of an index run. `ModelNotFound` is surfaced distinctly so the
/// caller can map it to the `model_not_found` envelope (never a fetch).
pub enum IndexError {
    ModelNotFound(String),
    Internal(String),
}

impl<E: std::fmt::Display> From<E> for IndexError {
    fn from(e: E) -> Self {
        IndexError::Internal(e.to_string())
    }
}

/// What an index run did, so callers can render counters or decide freshness.
pub struct IndexReport {
    pub files_indexed: usize,
    pub files_reindexed: usize,
    pub files_unchanged: usize,
    pub files_parsed: usize,
}

/// Discover source files under `dir`, matching the same extensions `init` uses.
/// Shared so query-time and init-time never disagree about what counts as a
/// source file.
///
/// Uses the pure-Rust `ignore` walker (ripgrep's), so discovery honors
/// `.gitignore`, `.ignore`, the global gitignore, and `.git/info/exclude`, and
/// skips hidden entries (`.git`, `.lattice`, ...) — bug #1.
/// `require_git(false)` makes gitignore rules apply even when the tree is not a
/// git checkout (e.g. test fixtures). A top-level `target/` is skipped
/// explicitly so the pre-gitignore guarantee holds for repos that do not
/// gitignore their build dir.
pub fn collect_source_files(
    root: &Path,
    dir: &Path,
    extra_exts: &[String],
    out: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .require_git(false)
        .build();

    for result in walker {
        let entry = result.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        // Belt-and-suspenders: never index a top-level Rust build dir, even when
        // it is not gitignored (preserves the pre-gitignore heuristic).
        if let Ok(rel) = path.strip_prefix(root) {
            if rel
                .components()
                .next()
                .is_some_and(|c| c.as_os_str() == "target")
            {
                continue;
            }
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let custom = extra_exts.iter().any(|ext| {
            let ext = ext.trim_start_matches('.');
            !ext.is_empty() && name.ends_with(&format!(".{ext}"))
        });
        if name.ends_with(".rs")
            || name.ends_with(".py")
            || name.ends_with(".ts")
            || name.ends_with(".tsx")
            // Markdown is indexed for retrieval (vectors + FTS) only; it never
            // enters the call graph. See `ingest::language_for_path`.
            || name.ends_with(".md")
            || name.ends_with(".markdown")
            || custom
        {
            out.push(path.to_path_buf());
        }
    }
    Ok(())
}

/// The current facts-fragment cache schema version. Bumping this value forces
/// a full reparse on next `init` or query-time refresh — `ensure_fresh` reads
/// this constant to detect a binary upgrade (schema drift = under-resolved graph
/// = stale). `index::run` writes it after every successful run.
pub const FACTS_SCHEMA_VERSION: &str = "7";

/// Build/refresh every store under `.lattice/` from the current tree,
/// incrementally: re-parse only changed/new files (fragment-cache reuse for the
/// rest), prune removed files, and re-resolve the full graph. Idempotent — a
/// re-run over an unchanged tree touches no stores and reports
/// `files_reindexed == 0`.
///
/// **Always silent.** This is the pipeline the query-time freshness self-heal
/// re-runs (`freshness::ensure_fresh`), so it must never print a progress
/// footer. `init` passes its progress reporter to [`run_with_registry`].
pub fn run(root: &Path, lattice_dir: &Path) -> std::result::Result<IndexReport, IndexError> {
    let registry = crate::languages::load_stored(lattice_dir).unwrap_or_default();
    run_with_registry(root, lattice_dir, &progress::Progress::hidden(), &registry)
}

/// Build or refresh the index with an explicit language registry. `cmd_init` passes
/// the registry it just validated from `languages.toml` (init-only read).
/// Query-time freshness goes through [`run`], which loads the stored copy.
pub fn run_with_registry(
    root: &Path,
    lattice_dir: &Path,
    prog: &progress::Progress,
    registry: &crate::languages::LanguageRegistry,
) -> std::result::Result<IndexReport, IndexError> {
    let _guard = crate::languages::activate(registry);
    // 1. Discover source files (skip .lattice/, .git/, other dot-dirs).
    prog.stage("scan", None);
    let mut sources: Vec<PathBuf> = Vec::new();
    collect_source_files(root, root, &registry.extensions(), &mut sources)
        .map_err(|e| format!("walk tree: {e}"))?;
    sources.sort();
    prog.stage(&format!("scan · {} file(s)", sources.len()), None);

    // Prior per-file content hashes (empty on a first-ever init) drive the
    // incremental-reindex hash-diff: re-parse only changed/new files, prune
    // deleted ones, leave unchanged files' graph/vector state untouched.
    let prior_hashes = meta::read_hashes(lattice_dir)?;
    // Prior per-file parse fragments (slice 10): an unchanged file's FileFacts
    // are reloaded from here instead of being re-parsed with tree-sitter.
    // facts_schema migration: if the stored schema version != FACTS_SCHEMA_VERSION,
    // treat ALL files as cache-misses (force full reparse) so updated walkers run
    // on every file. Write FACTS_SCHEMA_VERSION after a successful run.
    let facts_schema_current = meta::read_kv(lattice_dir, "facts_schema")
        .unwrap_or(None)
        .as_deref() == Some(FACTS_SCHEMA_VERSION);
    let prior_facts = if facts_schema_current {
        meta::read_facts(lattice_dir)?
    } else {
        // Force full reparse by treating fragment cache as empty.
        std::collections::BTreeMap::new()
    };

    // 2. Assemble the full current graph. Cross-file resolution needs every
    // file's facts, but each file's facts come from the cache when its content
    // hash is unchanged; only cache misses hit tree-sitter.
    let mut all_facts = Vec::new();
    let mut file_bytes: Vec<(String, String, Vec<u8>)> = Vec::new();
    let mut doc_texts: Vec<(String, String)> = Vec::new();
    let mut all_doc_texts: Vec<(String, String)> = Vec::new();
    let mut reindex: Vec<String> = Vec::new(); // changed + new files
    let mut facts_upserts: Vec<(String, String, String)> = Vec::new(); // (path, hash, json)
    let mut unchanged = 0usize;
    let mut files_parsed = 0usize;
    let scan_total = sources.len();
    for (i, abs) in sources.iter().enumerate() {
        let rel = abs
            .strip_prefix(root)
            .unwrap_or(abs)
            .to_string_lossy()
            .replace('\\', "/");
        prog.step("parse", i + 1, scan_total, Some(&rel));
        let bytes = std::fs::read(abs).map_err(|e| format!("read {rel}: {e}"))?;
        let hash = meta::content_hash(&bytes);
        let is_unchanged = prior_hashes.get(&rel).is_some_and(|prev| *prev == hash);
        if is_unchanged {
            unchanged += 1;
        } else {
            reindex.push(rel.clone()); // changed or new → re-persist stores
        }

        // Fragment-cache hit: reuse the stored facts, skip parsing. Miss (new,
        // changed, or a pre-slice-10 store with no cached fragment): parse now.
        let cache_hit = prior_facts
            .get(&rel)
            .filter(|cached| cached.content_hash == hash);
        let facts = match cache_hit {
            Some(cached) => {
                let mut f: ingest::FileFacts = serde_json::from_str(&cached.facts_json)
                    .map_err(|e| format!("decode cached facts for {rel}: {e}"))?;
                f.language = ingest::language_for_path(&rel);
                f
            }
            None => {
                files_parsed += 1;
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let f = ingest::parse_file(&rel, &text)?;
                let json = serde_json::to_string(&f)
                    .map_err(|e| format!("encode facts for {rel}: {e}"))?;
                facts_upserts.push((rel.clone(), hash.clone(), json));
                f
            }
        };

        // Only re-persisted files need their document text re-embedded (the
        // v1→v2 migration path below widens this to every file).
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if !is_unchanged {
            doc_texts.push((rel.clone(), text.clone()));
        }
        all_doc_texts.push((rel.clone(), text));
        file_bytes.push((rel, ingest::language_for_path(&facts.path).to_string(), bytes));
        all_facts.push(facts);
    }
    prog.stage("graph · resolving call edges", None);
    let resolved = ingest::resolve_with_root(&all_facts, &root.to_string_lossy());

    // Files present in the prior index but gone from the current tree.
    let current: BTreeSet<&str> = file_bytes.iter().map(|(p, _, _)| p.as_str()).collect();
    let removed: Vec<String> = prior_hashes
        .keys()
        .filter(|p| !current.contains(p.as_str()))
        .cloned()
        .collect();

    let files_indexed = file_bytes.len();
    let files_reindexed = reindex.len();

    // 3. Persist the stores under .lattice/ using the file hash diff.
    std::fs::create_dir_all(lattice_dir).map_err(|e| format!("create .lattice/: {e}"))?;

    // Vector-store migration: a store lacking schema_version = '4' is deleted
    // and every current file re-embedded once against the current backend.
    // This handles v1/v2/v3 → v4 upgrades as a one-time full re-embed.
    let vec_full_reindex = !vectors::is_current_schema(lattice_dir);
    if vec_full_reindex {
        vectors::delete_store(lattice_dir)?;
    }

    // Derive the embed set from embedded-hash drift (not from `reindex`).
    //
    // vec_pending = { path | current_hash(path) != embedded_files.get(path) }
    //
    // This is the resumability mechanism: even if meta.db hashes committed
    // before embedding was interrupted, the embedded_files table only records
    // paths that were fully and durably embedded. A kill-and-restart converges
    // because only the un-embedded files (missing or mismatched hash in
    // embedded_files) appear in vec_pending.
    //
    // For a vec_full_reindex (schema migration), every file is pending by
    // construction since the store was just deleted. For a fresh/partial store
    // we compare current hashes against embedded_files.
    let embedded_hashes: std::collections::HashMap<String, String> = if vec_full_reindex {
        std::collections::HashMap::new() // store deleted → all files pending
    } else {
        vectors::read_embedded_hashes(lattice_dir)
    };

    // Build current path→hash map: recompute hashes from file_bytes (the raw
    // bytes were already read in the walk above). This is a cheap SHA-256 pass;
    // the walk already did the I/O, so bytes are in memory.
    let current_path_hashes: std::collections::HashMap<String, String> = file_bytes
        .iter()
        .map(|(p, _, bytes)| (p.clone(), meta::content_hash(bytes)))
        .collect();

    let vec_pending_paths: Vec<&str> = current_path_hashes
        .iter()
        .filter(|(path, hash)| {
            match embedded_hashes.get(*path) {
                Some(recorded) if recorded == *hash => false, // up to date
                _ => true, // missing or hash mismatch → needs embedding
            }
        })
        .map(|(path, _)| path.as_str())
        .collect();

    // Model-presence guard (zero-egress): (re)index must embed changed files, so
    // the candle weights must be present locally. Enforce the same rule as
    // `query` (B9.3) *before* any store write — emits `model_not_found`, never
    // fetches. Only needed when something will actually be embedded this run.
    let vec_will_embed = !vec_pending_paths.is_empty() || !removed.is_empty();
    if vec_will_embed {
        embed::ensure_model_present().map_err(|e| match e {
            embed::EmbedError::ModelNotFound(m) => IndexError::ModelNotFound(m),
            embed::EmbedError::Internal(m) => IndexError::Internal(m),
        })?;
    }

    prog.stage("graph · writing store", None);
    graph::refresh(lattice_dir, &resolved, &reindex, &removed)?;

    // In-degree precompute (slice 14): count incoming edges for each symbol.
    // - calls edges: (from Function id, to Function id) — increment callee's count
    // - imports edges: (from File path, to File path) — increment target's count
    // This is the same in-memory graph used by graph::refresh, so it is
    // always in sync with what is stored in graph.sparrow.
    let mut in_degree_map: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (_, callee, _, _) in &resolved.calls {
        *in_degree_map.entry(callee.clone()).or_default() += 1;
    }
    for (_, target) in &resolved.imports {
        *in_degree_map.entry(target.clone()).or_default() += 1;
    }

    // Symbol FTS index: rebuilt from the in-memory resolved graph.
    // Each Symbol carries its graph id so fts::rebuild can look up in_degree.
    let mut symbols: Vec<fts::Symbol> = Vec::new();
    for (id, name, path, start_line, parent) in &resolved.functions {
        let kind = if parent.is_some() { "method" } else { "function" };
        symbols.push(fts::Symbol {
            name: name.clone(),
            path: path.clone(),
            kind: kind.to_string(),
            line: *start_line,
            id: id.clone(),
        });
    }
    for (id, name, path) in &resolved.modules {
        symbols.push(fts::Symbol {
            name: name.clone(),
            path: path.clone(),
            kind: "module".to_string(),
            line: 1,
            id: id.clone(),
        });
    }
    // Markdown headings join the FTS list on a separate path from the graph-
    // derived loops above, because .md files never enter `resolved`. Driven by
    // `all_doc_texts` (every indexed file, not just the reindexed set) since
    // fts::rebuild drops and recreates the whole table.
    for (rel_path, text) in &all_doc_texts {
        if ingest::language_for_path(rel_path) != "markdown" {
            continue;
        }
        let stem = rel_path
            .rsplit('/')
            .next()
            .and_then(|f| f.rsplit_once('.').map(|(s, _)| s))
            .unwrap_or(rel_path.as_str());
        for sec in crate::markdown::parse_sections(text, stem) {
            symbols.push(fts::Symbol {
                name: sec.heading.clone(),
                path: rel_path.clone(),
                kind: "section".to_string(),
                line: sec.start_line,
                // Synthetic id: doc sections have no graph node, so in_degree
                // lookup misses and correctly yields 0.
                id: format!("{rel_path}::{}", sec.heading),
            });
        }
    }
    prog.step("fts", symbols.len(), symbols.len(), Some("rebuilding symbol index"));
    fts::rebuild(lattice_dir, &symbols, &in_degree_map)?;

    // meta.db upserts only reindexed rows and deletes removed rows (unchanged
    // rows keep their prior indexed_at); vectors.db mirrors the same scoping.
    let reindex_set: BTreeSet<&str> = reindex.iter().map(String::as_str).collect();
    let meta_updates: Vec<(String, String, Vec<u8>)> = file_bytes
        .iter()
        .filter(|(p, _, _)| reindex_set.contains(p.as_str()))
        .cloned()
        .collect();
    prog.stage("meta · writing hashes + fragments", None);
    meta::update(lattice_dir, &meta_updates, &removed)?;
    meta::update_facts(lattice_dir, &facts_upserts, &removed)?;
    // Mark facts_schema version so next run reuses the cache.
    meta::write_kv(lattice_dir, "facts_schema", FACTS_SCHEMA_VERSION)?;

    // Build span-granular embedding units from the facts + source text.
    // Use all_doc_texts filtered by vec_pending_paths (the embedded-hash drift
    // set). For a full reindex (vec_full_reindex) the store was deleted so
    // embedded_hashes is empty → every file is in vec_pending_paths.
    // For an incremental run, only files with missing/mismatched embedded_files
    // rows appear. This is the resumability mechanism: files that are already
    // durably embedded are skipped cheaply; only un-embedded files are retried.
    let vec_pending_set: std::collections::BTreeSet<&str> =
        vec_pending_paths.iter().copied().collect();

    let embed_source: Vec<(String, String)> = all_doc_texts
        .iter()
        .filter(|(p, _)| vec_pending_set.contains(p.as_str()))
        .cloned()
        .collect();

    // Build a lookup: path → FileFacts so span-building can find functions.
    let facts_by_path: std::collections::HashMap<&str, &ingest::FileFacts> = all_facts
        .iter()
        .map(|f| (f.path.as_str(), f))
        .collect();

    // span_updates: (path, content_hash, spans) — hash is threaded into the
    // tuple so vectors::refresh can commit it alongside the vectors in one tx.
    let span_updates: Vec<(String, String, Vec<vectors::Span>)> = embed_source
        .iter()
        .map(|(rel_path, text)| {
            let lines: Vec<&str> = text.lines().collect();
            let n_lines = lines.len().max(1);

            // Markdown is sectioned by heading rather than by symbol: it has no
            // functions, so the generic empty-functions fallback below would
            // collapse a whole design doc into one averaged vector.
            if ingest::language_for_path(rel_path) == "markdown" {
                let stem = rel_path
                    .rsplit('/')
                    .next()
                    .and_then(|f| f.rsplit_once('.').map(|(s, _)| s))
                    .unwrap_or(rel_path.as_str());
                let spans: Vec<vectors::Span> = crate::markdown::parse_sections(text, stem)
                    .into_iter()
                    .map(|sec| vectors::Span {
                        symbol: sec.heading,
                        kind: "section".to_string(),
                        start_line: sec.start_line,
                        end_line: sec.end_line,
                        text: sec.text,
                    })
                    .collect();
                let hash = current_path_hashes
                    .get(rel_path.as_str())
                    .cloned()
                    .unwrap_or_default();
                return (rel_path.clone(), hash, spans);
            }

            let spans = if let Some(facts) = facts_by_path.get(rel_path.as_str()) {
                if facts.functions.is_empty() {
                    // Whole-file fallback: no function symbols found.
                    vec![vectors::Span {
                        symbol: String::new(),
                        kind: "file".to_string(),
                        start_line: 1,
                        end_line: n_lines,
                        text: text.clone(),
                    }]
                } else {
                    facts
                        .functions
                        .iter()
                        .map(|decl| {
                            let start = decl.start_line.clamp(1, n_lines);
                            let end = if decl.end_line == 0 {
                                // Stale fragment default — clamp to start
                                start
                            } else {
                                decl.end_line.clamp(start, n_lines)
                            };
                            let span_text = lines[start - 1..=end - 1].join("\n");
                            let kind = if decl.parent.is_some() {
                                "method".to_string()
                            } else {
                                "function".to_string()
                            };
                            vectors::Span {
                                symbol: decl.name.clone(),
                                kind,
                                start_line: start,
                                end_line: end,
                                text: span_text,
                            }
                        })
                        .collect()
                }
            } else {
                // No facts entry at all (e.g. prose / unparsed file) — whole-file fallback.
                vec![vectors::Span {
                    symbol: String::new(),
                    kind: "file".to_string(),
                    start_line: 1,
                    end_line: n_lines,
                    text: text.clone(),
                }]
            };

            // Thread the current content hash alongside the spans so
            // vectors::refresh can commit it in the same per-file transaction.
            let hash = current_path_hashes
                .get(rel_path.as_str())
                .cloned()
                .unwrap_or_default();
            (rel_path.clone(), hash, spans)
        })
        .collect();

    vectors::refresh(lattice_dir, &span_updates, &removed, prog)?;

    // Persist the language registry so the next query-time refresh sees custom
    // extensions without re-reading languages.toml (that read is init-only).
    crate::languages::store(lattice_dir, registry)?;

    // Branch catalog: record the symbols visible on the active branch. Other
    // branches' rows stay, so `--all-branches` can still see them after a
    // checkout rebuilds the live index. Non-git / detached HEAD: no catalog.
    if let Some(branch_name) = crate::branch::active_branch(root) {
        let files: Vec<(String, String)> = file_bytes
            .iter()
            .map(|(path, _, bytes)| (path.clone(), meta::content_hash(bytes)))
            .collect();
        crate::branch::replace_catalog(lattice_dir, &branch_name, &resolved.functions, &files)?;
        meta::write_kv(lattice_dir, &format!("branch_seen:{branch_name}"), "1")?;
    }

    Ok(IndexReport {
        files_indexed,
        files_reindexed,
        files_unchanged: unchanged,
        files_parsed,
    })
}

/// Absolute `.lattice/` path for a repo root.
pub fn lattice_dir(root: &Path) -> PathBuf {
    root.join(LATTICE_DIR)
}
