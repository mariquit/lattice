# Contributing to Lattice

Use **Lattice** in prose and `lattice` for the CLI and OpenCode skill ID.

## Licensing of contributions

Lattice is licensed under the [Apache License, Version 2.0](LICENSE). By
submitting a contribution, you agree to license it under the same terms. There
is no CLA. You retain copyright to your work.

Sign off each commit under the [Developer Certificate of Origin 1.1](DCO):

```bash
git commit -s
```

Use your real name and a working email address in the `Signed-off-by` trailer.

### Copyright headers

Add these two lines to new first-party Rust source files, using the appropriate
year and copyright holder:

```rust
// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>
```

In authored skill Markdown, put both notices in YAML comments just after the
opening `---` so the frontmatter remains valid and the notice is not part of
the skill body. Do not insert lines into parser fixtures whose line positions
are part of the test contract. Preserve applicable third-party notices, and
mark modified files as required by Apache-2.0 §4.

Do not copy code from proprietary, unlicensed, or license-incompatible sources.
Identify adapted material in the merge request and record the applicable
attribution in [NOTICE](NOTICE). See [TRADEMARK.md](TRADEMARK.md) for naming
guidance when redistributing Lattice or a fork.

## Build and test

Use a stable Rust toolchain. Provision the local MiniLM cache with `lattice
install`, or set `LATTICE_MODEL_DIR` to an existing model directory. Then run
the locked build and test suite from the repository root:

```bash
cargo build --locked
cargo test --locked --offline
```

The offline test run requires cached Cargo dependencies and model files. Keep
new and changed behavior covered by tests. For graph or parser changes, use
fixtures with hand-countable expected results. See [AGENTS.md](AGENTS.md) for
repository conventions. Edit the OpenCode skill in
[`skills/lattice/SKILL.md`](skills/lattice/SKILL.md), not in the installed copy.
Follow [WORKFLOW.md](WORKFLOW.md) for the verification sequence and OpenCode
smoke test.

## Commits and merge requests

Use [Conventional Commits](https://www.conventionalcommits.org/), such as
`fix(query): reject unsupported branch scopes`. Explain why the change is
needed, keep it focused, and include the commands you ran to verify it. Update
[README.md](README.md) when CLI behavior changes.

For ordinary bugs, open a GitLab issue with the output of `lattice --version`,
your operating system, the exact command, and a minimal reproduction. Redact
secrets, private paths, and confidential source. Report vulnerabilities through
[SECURITY.md](SECURITY.md), not the issue tracker. Participation is subject to
the [Code of Conduct](CODE_OF_CONDUCT.md).
