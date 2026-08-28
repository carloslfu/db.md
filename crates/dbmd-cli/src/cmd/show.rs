// SPDX-License-Identifier: Apache-2.0

//! `dbmd show <file>` — one file as its full structured record.
//!
//! Thin wrapper: resolve the store from the file itself (nearest ancestor
//! `DB.md`), project the file through `dbmd_core::emit::emit_file` — the
//! same single-file projection `dbmd emit` streams — and print it. Under
//! `--json` the object is byte-identical in content to the file's entry in
//! `emit --ndjson` (pretty-printed here, the one-shot convention), so a
//! consumer can treat `show` as the random-access form of the dump. Text
//! mode prints the derived header fields then the verbatim body.
//!
//! Read-only and O(one file): a bounded single-file read plus the store
//! open — never a store walk — so it is safe anywhere in the loop.

use std::path::Path;

use dbmd_core::emit;
use dbmd_core::store::Layer;

use crate::cli::ShowArgs;
use crate::cmd::emit::file_json;
use crate::cmd::file_target::{lexical_absolute_before_open, locate_store, store_relative};
use crate::context::Context;
use crate::error::{CliError, CliResult};
use crate::sanitize::{sanitize, sanitize_single_line};

/// Run `dbmd show`.
pub fn run(ctx: &Context, args: &ShowArgs) -> CliResult {
    let input = Path::new(&args.file);
    let file = lexical_absolute_before_open(input)?;
    let store = locate_store(&file)?;
    let rel = store_relative(&store, &file);

    let emitted = emit::emit_file(&store, &rel)
        .map_err(|e| CliError::from(e).with_hint(format!("could not read `{}`", args.file)))?;

    if ctx.json {
        let mut s =
            serde_json::to_string_pretty(&file_json(&emitted)).unwrap_or_else(|_| "{}".to_string());
        s.push('\n');
        print!("{s}");
    } else {
        print!("{}", show_text(&emitted));
    }
    Ok(())
}

/// Human form: the derived header fields (one `key: value` per line, absent
/// fields skipped), a blank separator, then the verbatim body. Store-authored
/// strings are terminal-sanitized; the body keeps its newlines.
fn show_text(f: &emit::EmittedFile) -> String {
    let mut out = String::new();
    out.push_str(&format!("path: {}\n", sanitize_single_line(&f.path)));
    if let Some(layer) = f.layer {
        let word = match layer {
            Layer::Sources => "source",
            Layer::Records => "record",
        };
        out.push_str(&format!("layer: {word}\n"));
    }
    if let Some(type_) = &f.type_ {
        out.push_str(&format!("type: {}\n", sanitize_single_line(type_)));
    }
    if let Some(meta_type) = &f.meta_type {
        out.push_str(&format!("meta-type: {}\n", sanitize_single_line(meta_type)));
    }
    if let Some(title) = &f.title {
        out.push_str(&format!("title: {}\n", sanitize_single_line(title)));
    }
    if let Some(summary) = &f.summary {
        out.push_str(&format!("summary: {}\n", sanitize_single_line(summary)));
    }
    if let Some(created) = f.created {
        out.push_str(&format!("created: {}\n", created.to_rfc3339()));
    }
    if let Some(updated) = f.updated {
        out.push_str(&format!("updated: {}\n", updated.to_rfc3339()));
    }
    if !f.links.is_empty() {
        let links: Vec<String> = f.links.iter().map(|l| sanitize_single_line(l)).collect();
        out.push_str(&format!("links: {}\n", links.join(", ")));
    }
    out.push_str(&format!("sha256: {}\n", f.sha256));
    if !f.body.is_empty() {
        out.push('\n');
        out.push_str(&sanitize(&f.body));
    }
    out
}
