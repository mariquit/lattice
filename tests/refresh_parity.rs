// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

use lattice::{graph, index, ingest::CallSite};
use serde_json::Value;
use std::{fs, path::Path, process::Command};

const ENTRY: &str = "import { Service } from './service';\n\
class Checkout {\n\
    constructor(private svc: Service) {}\n\
    run() {\n\
        this.svc.submit();\n\
    }\n\
}\n";
const SERVICE: &str = "export class Service { submit() {} }\n";

fn reindex(root: &Path, store: &Path) -> index::IndexReport {
    index::run(root, store).unwrap_or_else(|e| match e {
        index::IndexError::ModelNotFound(message) => panic!("{message}"),
        index::IndexError::Internal(error) => panic!("{error}"),
    })
}

fn symbols(root: &Path, query: &str) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_lattice"))
        .current_dir(root)
        .args(["query", "--symbol", query])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let env: Value = serde_json::from_slice(&out.stdout).unwrap();
    env["hits"].clone()
}

fn assert_parity(root: &Path, stage: &str, target: Option<&str>) {
    let clean = tempfile::tempdir().unwrap();
    for file in ["checkout.ts", "service.ts", "service.tsx"] {
        if root.join(file).is_file() {
            fs::copy(root.join(file), clean.path().join(file)).unwrap();
        }
    }
    let live_dir = root.join(".lattice");
    let clean_dir = clean.path().join(".lattice");
    reindex(clean.path(), &clean_dir);
    let live = graph::load_call_graph(&live_dir).unwrap();
    let rebuilt = graph::load_call_graph(&clean_dir).unwrap();
    // CallGraph maps and adjacency sets use IDs, not database row order.
    assert_eq!(live.out, rebuilt.out, "call edges after {stage}");
    assert_eq!(live.inn, rebuilt.inn, "reverse call edges after {stage}");
    assert_eq!(live.sites, rebuilt.sites, "call-site spans after {stage}");
    assert_eq!(live.evidence, rebuilt.evidence, "call evidence after {stage}");
    assert_eq!(graph::read(&live_dir).unwrap().imports, graph::read(&clean_dir).unwrap().imports,
        "import edges after {stage}");
    for name in ["run", "submit", "service"] {
        assert_eq!(symbols(root, name), symbols(clean.path(), name), "{name} search after {stage}");
    }

    let from = "checkout.ts::Checkout::run";
    if let Some(file) = target {
        let to = format!("{file}::Service::submit");
        assert_eq!(live.out[from].iter().collect::<Vec<_>>(), [&to]);
        assert_eq!(live.sites.len(), 1);
        assert_eq!(live.sites[&(from.to_owned(), to)], CallSite {
            path: "checkout.ts".to_owned(), start_line: 5, end_line: 5,
        });
        assert_eq!(symbols(root, "submit")[0]["path"], file);
    } else {
        assert!(live.out.is_empty());
        assert!(live.sites.is_empty());
        assert_eq!(symbols(root, "submit"), serde_json::json!([]));
    }
}

#[test]
fn refresh_matches_a_clean_build_after_add_rename_and_delete() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    let store = root.join(".lattice");
    fs::write(root.join("checkout.ts"), ENTRY).unwrap();
    reindex(root, &store);
    assert!(graph::load_call_graph(&store).unwrap().out.is_empty());

    fs::write(root.join("service.ts"), SERVICE).unwrap();
    assert_eq!(reindex(root, &store).files_parsed, 1);
    assert_parity(root, "add", Some("service.ts"));

    fs::rename(root.join("service.ts"), root.join("service.tsx")).unwrap();
    assert_eq!(reindex(root, &store).files_parsed, 1);
    assert_parity(root, "rename", Some("service.tsx"));

    fs::remove_file(root.join("service.tsx")).unwrap();
    assert_eq!(reindex(root, &store).files_parsed, 0);
    assert_parity(root, "delete", None);
}
