// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Branch-scoped index catalog.
//!
//! Content stays stored by hash (the existing incremental path). This module
//! adds a `branch → symbol` catalog in `meta.db` so a query can stay scoped to
//! the active branch while `--all-branches` still sees symbols recorded on
//! other branches. The live graph/FTS/vector stores follow the working tree
//! (rebuilt by `index::run` on a branch switch); the catalog is what survives
//! that rebuild.

use std::path::Path;

use gix::bstr::ByteSlice;
use rusqlite::{params, Connection};

use crate::meta::META_DB;
use crate::Result;

/// Envelope value for `--all-branches` (the pre-bump global opt-out).
pub const ALL_BRANCHES: &str = "*";

/// How a query is scoped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Non-git tree or detached HEAD: the single global catalog (today's
    /// behavior). Envelope `branch` is JSON null.
    Global,
    /// Results limited to this branch's catalog. Envelope `branch` is the name.
    Branch(String),
    /// Union of every recorded branch. Envelope `branch` is `"*"`.
    All,
}

impl Scope {
    /// The additive envelope field. `Null` for the global fallback.
    pub fn json(&self) -> serde_json::Value {
        match self {
            Scope::Global => serde_json::Value::Null,
            Scope::Branch(name) => serde_json::Value::String(name.clone()),
            Scope::All => serde_json::Value::String(ALL_BRANCHES.to_string()),
        }
    }

}

/// Resolve the scope a query should apply.
///
/// `all_branches` wins over an explicit name only as a caller-side mutual
/// exclusion (the router rejects both flags together before calling this).
pub fn resolve(root: &Path, all_branches: bool, explicit: Option<&str>) -> Scope {
    if all_branches {
        return Scope::All;
    }
    if let Some(name) = explicit {
        if !name.is_empty() {
            return Scope::Branch(name.to_string());
        }
    }
    match active_branch(root) {
        Some(name) => Scope::Branch(name),
        None => Scope::Global,
    }
}

/// The checked-out branch name, or `None` for a non-git tree / detached HEAD.
///
/// Pure-Rust `gix` reader. A linked worktree reports its own HEAD; an isolated
/// `.lattice/` in that worktree is the boundary (a worktree without its own
/// store does not silently rewrite the main checkout's index).
pub fn active_branch(root: &Path) -> Option<String> {
    if !root.join(".git").exists() {
        return None;
    }
    let repo = gix::open(root).ok()?;
    let head = repo.head().ok()?;
    let name = head.referent_name()?;
    let short = name.shorten();
    let text = short.to_str().ok()?.to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn open(lattice_dir: &Path) -> Result<Connection> {
    Connection::open(lattice_dir.join(META_DB)).map_err(|e| format!("open meta.db: {e}").into())
}

fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS branch_symbols (
            branch TEXT NOT NULL,
            symbol_id TEXT NOT NULL,
            path TEXT NOT NULL,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            line INTEGER NOT NULL,
            PRIMARY KEY (branch, symbol_id)
        );
        CREATE TABLE IF NOT EXISTS branch_files (
            branch TEXT NOT NULL,
            path TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            PRIMARY KEY (branch, path)
        );",
    )
    .map_err(|e| format!("create branch catalog: {e}"))?;
    Ok(())
}

/// True when this branch already has catalog rows (so a query on it need not
/// force a refresh just to populate the catalog).
pub fn has_symbols(lattice_dir: &Path, branch: &str) -> bool {
    let marker = format!("branch_seen:{branch}");
    if crate::meta::read_kv(lattice_dir, &marker)
        .ok()
        .flatten()
        .is_some()
    {
        return true;
    }
    let Ok(conn) = open(lattice_dir) else {
        return false;
    };
    if ensure_schema(&conn).is_err() {
        return false;
    }
    conn.query_row(
        "SELECT 1 FROM branch_symbols WHERE branch = ?1 LIMIT 1",
        [branch],
        |_| Ok(true),
    )
    .unwrap_or(false)
}

/// A symbol recorded against a branch.
#[derive(Debug, Clone)]
pub struct CatalogSymbol {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub line: usize,
}

/// Replace one branch's catalog with the symbols/files visible on it now.
/// Other branches' rows are left untouched — that is what `--all-branches`
/// reads after a checkout rebuilds the live index.
pub fn replace_catalog(
    lattice_dir: &Path,
    branch: &str,
    symbols: &[(String, String, String, usize, Option<String>)],
    files: &[(String, String)],
) -> Result<()> {
    let conn = open(lattice_dir)?;
    ensure_schema(&conn)?;
    conn.execute("DELETE FROM branch_symbols WHERE branch = ?1", [branch])
        .map_err(|e| format!("clear branch_symbols: {e}"))?;
    conn.execute("DELETE FROM branch_files WHERE branch = ?1", [branch])
        .map_err(|e| format!("clear branch_files: {e}"))?;
    for (id, name, path, line, parent) in symbols {
        let kind = if parent.is_some() { "method" } else { "function" };
        conn.execute(
            "INSERT OR REPLACE INTO branch_symbols
             (branch, symbol_id, path, name, kind, line) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![branch, id, path, name, kind, *line as i64],
        )
        .map_err(|e| format!("insert branch_symbol: {e}"))?;
    }
    for (path, hash) in files {
        conn.execute(
            "INSERT OR REPLACE INTO branch_files (branch, path, content_hash) VALUES (?1, ?2, ?3)",
            params![branch, path, hash],
        )
        .map_err(|e| format!("insert branch_file: {e}"))?;
    }
    Ok(())
}

/// Symbols on `branch` (or every branch when `branch` is `None`) whose name
/// equals `query`. Deterministic: path, then name, then symbol id.
pub fn symbols_named(
    lattice_dir: &Path,
    branch: Option<&str>,
    query: &str,
) -> Result<Vec<CatalogSymbol>> {
    let conn = open(lattice_dir)?;
    ensure_schema(&conn)?;
    let sql = match branch {
        Some(_) => {
            "SELECT path, name, kind, line FROM branch_symbols
             WHERE branch = ?1 AND name = ?2
             ORDER BY path ASC, name ASC, symbol_id ASC"
        }
        None => {
            "SELECT path, name, kind, line FROM branch_symbols
             WHERE name = ?1
             ORDER BY path ASC, name ASC, symbol_id ASC"
        }
    };
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| format!("prepare branch symbol search: {e}"))?;
    let map = |row: &rusqlite::Row<'_>| {
        Ok(CatalogSymbol {
            path: row.get(0)?,
            name: row.get(1)?,
            kind: row.get(2)?,
            line: row.get::<_, i64>(3)? as usize,
        })
    };
    let rows = match branch {
        Some(b) => stmt
            .query_map(params![b, query], map)
            .map_err(|e| format!("search branch symbols: {e}"))?,
        None => stmt
            .query_map(params![query], map)
            .map_err(|e| format!("search branch symbols: {e}"))?,
    };
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("read branch symbol: {e}"))?);
    }
    Ok(out)
}
