// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

use std::fs;
use std::process::{Command, Output};

use serde_json::Value;
use lattice::embed::{ensure_model_present, MODEL_FILES, MODEL_NAME};

fn successful_stdout(output: Output) -> String {
    assert!(
        output.status.success(),
        "lattice exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("stdout is UTF-8")
}

#[test]
fn cli_works_with_only_lattice_installed() {
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let home = sandbox.path().join("home");
    let root = sandbox.path().join("project");
    let empty_path = sandbox.path().join("bin");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&empty_path).unwrap();

    // Use the normal model cache layout in an otherwise clean home. Never
    // fetch weights or pass the caller's environment to the child process.
    let cached_model =
        ensure_model_present().expect("run `lattice install` before tests");
    let model = home.join(".lattice").join("models").join(MODEL_NAME);
    fs::create_dir_all(&model).unwrap();
    for file in MODEL_FILES {
        fs::copy(cached_model.join(file), model.join(file)).unwrap();
    }

    fs::write(root.join("lib.rs"), "pub fn standalone_entry() {}\n").unwrap();
    let command = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lattice"));
        cmd.current_dir(&root)
            .env_clear()
            .env("HOME", &home)
            .env("PATH", &empty_path);
        cmd
    };

    let init = successful_stdout(command().arg("init").output().unwrap());
    let init: Value = serde_json::from_str(&init).unwrap();
    assert_eq!(init["status"], "ok");
    assert_eq!(init["files_indexed"], 1);

    let impact = successful_stdout(
        command()
            .args(["query", "--impact", "lib.rs"])
            .output()
            .unwrap(),
    );
    let cli: Value = serde_json::from_str(&impact).unwrap();
    assert_eq!(cli["query_type"], "impact");
    assert_eq!(cli["target"], "lib.rs");

    let retired = command().arg("mcp").output().unwrap();
    assert_eq!(retired.status.code(), Some(2));
    let env: Value = serde_json::from_slice(&retired.stdout).unwrap();
    assert_eq!(env["error"]["code"], "unknown_command");
}
