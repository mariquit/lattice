// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! `.lattice/meta.db` — SQLite flat lookups: per-file content hashes (used
//! by the incremental-reindex slice) and a kv table (protocol_version, etc).

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::envelope::PROTOCOL_VERSION;
use crate::Result;

pub const META_DB: &str = "meta.db";

/// SHA-256 (lowercase hex) of a file's bytes — the content-hash used by the
/// incremental-reindex hash-diff.
pub fn content_hash(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Read the per-file content hashes previously recorded in `meta.db`.
/// Returns `path -> content_hash`. Empty (no `files` table / no `.lattice/`)
/// means a first-ever init: every file will be treated as new.
pub fn read_hashes(lattice_dir: &Path) -> Result<BTreeMap<String, String>> {
    let db_path = lattice_dir.join(META_DB);
    if !db_path.is_file() {
        return Ok(BTreeMap::new());
    }
    let conn = Connection::open(&db_path).map_err(|e| format!("open meta.db: {e}"))?;
    // No `files` table yet (partial/older store) → treat as empty.
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='files'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        return Ok(BTreeMap::new());
    }
    let mut stmt = conn
        .prepare("SELECT path, content_hash FROM files")
        .map_err(|e| format!("prepare files scan: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| format!("scan files: {e}"))?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (path, hash) = row.map_err(|e| format!("read files row: {e}"))?;
        out.insert(path, hash);
    }
    Ok(out)
}

/// Read the per-file `(content_hash, indexed_at)` pairs from `meta.db`, keyed
/// by repo-relative path. Read-only — no schema change, no writer. Empty (no
/// `files` table / no `.lattice/`) means a first-ever init: the freshness
/// check then treats every file as added. `indexed_at` is the RFC 3339 UTC
/// timestamp written by `meta::update`.
pub fn read_index(lattice_dir: &Path) -> Result<BTreeMap<String, (String, String)>> {
    let db_path = lattice_dir.join(META_DB);
    if !db_path.is_file() {
        return Ok(BTreeMap::new());
    }
    let conn = Connection::open(&db_path).map_err(|e| format!("open meta.db: {e}"))?;
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='files'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        return Ok(BTreeMap::new());
    }
    let mut stmt = conn
        .prepare("SELECT path, content_hash, indexed_at FROM files")
        .map_err(|e| format!("prepare files scan: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (row.get::<_, String>(1)?, row.get::<_, String>(2)?),
            ))
        })
        .map_err(|e| format!("scan files: {e}"))?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (path, pair) = row.map_err(|e| format!("read files row: {e}"))?;
        out.insert(path, pair);
    }
    Ok(out)
}

/// A file's cached parse fragment: the content hash it was parsed at, plus the
/// serialized `FileFacts` JSON. A re-`init` reuses this (skipping tree-sitter)
/// whenever the on-disk hash still matches.
pub struct CachedFacts {
    pub content_hash: String,
    pub facts_json: String,
}

/// Look up one indexed path without loading every cached fragment.
pub fn read_file_facts(lattice_dir: &Path, path: &str) -> Result<Option<CachedFacts>> {
    use rusqlite::OptionalExtension;

    let conn = Connection::open(lattice_dir.join(META_DB))?;
    Ok(conn
        .query_row(
            "SELECT content_hash, facts_json FROM file_facts WHERE path = ?1",
            [path],
            |row| {
                Ok(CachedFacts {
                    content_hash: row.get(0)?,
                    facts_json: row.get(1)?,
                })
            },
        )
        .optional()?)
}

/// Read the per-file parse-fragment cache. Returns `path -> CachedFacts`.
/// Empty when the `file_facts` table is absent (first-ever init, or a store
/// written before slice 10) — every file is then parsed fresh.
pub fn read_facts(lattice_dir: &Path) -> Result<BTreeMap<String, CachedFacts>> {
    let db_path = lattice_dir.join(META_DB);
    if !db_path.is_file() {
        return Ok(BTreeMap::new());
    }
    let conn = Connection::open(&db_path).map_err(|e| format!("open meta.db: {e}"))?;
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='file_facts'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        return Ok(BTreeMap::new());
    }
    let mut stmt = conn
        .prepare("SELECT path, content_hash, facts_json FROM file_facts")
        .map_err(|e| format!("prepare file_facts scan: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                CachedFacts {
                    content_hash: row.get::<_, String>(1)?,
                    facts_json: row.get::<_, String>(2)?,
                },
            ))
        })
        .map_err(|e| format!("scan file_facts: {e}"))?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (path, cached) = row.map_err(|e| format!("read file_facts row: {e}"))?;
        out.insert(path, cached);
    }
    Ok(out)
}

/// Upsert the fragment cache for files parsed this run (`upserts` =
/// (path, content_hash, facts_json)) and delete `removed` files' rows.
/// Files not mentioned keep their existing rows byte-untouched, so an
/// unchanged file's cached fragment stays stable across a refresh.
pub fn update_facts(
    lattice_dir: &Path,
    upserts: &[(String, String, String)],
    removed: &[String],
) -> Result<()> {
    let conn =
        Connection::open(lattice_dir.join(META_DB)).map_err(|e| format!("open meta.db: {e}"))?;
    ensure_schema(&conn)?;
    for (path, hash, json) in upserts {
        conn.execute(
            "INSERT OR REPLACE INTO file_facts (path, content_hash, facts_json)
             VALUES (?1, ?2, ?3)",
            (path, hash, json),
        )
        .map_err(|e| format!("upsert file_facts row: {e}"))?;
    }
    for path in removed {
        conn.execute("DELETE FROM file_facts WHERE path = ?1", (path,))
            .map_err(|e| format!("delete file_facts row: {e}"))?;
    }
    Ok(())
}

/// Ensure meta.db schema exists (used before incremental upsert/delete so a
/// first-ever init has the tables to write into).
fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS files (
           path         TEXT PRIMARY KEY,
           language     TEXT NOT NULL,
           content_hash TEXT NOT NULL,
           indexed_at   TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS file_facts (
           path         TEXT PRIMARY KEY,
           content_hash TEXT NOT NULL,
           facts_json   TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS kv (
           key   TEXT PRIMARY KEY,
           value TEXT NOT NULL
         );",
    )
    .map_err(|e| format!("create meta.db schema: {e}"))?;
    Ok(())
}

/// Incrementally update `meta.db`: upsert `reindexed` rows (changed + new,
/// with fresh hash + indexed_at), delete `removed` files' rows entirely.
/// Unchanged rows are left byte-untouched. `reindexed` = (path, language, bytes).
pub fn update(
    lattice_dir: &Path,
    reindexed: &[(String, String, Vec<u8>)],
    removed: &[String],
) -> Result<()> {
    let conn =
        Connection::open(lattice_dir.join(META_DB)).map_err(|e| format!("open meta.db: {e}"))?;
    ensure_schema(&conn)?;
    let now = chrono::Utc::now().to_rfc3339();
    for (path, language, bytes) in reindexed {
        let hash = content_hash(bytes);
        conn.execute(
            "INSERT OR REPLACE INTO files (path, language, content_hash, indexed_at)
             VALUES (?1, ?2, ?3, ?4)",
            (path, language, &hash, &now),
        )
        .map_err(|e| format!("upsert file row: {e}"))?;
    }
    for path in removed {
        conn.execute("DELETE FROM files WHERE path = ?1", (path,))
            .map_err(|e| format!("delete file row: {e}"))?;
    }
    conn.execute(
        "INSERT OR REPLACE INTO kv (key, value) VALUES ('protocol_version', ?1)",
        (PROTOCOL_VERSION,),
    )
    .map_err(|e| format!("upsert protocol_version: {e}"))?;
    Ok(())
}

/// Read a value from the `kv` table. Returns `None` if absent.
pub fn read_kv(lattice_dir: &Path, key: &str) -> Result<Option<String>> {
    let db_path = lattice_dir.join(META_DB);
    if !db_path.is_file() {
        return Ok(None);
    }
    let conn = Connection::open(&db_path).map_err(|e| format!("open meta.db: {e}"))?;
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='kv'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        return Ok(None);
    }
    let val = conn
        .query_row("SELECT value FROM kv WHERE key = ?1", (key,), |row| {
            row.get::<_, String>(0)
        })
        .ok();
    Ok(val)
}

/// Write (upsert) a value into the `kv` table.
pub fn write_kv(lattice_dir: &Path, key: &str, value: &str) -> Result<()> {
    let conn =
        Connection::open(lattice_dir.join(META_DB)).map_err(|e| format!("open meta.db: {e}"))?;
    ensure_schema(&conn)?;
    conn.execute(
        "INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)",
        (key, value),
    )
    .map_err(|e| format!("write kv {key}: {e}"))?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
