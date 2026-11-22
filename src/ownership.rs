// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Git ownership ingestion (`lattice init` git pass) and query backend.
//!
//! Entry points:
//!   - `scan(root, lattice_dir)` — called from `cmd_init` ONLY (not from any
//!     query path; a full-history git scan per query is unacceptable latency).
//!   - `owners(lattice_dir, path)` — read-only query from the router.
//!   - `all_file_stats(lattice_dir)` — bulk read for OKF teams export.
//!
//! Pure-Rust: uses `gix` (no libgit2/C++) for reading the repo. Zero network
//! egress — gix is opened read-only on a local .git; no fetch API is called.
//!
//! Line-count method: imara-diff on blob bytes, giving true added/deleted lines
//! per diff hunk rather than blob sizes (important for modifications).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gix::bstr::ByteSlice;
use imara_diff::intern::InternedInput;
use imara_diff::sink::Counter;
use imara_diff::{diff, Algorithm};
use rusqlite::Connection;

use crate::meta::META_DB;
use crate::Result;

// Only track files with these extensions (same as index::collect_source_files).
const INDEXED_EXTS: &[&str] = &[".rs", ".py", ".ts", ".tsx"];

/// Maximum co-changed file count per commit (noise guard).
const CO_CHANGE_MAX_FILES: usize = 20;

/// Summary returned to `cmd_init` for the stderr report.
pub struct GitReport {
    pub commits: usize,
    pub files: usize,
}

/// A single file owner entry (query result).
pub struct Owner {
    pub author: String,
    pub share: f64,
    pub last_touched: String, // RFC 3339
}

/// Full query result for `--owners <path>`.
pub struct OwnersReport {
    pub owners: Vec<Owner>,
    pub churn: u64,
    pub co_changed: Vec<String>,
}

// ── schema ────────────────────────────────────────────────────────────────────

/// Ensure the three ownership tables exist (called before a scan pass writes).
/// NOT folded into `meta::ensure_schema` — query paths that never touch git
/// data don't create these tables.
fn ensure_git_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS git_file_authors (
            path         TEXT NOT NULL,
            author       TEXT NOT NULL,
            added        INTEGER NOT NULL DEFAULT 0,
            deleted      INTEGER NOT NULL DEFAULT 0,
            commits      INTEGER NOT NULL DEFAULT 0,
            last_touched TEXT NOT NULL,
            PRIMARY KEY (path, author)
         );
         CREATE TABLE IF NOT EXISTS git_file_stats (
            path         TEXT PRIMARY KEY,
            churn        INTEGER NOT NULL,
            last_touched TEXT NOT NULL,
            last_commit  TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS git_co_changes (
            path_a   TEXT NOT NULL,
            path_b   TEXT NOT NULL,
            together INTEGER NOT NULL,
            PRIMARY KEY (path_a, path_b)
         );",
    )
    .map_err(|e| format!("create git schema: {e}"))?;
    Ok(())
}

/// Check whether the git tables exist in the connection.
fn git_tables_exist(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='git_file_stats'",
        [],
        |_| Ok(true),
    )
    .unwrap_or(false)
}

// ── ingestion ─────────────────────────────────────────────────────────────────

/// Count true added/deleted lines between two blobs using imara-diff.
/// `old_bytes` = None means an empty tree (file was added).
/// `new_bytes` = None means the file was deleted.
fn count_diff_lines(old_bytes: &[u8], new_bytes: &[u8]) -> (u64, u64) {
    let input = InternedInput::new(old_bytes, new_bytes);
    let counter = diff(Algorithm::Histogram, &input, Counter::default());
    (counter.insertions as u64, counter.removals as u64)
}

/// Returns `true` if the path ends with one of the indexed extensions.
fn is_indexed_ext(path: &str) -> bool {
    INDEXED_EXTS.iter().any(|ext| path.ends_with(ext))
}

/// A gitignore matcher scoped to the directory that declared it.
///
/// A nested `.gitignore` governs ONLY its own subtree, so its patterns cannot be
/// merged into one root-anchored matcher: doing so makes `sub/.gitignore`'s
/// `nested_skip.rs` wrongly ignore a root-level `nested_skip.rs` too (verified
/// against `git check-ignore`). Each file therefore gets its own matcher plus
/// the directory it applies to.
struct ScopedIgnore {
    dir: PathBuf,
    matcher: ignore::gitignore::Gitignore,
}

/// The gitignore matchers used to filter HISTORY paths (bug #2).
///
/// The ownership pass walks git history, not the working tree, so unlike
/// `index::collect_source_files` it cannot lean on the `ignore` walker to prune
/// as it goes: a path that was committed and only later gitignored (the classic
/// `git rm --cached target/`) is still present in old trees forever. Without
/// this filter such paths keep their churn/authors/co-changes and leak back out
/// through `query --owners` (including `co_changed`) and the `--hotspots` churn
/// weighting, contradicting the documented "`init` respects `.gitignore`".
///
/// Sources mirror what the walker honors: every `.gitignore` in the tree (root
/// and nested), `.git/info/exclude`, and the user's global gitignore. Uses
/// `ignore`'s pure-Rust matcher — no new dependency.
///
/// Returned deepest-directory-first so the most specific rule wins, matching
/// git's precedence.
fn build_ignore_matchers(root: &Path) -> Vec<ScopedIgnore> {
    let mut out: Vec<ScopedIgnore> = Vec::new();

    // NOTE: `.hidden(false)` is REQUIRED here. `.gitignore` is itself a hidden
    // file, so the `hidden(true)` used for source discovery makes this walk
    // yield nothing at all (silently building an empty matcher). `.git` is
    // pruned explicitly instead.
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for entry in walker.flatten() {
        if entry.file_name() != ".gitignore" {
            continue;
        }
        let Some(dir) = entry.path().parent() else {
            continue;
        };
        let mut builder = ignore::gitignore::GitignoreBuilder::new(dir);
        builder.add(entry.path());
        if let Ok(matcher) = builder.build() {
            out.push(ScopedIgnore {
                dir: dir.to_path_buf(),
                matcher,
            });
        }
    }

    // Repo-local excludes + the user's global gitignore, both rooted at the
    // repo root. `GitignoreBuilder::build_global()` is deliberately NOT used:
    // it returns an EMPTY matcher when no global config exists, discarding
    // whatever was already added to the builder.
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    let mut any = false;
    let exclude = root.join(".git").join("info").join("exclude");
    if exclude.is_file() {
        builder.add(&exclude);
        any = true;
    }
    if let Some(global) = ignore::gitignore::gitconfig_excludes_path() {
        if global.is_file() {
            builder.add(&global);
            any = true;
        }
    }
    if any {
        // A malformed pattern must not fail init (ownership is optional
        // context); just drop this matcher.
        if let Ok(matcher) = builder.build() {
            out.push(ScopedIgnore {
                dir: root.to_path_buf(),
                matcher,
            });
        }
    }

    // Deepest first = most specific rule decides.
    out.sort_by_key(|s| std::cmp::Reverse(s.dir.components().count()));
    out
}

/// `true` when a git-history path must be excluded from the ownership store:
/// wrong extension, or matched by a gitignore rule.
///
/// `path` is repo-relative (as stored in git trees); it is joined onto `root`
/// because `matched_path_or_any_parents` PANICS on a path outside the matcher's
/// root. The `starts_with` guard keeps each nested matcher to its own subtree
/// and prevents exactly that panic. A whitelist (`!pattern`) hit short-circuits
/// to "not ignored", so negations keep working.
fn is_excluded(path: &str, root: &Path, matchers: &[ScopedIgnore]) -> bool {
    if !is_indexed_ext(path) {
        return true;
    }
    let abs = root.join(path);
    for scoped in matchers {
        if !abs.starts_with(&scoped.dir) {
            continue;
        }
        // is_dir=false: entries filtered here are always blobs.
        let m = scoped.matcher.matched_path_or_any_parents(&abs, false);
        if m.is_ignore() {
            return true;
        }
        if m.is_whitelist() {
            return false;
        }
    }
    false
}

/// Run the git ownership pass. `Ok(None)` = not a git repo (clean skip).
/// Git read errors are mapped to `Ok(None)` + a stderr warning so init
/// never fails due to ownership being optional context.
pub fn scan(root: &Path, lattice_dir: &Path) -> Result<Option<GitReport>> {
    // Guard: explicit .git presence check (do NOT use gix::discover to avoid
    // walking up to a parent repo — important when fixture is a nested tempdir).
    if !root.join(".git").exists() {
        return Ok(None);
    }

    let repo = match gix::open(root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("lattice: git pass: skipped (gix open: {e})");
            return Ok(None);
        }
    };

    let head_id = match repo.head_id() {
        Ok(id) => id,
        Err(e) => {
            eprintln!("lattice: git pass: skipped (head_id: {e})");
            return Ok(None);
        }
    };

    let walk = match repo.rev_walk([head_id]).all() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("lattice: git pass: skipped (rev_walk: {e})");
            return Ok(None);
        }
    };

    // In-memory accumulators.
    // (path, author) -> {added, deleted, commits, last_touched_secs}
    let mut author_map: HashMap<(String, String), (u64, u64, u64, i64)> = HashMap::new();
    // path -> {churn, last_touched_secs, last_commit_hex}
    let mut file_map: HashMap<String, (u64, i64, String)> = HashMap::new();
    // (path_a, path_b) with path_a < path_b -> together count
    let mut co_map: HashMap<(String, String), u64> = HashMap::new();

    // Built ONCE, outside the commit loop: history can span thousands of
    // commits and re-reading every .gitignore per commit would dominate the
    // pass. Reflects the CURRENT ignore rules, which is the intent — a path
    // ignored today is excluded from all of history.
    let ignore_matchers = build_ignore_matchers(root);

    let mut commit_count = 0usize;

    for info in walk {
        let info = match info {
            Ok(i) => i,
            Err(e) => {
                eprintln!("lattice: git pass: walk step error ({e}); skipping");
                continue;
            }
        };

        let commit_obj = match repo.find_object(info.id) {
            Ok(o) => o,
            Err(_) => continue,
        };
        let commit = match commit_obj.peel_to_commit() {
            Ok(c) => c,
            Err(_) => continue,
        };
        let commit_ref = match commit.decode() {
            Ok(r) => r,
            Err(_) => continue,
        };

        let author_sig = commit_ref.author();
        let author_str = format!(
            "{} <{}>",
            author_sig.name.to_str_lossy(),
            author_sig.email.to_str_lossy()
        );
        let commit_time_secs: i64 = author_sig.time.seconds;
        let commit_hex = info.id.to_hex().to_string();

        // Get parent tree (None for root commits).
        let parent_ids: Vec<gix::hash::ObjectId> = commit_ref
            .parents()
            .map(|p| p.into())
            .collect();

        let cur_tree_id = commit_ref.tree();
        let cur_tree = match repo.find_object(cur_tree_id) {
            Ok(o) => o.into_tree(),
            Err(_) => continue,
        };

        // Collect changed (path, old_blob_id, new_blob_id) for indexed extensions.
        let mut changed_files: Vec<(String, Option<gix::hash::ObjectId>, Option<gix::hash::ObjectId>)> = Vec::new();

        if let Some(parent_id) = parent_ids.first() {
            let parent_commit = match repo.find_object(*parent_id) {
                Ok(o) => o,
                Err(_) => continue,
            };
            let parent_commit = match parent_commit.peel_to_commit() {
                Ok(c) => c,
                Err(_) => continue,
            };
            let parent_ref = match parent_commit.decode() {
                Ok(r) => r,
                Err(_) => continue,
            };
            let par_tree_id = parent_ref.tree();
            let par_tree = match repo.find_object(par_tree_id) {
                Ok(o) => o.into_tree(),
                Err(_) => continue,
            };

            // Diff direction: par_tree.changes().for_each_to_obtain_tree(&cur_tree)
            // = "what changes transform par_tree into cur_tree" (commits = forward).
            // Addition { id } = file added in cur_tree (new blob = id).
            // Deletion { id } = file removed from par_tree (old blob = id).
            // Modification { previous_id, id } = old blob was previous_id, new is id.
            let _ = par_tree
                .changes()
                .map(|mut p| {
                    p.for_each_to_obtain_tree(&cur_tree, |change| {
                        use gix::object::tree::diff::Change;
                        match &change {
                            Change::Addition {
                                entry_mode,
                                id,
                                location,
                                ..
                            } => {
                                if entry_mode.is_blob() {
                                    let path = location.to_str_lossy().into_owned();
                                    if !is_excluded(&path, root, &ignore_matchers) {
                                        changed_files.push((path, None, Some(id.detach())));
                                    }
                                }
                            }
                            Change::Modification {
                                entry_mode,
                                id,
                                previous_entry_mode: _,
                                previous_id,
                                location,
                            } => {
                                if entry_mode.is_blob() {
                                    let path = location.to_str_lossy().into_owned();
                                    if !is_excluded(&path, root, &ignore_matchers) {
                                        changed_files.push((
                                            path,
                                            Some(previous_id.detach()),
                                            Some(id.detach()),
                                        ));
                                    }
                                }
                            }
                            Change::Deletion {
                                entry_mode,
                                id,
                                location,
                                ..
                            } => {
                                if entry_mode.is_blob() {
                                    let path = location.to_str_lossy().into_owned();
                                    if !is_excluded(&path, root, &ignore_matchers) {
                                        changed_files.push((path, Some(id.detach()), None));
                                    }
                                }
                            }
                            _ => {}
                        }
                        Ok::<_, std::convert::Infallible>(
                            gix::object::tree::diff::Action::Continue,
                        )
                    })
                })
                .ok();
        } else {
            // Root commit: all files in tree are additions (no parent tree).
            // We enumerate via an empty-tree diff in the forward direction:
            // empty_tree.changes().for_each_to_obtain_tree(&cur_tree) → all ADD.
            let empty_tree_id = gix::hash::ObjectId::empty_tree(repo.object_hash());
            // The empty tree may or may not exist as a stored object. Use a
            // fallback path if gix can't find it: directly enumerate the tree entries.
            let added_via_diff = if let Ok(empty_obj) = repo.find_object(empty_tree_id) {
                let empty_tree = empty_obj.into_tree();
                let mut collected = Vec::new();
                let _ = empty_tree.changes().map(|mut p| {
                    p.for_each_to_obtain_tree(&cur_tree, |change| {
                        use gix::object::tree::diff::Change;
                        if let Change::Addition { entry_mode, id, location, .. } = change {
                            if entry_mode.is_blob() {
                                let path = location.to_str_lossy().into_owned();
                                collected.push((path, id.detach()));
                            }
                        }
                        Ok::<_, std::convert::Infallible>(
                            gix::object::tree::diff::Action::Continue,
                        )
                    })
                });
                collected
            } else {
                Vec::new()
            };

            if !added_via_diff.is_empty() {
                for (path, blob_id) in added_via_diff {
                    if !is_excluded(&path, root, &ignore_matchers) {
                        changed_files.push((path, None, Some(blob_id)));
                    }
                }
            } else {
                // Fallback: enumerate blobs by walking the tree directly.
                let mut collected = Vec::new();
                let _ = enumerate_tree_blobs(&repo, &cur_tree, &mut |path, blob_id| {
                    collected.push((path, blob_id));
                });
                for (path, blob_id) in collected {
                    if !is_excluded(&path, root, &ignore_matchers) {
                        changed_files.push((path, None, Some(blob_id)));
                    }
                }
            }
        }

        if changed_files.is_empty() {
            commit_count += 1;
            continue;
        }

        // Per-file line counts via imara-diff.
        for (path, old_id, new_id) in &changed_files {
            let old_bytes: Vec<u8> = if let Some(id) = old_id {
                repo.find_object(*id)
                    .map(|o| o.data.to_vec())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let new_bytes: Vec<u8> = if let Some(id) = new_id {
                repo.find_object(*id)
                    .map(|o| o.data.to_vec())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            let (added, deleted) = count_diff_lines(&old_bytes, &new_bytes);

            // Update (path, author) accumulator.
            let key = (path.clone(), author_str.clone());
            let entry = author_map.entry(key).or_insert((0, 0, 0, i64::MIN));
            entry.0 += added;
            entry.1 += deleted;
            entry.2 += 1; // commits
            if commit_time_secs > entry.3 {
                entry.3 = commit_time_secs;
            }

            // Update file stats.
            let fentry = file_map.entry(path.clone()).or_insert((0, i64::MIN, String::new()));
            fentry.0 += 1; // churn
            if commit_time_secs > fentry.1 {
                fentry.1 = commit_time_secs;
                fentry.2 = commit_hex.clone();
            }
        }

        // Co-change accounting (noise guard: skip commits with > CO_CHANGE_MAX_FILES).
        let indexed_paths: Vec<&str> = changed_files.iter().map(|(p, _, _)| p.as_str()).collect();
        if indexed_paths.len() <= CO_CHANGE_MAX_FILES {
            for i in 0..indexed_paths.len() {
                for j in (i + 1)..indexed_paths.len() {
                    let (a, b) = if indexed_paths[i] < indexed_paths[j] {
                        (indexed_paths[i].to_string(), indexed_paths[j].to_string())
                    } else {
                        (indexed_paths[j].to_string(), indexed_paths[i].to_string())
                    };
                    *co_map.entry((a, b)).or_insert(0) += 1;
                }
            }
        }

        commit_count += 1;
    }

    // Write to meta.db in a single transaction.
    let db_path = lattice_dir.join(META_DB);
    let conn = Connection::open(&db_path).map_err(|e| format!("open meta.db: {e}"))?;
    ensure_git_schema(&conn)?;

    conn.execute_batch("BEGIN;").map_err(|e| format!("begin tx: {e}"))?;

    conn.execute("DELETE FROM git_file_authors", [])
        .map_err(|e| format!("delete git_file_authors: {e}"))?;
    conn.execute("DELETE FROM git_file_stats", [])
        .map_err(|e| format!("delete git_file_stats: {e}"))?;
    conn.execute("DELETE FROM git_co_changes", [])
        .map_err(|e| format!("delete git_co_changes: {e}"))?;

    let mut files_with_data: std::collections::HashSet<String> = std::collections::HashSet::new();

    for ((path, author), (added, deleted, commits, last_secs)) in &author_map {
        let last_touched = secs_to_rfc3339(*last_secs);
        conn.execute(
            "INSERT OR REPLACE INTO git_file_authors (path, author, added, deleted, commits, last_touched)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (path, author, added, deleted, commits, &last_touched),
        )
        .map_err(|e| format!("insert git_file_authors: {e}"))?;
        files_with_data.insert(path.clone());
    }

    for (path, (churn, last_secs, last_commit)) in &file_map {
        let last_touched = secs_to_rfc3339(*last_secs);
        conn.execute(
            "INSERT OR REPLACE INTO git_file_stats (path, churn, last_touched, last_commit)
             VALUES (?1, ?2, ?3, ?4)",
            (path, churn, &last_touched, last_commit),
        )
        .map_err(|e| format!("insert git_file_stats: {e}"))?;
    }

    for ((path_a, path_b), together) in &co_map {
        conn.execute(
            "INSERT OR REPLACE INTO git_co_changes (path_a, path_b, together)
             VALUES (?1, ?2, ?3)",
            (path_a, path_b, together),
        )
        .map_err(|e| format!("insert git_co_changes: {e}"))?;
    }

    conn.execute_batch("COMMIT;").map_err(|e| format!("commit tx: {e}"))?;

    // Write git_schema KV key outside the transaction (uses meta::write_kv
    // which ensures the kv table exists).
    crate::meta::write_kv(lattice_dir, "git_schema", "1")?;

    let files_count = files_with_data.len();
    Ok(Some(GitReport {
        commits: commit_count,
        files: files_count,
    }))
}

/// Enumerate all blob entries in a tree recursively.
fn enumerate_tree_blobs(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    f: &mut impl FnMut(String, gix::hash::ObjectId),
) -> Result<()> {
    // Use the changes() API against an empty tree to enumerate all additions.
    // The empty tree has a well-known SHA1/SHA256 ObjectId.
    let empty_tree_id = gix::hash::ObjectId::empty_tree(repo.object_hash());
    let empty_tree = repo
        .find_object(empty_tree_id)
        .map(|o| o.into_tree())
        .unwrap_or_else(|_| {
            // Fallback: open a synthetic empty tree by computing the empty tree id
            // and re-fetching. If gix can't find it, we skip (root commit will be
            // treated as no-changes; not ideal but the guard handles it).
            // We already tried above — just clone the current tree as a sentinel.
            tree.clone()
        });

    let _ = tree.changes().map(|mut p| {
        p.for_each_to_obtain_tree(&empty_tree, |change| {
            use gix::object::tree::diff::Change;
            if let Change::Addition {
                entry_mode,
                id,
                location,
                ..
            } = change
            {
                if entry_mode.is_blob() {
                    let path = location.to_str_lossy().into_owned();
                    f(path, id.detach());
                }
            }
            Ok::<_, std::convert::Infallible>(gix::object::tree::diff::Action::Continue)
        })
    });

    Ok(())
}

/// Convert Unix seconds to an RFC 3339 UTC timestamp string.
fn secs_to_rfc3339(secs: i64) -> String {
    use chrono::{DateTime, TimeZone, Utc};
    let dt: DateTime<Utc> = Utc.timestamp_opt(secs, 0).single().unwrap_or_default();
    dt.to_rfc3339()
}

// ── query ─────────────────────────────────────────────────────────────────────

/// Query the ownership data for a single file path.
/// Missing tables or unknown path → empty report (exit 0, not an error).
pub fn owners(lattice_dir: &Path, path: &str) -> Result<OwnersReport> {
    let db_path = lattice_dir.join(META_DB);
    if !db_path.is_file() {
        return Ok(empty_report());
    }

    let conn =
        Connection::open(&db_path).map_err(|e| format!("open meta.db for owners: {e}"))?;

    if !git_tables_exist(&conn) {
        return Ok(empty_report());
    }

    // Churn + last_touched from git_file_stats.
    let (churn, _last_touched) = conn
        .query_row(
            "SELECT churn, last_touched FROM git_file_stats WHERE path = ?1",
            [path],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
        )
        .unwrap_or((0, String::new()));

    if churn == 0 {
        // Unknown path — return empty.
        return Ok(empty_report());
    }

    // Authors from git_file_authors.
    let mut stmt = conn
        .prepare(
            "SELECT author, added, commits, last_touched
             FROM git_file_authors
             WHERE path = ?1
             ORDER BY added DESC, author ASC",
        )
        .map_err(|e| format!("prepare git_file_authors: {e}"))?;

    struct AuthorRow {
        author: String,
        added: u64,
        commits: u64,
        last_touched: String,
    }

    let rows: Vec<AuthorRow> = stmt
        .query_map([path], |row| {
            Ok(AuthorRow {
                author: row.get::<_, String>(0)?,
                added: row.get::<_, u64>(1)?,
                commits: row.get::<_, u64>(2)?,
                last_touched: row.get::<_, String>(3)?,
            })
        })
        .map_err(|e| format!("query git_file_authors: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    let total_added: u64 = rows.iter().map(|r| r.added).sum();
    let total_commits: u64 = rows.iter().map(|r| r.commits).sum();

    let owners: Vec<Owner> = rows
        .into_iter()
        .map(|r| {
            let share = if total_added > 0 {
                r.added as f64 / total_added as f64
            } else if total_commits > 0 {
                r.commits as f64 / total_commits as f64
            } else {
                0.0
            };
            Owner {
                author: r.author,
                share,
                last_touched: r.last_touched,
            }
        })
        .collect();

    // Co-changed paths.
    let mut co_stmt = conn
        .prepare(
            "SELECT CASE WHEN path_a = ?1 THEN path_b ELSE path_a END AS partner, together
             FROM git_co_changes
             WHERE (path_a = ?1 OR path_b = ?1) AND together >= 1
             ORDER BY together DESC, partner ASC",
        )
        .map_err(|e| format!("prepare git_co_changes: {e}"))?;

    let co_changed: Vec<String> = co_stmt
        .query_map([path], |row| row.get::<_, String>(0))
        .map_err(|e| format!("query git_co_changes: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(OwnersReport {
        owners,
        churn,
        co_changed,
    })
}

fn empty_report() -> OwnersReport {
    OwnersReport {
        owners: Vec::new(),
        churn: 0,
        co_changed: Vec::new(),
    }
}

/// Read churn values for all indexed files.
/// Returns a `path → churn` map for hotspot scoring. An empty map is returned
/// when the git tables have not been created (no git history scanned).
/// Query paths MUST NOT call `ensure_git_schema` — this function never does.
pub fn churn_map(lattice_dir: &Path) -> crate::Result<HashMap<String, u64>> {
    let db_path = lattice_dir.join(META_DB);
    if !db_path.is_file() {
        return Ok(HashMap::new());
    }

    let conn =
        Connection::open(&db_path).map_err(|e| format!("open meta.db for churn_map: {e}"))?;
    if !git_tables_exist(&conn) {
        return Ok(HashMap::new());
    }

    let mut stmt = conn
        .prepare("SELECT path, churn FROM git_file_stats")
        .map_err(|e| format!("prepare churn_map: {e}"))?;

    let map: HashMap<String, u64> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })
        .map_err(|e| format!("query churn_map: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(map)
}

/// Check whether the git_file_stats table exists AND has at least one row.
/// Used to set `churn_weighted` in hotspot/health envelopes.
pub fn git_stats_have_rows(lattice_dir: &Path) -> bool {
    let db_path = lattice_dir.join(META_DB);
    if !db_path.is_file() {
        return false;
    }
    let conn = match Connection::open(&db_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    if !git_tables_exist(&conn) {
        return false;
    }
    conn.query_row(
        "SELECT 1 FROM git_file_stats LIMIT 1",
        [],
        |_| Ok(true),
    )
    .unwrap_or(false)
}

/// Bulk read of all file stats for OKF teams export.
/// Returns map: path → (churn, last_touched, Vec<(author, added, commits, last_touched)>).
pub fn all_file_stats(
    lattice_dir: &Path,
) -> Result<HashMap<String, (u64, String, Vec<(String, u64, u64, String)>)>> {
    let db_path = lattice_dir.join(META_DB);
    if !db_path.is_file() {
        return Ok(HashMap::new());
    }

    let conn =
        Connection::open(&db_path).map_err(|e| format!("open meta.db for all_file_stats: {e}"))?;
    if !git_tables_exist(&conn) {
        return Ok(HashMap::new());
    }

    // Load all file stats.
    let mut stats: HashMap<String, (u64, String, Vec<(String, u64, u64, String)>)> =
        HashMap::new();

    let mut stmt = conn
        .prepare("SELECT path, churn, last_touched FROM git_file_stats")
        .map_err(|e| format!("prepare git_file_stats: {e}"))?;

    let rows: Vec<(String, u64, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| format!("query git_file_stats: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    for (path, churn, last) in rows {
        stats.insert(path, (churn, last, Vec::new()));
    }

    // Load all author data.
    let mut author_stmt = conn
        .prepare(
            "SELECT path, author, added, commits, last_touched FROM git_file_authors",
        )
        .map_err(|e| format!("prepare all authors: {e}"))?;

    let author_rows: Vec<(String, String, u64, u64, String)> = author_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|e| format!("query all authors: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    for (path, author, added, commits, last) in author_rows {
        if let Some(entry) = stats.get_mut(&path) {
            entry.2.push((author, added, commits, last));
        }
    }

    Ok(stats)
}
