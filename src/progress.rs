// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Human-facing progress footer for long-running `lattice init` runs.
//!
//! A single in-place spinner line on **stderr** naming the current stage and
//! file, so a multi-minute cold index is visibly alive instead of looking hung.
//!
//! Three invariants make this safe to thread through the index pipeline:
//!
//! 1. **stdout is never touched.** The one-JSON-envelope-per-invocation
//!    contract is unaffected; progress is prose, and prose goes to stderr.
//! 2. **Off unless explicitly turned on.** `Progress::hidden()` is a no-op sink
//!    and is what the shared `index::run` uses, so query-time freshness
//!    self-heal (which re-runs the SAME pipeline) stays silent. Only
//!    `main.rs::cmd_init` opts in. This is a type-level guarantee, not a
//!    runtime flag someone can forget to reset.
//! 3. **Auto-disabled when stderr is not a terminal.** Piped/redirected/CI
//!    output is byte-identical to the no-progress behavior, so machine
//!    consumers and the test suite never see spinner frames or ANSI escapes.
//!
//! `LATTICE_PROGRESS` overrides the terminal sniff: `never` forces off,
//! `always` forces on (used by the gate to assert real frames without a pty).
//! `indicatif` is already in the dependency graph via `hf-hub`, so this adds no
//! new build cost and stays pure-Rust.

use std::io::Write;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle, TermLike};

/// Spinner tick interval. Fast enough to look alive, slow enough that the
/// redraw cost is irrelevant next to a BERT forward pass.
const TICK: Duration = Duration::from_millis(120);

/// Max width of the footer's variable part before the middle is elided, so the
/// footer stays one line on a normal terminal.
const MAX_DETAIL: usize = 60;

/// A plain-stderr `TermLike` used ONLY by `LATTICE_PROGRESS=always`.
///
/// `ProgressDrawTarget::stderr()` refuses to draw when stderr is not a tty
/// (correctly — that is invariant 3). To let the gate assert that real frames
/// are produced, without allocating a pty, this target writes the same escape
/// sequences to a plain, non-tty stderr.
#[derive(Debug)]
struct PlainStderr;

impl TermLike for PlainStderr {
    fn width(&self) -> u16 {
        80
    }

    fn move_cursor_up(&self, n: usize) -> std::io::Result<()> {
        self.write_str(&format!("\x1b[{n}A"))
    }

    fn move_cursor_down(&self, n: usize) -> std::io::Result<()> {
        self.write_str(&format!("\x1b[{n}B"))
    }

    fn move_cursor_right(&self, n: usize) -> std::io::Result<()> {
        self.write_str(&format!("\x1b[{n}C"))
    }

    fn move_cursor_left(&self, n: usize) -> std::io::Result<()> {
        self.write_str(&format!("\x1b[{n}D"))
    }

    fn write_line(&self, s: &str) -> std::io::Result<()> {
        self.write_str(s)?;
        self.write_str("\n")
    }

    fn write_str(&self, s: &str) -> std::io::Result<()> {
        std::io::stderr().write_all(s.as_bytes())
    }

    fn clear_line(&self) -> std::io::Result<()> {
        self.write_str("\r\x1b[2K")
    }

    fn flush(&self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

/// A progress footer handle. Cheap to clone-free pass by reference; every method
/// is a no-op when hidden.
pub struct Progress {
    bar: Option<ProgressBar>,
}

impl Progress {
    /// A no-op sink. Used by every non-`init` caller of the index pipeline.
    pub fn hidden() -> Self {
        Self { bar: None }
    }

    /// A live footer, unless stderr is not a terminal or `LATTICE_PROGRESS`
    /// says otherwise. Falls back to `hidden()` in those cases.
    pub fn stderr() -> Self {
        let forced = match std::env::var("LATTICE_PROGRESS").as_deref() {
            Ok("never") | Ok("0") | Ok("off") => return Self::hidden(),
            Ok("always") | Ok("1") | Ok("on") => true,
            _ => false,
        };

        // `ProgressDrawTarget::stderr()` already hides itself for a non-tty and
        // for TERM=dumb; `is_hidden()` below detects that so we can degrade to a
        // true no-op instead of paying for hidden draw calls.
        let bar = ProgressBar::new_spinner().with_style(
            ProgressStyle::with_template("{spinner:.cyan} lattice: {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner())
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
        );
        if forced {
            // Draw unconditionally (no tty required) — makes the gate possible.
            bar.set_draw_target(ProgressDrawTarget::term_like_with_hz(
                Box::new(PlainStderr),
                8,
            ));
        } else {
            bar.set_draw_target(ProgressDrawTarget::stderr());
            if bar.is_hidden() {
                return Self::hidden();
            }
        }
        bar.enable_steady_tick(TICK);
        Self { bar: Some(bar) }
    }

    /// True when a live footer is being drawn. Callers use this to suppress the
    /// line-per-N `eprintln!` progress notes that the footer supersedes, so the
    /// two never interleave.
    pub fn is_active(&self) -> bool {
        self.bar.is_some()
    }

    /// Replace the footer text with `stage` plus an optional `detail`
    /// (typically the current file), e.g. `parse 12/102 · src/index.rs`.
    pub fn stage(&self, stage: &str, detail: Option<&str>) {
        if let Some(bar) = &self.bar {
            bar.set_message(match detail {
                Some(d) => format!("{stage} · {}", elide(d, MAX_DETAIL)),
                None => stage.to_string(),
            });
        }
    }

    /// `stage` with an `n/total` counter: `embed 12/102 · src/index.rs`.
    pub fn step(&self, stage: &str, n: usize, total: usize, detail: Option<&str>) {
        if self.bar.is_some() {
            self.stage(&format!("{stage} {n}/{total}"), detail);
        }
    }

    /// Print a durable line without the footer overwriting it (or being
    /// overwritten by it). Equivalent to `eprintln!` when hidden.
    pub fn note(&self, msg: &str) {
        match &self.bar {
            Some(bar) => bar.suspend(|| eprintln!("{msg}")),
            None => eprintln!("{msg}"),
        }
    }

    /// Erase the footer. Must be called before emitting the final envelope /
    /// summary so no partial spinner line is left on screen.
    pub fn finish(&self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }
}

/// Shorten `s` to at most `max` chars by eliding the MIDDLE, keeping the head
/// and the tail. Paths are most identifiable at both ends (crate dir + file
/// name), so a plain truncation of either end is the wrong trade.
///
/// Operates on chars, not bytes, so a multi-byte path can never be split
/// mid-codepoint (that would panic on a `&str` slice).
fn elide(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    // Reserve one char for the ellipsis; split the rest tail-heavy so the file
    // name survives.
    let keep = max.saturating_sub(1);
    let tail = keep * 2 / 3;
    let head = keep - tail;
    let mut out = String::with_capacity(max + 2);
    out.extend(&chars[..head]);
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_progress_is_inert() {
        let p = Progress::hidden();
        assert!(!p.is_active());
        // Must not panic and must not require a terminal.
        p.stage("scan", Some("src/x.rs"));
        p.step("embed", 1, 2, None);
        p.finish();
    }

    #[test]
    fn elide_keeps_head_and_tail_within_budget() {
        let long = "crates/alpha/src/deeply/nested/module/implementation.rs";
        let out = elide(long, 30);
        assert_eq!(out.chars().count(), 30, "elided string must hit the budget");
        assert!(out.starts_with("crates/a"), "head must survive: {out}");
        assert!(out.ends_with("implementation.rs"), "tail must survive: {out}");
        assert!(out.contains('…'));
    }

    #[test]
    fn elide_leaves_short_strings_untouched() {
        assert_eq!(elide("src/x.rs", 60), "src/x.rs");
        // Exactly at the budget is not elided.
        assert_eq!(elide("abcde", 5), "abcde");
    }

    #[test]
    fn elide_never_splits_a_multibyte_char() {
        // Every char is 3 bytes; a byte-based slice would panic here.
        let s = "日本語".repeat(20);
        let out = elide(&s, 10);
        assert_eq!(out.chars().count(), 10);
    }
}
