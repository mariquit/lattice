# Security Policy

## Reporting a vulnerability

Email [John Anthony Mariquit](mailto:john@mariquit.com) privately. Use the
subject `SECURITY: lattice - <short summary>`. Do not open a public issue,
merge request, or discussion for a suspected vulnerability. If you want to
encrypt the report, ask for a key before sending details.

Include the output of `lattice --version`, your operating system, the
affected command, a minimal reproduction, and the impact you observed. If the
OpenCode skill is involved, include the OpenCode version. Redact secrets and
private source, paths, and Git history unless they are necessary to reproduce
the issue.

## Response and supported versions

The maintainer aims to acknowledge reports within five working days and
provide an initial assessment within 14 days. These are best-effort targets.
Fix or mitigation timing depends on severity. Please allow time for a fix
before public disclosure, and mention any disclosure deadline in your report.

Only the latest commit on `main` receives security fixes. There are no backports
to earlier tags.

## Scope

Lattice is a local CLI, not a server. Reports are in scope for unintended
file access or writes during indexing, querying, or export; unsafe handling of
untrusted project or index data; disclosure of private information by the CLI;
and installer changes that corrupt unrelated OpenCode configuration. The model
installer can access the network. Indexing and queries use local model files.

Report dependency vulnerabilities upstream unless Lattice's use of the
dependency creates the exposure. Report host-agent behavior to the host
maintainers unless the issue originates in Lattice's integration.

Treat `.lattice/` as private project data. The index and its exports can
contain source-derived information and Git metadata. Review query output before
passing it to an external model or attaching it to a public report.
