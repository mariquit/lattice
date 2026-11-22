# AGENTS.md: lattice operating manual

**lattice** is a standalone Rust CLI that builds and serves a *knowledge
memory* (`.lattice/`) for a brownfield codebase — code context and
relationships (call edges, deps, blast radius, ownership, tribal knowledge).
It does not require an external workflow or coding agent.

> Product overview, architecture, the full CLI surface, and status live in
> [`README.md`](README.md). **This file is the operating manual for agents
> working *in* this repo** — fixed decisions, the quality bar, conventions, and
> the gotchas that will bite you. Read it before changing code.

## Fixed decisions — do not relitigate these

- **Language/runtime:** Rust, single CLI binary `lattice`. No server, no
  daemon, no network at runtime.
- **Graph engine: SparrowDB** (`sparrowdb` on crates.io, ~0.1.16; source
  <https://github.com/ryaker/SparrowDB>) — a *pure-Rust* embedded property-graph
  DB with Cypher. **Chosen over Kùzu on purpose:** Kùzu's Rust crate drives a
  heavy, fragile embedded C++ build; SparrowDB is pure Rust and builds cleanly.
  Prefer pure-Rust dependencies generally — avoid crates that need `cmake` /
  `libclang` / a C++ toolchain wherever a pure-Rust option exists.
- **Vectors:** SQLite + `sqlite-vec`. Embeddings computed **locally/offline**
  via **candle** (pure-Rust), zero network egress. The hard rule is *local +
  deterministic-enough-to-test + zero egress*, not any specific model.
- **Ingestion: tree-sitter (syntactic).** One grammar per language, no language
  servers or toolchains. Call edges are heuristic (name + scope resolution) —
  good enough for blast-radius, precision can improve later. `lattice init`
  **respects `.gitignore`** (ripgrep's `ignore` crate walker).
- **MVP languages:** Rust, Python, TypeScript.
- **Graph model:** nodes = file / module / function-or-method; edges = `calls`,
  `imports`, `contains`.
- **No running services.** The whole memory is a self-contained, git-ignored
  folder that travels with the repo. Deliberate deviation from the org default
  (PostgreSQL + Redis): portability, offline/air-gapped, confidential-code
  posture (nothing leaves the machine).

## Quality bar (checkable, not adjectives)

- `cargo build` and `cargo test` run cleanly **in this environment with ZERO
  warnings** — no C++ toolchain surprises (that's why SparrowDB). If a dep won't
  build, swap it for a pure-Rust one or stop and flag it; never fake a green test.
- **Zero network egress** during `init` and `query` — verifiable; embeddings are
  local.
- **Never weaken a test to make it pass; fix the implementation.** Fixtures have
  hand-countable ground truth so assertions are exact (`assert_eq!`, not `>=`).
  If actual ingested counts differ, fix the FIXTURE, never the assertion.
- stdout is exactly ONE JSON envelope per invocation with `protocol_version`;
  prose goes to stderr only.
- Update the CLI contract in [`README.md`](README.md) in the same
  change as any new CLI or envelope behavior.

## Test-driven vertical slices

Build thin, end-to-end, one slice at a time. Each slice is provable by ONE
non-interactive `cargo test --test <name>`; write the failing test FIRST, then
implement until green. Small fixtures live under `tests/fixtures/<lang>_basic/`
with known, hand-countable node/edge ground truth.

## Conventions & gotchas (these will bite you)

- **JSON envelope discipline.** Every `query` response is one envelope from
  `src/envelope.rs`, built with the shared `PROTOCOL_VERSION` const (never a
  string literal), `query_type` set, and the `freshness` field threaded through.
  This envelope is the contract for CLI consumers.
- **Versioning policy.** A *new* `query_type` or a *new* additive field = **no
  protocol bump**. Changing an *existing* envelope's default shape/cardinality =
  **minor bump** (ship an opt-out flag). Confirmed by `callers`/`callees`/
  `call_chain`/`owners`/health-analytics (all added without a bump) and by
  symbol-granular-embeddings (1.0.0→1.1.0, a default-shape change).
- **Query-time freshness self-heal reuses `index::run`.** `router::route_value`
  calls `freshness::ensure_fresh` before every dispatch, which re-runs the
  incremental index pipeline. Consequence: anything you only want to happen on
  explicit `init` (e.g. the git-ownership scan) must be hooked in
  `main.rs::cmd_init` ONLY, never inside `index::run` — otherwise it runs on
  every query. The ownership git-scan is init-only for exactly this reason
  (so `--hotspots` churn weighting only reflects the last `init`).
- **`.gitignore` respect has TWO independent paths.** Tree discovery
  (`index::collect_source_files`, shared with `freshness`) gets it free from the
  `ignore` walker. The ownership pass (`ownership::scan`) walks git *history*, so
  the walker cannot prune for it — a path committed then later ignored
  (`git rm --cached target/`) lives in old trees forever. It filters explicitly
  via `build_ignore_matchers` + `is_excluded`. Three traps in that code:
  (1) the `.gitignore`-collecting walk needs `.hidden(false)` — `.gitignore` is
  itself hidden, so `hidden(true)` yields NOTHING and silently builds an empty
  matcher; (2) a nested `.gitignore` is subtree-scoped, so patterns must NOT be
  merged into one root-anchored matcher (that makes `sub/.gitignore`'s `x.rs`
  wrongly ignore a root-level `x.rs`); (3) `matched_path_or_any_parents` PANICS
  on a path outside the matcher root — keep the `starts_with(dir)` guard.
  Deleted-but-never-ignored paths SHOULD stay in ownership history; only
  *ignored* ones are filtered.
- **Adding a new query intent touches FOUR router sites** (miss one → misroute):
  (1) `extract_flags` unknown-flag whitelist; (2) an `Intent` variant;
  (3) a `parse_intent` arm; (4) a `route_value` dispatch arm + `dispatch_*` fn.
  The `--kind`/`--file-level` guards use `matches!(intent, …)` allow-lists, so
  they reject new intents for free — but assert that in the gate.
- **Progress output is opt-in by TYPE, not by flag.** `index::run` hard-wires
  `progress::Progress::hidden()`; the footer only exists via
  `index::run_with_registry` when `main.rs::cmd_init` supplies a live reporter. This is
  deliberate: query-time freshness self-heal re-runs the same pipeline, so an
  "always on" footer would spin during every stale `query` and corrupt agent
  output. Keep new progress calls behind the passed-in `&Progress`; never
  `eprintln!` per-file from inside the pipeline (use `prog.note` so the footer
  is suspended instead of interleaved). The footer self-disables on a non-tty,
  so piped stderr stays byte-identical; `LATTICE_PROGRESS=always|never`
  overrides that (the gate uses `always` to assert frames without a pty).
- **Agent integrations use the CLI.** Keep the on-demand skill in
  `skills/lattice/SKILL.md`. Do not register background services or inject
  repository overviews into always-loaded agent instructions.
- **gix tree-diff direction** (ownership layer): call `.changes()` on the PARENT
  tree and pass the CHILD; the reverse inverts add/del.
- **Acceptance tests** document expected behavior in their assertions and
  fixtures. Change those expectations only when an approved contract change
  requires it, and explain the change in the merge request.

## Handy commands

```bash
cargo test                              # full suite (must be green, 0 warnings)
cargo test --test impact_rust           # one slice's gate
cargo build && ./target/debug/lattice init
./target/debug/lattice query --impact src/graph.rs
```

## Repository layout

Production code lives in `src/`, integration tests in `tests/`, and retrieval
evaluation data and code in `eval/` and `examples/`. The workspace index lives
under `.lattice/` and is ignored by Git.
