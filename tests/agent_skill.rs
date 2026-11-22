// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

use serde_json::{json, Value};
use std::{fs, process::Command};
#[path = "support/install.rs"]
mod install_support;

#[test]
fn reinstall_upgrades_the_previous_bundled_skill() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = home
        .path()
        .join(".config/opencode/skills/lattice/SKILL.md");
    let bundled = include_str!("../skills/lattice/SKILL.md");
    let previous = bundled
        .lines()
        .filter(|line| {
            !line.starts_with("# SPDX-License-Identifier:") && !line.starts_with("# Copyright (c)")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let old_hybrid = previous.replace("'<question>' --limit 5", "'<question>' --hybrid --limit 5");
    assert_ne!(old_hybrid, previous);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    for installed in [&previous, &old_hybrid] {
        fs::write(&path, installed).unwrap();
        let out = install_support::isolated_install(repo.path(), home.path())
            .arg("install")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), bundled);
    }
}

#[test]
fn installs_global_skills_and_reuses_one_model_cache() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    let model = root
        .join(".lattice/models")
        .join(lattice::embed::MODEL_NAME);
    fs::create_dir_all(&model).unwrap();
    for file in lattice::embed::MODEL_FILES {
        fs::write(model.join(file), file).unwrap();
    }
    for args in [&["install"][..], &["install", "--agent", "opencode"][..]] {
        let skill = root.join(".config/opencode/skills/lattice/SKILL.md");
        let command = || {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_lattice"));
            cmd.current_dir(repo.path())
                .env("HOME", home.path())
                .env("XDG_CONFIG_HOME", home.path().join(".config"))
                .env_remove("LATTICE_MODEL_DIR")
                .args(args);
            cmd.output().unwrap()
        };
        for _ in 0..2 {
            let out = command();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let body = std::fs::read_to_string(&skill).expect("agent skill installed");
            assert_eq!(body, include_str!("../skills/lattice/SKILL.md"));
            let result: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(result["agent"], "opencode");
            assert_eq!(result["path"], model.to_str().unwrap());
            assert_eq!(result["cached"], true);
        }
        assert!(!root.join("AGENTS.md").exists());
        assert!(!root.join("mcp.json").exists());
        assert!(!root.join(".config/opencode/opencode.json").exists());
    }
    assert_eq!(fs::read_dir(repo.path()).unwrap().count(), 0);
}

#[test]
fn migrates_legacy_registrations_and_preserves_other_settings() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let scope = home.path();
    let file = scope.join(".config/opencode/opencode.json");
    let md = scope.join(".config/opencode/AGENTS.md");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::create_dir_all(md.parent().unwrap()).unwrap();
    let key = "mcp";
    let entry = json!({"type": "local", "command": ["substrate", "mcp"], "enabled": true});
    let mut expected = json!({ (key): {"other": {"command": "keep-me"}}, "unrelated": 42 });
    let mut old = expected.clone();
    old[key]["substrate"] = entry.clone();
    fs::write(&file, old.to_string()).unwrap();
    fs::write(
        &md,
        "before\n<!-- substrate:begin -->\nold guidance\n<!-- substrate:end -->\nafter\n",
    )
    .unwrap();
    let run = || {
        let mut cmd = install_support::isolated_install(repo.path(), home.path());
        cmd.arg("install");
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run();
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&file).unwrap()).unwrap(),
        expected
    );
    assert_eq!(fs::read_to_string(&md).unwrap(), "before\n\nafter\n");
    let migrated = fs::read(&file).unwrap();
    run();
    assert_eq!(fs::read(&file).unwrap(), migrated);

    expected[key]["substrate"] = entry;
    expected[key]["substrate"]["custom"] = json!(true);
    let custom = expected.to_string();
    fs::write(&file, &custom).unwrap();
    run();
    assert_eq!(fs::read_to_string(&file).unwrap(), custom);
}

#[test]
fn removed_flags_and_invalid_agents_do_not_touch_the_cache() {
    let home = tempfile::tempdir().unwrap();
    for (args, error) in [
        (&["install", "--model"][..], "invalid_flag"),
        (&["install", "--global"][..], "invalid_flag"),
        (&["install", "--agent", "invalid"][..], "unknown_agent"),
        (&["install", "--agent", "claude"][..], "unknown_agent"),
        (&["install", "--agent", "pi"][..], "unknown_agent"),
        (&["hook"][..], "unknown_command"),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_lattice"))
            .env("HOME", home.path())
            .env_remove("LATTICE_MODEL_DIR")
            .args(args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        let result: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(result["error"]["code"], error);
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
    }
}
