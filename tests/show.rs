// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

use serde_json::Value;
use std::{fs, path::Path, process::Command};

fn run(root: &Path, args: &[&str], success: bool) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_lattice"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert_eq!(output.status.success(), success, "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn context_id_opens_only_the_selected_declaration() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    let source = "class A { submit() { return 'a'; } }\n\
                  class B { submit() { return 'b'; } }\n";
    fs::write(root.join("checkout.ts"), source).unwrap();
    fs::write(
        root.join("guide.md"),
        "# Start\nOne\n# Finish\nTwo\n# Notes\nFirst\n# Notes\nSecond\n",
    ).unwrap();
    run(root, &["init"], true);
    let context = run(root, &["query", "--context", "submit"], true);
    let hit = context["hits"].as_array().unwrap().iter()
        .find(|hit| hit["id"] == "checkout.ts::A::submit" || hit["id"] == "checkout.ts::B::submit")
        .expect("qualified method hit");
    assert!(hit.get("source").is_none());
    let id = hit["id"].as_str().unwrap();
    let (line, body) = if id == "checkout.ts::A::submit" {
        (1, "submit() { return 'a'; }")
    } else {
        (2, "submit() { return 'b'; }")
    };
    let shown = run(root, &["query", "--show", id], true);
    assert_eq!(shown["query_type"], "show");
    assert_eq!(shown["id"], id);
    assert_eq!(shown["path"], "checkout.ts");
    assert_eq!(shown["start_line"], line);
    assert_eq!(shown["end_line"], line);
    assert_eq!(shown["source"], body);

    let other = if line == 1 { "checkout.ts::B::submit" } else { "checkout.ts::A::submit" };
    assert_eq!(run(root, &["query", "--show", other], true)["start_line"], 3 - line);
    let section = run(root, &["query", "--show", "guide.md::Finish"], true);
    assert_eq!(section["source"], "# Finish\nTwo");
    assert_eq!(section["start_line"], 3);
    assert_eq!(section["end_line"], 4);
    assert_eq!(
        run(root, &["query", "--show", "guide.md::Notes"], false)["error"]["code"],
        "ambiguous_symbol"
    );
    assert_eq!(run(root, &["query", "--show", "submit"], false)["error"]["code"], "not_found");
    assert_eq!(run(root, &["query", "--show", "../checkout.ts"], false)["error"]["code"], "not_found");
    for flags in [&["--kind", "method"][..], &["--file-level"][..]] {
        let mut args = vec!["query", "--show", id];
        args.extend(flags);
        assert_eq!(run(root, &args, false)["error"]["code"], "invalid_flag");
    }

    fs::write(root.join("checkout.ts"), format!("\n{source}")).unwrap();
    let stale = Command::new(env!("CARGO_BIN_EXE_lattice"))
        .current_dir(root)
        .env("LATTICE_SKIP_FRESHNESS", "1")
        .args(["query", "--show", id])
        .output()
        .unwrap();
    assert!(!stale.status.success());
    let stale: Value = serde_json::from_slice(&stale.stdout).unwrap();
    assert_eq!(stale["error"]["code"], "stale_source");
    let refreshed = run(root, &["query", "--show", id], true);
    assert_eq!(refreshed["freshness"], "refreshed");
    assert_eq!(refreshed["start_line"], line + 1);
    assert_eq!(refreshed["source"], body);
}
