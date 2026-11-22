// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Source-file classification: distinguishes production source from test,
//! fixture, generated, and vendored files so the query layer can filter them.
//!
//! `classify` is pure (no I/O, no allocations beyond the split on `/`) and
//! deterministic. It operates on repo-relative POSIX-style paths as stored in
//! the FTS and vector indexes. Precedence (first match wins, evaluated by
//! splitting the path on `/`):
//!
//!   Vendored   — any path component is `vendor` or `third_party`
//!   Generated  — filename ends `.min.js` or `_pb2.py`, or contains `.generated.`
//!   Fixture    — any path component is `fixtures` or `__fixtures__`
//!   Test       — any path component is `tests`, `test`, or `__tests__`;
//!                OR filename matches `*.test.*`, `*.spec.*`, `*_test.<ext>`,
//!                `test_*.py`, or `conftest.py`
//!   Source     — everything else

/// Classification of a source file by its purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceClass {
    Source,
    Test,
    Fixture,
    Generated,
    Vendored,
}

impl SourceClass {
    /// Lowercase string label used in JSON envelopes and FTS rows.
    pub fn as_str(self) -> &'static str {
        match self {
            SourceClass::Source => "source",
            SourceClass::Test => "test",
            SourceClass::Fixture => "fixture",
            SourceClass::Generated => "generated",
            SourceClass::Vendored => "vendored",
        }
    }
}

/// Classify a repo-relative path. Pure function — no I/O, no allocations
/// beyond splitting. POSIX `/` separator assumed; Windows paths not normalised
/// here (the callers already normalise to `/`).
pub fn classify(path: &str) -> SourceClass {
    // Normalise: strip a leading `./` if present (tolerate both forms).
    let path = path.strip_prefix("./").unwrap_or(path);

    let components: Vec<&str> = path.split('/').collect();
    // The last component is the filename.
    let filename = components.last().copied().unwrap_or(path);

    // ── Vendored (highest precedence) ────────────────────────────────────────
    for comp in &components[..components.len().saturating_sub(1)] {
        if *comp == "vendor" || *comp == "third_party" {
            return SourceClass::Vendored;
        }
    }

    // ── Generated ────────────────────────────────────────────────────────────
    if filename.ends_with(".min.js")
        || filename.ends_with("_pb2.py")
        || filename.contains(".generated.")
    {
        return SourceClass::Generated;
    }

    // ── Fixture ───────────────────────────────────────────────────────────────
    for comp in &components[..components.len().saturating_sub(1)] {
        if *comp == "fixtures" || *comp == "__fixtures__" {
            return SourceClass::Fixture;
        }
    }

    // ── Test ─────────────────────────────────────────────────────────────────
    // Directory components named `tests`, `test`, or `__tests__`.
    for comp in &components[..components.len().saturating_sub(1)] {
        if *comp == "tests" || *comp == "test" || *comp == "__tests__" {
            return SourceClass::Test;
        }
    }
    // Filename patterns.
    if is_test_filename(filename) {
        return SourceClass::Test;
    }

    SourceClass::Source
}

/// Returns true when the filename alone (no directory) marks the file as a test.
fn is_test_filename(filename: &str) -> bool {
    // conftest.py
    if filename == "conftest.py" {
        return true;
    }
    // test_*.py
    if filename.starts_with("test_") && filename.ends_with(".py") {
        return true;
    }
    // *_test.<ext> — split on the last `.` to get stem + ext, then check stem
    // suffix `_test`.
    if let Some(dot) = filename.rfind('.') {
        let stem = &filename[..dot];
        if stem.ends_with("_test") {
            return true;
        }
        // *.test.* and *.spec.* — check if the stem (without the last extension)
        // contains `.test` or `.spec` as a dot-delimited segment.
        // e.g. `foo.test.ts` → stem = `foo.test`, contains `.test`.
        if stem.contains(".test") || stem.contains(".spec") {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_table() {
        let cases: &[(&str, SourceClass)] = &[
            // Source
            ("src/lib.rs", SourceClass::Source),
            ("src/auth/token.rs", SourceClass::Source),
            ("main.py", SourceClass::Source),
            // Vendored — component anywhere in dir prefix
            ("vendor/lib.rs", SourceClass::Vendored),
            ("vendor/foo/bar.rs", SourceClass::Vendored),
            ("third_party/lib.py", SourceClass::Vendored),
            // Generated — filename rules
            ("dist/bundle.min.js", SourceClass::Generated),
            ("proto/x_pb2.py", SourceClass::Generated),
            ("src/foo.generated.rs", SourceClass::Generated),
            // Fixture — `fixtures` dir component
            ("tests/fixtures/data.rs", SourceClass::Fixture),
            ("__fixtures__/mock.ts", SourceClass::Fixture),
            // Test — directory component
            ("tests/core_test.rs", SourceClass::Test),
            ("src/tests/auth_test.rs", SourceClass::Test),
            ("__tests__/foo.ts", SourceClass::Test),
            // Test — filename patterns
            ("src/auth_test.rs", SourceClass::Test),
            ("src/foo.test.ts", SourceClass::Test),
            ("src/foo.spec.ts", SourceClass::Test),
            ("test_auth.py", SourceClass::Test),
            ("conftest.py", SourceClass::Test),
            // Vendored beats test
            ("vendor/tests/x.rs", SourceClass::Vendored),
            // Fixture beats test (fixtures/ is inside tests/ but the loop hits
            // fixtures first when it appears before tests in the path).
            ("src/__fixtures__/data.rs", SourceClass::Fixture),
        ];
        for (path, expected) in cases {
            let got = classify(path);
            assert_eq!(
                got, *expected,
                "classify({path:?}) = {:?}, want {:?}",
                got, expected
            );
        }
    }
}
