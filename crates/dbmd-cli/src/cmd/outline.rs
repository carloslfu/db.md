//! `dbmd outline <file>` — section + sub-section outline of one file.
//!
//! Thin wrapper: read the single file directly, extract its `##`+ sections, and
//! print the nested outline (text) or the structured outline (`--json`). Like
//! its twin `dbmd sections`, this is a **single-file read** — it does NOT
//! require a db.md store. All section parsing (frontmatter offset included)
//! lives in `dbmd_core::parser::extract_sections_in_file`, which numbers
//! `Section::line` against the source file; this body only reads and formats.

use std::path::{Path, PathBuf};

use dbmd_core::parser::{extract_sections_in_file, Section};

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
    let sections = extract_sections_in_file(&text);

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
