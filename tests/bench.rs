// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Gate for `lattice bench`.
//!
//! `cargo test --test bench`
//!
//! The fixture corpus is long enough that a two-question routed envelope is
//! smaller than the concatenated source. Ratios are the integer division the
//! envelope itself computes (`query_tokens as f64 / naive_tokens as f64`).

use std::fs;
use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_lattice");

fn run_in(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN).args(args).current_dir(dir).output().expect("spawn")
}

fn parse_json(output: &std::process::Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "parse: {e}\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn bench_reports_hand_checked_ratios() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // A few kilobytes of source so naive_tokens exceeds either routed envelope.
    // Long enough that the concatenated corpus out-tokens either routed
    // envelope, short enough that init's embed stays inside the gate budget.
    let body = "pub fn pad() { let _ = 1; }\n".repeat(80);
    fs::write(root.join("a.rs"), &body).unwrap();
    fs::write(root.join("b.rs"), &body).unwrap();
    let init = run_in(root, &["init"]);
    assert!(
        init.status.success(),
        "init: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let questions = root.join("questions.txt");
    fs::write(&questions, "where is retry handled\nhow is auth validated\n").unwrap();

    let out = run_in(root, &["bench", "--questions", questions.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "bench: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_json(&out);
    assert_eq!(env["query_type"], "bench");
    assert_eq!(env["model"], "all-MiniLM-L6-v2");

    let results = env["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    let mut ratios = Vec::new();
    for row in results {
        let naive = row["naive_tokens"].as_u64().unwrap();
        let query = row["query_tokens"].as_u64().unwrap();
        assert!(query < naive, "query_tokens {query} < naive_tokens {naive}");
        let ratio = row["ratio"].as_f64().unwrap();
        let expected = query as f64 / naive as f64;
        assert_eq!(ratio, expected);
        ratios.push(ratio);
    }
    let mean = (ratios[0] + ratios[1]) / 2.0;
    assert_eq!(env["average_ratio"].as_f64().unwrap(), mean);

    let again = run_in(root, &["bench", "--questions", questions.to_str().unwrap()]);
    assert_eq!(out.stdout, again.stdout, "bench stdout must be byte-identical");
}
