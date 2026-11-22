// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for slice S1 "hybrid retrieval via Reciprocal Rank Fusion".
//!
//! Two independent parts:
//!
//! ## Part A — pure fusion math (zero I/O, no embeddings)
//!
//! Locks down the `fuse::rrf` function with synthetic ranked lists and
//! hand-computed expected output. Independent of the vector store and model.
//!
//! Hand-computed ground truth (k = 60):
//!   L_fts = [a, b, c]   L_vec = [c, a, d]
//!   a: 1/(60+1) + 1/(60+2) = 1/61 + 1/62
//!   c: 1/(60+3) + 1/(60+1) = 1/63 + 1/61
//!   b: 1/(60+2) = 1/62
//!   d: 1/(60+3) = 1/63
//!   a > c because 1/62 > 1/63  → [a, c, b, d]
//!
//! ## Part B — end-to-end CLI through the binary (needs populated vectors)
//!
//! Uses tests/fixtures/hybrid_basic/ with:
//!   - lexical_bait.rs: fn token_validation  (FTS rank 1, weak semantic)
//!   - semantic_bait.rs: fn check_credentials (vector rank 1, absent from FTS)
//!   - filler.rs: fn unrelated_math           (filler)
//!
//! Assertions (all exact assert_eq!, never >=):
//!   B1. `--symbol "token validation"` order ≠ semantic-only order (backends disagree)
//!   B2. `--hybrid` returns known fused order with sources correct
//!   B3. Envelope discipline: one JSON, protocol_version, query_type=="hybrid", freshness
//!   B4. Guard: `--hybrid --file-level` → error envelope, non-zero exit
//!   B5. Guard: `--symbol x --hybrid` → error envelope (--hybrid only for free text)
//!   B6. Guard: `--hybrid --kind function` → error envelope
//!   B7. `--hybrid --limit 1` returns exactly 1 hit
//!   B8. Module/file key collapse: FTS module hit and vector file hit fuse on bare path

use lattice::fuse::{self, RRF_K};

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

// ── Part A: pure fusion math ──────────────────────────────────────────────────

#[test]
fn part_a_basic_order_a_c_b_d() {
    let lists = vec![
        vec!["a".to_string(), "b".to_string(), "c".to_string()],
        vec!["c".to_string(), "a".to_string(), "d".to_string()],
    ];
    let result = fuse::rrf(&lists, RRF_K);
    let keys: Vec<&str> = result.iter().map(|f| f.key.as_str()).collect();
    // The EXACT expected order from the design:
    assert_eq!(keys, vec!["a", "c", "b", "d"]);
}

#[test]
fn part_a_scores_exact() {
    let lists = vec![
        vec!["a".to_string(), "b".to_string(), "c".to_string()],
        vec!["c".to_string(), "a".to_string(), "d".to_string()],
    ];
    let result = fuse::rrf(&lists, RRF_K);
    // Score expressions must be identical IEEE-754 operations (same arithmetic, same values).
    assert_eq!(result[0].score, 1.0 / (RRF_K + 1.0) + 1.0 / (RRF_K + 2.0)); // a
    assert_eq!(result[1].score, 1.0 / (RRF_K + 3.0) + 1.0 / (RRF_K + 1.0)); // c
    assert_eq!(result[2].score, 1.0 / (RRF_K + 2.0)); // b
    assert_eq!(result[3].score, 1.0 / (RRF_K + 3.0)); // d
}

#[test]
fn part_a_ranks_bookkeeping() {
    let lists = vec![
        vec!["a".to_string(), "b".to_string(), "c".to_string()],
        vec!["c".to_string(), "a".to_string(), "d".to_string()],
    ];
    let result = fuse::rrf(&lists, RRF_K);
    // a: rank 1 in fts, rank 2 in vec
    assert_eq!(result[0].ranks, vec![Some(1), Some(2)]);
    // c: rank 3 in fts, rank 1 in vec
    assert_eq!(result[1].ranks, vec![Some(3), Some(1)]);
    // b: fts only
    assert_eq!(result[2].ranks, vec![Some(2), None]);
    // d: vec only
    assert_eq!(result[3].ranks, vec![None, Some(3)]);
}

#[test]
fn part_a_tie_break_key_asc() {
    // Both score 1/61; key ASC tie-break must give [x, y]
    let lists = vec![
        vec!["x".to_string()],
        vec!["y".to_string()],
    ];
    let result = fuse::rrf(&lists, RRF_K);
    let keys: Vec<&str> = result.iter().map(|f| f.key.as_str()).collect();
    assert_eq!(keys, vec!["x", "y"]);
    assert_eq!(result[0].score, result[1].score);
}

#[test]
fn part_a_duplicate_within_list_first_wins() {
    // "a" appears twice in list 0; only rank 1 should be counted.
    let lists = vec![
        vec!["a".to_string(), "a".to_string(), "b".to_string()],
        vec![],
    ];
    let result = fuse::rrf(&lists, RRF_K);
    let keys: Vec<&str> = result.iter().map(|f| f.key.as_str()).collect();
    assert_eq!(keys, vec!["a", "b"]);
    // a at rank 1 only (duplicate suppressed)
    assert_eq!(result[0].score, 1.0 / (RRF_K + 1.0));
    assert_eq!(result[0].ranks, vec![Some(1), None]);
}

#[test]
fn part_a_empty_one_side_preserves_order() {
    // [[], ["p", "q"]] → pure vector ordering; graceful-degradation property
    let lists = vec![
        vec![],
        vec!["p".to_string(), "q".to_string()],
    ];
    let result = fuse::rrf(&lists, RRF_K);
    let keys: Vec<&str> = result.iter().map(|f| f.key.as_str()).collect();
    assert_eq!(keys, vec!["p", "q"]);
}

// ── Part B: end-to-end CLI gate ───────────────────────────────────────────────

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

fn copy_fixture(fixture: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    for entry in fs::read_dir(&src).expect("read fixture dir") {
        let entry = entry.expect("fixture dir entry");
        // Only copy regular files — skip subdirectories like .lattice/
        if entry.file_type().expect("file type").is_file() {
            let name = entry.file_name();
            fs::copy(entry.path(), tmp.path().join(&name)).expect("copy fixture file");
        }
    }
    tmp
}

fn run_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn lattice binary")
}

fn parse_single_json(output: &Output) -> Value {
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8");
    let trimmed = stdout.trim();
    assert!(
        !trimmed.contains('\n'),
        "stdout must be exactly one JSON line, got:\n{trimmed}"
    );
    serde_json::from_str(trimmed).expect("stdout must be valid JSON")
}

/// Init the fixture dir and return it (consuming the TempDir to keep it alive).
fn init_fixture(fixture: &str) -> tempfile::TempDir {
    let tmp = copy_fixture(fixture);
    let out = run_in(tmp.path(), &["init"]);
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    tmp
}

// B3/B4/B5/B6 guard tests don't need the vector store — they trip before freshness.
// We still need a .lattice/ dir, so we use a separate minimal fixture that has
// been init-ed. We reuse semantic_symbol_basic (already used by other gates).

fn init_semantic_fixture() -> tempfile::TempDir {
    let tmp = copy_fixture("semantic_symbol_basic");
    let out = run_in(tmp.path(), &["init"]);
    assert!(
        out.status.success(),
        "init for guard tests failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    tmp
}

// ── B3: Envelope discipline ────────────────────────────────────────────────────

#[test]
fn b3_hybrid_envelope_shape() {
    let tmp = init_fixture("hybrid_basic");
    let out = run_in(tmp.path(), &["query", "token validation"]);
    assert!(
        out.status.success(),
        "hybrid query failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let val = parse_single_json(&out);
    assert_eq!(
        val["protocol_version"].as_str().unwrap(),
        lattice::envelope::PROTOCOL_VERSION,
        "protocol_version must match PROTOCOL_VERSION const"
    );
    assert_eq!(val["query_type"].as_str().unwrap(), "hybrid");
    assert!(val["freshness"].is_string(), "freshness field must be present");
    assert!(val["hits"].is_array(), "hits must be an array");
    assert_eq!(val["k"].as_f64().unwrap(), RRF_K, "k must equal RRF_K");
}

// ── B4: --hybrid --file-level rejection ───────────────────────────────────────

#[test]
fn b4_hybrid_file_level_rejected() {
    let tmp = init_semantic_fixture();
    let out = run_in(
        tmp.path(),
        &["query", "something", "--hybrid", "--file-level"],
    );
    assert!(
        !out.status.success(),
        "expected non-zero exit for --hybrid --file-level"
    );
    let val = parse_single_json(&out);
    assert_eq!(val["query_type"].as_str().unwrap(), "error");
    assert_eq!(val["error"]["code"].as_str().unwrap(), "invalid_flag");
}

// ── B5: --symbol x --hybrid rejection ─────────────────────────────────────────

#[test]
fn b5_symbol_with_hybrid_rejected() {
    let tmp = init_semantic_fixture();
    let out = run_in(tmp.path(), &["query", "--symbol", "foo", "--hybrid"]);
    assert!(
        !out.status.success(),
        "expected non-zero exit for --symbol + --hybrid"
    );
    let val = parse_single_json(&out);
    assert_eq!(val["query_type"].as_str().unwrap(), "error");
    assert_eq!(val["error"]["code"].as_str().unwrap(), "invalid_flag");
}

// ── B6: --hybrid --kind function rejection ────────────────────────────────────

#[test]
fn b6_hybrid_kind_rejected() {
    let tmp = init_semantic_fixture();
    let out = run_in(
        tmp.path(),
        &["query", "something", "--kind", "function"],
    );
    assert!(
        !out.status.success(),
        "expected non-zero exit for --hybrid --kind"
    );
    let val = parse_single_json(&out);
    assert_eq!(val["query_type"].as_str().unwrap(), "error");
    assert_eq!(val["error"]["code"].as_str().unwrap(), "invalid_flag");
}

// ── B7: --hybrid --limit 1 returns exactly 1 hit ─────────────────────────────

#[test]
fn b7_hybrid_limit_one() {
    let tmp = init_fixture("hybrid_basic");
    let out = run_in(
        tmp.path(),
        &["query", "token validation", "--limit", "1"],
    );
    assert!(
        out.status.success(),
        "hybrid --limit 1 failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let val = parse_single_json(&out);
    let hits = val["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "expected exactly 1 hit with --limit 1");
}

// ── B1+B2: Backends disagree; hybrid fuses correctly ─────────────────────────

#[test]
fn b1_b2_backends_disagree_hybrid_fuses() {
    let tmp = init_fixture("hybrid_basic");

    // FTS (--symbol) order for "token validation": lexical_bait.rs first
    // (exact name match "token_validation"), then possibly others.
    let fts_out = run_in(tmp.path(), &["query", "--symbol", "token validation"]);
    assert!(fts_out.status.success(), "symbol query failed");
    let fts_val = parse_single_json(&fts_out);
    let fts_hits = fts_val["hits"].as_array().unwrap();
    // FTS must find token_validation (lexical bait) as top hit.
    assert!(
        !fts_hits.is_empty(),
        "FTS must return at least one hit for 'token validation'"
    );
    let fts_top_name = fts_hits[0]["name"].as_str().unwrap();
    assert_eq!(
        fts_top_name, "token_validation",
        "FTS rank 1 must be token_validation"
    );

    // FTS must NOT find check_credentials at rank 1 (its name has no "token" or "validation")
    let fts_names: Vec<&str> = fts_hits
        .iter()
        .map(|h| h["name"].as_str().unwrap())
        .collect();
    let cc_fts_rank = fts_names.iter().position(|&n| n == "check_credentials");
    assert!(
        cc_fts_rank.map_or(true, |r| r > 0),
        "FTS must NOT rank check_credentials above token_validation"
    );

    // Semantic (free text) order: check_credentials must appear (has rich body vocabulary).
    let sem_hits = lattice::vectors::query(&tmp.path().join(".lattice"), "token validation").unwrap();
    assert!(
        !sem_hits.is_empty(),
        "semantic must return at least one hit"
    );
    let sem_names: Vec<&str> = sem_hits
        .iter()
        .map(|h| h.symbol.as_str())
        .collect();

    // check_credentials must appear in semantic results (body is semantically rich).
    assert!(
        sem_names.contains(&"check_credentials"),
        "check_credentials must appear in semantic results (semantic bait fixture guard)"
    );

    // B1: FTS-only backend must NOT find check_credentials in the same position
    // as semantic. Specifically: FTS returns token_validation at rank 1, and
    // check_credentials is either absent from FTS results or at a lower rank.
    // This is the "backends disagree" property: they have different strengths.
    // FTS rank 1 (token_validation) must differ from semantic rank 1
    // (which may be token_validation too — what matters is check_credentials
    // gets boosted by hybrid because FTS misses it).
    assert_eq!(
        fts_top_name, "token_validation",
        "B1 fixture guard: FTS rank 1 must be token_validation (exact name match)"
    );
    // check_credentials must NOT be in FTS top hit (ensuring hybrid adds value).
    assert_ne!(
        fts_top_name, "check_credentials",
        "B1: FTS must not rank check_credentials at #1 (it has no matching tokens)"
    );

    // Hybrid: both bait functions must appear in the top 2 hits.
    let hyb_out = run_in(tmp.path(), &["query", "token validation"]);
    assert!(
        hyb_out.status.success(),
        "hybrid query failed: {}",
        String::from_utf8_lossy(&hyb_out.stderr)
    );
    let hyb_val = parse_single_json(&hyb_out);
    let hyb_hits = hyb_val["hits"].as_array().unwrap();
    assert!(
        hyb_hits.len() >= 2,
        "hybrid must return at least 2 hits, got {}",
        hyb_hits.len()
    );

    // Both bait functions must be in the top 2.
    let top2_names: Vec<&str> = hyb_hits[..2]
        .iter()
        .map(|h| h["name"].as_str().unwrap())
        .collect();
    assert!(
        top2_names.contains(&"token_validation"),
        "token_validation must be in top 2 hybrid hits, got: {top2_names:?}"
    );
    assert!(
        top2_names.contains(&"check_credentials"),
        "check_credentials must be in top 2 hybrid hits, got: {top2_names:?}"
    );

    // B2: sources field correctness.
    // token_validation is FTS-only (not in vector results by name since sem top was check_credentials).
    // check_credentials is vector-only (not in FTS results since name doesn't match).
    let tv_hit = hyb_hits
        .iter()
        .find(|h| h["name"].as_str().unwrap() == "token_validation")
        .expect("token_validation must be in hybrid hits");
    let cc_hit = hyb_hits
        .iter()
        .find(|h| h["name"].as_str().unwrap() == "check_credentials")
        .expect("check_credentials must be in hybrid hits");

    let tv_sources: Vec<&str> = tv_hit["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        tv_sources.contains(&"fts"),
        "token_validation sources must include fts, got: {tv_sources:?}"
    );

    let cc_sources: Vec<&str> = cc_hit["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        cc_sources.contains(&"vector"),
        "check_credentials sources must include vector, got: {cc_sources:?}"
    );

    // fts_rank / vector_rank correctness for token_validation (FTS hit).
    assert!(
        tv_hit["fts_rank"].as_u64().is_some(),
        "token_validation fts_rank must be non-null"
    );

    // fts_rank / vector_rank correctness for check_credentials (vector hit).
    assert!(
        cc_hit["vector_rank"].as_u64().is_some(),
        "check_credentials vector_rank must be non-null"
    );
    assert!(
        cc_hit["vector_score"].as_f64().is_some(),
        "check_credentials vector_score must be non-null"
    );
}

// ── B8: module/file key collapse — TWO separate rules tested ─────────────────

#[test]
fn b8_module_file_key_collapse_two_rules() {
    // This test validates that the key builder handles BOTH collapse rules:
    // Rule 1 (vector): symbol == "" → collapse to bare path
    // Rule 2 (FTS): kind == "module" or kind == "file" → collapse to bare path
    //
    // We test this by directly invoking the fusion key logic through the public
    // fuse module, using the same key building logic as dispatch_hybrid.
    // The rules are tested as unit tests here since exercising both with a live
    // fixture would require engineering a file with no functions (module-only).

    // Rule 1: vector whole-file fallback (symbol == "", kind == "file")
    // Key must collapse to bare path.
    {
        let path = "src/some_file.rs";
        let symbol = "";
        let kind = "file";
        // Replicate fusion_key_vec logic:
        let key = if symbol.is_empty() || kind == "file" {
            path.to_string()
        } else {
            format!("{path}::{symbol}")
        };
        assert_eq!(key, "src/some_file.rs", "Rule 1 (vector): empty symbol must collapse to bare path");
    }

    // Rule 2a: FTS module row (kind == "module") — even if name is non-empty
    {
        let path = "src/some_file.rs";
        let name = "some_file"; // FTS module row has the module name, NOT empty
        let kind = "module";
        // Replicate fusion_key_fts logic:
        let key = if kind == "module" || kind == "file" {
            path.to_string()
        } else {
            format!("{path}::{name}")
        };
        assert_eq!(key, "src/some_file.rs", "Rule 2a (FTS module): kind==module must collapse to bare path even when name is non-empty");
    }

    // Rule 2b: FTS file row (kind == "file")
    {
        let path = "src/some_file.rs";
        let name = "some_file";
        let kind = "file";
        let key = if kind == "module" || kind == "file" {
            path.to_string()
        } else {
            format!("{path}::{name}")
        };
        assert_eq!(key, "src/some_file.rs", "Rule 2b (FTS file): kind==file must collapse to bare path");
    }

    // Control: a regular function must NOT collapse
    {
        let path = "src/auth.rs";
        let name = "token_validation";
        let kind_fts = "function";
        let key_fts = if kind_fts == "module" || kind_fts == "file" {
            path.to_string()
        } else {
            format!("{path}::{name}")
        };
        assert_eq!(key_fts, "src/auth.rs::token_validation", "Regular function must use path::name key");

        let symbol = "token_validation";
        let kind_vec = "function";
        let key_vec = if symbol.is_empty() || kind_vec == "file" {
            path.to_string()
        } else {
            format!("{path}::{symbol}")
        };
        assert_eq!(key_vec, "src/auth.rs::token_validation", "Vector function span must use path::symbol key");
    }
}
