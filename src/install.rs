// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Install the local embedding model or the on-demand CLI skill.

use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use hf_hub::api::sync::ApiBuilder;
use serde_json::{json, Value};

use crate::embed::{model_present, resolve_model_dir, MODEL_FILES, MODEL_NAME};

const HF_REPO: &str = "sentence-transformers/all-MiniLM-L6-v2";

pub enum Installed {
    Ok { path: PathBuf, cached: bool },
    FetchFailed(String),
}

/// Fetch model files only when the local cache is incomplete.
pub fn install_model() -> Installed {
    let dest = resolve_model_dir();
    if model_present(&dest) {
        return Installed::Ok {
            path: dest,
            cached: true,
        };
    }
    if let Err(e) = fs::create_dir_all(&dest) {
        return Installed::FetchFailed(format!("create model cache dir: {e}"));
    }
    let api = match ApiBuilder::new().build() {
        Ok(api) => api,
        Err(e) => return Installed::FetchFailed(format!("build hf-hub api: {e}")),
    };
    let repo = api.model(HF_REPO.to_string());
    for file in MODEL_FILES {
        let src = match repo.get(file) {
            Ok(p) => p,
            Err(e) => {
                return Installed::FetchFailed(format!(
                    "could not download {MODEL_NAME}; check network and retry ({file}: {e})"
                ))
            }
        };
        if let Err(e) = fs::copy(src, dest.join(file)) {
            return Installed::FetchFailed(format!("cache {file} into {}: {e}", dest.display()));
        }
    }
    Installed::Ok {
        path: dest,
        cached: false,
    }
}

const SKILL: &str = include_str!("../skills/lattice/SKILL.md");
const MARKER_BEGIN: &str = "<!-- substrate:begin -->";
const MARKER_END: &str = "<!-- substrate:end -->";

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn install_opencode(home: &Path) -> Result<Vec<String>, String> {
    let scope = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
        .join("opencode");
    Ok(install_skill(home, &scope)?
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect())
}

fn install_skill(home: &Path, scope: &Path) -> Result<Vec<PathBuf>, String> {
    let skill = scope.join("skills/lattice/SKILL.md");
    // Refuse to replace a user-authored skill with the same name.
    if let Some(existing) = read_optional(&skill)? {
        let previous = SKILL.replacen(
            concat!(
                "# SPDX-License-Identifier: Apache-2.0\n",
                "# Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>\n",
            ),
            "",
            1,
        );
        if existing == previous
            || existing
                == previous.replace("'<question>' --limit 5", "'<question>' --hybrid --limit 5")
        {
            write(&skill, SKILL)?;
        } else if existing != SKILL {
            return Err(format!(
                "skill already exists with different content: {}",
                skill.display()
            ));
        }
    } else {
        write(&skill, SKILL)?;
    }
    let mut wrote = vec![skill];

    let instructions = scope.join("AGENTS.md");
    if remove_legacy_block(&instructions)? {
        wrote.push(instructions);
    }
    let legacy_path = home.join(".config/opencode/opencode.json");
    let config_path = scope.join("opencode.json");
    let paths = if config_path == legacy_path {
        vec![config_path]
    } else {
        vec![config_path, legacy_path]
    };
    for path in paths {
        if remove_legacy_server(&path, "mcp", &legacy_opencode())? {
            wrote.push(path);
        }
    }
    Ok(wrote)
}

fn legacy_opencode() -> Value {
    json!({"type": "local", "command": ["substrate", "mcp"], "enabled": true})
}

fn remove_legacy_server(path: &Path, key: &str, expected: &Value) -> Result<bool, String> {
    let Some(text) = read_optional(path)? else {
        return Ok(false);
    };
    // Do not rewrite JSONC or customized server definitions.
    let Ok(mut config) = serde_json::from_str::<Value>(&text) else {
        return Ok(false);
    };
    let Some(servers) = config.get_mut(key).and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    if servers.get("substrate") != Some(expected) {
        return Ok(false);
    }
    servers.remove("substrate");
    write_json(path, &config)?;
    Ok(true)
}

fn remove_legacy_block(path: &Path) -> Result<bool, String> {
    let Some(text) = read_optional(path)? else {
        return Ok(false);
    };
    let Some((before, rest)) = text.split_once(MARKER_BEGIN) else {
        return Ok(false);
    };
    let Some((body, after)) = rest.split_once(MARKER_END) else {
        return Ok(false);
    };
    if body.contains(MARKER_BEGIN) {
        return Ok(false);
    }
    write(path, &format!("{before}{after}"))?;
    Ok(true)
}

fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| format!("serialize {}: {e}", path.display()))?;
    write(path, &format!("{text}\n"))
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))
}
