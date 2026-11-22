// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Optional `.lattice/languages.toml` registry.
//!
//! Read at `lattice init` only (never on the query-time freshness path — that
//! path loads the registry this module persisted into `meta.db`). Built-in
//! Rust/Python/TypeScript stay the default; entries here add extensions bound
//! to a grammar already compiled into the binary.

use std::cell::RefCell;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::meta;
use crate::Result;

/// Grammars compiled into this binary. An entry naming anything else fails
/// `init` with `unknown_grammar` — never a silent skip.
pub const BUNDLED_GRAMMARS: &[&str] = &["rust", "python", "typescript", "kotlin"];

/// One `[[language]]` table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LanguageEntry {
    /// File extensions without the leading dot (`"ktsub"` matches `*.ktsub`).
    pub extensions: Vec<String>,
    /// A name from [`BUNDLED_GRAMMARS`].
    pub grammar: String,
    /// tree-sitter node types treated as functions.
    #[serde(default)]
    pub functions: Vec<String>,
    /// tree-sitter node types treated as classes / impls (enclosing parent).
    #[serde(default)]
    pub classes: Vec<String>,
    /// tree-sitter node types treated as imports.
    #[serde(default)]
    pub imports: Vec<String>,
    /// tree-sitter node types treated as calls.
    #[serde(default)]
    pub calls: Vec<String>,
}

/// The merged custom-language registry. Empty means "built-ins only".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LanguageRegistry {
    pub languages: Vec<LanguageEntry>,
}

#[derive(Debug)]
pub enum LanguageError {
    /// TOML did not match the expected shape.
    Invalid(String),
    /// `grammar` is not compiled into this binary.
    UnknownGrammar(String),
}

impl std::fmt::Display for LanguageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) => write!(f, "{m}"),
            Self::UnknownGrammar(g) => write!(
                f,
                "unknown grammar {g:?}; bundled grammars: {}",
                BUNDLED_GRAMMARS.join(", ")
            ),
        }
    }
}

impl std::error::Error for LanguageError {}

#[derive(Deserialize)]
struct TomlFile {
    #[serde(default)]
    language: Vec<LanguageEntry>,
}

impl LanguageRegistry {
    /// Extensions (no dot) contributed by custom entries.
    pub fn extensions(&self) -> Vec<String> {
        let mut out = Vec::new();
        for lang in &self.languages {
            for ext in &lang.extensions {
                let ext = ext.trim_start_matches('.').to_string();
                if !ext.is_empty() && !out.contains(&ext) {
                    out.push(ext);
                }
            }
        }
        out
    }

    /// The custom entry that claims `rel_path`'s extension, if any.
    pub fn for_path(&self, rel_path: &str) -> Option<&LanguageEntry> {
        let ext = rel_path.rsplit('.').next().unwrap_or("");
        if ext.is_empty() || ext == rel_path {
            return None;
        }
        self.languages.iter().find(|lang| {
            lang.extensions
                .iter()
                .any(|e| e.trim_start_matches('.') == ext)
        })
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Language label stored on graph nodes for a bundled grammar name.
pub fn language_label(grammar: &str) -> &'static str {
    match grammar {
        "python" => "python",
        "typescript" => "typescript",
        "kotlin" => "kotlin",
        _ => "rust",
    }
}

/// Read `.lattice/languages.toml`. A missing file is an empty registry
/// (byte-identical to today). A present file is validated before return.
pub fn read_file(path: &Path) -> std::result::Result<LanguageRegistry, LanguageError> {
    if !path.is_file() {
        return Ok(LanguageRegistry::default());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| LanguageError::Invalid(format!("read {}: {e}", path.display())))?;
    let parsed: TomlFile = toml::from_str(&text)
        .map_err(|e| LanguageError::Invalid(format!("parse {}: {e}", path.display())))?;
    let registry = LanguageRegistry {
        languages: parsed.language,
    };
    for lang in &registry.languages {
        if !BUNDLED_GRAMMARS.contains(&lang.grammar.as_str()) {
            return Err(LanguageError::UnknownGrammar(lang.grammar.clone()));
        }
        if lang.extensions.is_empty() {
            return Err(LanguageError::Invalid(
                "[[language]] requires a non-empty extensions list".into(),
            ));
        }
    }
    Ok(registry)
}

/// Load the registry persisted by the last `init`. Empty when absent.
pub fn load_stored(lattice_dir: &Path) -> Result<LanguageRegistry> {
    match meta::read_kv(lattice_dir, "language_registry")? {
        Some(json) => Ok(serde_json::from_str(&json).unwrap_or_default()),
        None => Ok(LanguageRegistry::default()),
    }
}

/// Persist the registry so query-time `index::run` can see it without
/// re-reading `languages.toml` (that read is init-only).
pub fn store(lattice_dir: &Path, registry: &LanguageRegistry) -> Result<()> {
    meta::write_kv(lattice_dir, "language_registry", &registry.to_json())
}

thread_local! {
    static ACTIVE: RefCell<Option<LanguageRegistry>> = const { RefCell::new(None) };
}

/// Install `registry` as the process-local active registry for the duration
/// of the guard. `index::run` holds this so `parse_file` / `language_for_path`
/// see custom extensions without threading the registry through every call.
pub struct ActiveGuard;

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE.with(|a| *a.borrow_mut() = None);
    }
}

pub fn activate(registry: &LanguageRegistry) -> ActiveGuard {
    ACTIVE.with(|a| *a.borrow_mut() = Some(registry.clone()));
    ActiveGuard
}

pub fn active() -> Option<LanguageRegistry> {
    ACTIVE.with(|a| a.borrow().clone())
}
