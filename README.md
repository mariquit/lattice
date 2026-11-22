# Project: Lattice

**Lattice** is a standalone Rust CLI for searching code and documentation, tracing calls and dependencies, assessing change impact, and inspecting Git ownership. Use it from a terminal, a script, or OpenCode. No agent or service is required.

```text
  ┌───┬───┬───┬───┬───┬───┬───┐
  │ L │ A │ T │ T │ I │ C │ E │ v0.1.0
  └───┴───┴───┴───┴───┴───┴───┘
  Semantic codebase wiring for OpenCode
```

Lattice answers questions about one workspace. [Span](https://github.com/mariquit/span) connects context across workspaces. Neither is required to run the other.

Lattice fuses FTS5 and local embeddings with reciprocal rank fusion, then uses a call graph to show what your edit could affect.


## Related OpenCode projects

- [Trestle](https://github.com/mariquit/trestle): Software engineering skills for OpenCode
- [Truss](https://github.com/mariquit/truss): Plugins for OpenCode
- [Span](https://github.com/mariquit/span): Contextual aggregate memory mesh for OpenCode
- [Formwork](https://github.com/mariquit/formwork): Spec-driven agent for solo developers on OpenCode
- [Catwalk](https://github.com/mariquit/catwalk): Terminal viewer for agent-owned TODO lists

## Quick start

1. From the Lattice checkout, build and install the CLI with `cargo install --path . --locked`.
   Verify that `lattice --version` works from your shell.
2. Run `lattice install` to cache MiniLM and install the user-level
   [OpenCode skill](skills/lattice/SKILL.md). If you already have the model
   files, use the default cache at `$HOME/.lattice/models/all-MiniLM-L6-v2/`
   or set `LATTICE_MODEL_DIR` to the directory containing `config.json`,
   `tokenizer.json`, and `model.safetensors`. With the model cached, standalone
   CLI users can skip `lattice install`.
3. From the project root, run `lattice init`. Confirm the JSON result contains
   `"status":"ok"`, then search or inspect a project-relative path:

   ```bash
   lattice query "where are authentication tokens validated" --limit 5
   lattice query --symbol validate_token
   lattice query --impact src/auth/token.rs
   ```

The OpenCode skill uses these same commands. `lattice install` does not create a workspace index. See [WORKFLOW.md](WORKFLOW.md) for setup and skill verification.

## Search and analysis

| Task | Command |
|---|---|
| Search by description using FTS5, MiniLM, and reciprocal rank fusion | `lattice query "<question>"` |
| Search by symbol name or look up a Markdown heading | `lattice query --symbol <text>` |
| Find similar symbols or get locator-only search results | `lattice query --similar <symbol>` or `lattice query --peek "<text>"` |
| Retrieve a graph-enriched context pack or file-level vector results | `lattice query --context "<text>"` or `lattice query "<text>" --file-level` |
| Check file or symbol impact | `lattice query --impact <path>` or `lattice query --impact --symbol <name>` |
| Trace calls | `lattice query --callers <symbol>`, `lattice query --callees <symbol>`, or `lattice query --call-chain <from> <to>` |
| Inspect ownership, churn, and co-changes | `lattice query --owners <path>` |
| Inspect hotspots, dead-code candidates, coupling, or health | `lattice query --hotspots`, `lattice query --dead-code`, `lattice query --coupling <path>`, or `lattice query --health` |
| Trace entry-point flows or find call-graph chokepoints | `lattice query --flows`, `lattice query --flow <symbol>`, or `lattice query --bridges` |
| Find or inspect function communities | `lattice query --communities` or `lattice query --community <id>` |
| Review a Git commit or range | `lattice query --review <ref>` |
| Compare saved or live graphs | `lattice snapshot <label>` then `lattice query --graph-diff <label> working` |
| Get a repository overview or a path summary | `lattice context [--task "<text>"]` or `lattice summarize <path>` |
| Export human-readable OKF Markdown | `lattice export` |
| Compare query response size with reading the indexed corpus | `lattice bench [--questions <file>]` |

For retrieval, use `--limit` to cap results and `--exclude-tests` to filter test and fixture hits where supported. Only `--symbol` accepts branch overrides: `--branch <name>` and `--all-branches` use a local catalog of previously indexed branches. They do not fetch remote data. `--hints` adds suggested CLI command templates to query results.

Call edges are inferred from syntax, not observed at runtime. Binding evidence comes from same-file or import/qualifier resolution; name evidence comes from a unique bare-name match. Use `--min-evidence binding` with impact, caller, callee, or call-chain queries to exclude name-only matches. Use `--with-evidence` with callers, callees, or symbol impact to see the evidence on hits. Verify important relationships in source before relying on them.

`--review` identifies changed symbols from the old and new Git revisions. It uses matching nodes in the working-tree index for caller, risk, and test-gap estimates. Test gaps are inferred from calls, not measured test coverage.

## What gets indexed

Lattice parses Rust, Python, and TypeScript with Tree-sitter. Use `.lattice/languages.toml` to map extensions to bundled grammars, including Kotlin. Markdown is searchable by heading and prose, but does not add nodes or edges to the code graph. Sections longer than 150 lines are split; hits on those parts include the original section's heading and line range in `parent`.

`lattice init` builds or refreshes the index, respects `.gitignore`, and attempts a Git-history scan for ownership and churn. Queries can refresh changed source files before answering, without editing source or rescanning Git history. `lattice context` reads the stored index as-is. Run `lattice init` again when you need to refresh ownership and churn. Use `LATTICE_PROGRESS=always` or `LATTICE_PROGRESS=never` to override terminal progress detection during `init`.

The git-ignored `.lattice/` directory contains the workspace index:

```text
.lattice/
  graph.sparrow/   # SparrowDB call, import, and containment graph
  meta.db          # SQLite FTS5, file metadata, Git ownership, and snapshots
  vectors.db       # SQLite + sqlite-vec embeddings
  export/okf/      # Markdown written by lattice export
```

Indexing and queries run offline. Model installation can download weights if they are missing. Treat the index and its exports as private project data.

## CLI output

Machine-facing commands emit one JSON object on stdout with `protocol_version` (`1.5.0`). Diagnostics go to stderr. Failures have a nonzero exit code and a structured error. `--help` and `--version` print human-readable output to stderr.

## Build and test

With Cargo dependencies and the MiniLM model cached:

```bash
cargo build --locked --offline
cargo test --locked --offline
```



## License and contributing

Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>.

Lattice is licensed under the [Apache License, Version 2.0](LICENSE). 

See [NOTICE](NOTICE) for attribution, [TRADEMARK.md](TRADEMARK.md) for naming, [DCO](DCO) for sign-off terms, [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) for community standards, and [SECURITY.md](SECURITY.md) for private reporting.

See [CONTRIBUTING.md](CONTRIBUTING.md) for development and [AGENTS.md](AGENTS.md) for repository-specific coding guidance.
