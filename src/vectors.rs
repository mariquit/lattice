// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! `.lattice/vectors.db` — SQLite + `sqlite-vec` semantic vector store.
//!
//! Populated during `init`; queried by the semantic branch of `query`.
//! Embeddings are computed by a local candle `all-MiniLM-L6-v2` model
//! (`src/embed.rs`) behind the unchanged `Embedder` trait (O2): 384-dim,
//! CPU, zero network egress at index and query time. The nearest-neighbour
//! search runs *inside the engine* against a `vec0` virtual table — no cosine
//! math remains in app code.
//!
//! ## Storage (schema v4 — span-granular sqlite-vec + resumable embedded_files)
//!
//! ```sql
//! CREATE TABLE documents (
//!   id INTEGER PRIMARY KEY, path TEXT NOT NULL, symbol TEXT NOT NULL,
//!   kind TEXT NOT NULL, start_line INTEGER NOT NULL, end_line INTEGER NOT NULL);
//! CREATE INDEX idx_documents_path ON documents(path);
//! CREATE VIRTUAL TABLE vec_documents USING vec0(embedding float[384] distance_metric=cosine);
//! CREATE TABLE vec_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
//! CREATE TABLE embedded_files (path TEXT PRIMARY KEY, content_hash TEXT NOT NULL);
//! ```
//!
//! One row per SPAN (function/method, or a whole-file fallback for symbol-less
//! files), so `path` is no longer unique. `vec_documents.rowid == documents.id`
//! (1:1). Any older store (v1/v2/v3) is delete-and-full-reindexed on next
//! `init`, driven by `is_current_schema`.
//!
//! Schema v4 adds `embedded_files` which records per-file completion: a file
//! is "embedded" iff its `(path, content_hash)` row is present. The row is
//! committed in the SAME per-file transaction as that file's vec rows, making
//! embedding resumable: a kill loses at most the in-flight file; a re-run
//! continues from where it stopped. See `refresh` and `read_embedded_hashes`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Once;

use rusqlite::Connection;

use crate::embed::{CandleEmbedder, MODEL_NAME};
use crate::Result;

pub const VECTORS_DB: &str = "vectors.db";

/// Embedding dimension of the candle all-MiniLM-L6-v2 model.
pub const DIM: usize = 384;

/// Current store schema version. Bumped v3→v4 to add `embedded_files` table
/// and per-file transaction semantics (resumable embedding).
const SCHEMA_VERSION: &str = "5";

/// Max ANN neighbours a semantic query asks the engine for.
const SEMANTIC_LIMIT: usize = 20;

/// Oversample factor: ask KNN for this many more candidates before dedup so
/// that symbol-level dedup has enough raw material to fill the final limit.
const OVERSAMPLE: usize = 4;

/// A single embeddable span: one function/method body or a whole-file fallback.
#[derive(Debug, Clone)]
pub struct Span {
    /// Function/method name, or `""` for the whole-file fallback.
    pub symbol: String,
    /// `"function"` | `"method"` | `"file"`
    pub kind: String,
    /// 1-based inclusive start line.
    pub start_line: usize,
    /// 1-based inclusive end line.
    pub end_line: usize,
    /// The source text to embed (function body or whole file).
    pub text: String,
}

/// Local, deterministic, zero-egress text embedder.
///
/// Behind a trait so the embedding backend can change without touching
/// consumers (`init` writes, `query` reads). The candle `CandleEmbedder`
/// implements it (O2 — the trait signature is unchanged across the swap).
pub trait Embedder {
    /// Fixed embedding dimension produced by this embedder.
    const DIM: usize;

    /// Embed `text` into a fixed-length L2-normalized vector. Pure, local, and
    /// deterministic for identical input; no network.
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// Register the sqlite-vec extension process-wide, exactly once, before any
/// connection is opened. `sqlite3_auto_extension` makes every
/// subsequent `Connection::open` in this process see the `vec0` module.
fn register_sqlite_vec() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        unsafe {
            // Same registration pattern the sqlite-vec crate documents: cast
            // the extension entry point to the exact signature
            // `sqlite3_auto_extension` expects (matching libsqlite3-sys'
            // generated binding — the middle arg is `*mut *const c_char`).
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *const std::os::raw::c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> std::os::raw::c_int,
            >(sqlite_vec::sqlite3_vec_init as *const ())));
        }
    });
}

/// Open a connection to `vectors.db` with the sqlite-vec extension available.
fn open(lattice_dir: &Path) -> Result<Connection> {
    register_sqlite_vec();
    let conn = Connection::open(lattice_dir.join(VECTORS_DB))
        .map_err(|e| format!("open vectors.db: {e}"))?;
    Ok(conn)
}

/// Create the v4 (span-granular + resumable) schema on a fresh store.
///
/// The `embedded_files` table is used to track per-file completion: its row
/// commits in the same transaction as the file's vec rows, so the store is
/// always consistent (no partial-file state). `read_embedded_hashes` reads
/// this table; a missing table (old store, or first-ever open) is treated as
/// "empty map" by the callers.
fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS documents (
           id         INTEGER PRIMARY KEY,
           path       TEXT NOT NULL,
           symbol     TEXT NOT NULL,
           kind       TEXT NOT NULL,
           start_line INTEGER NOT NULL,
           end_line   INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_documents_path ON documents(path);
         CREATE VIRTUAL TABLE IF NOT EXISTS vec_documents USING vec0(
           embedding float[{DIM}] distance_metric=cosine
         );
         CREATE TABLE IF NOT EXISTS vec_meta (
           key   TEXT PRIMARY KEY,
           value TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS embedded_files (
           path         TEXT PRIMARY KEY,
           content_hash TEXT NOT NULL
         );"
    ))
    .map_err(|e| format!("create vectors.db schema: {e}"))?;
    conn.execute(
        "INSERT OR REPLACE INTO vec_meta (key, value) VALUES ('schema_version', ?1)",
        rusqlite::params![SCHEMA_VERSION],
    )
    .map_err(|e| format!("write schema_version: {e}"))?;
    conn.execute(
        "INSERT OR REPLACE INTO vec_meta (key, value) VALUES ('embedder', ?1)",
        rusqlite::params![MODEL_NAME],
    )
    .map_err(|e| format!("write embedder meta: {e}"))?;
    conn.execute(
        "INSERT OR REPLACE INTO vec_meta (key, value) VALUES ('dim', ?1)",
        rusqlite::params![DIM.to_string()],
    )
    .map_err(|e| format!("write dim meta: {e}"))?;
    Ok(())
}

/// True iff `vectors.db` exists AND carries `vec_meta.schema_version = '4'`.
/// A missing file (first-ever init) or an older store (v1/v2/v3) returns false,
/// so `init` deletes it and re-embeds every span once against the current
/// backend.
pub fn is_current_schema(lattice_dir: &Path) -> bool {
    let db_path = lattice_dir.join(VECTORS_DB);
    if !db_path.is_file() {
        return false;
    }
    register_sqlite_vec();
    let conn = match Connection::open(&db_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let version: rusqlite::Result<String> = conn.query_row(
        "SELECT value FROM vec_meta WHERE key = 'schema_version'",
        [],
        |row| row.get(0),
    );
    matches!(version, Ok(v) if v == SCHEMA_VERSION)
}

/// Delete the store file entirely (used by the v1→v4 migration path in init).
pub fn delete_store(lattice_dir: &Path) -> Result<()> {
    let db_path = lattice_dir.join(VECTORS_DB);
    if db_path.exists() {
        std::fs::remove_file(&db_path).map_err(|e| format!("delete legacy vectors.db: {e}"))?;
    }
    Ok(())
}

/// Read the `embedded_files` table as a `path → content_hash` map.
///
/// Returns an empty map when:
/// - the store file does not exist (first-ever init), or
/// - the store lacks the `embedded_files` table (old v3 store opened directly),
/// - or any other query failure.
///
/// Callers treat missing = "nothing embedded" and proceed to embed everything.
pub fn read_embedded_hashes(lattice_dir: &Path) -> HashMap<String, String> {
    let db_path = lattice_dir.join(VECTORS_DB);
    if !db_path.is_file() {
        return HashMap::new();
    }
    let conn = match open(lattice_dir) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    // The table may not exist on old stores; treat that as empty.
    let mut stmt = match conn.prepare("SELECT path, content_hash FROM embedded_files") {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    let rows = match stmt.query_map([], |row| {
        let path: String = row.get(0)?;
        let hash: String = row.get(1)?;
        Ok((path, hash))
    }) {
        Ok(r) => r,
        Err(_) => return HashMap::new(),
    };
    rows.filter_map(|r| r.ok()).collect()
}

/// Return true iff any path in `indexed_hashes` (path → content_hash) is
/// either absent from `embedded_files` or has a different hash recorded there.
/// Also returns true when the store is absent/non-current-schema (v3 or older)
/// and `indexed_hashes` is non-empty — this is the v3→v4 query-time heal clause
/// (gate #2): a wild v3 store is treated as "all files pending" so query-time
/// freshness triggers a full re-embed via `index::run`.
///
/// Pure SQLite reads — no file I/O, no embedding, no git scan. Safe to call on
/// every query via `freshness::ensure_fresh`.
pub fn has_pending(lattice_dir: &Path, indexed_hashes: &HashMap<String, String>) -> bool {
    if indexed_hashes.is_empty() {
        return false;
    }
    // v3→v4 heal clause: non-current schema with a non-empty index = pending.
    if !is_current_schema(lattice_dir) {
        return true;
    }
    let embedded = read_embedded_hashes(lattice_dir);
    for (path, hash) in indexed_hashes {
        match embedded.get(path) {
            Some(recorded) if recorded == hash => {} // up to date
            _ => return true,                        // missing or hash mismatch
        }
    }
    false
}

/// Count of rows in `documents`. Returns 0 on any error or missing store.
pub fn document_count(lattice_dir: &Path) -> i64 {
    let db_path = lattice_dir.join(VECTORS_DB);
    if !db_path.is_file() {
        return 0;
    }
    let conn = match open(lattice_dir) {
        Ok(c) => c,
        Err(_) => return 0,
    };
    conn.query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
        .unwrap_or(0)
}

/// Refresh the vector store for a set of (re)indexed and removed paths.
///
/// The model must be present (guarded upstream by `embed::ensure_model_present`
/// before init reaches this); loading it here surfaces `EmbedError` as a boxed
/// error if it is somehow absent.
///
/// `reindexed` is a list of `(rel_path, content_hash, spans)` — every path here
/// has ALL of its existing rows deleted first, then one row is inserted per span
/// (replace-all semantics). Each file's vectors AND its `embedded_files` row are
/// committed in ONE transaction per file: atomicity by construction, so an
/// interrupted run loses at most the in-flight file and resumes on the next run.
///
/// `removed` is a list of paths whose rows (documents + vecs + embedded row)
/// should be deleted entirely.
/// `prog` reports per-file embedding progress. Embedding is the slowest stage of
/// `init` by far (a BERT forward pass per span), so this is where a footer
/// matters most; pass `Progress::hidden()` for a silent run. When `prog` is
/// active it supersedes the every-10-files `eprintln!` note below, so the two
/// never interleave.
pub fn refresh(
    lattice_dir: &Path,
    reindexed: &[(String, String, Vec<Span>)],
    removed: &[String],
    prog: &crate::progress::Progress,
) -> Result<()> {
    let mut conn = open(lattice_dir)?;
    create_schema(&conn)?;

    // Nothing to embed → still ensure schema exists, then return.
    if reindexed.is_empty() && removed.is_empty() {
        return Ok(());
    }

    let embedder = CandleEmbedder::from_env()
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;

    // Process removed paths first in one shared transaction (no embed needed).
    if !removed.is_empty() {
        let tx = conn
            .transaction()
            .map_err(|e| format!("begin vectors.db remove transaction: {e}"))?;
        for path in removed {
            delete_path_in_tx(path, &tx)?;
        }
        tx.commit()
            .map_err(|e| format!("commit vectors.db remove transaction: {e}"))?;
    }

    // Per-file transactions: one commit per file so the store is always consistent.
    // On interrupt: every committed file stays; only the in-flight file is lost.
    let total = reindexed.len();
    for (idx, (path, content_hash, spans)) in reindexed.iter().enumerate() {
        prog.step("embed", idx + 1, total, Some(path));
        let tx = conn
            .transaction()
            .map_err(|e| format!("begin per-file tx for {path}: {e}"))?;

        // Replace all existing rows for this path (documents + vecs + completion row).
        delete_path_in_tx(path, &tx)?;

        // Insert one row per span.
        for span in spans {
            tx.execute(
                "INSERT INTO documents (path, symbol, kind, start_line, end_line)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    path,
                    span.symbol,
                    span.kind,
                    span.start_line as i64,
                    span.end_line as i64,
                ],
            )
            .map_err(|e| format!("insert document row for {path}: {e}"))?;
            let id = tx.last_insert_rowid();
            let vec = embedder.embed(&span.text);
            tx.execute(
                "INSERT INTO vec_documents (rowid, embedding) VALUES (?1, ?2)",
                rusqlite::params![id, vec_to_json(&vec)],
            )
            .map_err(|e| format!("insert vec row for {path}: {e}"))?;
        }

        // Record completion IN THE SAME TRANSACTION — the marker and the vectors
        // are atomic. If this tx commits, the file is fully embedded. If not,
        // neither the vectors nor this marker survive, and the next run retries.
        tx.execute(
            "INSERT OR REPLACE INTO embedded_files (path, content_hash) VALUES (?1, ?2)",
            rusqlite::params![path, content_hash],
        )
        .map_err(|e| format!("insert embedded_files row for {path}: {e}"))?;

        tx.commit()
            .map_err(|e| format!("commit per-file tx for {path}: {e}"))?;

        // Optional progress note to stderr so long inits are visibly alive.
        // Suppressed when a live footer is drawing: the footer already reports
        // every file, and interleaving the two would scroll the screen.
        if !prog.is_active() && ((idx + 1) % 10 == 0 || idx + 1 == total) {
            eprintln!("lattice: embedded {}/{} files", idx + 1, total);
        }
    }

    Ok(())
}

/// Delete all document rows (and their vec rows, and `embedded_files` entry)
/// for `path` within the given transaction.
fn delete_path_in_tx(path: &str, tx: &rusqlite::Transaction) -> Result<()> {
    // Collect ids first (path is not UNIQUE — may be multiple rows per path).
    let mut stmt = tx
        .prepare("SELECT id FROM documents WHERE path = ?1")
        .map_err(|e| format!("prepare select ids: {e}"))?;
    let ids: Vec<i64> = stmt
        .query_map(rusqlite::params![path], |row| row.get(0))
        .map_err(|e| format!("query ids for {path}: {e}"))?
        .filter_map(|r| r.ok())
        .collect();
    for id in &ids {
        tx.execute(
            "DELETE FROM vec_documents WHERE rowid = ?1",
            rusqlite::params![id],
        )
        .map_err(|e| format!("delete vec row {id}: {e}"))?;
    }
    tx.execute(
        "DELETE FROM documents WHERE path = ?1",
        rusqlite::params![path],
    )
    .map_err(|e| format!("delete document rows for {path}: {e}"))?;
    // Also remove the completion marker so the next run knows this file is pending.
    tx.execute(
        "DELETE FROM embedded_files WHERE path = ?1",
        rusqlite::params![path],
    )
    .map_err(|e| format!("delete embedded_files row for {path}: {e}"))?;
    Ok(())
}

/// A ranked semantic hit.
pub struct Hit {
    pub path: String,
    /// Function/method name; `""` for whole-file fallback spans.
    pub symbol: String,
    /// `"function"` | `"method"` | `"file"`
    pub kind: String,
    /// 1-based inclusive start line of the matched span.
    pub start_line: usize,
    /// 1-based inclusive end line of the matched span.
    pub end_line: usize,
    pub score: f32,
}

/// Embed `query_text` with the candle embedder, then run KNN inside sqlite-vec,
/// oversampling to `min(row_count, SEMANTIC_LIMIT * OVERSAMPLE)` so symbol-level
/// dedup still yields `SEMANTIC_LIMIT` results. Score stays "higher is better":
/// `score = 1.0 - distance`; hits are deduped to the best span per (path, symbol)
/// and ordered score desc, then path asc, then start_line asc for stable output.
pub fn query(lattice_dir: &Path, query_text: &str) -> Result<Vec<Hit>> {
    let embedder = CandleEmbedder::from_env()
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    let q = embedder.embed(query_text);

    let conn = open(lattice_dir)?;

    let doc_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
        .map_err(|e| format!("count documents: {e}"))?;
    if doc_count == 0 {
        return Ok(Vec::new());
    }
    // Oversample so that after symbol-level dedup we still have SEMANTIC_LIMIT
    // candidates (one file can now contribute multiple spans).
    let k = std::cmp::min(doc_count as usize, SEMANTIC_LIMIT * OVERSAMPLE);

    let mut stmt = conn
        .prepare(
            "SELECT d.path, d.symbol, d.kind, d.start_line, d.end_line, v.distance
             FROM vec_documents v
             JOIN documents d ON d.id = v.rowid
             WHERE v.embedding MATCH ?1 AND k = ?2
             ORDER BY v.distance ASC",
        )
        .map_err(|e| format!("prepare knn query: {e}"))?;
    let rows = stmt
        .query_map(rusqlite::params![vec_to_json(&q), k as i64], |row| {
            let path: String = row.get(0)?;
            let symbol: String = row.get(1)?;
            let kind: String = row.get(2)?;
            let start_line: i64 = row.get(3)?;
            let end_line: i64 = row.get(4)?;
            let distance: f64 = row.get(5)?;
            Ok((path, symbol, kind, start_line, end_line, distance))
        })
        .map_err(|e| format!("run knn query: {e}"))?;

    // Dedup: keep best score per (path, symbol) key.
    let mut dedup: std::collections::HashMap<(String, String), Hit> =
        std::collections::HashMap::new();
    for row in rows {
        let (path, symbol, kind, start_line, end_line, distance) =
            row.map_err(|e| format!("read knn row: {e}"))?;
        let score = 1.0 - distance as f32;
        let key = (path.clone(), symbol.clone());
        let entry = dedup.entry(key).or_insert_with(|| Hit {
            path: path.clone(),
            symbol: symbol.clone(),
            kind: kind.clone(),
            start_line: start_line as usize,
            end_line: end_line as usize,
            score,
        });
        // Keep the better score for this (path, symbol) pair.
        if score > entry.score {
            *entry = Hit {
                path,
                symbol,
                kind,
                start_line: start_line as usize,
                end_line: end_line as usize,
                score,
            };
        }
    }

    let mut hits: Vec<Hit> = dedup.into_values().collect();

    // Sort: score desc, then path asc, then start_line asc for byte-stable output.
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.start_line.cmp(&b.start_line))
    });
    hits.truncate(SEMANTIC_LIMIT);
    Ok(hits)
}

/// Nearest neighbors of a symbol's stored embedding. Does not re-embed.
/// Excludes the symbol itself. `not_found` / `ambiguous` are returned as errors
/// whose messages the router maps to those codes.
pub fn similar(lattice_dir: &Path, symbol: &str, limit: usize) -> Result<Vec<Hit>> {
    let conn = open(lattice_dir)?;
    let mut stmt = conn
        .prepare("SELECT id, path, symbol FROM documents WHERE symbol = ?1 ORDER BY path ASC, id ASC")
        .map_err(|e| format!("prepare similar lookup: {e}"))?;
    let rows = stmt
        .query_map([symbol], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| format!("lookup symbol embedding: {e}"))?;
    let mut matches: Vec<(i64, String, String)> = Vec::new();
    for row in rows {
        matches.push(row.map_err(|e| format!("read symbol row: {e}"))?);
    }
    if matches.is_empty() {
        return Err(format!("not_found: no embedded symbol {symbol:?}").into());
    }
    if matches.len() > 1 {
        return Err(format!("ambiguous: {symbol:?} matches {} symbols", matches.len()).into());
    }
    let (rowid, self_path, self_symbol) = matches.remove(0);
    let embedding_json: String = conn
        .query_row(
            "SELECT vec_to_json(embedding) FROM vec_documents WHERE rowid = ?1",
            [rowid],
            |row| row.get(0),
        )
        .map_err(|e| format!("read stored embedding: {e}"))?;

    let doc_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
        .map_err(|e| format!("count documents: {e}"))?;
    if doc_count == 0 {
        return Ok(Vec::new());
    }
    let k = std::cmp::min(doc_count as usize, limit.saturating_mul(OVERSAMPLE).max(limit + 1));
    let mut knn = conn
        .prepare(
            "SELECT d.path, d.symbol, d.kind, d.start_line, d.end_line, v.distance
             FROM vec_documents v
             JOIN documents d ON d.id = v.rowid
             WHERE v.embedding MATCH ?1 AND k = ?2
             ORDER BY v.distance ASC",
        )
        .map_err(|e| format!("prepare similar knn: {e}"))?;
    let knn_rows = knn
        .query_map(rusqlite::params![embedding_json, k as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, f64>(5)?,
            ))
        })
        .map_err(|e| format!("run similar knn: {e}"))?;

    let mut hits = Vec::new();
    for row in knn_rows {
        let (path, sym, kind, start_line, end_line, distance) =
            row.map_err(|e| format!("read similar row: {e}"))?;
        if path == self_path && sym == self_symbol {
            continue;
        }
        hits.push(Hit {
            path,
            symbol: sym,
            kind,
            start_line: start_line as usize,
            end_line: end_line as usize,
            score: 1.0 - distance as f32,
        });
        if hits.len() >= limit {
            break;
        }
    }
    Ok(hits)
}

/// Split `text` into lowercase sub-tokens on non-alphanumeric boundaries and
/// camelCase/PascalCase transitions (`validateToken` -> [`validate`, `token`]).
/// A general text utility retained from the v1 embedder; the FTS symbol index
/// (slice 8) tokenizes symbol names with the same splitter so lexical search
/// matches sub-identifiers. Pure and dependency-free.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        split_identifier(word, &mut out);
    }
    out
}

fn split_identifier(word: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = word.chars().collect();
    let mut start = 0usize;
    for i in 1..chars.len() {
        let prev = chars[i - 1];
        let cur = chars[i];
        // Boundary when a lowercase/digit is followed by an uppercase letter
        // (camelCase: validateToken -> validate | Token).
        let boundary = (prev.is_lowercase() || prev.is_ascii_digit()) && cur.is_uppercase();
        if boundary {
            let sub: String = chars[start..i].iter().collect();
            out.push(sub.to_lowercase());
            start = i;
        }
    }
    let sub: String = chars[start..].iter().collect();
    if !sub.is_empty() {
        out.push(sub.to_lowercase());
    }
}

/// Serialize an f32 vector as the JSON array text sqlite-vec accepts for a
/// `float[N]` column (`[0.1,0.2,...]`). sqlite-vec parses this into the packed
/// float vector; a Vec<u8> little-endian blob works too, but JSON keeps the
/// bind path dependency-free and matches the extension's documented input.
fn vec_to_json(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        // Full precision so the round-trip is exact and deterministic.
        s.push_str(&format!("{x}"));
    }
    s.push(']');
    s
}
