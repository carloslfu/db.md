//! `dbmd sections <file>` — list the `##` sections in a file.
//!
//! Thin wrapper: parse [`SectionsArgs`], read the file via the
//! `dbmd_core::parser` read path, run the section extractor, and print each
//! `##`+ heading (text: `<indent><heading>  (L<line>)`) or a structured array
//! (`--json`). All logic — frontmatter split, fenced-code-aware heading scan —
//! lives in `dbmd_core::parser::extract_sections`; this body only formats.

use std::path::Path;

use dbmd_core::parser::{extract_sections, read_file, Section};

use crate::cli::SectionsArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult};

/// Run `dbmd sections`.
pub fn run(ctx: &Context, args: &SectionsArgs) -> CliResult {
    let path = Path::new(&args.file);

    // The parser read path returns (frontmatter, verbatim body); sections are a
    // property of the body. A read / frontmatter error bubbles as a runtime
    // error via the `ParseError -> dbmd_core::Error -> CliError` chain.
    let (_frontmatter, body) = read_file(path).map_err(|e| map_parse_error(e, &args.file))?;
    let sections = extract_sections(&body);

    if ctx.json {
        print!("{}", sections_json(&sections));
    } else {
        print!("{}", sections_text(&sections));
    }
    Ok(())
}

/// Human form: one heading per line, indented two spaces per level past `##`,
/// with a right-aligned 1-based source line. Empty (no `##`+ headings) prints
/// nothing — a clean, pipe-safe "no sections" signal.
fn sections_text(sections: &[Section]) -> String {
    let mut out = String::new();
    for s in sections {
        // `##` is depth 2 and sits flush-left; each deeper level indents two
        // spaces so the outline nesting is visible at a glance.
        let indent = "  ".repeat(s.level.saturating_sub(2) as usize);
        out.push_str(&format!("{indent}{}  (L{})\n", s.heading, s.line));
    }
    out
}

/// Machine form: a JSON array of `{heading, level, line}` — the body slice is
/// omitted (use `dbmd outline` for spans); this command answers "what sections
/// exist". Pretty-printed with a trailing newline for stable snapshots.
fn sections_json(sections: &[Section]) -> String {
    let arr: Vec<serde_json::Value> = sections
        .iter()
        .map(|s| {
            serde_json::json!({
                "heading": s.heading,
                "level": s.level,
                "line": s.line,
            })
        })
        .collect();
    let mut s = serde_json::to_string_pretty(&serde_json::Value::Array(arr))
        .unwrap_or_else(|_| "[]".to_string());
    s.push('\n');
    s
}

/// Map a parser error to a CLI error. A missing file or unreadable path is a
/// runtime failure; the `From<ParseError>` path on `dbmd_core::Error` already
/// gives the right exit code, so we route through it and annotate the file.
fn map_parse_error(err: dbmd_core::ParseError, file: &str) -> CliError {
    let core: dbmd_core::Error = err.into();
    CliError::from(core).with_hint(format!("could not read sections from `{file}`"))
}
