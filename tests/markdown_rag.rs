// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate: Markdown must be indexed into the RAG (vectors + FTS) and must stay
//! OUT of the call graph.
//!
//! Before this slice, `index::collect_source_files` allowed only `.rs` / `.py` /
//! `.ts` / `.tsx`, so all Markdown was invisible to semantic search — a repo's
//! design docs and operating manuals could not be retrieved at all.
//!
//! Scenarios:
//!   1. Section granularity: headings become separate embedding units.
//!   2. A `#` inside a fenced code block does not create a section.
//!   3. A heading-less document still yields exactly one section.
//!   4. Markdown-only prose is findable via free-text semantic search.
//!   5. Markdown headings are findable via `query --symbol` (FTS5).
//!   6. Gitignored Markdown is excluded from every store.
//!   7. Markdown contributes ZERO functions/graph nodes (no `.md` in the graph).
//!   8. Markdown is never parsed by the Rust tree-sitter grammar.
//!   9. Two inits produce byte-identical retrieval output (determinism).

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

fn run_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn lattice binary")
}

/// Last stdout line as JSON (init prints one envelope; queries print one).
fn parse_json(output: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let last = stdout.lines().last().unwrap_or("");
    serde_json::from_str(last)
        .unwrap_or_else(|e| panic!("stdout not JSON ({e}): {stdout:?}"))
}

fn write(root: &Path, rel: &str, body: &str) {
    let abs = root.join(rel);
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(abs, body).expect("write fixture file");
}

/// Build the shared fixture: 1 code file + 3 markdown files + 1 gitignored md.
///
/// Ground truth (hand-counted):
///   code.rs   → 2 functions (alpha, beta), 1 call edge
///   guide.md  → 3 sections: preamble? no — starts with `#`, so:
///                 "Zebra Architecture", "Quokka Scheduling", "Narwhal Persistence"
///   flat.md   → 1 section (no headings → whole file, titled "flat")
///   fenced.md → 1 section ("Fenced Sample"); the `#` inside ``` is not a heading
///   secret.md → gitignored, 0 sections
/// Total markdown sections = 3 + 1 + 1 = 5
fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("temp dir");
    let root = tmp.path();

    write(root, ".gitignore", "secret.md\n");

    write(root, "code.rs", "pub fn alpha() {\n    beta();\n}\n\npub fn beta() {}\n");

    write(
        root,
        "guide.md",
        "# Zebra Architecture\n\
         \n\
         The zebra subsystem coordinates quokka scheduling across shards.\n\
         \n\
         ## Quokka Scheduling\n\
         \n\
         Quokka scheduling uses a deterministic ratchet to order work items.\n\
         \n\
         ## Narwhal Persistence\n\
         \n\
         Narwhal persistence writes checkpoints to durable storage.\n",
    );

    write(
        root,
        "flat.md",
        "This document has no headings at all.\n\
         \n\
         It exists to prove the whole-file fallback works for plain prose.\n",
    );

    write(
        root,
        "fenced.md",
        "# Fenced Sample\n\
         \n\
         ```sh\n\
         # this is a comment, not a heading\n\
         echo hello\n\
         ```\n\
         \n\
         Trailing prose after the fence.\n",
    );

    write(root, "secret.md", "# Secret Wombat\n\nMust never be indexed.\n");

    let out = run_in(root, &["init"]);
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    tmp
}

/// All FTS symbols of kind "section", as (path, heading).
///
/// FTS5 tokenizes on word boundaries, so there is no single term that matches
/// every row. Instead union several `--symbol` probes covering each distinct
/// heading word in the fixture, then dedup — this exercises the real query path
/// rather than reading SQLite directly.
fn sections(root: &Path) -> Vec<(String, String)> {
    let probes = [
        "Zebra", "Quokka", "Narwhal", "flat", "Fenced", "Sample",
        "Architecture", "Scheduling", "Persistence", "Secret", "Wombat",
    ];
    let mut out: Vec<(String, String)> = Vec::new();
    for probe in probes {
        let res = run_in(root, &["query", "--symbol", probe, "--limit", "200"]);
        let env = parse_json(&res);
        if let Some(hits) = env["hits"].as_array() {
            for h in hits {
                if h["kind"].as_str() != Some("section") {
                    continue;
                }
                let row = (
                    h["path"].as_str().unwrap_or_default().to_string(),
                    h["name"].as_str().unwrap_or_default().to_string(),
                );
                if !out.contains(&row) {
                    out.push(row);
                }
            }
        }
    }
    out
}

#[test]
fn markdown_files_are_indexed() {
    let tmp = fixture();
    let env = parse_json(&run_in(tmp.path(), &["init"]));
    // 1 code + 3 markdown = 4 (secret.md is gitignored).
    assert_eq!(
        env["files_indexed"], 4,
        "code.rs + guide.md + flat.md + fenced.md; secret.md gitignored"
    );
}

#[test]
fn headings_become_separate_sections() {
    let tmp = fixture();
    let secs = sections(tmp.path());

    let guide: Vec<&String> = secs
        .iter()
        .filter(|(p, _)| p == "guide.md")
        .map(|(_, h)| h)
        .collect();
    assert_eq!(guide.len(), 3, "guide.md must yield 3 heading sections, got {guide:?}");
    assert!(guide.iter().any(|h| *h == "Zebra Architecture"));
    assert!(guide.iter().any(|h| *h == "Quokka Scheduling"));
    assert!(guide.iter().any(|h| *h == "Narwhal Persistence"));
}

#[test]
fn fenced_hash_is_not_a_heading() {
    let tmp = fixture();
    let secs = sections(tmp.path());

    let fenced: Vec<&String> = secs
        .iter()
        .filter(|(p, _)| p == "fenced.md")
        .map(|(_, h)| h)
        .collect();
    assert_eq!(fenced.len(), 1, "fenced.md must yield exactly 1 section, got {fenced:?}");
    assert_eq!(fenced[0], "Fenced Sample");
    assert!(
        !fenced.iter().any(|h| h.contains("comment")),
        "the # inside the code fence must not become a section"
    );
}

#[test]
fn heading_less_file_is_one_section() {
    let tmp = fixture();
    let secs = sections(tmp.path());

    let flat: Vec<&String> = secs
        .iter()
        .filter(|(p, _)| p == "flat.md")
        .map(|(_, h)| h)
        .collect();
    assert_eq!(flat.len(), 1, "flat.md must yield 1 whole-file section, got {flat:?}");
    assert_eq!(flat[0], "flat", "titled with the file stem");
}

#[test]
fn markdown_is_semantically_searchable() {
    let tmp = fixture();
    // "quokka" appears ONLY in guide.md — impossible to satisfy from code.
    let out = run_in(tmp.path(), &["query", "quokka scheduling deterministic ratchet"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);

    let hits = env["hits"].as_array().expect("hits array");
    assert!(!hits.is_empty(), "semantic search returned nothing");
    assert_eq!(
        hits[0]["path"].as_str(),
        Some("guide.md"),
        "markdown-only prose must be the top hit; got {hits:?}"
    );
}

#[test]
fn markdown_heading_findable_via_fts() {
    let tmp = fixture();
    let out = run_in(tmp.path(), &["query", "--symbol", "Narwhal"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);

    let hits = env["hits"].as_array().expect("hits array");
    assert!(
        hits.iter().any(|h| h["path"].as_str() == Some("guide.md")
            && h["kind"].as_str() == Some("section")),
        "heading must be findable via --symbol; got {hits:?}"
    );
}

#[test]
fn gitignored_markdown_is_excluded() {
    let tmp = fixture();

    // Not in FTS.
    let secs = sections(tmp.path());
    assert!(
        !secs.iter().any(|(p, _)| p == "secret.md"),
        "gitignored markdown must not appear in FTS: {secs:?}"
    );

    // Not retrievable semantically either.
    let out = run_in(tmp.path(), &["query", "secret wombat"]);
    let env = parse_json(&out);
    if let Some(hits) = env["hits"].as_array() {
        assert!(
            !hits.iter().any(|h| h["path"].as_str() == Some("secret.md")),
            "gitignored markdown must not be embedded: {hits:?}"
        );
    }
}

#[test]
fn markdown_absent_from_call_graph() {
    let tmp = fixture();
    let out = run_in(tmp.path(), &["query", "--health"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let env = parse_json(&out);

    // Only code.rs contributes functions: alpha + beta.
    assert_eq!(
        env["functions"], 2,
        "markdown must contribute zero functions to the graph"
    );
    assert_eq!(
        env["call_edges"], 1,
        "only alpha->beta; markdown adds no call edges"
    );
}

#[test]
fn markdown_never_parsed_as_rust() {
    let tmp = fixture();
    // If guide.md were fed to the Rust grammar it would emit bogus function or
    // method symbols. Probe each heading word and assert every .md hit is a
    // section — never a function/method/module.
    let probes = ["Zebra", "Quokka", "Narwhal", "flat", "Fenced", "Architecture"];
    let mut bogus: Vec<Value> = Vec::new();
    for probe in probes {
        let env = parse_json(&run_in(tmp.path(), &["query", "--symbol", probe, "--limit", "200"]));
        if let Some(hits) = env["hits"].as_array() {
            for h in hits {
                if h["path"].as_str().is_some_and(|p| p.ends_with(".md"))
                    && h["kind"].as_str() != Some("section")
                {
                    bogus.push(h.clone());
                }
            }
        }
    }
    assert!(
        bogus.is_empty(),
        "markdown produced non-section symbols (parsed as code?): {bogus:?}"
    );

    // Positive control: the probes DO find sections, so the assertion above is
    // not passing vacuously.
    let secs = sections(tmp.path());
    assert!(
        secs.iter().any(|(p, _)| p == "guide.md"),
        "probe set must actually match markdown sections"
    );
}

#[test]
fn determinism_two_inits_identical() {
    let tmp = fixture();
    let root = tmp.path();

    let a = run_in(root, &["query", "quokka scheduling deterministic ratchet"]);
    // Re-init (no source change) then repeat the same query.
    run_in(root, &["init"]);
    let b = run_in(root, &["query", "quokka scheduling deterministic ratchet"]);

    assert_eq!(
        String::from_utf8_lossy(&a.stdout),
        String::from_utf8_lossy(&b.stdout),
        "identical query across re-init must be byte-identical"
    );
}
