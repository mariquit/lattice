# Lattice workflow

The `lattice` binary owns the index and runs the queries. The OpenCode skill
explains which CLI commands to use. It does not install models, initialize a
workspace, or decide when work is complete. Lattice also works without OpenCode.

```text
User setup          Workspace setup          Code research
lattice install  →  lattice init          →  lattice query …
```

## Setup and operation

| Step | Actor | Command | Successful result |
| --- | --- | --- | --- |
| User setup | Developer | `lattice install` | The local MiniLM cache is ready and the OpenCode skill is installed for the current user. No workspace is indexed. |
| Workspace setup | Developer | `lattice init` from the project root | The `.lattice/` index exists, and stdout reports `"status":"ok"`. In a Git repository, `init` also attempts the ownership and churn scan. |
| Code research | Developer or agent | `lattice query …` from the indexed project root | A versioned JSON result describes matches or graph relationships. Inspect `query_type`, `freshness`, and the payload before relying on it. |
| Full refresh | Developer or calling workflow | `lattice init` from the project root | The index is refreshed and the Git-history scan is attempted again. |

Use `lattice query --symbol '<name>'` for a known identifier, `lattice query
'<question>'` for hybrid search, and `lattice query --impact '<path>'` before
editing a file. Verify important relationships in source: the call graph is
syntactic, and an empty result does not prove there are no callers.

Queries can refresh a stale local index before answering. They do not run the
Git ownership and churn scan. `lattice context` reads the persisted index
without refreshing it. Lattice does not monitor edits, commits, or merges in
the background. If a team needs a full refresh at task completion, its workflow
must invoke `lattice init` explicitly in the intended checkout.

## Workspace artifacts

| Artifact | Owner | Purpose |
| --- | --- | --- |
| `.lattice/graph.sparrow/` | Lattice | Structural relationships used for call and impact queries. |
| `.lattice/vectors.db` | Lattice | Local embedding vectors for semantic retrieval. |
| `.lattice/meta.db` | Lattice | Symbol lookup, index metadata, ownership, and graph snapshot manifests. |
| `.lattice/languages.toml` | Developer | Optional language mappings read by `lattice init`. |
| [`skills/lattice/SKILL.md`](skills/lattice/SKILL.md) | Contributors | Source of the installed OpenCode skill. Edit this file, rebuild, and reinstall to test a skill change. |

Generated index data stays out of Git. Treat it and any exports as private
project data. See [SECURITY.md](SECURITY.md) for reporting and handling guidance.

## Contribution gates

1. Define the intended behavior and expected result before changing code. For a
   bug fix, reproduce the failure through a focused test. For parser and graph
   changes, use a fixture with hand-countable ground truth.
2. Run the focused test, then `cargo build --locked --offline` and
   `cargo test --locked --offline`. Use cached Cargo dependencies and MiniLM
   files. Verify the commands exit successfully without warnings. Do not
   weaken a test to make a change pass.
3. Update the [README](README.md) in the same change when the CLI contract
   changes. For installer or skill changes, verify the flow below. Record the
   commands and results in the merge request and sign off commits as described
   in [CONTRIBUTING.md](CONTRIBUTING.md).

### OpenCode smoke test

Use a temporary home and a disposable checkout so testing does not change your
regular OpenCode configuration or index. Point `LATTICE_MODEL_DIR` at an
absolute path to an existing model cache. Run the newly built binary with
`HOME` and `XDG_CONFIG_HOME` set to temporary locations.

1. Run `lattice install` and verify that the reported `wrote` paths include
   `opencode/skills/lattice/SKILL.md` under the temporary config directory.
2. Run `lattice init` from the disposable project root, then look up a known
   symbol with `lattice query --symbol '<name>'`. Verify the expected file and
   symbol appear in `hits`.
3. For changes that affect skill loading or agent behavior, open that workspace
   in OpenCode with the same temporary environment and the new binary on `PATH`.
   Load the Lattice skill and check that its suggested query runs from the
   indexed root. A successful `install` only verifies the file was written;
   it does not verify that OpenCode loaded it.
