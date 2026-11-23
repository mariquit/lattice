// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Query router — the single dispatch point for `lattice query`.
//!
//! This module makes `query_type` trustworthy for CLI consumers by
//! resolving every `query` invocation through one fixed precedence and emitting
//! exactly one envelope from the union {impact | semantic | error}:
//!
//!   1. guard: `.lattice/` absent at the repo root  -> error `no_lattice`
//!   2. `--impact <path>`  (flag beats text)          -> graph backend, `impact`
//!   3. non-empty free text                           -> vector backend, `semantic`
//!   4. anything else (no flag, no text)              -> error `ambiguous_query`
//!
//! Guard failures and `ambiguous_query` both exit non-zero; the two happy paths
//! exit 0. stdout carries the single JSON envelope; prose goes to stderr.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Value};

use crate::class;
use crate::freshness::{self, Freshness};
use crate::ingest::{CallEvidence, CallSite};
use crate::{analysis, branch, centrality, embed, envelope, fts, fuse, graph, health, markdown, ownership, review, snapshot, vectors, LATTICE_DIR};

/// Default hit limit for symbol and semantic queries.
pub const DEFAULT_LIMIT: usize = 20;

/// Depth at which each backend is queried during hybrid fusion. Symmetric
/// depths keep RRF contribution scales balanced; both lists are fetched deep
/// and the fused result is truncated to `filters.limit` afterward.
const FUSION_DEPTH: usize = 20;

/// Query-level filters extracted from the CLI argument tail by `extract_flags`.
/// These are passed alongside the routing intent into the dispatch functions.
pub struct Filters {
    /// Maximum hits to return in the response.
    pub limit: usize,
    /// When true, drop hits whose source class is "test" or "fixture".
    pub exclude_tests: bool,
    /// When Some, restrict `--symbol` results to this kind. Invalid on
    /// semantic/impact → `invalid_flag`.
    pub kind: Option<String>,
    /// When true, return the legacy file-level hit shape `{path, score, class}`
    /// via `--file-level`.  Invalid on non-semantic paths → `invalid_flag`.
    pub file_level: bool,
    /// When true, include `next_tool_suggestions` in the response envelope.
    pub hints: bool,
    /// When set, drop call edges below this level. `None` means `name`
    /// (every edge) and the field is omitted from the envelope.
    pub min_evidence: Option<CallEvidence>,
    /// When true, symbol call hits gain an `evidence` field. Opt-in so the
    /// default hit shape stays `{path, name, line}`.
    pub with_evidence: bool,
    /// Leiden resolution for `--communities`. `None` = default.
    pub resolution: Option<f64>,
    /// Recursively split oversized communities (`--communities --split`).
    pub split: bool,
    /// Explicit `--branch <name>` override. `None` = active branch.
    pub branch: Option<String>,
    /// `--all-branches`: pre-bump global catalog (protocol opt-out).
    pub all_branches: bool,
}

impl Default for Filters {
    fn default() -> Self {
        Filters {
            limit: DEFAULT_LIMIT,
            exclude_tests: false,
            kind: None,
            file_level: false,
            hints: false,
            min_evidence: None,
            with_evidence: false,
            resolution: None,
            split: false,
            branch: None,
            all_branches: false,
        }
    }
}

/// Strip `--limit <N>`, `--exclude-tests`, and `--kind <k>` from `rest`,
/// returning the remaining "core" positional args and the parsed `Filters`.
/// Any syntactically invalid flag (unknown flag, bad value) returns `Err` with
/// an error message for the `invalid_flag` envelope.
fn extract_flags<'a>(rest: &'a [&'a str]) -> Result<(Vec<&'a str>, Filters), String> {
    let mut core: Vec<&'a str> = Vec::new();
    let mut limit = DEFAULT_LIMIT;
    let mut exclude_tests = false;
    let mut kind: Option<String> = None;
    let mut file_level = false;
    let mut hints = false;
    let mut min_evidence: Option<CallEvidence> = None;
    let mut with_evidence = false;
    let mut resolution: Option<f64> = None;
    let mut split = false;
    let mut branch: Option<String> = None;
    let mut all_branches = false;

    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "--limit" => {
                i += 1;
                let val = rest.get(i).ok_or("--limit requires a value")?;
                let n: usize = val
                    .parse::<usize>()
                    .map_err(|_| format!("--limit value must be a positive integer, got {val:?}"))?;
                if n == 0 {
                    return Err("--limit must be >= 1".to_string());
                }
                limit = n;
            }
            "--exclude-tests" => {
                exclude_tests = true;
            }
            "--kind" => {
                i += 1;
                let val = rest.get(i).ok_or("--kind requires a value")?;
                match *val {
                    "function" | "method" | "module" => {
                        kind = Some(val.to_string());
                    }
                    other => {
                        return Err(format!(
                            "--kind must be one of function, method, module; got {other:?}"
                        ));
                    }
                }
            }
            "--file-level" => {
                file_level = true;
            }
            "--hints" => {
                hints = true;
            }
            "--min-evidence" => {
                i += 1;
                let val = rest.get(i).ok_or("--min-evidence requires a value")?;
                min_evidence = Some(CallEvidence::parse(val).ok_or_else(|| {
                    format!("--min-evidence must be one of name, binding; got {val:?}")
                })?);
            }
            "--with-evidence" => {
                with_evidence = true;
            }
            "--resolution" => {
                i += 1;
                let val = rest.get(i).ok_or("--resolution requires a value")?;
                let r: f64 = val
                    .parse::<f64>()
                    .map_err(|_| format!("--resolution value must be a number, got {val:?}"))?;
                if !r.is_finite() || r <= 0.0 {
                    return Err("--resolution must be a finite number > 0".to_string());
                }
                resolution = Some(r);
            }
            "--split" => {
                split = true;
            }
            "--branch" => {
                i += 1;
                let val = rest.get(i).ok_or("--branch requires a value")?;
                if val.is_empty() || val.starts_with("--") {
                    return Err("--branch requires a non-empty branch name".to_string());
                }
                branch = Some((*val).to_string());
            }
            "--all-branches" => {
                all_branches = true;
            }
            "--hybrid" | "--no-route" => {
                return Err(format!("{} was removed; pass search text without this flag", rest[i]));
            }
            "--with-criticality" => {
                // Accepted but handled in dispatch_hotspots.
            }
            other if other.starts_with("--") && other != "--impact" && other != "--symbol"
                && other != "--callers" && other != "--callees" && other != "--call-chain"
                && other != "--owners"
                && other != "--health" && other != "--hotspots"
                && other != "--dead-code" && other != "--coupling"
                && other != "--flows" && other != "--flow"
                && other != "--communities" && other != "--community"
                && other != "--graph-diff" && other != "--review"
                && other != "--similar" && other != "--peek"
                && other != "--bridges" && other != "--context" =>
            {
                return Err(format!("unknown flag {other:?}"));
            }
            arg => {
                core.push(arg);
            }
        }
        i += 1;
    }

    Ok((
        core,
        Filters {
            limit,
            exclude_tests,
            kind,
            file_level,
            hints,
            min_evidence,
            with_evidence,
            resolution,
            split,
            branch,
            all_branches,
        },
    ))
}

/// The resolved intent of a `query` invocation, after parsing but before the
/// guard runs. `--impact`/`--symbol` flags beat free text; empty/unknown shapes
/// are ambiguous.
enum Intent<'a> {
    Impact(&'a str),
    ImpactSymbol(&'a str),
    Callers(&'a str),
    Callees(&'a str),
    CallChain(&'a str, &'a str),
    Symbol(&'a str),
    Search(&'a str),
    Owners(&'a str),
    Health,
    Hotspots,
    DeadCode,
    Coupling(&'a str),
    Flows,
    Flow(&'a str),
    Communities,
    Community(&'a str),
    GraphDiff(&'a str, &'a str),
    Review(&'a str),
    Similar(&'a str),
    Peek(&'a str),
    Bridges,
    Context(&'a str),
    Ambiguous,
}

/// Outcome of routing: which envelope was emitted and the process disposition.
pub enum Routed {
    /// A success envelope (impact | semantic) was emitted; exit 0.
    Ok,
    /// An error envelope was emitted; the caller must exit non-zero.
    Emitted,
    /// Unexpected internal failure; `main` wraps it in an envelope.
    Internal(String),
}

/// The disposition of a routed query BEFORE the envelope is printed — the
/// return-value analogue of `Routed`. `Ok` mirrors exit 0; `Error` mirrors a
/// structured error with a nonzero exit. `Internal` is an unexpected failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// A success envelope (impact | symbol | semantic).
    Ok,
    /// A structured error envelope (no_lattice | ambiguous_query |
    /// model_not_found).
    Error,
    /// An unexpected internal failure; the caller wraps the message.
    Internal,
}

/// Parse the `query` argument tail (everything after the `query` subcommand)
/// into an intent. `--impact <path>` takes precedence over free text; a bare
/// `query`, or any shape we cannot resolve to a backend, is ambiguous.
fn parse_intent<'a>(rest: &'a [&'a str]) -> Intent<'a> {
    match rest {
        // 3-element: most-specific first (must precede 2-element --impact arm)
        ["--impact", "--symbol", name] if !name.is_empty() => Intent::ImpactSymbol(name),
        // 2-element flag arms
        ["--impact", path] => Intent::Impact(path),
        ["--callers", sym] if !sym.is_empty() => Intent::Callers(sym),
        ["--callees", sym] if !sym.is_empty() => Intent::Callees(sym),
        ["--symbol", text] if !text.is_empty() => Intent::Symbol(text),
        ["--owners", path] if !path.is_empty() => Intent::Owners(path),
        // 3-element call-chain
        ["--call-chain", from, to] if !from.is_empty() && !to.is_empty() => {
            Intent::CallChain(from, to)
        }
        // Health analytics intents (before free-text arm)
        ["--health"]                              => Intent::Health,
        ["--hotspots"]                            => Intent::Hotspots,
        ["--dead-code"]                           => Intent::DeadCode,
        ["--coupling", path] if !path.is_empty() => Intent::Coupling(path),
        // Flow/community intents
        ["--flows"]                               => Intent::Flows,
        ["--flow", sym] if !sym.is_empty()        => Intent::Flow(sym),
        ["--communities"]                         => Intent::Communities,
        ["--community", id] if !id.is_empty()     => Intent::Community(id),
        ["--graph-diff", a, b] if !a.is_empty() && !b.is_empty() => Intent::GraphDiff(a, b),
        ["--review", spec] if !spec.is_empty()    => Intent::Review(spec),
        ["--similar", sym] if !sym.is_empty()     => Intent::Similar(sym),
        ["--peek", text] if !text.is_empty()      => Intent::Peek(text),
        ["--bridges"]                             => Intent::Bridges,
        ["--context", text] if !text.is_empty()   => Intent::Context(text),
        [text] if !text.is_empty() && !text.starts_with("--") => Intent::Search(text),
        _ => Intent::Ambiguous,
    }
}

/// Route a `lattice query ...` invocation and EMIT the resulting envelope on
/// stdout (the CLI transport). A thin wrapper over `route_value`: compute the
/// envelope + disposition, print it, and map the disposition to the process
/// `Routed` outcome. CLI behavior is byte-identical to before the slice-13
/// refactor.
pub fn route(rest: &[&str]) -> Routed {
    let (value, disposition) = route_value(rest);
    match disposition {
        // Internal failures carry no printable envelope; `main` wraps them.
        Disposition::Internal => Routed::Internal(internal_message(&value)),
        Disposition::Ok => {
            envelope::emit(&value);
            Routed::Ok
        }
        Disposition::Error => {
            envelope::emit(&value);
            Routed::Emitted
        }
    }
}

/// Route a `lattice query ...` invocation and RETURN the envelope instead of
/// printing it. `rest` is the argument slice following the `query` subcommand.
///
/// For `Disposition::Internal` the returned `Value` is a marker envelope
/// carrying the failure message under `error.message`.
pub fn route_value(rest: &[&str]) -> (Value, Disposition) {
    let root = match std::env::current_dir() {
        Ok(r) => r,
        Err(e) => return internal(format!("resolve cwd: {e}")),
    };
    let lattice_dir = root.join(LATTICE_DIR);

    // 1. Guard first: no store means the consumer is greenfield — a structured,
    //    versioned, non-zero signal, evaluated before any dispatch.
    if !lattice_dir.is_dir() {
        eprintln!("lattice: no .lattice/ store at {}", root.display());
        return (
            envelope::error(
                "no_lattice",
                "no .lattice/ found; run `lattice init` first",
            ),
            Disposition::Error,
        );
    }

    // Extract --limit / --exclude-tests / --kind flags from the tail before
    // parsing the intent. An invalid flag returns invalid_flag BEFORE freshness
    // (no work done for an unroutable invocation).
    let (core, filters) = match extract_flags(rest) {
        Ok(pair) => pair,
        Err(msg) => {
            eprintln!("lattice: invalid flag: {msg}");
            return (
                envelope::error("invalid_flag", &msg),
                Disposition::Error,
            );
        }
    };

    // Parse the intent first so the ambiguous shape errors out BEFORE the
    // freshness check runs any work (an unroutable query has nothing to answer).
    let intent = parse_intent(&core);
    let explicit = !matches!(intent, Intent::Search(_)) || filters.file_level;
    if let Intent::Ambiguous = intent {
        eprintln!("lattice: ambiguous query; give --impact <path> or free text");
        return (
            envelope::error("ambiguous_query", "give --impact <path> or free text"),
            Disposition::Error,
        );
    }

    // --kind is only valid for --symbol queries.
    if filters.kind.is_some() {
        let kind_applies = matches!(intent, Intent::Symbol(_));
        if !kind_applies {
            let msg = "--kind is only valid with --symbol queries";
            eprintln!("lattice: invalid flag: {msg}");
            return (
                envelope::error("invalid_flag", msg),
                Disposition::Error,
            );
        }
    }

    // --file-level preserves the legacy vector-only file projection.
    if filters.file_level {
        let file_level_applies = matches!(intent, Intent::Search(_));
        if !file_level_applies {
            let msg = "--file-level is only valid with free-text queries";
            eprintln!("lattice: invalid flag: {msg}");
            return (
                envelope::error("invalid_flag", msg),
                Disposition::Error,
            );
        }
    }

    if filters.all_branches && filters.branch.is_some() {
        let msg = "--branch and --all-branches are mutually exclusive";
        eprintln!("lattice: invalid flag: {msg}");
        return (envelope::error("invalid_flag", msg), Disposition::Error);
    }
    if (filters.all_branches || filters.branch.is_some()) && !matches!(intent, Intent::Symbol(_)) {
        let msg = "--branch and --all-branches are only valid with --symbol";
        eprintln!("lattice: invalid flag: {msg}");
        return (envelope::error("invalid_flag", msg), Disposition::Error);
    }
    if filters.resolution.is_some() || filters.split {
        let ok = matches!(intent, Intent::Communities);
        if !ok {
            let msg = "--resolution and --split are only valid with --communities";
            eprintln!("lattice: invalid flag: {msg}");
            return (envelope::error("invalid_flag", msg), Disposition::Error);
        }
    }

    // --min-evidence / --with-evidence apply only to call-graph queries.
    let call_query = matches!(
        intent,
        Intent::Impact(_)
            | Intent::ImpactSymbol(_)
            | Intent::Callers(_)
            | Intent::Callees(_)
            | Intent::CallChain(_, _)
    );
    if filters.min_evidence.is_some() && !call_query {
        let msg = "--min-evidence is only valid with --impact, --callers, --callees, or --call-chain";
        eprintln!("lattice: invalid flag: {msg}");
        return (
            envelope::error("invalid_flag", msg),
            Disposition::Error,
        );
    }
    if filters.with_evidence {
        let show = matches!(
            intent,
            Intent::ImpactSymbol(_) | Intent::Callers(_) | Intent::Callees(_)
        );
        if !show {
            let msg = "--with-evidence is only valid with --callers, --callees, or --impact --symbol";
            eprintln!("lattice: invalid flag: {msg}");
            return (
                envelope::error("invalid_flag", msg),
                Disposition::Error,
            );
        }
    }

    // Freshness check: after the guard and ahead of
    // ALL dispatch, self-heal any changed/added/removed source file through the
    // same incremental pipeline `init` uses. A refresh failure fails loud — we
    // never answer from a store known to be stale. The resulting Freshness is
    // threaded into every success envelope as the additive `freshness` field.
    let freshness = match freshness::ensure_fresh(&lattice_dir, &root) {
        Ok(f) => f,
        Err(e) => return internal(e.to_string()),
    };

    let (mut env, disp) = match intent {
        // 2. --impact <path> -> graph backend (file scope).
        Intent::Impact(target) => dispatch_impact(&lattice_dir, target, freshness, &filters),
        // --impact --symbol <name> -> symbol-scoped impact.
        Intent::ImpactSymbol(name) => dispatch_impact_symbol(&lattice_dir, name, freshness, &filters),
        // --callers <sym> -> direct callers.
        Intent::Callers(sym) => dispatch_callers(&lattice_dir, sym, freshness, &filters),
        // --callees <sym> -> direct callees.
        Intent::Callees(sym) => dispatch_callees(&lattice_dir, sym, freshness, &filters),
        // --call-chain <from> <to> -> BFS paths.
        Intent::CallChain(from, to) => {
            dispatch_call_chain(&lattice_dir, from, to, freshness, &filters)
        }
        // 3. --symbol <text> -> lexical (FTS5) symbol backend.
        Intent::Symbol(text) => dispatch_symbol(&lattice_dir, text, freshness, &filters),
        // Free text uses ranked hybrid search unless the legacy file view is requested.
        Intent::Search(text) => {
            if filters.file_level {
                dispatch_semantic(&lattice_dir, text, freshness, &filters)
            } else {
                dispatch_hybrid(&lattice_dir, text, freshness, &filters)
            }
        }
        // 5. --owners <path> -> git ownership backend.
        Intent::Owners(path) => dispatch_owners(&lattice_dir, path, freshness, &filters),
        // 6–9. Code-health analytics (read-only over existing stores).
        Intent::Health       => dispatch_health(&lattice_dir, freshness),
        Intent::Hotspots     => dispatch_hotspots(&lattice_dir, freshness, &filters),
        Intent::DeadCode     => dispatch_dead_code(&lattice_dir, freshness, &filters),
        Intent::Coupling(path) => dispatch_coupling(&lattice_dir, path, freshness),
        // 10–13. Flow & community analytics.
        Intent::Flows        => dispatch_flows(&lattice_dir, freshness, &filters),
        Intent::Flow(sym)    => dispatch_flow(&lattice_dir, sym, freshness, &filters),
        Intent::Communities  => dispatch_communities(&lattice_dir, freshness, &filters),
        Intent::Community(id) => dispatch_community(&lattice_dir, id, freshness, &filters),
        Intent::GraphDiff(a, b) => dispatch_graph_diff(&lattice_dir, &root, a, b, freshness),
        Intent::Review(spec) => dispatch_review(&lattice_dir, &root, spec, freshness),
        Intent::Similar(sym) => dispatch_similar(&lattice_dir, sym, freshness, &filters),
        Intent::Peek(text) => dispatch_peek(&lattice_dir, text, freshness, &filters),
        Intent::Bridges => dispatch_bridges(&lattice_dir, freshness, &filters),
        Intent::Context(text) => dispatch_context(&lattice_dir, text, freshness, &filters),
        // unresolvable intent -> ambiguous_query (handled above).
        Intent::Ambiguous => {
            eprintln!("lattice: ambiguous query; give --impact <path>, --callers/--callees/--call-chain <symbol>, or free text");
            (
                envelope::error(
                    "ambiguous_query",
                    "give --impact <path>, --callers/--callees/--call-chain <symbol>, or free text",
                ),
                Disposition::Error,
            )
        }
    };

    // Additive `branch` field on every query envelope (protocol 1.2.0).
    let scope = branch::resolve(&root, filters.all_branches, filters.branch.as_deref());
    if let Some(obj) = env.as_object_mut() {
        obj.insert("branch".to_string(), scope.json());
        if explicit && disp == Disposition::Ok {
            obj.insert("routed_by".to_string(), json!("explicit"));
        }
    }

    // Inject --hints: additive next_tool_suggestions field on success envelopes.
    if filters.hints && disp == Disposition::Ok {
        if let Some(qt) = env.get("query_type").and_then(|v| v.as_str()) {
            let suggestions: Vec<Value> = analysis::next_tools(qt)
                .iter()
                .map(|s| {
                    json!({
                        "tool": s.tool,
                        "description": s.description,
                    })
                })
                .collect();
            if !suggestions.is_empty() {
                env.as_object_mut()
                    .unwrap()
                    .insert("next_tool_suggestions".to_string(), json!(suggestions));
            }
        }
    }

    (env, disp)
}

fn internal(msg: String) -> (Value, Disposition) {
    (envelope::error("internal", &msg), Disposition::Internal)
}

/// Extract the message from an internal marker envelope (for the CLI's
/// `Routed::Internal`, which `main` re-wraps into the same envelope shape).
fn internal_message(value: &Value) -> String {
    value["error"]["message"]
        .as_str()
        .unwrap_or("internal error")
        .to_string()
}

fn dispatch_impact(lattice_dir: &Path, target: &str, freshness: Freshness, filters: &Filters) -> (Value, Disposition) {
    // Normalize a leading `./` so `./auth.rs` matches the stored path.
    let normalized = target.strip_prefix("./").unwrap_or(target);
    let min = filters.min_evidence.unwrap_or(CallEvidence::Name);
    let result = match graph::impact_min(lattice_dir, normalized, min) {
        Ok(r) => r,
        Err(e) => return internal(e.to_string()),
    };
    // --exclude-tests: drop caller/dependent paths whose file class is test/fixture.
    // --limit: cap the node lists (blast_radius stays the TRUE untruncated count).
    let filter_node = |path: &String| -> bool {
        if filters.exclude_tests {
            let c = class::classify(path);
            c == class::SourceClass::Test || c == class::SourceClass::Fixture
        } else {
            false
        }
    };
    let callers: Vec<String> = result
        .callers
        .into_iter()
        .filter(|p| !filter_node(p))
        .take(filters.limit)
        .collect();
    let dependents: Vec<String> = result
        .dependents
        .into_iter()
        .filter(|p| !filter_node(p))
        .take(filters.limit)
        .collect();
    let blast_radius = result.blast_radius; // untruncated
    eprintln!(
        "lattice: impact of {normalized}: {} caller(s), {} dependent(s), blast radius {}",
        callers.len(),
        dependents.len(),
        blast_radius
    );
    let mut env = envelope::impact(
        target,
        &callers,
        &dependents,
        blast_radius,
        freshness.as_str(),
    );
    stamp_min_evidence(&mut env, filters.min_evidence);
    (env, Disposition::Ok)
}

fn dispatch_symbol(lattice_dir: &Path, text: &str, freshness: Freshness, filters: &Filters) -> (Value, Disposition) {
    let root = match lattice_dir.parent() {
        Some(r) => r,
        None => return internal("resolve repo root".into()),
    };
    let scope = branch::resolve(root, filters.all_branches, filters.branch.as_deref());
    let fts_filters = fts::Filters {
        limit: filters.limit,
        exclude_tests: filters.exclude_tests,
        kind: filters.kind.clone(),
    };
    let mut hits = match fts::search(lattice_dir, text, &fts_filters) {
        Ok(h) => h,
        Err(e) => return internal(e.to_string()),
    };

    match &scope {
        branch::Scope::Global => {}
        branch::Scope::Branch(name) => {
            // Live FTS follows the working tree. An explicit other branch, or a
            // catalog that already recorded this branch, is the source of truth.
            let active = branch::active_branch(root);
            let use_catalog = active.as_deref() != Some(name.as_str());
            if use_catalog {
                hits = catalog_symbol_hits(lattice_dir, Some(name), text, filters);
            }
        }
        branch::Scope::All => {
            let extra = catalog_symbol_hits(lattice_dir, None, text, filters);
            let mut seen: BTreeMap<(String, String), ()> = BTreeMap::new();
            for h in &hits {
                seen.insert((h.path.clone(), h.name.clone()), ());
            }
            for h in extra {
                if seen.insert((h.path.clone(), h.name.clone()), ()).is_none() {
                    hits.push(h);
                }
            }
            hits.truncate(filters.limit);
        }
    }

    eprintln!("lattice: symbol query {text:?}: {} hit(s)", hits.len());
    let mut env = envelope::symbol(text, &hits, freshness.as_str());
    attach_parent_spans(&mut env, lattice_dir);
    (env, Disposition::Ok)
}

fn catalog_symbol_hits(
    lattice_dir: &Path,
    branch_name: Option<&str>,
    text: &str,
    filters: &Filters,
) -> Vec<fts::SymbolHit> {
    let found = branch::symbols_named(lattice_dir, branch_name, text).unwrap_or_default();
    let mut hits = Vec::new();
    let mut seen = BTreeMap::new();
    for sym in found {
        if filters.exclude_tests {
            let c = class::classify(&sym.path);
            if c == class::SourceClass::Test || c == class::SourceClass::Fixture {
                continue;
            }
        }
        if let Some(kind) = &filters.kind {
            if &sym.kind != kind {
                continue;
            }
        }
        if seen.insert((sym.path.clone(), sym.name.clone()), ()).is_some() {
            continue;
        }
        hits.push(fts::SymbolHit {
            path: sym.path.clone(),
            name: sym.name,
            kind: sym.kind,
            line: sym.line,
            class: class::classify(&sym.path).as_str().to_string(),
            in_degree: 0,
        });
        if hits.len() >= filters.limit {
            break;
        }
    }
    hits
}

fn dispatch_semantic(
    lattice_dir: &Path,
    text: &str,
    freshness: Freshness,
    filters: &Filters,
) -> (Value, Disposition) {
    // Model-presence guard (B9.3): the semantic backend must embed the query
    // with the local candle model. If the weights are absent, emit the
    // structured `model_not_found` error from this local path check — no fetch
    // code is reachable from `query`, so the invocation stays zero-egress.
    if let Err(embed::EmbedError::ModelNotFound(msg)) = embed::ensure_model_present() {
        eprintln!("lattice: {msg}");
        return (
            envelope::error("model_not_found", &msg),
            Disposition::Error,
        );
    }

    // Non-silent empty-semantic: check whether the store has any documents.
    // With resumable embedding in place this path is unreachable in normal
    // operation, but kept as belt-and-suspenders for the legitimately-empty
    // case (repo with zero embeddable sources) and future regression detection.
    let doc_count = vectors::document_count(lattice_dir);
    if doc_count == 0 {
        eprintln!(
            "lattice: semantic index is empty (0 embedded documents); \
             run `lattice init` to build embeddings"
        );
        if filters.file_level {
            return (
                envelope::semantic_with_class_empty(text, freshness.as_str()),
                Disposition::Ok,
            );
        } else {
            return (
                envelope::semantic_symbols_empty(text, freshness.as_str()),
                Disposition::Ok,
            );
        }
    }

    let hits = match vectors::query(lattice_dir, text) {
        Ok(h) => h,
        Err(e) => return internal(e.to_string()),
    };
    eprintln!("lattice: semantic query {text:?}: {} hit(s)", hits.len());

    // Common filter: --exclude-tests
    let filtered: Vec<_> = hits
        .into_iter()
        .filter(|h| {
            if filters.exclude_tests {
                let c = class::classify(&h.path);
                c != class::SourceClass::Test && c != class::SourceClass::Fixture
            } else {
                true
            }
        })
        .collect();

    if filters.file_level {
        // --file-level: project to best-score-per-PATH then emit old shape.
        let mut best_per_path: std::collections::HashMap<String, (f32, String)> =
            std::collections::HashMap::new();
        for h in &filtered {
            let c = class::classify(&h.path).as_str().to_string();
            let entry = best_per_path
                .entry(h.path.clone())
                .or_insert((h.score, c.clone()));
            if h.score > entry.0 {
                *entry = (h.score, c);
            }
        }
        let mut file_hits: Vec<(String, f32, String)> = best_per_path
            .into_iter()
            .map(|(path, (score, class))| (path, score, class))
            .collect();
        file_hits.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        file_hits.truncate(filters.limit);
        let mut env = envelope::semantic_with_class(text, &file_hits, freshness.as_str());
        attach_parent_spans(&mut env, lattice_dir);
        (env, Disposition::Ok)
    } else {
        // Default: symbol-granular shape {path, name, kind, start_line, end_line, score, class}
        let symbol_hits: Vec<(String, String, String, usize, usize, f32, String)> = filtered
            .into_iter()
            .take(filters.limit)
            .map(|h| {
                let c = class::classify(&h.path).as_str().to_string();
                (h.path, h.symbol, h.kind, h.start_line, h.end_line, h.score, c)
            })
            .collect();
        let mut env = envelope::semantic_symbols(text, &symbol_hits, freshness.as_str());
        attach_parent_spans(&mut env, lattice_dir);
        (env, Disposition::Ok)
    }
}

/// Add `parent` to split Markdown section hits. Code hits and unsplit sections
/// are left untouched, so their default shape stays byte-identical.
fn attach_parent_spans(env: &mut Value, lattice_dir: &Path) {
    let Some(root) = lattice_dir.parent() else {
        return;
    };
    let Some(hits) = env.get_mut("hits").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for hit in hits {
        let Some(obj) = hit.as_object_mut() else {
            continue;
        };
        if obj.get("kind").and_then(|v| v.as_str()) != Some("section") {
            continue;
        }
        let Some(name) = obj.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if markdown::part_base(name).is_none() {
            continue;
        }
        let Some(path) = obj.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        let stem = path
            .rsplit('/')
            .next()
            .and_then(|f| f.rsplit_once('.').map(|(s, _)| s))
            .unwrap_or(path);
        let Ok(text) = std::fs::read_to_string(root.join(path)) else {
            continue;
        };
        let Some(parent) = markdown::parent_span(&text, stem, name) else {
            continue;
        };
        obj.insert(
            "parent".to_string(),
            json!({
                "name": parent.name,
                "start_line": parent.start_line,
                "end_line": parent.end_line,
            }),
        );
    }
}

// ── Symbol-level structural dispatch ─────────────────────────────────────────

/// Convert a slice of `SymbolNode` to the `(path, name, line)` tuples used by
/// the envelope builders.
fn nodes_to_tuples(nodes: &[graph::SymbolNode]) -> Vec<(String, String, usize)> {
    nodes
        .iter()
        .map(|n| (n.path.clone(), n.name.clone(), n.line))
        .collect()
}

fn dispatch_callers(
    lattice_dir: &Path,
    sym: &str,
    freshness: Freshness,
    filters: &Filters,
) -> (Value, Disposition) {
    let cg = match graph::load_call_graph(lattice_dir) {
        Ok(cg) => cg,
        Err(e) => return internal(e.to_string()),
    };
    let resolved_nodes = graph::resolve_symbol(&cg, sym);
    let resolved_tuples = nodes_to_tuples(&resolved_nodes.iter().map(|n| (*n).clone()).collect::<Vec<_>>());
    let target_ids: std::collections::BTreeSet<String> =
        resolved_nodes.iter().map(|n| n.id.clone()).collect();
    let min = filters.min_evidence.unwrap_or(CallEvidence::Name);
    let hits = graph::callers_min(&cg, &target_ids, min);
    let hit_nodes: Vec<graph::SymbolNode> = hits.iter().map(|(n, _)| n.clone()).collect();
    let hit_tuples = nodes_to_tuples(&hit_nodes);
    eprintln!(
        "lattice: callers of {sym:?}: {} resolved, {} hit(s)",
        resolved_tuples.len(),
        hit_tuples.len()
    );
    let mut env = envelope::callers(sym, &resolved_tuples, &hit_tuples, freshness.as_str());
    if filters.with_evidence {
        let levels: Vec<&str> = hits.iter().map(|(_, ev)| ev.as_str()).collect();
        stamp_hit_evidence(&mut env, "hits", &levels);
    }
    stamp_min_evidence(&mut env, filters.min_evidence);
    (env, Disposition::Ok)
}

fn dispatch_callees(
    lattice_dir: &Path,
    sym: &str,
    freshness: Freshness,
    filters: &Filters,
) -> (Value, Disposition) {
    let cg = match graph::load_call_graph(lattice_dir) {
        Ok(cg) => cg,
        Err(e) => return internal(e.to_string()),
    };
    let resolved_nodes = graph::resolve_symbol(&cg, sym);
    let resolved_tuples = nodes_to_tuples(&resolved_nodes.iter().map(|n| (*n).clone()).collect::<Vec<_>>());
    let target_ids: std::collections::BTreeSet<String> =
        resolved_nodes.iter().map(|n| n.id.clone()).collect();
    let min = filters.min_evidence.unwrap_or(CallEvidence::Name);
    let hits = graph::callees_min(&cg, &target_ids, min);
    let hit_nodes: Vec<graph::SymbolNode> = hits.iter().map(|(n, _)| n.clone()).collect();
    let hit_tuples = nodes_to_tuples(&hit_nodes);
    eprintln!(
        "lattice: callees of {sym:?}: {} resolved, {} hit(s)",
        resolved_tuples.len(),
        hit_tuples.len()
    );
    let mut env = envelope::callees(sym, &resolved_tuples, &hit_tuples, freshness.as_str());
    if filters.with_evidence {
        let levels: Vec<&str> = hits.iter().map(|(_, ev)| ev.as_str()).collect();
        stamp_hit_evidence(&mut env, "hits", &levels);
    }
    stamp_min_evidence(&mut env, filters.min_evidence);
    (env, Disposition::Ok)
}

fn dispatch_call_chain(
    lattice_dir: &Path,
    from: &str,
    to: &str,
    freshness: Freshness,
    filters: &Filters,
) -> (Value, Disposition) {
    let cg = match graph::load_call_graph(lattice_dir) {
        Ok(cg) => cg,
        Err(e) => return internal(e.to_string()),
    };
    let from_nodes = graph::resolve_symbol(&cg, from);
    let to_nodes = graph::resolve_symbol(&cg, to);
    let from_tuples = nodes_to_tuples(&from_nodes.iter().map(|n| (*n).clone()).collect::<Vec<_>>());
    let to_tuples = nodes_to_tuples(&to_nodes.iter().map(|n| (*n).clone()).collect::<Vec<_>>());
    let from_ids: std::collections::BTreeSet<String> =
        from_nodes.iter().map(|n| n.id.clone()).collect();
    let to_ids: std::collections::BTreeSet<String> =
        to_nodes.iter().map(|n| n.id.clone()).collect();
    let min = filters.min_evidence.unwrap_or(CallEvidence::Name);
    let paths = graph::call_chain_min(&cg, &from_ids, &to_ids, min);
    // Convert paths to tuple form
    let path_tuples: Vec<Vec<(String, String, usize)>> = paths
        .iter()
        .map(|path| nodes_to_tuples(path))
        .collect();
    eprintln!(
        "lattice: call-chain {from:?} -> {to:?}: {} path(s)",
        path_tuples.len()
    );
    let mut env = envelope::call_chain(from, to, &from_tuples, &to_tuples, &path_tuples, freshness.as_str());
    stamp_min_evidence(&mut env, filters.min_evidence);
    (env, Disposition::Ok)
}

fn dispatch_impact_symbol(
    lattice_dir: &Path,
    sym: &str,
    freshness: Freshness,
    filters: &Filters,
) -> (Value, Disposition) {
    let _ = filters; // --limit/--exclude-tests on impact_symbol: noted as future work
    let cg = match graph::load_call_graph(lattice_dir) {
        Ok(cg) => cg,
        Err(e) => return internal(e.to_string()),
    };
    let resolved_nodes = graph::resolve_symbol(&cg, sym);
    let resolved_tuples = nodes_to_tuples(&resolved_nodes.iter().map(|n| (*n).clone()).collect::<Vec<_>>());
    let target_ids: std::collections::BTreeSet<String> =
        resolved_nodes.iter().map(|n| n.id.clone()).collect();
    let min = filters.min_evidence.unwrap_or(CallEvidence::Name);
    let direct_hits = graph::callers_min(&cg, &target_ids, min);
    let direct: Vec<graph::SymbolNode> = direct_hits.iter().map(|(n, _)| n.clone()).collect();
    let transitive = graph::transitive_callers_min(&cg, &target_ids, min);
    let blast_radius = transitive.len();
    let direct_tuples = nodes_to_tuples(&direct);
    let transitive_tuples = nodes_to_tuples(&transitive);
    eprintln!(
        "lattice: symbol impact of {sym:?}: {} resolved, {} direct caller(s), {} transitive, blast radius {blast_radius}",
        resolved_tuples.len(),
        direct_tuples.len(),
        transitive_tuples.len(),
    );
    let mut env = envelope::impact_symbol(
        sym,
        &resolved_tuples,
        &direct_tuples,
        &transitive_tuples,
        blast_radius,
        freshness.as_str(),
    );
    if filters.with_evidence {
        let levels: Vec<&str> = direct_hits.iter().map(|(_, ev)| ev.as_str()).collect();
        stamp_hit_evidence(&mut env, "callers", &levels);
    }
    stamp_min_evidence(&mut env, filters.min_evidence);
    (env, Disposition::Ok)
}

fn stamp_min_evidence(env: &mut Value, min: Option<CallEvidence>) {
    if let Some(level) = min {
        if let Some(obj) = env.as_object_mut() {
            obj.insert(
                "min_evidence".to_string(),
                Value::String(level.as_str().to_string()),
            );
        }
    }
}

fn stamp_hit_evidence(env: &mut Value, field: &str, levels: &[&str]) {
    let Some(arr) = env.get_mut(field).and_then(|v| v.as_array_mut()) else {
        return;
    };
    for (hit, level) in arr.iter_mut().zip(levels) {
        if let Some(obj) = hit.as_object_mut() {
            obj.insert(
                "evidence".to_string(),
                Value::String((*level).to_string()),
            );
        }
    }
}

// ── Code-health analytics dispatch ────────────────────────────────────────────

fn dispatch_dead_code(
    lattice_dir: &Path,
    freshness: Freshness,
    filters: &Filters,
) -> (Value, Disposition) {
    let cg = match graph::load_call_graph(lattice_dir) {
        Ok(cg) => cg,
        Err(e) => return internal(e.to_string()),
    };
    let all_hits = health::dead_code(&cg);
    let count = all_hits.len(); // untruncated
    let truncated: Vec<(String, String, usize, String)> = all_hits
        .into_iter()
        .take(filters.limit)
        .map(|h| (h.path, h.name, h.line, h.id))
        .collect();
    eprintln!("lattice: dead-code: {} candidate(s)", count);
    (
        envelope::dead_code(&truncated, count, freshness.as_str()),
        Disposition::Ok,
    )
}

fn dispatch_hotspots(
    lattice_dir: &Path,
    freshness: Freshness,
    filters: &Filters,
) -> (Value, Disposition) {
    let cg = match graph::load_call_graph(lattice_dir) {
        Ok(cg) => cg,
        Err(e) => return internal(e.to_string()),
    };
    let view = match graph::read(lattice_dir) {
        Ok(v) => v,
        Err(e) => return internal(e.to_string()),
    };
    let churn_map = match ownership::churn_map(lattice_dir) {
        Ok(m) => m,
        Err(e) => return internal(e.to_string()),
    };
    let churn_weighted = ownership::git_stats_have_rows(lattice_dir);
    let degrees = health::file_degrees(&cg, &view);
    let entries = health::hotspots(&degrees, &churn_map, filters.limit);

    // --with-criticality: compute per-file max criticality from flows.
    let mut file_crit: BTreeMap<String, i64> = BTreeMap::new();
    if filters.hints {  // reuse hints flag as criticality flag for now
        let flow_list = analysis::flows(&cg, &view, &churn_map);
        for flow in &flow_list {
            for member_id in &flow.members {
                if let Some(node) = cg.nodes.get(member_id) {
                    let cur = file_crit.entry(node.path.clone()).or_insert(0);
                    if flow.criticality > *cur {
                        *cur = flow.criticality;
                    }
                }
            }
        }
    }

    let hotspot_values: Vec<Value> = entries
        .iter()
        .map(|e| {
            let mut obj = json!({
                "path": e.path,
                "imports_in": e.imports_in,
                "imports_out": e.imports_out,
                "calls_in": e.calls_in,
                "calls_out": e.calls_out,
                "degree": e.degree,
                "churn": e.churn,
                "score": e.score,
            });
            if let Some(&crit) = file_crit.get(&e.path) {
                obj.as_object_mut().unwrap().insert(
                    "max_criticality".to_string(),
                    json!(crit),
                );
            }
            obj
        })
        .collect();
    eprintln!("lattice: hotspots: {} file(s)", entries.len());
    (
        envelope::hotspots(&hotspot_values, churn_weighted, freshness.as_str()),
        Disposition::Ok,
    )
}

fn dispatch_coupling(
    lattice_dir: &Path,
    target: &str,
    freshness: Freshness,
) -> (Value, Disposition) {
    let normalized = target.strip_prefix("./").unwrap_or(target);
    let view = match graph::read(lattice_dir) {
        Ok(v) => v,
        Err(e) => return internal(e.to_string()),
    };
    let c = health::coupling(&view, normalized);
    eprintln!(
        "lattice: coupling of {normalized}: afferent {}, efferent {}",
        c.afferent_files.len(),
        c.efferent_files.len()
    );
    (
        envelope::coupling(target, &c.afferent_files, &c.efferent_files, freshness.as_str()),
        Disposition::Ok,
    )
}

fn dispatch_health(
    lattice_dir: &Path,
    freshness: Freshness,
) -> (Value, Disposition) {
    let cg = match graph::load_call_graph(lattice_dir) {
        Ok(cg) => cg,
        Err(e) => return internal(e.to_string()),
    };
    let view = match graph::read(lattice_dir) {
        Ok(v) => v,
        Err(e) => return internal(e.to_string()),
    };
    let churn_map = match ownership::churn_map(lattice_dir) {
        Ok(m) => m,
        Err(e) => return internal(e.to_string()),
    };
    let churn_weighted = ownership::git_stats_have_rows(lattice_dir);

    let files = view.files.len();
    let functions = cg.nodes.len();
    let call_edges: usize = cg.out.values().map(|s| s.len()).sum();
    let import_edges = view.imports.len();
    let avg_coupling = if files == 0 {
        0.0
    } else {
        2.0 * import_edges as f64 / files as f64
    };

    let degrees = health::file_degrees(&cg, &view);
    let hotspot_entries = health::hotspots(&degrees, &churn_map, health::HEALTH_HOTSPOT_LIMIT);

    let all_dead = health::dead_code(&cg);
    let dead_count = all_dead.len();
    let dead_hits: Vec<(String, String, usize, String)> = all_dead
        .into_iter()
        .take(DEFAULT_LIMIT)
        .map(|h| (h.path, h.name, h.line, h.id))
        .collect();

    let god_entries_raw = health::god_classes(&cg, health::GOD_CLASS_THRESHOLD);
    let god_entries: Vec<(String, String, usize)> = god_entries_raw
        .iter()
        .map(|g| (g.path.clone(), g.class_name.clone(), g.methods))
        .collect();

    eprintln!(
        "lattice: health: {} files, {} functions, {} call edges, {} import edges",
        files, functions, call_edges, import_edges
    );
    (
        envelope::health(
            files,
            functions,
            call_edges,
            import_edges,
            avg_coupling,
            &hotspot_entries,
            &dead_hits,
            dead_count,
            health::GOD_CLASS_THRESHOLD,
            &god_entries,
            churn_weighted,
            freshness.as_str(),
        ),
        Disposition::Ok,
    )
}

/// Build the RRF fusion key for a hit.
///
/// Per design §1.2, two separate rules collapse to bare `path`:
/// 1. Vector: `symbol == ""` (whole-file fallback span, kind "file")
/// 2. FTS: `kind == "module"` or `kind == "file"` (module row, line 1)
///
/// All other hits use `"{path}::{name}"`.
fn fusion_key_vec(path: &str, symbol: &str, kind: &str) -> String {
    if symbol.is_empty() || kind == "file" {
        path.to_string()
    } else {
        format!("{path}::{symbol}")
    }
}

fn fusion_key_fts(path: &str, name: &str, kind: &str) -> String {
    if kind == "module" || kind == "file" {
        path.to_string()
    } else {
        format!("{path}::{name}")
    }
}

fn dispatch_hybrid(
    lattice_dir: &Path,
    text: &str,
    freshness: Freshness,
    filters: &Filters,
) -> (Value, Disposition) {
    // Model-presence guard (same as dispatch_semantic).
    if let Err(embed::EmbedError::ModelNotFound(msg)) = embed::ensure_model_present() {
        eprintln!("lattice: {msg}");
        return (
            envelope::error("model_not_found", &msg),
            Disposition::Error,
        );
    }

    let (mut hits, fts_hits_count, vec_hits_count) =
        match ranked_hybrid(lattice_dir, text, filters, true) {
            Ok(results) => results,
            Err(e) => return internal(e.to_string()),
        };
    hits.truncate(filters.limit);

    let envelope_hits: Vec<_> = hits
        .into_iter()
        .map(|h| {
            (
                h.path,
                h.name,
                h.kind,
                h.start_line,
                h.end_line,
                h.class,
                h.fused_score,
                h.sources,
                h.fts_rank,
                h.vector_rank,
                h.vector_score,
                h.in_degree,
            )
        })
        .collect();

    eprintln!(
        "lattice: hybrid query {text:?}: {} FTS hit(s), {} vector hit(s), {} fused hit(s)",
        fts_hits_count,
        vec_hits_count,
        envelope_hits.len()
    );

    let mut env = envelope::hybrid(text, &envelope_hits, freshness.as_str());
    if vec_hits_count == 0 && vectors::document_count(lattice_dir) == 0 {
        eprintln!("lattice: semantic index is empty");
        env["vectors_empty"] = json!(true);
    }
    attach_parent_spans(&mut env, lattice_dir);
    (env, Disposition::Ok)
}

struct HybridHit {
    key: String,
    path: String,
    name: String,
    kind: String,
    start_line: usize,
    end_line: Option<usize>,
    class: String,
    fused_score: f64,
    sources: Vec<String>,
    fts_rank: Option<usize>,
    vector_rank: Option<usize>,
    vector_score: Option<f32>,
    in_degree: usize,
}

fn ranked_hybrid(
    lattice_dir: &Path,
    text: &str,
    filters: &Filters,
    use_vectors: bool,
) -> crate::Result<(Vec<HybridHit>, usize, usize)> {
    // FTS backend at FUSION_DEPTH.
    let fts_filters = fts::Filters {
        limit: FUSION_DEPTH,
        exclude_tests: filters.exclude_tests,
        kind: None, // hybrid is kind-agnostic on the input side
    };
    let fts_hits = fts::search(lattice_dir, text, &fts_filters)?;

    // Vector backend at SEMANTIC_LIMIT (already 20 = FUSION_DEPTH).
    let vec_hits = if use_vectors {
        vectors::query(lattice_dir, text)?
    } else {
        Vec::new()
    };

    // Apply --exclude-tests to vector hits (same as dispatch_semantic).
    let vec_hits: Vec<_> = vec_hits
        .into_iter()
        .filter(|h| {
            if filters.exclude_tests {
                let c = class::classify(&h.path);
                c != class::SourceClass::Test && c != class::SourceClass::Fixture
            } else {
                true
            }
        })
        .collect();
    let vec_hits_count = vec_hits.len();
    let fts_hits_count = fts_hits.len();

    // Build key lists for RRF.
    let fts_keys: Vec<String> = fts_hits
        .iter()
        .map(|h| fusion_key_fts(&h.path, &h.name, &h.kind))
        .collect();
    let vec_keys: Vec<String> = vec_hits
        .iter()
        .map(|h| fusion_key_vec(&h.path, &h.symbol, &h.kind))
        .collect();

    let fused = fuse::rrf(&[fts_keys.clone(), vec_keys.clone()], fuse::RRF_K);

    // Build lookup maps for hit details by fusion key (first-occurrence wins for
    // same-key duplicates, matching the RRF first-wins duplicate rule).
    let mut fts_map: std::collections::HashMap<String, &fts::SymbolHit> =
        std::collections::HashMap::new();
    for (hit, key) in fts_hits.iter().zip(fts_keys.iter()) {
        fts_map.entry(key.clone()).or_insert(hit);
    }
    let mut vec_map: std::collections::HashMap<String, &vectors::Hit> =
        std::collections::HashMap::new();
    for (hit, key) in vec_hits.iter().zip(vec_keys.iter()) {
        vec_map.entry(key.clone()).or_insert(hit);
    }

    // Bulk-fetch in_degree for vector-only hits (FTS hits already carry in_degree).
    let vec_only_keys: Vec<(String, String)> = fused
        .iter()
        .filter(|f| f.ranks[0].is_none() && f.ranks[1].is_some())
        .filter_map(|f| vec_map.get(&f.key).map(|h| (h.path.clone(), h.symbol.clone())))
        .filter(|(_, sym)| !sym.is_empty())
        .collect();
    let extra_degrees = fts::in_degrees(lattice_dir, &vec_only_keys)?;

    // Build the ranked hit list with secondary sort by in_degree DESC, then
    // name ASC, path ASC, start_line ASC (per design §1.1 final ordering).
    // The fused list is already score-sorted; we need a stable sort that
    // applies in_degree as first tie-break.
    //
    // Strategy: collect all data, then sort once with the full comparator.
    let mut hits: Vec<HybridHit> = fused
        .iter()
        .map(|f| {
            let fts_hit = fts_map.get(&f.key);
            let vec_hit = vec_map.get(&f.key);
            let fts_rank = f.ranks[0];
            let vector_rank = f.ranks[1];

            // Name, kind, path: prefer vector span (richer); fall back to FTS.
            let (path, name, kind, start_line, end_line, class) = match (vec_hit, fts_hit) {
                (Some(v), _) => {
                    let c = class::classify(&v.path).as_str().to_string();
                    let end_line = if v.end_line > 0 { Some(v.end_line) } else { None };
                    // For whole-file fallback spans (symbol == ""), use path as name.
                    let display_name = if v.symbol.is_empty() {
                        v.path.clone()
                    } else {
                        v.symbol.clone()
                    };
                    (v.path.clone(), display_name, v.kind.clone(), v.start_line, end_line, c)
                }
                (None, Some(h)) => {
                    let c = h.class.clone();
                    (h.path.clone(), h.name.clone(), h.kind.clone(), h.line, None, c)
                }
                (None, None) => {
                    // Fusion key with no hit data — should not happen; use key as path.
                    (f.key.clone(), f.key.clone(), "unknown".to_string(), 0, None, "source".to_string())
                }
            };

            // sources list in fixed order: fts first, then vector.
            let mut sources: Vec<String> = Vec::new();
            if fts_rank.is_some() { sources.push("fts".to_string()); }
            if vector_rank.is_some() { sources.push("vector".to_string()); }

            let vector_score = vec_hit.map(|v| v.score);

            // in_degree: from FTS hit if available, else from the bulk lookup.
            let in_degree = if let Some(fh) = fts_hit {
                fh.in_degree
            } else {
                let key = format!("{path}::{name}");
                extra_degrees.get(&key).copied().unwrap_or(0)
            };

            HybridHit {
                key: f.key.clone(),
                path,
                name,
                kind,
                start_line,
                end_line,
                class,
                fused_score: f.score,
                sources,
                fts_rank,
                vector_rank,
                vector_score,
                in_degree,
            }
        })
        .collect();

    // Final ordering: fused_score DESC, in_degree DESC, name ASC, path ASC, start_line ASC.
    hits.sort_by(|a, b| {
        b.fused_score
            .partial_cmp(&a.fused_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.in_degree.cmp(&a.in_degree))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.start_line.cmp(&b.start_line))
    });

    Ok((hits, fts_hits_count, vec_hits_count))
}

fn dispatch_owners(
    lattice_dir: &Path,
    target: &str,
    freshness: Freshness,
    _filters: &Filters,
) -> (Value, Disposition) {
    let normalized = target.strip_prefix("./").unwrap_or(target);
    let report = match ownership::owners(lattice_dir, normalized) {
        Ok(r) => r,
        Err(e) => return internal(e.to_string()),
    };
    eprintln!(
        "lattice: owners of {normalized}: {} author(s), churn {}",
        report.owners.len(),
        report.churn
    );
    // Convert OwnersReport to raw tuples for the envelope builder.
    let owner_list: Vec<(String, f64, String)> = report
        .owners
        .iter()
        .map(|o| (o.author.clone(), o.share, o.last_touched.clone()))
        .collect();
    (
        envelope::owners(target, &owner_list, report.churn, &report.co_changed, freshness.as_str()),
        Disposition::Ok,
    )
}

// ── Flow dispatch ─────────────────────────────────────────────────────────────

fn dispatch_flows(
    lattice_dir: &Path,
    freshness: Freshness,
    _filters: &Filters,
) -> (Value, Disposition) {
    let cg = match graph::load_call_graph(lattice_dir) {
        Ok(cg) => cg,
        Err(e) => return internal(e.to_string()),
    };
    let view = match graph::read(lattice_dir) {
        Ok(v) => v,
        Err(e) => return internal(e.to_string()),
    };
    let churn_map = match ownership::churn_map(lattice_dir) {
        Ok(m) => m,
        Err(e) => return internal(e.to_string()),
    };

    let flow_list = analysis::flows(&cg, &view, &churn_map);
    let count = flow_list.len();
    eprintln!("lattice: flows: {count} execution flow(s) detected");
    (envelope::flows(&flow_list, freshness.as_str()), Disposition::Ok)
}

fn dispatch_flow(
    lattice_dir: &Path,
    symbol: &str,
    freshness: Freshness,
    _filters: &Filters,
) -> (Value, Disposition) {
    let cg = match graph::load_call_graph(lattice_dir) {
        Ok(cg) => cg,
        Err(e) => return internal(e.to_string()),
    };

    // Resolve symbol to a node id.
    let resolved = graph::resolve_symbol(&cg, symbol);
    if resolved.is_empty() {
        return (
            envelope::error("not_found", &format!("no function matching '{symbol}'")),
            Disposition::Error,
        );
    }
    let entry_id = &resolved[0].id;
    let members = analysis::trace_flow(&cg, entry_id);
    let churn_map = match ownership::churn_map(lattice_dir) {
        Ok(m) => m,
        Err(e) => return internal(e.to_string()),
    };
    let crit = analysis::score_criticality(&cg, &churn_map, &members);

    let flow = analysis::Flow {
        entry: entry_id.clone(),
        chain: analysis::trace_chain(&cg, entry_id),
        members,
        criticality: crit,
    };
    eprintln!("lattice: flow of {symbol}: {} member(s)", flow.members.len());
    (envelope::flow(&flow, freshness.as_str()), Disposition::Ok)
}

// ── Community dispatch ────────────────────────────────────────────────────────

fn dispatch_communities(
    lattice_dir: &Path,
    freshness: Freshness,
    filters: &Filters,
) -> (Value, Disposition) {
    let cg = match graph::load_call_graph(lattice_dir) {
        Ok(cg) => cg,
        Err(e) => return internal(e.to_string()),
    };

    let resolution = filters.resolution.unwrap_or(crate::leiden::DEFAULT_RESOLUTION);
    let report = analysis::communities_report(&cg, resolution, filters.split);
    let count = report.communities.len();
    eprintln!("lattice: communities: {count} community(ies) detected");
    (envelope::communities(&report, freshness.as_str()), Disposition::Ok)
}

fn dispatch_community(
    lattice_dir: &Path,
    id_or_name: &str,
    freshness: Freshness,
    _filters: &Filters,
) -> (Value, Disposition) {
    let cg = match graph::load_call_graph(lattice_dir) {
        Ok(cg) => cg,
        Err(e) => return internal(e.to_string()),
    };

    let comms = analysis::communities(&cg);

    // Try to find by id (numeric) first, then by name.
    let found = if let Ok(id) = id_or_name.parse::<usize>() {
        comms.iter().find(|c| c.id == id)
    } else {
        comms.iter().find(|c| c.name == id_or_name)
    };

    match found {
        Some(comm) => {
            eprintln!(
                "lattice: community '{}': {} member(s)",
                comm.name,
                comm.members.len()
            );
            (envelope::community(comm, freshness.as_str()), Disposition::Ok)
        }
        None => (
            envelope::error(
                "not_found",
                &format!("no community matching '{id_or_name}'"),
            ),
            Disposition::Error,
        ),
    }
}

fn dispatch_graph_diff(
    lattice_dir: &Path,
    root: &Path,
    from: &str,
    to: &str,
    freshness: Freshness,
) -> (Value, Disposition) {
    let _ = root;
    let cg = match graph::load_call_graph(lattice_dir) {
        Ok(cg) => cg,
        Err(e) => return internal(e.to_string()),
    };
    let from_m = match snapshot::resolve(lattice_dir, &cg, from) {
        Ok(m) => m,
        Err(e) => {
            return (
                envelope::error("not_found", &e.to_string()),
                Disposition::Error,
            )
        }
    };
    let to_m = match snapshot::resolve(lattice_dir, &cg, to) {
        Ok(m) => m,
        Err(e) => {
            return (
                envelope::error("not_found", &e.to_string()),
                Disposition::Error,
            )
        }
    };
    let diff = snapshot::diff(&from_m, &to_m);
    eprintln!(
        "lattice: graph-diff {from}..{to}: +{} -{} nodes, +{} -{} edges",
        diff.nodes_added.len(),
        diff.nodes_removed.len(),
        diff.edges_added.len(),
        diff.edges_removed.len()
    );
    (
        envelope::graph_diff_query(from, to, &diff, freshness.as_str()),
        Disposition::Ok,
    )
}

fn dispatch_review(
    lattice_dir: &Path,
    root: &Path,
    spec: &str,
    freshness: Freshness,
) -> (Value, Disposition) {
    let cg = match graph::load_call_graph(lattice_dir) {
        Ok(cg) => cg,
        Err(e) => return internal(e.to_string()),
    };
    match review::review(root, lattice_dir, &cg, spec) {
        Ok(report) => {
            eprintln!(
                "lattice: review {spec}: {} changed file(s), {} affected symbol(s)",
                report.changed.len(),
                report.affected.len()
            );
            (
                envelope::review(spec, &report, freshness.as_str()),
                Disposition::Ok,
            )
        }
        Err(e) => {
            let msg = e.to_string();
            let code = if msg.contains("unresolved ref") {
                "unresolved_ref"
            } else {
                "invalid_flag"
            };
            (envelope::error(code, &msg), Disposition::Error)
        }
    }
}

fn dispatch_similar(
    lattice_dir: &Path,
    symbol: &str,
    freshness: Freshness,
    filters: &Filters,
) -> (Value, Disposition) {
    match vectors::similar(lattice_dir, symbol, filters.limit) {
        Ok(hits) => {
            let projected: Vec<_> = hits
                .into_iter()
                .filter(|h| {
                    if filters.exclude_tests {
                        let c = class::classify(&h.path);
                        c != class::SourceClass::Test && c != class::SourceClass::Fixture
                    } else {
                        true
                    }
                })
                .map(|h| {
                    (
                        h.path.clone(),
                        h.symbol.clone(),
                        h.kind.clone(),
                        h.start_line,
                        h.end_line,
                        h.score,
                        class::classify(&h.path).as_str().to_string(),
                    )
                })
                .collect();
            eprintln!("lattice: similar to {symbol}: {} hit(s)", projected.len());
            (
                envelope::similar(symbol, &projected, freshness.as_str()),
                Disposition::Ok,
            )
        }
        Err(e) => {
            let msg = e.to_string();
            let code = if msg.contains("not_found") {
                "not_found"
            } else if msg.contains("ambiguous") {
                "ambiguous_query"
            } else {
                "internal"
            };
            if code == "internal" {
                return internal(msg);
            }
            (envelope::error(code, &msg), Disposition::Error)
        }
    }
}

const CONTEXT_SEED_LIMIT: usize = 5;
const CONTEXT_NEIGHBOR_LIMIT: usize = 4;

fn dispatch_context(
    lattice_dir: &Path,
    text: &str,
    freshness: Freshness,
    filters: &Filters,
) -> (Value, Disposition) {
    // A missing model leaves the lexical ranking available.
    let use_vectors = embed::ensure_model_present().is_ok();
    let (ranked, _, _) = match ranked_hybrid(lattice_dir, text, filters, use_vectors) {
        Ok(results) => results,
        Err(e) => return internal(e.to_string()),
    };

    let cg = match graph::load_call_graph(lattice_dir) {
        Ok(cg) => cg,
        Err(e) => return internal(e.to_string()),
    };
    let hits = expand_context(ranked, &cg, filters);
    eprintln!("lattice: context {text:?}: {} hit(s)", hits.len());
    (envelope::context(&hits, freshness.as_str()), Disposition::Ok)
}

fn expand_context(
    ranked: Vec<HybridHit>,
    cg: &graph::CallGraph,
    filters: &Filters,
) -> Vec<envelope::ContextHit> {
    let mut nodes_by_location = BTreeMap::new();
    for node in cg.nodes.values() {
        nodes_by_location
            .entry((node.path.as_str(), node.name.as_str(), node.line))
            .and_modify(|id| *id = None)
            .or_insert(Some(node.id.as_str()));
    }

    let neighbor_budget = CONTEXT_NEIGHBOR_LIMIT.min(filters.limit / 2);
    let head_len = ranked.len().min(filters.limit - neighbor_budget);
    let mut seeds = Vec::new();
    let mut hits = Vec::new();
    let mut by_id = BTreeMap::new();
    for (rank, hit) in ranked.into_iter().enumerate() {
        let graph_id = if matches!(hit.kind.as_str(), "function" | "method") {
            nodes_by_location
                .get(&(hit.path.as_str(), hit.name.as_str(), hit.start_line))
                .copied()
                .flatten()
        } else {
            None
        };
        if rank < head_len.min(CONTEXT_SEED_LIMIT) {
            seeds.extend(graph_id);
        }
        let id = graph_id.map(str::to_owned).unwrap_or(hit.key);
        by_id.insert(id.clone(), hits.len());
        hits.push(envelope::ContextHit {
            id,
            path: hit.path,
            name: hit.name,
            kind: hit.kind,
            line: hit.start_line,
            matched_by: hit.sources,
            evidence: None,
        });
    }

    // Protect the search prefix. Lower-ranked candidates may fill graph slots
    // without losing their retrieval provenance. Visit edges in seed order,
    // with outgoing edges before incoming edges.
    let mut expanded = BTreeMap::<&str, usize>::new();
    let mut neighbors = Vec::<(&graph::SymbolNode, Option<usize>, Option<&CallSite>)>::new();
    for seed in seeds {
        let outgoing = cg.out.get(seed).into_iter().flatten().map(|id| (id, true));
        let incoming = cg.inn.get(seed).into_iter().flatten().map(|id| (id, false));
        for (id, outgoing) in outgoing.chain(incoming) {
            if id == seed {
                continue;
            }
            let Some(other) = cg.nodes.get(id) else {
                continue;
            };
            if filters.exclude_tests
                && matches!(
                    class::classify(&other.path),
                    class::SourceClass::Test | class::SourceClass::Fixture
                )
            {
                continue;
            }

            let index = by_id.get(id).copied();
            // Once the budget is full, only inspect edges that can annotate a
            // retrieved hit or add missing evidence to a selected neighbor.
            if index.is_none()
                && neighbors.len() == neighbor_budget
                && !expanded.contains_key(id.as_str())
            {
                continue;
            }
            let key = if outgoing {
                (seed.to_owned(), id.clone())
            } else {
                (id.clone(), seed.to_owned())
            };
            let site = cg.sites.get(&key);
            if let Some(index) = index {
                let hit = &mut hits[index];
                if !hit.matched_by.iter().any(|tag| tag == "graph") {
                    hit.matched_by.push("graph".to_owned());
                    hit.matched_by.sort();
                }
                if hit.evidence.is_none() {
                    hit.evidence = site.cloned();
                }
            }
            if let Some(&slot) = expanded.get(id.as_str()) {
                if neighbors[slot].2.is_none() {
                    neighbors[slot].2 = site;
                }
            } else if neighbors.len() < neighbor_budget && index.is_none_or(|i| i >= head_len) {
                expanded.insert(id.as_str(), neighbors.len());
                neighbors.push((other, index, site));
            }
        }
    }

    let retrieval_budget = filters.limit - neighbors.len();
    let mut hits: Vec<_> = hits.into_iter().map(Some).collect();
    let neighbors: Vec<_> = neighbors
        .into_iter()
        .map(|(other, index, site)| {
            index.and_then(|i| hits[i].take()).unwrap_or_else(|| {
                let kind = if other.parent.is_some() { "method" } else { "function" };
                envelope::ContextHit {
                    id: other.id.clone(),
                    path: other.path.clone(),
                    name: other.name.clone(),
                    kind: kind.to_owned(),
                    line: other.line,
                    matched_by: vec!["graph".to_owned()],
                    evidence: site.cloned(),
                }
            })
        })
        .collect();
    hits.into_iter().flatten().take(retrieval_budget).chain(neighbors).collect()
}

fn dispatch_peek(
    lattice_dir: &Path,
    text: &str,
    freshness: Freshness,
    filters: &Filters,
) -> (Value, Disposition) {
    if let Err(embed::EmbedError::ModelNotFound(msg)) = embed::ensure_model_present() {
        eprintln!("lattice: {msg}");
        return (envelope::error("model_not_found", &msg), Disposition::Error);
    }
    let hits = match vectors::query(lattice_dir, text) {
        Ok(h) => h,
        Err(e) => return internal(e.to_string()),
    };
    let projected: Vec<_> = hits
        .into_iter()
        .filter(|h| {
            if filters.exclude_tests {
                let c = class::classify(&h.path);
                c != class::SourceClass::Test && c != class::SourceClass::Fixture
            } else {
                true
            }
        })
        .take(filters.limit)
        .map(|h| {
            (
                h.path.clone(),
                if h.symbol.is_empty() {
                    h.path.clone()
                } else {
                    h.symbol.clone()
                },
                h.kind.clone(),
                h.start_line,
                class::classify(&h.path).as_str().to_string(),
            )
        })
        .collect();
    eprintln!("lattice: peek {text:?}: {} hit(s)", projected.len());
    (envelope::peek(&projected, freshness.as_str()), Disposition::Ok)
}

fn dispatch_bridges(
    lattice_dir: &Path,
    freshness: Freshness,
    filters: &Filters,
) -> (Value, Disposition) {
    let cg = match graph::load_call_graph(lattice_dir) {
        Ok(cg) => cg,
        Err(e) => return internal(e.to_string()),
    };
    let hits = centrality::bridges(&cg, filters.limit);
    eprintln!("lattice: bridges: {} chokepoint(s)", hits.len());
    (envelope::bridges(&hits, freshness.as_str()), Disposition::Ok)
}
