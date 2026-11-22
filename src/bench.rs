// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! `lattice bench` — measure naive corpus tokens against the tokens of the
//! envelope a question actually produces.
//!
//! Counts with the already-installed MiniLM WordPiece tokenizer. No new
//! tokenizer, no network. A new `query_type`, so no protocol bump of its own.

use std::path::Path;

use crate::embed::{self, MODEL_NAME};
use crate::envelope::{self, BenchResult};
use crate::freshness;
use crate::index;
use crate::router;
use crate::Result;

/// Bundled question set used when `--questions` is absent. Generic enough to
/// route on any indexed repo; the gate passes its own file.
pub const DEFAULT_QUESTIONS: &[&str] = &[
    "where is retry handled",
    "how is authentication validated",
];

/// One bench run over `root`. `questions` overrides the bundled set.
///
/// Freshness runs once, up front. Each question is then routed with
/// `LATTICE_SKIP_FRESHNESS` set so the per-question dispatch does not
/// re-embed the corpus (bench is a measurement, not an index).
pub fn run(root: &Path, lattice_dir: &Path, questions: &[String]) -> Result<serde_json::Value> {
    let freshness = freshness::ensure_fresh(lattice_dir, root)?;
    let naive = naive_text(root, lattice_dir)?;
    let naive_tokens = embed::count_tokens(&naive).map_err(|e| e.to_string())?;

    let prev = std::env::var("LATTICE_SKIP_FRESHNESS").ok();
    std::env::set_var("LATTICE_SKIP_FRESHNESS", "1");
    let mut results = Vec::with_capacity(questions.len());
    for question in questions {
        let (env, _) = router::route_value(&[question]);
        let rendered = env.to_string();
        let query_tokens = embed::count_tokens(&rendered).map_err(|e| {
            if let Some(v) = &prev {
                std::env::set_var("LATTICE_SKIP_FRESHNESS", v);
            } else {
                std::env::remove_var("LATTICE_SKIP_FRESHNESS");
            }
            e.to_string()
        })?;
        let ratio = if naive_tokens == 0 {
            0.0
        } else {
            query_tokens as f64 / naive_tokens as f64
        };
        results.push(BenchResult {
            question: question.clone(),
            naive_tokens,
            query_tokens,
            ratio,
        });
    }
    if let Some(v) = prev {
        std::env::set_var("LATTICE_SKIP_FRESHNESS", v);
    } else {
        std::env::remove_var("LATTICE_SKIP_FRESHNESS");
    }
    let average_ratio = if results.is_empty() {
        0.0
    } else {
        results.iter().map(|r| r.ratio).sum::<f64>() / results.len() as f64
    };
    Ok(envelope::bench(
        MODEL_NAME,
        &results,
        average_ratio,
        freshness.as_str(),
    ))
}

/// The naive candidate set: every indexed source file, concatenated in path
/// order so the count is stable across runs.
fn naive_text(root: &Path, lattice_dir: &Path) -> Result<String> {
    let extra = crate::languages::load_stored(lattice_dir)
        .map(|reg| reg.extensions())
        .unwrap_or_default();
    let mut files = Vec::new();
    index::collect_source_files(root, root, &extra, &mut files)?;
    let mut parts = Vec::with_capacity(files.len());
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        parts.push(text);
    }
    Ok(parts.join("\n"))
}
