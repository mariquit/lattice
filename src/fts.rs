// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Lexical symbol search via SQLite FTS5, colocated in `.lattice/meta.db`.
//!
//! Complements the semantic (vector) backend: feature-hash embeddings rank
//! whole files by bag-of-words similarity, which is weak at exact symbol
//! lookup. This index answers "find the symbol named / matching X" — the
//! tokensave-style search lattice previously lacked.
//!
//! Symbol names are tokenized with the same snake/camelCase splitter the
//! embedder uses (`vectors::tokenize`) and stored in an indexed `tokens`
//! column, so a search for `validate` matches `validate_token`. The index is
//! rebuilt from the in-memory resolved graph each `init` (no parse cost, pure
//! deterministic function of the tree — same posture as the graph store).
//!
//! Slice 14 additions: `class` and `in_degree` UNINDEXED columns. `class`
//! enables `--exclude-tests` filtering. `in_degree` (count of incoming
//! calls+imports edges) breaks rank ties so higher-centrality symbols surface
//! first.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::Connection;

use crate::class;
use crate::meta::META_DB;
use crate::router::DEFAULT_LIMIT;
use crate::vectors::tokenize;
use crate::Result;

/// A symbol to index (function/method/module) from the resolved graph.
pub struct Symbol {
    pub name: String,
    pub path: String,
    pub kind: String, // "function" | "method" | "module"
    pub line: usize,  // 1-based; modules use 1
    /// The graph id for this symbol — used to look up in_degree.
    /// For free functions: "<path>::<name>"
    /// For methods:        "<path>::<Type>::<name>"
    /// For modules:        "<path>"
    pub id: String,
}

/// A ranked symbol search hit.
pub struct SymbolHit {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub line: usize,
    pub class: String,
    /// Count of incoming call+import edges for this symbol (used for RRF
    /// tie-breaking in hybrid dispatch). Always 0 for old stores that
    /// predate the in_degree column.
    pub in_degree: usize,
}

/// Drop and rebuild the `symbols` FTS5 table from `symbols`. Deterministic:
/// rows are inserted in (path, line, name) order.
///
/// `in_degree_map`: graph id → incoming edge count. Built in `index::run` after
/// graph::refresh, counting (calls callee + imports target) edges. Passed here
/// so each row stores its in_degree for ORDER BY ranking.
pub fn rebuild(
    lattice_dir: &Path,
    symbols: &[Symbol],
    in_degree_map: &HashMap<String, usize>,
) -> Result<()> {
    let conn =
        Connection::open(lattice_dir.join(META_DB)).map_err(|e| format!("open meta.db: {e}"))?;
    conn.execute_batch(
        "DROP TABLE IF EXISTS symbols;
         CREATE VIRTUAL TABLE symbols USING fts5(
             name, tokens, path UNINDEXED, kind UNINDEXED, line UNINDEXED,
             class UNINDEXED, in_degree UNINDEXED
         );",
    )
    .map_err(|e| format!("create symbols FTS5 table: {e}"))?;

    let mut sorted: Vec<&Symbol> = symbols.iter().collect();
    sorted.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.name.cmp(&b.name))
    });
    for s in sorted {
        let tokens = tokenize(&s.name).join(" ");
        let class_str = class::classify(&s.path).as_str();
        let indeg = in_degree_map.get(&s.id).copied().unwrap_or(0);
        conn.execute(
            "INSERT INTO symbols (name, tokens, path, kind, line, class, in_degree) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![s.name, tokens, s.path, s.kind, s.line as i64, class_str, indeg as i64],
        )
        .map_err(|e| format!("insert symbol row: {e}"))?;
    }
    Ok(())
}

/// Filters applied to a symbol search query.
pub struct Filters {
    /// Maximum hits to return (default DEFAULT_LIMIT).
    pub limit: usize,
    /// When true, exclude symbols whose class is "test" or "fixture".
    pub exclude_tests: bool,
    /// When Some, restrict to symbols of this kind ("function"|"method"|"module").
    pub kind: Option<String>,
}

impl Default for Filters {
    fn default() -> Self {
        Filters {
            limit: DEFAULT_LIMIT,
            exclude_tests: false,
            kind: None,
        }
    }
}

/// Column weights for explicit bm25() ordering.
///
/// `name` gets 10× weight, `tokens` gets 1×.  The remaining five columns
/// (`path`, `kind`, `line`, `class`, `in_degree`) are all UNINDEXED and
/// contribute 0 regardless of weight, so they are omitted from the call.
///
/// Rationale: a query like "validate" should rank a symbol whose *name* is
/// "validate_foo" above one that only matches via the `tokens` column (e.g.
/// "reValidate" whose camelCase expansion contains "validate").  Under equal
/// weights, short-doc tokens-column hits sometimes outrank long-name-column
/// hits due to BM25 length normalisation.  The 10:1 ratio decisively boosts
/// name-column matches.  bm25() returns more-negative = more-relevant, so
/// `ORDER BY bm25(…) ASC` (the default) is correct.
const BM25_WEIGHT_NAME: f64 = 10.0;
const BM25_WEIGHT_TOKENS: f64 = 1.0;

/// FTS5 prefix search over symbol names with optional filtering.
///
/// Results ordered by `bm25(symbols, name_weight, tokens_weight)` with explicit
/// column weights (name 10×, tokens 1×), then `in_degree DESC` (more-central
/// symbols first), then `name`, then `path` for full determinism.
///
/// Returns empty if the index is absent (a store from before slice 8).
pub fn search(
    lattice_dir: &Path,
    query_text: &str,
    filters: &Filters,
) -> Result<Vec<SymbolHit>> {
    let conn =
        Connection::open(lattice_dir.join(META_DB)).map_err(|e| format!("open meta.db: {e}"))?;
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='symbols'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        return Ok(Vec::new());
    }

    // Build the FTS5 match expression. tokenize() yields alphanumeric-only
    // sub-tokens so quoting is injection-safe against FTS5 syntax chars.
    let terms: Vec<String> = tokenize(query_text)
        .into_iter()
        .map(|t| format!("\"{t}\"*"))
        .collect();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let match_expr = terms.join(" OR ");

    // Build optional extra WHERE clauses. `kind` is safe: only whitelisted
    // values are accepted (validated upstream in router::extract_flags).
    // `class NOT IN (...)` uses a literal (not user input).
    let mut extra = String::new();
    if let Some(ref k) = filters.kind {
        // Safety: only the three whitelisted values reach here.
        if k == "function" || k == "method" || k == "module" {
            extra.push_str(&format!(" AND kind = '{k}'"));
        }
    }
    if filters.exclude_tests {
        extra.push_str(" AND class NOT IN ('test','fixture')");
    }

    let sql = format!(
        "SELECT path, name, kind, line, class, CAST(in_degree AS INTEGER) FROM symbols \
         WHERE symbols MATCH ?1{} \
         ORDER BY bm25(symbols, {BM25_WEIGHT_NAME}, {BM25_WEIGHT_TOKENS}), \
                  CAST(in_degree AS INTEGER) DESC, name, path \
         LIMIT ?2",
        extra
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("prepare symbol search: {e}"))?;
    let rows = stmt
        .query_map(rusqlite::params![match_expr, filters.limit as i64], |row| {
            Ok(SymbolHit {
                path: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                line: row.get::<_, i64>(3)? as usize,
                class: row.get(4)?,
                in_degree: row.get::<_, i64>(5)? as usize,
            })
        })
        .map_err(|e| format!("run symbol search: {e}"))?;
    let mut hits = Vec::new();
    for r in rows {
        hits.push(r.map_err(|e| format!("read symbol hit: {e}"))?);
    }
    Ok(hits)
}

/// Batch-lookup `in_degree` values for a set of `(path, name)` pairs from the
/// `symbols` FTS5 table. Returns a map from `"{path}::{name}"` to `in_degree`.
///
/// Keys absent from the table (e.g. whole-file vector spans whose symbol is "")
/// default to 0 and simply won't appear in the returned map — callers should
/// use `.get(&key).copied().unwrap_or(0)`.
///
/// A single prepared SELECT is used; the table holds an UNINDEXED `in_degree`
/// column stored as an integer (written by `rebuild`).
pub fn in_degrees(
    lattice_dir: &Path,
    keys: &[(String, String)], // (path, name)
) -> Result<HashMap<String, usize>> {
    if keys.is_empty() {
        return Ok(HashMap::new());
    }
    let conn =
        Connection::open(lattice_dir.join(META_DB)).map_err(|e| format!("open meta.db: {e}"))?;
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='symbols'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        return Ok(HashMap::new());
    }

    // Use a single query with OR-joined (path, name) pairs.
    // This is safe: paths and names are stored values (not user input to this
    // function at runtime), but we still use ? placeholders for correctness.
    // For large key sets we query in batches of 50 to stay under any SQLITE_LIMIT.
    let mut result: HashMap<String, usize> = HashMap::new();
    for chunk in keys.chunks(50) {
        let placeholders: String = chunk
            .iter()
            .map(|_| "(path = ? AND name = ?)")
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT path, name, CAST(in_degree AS INTEGER) FROM symbols WHERE {placeholders}"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("prepare in_degrees lookup: {e}"))?;
        // Flatten (path, name) pairs into a flat params list.
        let params_vec: Vec<&dyn rusqlite::ToSql> = chunk
            .iter()
            .flat_map(|(p, n)| {
                let p: &dyn rusqlite::ToSql = p;
                let n: &dyn rusqlite::ToSql = n;
                [p, n]
            })
            .collect();
        let rows = stmt
            .query_map(params_vec.as_slice(), |row| {
                let path: String = row.get(0)?;
                let name: String = row.get(1)?;
                let deg: i64 = row.get(2)?;
                Ok((path, name, deg as usize))
            })
            .map_err(|e| format!("run in_degrees query: {e}"))?;
        for r in rows {
            let (path, name, deg) = r.map_err(|e| format!("read in_degrees row: {e}"))?;
            result.insert(format!("{path}::{name}"), deg);
        }
    }
    Ok(result)
}
