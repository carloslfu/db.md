// SPDX-License-Identifier: Apache-2.0

//! Body and section editing — the corpus-write primitives behind
//! `dbmd body set/append` and `dbmd section set/append`.
//!
//! Everything here is a pure `&str -> String` transformation over a file's
//! markdown BODY (the verbatim text after the frontmatter block); reading the
//! file, the frozen-page policy, the `updated` re-stamp, the atomic write,
//! and the index write-through all belong to the caller (the CLI bodies),
//! exactly as with every other mutation.
//!
//! Section addressing shares the extractor's boundary rule via
//! [`parser::extract_section_spans`] — a section runs from its heading line
//! to the next heading at an equal-or-shallower level (an `# H1` terminates a
//! span without being a section), fenced code blocks hide headings, and the
//! span text is verbatim. Replacing a section therefore replaces its whole
//! subtree (deeper `###…` sub-sections included), which is the same unit the
//! read views (`sections`, `outline`, `section get`) present.
//!
//! Newline discipline: section edits are STRUCTURAL — inserted content is
//! newline-terminated so a following heading always starts on its own line —
//! while [`append_body`] is RAW (the joint gains a newline when the existing
//! body lacks one, the appended content itself rides verbatim). `dbmd body
//! set` is rawer still and does not come through here: the new body is stored
//! exactly as given.

use crate::parser::{extract_section_spans, SectionSpan};

/// A section-addressing failure. Whole-body operations cannot fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// No section carries the requested heading text.
    SectionNotFound {
        /// The heading text that matched nothing.
        heading: String,
    },
    /// More than one section carries the requested heading text — the address
    /// is ambiguous and the edit refuses rather than guessing.
    SectionAmbiguous {
        /// The heading text that matched more than once.
        heading: String,
        /// The 1-based body lines of every match.
        lines: Vec<u32>,
    },
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::SectionNotFound { heading } => {
                write!(f, "no section with heading `{heading}`")
            }
            EditError::SectionAmbiguous { heading, lines } => {
                let lines: Vec<String> = lines.iter().map(|l| format!("L{l}")).collect();
                write!(
                    f,
                    "heading `{heading}` matches {} sections ({})",
                    lines.len(),
                    lines.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for EditError {}

/// The result of a section edit: the new body plus where the edited (or
/// created) section sits — `line` is 1-based within the body, the
/// [`parser::Section::line`] frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionEdit {
    /// The whole new body.
    pub body: String,
    /// The edited section's heading level (2–6).
    pub level: u8,
    /// The edited section's heading line, 1-based within the body.
    pub line: u32,
}

/// Find the one section addressed by `heading` (exact text match on the
/// extracted heading, surrounding whitespace trimmed from the query).
pub fn find_section<'a>(
    spans: &'a [SectionSpan],
    heading: &str,
) -> Result<&'a SectionSpan, EditError> {
    let want = heading.trim();
    let matches: Vec<&SectionSpan> = spans.iter().filter(|s| s.section.heading == want).collect();
    match matches.as_slice() {
        [] => Err(EditError::SectionNotFound {
            heading: want.to_string(),
        }),
        [one] => Ok(one),
        many => Err(EditError::SectionAmbiguous {
            heading: want.to_string(),
            lines: many.iter().map(|s| s.section.line).collect(),
        }),
    }
}

/// Replace the addressed section's content — everything under its heading
/// line to the span end, deeper sub-sections included — with `content`. The
/// heading line itself is preserved byte-for-byte (gaining a terminating
/// newline only when unterminated content must follow it).
pub fn replace_section(body: &str, heading: &str, content: &str) -> Result<SectionEdit, EditError> {
    let spans = extract_section_spans(body);
    let target = find_section(&spans, heading)?;
    let (start, end, level, line) = (
        target.start,
        target.end,
        target.section.level,
        target.section.line,
    );
    let lines: Vec<&str> = body.split_inclusive('\n').collect();

    let mut out = String::with_capacity(body.len() + content.len());
    out.push_str(&lines[..start].concat());
    let heading_line = lines[start];
    out.push_str(heading_line);
    if !heading_line.ends_with('\n') && !content.is_empty() {
        out.push('\n');
    }
    out.push_str(&terminated(content));
    out.push_str(&lines[end..].concat());
    Ok(SectionEdit {
        body: out,
        level,
        line,
    })
}

/// Append `content` at the end of the addressed section (before the next
/// sibling-or-shallower heading), newline-terminated.
pub fn append_to_section(
    body: &str,
    heading: &str,
    content: &str,
) -> Result<SectionEdit, EditError> {
    let spans = extract_section_spans(body);
    let target = find_section(&spans, heading)?;
    let (end, level, line) = (target.end, target.section.level, target.section.line);
    let lines: Vec<&str> = body.split_inclusive('\n').collect();

    let mut out = lines[..end].concat();
    if !out.ends_with('\n') && !content.is_empty() {
        out.push('\n');
    }
    out.push_str(&terminated(content));
    out.push_str(&lines[end..].concat());
    Ok(SectionEdit {
        body: out,
        level,
        line,
    })
}

/// Append a NEW section at the end of the body: one separating blank line,
/// the `#`-run heading at `level` (2–6), then the newline-terminated content.
pub fn append_section(body: &str, heading: &str, level: u8, content: &str) -> SectionEdit {
    let mut out = String::with_capacity(body.len() + heading.len() + content.len() + 16);
    out.push_str(body);
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.ends_with("\n\n") {
            out.push('\n');
        }
    }
    let line = (out.split_inclusive('\n').count() + 1) as u32;
    out.push_str(&"#".repeat(usize::from(level)));
    out.push(' ');
    out.push_str(heading.trim());
    out.push('\n');
    out.push_str(&terminated(content));
    SectionEdit {
        body: out,
        level,
        line,
    }
}

/// Append raw `content` at the end of the body. The joint gains a newline
/// when the existing body lacks one; the content itself rides verbatim.
pub fn append_body(body: &str, content: &str) -> String {
    let mut out = String::with_capacity(body.len() + content.len() + 1);
    out.push_str(body);
    if !out.is_empty() && !out.ends_with('\n') && !content.is_empty() {
        out.push('\n');
    }
    out.push_str(content);
    out
}

/// Newline-terminate a non-empty block (structural section content must not
/// swallow whatever follows it); empty content stays empty.
fn terminated(content: &str) -> String {
    if content.is_empty() || content.ends_with('\n') {
        content.to_string()
    } else {
        format!("{content}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = "\
intro paragraph

## Status
active since May
detail line

### Sub-note
nested content

## Log
- entry one
";

    #[test]
    fn replace_replaces_the_whole_subtree() {
        let edited = replace_section(BODY, "Status", "replaced\n").unwrap();
        assert_eq!(
            edited.body,
            "intro paragraph\n\n## Status\nreplaced\n## Log\n- entry one\n"
        );
        assert_eq!(edited.level, 2);
        assert_eq!(edited.line, 3);
    }

    #[test]
    fn replace_targets_a_subsection_alone() {
        let edited = replace_section(BODY, "Sub-note", "tightened\n").unwrap();
        assert_eq!(
            edited.body,
            "intro paragraph\n\n## Status\nactive since May\ndetail line\n\n### Sub-note\ntightened\n## Log\n- entry one\n"
        );
        assert_eq!(edited.level, 3);
    }

    #[test]
    fn replace_with_empty_content_leaves_heading_only() {
        let edited = replace_section(BODY, "Log", "").unwrap();
        assert!(edited.body.ends_with("## Log\n"));
    }

    #[test]
    fn append_lands_before_the_next_sibling() {
        let edited = append_to_section(BODY, "Status", "- appended").unwrap();
        assert_eq!(
            edited.body,
            "intro paragraph\n\n## Status\nactive since May\ndetail line\n\n### Sub-note\nnested content\n\n- appended\n## Log\n- entry one\n"
        );
    }

    #[test]
    fn append_at_eof_terminates_cleanly() {
        let edited = append_to_section(BODY, "Log", "- entry two").unwrap();
        assert!(edited.body.ends_with("## Log\n- entry one\n- entry two\n"));
    }

    /// An `# H1` line terminates a section span without being a section — the
    /// extractor's rule, which the splice must share or an edit would swallow
    /// the H1.
    #[test]
    fn h1_terminates_the_span() {
        let body = "## Notes\nold\n# Title\nafter\n";
        let edited = replace_section(body, "Notes", "new\n").unwrap();
        assert_eq!(edited.body, "## Notes\nnew\n# Title\nafter\n");
    }

    /// A `## heading` inside a fenced code block is content, not an address
    /// and not a boundary.
    #[test]
    fn fenced_headings_are_invisible() {
        let body = "## Real\n```\n## Fake\n```\ntail\n";
        assert!(matches!(
            replace_section(body, "Fake", "x"),
            Err(EditError::SectionNotFound { .. })
        ));
        let edited = replace_section(body, "Real", "gone\n").unwrap();
        assert_eq!(edited.body, "## Real\ngone\n");
    }

    #[test]
    fn duplicate_headings_are_ambiguous() {
        let body = "## Twice\na\n## Twice\nb\n";
        match replace_section(body, "Twice", "x") {
            Err(EditError::SectionAmbiguous { lines, .. }) => assert_eq!(lines, vec![1, 3]),
            other => panic!("expected ambiguity, got {other:?}"),
        }
    }

    #[test]
    fn missing_heading_is_not_found() {
        assert!(matches!(
            append_to_section(BODY, "Nope", "x"),
            Err(EditError::SectionNotFound { .. })
        ));
    }

    /// A heading line at EOF without a trailing newline gains one only when
    /// content must follow it.
    #[test]
    fn unterminated_heading_line_edges() {
        let body = "## End";
        let edited = replace_section(body, "End", "x").unwrap();
        assert_eq!(edited.body, "## End\nx\n");
        let untouched = replace_section(body, "End", "").unwrap();
        assert_eq!(untouched.body, "## End");
    }

    #[test]
    fn append_section_separates_with_one_blank_line() {
        let edited = append_section("existing\n", "Fresh", 2, "content");
        assert_eq!(edited.body, "existing\n\n## Fresh\ncontent\n");
        assert_eq!(edited.line, 3);

        let on_empty = append_section("", "Fresh", 3, "content\n");
        assert_eq!(on_empty.body, "### Fresh\ncontent\n");
        assert_eq!(on_empty.line, 1);

        let already_spaced = append_section("existing\n\n", "Fresh", 2, "");
        assert_eq!(already_spaced.body, "existing\n\n## Fresh\n");
    }

    #[test]
    fn append_body_is_raw_with_a_safe_joint() {
        assert_eq!(append_body("a\n", "b"), "a\nb");
        assert_eq!(append_body("a", "b\n"), "a\nb\n");
        assert_eq!(append_body("", "b"), "b");
        assert_eq!(append_body("a\n", ""), "a\n");
    }

    #[test]
    fn find_section_trims_the_query_only() {
        let spans = extract_section_spans(BODY);
        assert!(find_section(&spans, "  Status  ").is_ok());
        assert!(find_section(&spans, "status").is_err());
    }
}
