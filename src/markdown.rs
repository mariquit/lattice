// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Markdown sectioniser: splits a Markdown document into heading-scoped
//! sections for symbol-granular embedding.
//!
//! Markdown has no functions, so the embedding unit is an ATX heading section:
//! a heading line plus every line up to (but excluding) the next heading of any
//! level. This keeps a retrieval hit interpretable — it lands on a heading
//! rather than an averaged whole-file vector.
//!
//! Rules (all covered by the unit tests at the bottom of this file):
//!
//! - Only ATX headings (`#` .. `######`) split. Setext (`===` underline) is not
//!   treated as a heading; it stays part of the surrounding section body.
//! - A `#` inside a fenced code block (``` or ~~~) is NOT a heading. This is the
//!   single most common false positive in real documents.
//! - Content before the first heading becomes a preamble section titled with the
//!   file stem, so nothing is silently dropped.
//! - A document with no headings yields exactly ONE whole-file section.
//! - A section longer than [`MAX_SECTION_LINES`] is split into numbered parts,
//!   each carrying the parent heading in its symbol so the hit stays readable.
//! - Line numbers are 1-based inclusive, matching [`crate::vectors::Span`].
//!
//! This module is pure — no I/O, no allocation beyond the returned sections.

/// Maximum body lines per emitted section before it is split into parts.
///
/// A single vector averages everything it covers, so an 800-line design doc as
/// one embedding retrieves poorly. 150 lines keeps a section specific enough to
/// be useful while avoiding one-vector-per-paragraph fragmentation.
pub const MAX_SECTION_LINES: usize = 150;

/// One heading-scoped slice of a Markdown document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// Heading text with `#` markers and surrounding whitespace stripped.
    /// For a preamble or heading-less document this is the file stem.
    /// For a split part this carries a ` (part N)` suffix.
    pub heading: String,
    /// ATX level 1-6. `0` for a preamble / heading-less section, which has no
    /// heading marker of its own.
    pub level: usize,
    /// 1-based inclusive first line of the section (the heading line itself
    /// when the section has a heading).
    pub start_line: usize,
    /// 1-based inclusive last line of the section.
    pub end_line: usize,
    /// Raw text of the section, including its heading line.
    pub text: String,
}

/// Split `source` into heading-scoped sections.
///
/// `stem` is used to title the preamble / heading-less section (normally the
/// file name without extension). Never returns an empty vector for non-empty
/// input; an empty or whitespace-only document returns no sections, because
/// there is nothing meaningful to embed.
pub fn parse_sections(source: &str, stem: &str) -> Vec<Section> {
    // Normalise CRLF so line splitting and re-joining are stable. `str::lines`
    // already strips a trailing \r, but we re-join with \n, so doing this once
    // up front keeps `text` free of stray carriage returns.
    let lines: Vec<&str> = source
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();

    if lines.iter().all(|l| l.trim().is_empty()) {
        return Vec::new();
    }

    // Pass 1: find heading line indices, skipping fenced code blocks.
    let mut heading_idx: Vec<(usize, usize, String)> = Vec::new(); // (line_idx, level, text)
    let mut fence: Option<&str> = None;

    for (i, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim_start();

        // Fence open/close. A fence closes only with the same marker type, so a
        // ``` block containing ~~~ stays open.
        if let Some(marker) = fence {
            if trimmed.starts_with(marker) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            fence = Some("```");
            continue;
        }
        if trimmed.starts_with("~~~") {
            fence = Some("~~~");
            continue;
        }

        if let Some((level, text)) = parse_atx_heading(raw) {
            heading_idx.push((i, level, text));
        }
    }

    let mut out: Vec<Section> = Vec::new();

    // Preamble: content before the first heading (or the whole document when
    // there are no headings at all).
    let first_heading = heading_idx
        .first()
        .map(|(i, _, _)| *i)
        .unwrap_or(lines.len());
    if first_heading > 0 {
        let body = &lines[0..first_heading];
        if body.iter().any(|l| !l.trim().is_empty()) {
            push_with_split(&mut out, stem, 0, 1, body);
        }
    }

    // Each heading owns lines up to the next heading of ANY level. Nesting is
    // deliberately flat: a `##` under a `#` becomes its own section rather than
    // being absorbed, so retrieval can land on the specific subsection.
    for (n, (start_idx, level, text)) in heading_idx.iter().enumerate() {
        let end_idx = heading_idx
            .get(n + 1)
            .map(|(i, _, _)| *i)
            .unwrap_or(lines.len());
        let body = &lines[*start_idx..end_idx];
        push_with_split(&mut out, text, *level, start_idx + 1, body);
    }

    out
}

/// The heading a split part was cut from, if `heading` is `"Title (part N)"`.
pub fn part_base(heading: &str) -> Option<&str> {
    let (base, rest) = heading.rsplit_once(" (part ")?;
    let n = rest.strip_suffix(')')?;
    if base.is_empty() || n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(base)
}

/// Span of the whole section a split part belongs to.
///
/// Returned only for headings produced by the 150-line split (`"Title (part N)"`).
/// Unsplit sections are already the parent, so they return `None`.
pub struct ParentSpan {
    pub name: String,
    pub start_line: usize,
    pub end_line: usize,
}

pub fn parent_span(source: &str, stem: &str, heading: &str) -> Option<ParentSpan> {
    let base = part_base(heading)?;
    let sections = parse_sections(source, stem);
    let mut start = usize::MAX;
    let mut end = 0usize;
    let mut found = false;
    for sec in &sections {
        if part_base(&sec.heading) == Some(base) {
            found = true;
            start = start.min(sec.start_line);
            end = end.max(sec.end_line);
        }
    }
    if !found {
        return None;
    }
    Some(ParentSpan {
        name: base.to_string(),
        start_line: start,
        end_line: end,
    })
}

/// Parse an ATX heading line into `(level, text)`.
///
/// Requires 1-6 leading `#` followed by whitespace or end-of-line, per
/// CommonMark. `#hashtag` is therefore not a heading. A trailing closing
/// sequence (`## Foo ##`) is stripped.
fn parse_atx_heading(raw: &str) -> Option<(usize, String)> {
    // Up to 3 leading spaces are allowed; 4+ makes it an indented code block.
    let indent = raw.len() - raw.trim_start_matches(' ').len();
    if indent >= 4 {
        return None;
    }
    let s = raw.trim_start_matches(' ');

    let hashes = s.len() - s.trim_start_matches('#').len();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &s[hashes..];
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None; // e.g. "#hashtag"
    }

    let text = rest.trim().trim_end_matches('#').trim().to_string();
    Some((hashes, text))
}

/// Push `body` as one section, splitting into numbered parts when it exceeds
/// [`MAX_SECTION_LINES`].
fn push_with_split(
    out: &mut Vec<Section>,
    heading: &str,
    level: usize,
    start_line: usize,
    body: &[&str],
) {
    if body.len() <= MAX_SECTION_LINES {
        out.push(Section {
            heading: heading.to_string(),
            level,
            start_line,
            end_line: start_line + body.len() - 1,
            text: body.join("\n"),
        });
        return;
    }

    for (part, chunk) in body.chunks(MAX_SECTION_LINES).enumerate() {
        let offset = part * MAX_SECTION_LINES;
        out.push(Section {
            heading: format!("{heading} (part {})", part + 1),
            level,
            start_line: start_line + offset,
            end_line: start_line + offset + chunk.len() - 1,
            text: chunk.join("\n"),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_atx_headings() {
        let src = "# One\nalpha\n\n## Two\nbeta\n";
        let s = parse_sections(src, "doc");
        assert_eq!(s.len(), 2);

        assert_eq!(s[0].heading, "One");
        assert_eq!(s[0].level, 1);
        assert_eq!(s[0].start_line, 1);
        assert_eq!(s[0].end_line, 3);

        assert_eq!(s[1].heading, "Two");
        assert_eq!(s[1].level, 2);
        assert_eq!(s[1].start_line, 4);
        assert_eq!(s[1].end_line, 5);
    }

    #[test]
    fn hash_inside_fenced_block_is_not_a_heading() {
        let src = "# Real\n```sh\n# not a heading\n```\ntail\n";
        let s = parse_sections(src, "doc");
        assert_eq!(s.len(), 1, "fenced # must not split");
        assert_eq!(s[0].heading, "Real");
        assert_eq!(s[0].end_line, 5);
    }

    #[test]
    fn tilde_fence_also_guards() {
        let src = "# Real\n~~~\n# nope\n~~~\n";
        let s = parse_sections(src, "doc");
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn no_heading_file_is_one_section() {
        let src = "just prose\nmore prose\n";
        let s = parse_sections(src, "notes");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].heading, "notes");
        assert_eq!(s[0].level, 0);
        assert_eq!(s[0].start_line, 1);
        assert_eq!(s[0].end_line, 2);
    }

    #[test]
    fn preamble_before_first_heading_is_kept() {
        let src = "intro line\n\n# First\nbody\n";
        let s = parse_sections(src, "doc");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].heading, "doc");
        assert_eq!(s[0].level, 0);
        assert_eq!(s[0].start_line, 1);
        assert_eq!(s[1].heading, "First");
        assert_eq!(s[1].start_line, 3);
    }

    #[test]
    fn empty_document_yields_nothing() {
        assert!(parse_sections("", "doc").is_empty());
        assert!(parse_sections("\n\n   \n", "doc").is_empty());
    }

    #[test]
    fn oversized_section_is_split_into_parts() {
        let mut src = String::from("# Big\n");
        for i in 0..(MAX_SECTION_LINES * 2) {
            src.push_str(&format!("line {i}\n"));
        }
        let s = parse_sections(&src, "doc");
        assert_eq!(s.len(), 3, "301 lines / 150 => 3 parts");
        assert_eq!(s[0].heading, "Big (part 1)");
        assert_eq!(s[1].heading, "Big (part 2)");
        assert_eq!(s[2].heading, "Big (part 3)");

        // Parts must be contiguous and non-overlapping.
        assert_eq!(s[0].start_line, 1);
        assert_eq!(s[1].start_line, s[0].end_line + 1);
        assert_eq!(s[2].start_line, s[1].end_line + 1);

        let parent = parent_span(&src, "doc", "Big (part 2)").expect("split part has a parent");
        assert_eq!(parent.name, "Big");
        assert_eq!(parent.start_line, 1);
        assert_eq!(parent.end_line, s[2].end_line);
        assert!(parent_span(&src, "doc", "Big").is_none());
    }

    #[test]
    fn crlf_is_normalised() {
        let src = "# One\r\nalpha\r\n";
        let s = parse_sections(src, "doc");
        assert_eq!(s.len(), 1);
        assert!(!s[0].text.contains('\r'), "no stray CR in embedded text");
    }

    #[test]
    fn hashtag_is_not_a_heading() {
        let src = "#hashtag not a heading\nbody\n";
        let s = parse_sections(src, "doc");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].heading, "doc", "should fall through to preamble");
    }

    #[test]
    fn closing_hashes_are_stripped() {
        let src = "## Foo ##\nbody\n";
        let s = parse_sections(src, "doc");
        assert_eq!(s[0].heading, "Foo");
    }

    #[test]
    fn deep_levels_are_flat_not_nested() {
        let src = "# A\na\n## B\nb\n### C\nc\n";
        let s = parse_sections(src, "doc");
        assert_eq!(s.len(), 3);
        assert_eq!(s.iter().map(|x| x.level).collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn indented_four_spaces_is_code_not_heading() {
        let src = "text\n\n    # indented code\n";
        let s = parse_sections(src, "doc");
        assert_eq!(s.len(), 1, "4-space indent is a code block");
        assert_eq!(s[0].heading, "doc");
    }
}
