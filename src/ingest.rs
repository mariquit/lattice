// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Syntactic ingestion via tree-sitter (no language servers, no toolchains).
//!
//! Languages: Rust, Python, and TypeScript/TSX. Parsing produces per-file
//! [`FileFacts`];
//! [`resolve`] then performs heuristic name+scope resolution across the whole
//! file set to yield the graph:
//! nodes = File / Module / Function, edges = `contains`, `calls`, `imports`.

use std::collections::BTreeMap;
use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::Result;

// ── Facts extracted per file (before cross-file resolution) ─────────────────

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct FnDecl {
    pub name: String,
    pub start_line: usize, // 1-based
    /// Last line of the function body, 1-based inclusive.
    /// `node.end_position().row + 1` of the whole function node.
    /// `#[serde(default)]` so stale cached fragments deserialise with 0
    /// instead of erroring; a schema-version bump forces re-parse in practice.
    #[serde(default)]
    pub end_line: usize,
    /// Exact declaration bytes in the UTF-8 text passed to Tree-sitter.
    #[serde(default)]
    pub byte_range: std::ops::Range<usize>,
    /// Enclosing class / impl type. `None` = free function.
    #[serde(default)]
    pub parent: Option<String>,
}

/// How a `calls` edge was bound. Ordered weakest → strongest.
///
/// `name` is a unique bare-name match with no import or same-file definition.
/// `binding` is a same-file definition or an import/qualifier resolution.
/// An ambiguous name (more than one candidate, in-file or cross-file) produces
/// no edge. There is no `type` or `indexer` level: resolution stays syntactic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallEvidence {
    Name,
    Binding,
}

impl CallEvidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Binding => "binding",
        }
    }

    /// Rank used by `--min-evidence`. Higher keeps the edge.
    pub fn rank(self) -> u8 {
        match self {
            Self::Name => 0,
            Self::Binding => 1,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "name" => Some(Self::Name),
            "binding" => Some(Self::Binding),
            _ => None,
        }
    }

    pub fn meets(self, min: Self) -> bool {
        self.rank() >= min.rank()
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct CallRef {
    /// Name of the enclosing function making the call.
    pub caller_fn: String,
    /// Callee function name (last path segment).
    pub callee_name: String,
    /// Module qualifier, e.g. `auth` for `auth::validate_token(..)` or
    /// `crate::auth::validate_token(..)`; `None` for bare `f(..)` calls.
    pub qualifier: Option<String>,
    /// 1-based line of the call expression. Surfaced only on `--context`
    /// graph hits; absent from callers/callees/impact envelopes.
    /// `#[serde(default)]` so a stale fragment cache still loads.
    #[serde(default)]
    pub start_line: usize,
    /// 1-based inclusive end line of the call expression.
    #[serde(default)]
    pub end_line: usize,
    #[serde(default)]
    ts_receiver: Option<TsReceiver>,
}

impl CallRef {
    fn new(
        caller_fn: &str,
        callee_name: String,
        qualifier: Option<String>,
        node: Node,
    ) -> Self {
        Self {
            caller_fn: caller_fn.to_string(),
            callee_name,
            qualifier,
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            ts_receiver: None,
        }
    }
}

/// A re-export declaration: `export * from "./parse"` or `export { A } from "./parse"`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ReExport {
    /// Source specifier (e.g. `"./parse"`).
    pub module: String,
    /// `None` = `export * from`; `Some(names)` = named re-exports.
    pub names: Option<Vec<String>>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct FileFacts {
    /// Repo-relative, `/`-separated path.
    pub path: String,
    /// Not persisted in the fragment cache: `language` is a pure function of
    /// the path extension (`language_for_path`), so it is reconstructed on
    /// load rather than stored. This keeps the cached JSON free of a
    /// `&'static str` that serde cannot round-trip.
    #[serde(skip, default = "default_language")]
    pub language: &'static str,
    pub functions: Vec<FnDecl>,
    pub calls: Vec<CallRef>,
    /// Module names referenced by `mod x;` declarations (resolve relative to
    /// this file's directory).
    pub mod_decls: Vec<String>,
    /// Module names referenced by `use` paths (Rust) or `import x` /
    /// `from x import y` statements (Python).
    pub use_decls: Vec<String>,
    /// Python `from x import y`: (module, imported name). Lets bare `y(..)`
    /// calls resolve to `x`'s file. Empty for Rust.
    pub from_imports: Vec<(String, String)>,
    /// TypeScript / Python namespace imports: (local qualifier, specifier).
    /// e.g. `import * as utils from "./utils"` → `("utils", "./utils")`.
    #[serde(default)]
    pub ns_imports: Vec<(String, String)>,
    /// TypeScript export-from / Python __init__ re-exports.
    #[serde(default)]
    pub reexports: Vec<ReExport>,
    #[serde(default)]
    ts_classes: Vec<TsClass>,
    #[serde(default)]
    ts_named_imports: Vec<TsNamedImport>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct TsReceiver {
    caller_class: String,
    field: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct TsClass {
    name: String,
    exported: bool,
    fields: Vec<(String, Option<String>)>,
    methods: Vec<(String, bool)>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct TsNamedImport {
    module: String,
    exported: String,
    local: String,
}

// ── Resolved graph (what gets persisted to SparrowDB) ────────────────────────

#[derive(Debug, Default)]
pub struct Graph {
    /// (path, language)
    pub files: Vec<(String, String)>,
    /// (id = "<path>::<name>", name, path, start_line, parent)
    pub functions: Vec<(String, String, String, usize, Option<String>)>,
    /// (id, name, path) — Rust MVP: one file-level module per file.
    pub modules: Vec<(String, String, String)>,
    /// (from Function id, to Function id, evidence, call-site span).
    /// Span is `(path, start_line, end_line)` of the call expression; lines
    /// are 0 when the edge came from a cache written before spans existed.
    pub calls: Vec<(String, String, CallEvidence, CallSite)>,
    /// (from File path, to File path)
    pub imports: Vec<(String, String)>,
}

/// Where a resolved `calls` edge was written in source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallSite {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
}

// ── Language dispatch ────────────────────────────────────────────────────────

fn default_language() -> &'static str {
    "rust"
}

/// The language label for a repo-relative path, decided purely by extension —
/// the single source of truth shared by parsing and by fragment-cache reload.
pub(crate) fn is_builtin_ext(rel_path: &str) -> bool {
    rel_path.ends_with(".rs")
        || rel_path.ends_with(".py")
        || rel_path.ends_with(".ts")
        || rel_path.ends_with(".tsx")
        || rel_path.ends_with(".md")
        || rel_path.ends_with(".markdown")
}

pub fn language_for_path(rel_path: &str) -> &'static str {
    if let Some(reg) = crate::languages::active() {
        if let Some(entry) = reg.for_path(rel_path) {
            if !is_builtin_ext(rel_path) {
                return crate::languages::language_label(&entry.grammar);
            }
        }
    }
    if rel_path.ends_with(".py") {
        "python"
    } else if rel_path.ends_with(".ts") || rel_path.ends_with(".tsx") {
        "typescript"
    } else if rel_path.ends_with(".md") || rel_path.ends_with(".markdown") {
        // Markdown is embedded for retrieval but has NO tree-sitter grammar and
        // contributes nothing to the graph. This arm MUST precede the `rust`
        // fallback below: without it every .md file would be fed to the Rust
        // parser and silently produce garbage facts.
        "markdown"
    } else {
        "rust"
    }
}

/// Parse a source file into [`FileFacts`], dispatching on [`language_for_path`].
pub fn parse_file(rel_path: &str, source: &str) -> Result<FileFacts> {
    if let Some(reg) = crate::languages::active() {
        if let Some(entry) = reg.for_path(rel_path).cloned() {
            if !is_builtin_ext(rel_path) {
                return parse_custom(rel_path, source, &entry);
            }
        }
    }
    match language_for_path(rel_path) {
        "python" => parse_python_file(rel_path, source),
        "typescript" => parse_typescript_file(rel_path, source),
        // Markdown yields empty-but-valid facts: no functions, no calls, no
        // module decls. This keeps .md out of the call graph entirely while
        // still letting the file flow through the embedding pipeline, where
        // `markdown::parse_sections` supplies the spans.
        "markdown" => Ok(empty_facts(rel_path, "markdown")),
        _ => parse_rust_file(rel_path, source),
    }
}

/// Build a facts record with no symbols, for languages that are indexed for
/// retrieval only (currently Markdown).
fn load_bundled_grammar(name: &str) -> Result<tree_sitter::Language> {
    match name {
        "rust" => Ok(tree_sitter_rust::language()),
        "python" => Ok(tree_sitter_python::language()),
        "typescript" => Ok(tree_sitter_typescript::language_typescript()),
        "kotlin" => Ok(tree_sitter_kotlin::language()),
        other => Err(format!("unknown grammar {other:?}").into()),
    }
}

/// Table-driven extractor for a custom extension bound to a bundled grammar.
/// The extractor keys on the node types in the registry entry; resolution of
/// the resulting [`CallRef`]s is otherwise the existing same-file path.
fn parse_custom(rel_path: &str, source: &str, entry: &crate::languages::LanguageEntry) -> Result<FileFacts> {
    let grammar = load_bundled_grammar(&entry.grammar)?;
    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .map_err(|e| format!("load {} grammar: {e}", entry.grammar))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| format!("tree-sitter failed to parse {rel_path}"))?;
    let mut facts = empty_facts(rel_path, crate::languages::language_label(&entry.grammar));
    walk_custom(tree.root_node(), source.as_bytes(), entry, None, &mut facts);
    Ok(facts)
}

fn node_name(node: Node, src: &[u8]) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        let text = text(name, src);
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // Rust/Python/TS use `identifier`. Kotlin uses `simple_identifier`.
        if child.kind() == "identifier" || child.kind() == "simple_identifier" {
            let t = text(child, src);
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn call_target(node: Node, src: &[u8]) -> Option<(String, Option<String>)> {
    let func = node.child_by_field_name("function").unwrap_or(node);
    if func.kind() == "identifier" || func.kind() == "simple_identifier" {
        let name = text(func, src);
        if name.is_empty() {
            return None;
        }
        return Some((name.to_string(), None));
    }
    if let Some(name_node) = func.child_by_field_name("name") {
        let name = text(name_node, src);
        if name.is_empty() {
            return None;
        }
        let qual = func
            .child_by_field_name("path")
            .map(|p| text(p, src).to_string())
            .filter(|s| !s.is_empty());
        return Some((name.to_string(), qual));
    }
    node_name(func, src).map(|n| (n, None))
}

fn walk_custom(
    node: Node,
    src: &[u8],
    entry: &crate::languages::LanguageEntry,
    enclosing: Option<&str>,
    facts: &mut FileFacts,
) {
    let kind = node.kind();
    let mut next = enclosing.map(str::to_string);
    if entry.functions.iter().any(|k| k == kind) {
        if let Some(name) = node_name(node, src) {
            facts.functions.push(FnDecl {
                name: name.clone(),
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                byte_range: node.byte_range(),
                parent: None,
            });
            next = Some(name);
        }
    }
    if entry.calls.iter().any(|k| k == kind) {
        if let Some(caller) = enclosing {
            if let Some((callee, qualifier)) = call_target(node, src) {
                facts.calls.push(CallRef::new(caller, callee, qualifier, node));
            }
        }
    }
    if entry.imports.iter().any(|k| k == kind) {
        if let Some(name) = node_name(node, src) {
            facts.use_decls.push(name);
        }
    }
    let mut cursor = node.walk();
    let children: Vec<Node> = node.children(&mut cursor).collect();
    for child in children {
        walk_custom(child, src, entry, next.as_deref(), facts);
    }
}

fn empty_facts(rel_path: &str, language: &'static str) -> FileFacts {
    FileFacts {
        path: rel_path.to_string(),
        language,
        functions: Vec::new(),
        calls: Vec::new(),
        mod_decls: Vec::new(),
        use_decls: Vec::new(),
        from_imports: Vec::new(),
        ns_imports: Vec::new(),
        reexports: Vec::new(),
        ts_classes: Vec::new(),
        ts_named_imports: Vec::new(),
    }
}

// ── Rust parsing ─────────────────────────────────────────────────────────────

pub fn parse_rust_file(rel_path: &str, source: &str) -> Result<FileFacts> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::language())
        .map_err(|e| format!("load Rust grammar: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| format!("tree-sitter failed to parse {rel_path}"))?;

    let mut facts = empty_facts(rel_path, "rust");
    walk(tree.root_node(), source.as_bytes(), None, None, &mut facts);
    Ok(facts)
}

fn text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

/// Recursive walk tracking the enclosing function name and enclosing impl/class type.
fn walk(node: Node, src: &[u8], enclosing_fn: Option<&str>, enclosing_type: Option<&str>, facts: &mut FileFacts) {
    let mut next_enclosing = enclosing_fn.map(str::to_string);
    let mut next_type = enclosing_type.map(str::to_string);

    match node.kind() {
        "impl_item" => {
            // Track the type name for method qualification.
            if let Some(type_node) = node.child_by_field_name("type") {
                let type_name = text(type_node, src).to_string();
                next_type = Some(type_name);
            }
        }
        "function_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = text(name_node, src).to_string();
                let parent = enclosing_type.map(str::to_string);
                facts.functions.push(FnDecl {
                    name: name.clone(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    byte_range: node.byte_range(),
                    parent,
                });
                next_enclosing = Some(name);
            }
        }
        "mod_item" => {
            // `mod x;` (no body) declares a child module living in another file.
            if node.child_by_field_name("body").is_none() {
                if let Some(name_node) = node.child_by_field_name("name") {
                    facts.mod_decls.push(text(name_node, src).to_string());
                }
            }
        }
        "use_declaration" => {
            if let Some(arg) = node.child_by_field_name("argument") {
                if let Some(module) = first_module_segment(arg, src) {
                    facts.use_decls.push(module);
                }
            }
        }
        "call_expression" => {
            if let Some(caller) = enclosing_fn {
                if let Some(func) = node.child_by_field_name("function") {
                    match func.kind() {
                        "identifier" => facts.calls.push(CallRef::new(
                            caller,
                            text(func, src).to_string(),
                            None,
                            node,
                        )),
                        "scoped_identifier" => {
                            let callee = func
                                .child_by_field_name("name")
                                .map(|n| text(n, src).to_string());
                            let qualifier = func
                                .child_by_field_name("path")
                                .and_then(|p| last_module_segment(p, src));
                            if let Some(callee_name) = callee {
                                facts.calls.push(CallRef::new(
                                    caller,
                                    callee_name,
                                    qualifier,
                                    node,
                                ));
                            }
                        }
                        // Method calls (field_expression) need type info we do
                        // not have syntactically; skip (heuristic ingestion).
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, src, next_enclosing.as_deref(), next_type.as_deref(), facts);
    }
}

/// First path segment that is a real module name (skips `crate`/`self`/`super`).
/// For `crate::auth::validate_token` this is `auth`.
fn first_module_segment(node: Node, src: &[u8]) -> Option<String> {
    path_segments(node, src)
        .into_iter()
        .find(|s| !matches!(s.as_str(), "crate" | "self" | "super"))
}

/// Last real module segment of a scoped path (the module owning the item).
/// For `crate::auth` -> `auth`; for `auth` -> `auth`.
fn last_module_segment(node: Node, src: &[u8]) -> Option<String> {
    path_segments(node, src)
        .into_iter()
        .filter(|s| !matches!(s.as_str(), "crate" | "self" | "super"))
        .next_back()
}

/// Flatten a (possibly nested) path node into its identifier segments, in order.
fn path_segments(node: Node, src: &[u8]) -> Vec<String> {
    match node.kind() {
        "identifier" | "crate" | "self" | "super" => vec![text(node, src).to_string()],
        _ => {
            let mut segs = Vec::new();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() != "::" {
                    segs.extend(path_segments(child, src));
                }
            }
            segs
        }
    }
}

/// Lexically normalize a path: resolve `.` and `..` segments without touching
/// the filesystem, so `foo/./bar` -> `foo/bar` and `foo/../bar` -> `bar`.
/// Used to turn a relative TS import specifier into a repo-relative path.
fn normalize_path(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(seg) => out.push(seg.to_os_string()),
            other => out.push(other.as_os_str().to_os_string()),
        }
    }
    out.iter().collect()
}

// ── JSONC strip ──────────────────────────────────────────────────────────────

/// Strip `//`-style line comments and trailing commas from a JSONC string,
/// producing valid JSON. String-aware: slashes and commas inside string
/// literals are not touched. Malformed surrogate or non-UTF-8 input returns
/// the original string so serde's error carries the real content.
///
/// Trailing-comma removal handles two patterns:
///   `{ "a": 1, }` → `{ "a": 1 }`
///   `[ 1, 2, ]` → `[ 1, 2 ]`
pub fn strip_jsonc(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut prev_was_backslash = false;

    while let Some(ch) = chars.next() {
        if in_string {
            if prev_was_backslash {
                out.push(ch);
                prev_was_backslash = false;
                continue;
            }
            if ch == '\\' {
                out.push(ch);
                prev_was_backslash = true;
                continue;
            }
            if ch == '"' {
                in_string = false;
            }
            out.push(ch);
        } else {
            if ch == '"' {
                in_string = true;
                out.push(ch);
                continue;
            }
            if ch == '/' && chars.peek() == Some(&'/') {
                // consume rest of line
                chars.next(); // second /
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
                continue;
            }
            out.push(ch);
        }
    }

    // Remove trailing commas before `}` or `]`.
    remove_trailing_commas(&out)
}

fn remove_trailing_commas(s: &str) -> String {
    // State machine: track whether we're in a string to avoid modifying
    // commas inside string values.
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut in_string = false;
    let mut prev_backslash = false;

    while i < len {
        let ch = chars[i];
        if in_string {
            if prev_backslash {
                out.push(ch);
                prev_backslash = false;
                i += 1;
                continue;
            }
            if ch == '\\' {
                out.push(ch);
                prev_backslash = true;
                i += 1;
                continue;
            }
            if ch == '"' {
                in_string = false;
            }
            out.push(ch);
            i += 1;
            continue;
        }
        if ch == '"' {
            in_string = true;
            out.push(ch);
            i += 1;
            continue;
        }
        if ch == ',' {
            // Look ahead past whitespace/newlines to see if the next
            // non-whitespace char is `}` or `]`.
            let mut j = i + 1;
            while j < len && chars[j].is_whitespace() {
                j += 1;
            }
            if j < len && (chars[j] == '}' || chars[j] == ']') {
                // Skip this trailing comma.
                i += 1;
                continue;
            }
        }
        out.push(ch);
        i += 1;
    }
    out
}

#[cfg(test)]
mod jsonc_tests {
    use super::strip_jsonc;

    #[test]
    fn strip_line_comment() {
        let input = r#"{ "a": 1 // comment
}"#;
        let stripped = strip_jsonc(input);
        let v: serde_json::Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn strip_trailing_comma_object() {
        let input = r#"{ "a": 1, }"#;
        let stripped = strip_jsonc(input);
        let v: serde_json::Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn strip_trailing_comma_array() {
        let input = r#"[1, 2, ]"#;
        let stripped = strip_jsonc(input);
        let v: serde_json::Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(v[0], 1);
        assert_eq!(v[1], 2);
    }

    #[test]
    fn preserve_slash_in_string() {
        let input = r#"{ "url": "http://example.com" }"#;
        let stripped = strip_jsonc(input);
        let v: serde_json::Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(v["url"], "http://example.com");
    }

    #[test]
    fn tsconfig_style() {
        let input = r#"{
  // tsconfig for ts_aliases fixture
  "compilerOptions": {
    "baseUrl": ".",
    "paths": {
      "@core": ["src/core"],
      "@core/*": ["src/core/*"]
    }
  }
}"#;
        let stripped = strip_jsonc(input);
        let v: serde_json::Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(v["compilerOptions"]["baseUrl"], ".");
        let paths = &v["compilerOptions"]["paths"];
        assert!(paths["@core"].is_array());
    }
}

// ── Python parsing ───────────────────────────────────────────────────────────

pub fn parse_python_file(rel_path: &str, source: &str) -> Result<FileFacts> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::language())
        .map_err(|e| format!("load Python grammar: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| format!("tree-sitter failed to parse {rel_path}"))?;

    let mut facts = empty_facts(rel_path, "python");
    walk_python(tree.root_node(), source.as_bytes(), None, None, &mut facts);
    Ok(facts)
}

/// Recursive walk over a Python tree tracking the enclosing function name and class.
fn walk_python(node: Node, src: &[u8], enclosing_fn: Option<&str>, enclosing_class: Option<&str>, facts: &mut FileFacts) {
    let mut next_enclosing = enclosing_fn.map(str::to_string);
    let mut next_class = enclosing_class.map(str::to_string);

    match node.kind() {
        "class_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let class_name = text(name_node, src).to_string();
                next_class = Some(class_name);
            }
        }
        "function_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = text(name_node, src).to_string();
                let parent = enclosing_class.map(str::to_string);
                facts.functions.push(FnDecl {
                    name: name.clone(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    byte_range: node.byte_range(),
                    parent,
                });
                next_enclosing = Some(name);
            }
        }
        // `import x` / `import x.y` / `import x as a`
        "import_statement" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "dotted_name" => facts.use_decls.push(text(child, src).to_string()),
                    "aliased_import" => {
                        if let Some(name) = child.child_by_field_name("name") {
                            facts.use_decls.push(text(name, src).to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
        // `from x import y, z` — absolute or relative
        "import_from_statement" => {
            // Determine the module specifier (may be relative with leading dots).
            let module_opt = if let Some(module_node) = node.child_by_field_name("module_name") {
                let raw = text(module_node, src).to_string();
                Some(raw)
            } else {
                // `from . import y` has no module_name field (just a relative_import node).
                None
            };

            // Count leading dots for relative imports.
            let dot_count = {
                let mut cursor = node.walk();
                let mut dots = 0usize;
                for child in node.children(&mut cursor) {
                    if child.kind() == "." || child.kind() == "relative_import" {
                        // relative_import node contains dots as children
                        if child.kind() == "relative_import" {
                            let mut rc = child.walk();
                            for rc_child in child.children(&mut rc) {
                                if rc_child.kind() == "." {
                                    dots += 1;
                                }
                            }
                        } else {
                            dots += 1;
                        }
                    }
                }
                dots
            };

            // Build the effective module string with leading dots (for relative imports).
            let effective_module = if dot_count > 0 {
                let dots = ".".repeat(dot_count);
                match &module_opt {
                    Some(m) => format!("{dots}{m}"),
                    None => dots,
                }
            } else {
                match &module_opt {
                    Some(m) => m.clone(),
                    None => return,
                }
            };

            // Register the module in use_decls (for import resolution).
            if dot_count == 0 {
                facts.use_decls.push(effective_module.clone());
            }
            // Relative imports are resolved via from_imports at call-edge time.

            // Collect imported names.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                // Skip the module_name node itself.
                if let Some(mn) = node.child_by_field_name("module_name") {
                    if child.id() == mn.id() {
                        continue;
                    }
                }
                match child.kind() {
                    "dotted_name" => facts
                        .from_imports
                        .push((effective_module.clone(), text(child, src).to_string())),
                    "aliased_import" => {
                        if let Some(name) = child.child_by_field_name("name") {
                            facts
                                .from_imports
                                .push((effective_module.clone(), text(name, src).to_string()));
                        }
                    }
                    _ => {}
                }
            }
        }
        "call" => {
            if let Some(caller) = enclosing_fn {
                if let Some(func) = node.child_by_field_name("function") {
                    match func.kind() {
                        // bare `f(...)`
                        "identifier" => facts.calls.push(CallRef::new(
                            caller,
                            text(func, src).to_string(),
                            None,
                            node,
                        )),
                        // `x.f(...)` — only when the object is a plain
                        // identifier (a module qualifier candidate); chained
                        // attribute/method receivers need type info we do not
                        // have syntactically.
                        "attribute" => {
                            let object = func.child_by_field_name("object");
                            let attr = func.child_by_field_name("attribute");
                            if let (Some(object), Some(attr)) = (object, attr) {
                                if object.kind() == "identifier" {
                                    facts.calls.push(CallRef::new(
                                        caller,
                                        text(attr, src).to_string(),
                                        Some(text(object, src).to_string()),
                                        node,
                                    ));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_python(child, src, next_enclosing.as_deref(), next_class.as_deref(), facts);
    }
}

// ── TypeScript / TSX parsing ───────────────────────────────────────────────────

/// Parse a `.ts` (TypeScript) or `.tsx` (TSX) source. Both feed the identical
/// `FileFacts` shape and are labelled language `"typescript"`; only the grammar
/// differs so JSX in `.tsx` files parses.
pub fn parse_typescript_file(rel_path: &str, source: &str) -> Result<FileFacts> {
    let language = if rel_path.ends_with(".tsx") {
        tree_sitter_typescript::language_tsx()
    } else {
        tree_sitter_typescript::language_typescript()
    };
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|e| format!("load TypeScript grammar: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| format!("tree-sitter failed to parse {rel_path}"))?;

    let mut facts = empty_facts(rel_path, "typescript");
    walk_typescript(tree.root_node(), source.as_bytes(), None, None, &mut facts);
    Ok(facts)
}

/// Recursive walk over a TypeScript/TSX tree tracking the enclosing function and class.
fn walk_typescript(node: Node, src: &[u8], enclosing_fn: Option<&str>, enclosing_class: Option<&str>, facts: &mut FileFacts) {
    let mut next_enclosing = enclosing_fn.map(str::to_string);
    let mut next_class = enclosing_class.map(str::to_string);

    match node.kind() {
        "class_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let class_name = text(name_node, src).to_string();
                if let Some(class) = ts_top_level_class(node, &class_name, src) {
                    facts.ts_classes.push(class);
                }
                next_class = Some(class_name);
            }
        }
        // `function f(...) {}` — including exported (the export_statement wraps it).
        "function_declaration" | "generator_function_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = text(name_node, src).to_string();
                facts.functions.push(FnDecl {
                    name: name.clone(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    byte_range: node.byte_range(),
                    parent: enclosing_class.map(str::to_string),
                });
                next_enclosing = Some(name);
            }
        }
        // `method_definition` — class methods
        "method_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = text(name_node, src).to_string();
                // Skip constructor and special methods prefixed with # or [
                if name != "constructor" && !name.starts_with('#') {
                    let parent = enclosing_class.map(str::to_string);
                    facts.functions.push(FnDecl {
                        name: name.clone(),
                        start_line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        byte_range: node.byte_range(),
                        parent,
                    });
                    next_enclosing = Some(name);
                }
            }
        }
        // `import { a, b } from "./x"` / `import x from "./x"` / `import * as x from "./x"`
        "import_statement" => {
            if let Some(source_node) = node.child_by_field_name("source") {
                if let Some(spec) = string_literal_text(source_node, src) {
                    facts.use_decls.push(spec.clone());

                    // Also capture named imports for `from_imports`.
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        match child.kind() {
                            "import_clause" => {
                                // `import { a, b } from ...` — named imports
                                let mut ic = child.walk();
                                for ic_child in child.children(&mut ic) {
                                    match ic_child.kind() {
                                        "named_imports" => {
                                            let mut ni = ic_child.walk();
                                            for ni_child in ic_child.children(&mut ni) {
                                                if ni_child.kind() == "import_specifier" {
                                                    // Preserve the original import for existing resolution.
                                                    let import_name = ni_child
                                                        .child_by_field_name("name")
                                                        .map(|n| text(n, src).to_string())
                                                        .unwrap_or_default();
                                                    if !import_name.is_empty() {
                                                        if node.parent().is_some_and(|p| p.kind() == "program") {
                                                            let local = ni_child.child_by_field_name("alias")
                                                                .map_or(import_name.as_str(), |n| text(n, src));
                                                            facts.ts_named_imports.push(TsNamedImport {
                                                                module: spec.clone(),
                                                                exported: import_name.clone(),
                                                                local: local.to_owned(),
                                                            });
                                                        }
                                                        facts.from_imports.push((spec.clone(), import_name));
                                                    }
                                                }
                                            }
                                        }
                                        "namespace_import" => {
                                            // `import * as x from ...`
                                            let mut ns = ic_child.walk();
                                            for ns_child in ic_child.children(&mut ns) {
                                                if ns_child.kind() == "identifier" {
                                                    facts.ns_imports.push((text(ns_child, src).to_string(), spec.clone()));
                                                }
                                            }
                                        }
                                        "identifier" => {
                                            // `import x from ...` (default import)
                                            facts.from_imports.push((spec.clone(), text(ic_child, src).to_string()));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        // `export * from "./parse"` or `export { A, B } from "./parse"`
        "export_statement" => {
            // Get the source specifier if present (re-export).
            let source_spec = node.child_by_field_name("source")
                .and_then(|s| string_literal_text(s, src));
            if let Some(spec) = source_spec {
                // Check if it's `export * from ...` or `export { names } from ...`
                let mut is_star = false;
                let mut names: Vec<String> = Vec::new();
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "*" {
                        is_star = true;
                    }
                    if child.kind() == "export_clause" {
                        let mut ec = child.walk();
                        for ec_child in child.children(&mut ec) {
                            if ec_child.kind() == "export_specifier" {
                                // The name field is the original name.
                                if let Some(name_node) = ec_child.child_by_field_name("name") {
                                    names.push(text(name_node, src).to_string());
                                }
                            }
                        }
                    }
                }
                if is_star {
                    facts.reexports.push(ReExport {
                        module: spec,
                        names: None, // export * from
                    });
                } else if !names.is_empty() {
                    facts.reexports.push(ReExport {
                        module: spec,
                        names: Some(names),
                    });
                }
            }
        }
        "call_expression" => {
            if let Some(caller) = enclosing_fn {
                if let Some(func) = node.child_by_field_name("function") {
                    match func.kind() {
                        // bare `f(...)`
                        "identifier" => facts.calls.push(CallRef::new(
                            caller,
                            text(func, src).to_string(),
                            None,
                            node,
                        )),
                        // `obj.method(...)` — member expression
                        "member_expression" => {
                            let obj = func.child_by_field_name("object");
                            let prop = func.child_by_field_name("property");
                            if let (Some(obj), Some(prop)) = (obj, prop) {
                                if obj.kind() == "identifier" {
                                    facts.calls.push(CallRef::new(
                                        caller,
                                        text(prop, src).to_string(),
                                        Some(text(obj, src).to_string()),
                                        node,
                                    ));
                                } else if let Some(receiver) = ts_field_receiver(obj, node, src) {
                                    let mut call = CallRef::new(caller, text(prop, src).to_owned(), None, node);
                                    call.ts_receiver = Some(receiver);
                                    facts.calls.push(call);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_typescript(child, src, next_enclosing.as_deref(), next_class.as_deref(), facts);
    }
}

fn ts_has_child(node: Node, kinds: &[&str]) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).any(|child| kinds.contains(&child.kind()));
    found
}

fn ts_field_type(node: Node, src: &[u8]) -> Option<String> {
    let ty = node.child_by_field_name("type")?.named_child(0)?;
    (ty.kind() == "type_identifier").then(|| text(ty, src).to_owned())
}

fn ts_top_level_class(node: Node, name: &str, src: &[u8]) -> Option<TsClass> {
    let parent = node.parent()?;
    let is_export = parent.kind() == "export_statement";
    if (is_export && parent.parent()?.kind() != "program")
        || (!is_export && parent.kind() != "program")
    {
        return None;
    }
    let mut class = TsClass {
        name: name.to_owned(),
        exported: is_export && !ts_has_child(parent, &["default"]),
        fields: Vec::new(),
        methods: Vec::new(),
    };
    // Type parameters can shadow a class of the same name.
    let field_type = |field| {
        if node.child_by_field_name("type_parameters").is_some() {
            None
        } else {
            ts_field_type(field, src)
        }
    };
    let body = node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        let Some(field) = member.child_by_field_name("name") else { continue };
        let field = text(field, src);
        match member.kind() {
            "public_field_definition" => {
                let ty = if ts_has_child(member, &["static"]) {
                    None
                } else {
                    field_type(member)
                };
                class.fields.push((field.to_owned(), ty));
            }
            "method_definition" if field == "constructor" => {
                let Some(params) = member.child_by_field_name("parameters") else { continue };
                let mut cursor = params.walk();
                for param in params.named_children(&mut cursor) {
                    if !matches!(param.kind(), "required_parameter" | "optional_parameter")
                        || !ts_has_child(param, &["accessibility_modifier", "readonly", "override_modifier"])
                    {
                        continue;
                    }
                    let Some(field) = param.child_by_field_name("pattern").or_else(|| param.child_by_field_name("name")) else { continue };
                    if field.kind() == "identifier" {
                        class.fields.push((text(field, src).to_owned(), field_type(param)));
                    }
                }
            }
            "method_definition" => class.methods.push((
                field.to_owned(),
                !ts_has_child(member, &["static", "get", "set"]),
            )),
            _ => {}
        }
    }
    Some(class)
}

fn ts_field_receiver(receiver: Node, call: Node, src: &[u8]) -> Option<TsReceiver> {
    if receiver.kind() != "member_expression"
        || receiver.child_by_field_name("object")?.kind() != "this"
    {
        return None;
    }
    let field = receiver.child_by_field_name("property")?;
    if field.kind() != "property_identifier" {
        return None;
    }

    // `this` must belong to an indexed instance method, not a nested
    // ordinary function, object method, or a static method.
    let mut ancestor = call.parent();
    while let Some(node) = ancestor {
        match node.kind() {
            "method_definition" => {
                let name = node.child_by_field_name("name")?;
                let class_body = node.parent()?;
                let class = class_body.parent()?;
                if text(name, src) == "constructor"
                    || ts_has_child(node, &["static", "get", "set"])
                    || class_body.kind() != "class_body"
                    || class.kind() != "class_declaration"
                    || !class.parent().is_some_and(|p| p.kind() == "program"
                        || (p.kind() == "export_statement" && p.parent().is_some_and(|p| p.kind() == "program")))
                {
                    return None;
                }
                return Some(TsReceiver {
                    caller_class: text(class.child_by_field_name("name")?, src).to_owned(),
                    field: text(field, src).to_owned(),
                });
            }
            "function_declaration" | "generator_function_declaration" | "function_expression"
            | "generator_function" | "class_declaration" | "class" => return None,
            _ => ancestor = node.parent(),
        }
    }
    None
}

fn unique<T>(mut items: impl Iterator<Item = T>) -> Option<T> {
    let first = items.next()?;
    items.next().is_none().then_some(first)
}

fn resolve_ts_field_call(
    file: &FileFacts,
    call: &CallRef,
    receiver: &TsReceiver,
    all: &[FileFacts],
    known_paths: &[&str],
    aliases: &TsAliases,
) -> Option<(String, String)> {
    let caller = unique(file.ts_classes.iter().filter(|c| c.name == receiver.caller_class))?;
    let method = |f: &FileFacts, c: &TsClass, name: &str| {
        let (_, instance) = unique(c.methods.iter().filter(|(method, _)| method == name))?;
        if !*instance || c.fields.iter().any(|(field, _)| field == name) {
            return None;
        }
        unique(f.functions.iter().filter(|decl| {
            decl.name == name && decl.parent.as_deref() == Some(c.name.as_str())
        }))?;
        Some(format!("{}::{}::{name}", f.path, c.name))
    };
    let (_, ty) = unique(caller.fields.iter().filter(|(field, _)| *field == receiver.field))?;
    let ty = ty.as_deref()?;
    let from = method(file, caller, &call.caller_fn)?;

    let mut imports = file.ts_named_imports.iter().filter(|import| import.local == ty);
    let local = file.ts_classes.iter().filter(|class| class.name == ty);
    let (target, class) = if let Some(import) = imports.next() {
        if imports.next().is_some() || local.count() != 0 {
            return None;
        }
        let path = resolve_ts_module(&import.module, &file.path, aliases, known_paths)?;
        let target = all.iter().find(|f| f.path == path && f.language == "typescript")?;
        let class = unique(target.ts_classes.iter().filter(|class| class.name == import.exported))?;
        if !class.exported {
            return None;
        }
        (target, class)
    } else {
        (file, unique(local)?)
    };
    Some((from, method(target, class, &call.callee_name)?))
}

/// Extract the inner text of a `string` node (`"./auth"` -> `./auth`), via its
/// `string_fragment` child; falls back to trimming quote characters.
fn string_literal_text(node: Node, src: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "string_fragment" {
            return Some(text(child, src).to_string());
        }
    }
    let raw = text(node, src);
    let trimmed = raw.trim_matches(|c| c == '"' || c == '\'' || c == '`');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// ── TypeScript alias resolution ───────────────────────────────────────────────

/// Parsed tsconfig paths/baseUrl for alias resolution.
#[derive(Debug, Default)]
struct TsAliases {
    /// baseUrl relative to the tsconfig's directory (repo-relative).
    base_url: Option<String>,
    /// Sorted alias entries: (pattern_with_optional_star, replacement_with_optional_star).
    paths: Vec<(String, String)>,
}

/// Load tsconfig.json JSONC from `tsconfig_path` (repo-relative) and extract
/// baseUrl + paths aliases. On parse failure logs to stderr and returns empty.
fn load_ts_aliases(tsconfig_path: &str, known_paths: &[&str]) -> TsAliases {
    // tsconfig_path is repo-relative; resolve it from known_paths to find the
    // absolute path. For our fixtures tsconfig.json is at the repo root.
    let _ = known_paths; // used by the caller to find the root; we rely on cwd equivalence here.

    // Try reading the file from absolute path via std::fs.
    // The caller passes us the relative path; we read it relative to cwd.
    let contents = match std::fs::read_to_string(tsconfig_path) {
        Ok(s) => s,
        // An absent tsconfig is the normal case (any non-TS-alias repo) — stay
        // silent. Only a present-but-unreadable file (permissions, etc.) warns.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return TsAliases::default(),
        Err(e) => {
            eprintln!("lattice: warning: could not read {tsconfig_path}: {e}");
            return TsAliases::default();
        }
    };
    let stripped = strip_jsonc(&contents);
    let v: serde_json::Value = match serde_json::from_str(&stripped) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("lattice: warning: could not parse {tsconfig_path}: {e}");
            return TsAliases::default();
        }
    };

    let opts = &v["compilerOptions"];
    let base_url = opts["baseUrl"].as_str().map(|s| s.to_string());

    let mut paths: Vec<(String, String)> = Vec::new();
    if let Some(paths_map) = opts["paths"].as_object() {
        for (pattern, targets) in paths_map {
            if let Some(first_target) = targets.as_array().and_then(|a| a.first()).and_then(|v| v.as_str()) {
                paths.push((pattern.clone(), first_target.to_string()));
            }
        }
    }
    // Sort longer patterns first for greedy matching.
    paths.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    TsAliases { base_url, paths }
}

/// Resolve a TypeScript module specifier to a repo-relative path, using:
///   1. Relative specifiers (`./x`, `../x`) → normalize + try .ts/.tsx/index.ts/index.tsx
///   2. tsconfig alias patterns → expand then resolve as relative from baseUrl
///   3. `.js`/`.jsx`/`.mjs`/`.cjs` suffix → try `.ts`/`.tsx` variant
fn resolve_ts_module(
    spec: &str,
    from: &str,
    aliases: &TsAliases,
    known_paths: &[&str],
) -> Option<String> {
    // Helper: given a base path string (no extension), probe candidate extensions.
    let probe = |base: &str| -> Option<String> {
        let candidates = [
            format!("{base}.ts"),
            format!("{base}.tsx"),
            format!("{base}/index.ts"),
            format!("{base}/index.tsx"),
        ];
        for c in &candidates {
            if known_paths.contains(&c.as_str()) {
                return Some(c.clone());
            }
        }
        None
    };

    // Strip .js/.jsx/.mjs/.cjs suffix for resolution.
    let spec_base = if spec.ends_with(".js") || spec.ends_with(".jsx") || spec.ends_with(".mjs") || spec.ends_with(".cjs") {
        &spec[..spec.rfind('.').unwrap()]
    } else {
        spec
    };

    let dir = Path::new(from).parent().unwrap_or_else(|| Path::new(""));

    // 1. Relative specifier.
    if spec_base.starts_with('.') {
        let joined = dir.join(spec_base);
        let normalized = normalize_path(&joined);
        let base = normalized.to_string_lossy().replace('\\', "/");
        return probe(&base);
    }

    // 2. Alias resolution.
    for (pattern, replacement) in &aliases.paths {
        if let Some(resolved) = match_alias(spec_base, pattern, replacement) {
            // `resolved` is relative to baseUrl.
            let full = if let Some(ref bu) = aliases.base_url {
                if bu == "." {
                    resolved.clone()
                } else {
                    format!("{bu}/{resolved}")
                }
            } else {
                resolved.clone()
            };
            // Probe direct path (may be a directory → index.ts).
            if known_paths.contains(&full.as_str()) {
                return Some(full.clone());
            }
            // Try extensions on the full path.
            if let Some(hit) = probe(&full) {
                return Some(hit);
            }
            // Try as directory index.
            let idx_ts = format!("{full}/index.ts");
            let idx_tsx = format!("{full}/index.tsx");
            if known_paths.contains(&idx_ts.as_str()) {
                return Some(idx_ts);
            }
            if known_paths.contains(&idx_tsx.as_str()) {
                return Some(idx_tsx);
            }
        }
    }

    None
}

/// Match a specifier against an alias pattern (which may contain `*`).
/// Returns the expanded replacement string if matched, else None.
fn match_alias(spec: &str, pattern: &str, replacement: &str) -> Option<String> {
    if let Some(star_pos) = pattern.find('*') {
        let prefix = &pattern[..star_pos];
        let suffix = &pattern[star_pos + 1..];
        if spec.starts_with(prefix) && spec.ends_with(suffix) && spec.len() >= prefix.len() + suffix.len() {
            let captured = &spec[prefix.len()..spec.len() - suffix.len()];
            let result = replacement.replace('*', captured);
            return Some(result);
        }
    } else if spec == pattern {
        return Some(replacement.to_string());
    }
    None
}

/// Chase barrel re-exports: if `target_path` has `export * from "./x"` (or named
/// re-exports that include `name`), walk the chain to find the actual defining file.
/// Returns the path of the file that actually defines a function named `name`,
/// or `target_path.to_string()` if no chase is needed/possible.
fn chase_ts_export(
    target_path: &str,
    name: &str,
    all_facts: &[FileFacts],
    known_paths: &[&str],
    aliases: &TsAliases,
    depth: usize,
) -> String {
    if depth > 8 {
        return target_path.to_string();
    }
    // Check if `target_path` directly defines `name`.
    if let Some(f) = all_facts.iter().find(|f| f.path == target_path) {
        if f.functions.iter().any(|fn_| fn_.name == name) {
            return target_path.to_string();
        }
        // Chase re-exports.
        for re in &f.reexports {
            let export_all = re.names.is_none();
            let exports_name = re.names.as_ref().is_some_and(|names| names.iter().any(|n| n == name));
            if export_all || exports_name {
                if let Some(next_path) = resolve_ts_module(&re.module, &f.path, aliases, known_paths) {
                    let next = chase_ts_export(&next_path, name, all_facts, known_paths, aliases, depth + 1);
                    if next != next_path || all_facts.iter().any(|ff| ff.path == next && ff.functions.iter().any(|fn_| fn_.name == name)) {
                        return next;
                    }
                }
            }
        }
    }
    target_path.to_string()
}

/// Resolve a Python relative import specifier (e.g. `.core`, `..util`) to a
/// repo-relative file path. `from_path` is the importing file's repo-relative path.
fn resolve_py_relative(spec: &str, from_path: &str, known_paths: &[&str]) -> Option<String> {
    // Count leading dots.
    let dots = spec.chars().take_while(|c| *c == '.').count();
    let module_part = &spec[dots..];

    // Navigate up `dots` directory levels from from_path's directory.
    let dir = Path::new(from_path).parent().unwrap_or_else(|| Path::new(""));
    let mut base = dir.to_path_buf();
    for _ in 1..dots {
        // For 1 dot: stay in dir. For 2 dots: go up one. For n dots: go up n-1.
        base = base.parent().unwrap_or(&base).to_path_buf();
    }

    if module_part.is_empty() {
        // `from . import y` — base dir's __init__.py
        let init = base.join("__init__.py");
        let init_str = init.to_string_lossy().replace('\\', "/");
        if known_paths.contains(&init_str.as_str()) {
            return Some(init_str);
        }
        return None;
    }

    let module_path = module_part.replace('.', "/");
    let candidates = [
        base.join(format!("{module_path}.py")),
        base.join(&module_path).join("__init__.py"),
    ];
    for cand in &candidates {
        let s = cand.to_string_lossy().replace('\\', "/");
        if known_paths.contains(&s.as_str()) {
            return Some(s);
        }
    }
    None
}

/// Chase Python __init__.py re-exports: if `target_path` is an __init__.py,
/// look for `from .x import name` and follow to the actual defining file.
fn chase_py_name(
    target_path: &str,
    name: &str,
    all_facts: &[FileFacts],
    known_paths: &[&str],
    depth: usize,
) -> String {
    if depth > 8 {
        return target_path.to_string();
    }
    if let Some(f) = all_facts.iter().find(|f| f.path == target_path) {
        // If this file defines the function directly, we're done.
        if f.functions.iter().any(|fn_| fn_.name == name) {
            return target_path.to_string();
        }
        // If it's an __init__.py (or any file with from_imports re-exporting `name`),
        // chase the re-export.
        for (module, imported_name) in &f.from_imports {
            if imported_name == name {
                // Resolve the module relative to this file.
                let resolved = if module.starts_with('.') {
                    resolve_py_relative(module, &f.path, known_paths)
                } else {
                    // Absolute import within the package.
                    let module_path = module.replace('.', "/");
                    let dir = Path::new(&f.path).parent().unwrap_or_else(|| Path::new(""));
                    let candidates = [
                        dir.join(format!("{module_path}.py")),
                        dir.join(&module_path).join("__init__.py"),
                        std::path::PathBuf::from(format!("{module_path}.py")),
                        std::path::PathBuf::from(&module_path).join("__init__.py"),
                    ];
                    candidates.iter()
                        .map(|c| c.to_string_lossy().replace('\\', "/"))
                        .find(|s| known_paths.contains(&s.as_str()))
                };
                if let Some(next_path) = resolved {
                    return chase_py_name(&next_path, name, all_facts, known_paths, depth + 1);
                }
            }
        }
    }
    target_path.to_string()
}

// ── Cross-file resolution ────────────────────────────────────────────────────

/// Resolve module names to files and call references to function nodes,
/// producing the persistable graph. Unresolvable imports (std/external crates)
/// and unresolvable calls produce no edge. `root` locates `tsconfig.json` for
/// TypeScript alias resolution.
pub fn resolve_with_root(all: &[FileFacts], root: &str) -> Graph {
    let mut graph = Graph::default();

    // Function name index: name -> paths of files defining it.
    let mut fn_index: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for f in all {
        for func in &f.functions {
            fn_index.entry(&func.name).or_default().push(&f.path);
        }
    }
    let known_paths: Vec<&str> = all.iter().map(|f| f.path.as_str()).collect();

    // Load tsconfig.json aliases if present.
    let tsconfig_path = format!("{root}/tsconfig.json");
    let tsconfig_path_clean = if root == "." { "tsconfig.json".to_string() } else { tsconfig_path };
    let aliases = load_ts_aliases(&tsconfig_path_clean, &known_paths);

    // Rust: `mod x;` -> sibling x.rs or x/mod.rs relative to F's directory;
    //       `use crate::x...` -> root x.rs or x/mod.rs.
    // Python: `import x` / `from x import y` -> x.py or x/__init__.py relative
    //       to F's directory, then the repo root (no edge for stdlib/3rd-party).
    // TypeScript: relative + alias resolution (see resolve_ts_module).
    let resolve_module = |module: &str, from: &str, language: &str| -> Option<String> {
        let dir = Path::new(from).parent().unwrap_or_else(|| Path::new(""));
        match language {
            "typescript" => resolve_ts_module(module, from, &aliases, &known_paths),
            "python" => {
                if module.starts_with('.') {
                    resolve_py_relative(module, from, &known_paths)
                } else {
                    let module_path = module.replace('.', "/");
                    let candidates = [
                        dir.join(format!("{module_path}.py")),
                        dir.join(&module_path).join("__init__.py"),
                        Path::new(&format!("{module_path}.py")).to_path_buf(),
                        Path::new(&module_path).join("__init__.py"),
                    ];
                    for cand in &candidates {
                        let cand = cand.to_string_lossy().replace('\\', "/");
                        if known_paths.contains(&cand.as_str()) {
                            return Some(cand);
                        }
                    }
                    None
                }
            }
            _ => {
                // Rust
                let candidates = [
                    dir.join(format!("{module}.rs")),
                    dir.join(module).join("mod.rs"),
                    Path::new(&format!("{module}.rs")).to_path_buf(),
                    Path::new(module).join("mod.rs"),
                ];
                for cand in &candidates {
                    let cand = cand.to_string_lossy().replace('\\', "/");
                    if known_paths.contains(&cand.as_str()) {
                        return Some(cand);
                    }
                }
                None
            }
        }
    };

    for f in all {
        // Retrieval-only languages (Markdown) are embedded and FTS-indexed but
        // contribute NO graph nodes or edges — no File node, no module node, no
        // functions. Skipping here keeps every existing graph-cardinality gate
        // (health counts, hotspots, coupling, blast radius) byte-identical.
        if f.language == "markdown" {
            continue;
        }

        graph.files.push((f.path.clone(), f.language.to_string()));

        // File-level module node, 1:1 with its file (Rust MVP).
        let stem = Path::new(&f.path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| f.path.clone());
        graph
            .modules
            .push((format!("module:{}", f.path), stem, f.path.clone()));

        for func in &f.functions {
            // For method nodes: id includes parent qualifier.
            let id = if let Some(ref parent) = func.parent {
                format!("{}::{}::{}", f.path, parent, func.name)
            } else {
                format!("{}::{}", f.path, func.name)
            };
            graph.functions.push((
                id,
                func.name.clone(),
                f.path.clone(),
                func.start_line,
                func.parent.clone(),
            ));
        }

        // imports edges (deduplicated per file).
        let mut targets: Vec<String> = Vec::new();

        let add_target = |tgt: String, targets: &mut Vec<String>| {
            if tgt != f.path && !targets.contains(&tgt) {
                targets.push(tgt);
            }
        };

        for module in f.mod_decls.iter().chain(f.use_decls.iter()) {
            if let Some(target) = resolve_module(module, &f.path, f.language) {
                add_target(target.clone(), &mut targets);
                // For TypeScript: if the resolved file is a barrel (has reexports),
                // also add direct import edges to the re-exported source files
                // so the 1-hop dependents query finds this file as a dependent.
                if f.language == "typescript" {
                    if let Some(barrel) = all.iter().find(|ff| ff.path == target) {
                        for re in &barrel.reexports {
                            if re.names.is_none() {
                                // export * from: add import edge to the source file.
                                if let Some(re_target) = resolve_ts_module(&re.module, &barrel.path, &aliases, &known_paths) {
                                    add_target(re_target, &mut targets);
                                }
                            }
                        }
                    }
                }
            }
        }
        // Also chase re-exports for TS re-export files themselves (so index.ts → parse.ts).
        if f.language == "typescript" {
            for re in &f.reexports {
                if let Some(target) = resolve_ts_module(&re.module, &f.path, &aliases, &known_paths) {
                    add_target(target, &mut targets);
                }
            }
        }
        // For Python, relative from_imports also produce import edges.
        // Also chase __init__.py re-exports so 1-hop dependents finds transitive importers.
        if f.language == "python" {
            for (module, name) in &f.from_imports {
                let resolved = if module.starts_with('.') {
                    resolve_py_relative(module, &f.path, &known_paths)
                } else {
                    let module_path = module.replace('.', "/");
                    let dir = Path::new(&f.path).parent().unwrap_or_else(|| Path::new(""));
                    let candidates = [
                        dir.join(format!("{module_path}.py")),
                        dir.join(&module_path).join("__init__.py"),
                        std::path::PathBuf::from(format!("{module_path}.py")),
                        std::path::PathBuf::from(&module_path).join("__init__.py"),
                    ];
                    candidates.iter()
                        .map(|c| c.to_string_lossy().replace('\\', "/"))
                        .find(|s| known_paths.contains(&s.as_str()))
                };
                if let Some(ref target) = resolved {
                    add_target(target.clone(), &mut targets);
                    // Chase through __init__.py: if the resolved file re-exports
                    // `name` via `from .x import name`, add a direct edge to x.
                    let chased = chase_py_name(target, name, all, &known_paths, 0);
                    if chased != *target {
                        add_target(chased, &mut targets);
                    }
                }
            }
        }
        for target in targets {
            graph.imports.push((f.path.clone(), target));
        }
    }

    // Call edges use per-language resolution heuristics:
    // qualified `m::f(..)` / `x.f(..)` resolves via the module file (Python
    // additionally requires `import x` in scope) → evidence `binding`;
    // bare `f(..)` resolves same-file first (`binding`, and only when that
    // file has exactly one function of that name), then a named import
    // (`binding`), then Rust: a unique cross-file definition (`name`).
    // Ambiguous or unknown names produce no edge.
    for f in all {
        for call in &f.calls {
            if let Some(receiver) = &call.ts_receiver {
                if let Some((from, to)) = resolve_ts_field_call(f, call, receiver, all, &known_paths, &aliases) {
                    graph.calls.push((from, to, CallEvidence::Binding, CallSite {
                        path: f.path.clone(),
                        start_line: call.start_line,
                        end_line: call.end_line,
                    }));
                }
                continue;
            }
            let defines = |file: &str| {
                fn_index
                    .get(call.callee_name.as_str())
                    .is_some_and(|paths| paths.iter().any(|p| *p == file))
            };
            let resolved: Option<(String, CallEvidence)> = match &call.qualifier {
                Some(q) => {
                    let file = if f.language == "typescript" {
                        // Check if qualifier is a named import binding resolved to a module.
                        let resolved_spec = f.from_imports.iter()
                            .find(|(_, local)| local == q)
                            .map(|(spec, _)| spec.as_str());
                        if let Some(spec) = resolved_spec {
                            resolve_ts_module(spec, &f.path, &aliases, &known_paths)
                                .map(|file| chase_ts_export(&file, &call.callee_name, all, &known_paths, &aliases, 0).to_string())
                                .filter(|file| defines(file))
                        } else {
                            // Qualifier is a namespace import or module-level identifier.
                            resolve_ts_module(q, &f.path, &aliases, &known_paths)
                                .map(|file| chase_ts_export(&file, &call.callee_name, all, &known_paths, &aliases, 0).to_string())
                                .filter(|file| defines(file))
                        }
                    } else {
                        // Python or Rust: check if qualifier is in scope.
                        let in_scope = f.language != "python" || f.use_decls.iter().any(|m| {
                            // `import x` scope: module basename matches qualifier.
                            m.split('.').last().unwrap_or(m) == q
                        }) || f.from_imports.iter().any(|(_, name)| name == q);
                        if in_scope {
                            if f.language == "python" {
                                // Try to find the file that q was imported from.
                                let resolved_module = f.from_imports.iter()
                                    .find(|(_, name)| name == q)
                                    .and_then(|(module, _)| resolve_module(module, &f.path, f.language))
                                    .or_else(|| resolve_module(q, &f.path, f.language));
                                resolved_module
                                    .map(|file| chase_py_name(&file, &call.callee_name, all, &known_paths, 0).to_string())
                                    .filter(|file| defines(file))
                            } else {
                                resolve_module(q, &f.path, f.language).filter(|file| defines(file))
                            }
                        } else {
                            None
                        }
                    };
                    file.map(|file| (file, CallEvidence::Binding))
                }
                None => {
                    let same_file = f
                        .functions
                        .iter()
                        .filter(|d| d.name == call.callee_name)
                        .count();
                    if same_file == 1 {
                        Some((f.path.clone(), CallEvidence::Binding))
                    } else if same_file > 1 {
                        // Two definitions of the same name in one file: fail closed.
                        None
                    } else if f.language == "typescript" {
                        // Named import: find the specifier that imports this name.
                        f.from_imports
                            .iter()
                            .find(|(_, name)| *name == call.callee_name)
                            .and_then(|(spec, _)| resolve_ts_module(spec, &f.path, &aliases, &known_paths))
                            .map(|file| chase_ts_export(&file, &call.callee_name, all, &known_paths, &aliases, 0).to_string())
                            .filter(|file| defines(file))
                            .map(|file| (file, CallEvidence::Binding))
                    } else if f.language == "python" {
                        f.from_imports
                            .iter()
                            .find(|(_, name)| *name == call.callee_name)
                            .and_then(|(module, _)| {
                                if module.starts_with('.') {
                                    resolve_py_relative(module, &f.path, &known_paths)
                                } else {
                                    resolve_module(module, &f.path, f.language)
                                }
                            })
                            .map(|file| chase_py_name(&file, &call.callee_name, all, &known_paths, 0).to_string())
                            .filter(|file| defines(file))
                            .map(|file| (file, CallEvidence::Binding))
                    } else {
                        match fn_index.get(call.callee_name.as_str()) {
                            Some(paths) if paths.len() == 1 => {
                                Some((paths[0].to_string(), CallEvidence::Name))
                            }
                            _ => None, // ambiguous or unknown: no edge
                        }
                    }
                }
            };
            let Some((callee_file, evidence)) = resolved else {
                continue;
            };
            // Fail closed when the target file has more than one function of
            // that name — do not pick the first.
            let Some(callee_facts) = all.iter().find(|ff| ff.path == callee_file) else {
                continue;
            };
            let matches: Vec<&FnDecl> = callee_facts
                .functions
                .iter()
                .filter(|fn_| fn_.name == call.callee_name)
                .collect();
            if matches.len() != 1 {
                continue;
            }
            let fn_ = matches[0];
            let callee_id = if let Some(ref parent) = fn_.parent {
                format!("{}::{}::{}", callee_file, parent, fn_.name)
            } else {
                format!("{}::{}", callee_file, fn_.name)
            };

            let caller_id = f
                .functions
                .iter()
                .find(|fn_| fn_.name == call.caller_fn)
                .map(|fn_| {
                    if let Some(ref parent) = fn_.parent {
                        format!("{}::{}::{}", f.path, parent, fn_.name)
                    } else {
                        format!("{}::{}", f.path, fn_.name)
                    }
                })
                .unwrap_or_else(|| format!("{}::{}", f.path, call.caller_fn));

            graph.calls.push((
                caller_id,
                callee_id,
                evidence,
                CallSite {
                    path: f.path.clone(),
                    start_line: call.start_line,
                    end_line: call.end_line,
                },
            ));
        }
    }

    graph
}
