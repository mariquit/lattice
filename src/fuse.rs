// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Reciprocal Rank Fusion (RRF) — pure, deterministic, zero I/O.
//!
//! Fuses N ranked lists of string keys into one sorted list using
//! the standard RRF formula from the 2009 Cormack–Clarke–Buettcher paper
//! (the same formula used in tencentdb's `rrfMerge`).

use std::collections::BTreeMap;

/// Standard RRF constant. Higher k flattens the contribution curve.
/// tencentdb uses the same value: RRF_K = 60.
pub const RRF_K: f64 = 60.0;

/// One fused result. `ranks[i]` is the 1-based rank of this key in input
/// list `i`, or `None` if the key did not appear in that list.
pub struct Fused {
    pub key: String,
    pub score: f64,
    pub ranks: Vec<Option<usize>>,
}

/// Reciprocal Rank Fusion over N ranked lists of keys.
///
/// `score(key) = Σ over lists containing key of 1.0 / (k + rank)`,
/// with rank 1-based.
///
/// Duplicate key *within one list*: keep the first (best) rank.
///
/// Output is sorted: score DESC, then key ASC (deterministic tie-break).
///
/// # Determinism
/// - Accumulation uses `BTreeMap` (ordered, no hash-seed nondeterminism).
/// - `partial_cmp().unwrap_or(Equal)` on f64 scores is safe because all
///   RRF scores are finite positive fractions summed from finite positive
///   terms; NaN is impossible.
pub fn rrf(lists: &[Vec<String>], k: f64) -> Vec<Fused> {
    let n = lists.len();

    // Accumulate: key → (score, per-list ranks)
    // BTreeMap keeps insertion order irrelevant; final sort is explicit.
    let mut map: BTreeMap<String, (f64, Vec<Option<usize>>)> = BTreeMap::new();

    for (list_idx, list) in lists.iter().enumerate() {
        // Track seen keys within this list to enforce first-wins duplicate rule.
        let mut seen_this_list: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();

        for (rank0, key) in list.iter().enumerate() {
            if seen_this_list.contains(key.as_str()) {
                // duplicate within this list — skip, keep the first (best) rank
                continue;
            }
            seen_this_list.insert(key.as_str());

            let rank1 = rank0 + 1; // 1-based
            let contribution = 1.0 / (k + rank1 as f64);

            let entry = map.entry(key.clone()).or_insert_with(|| (0.0, vec![None; n]));
            entry.0 += contribution;
            entry.1[list_idx] = Some(rank1);
        }
    }

    // Convert to Vec<Fused> and sort: score DESC, then key ASC.
    let mut results: Vec<Fused> = map
        .into_iter()
        .map(|(key, (score, ranks))| Fused { key, score, ranks })
        .collect();

    results.sort_by(|a, b| {
        // score DESC
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            // key ASC as deterministic tie-break
            .then_with(|| a.key.cmp(&b.key))
    });

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_a_basic_rrf_order() {
        // design.md hand-computation:
        // L_fts = [a, b, c]   L_vec = [c, a, d]
        // a: 1/61 + 1/62   (rank 1 in fts, rank 2 in vec)
        // c: 1/63 + 1/61   (rank 3 in fts, rank 1 in vec)
        // b: 1/62          (fts only)
        // d: 1/63          (vec only)
        // a > c because 1/62 > 1/63 (both share a 1/61 term)
        let lists = vec![
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
            vec!["c".to_string(), "a".to_string(), "d".to_string()],
        ];
        let result = rrf(&lists, RRF_K);
        let keys: Vec<&str> = result.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, vec!["a", "c", "b", "d"]);

        // Verify exact scores using identical IEEE-754 arithmetic expressions.
        let a_score = 1.0 / (RRF_K + 1.0) + 1.0 / (RRF_K + 2.0);
        let c_score = 1.0 / (RRF_K + 3.0) + 1.0 / (RRF_K + 1.0);
        let b_score = 1.0 / (RRF_K + 2.0);
        let d_score = 1.0 / (RRF_K + 3.0);
        assert_eq!(result[0].score, a_score);
        assert_eq!(result[1].score, c_score);
        assert_eq!(result[2].score, b_score);
        assert_eq!(result[3].score, d_score);

        // ranks bookkeeping for 'a': [Some(1), Some(2)]
        assert_eq!(result[0].ranks, vec![Some(1), Some(2)]);
    }

    #[test]
    fn part_a_tie_break_key_asc() {
        // [["x"], ["y"]] → both score 1/61 → key ASC → [x, y]
        let lists = vec![
            vec!["x".to_string()],
            vec!["y".to_string()],
        ];
        let result = rrf(&lists, RRF_K);
        let keys: Vec<&str> = result.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, vec!["x", "y"]);
        // both have same score
        assert_eq!(result[0].score, result[1].score);
    }

    #[test]
    fn part_a_duplicate_within_list() {
        // [["a", "a", "b"], []] → a scores 1/61 once (first-wins), order [a, b]
        let lists = vec![
            vec!["a".to_string(), "a".to_string(), "b".to_string()],
            vec![],
        ];
        let result = rrf(&lists, RRF_K);
        let keys: Vec<&str> = result.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, vec!["a", "b"]);
        // a at rank 1 only (duplicate ignored)
        assert_eq!(result[0].score, 1.0 / (RRF_K + 1.0));
        assert_eq!(result[0].ranks, vec![Some(1), None]);
    }

    #[test]
    fn part_a_empty_list_one_side() {
        // [[], ["p", "q"]] → pure vector ordering survives ([p, q])
        let lists = vec![
            vec![],
            vec!["p".to_string(), "q".to_string()],
        ];
        let result = rrf(&lists, RRF_K);
        let keys: Vec<&str> = result.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, vec!["p", "q"]);
    }
}
