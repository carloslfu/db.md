//! `dbmd outline <file>` — section + sub-section outline of one file.
//!
//! Thin wrapper: open the store (current directory — `OutlineArgs` carries no
//! `--dir`), build the `dbmd_core::render::Outline` for the file, and print the
//! nested outline (text) or the structured outline (`--json`). All section
//! parsing lives in `dbmd_core::render`; this body only formats.

use std::path::Path;

use dbmd_core::render::{self, Outline};
use dbmd_core::Store;

use crate::cli::OutlineArgs;
use crate::context::Context;
use crate::error::CliResult;

/// Run `dbmd outline`.
pub fn run(ctx: &Context, args: &OutlineArgs) -> CliResult {
    // The store is `--dir` (default `.`); the file is then resolved
    // store-relative (or absolute) by `render::outline`.
    let store = Store::open_strict(Path::new(&args.dir))?;
    let outline = render::outline(&store, Path::new(&args.file)).map_err(dbmd_core::Error::from)?;

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
