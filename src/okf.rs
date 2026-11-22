// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! OKF (human-readable) Markdown export — the `export` and `summarize` commands.
//!
//! This layer adds NO source of truth: every page is derived from the persisted
//! graph (`graph.sparrow/`) read back via `graph::read`. `export` never
//! re-parses source files. Output layout:
//!
//! ```text
//! .lattice/export/okf/
//!   services/overview.md      repo-level counts + language mix
//!   modules/<stem>.md         one page per source file (imports/callers/blast)
//!   teams/unassigned.md       MVP ownership placeholder (all modules, no owner)
//! ```
//!
//! `summarize <path>` reuses the same module renderer to return one path's page
//! as a summary. Both commands emit exactly one JSON envelope on stdout; the
//! human prose goes to stderr and into the .md files.

use std::collections::BTreeMap;
use std::path::Path;

use crate::graph::{self, GraphView};
use crate::ownership;
use crate::Result;

/// Repo-relative root of the generated tree (frozen envelope value).
pub const OKF_ROOT_REL: &str = ".lattice/export/okf";

/// The `.lattice/export/okf` dir, relative to `lattice_dir` (= .lattice/).
const EXPORT_SUBDIR: &str = "export/okf";

/// Derive the module page stem for a source path (e.g. `auth.rs` -> `auth`,
/// `src/db/pool.rs` -> `pool`). Falls back to a sanitized full path if empty.
fn module_stem(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = match file.rfind('.') {
        Some(i) if i > 0 => &file[..i],
        _ => file,
    };
    if stem.is_empty() {
        path.replace('/', "_")
    } else {
        stem.to_string()
    }
}

/// Render the module page for a single source file, derived from the graph.
/// The source path (e.g. the literal `auth.rs`) always appears in the body —
/// this is the greppable evidence the test asserts. `impact` comes from the
/// same `graph::impact` semantics used by `query --impact` (exact, not
/// approximated), keeping the OKF pages consistent with structural queries.
fn render_module_page(
    lattice_dir: &Path,
    view: &GraphView,
    path: &str,
    language: &str,
) -> Result<String> {
    let impact = graph::impact(lattice_dir, path)?;
    let functions = view.functions_in(path);
    let imports = view.imports_of(path);

    let mut md = String::new();
    md.push_str(&format!("# Module `{path}`\n\n"));
    md.push_str(&format!("- **Source path:** `{path}`\n"));
    md.push_str(&format!("- **Language:** {language}\n"));
    if let Ok(cg) = graph::load_call_graph(lattice_dir) {
        let comms = crate::analysis::communities(&cg);
        if let Some(c) = comms.iter().find(|c| {
            c.members
                .iter()
                .any(|id| id.starts_with(&format!("{path}::")))
        }) {
            md.push_str(&format!("- **Community:** {} ({})\n", c.name, c.id));
        }
    }
    md.push_str(&format!("- **Blast radius:** {}\n\n", impact.blast_radius));

    md.push_str("## Contains\n\n");
    if functions.is_empty() {
        md.push_str("_No functions ingested._\n\n");
    } else {
        for f in &functions {
            md.push_str(&format!("- `{f}`\n"));
        }
        md.push('\n');
    }

    md.push_str("## Imports\n\n");
    if imports.is_empty() {
        md.push_str("_Imports no other indexed modules._\n\n");
    } else {
        for i in &imports {
            md.push_str(&format!("- `{i}`\n"));
        }
        md.push('\n');
    }

    md.push_str("## Called by\n\n");
    if impact.callers.is_empty() {
        md.push_str("_No indexed callers._\n\n");
    } else {
        for c in &impact.callers {
            md.push_str(&format!("- `{c}`\n"));
        }
        md.push('\n');
    }

    md.push_str("## Imported by (dependents)\n\n");
    if impact.dependents.is_empty() {
        md.push_str("_No indexed dependents._\n\n");
    } else {
        for d in &impact.dependents {
            md.push_str(&format!("- `{d}`\n"));
        }
        md.push('\n');
    }

    Ok(md)
}

/// Render the repo-level services overview: counts + language mix.
fn render_services_overview(view: &GraphView) -> String {
    let mut lang_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, lang) in &view.files {
        *lang_counts.entry(lang.as_str()).or_insert(0) += 1;
    }

    let mut md = String::new();
    md.push_str("# Repository Overview\n\n");
    md.push_str("Human-readable summary of the indexed codebase, derived from the lattice graph.\n\n");
    md.push_str("## Counts\n\n");
    md.push_str(&format!("- **Files:** {}\n", view.files.len()));
    md.push_str(&format!("- **Functions:** {}\n", view.functions.len()));
    md.push_str(&format!("- **Import edges:** {}\n\n", view.imports.len()));

    md.push_str("## Language mix\n\n");
    if lang_counts.is_empty() {
        md.push_str("_No files indexed._\n\n");
    } else {
        for (lang, count) in &lang_counts {
            md.push_str(&format!("- **{lang}:** {count}\n"));
        }
        md.push('\n');
    }

    md.push_str("## Modules\n\n");
    for (path, _) in &view.files {
        md.push_str(&format!("- `{path}`\n"));
    }
    md.push('\n');
    md
}

/// Render the teams page. If git ownership data is present, emit per-directory
/// ownership tables (top 3 authors, churn, last touched). If absent (no-git
/// store or first init before `lattice init` with git), emit the
/// `_No git history ingested._` fallback so the page is never empty.
///
/// Rename handling: delete+add for MVP; ownership "resets" across renames.
fn render_teams_page(view: &GraphView, lattice_dir: &Path) -> String {
    let mut md = String::new();
    md.push_str("# Ownership\n\n");

    // Load git ownership stats; fall back gracefully on any error.
    let file_stats = ownership::all_file_stats(lattice_dir).unwrap_or_default();

    if file_stats.is_empty() {
        md.push_str(
            "_No git history ingested._\n\n\
             Every indexed module is listed below under the explicit **unassigned** owner.\n\n",
        );
        md.push_str("## unassigned\n\n");
        if view.files.is_empty() {
            md.push_str("_No modules indexed._\n\n");
        } else {
            for (path, _) in &view.files {
                md.push_str(&format!("- `{path}`\n"));
            }
            md.push('\n');
        }
        return md;
    }

    // Aggregate by top-level path component (first segment; bare files → ".").
    // dir → Vec<(author, total_added, total_commits, latest_last_touched, churn_sum)>
    // We store: dir → { author → (added, commits, latest_touched), dir_churn, dir_last }
    struct DirEntry {
        author_added: BTreeMap<String, (u64, u64, String)>, // author -> (added, commits, latest)
        churn: u64,
        last_touched: String,
    }
    let mut dirs: BTreeMap<String, DirEntry> = BTreeMap::new();

    for (path, (churn, last, authors)) in &file_stats {
        let dir = match path.find('/') {
            Some(i) => path[..i].to_string(),
            None => ".".to_string(),
        };
        let entry = dirs.entry(dir).or_insert(DirEntry {
            author_added: BTreeMap::new(),
            churn: 0,
            last_touched: String::new(),
        });
        entry.churn += churn;
        if last > &entry.last_touched {
            entry.last_touched = last.clone();
        }

        // Compute total added for this file to calc shares.
        let total_added: u64 = authors.iter().map(|(_, a, _, _)| a).sum();
        let total_commits: u64 = authors.iter().map(|(_, _, c, _)| c).sum();

        for (author, added, commits, aut_last) in authors {
            let ae = entry
                .author_added
                .entry(author.clone())
                .or_insert((0, 0, String::new()));
            ae.0 += added;
            ae.1 += commits;
            if aut_last > &ae.2 {
                ae.2 = aut_last.clone();
            }
            let _ = (total_added, total_commits); // suppress warning
        }
    }

    md.push_str("## Ownership\n\n");
    for (dir, entry) in &dirs {
        md.push_str(&format!("### {dir}\n\n"));
        md.push_str("| author | share | churn | last touched |\n");
        md.push_str("|---|---|---|---|\n");

        // Compute shares across all authors in this directory.
        let total_added: u64 = entry.author_added.values().map(|(a, _, _)| a).sum();
        let total_commits: u64 = entry.author_added.values().map(|(_, c, _)| c).sum();

        // Sort by added desc, limit to top 3.
        let mut sorted: Vec<(&String, &(u64, u64, String))> =
            entry.author_added.iter().collect();
        sorted.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
        sorted.truncate(3);

        for (author, (added, commits, last)) in &sorted {
            let share = if total_added > 0 {
                *added as f64 / total_added as f64
            } else if total_commits > 0 {
                *commits as f64 / total_commits as f64
            } else {
                0.0
            };
            md.push_str(&format!(
                "| {author} | {share:.2} | {} | {last} |\n",
                entry.churn
            ));
        }
        md.push('\n');
    }

    md
}

/// `lattice export`: regenerate the OKF Markdown tree wholesale from the
/// graph. Returns the number of `.md` files written.
pub fn export(lattice_dir: &Path) -> Result<usize> {
    let view = graph::read(lattice_dir)?;

    let okf_dir = lattice_dir.join(EXPORT_SUBDIR);
    // Regenerate wholesale (idempotent): clear any prior export tree first.
    if okf_dir.exists() {
        std::fs::remove_dir_all(&okf_dir).map_err(|e| format!("clear export tree: {e}"))?;
    }
    let services_dir = okf_dir.join("services");
    let modules_dir = okf_dir.join("modules");
    let teams_dir = okf_dir.join("teams");
    for d in [&services_dir, &modules_dir, &teams_dir] {
        std::fs::create_dir_all(d).map_err(|e| format!("create {}: {e}", d.display()))?;
    }

    let mut written = 0usize;

    // services/overview.md
    std::fs::write(services_dir.join("overview.md"), render_services_overview(&view))
        .map_err(|e| format!("write services/overview.md: {e}"))?;
    written += 1;

    // modules/<stem>.md — one page per source file.
    for (path, language) in &view.files {
        let page = render_module_page(lattice_dir, &view, path, language)?;
        let stem = module_stem(path);
        std::fs::write(modules_dir.join(format!("{stem}.md")), page)
            .map_err(|e| format!("write modules/{stem}.md: {e}"))?;
        written += 1;
    }

    // teams/unassigned.md
    std::fs::write(
        teams_dir.join("unassigned.md"),
        render_teams_page(&view, lattice_dir),
    )
    .map_err(|e| format!("write teams/unassigned.md: {e}"))?;
    written += 1;

    Ok(written)
}

/// `lattice summarize <path>`: reuse the module renderer for a single path.
/// Returns the human-readable summary (also the same text that is emitted in
/// the envelope's `summary` field). If the path is not an indexed file, a
/// best-effort summary is still produced so the command never emits an empty
/// summary for a syntactically valid path.
pub fn summarize(lattice_dir: &Path, target: &str) -> Result<String> {
    let view = graph::read(lattice_dir)?;
    let normalized = target.strip_prefix("./").unwrap_or(target);

    let language = view
        .files
        .iter()
        .find(|(p, _)| p == normalized)
        .map(|(_, l)| l.clone());

    match language {
        Some(lang) => render_module_page(lattice_dir, &view, normalized, &lang),
        None => {
            // Path is not an indexed file; still return a non-empty summary.
            Ok(format!(
                "# Path `{normalized}`\n\nNo indexed module matches `{normalized}` in the \
                 lattice graph. It may be outside the indexed source set or not yet \
                 ingested.\n"
            ))
        }
    }
}
