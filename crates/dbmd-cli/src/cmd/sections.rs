//! `dbmd sections <file>` — list the `##` sections in a file.
//!
//! Thin wrapper: parse [`SectionsArgs`], read the raw file text, run the
//! whole-file section extractor, and print each `##`+ heading (text:
//! `<indent><heading>  (L<line>)`) or a structured array (`--json`). All logic —
//! frontmatter offset, fenced-code-aware heading scan — lives in
//! `dbmd_core::parser::extract_sections_in_file`, which numbers `Section::line`
//! against the source file (1-based) so an agent can jump straight to it; this
//! body only reads the file and formats.

use std::path::Path;

use dbmd_core::parser::{extract_sections_in_file, Section};

use crate::cli::SectionsArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// Run `dbmd sections`.
pub fn run(ctx: &Context, args: &SectionsArgs) -> CliResult {
    let path = Path::new(&args.file);

    // Read the raw file text; a missing / unreadable path is a runtime error
    // (exit 1), mirroring `dbmd outline`. Sections are then extracted with
    // source-relative line numbers (frontmatter offset applied in the parser).
    let text = std::fs::read_to_string(path).map_err(|e| {
        CliError::new(ExitCode::Runtime, "IO_ERROR", e.to_string())
            .with_hint(format!("could not read sections from `{}`", args.file))
    })?;
    let sections = extract_sections_in_file(&text);

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
