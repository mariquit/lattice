// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! lattice — knowledge memory CLI for brownfield codebases.
//!
//! Single offline binary, zero network egress at runtime. stdout carries
//! exactly one JSON envelope per invocation (protocol_version 1.0.0); human
//! prose goes to stderr. Non-zero exit + structured error envelope on failure.

mod analysis;
mod bench;
mod branch;
mod centrality;
mod class;
mod context;
mod embed;
mod envelope;
mod freshness;
mod fts;
mod fuse;
mod graph;
mod health;
mod index;
mod ingest;
mod install;
mod languages;
mod leiden;
mod markdown;
mod meta;
mod okf;
mod ownership;
mod progress;
mod review;
mod router;
mod snapshot;
mod vectors;

use std::path::Path;
use std::process::ExitCode;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const LATTICE_DIR: &str = ".lattice";
const GITIGNORE_ENTRY: &str = ".lattice/";

fn main() -> ExitCode {
    // Normalize the `--agent=<name>` equals-form into two tokens so the slice
    // patterns below match it identically to `--agent <name>`.
    let args: Vec<String> = std::env::args().skip(1).fold(Vec::new(), |mut acc, a| {
        if let Some(v) = a.strip_prefix("--agent=") {
            acc.push("--agent".to_string());
            acc.push(v.to_string());
        } else {
            acc.push(a);
        }
        acc
    });
    let arg_strs: Vec<&str> = args.iter().map(String::as_str).collect();

    // `query` is routed through the single dispatch point (guard-first
    // precedence, one envelope from {impact | semantic | error}); everything
    // else (`init`, unknown subcommands) is handled here.
    match arg_strs.as_slice() {
        // Human-facing help/version (prose → stderr, no JSON envelope, exit 0).
        // A bare invocation (no subcommand) is treated as a help request, not an
        // error — only *unrecognized* subcommands hit the exit-2 catch-all below.
        [] | ["--help"] | ["-h"] => {
            print_usage();
            ExitCode::SUCCESS
        }
        ["--version"] | ["-V"] => {
            eprintln!("lattice {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        ["install"] => cmd_install("opencode"),
        ["install", "--agent", agent] => cmd_install(agent),
        ["install", ..] => usage_error("usage: lattice install [--agent opencode]"),
        ["init"] => match cmd_init() {
            Ok(()) => ExitCode::SUCCESS,
            Err(EnvelopeError::ModelNotFound(msg)) => {
                envelope::emit(&envelope::error("model_not_found", &msg));
                eprintln!("lattice: error: {msg}");
                ExitCode::from(2)
            }
            Err(EnvelopeError::UnknownGrammar(msg)) => {
                envelope::emit(&envelope::error("unknown_grammar", &msg));
                eprintln!("lattice: error: {msg}");
                ExitCode::from(2)
            }
            Err(EnvelopeError::InvalidConfig(msg)) => {
                envelope::emit(&envelope::error("invalid_config", &msg));
                eprintln!("lattice: error: {msg}");
                ExitCode::from(2)
            }
            Err(EnvelopeError::Internal(msg)) => {
                envelope::emit(&envelope::error("internal", &msg));
                eprintln!("lattice: error: {msg}");
                ExitCode::from(1)
            }
        },
        ["bench", rest @ ..] => cmd_bench(rest),
        ["query", rest @ ..] => match router::route(rest) {
            router::Routed::Ok => ExitCode::SUCCESS,
            router::Routed::Emitted => ExitCode::from(2),
            router::Routed::Internal(msg) => {
                envelope::emit(&envelope::error("internal", &msg));
                eprintln!("lattice: error: {msg}");
                ExitCode::from(1)
            }
        },
        ["export"] => run_guarded(cmd_export),
        ["context"] => run_guarded(|dir| cmd_context(dir, None)),
        ["context", "--task", task] if !task.trim().is_empty() && !task.starts_with("--") => {
            run_guarded(|dir| cmd_context(dir, Some(task)))
        }
        ["context", ..] => usage_error("usage: lattice context [--task <text>]"),
        ["summarize", path] => {
            let path = path.to_string();
            run_guarded(move |dir| cmd_summarize(dir, &path))
        }
        ["snapshot", name] | ["snapshot", "--label", name] if !name.starts_with('-') => {
            run_guarded(|dir| cmd_snapshot_label(dir, name))
        }
        ["snapshot", ..] => usage_error("usage: lattice snapshot <label>"),
        _ => {
            envelope::emit(&envelope::error(
                "unknown_command",
                "unrecognized command; run `lattice --help` for usage",
            ));
            eprintln!("lattice: unknown command; run `lattice --help` for usage");
            ExitCode::from(2)
        }
    }
}

fn usage_error(message: &str) -> ExitCode {
    envelope::emit(&envelope::error("invalid_flag", message));
    eprintln!("lattice: {message}");
    ExitCode::from(2)
}

/// Human-facing usage text, printed to stderr (stdout stays reserved for the
/// machine JSON envelope). Shared by a bare invocation and `--help`/`-h`.
fn print_usage() {
    eprint!(
        "\
lattice {} — knowledge memory for brownfield codebases

usage:
  lattice init                    build/refresh .lattice/ for the current repo
  lattice query --impact <path>   structural: callers, dependents, blast radius
  lattice query --symbol <text>   lexical: find symbols by name (FTS5)
  lattice query \"<text>\"          hybrid search (FTS5 + MiniLM + RRF)
  lattice query --flows           execution flows detected from entry points
  lattice query --flow <sym>      trace one execution flow from a symbol
  lattice query --communities     function communities (Leiden clustering)
  lattice query --community <id>  inspect a specific community
  lattice query --bridges         chokepoints by betweenness centrality
  lattice query --review <ref>    risk, test gaps, and owners for a git diff
  lattice query --similar <sym>   nearest neighbors of a symbol's embedding
  lattice query --peek \"<text>\" locator-only semantic discovery (no body span)
  lattice query --graph-diff a b  diff two snapshots (or \"working\")
  lattice query --hotspots        high-churn, high-coupling files
  lattice query --dead-code       unused functions (heuristic)
  lattice query --show <hit-id>   exact source for an indexed context hit
  lattice context [--task <text>] repository overview and suggested queries
  lattice snapshot <label>        save a graph manifest for --graph-diff
  lattice export                  write human-readable OKF Markdown
  lattice summarize <path>        human-readable summary for a path
  lattice bench [--questions f]  naive vs query token counts (MiniLM tokenizer)
  lattice install                 cache MiniLM and install the global OpenCode skill

  flags: --hints (add next_tool_suggestions), --limit <N>, --exclude-tests,
         --kind <function|method|module>, --file-level, --with-criticality,
          --branch <name>, --all-branches, --resolution <R>, --split,
          --context \"<text>\"

  -h, --help                        print this help and exit
  -V, --version                     print version and exit
",
        env!("CARGO_PKG_VERSION")
    );
}

enum EnvelopeError {
    /// Candle model weights absent (init must embed changed files). Maps to the
    /// `model_not_found` error envelope + non-zero exit; no network is touched.
    ModelNotFound(String),
    /// `languages.toml` names a grammar that is not compiled into the binary.
    UnknownGrammar(String),
    /// `languages.toml` (or a snapshot label) is structurally invalid.
    InvalidConfig(String),
    /// Unexpected failure; main wraps it in an envelope.
    Internal(String),
}

impl<E: std::fmt::Display> From<E> for EnvelopeError {
    fn from(e: E) -> Self {
        EnvelopeError::Internal(e.to_string())
    }
}

// ── init ─────────────────────────────────────────────────────────────────────

fn cmd_init() -> std::result::Result<(), EnvelopeError> {
    let root = std::env::current_dir().map_err(|e| format!("resolve cwd: {e}"))?;
    eprintln!("lattice: indexing {}", root.display());

    let lattice_dir = index::lattice_dir(&root);

    // The ONLY place a live progress footer is created. Everything else in the
    // codebase reaches the index pipeline through `index::run`, which hard-wires
    // `Progress::hidden()` — that is what keeps query-time freshness self-heal
    // silent. Auto-disables itself when stderr is not a terminal.
    let prog = progress::Progress::stderr();

    // languages.toml is read HERE only (not inside index::run). A bad grammar
    // fails before any store write, so a previous index is left intact.
    let registry = match languages::read_file(&lattice_dir.join("languages.toml")) {
        Ok(r) => r,
        Err(languages::LanguageError::UnknownGrammar(g)) => {
            prog.finish();
            return Err(EnvelopeError::UnknownGrammar(g));
        }
        Err(languages::LanguageError::Invalid(m)) => {
            prog.finish();
            return Err(EnvelopeError::InvalidConfig(m));
        }
    };

    // The whole store build/refresh — discovery, hash-diff, fragment cache, and
    // the graph/FTS/vector/meta writes — lives in `index::run`, the SAME
    // pipeline the query-time freshness check calls. `cmd_init` is now just the
    // envelope + .gitignore wrapper around it, so a refresh can never drift
    // from what `init` would have done.
    let report = index::run_with_registry(&root, &lattice_dir, &prog, &registry).map_err(|e| {
        // Clear the footer before any error envelope reaches the terminal.
        prog.finish();
        match e {
            index::IndexError::ModelNotFound(m) => EnvelopeError::ModelNotFound(m),
            index::IndexError::Internal(m) => EnvelopeError::Internal(m),
        }
    })?;

    // Git ownership pass (optional — skipped when no .git, never fails init).
    // Hooked HERE (not inside index::run) so the full-history git scan runs
    // only on explicit `lattice init`, not on every query-time freshness check.
    prog.stage("git · scanning history for ownership", None);
    match ownership::scan(&root, &lattice_dir) {
        Ok(Some(r)) => prog.note(&format!(
            "lattice: git pass: {} commit(s), {} file(s)",
            r.commits, r.files
        )),
        Ok(None) => prog.note("lattice: git pass: skipped (no .git)"),
        Err(e) => prog.note(&format!("lattice: git pass: skipped ({e})")),
    }

    // Idempotent .gitignore guard: exactly one `.lattice/` entry.
    ensure_gitignore(&root)?;

    // Erase the footer BEFORE the summary + JSON envelope, so nothing is left
    // half-drawn and the envelope is never mixed into a spinner line.
    prog.finish();

    eprintln!(
        "lattice: indexed {} files ({} reindexed, {} parsed, {} unchanged)",
        report.files_indexed, report.files_reindexed, report.files_parsed, report.files_unchanged
    );
    envelope::emit(&envelope::init_ok(
        report.files_indexed,
        report.files_reindexed,
        report.files_unchanged,
        report.files_parsed,
    ));
    Ok(())
}

// Install is the only command that can download model files.

fn cmd_install(agent: &str) -> ExitCode {
    if agent != "opencode" {
        let msg = format!("unknown agent '{agent}'; expected opencode");
        envelope::emit(&envelope::error("unknown_agent", &msg));
        eprintln!("lattice: {msg}");
        return ExitCode::from(2);
    }
    let Some(home) = install::home_dir().filter(|path| path.is_absolute()) else {
        let msg = "install requires an absolute HOME directory";
        envelope::emit(&envelope::error("internal", msg));
        eprintln!("lattice: {msg}");
        return ExitCode::from(1);
    };
    let (path, cached) = match install::install_model() {
        install::Installed::Ok { path, cached } => {
            if cached {
                eprintln!("lattice: model already cached — nothing to fetch");
            } else {
                eprintln!("lattice: model cached at {}", path.display());
            }
            (path, cached)
        }
        install::Installed::FetchFailed(msg) => {
            envelope::emit(&envelope::error("model_fetch_failed", &msg));
            eprintln!("lattice: error: {msg}");
            return ExitCode::from(1);
        }
    };

    match install::install_opencode(&home) {
        Ok(wrote) => {
            eprintln!("lattice: install --agent {agent} → {}", wrote.join(", "));
            envelope::emit(&envelope::install_ok(embed::MODEL_NAME, &path.to_string_lossy(), embed::MODEL_FILES.len(), cached, agent, &wrote));
            ExitCode::SUCCESS
        }
        Err(msg) => {
            envelope::emit(&envelope::error("internal", &msg));
            eprintln!("lattice: error: {msg}");
            ExitCode::from(1)
        }
    }
}

// ── export / summarize (OKF) ───────────────────────────────────────────────

/// Run an OKF command behind the shared `no_lattice` guard (consistent with
/// the router slice): resolve the repo root, verify `.lattice/` exists, then
/// invoke `f` with the lattice dir. Returns the process exit code.
fn run_guarded<F>(f: F) -> ExitCode
where
    F: FnOnce(&Path) -> std::result::Result<(), EnvelopeError>,
{
    let root = match std::env::current_dir() {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("resolve cwd: {e}");
            envelope::emit(&envelope::error("internal", &msg));
            eprintln!("lattice: error: {msg}");
            return ExitCode::from(1);
        }
    };
    let lattice_dir = root.join(LATTICE_DIR);
    if !lattice_dir.is_dir() {
        envelope::emit(&envelope::error(
            "no_lattice",
            "no .lattice/ found; run `lattice init` first",
        ));
        eprintln!("lattice: no .lattice/ store at {}", root.display());
        return ExitCode::from(2);
    }
    match f(&lattice_dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(EnvelopeError::ModelNotFound(msg)) => {
            // OKF export/summarize never embed, so this is not expected here;
            // handle it for exhaustiveness with the same structured envelope.
            envelope::emit(&envelope::error("model_not_found", &msg));
            eprintln!("lattice: error: {msg}");
            ExitCode::from(2)
        }
        Err(EnvelopeError::UnknownGrammar(msg)) => {
            envelope::emit(&envelope::error("unknown_grammar", &msg));
            eprintln!("lattice: error: {msg}");
            ExitCode::from(2)
        }
        Err(EnvelopeError::InvalidConfig(msg)) => {
            envelope::emit(&envelope::error("invalid_config", &msg));
            eprintln!("lattice: error: {msg}");
            ExitCode::from(2)
        }
        Err(EnvelopeError::Internal(msg)) => {
            envelope::emit(&envelope::error("internal", &msg));
            eprintln!("lattice: error: {msg}");
            ExitCode::from(1)
        }
    }
}

fn cmd_context(
    lattice_dir: &Path,
    task: Option<&str>,
) -> std::result::Result<(), EnvelopeError> {
    let value = context::overview(lattice_dir, task)?;
    envelope::emit(&value);
    Ok(())
}

fn cmd_bench(rest: &[&str]) -> ExitCode {
    let mut questions_path: Option<&str> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "--questions" => {
                i += 1;
                match rest.get(i).copied() {
                    Some(p) if !p.is_empty() => questions_path = Some(p),
                    _ => {
                        envelope::emit(&envelope::error(
                            "invalid_flag",
                            "--questions requires a file path",
                        ));
                        eprintln!("lattice: --questions requires a file path");
                        return ExitCode::from(2);
                    }
                }
            }
            other => {
                let msg = format!("unknown bench flag {other:?}");
                envelope::emit(&envelope::error("invalid_flag", &msg));
                eprintln!("lattice: {msg}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    let questions: Vec<String> = match questions_path {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_string)
                .collect(),
            Err(e) => {
                let msg = format!("read questions {path}: {e}");
                envelope::emit(&envelope::error("invalid_flag", &msg));
                eprintln!("lattice: {msg}");
                return ExitCode::from(2);
            }
        },
        None => bench::DEFAULT_QUESTIONS
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    let root = match std::env::current_dir() {
        Ok(r) => r,
        Err(e) => {
            envelope::emit(&envelope::error("internal", &e.to_string()));
            return ExitCode::from(1);
        }
    };
    let lattice_dir = root.join(LATTICE_DIR);
    if !lattice_dir.is_dir() {
        envelope::emit(&envelope::error(
            "no_lattice",
            "no .lattice/ at the repo root; run `lattice init` first",
        ));
        eprintln!("lattice: no .lattice/; run `lattice init` first");
        return ExitCode::from(2);
    }
    match bench::run(&root, &lattice_dir, &questions) {
        Ok(env) => {
            envelope::emit(&env);
            ExitCode::SUCCESS
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("model missing") || msg.contains("ModelNotFound") {
                envelope::emit(&envelope::error("model_not_found", &msg));
                ExitCode::from(2)
            } else {
                envelope::emit(&envelope::error("internal", &msg));
                eprintln!("lattice: error: {msg}");
                ExitCode::from(1)
            }
        }
    }
}

fn cmd_export(lattice_dir: &Path) -> std::result::Result<(), EnvelopeError> {
    let written = okf::export(lattice_dir).map_err(|e| e.to_string())?;
    eprintln!(
        "lattice: exported OKF Markdown to {}/{} ({} files)",
        LATTICE_DIR,
        okf::OKF_ROOT_REL
            .strip_prefix(".lattice/")
            .unwrap_or(okf::OKF_ROOT_REL),
        written
    );
    envelope::emit(&envelope::export_ok(okf::OKF_ROOT_REL, written));
    Ok(())
}

fn cmd_summarize(lattice_dir: &Path, target: &str) -> std::result::Result<(), EnvelopeError> {
    let summary = okf::summarize(lattice_dir, target).map_err(|e| e.to_string())?;
    // Human-readable rendering goes to stderr; stdout stays machine-parseable.
    eprintln!("lattice: summary of {target}:\n{summary}");
    envelope::emit(&envelope::summarize_ok(target, &summary));
    Ok(())
}

fn cmd_snapshot_label(
    lattice_dir: &Path,
    label: &str,
) -> std::result::Result<(), EnvelopeError> {
    if label == snapshot::WORKING {
        return Err(EnvelopeError::InvalidConfig(format!(
            "label {:?} is reserved for the live graph",
            snapshot::WORKING
        )));
    }
    let root = lattice_dir
        .parent()
        .ok_or_else(|| "resolve repo root".to_string())?;
    // Capture the working tree, not a stale store.
    freshness::ensure_fresh(lattice_dir, root).map_err(|e| e.to_string())?;
    let cg = graph::load_call_graph(lattice_dir).map_err(|e| e.to_string())?;
    let manifest = snapshot::capture(&cg);
    let nodes = manifest.nodes.len();
    let edges = manifest.edges.len();
    snapshot::save(lattice_dir, label, &manifest).map_err(|e| e.to_string())?;
    eprintln!("lattice: snapshot '{label}' written ({nodes} nodes, {edges} edges)");
    envelope::emit(&envelope::snapshot_label_ok(label, nodes, edges));
    Ok(())
}

fn ensure_gitignore(root: &Path) -> Result<()> {
    let path = root.join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read .gitignore: {e}").into()),
    };
    if existing.lines().any(|line| line.trim() == GITIGNORE_ENTRY) {
        return Ok(()); // already guarded; never duplicate
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(GITIGNORE_ENTRY);
    updated.push('\n');
    std::fs::write(&path, updated).map_err(|e| format!("write .gitignore: {e}"))?;
    Ok(())
}
