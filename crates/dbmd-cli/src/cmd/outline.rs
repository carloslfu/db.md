//! `dbmd outline <file>` — section + sub-section outline of one file.
//!
//! Thin wrapper: read the single file directly, strip a leading YAML
//! frontmatter block, extract its `##`+ sections, and print the nested outline
//! (text) or the structured outline (`--json`). Like its twin `dbmd sections`,
//! this is a **single-file read** — it does NOT require a db.md store. All
//! section parsing lives in `dbmd_core::parser::extract_sections`; this body
//! only reads the file and formats.

use std::path::{Path, PathBuf};

use dbmd_core::parser::{extract_sections, Section};

use crate::cli::OutlineArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// A single file's section hierarchy: the file path plus its `##` sections.
///
/// Built directly from a one-file read (no store), so `outline` behaves like
/// `sections` and works outside a db.md store.
struct Outline {
    file: PathBuf,
    sections: Vec<Section>,
}

/// Run `dbmd outline`.
///
/// Reads the named file relative to `--dir` (default `.`) without opening a
/// store. Previously this opened the store with `Store::open_strict`, which
/// failed `NOT_A_STORE` (exit 3) outside a db.md store even though listing one
/// file's headings needs no `DB.md`. That diverged from the twin `dbmd
/// sections <file>`, which reads any file directly; both single-file views now
/// behave identically.
pub fn run(ctx: &Context, args: &OutlineArgs) -> CliResult {
    // Resolve the file against `--dir` when relative (mirrors the old
    // store-relative resolution) and display it the same way: store-relative
    // when it lives under `--dir`, otherwise as given.
    let dir = Path::new(&args.dir);
    let given = Path::new(&args.file);
    let abs = if given.is_absolute() {
        given.to_path_buf()
    } else {
        dir.join(given)
    };
    let display = abs.strip_prefix(dir).unwrap_or(given).to_path_buf();

    let text = std::fs::read_to_string(&abs).map_err(|e| {
        CliError::new(ExitCode::Runtime, "IO_ERROR", e.to_string())
            .with_hint(format!("could not read `{}`", args.file))
    })?;
    let body = strip_frontmatter(&text);
    let sections = extract_sections(body);

    let outline = Outline {
        file: display,
        sections,
    };

    if ctx.json {
        emit_json(&outline);
    } else {
        emit_text(&outline);
    }
    Ok(())
}

/// Return the file body with a leading YAML frontmatter block removed, so
/// section line numbers count from the first body line (matching the parser's
/// body frame). If the text does not open with a `---` fence, it is all body.
///
/// Lenient by design — an outline never fails just because a file is missing
/// frontmatter (mirrors `dbmd_core::render::outline`'s former body frame, which
/// is why this strip is duplicated here rather than routed through the strict
/// `parser::read_file`, which errors on missing frontmatter).
fn strip_frontmatter(text: &str) -> &str {
    // The opening fence must be the very first line, exactly `---`.
    let after_open = match text.strip_prefix("---\n") {
        Some(rest) => rest,
        None => match text.strip_prefix("---\r\n") {
            Some(rest) => rest,
            None => return text,
        },
    };

    // Find the closing `---` line; the body is everything after it.
    let mut search_from = 0usize;
    while let Some(rel_idx) = after_open[search_from..].find("---") {
        let idx = search_from + rel_idx;
        let at_line_start = idx == 0 || after_open.as_bytes()[idx - 1] == b'\n';
        let after = &after_open[idx + 3..];
        let line_ends = after.is_empty()
            || after.starts_with('\n')
            || after.starts_with("\r\n")
            || after.starts_with('\r');
        if at_line_start && line_ends {
            // Skip past the closing fence's own line terminator.
            if let Some(stripped) = after.strip_prefix("\r\n") {
                return stripped;
            }
            if let Some(stripped) = after.strip_prefix('\n') {
                return stripped;
            }
            if let Some(stripped) = after.strip_prefix('\r') {
                return stripped;
            }
            return after; // closing fence is the last line, no trailing body
        }
        search_from = idx + 3;
    }

    // Unterminated frontmatter: treat the whole thing as body rather than error.
    text
}

/// Nested text outline: each `##`+ heading, indented by `(level - 2) * 2`
/// spaces so `##` is flush-left, `###` indents one step, and so on. A
/// heading-free file prints nothing (exit 0).
fn emit_text(outline: &Outline) {
    for section in &outline.sections {
        let indent = "  ".repeat(section.level.saturating_sub(2) as usize);
        println!("{indent}{}", section.heading);
    }
}

/// Structured outline as `{file, sections:[{heading, level, line}]}`. The
/// section `body` is intentionally omitted — outline is a navigational view;
/// `dbmd sections` is the body-bearing one.
fn emit_json(outline: &Outline) {
    let sections: Vec<serde_json::Value> = outline
        .sections
        .iter()
        .map(|s| {
            serde_json::json!({
                "heading": s.heading,
                "level": s.level,
                "line": s.line,
            })
        })
        .collect();
    let out = serde_json::json!({
        "file": path_str(&outline.file),
        "sections": sections,
    });
    println!(
        "{}",
        serde_json::to_string(&out).expect("serialize outline")
    );
}

/// Render a store-relative path with `/` separators (never `\`).
fn path_str(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}
