// SPDX-License-Identifier: Apache-2.0

//! `dbmd emit` — the whole-store structured dump (a SWEEP, off the loop).
//!
//! Thin wrapper: open the store, call `dbmd_core::emit::compute` (every
//! content file plus `DB.md`, projected with parsed frontmatter, derived
//! fields, verbatim body, normalized links, and the file-bytes SHA-256), and
//! print the dump. `--json` is the point of the command — one
//! `{"store", "files": [...], "summary": {...}}` document a hosting hub or
//! indexer loads without reimplementing the db.md parse; text mode prints the
//! store-relative paths that would be emitted (one per line, `rg`-composable,
//! the `query` convention). All projection logic lives in `dbmd_core::emit`;
//! this body only formats the returned struct.
//!
//! Compact (single-line) JSON, deliberately: this is the one command whose
//! output scales with the whole store's content, and it is consumed by
//! machines, never eyeballed.

use std::path::Path;

use dbmd_core::emit::{self, Emit, EmittedFile};
use dbmd_core::store::{EdgeSpan, Layer};
use dbmd_core::Store;

use crate::cli::EmitArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult};

/// Run `dbmd emit`.
pub fn run(ctx: &Context, args: &EmitArgs) -> CliResult {
    let store = Store::open_strict(Path::new(&args.dir))?;
    let dump = emit::compute(&store).map_err(CliError::from)?;

    if ctx.json {
        println!("{}", json_dump(&args.dir, &dump));
    } else {
        print!("{}", text_dump(&dump));
    }
    Ok(())
}

/// Human form: the store-relative paths that would be emitted, one per line
/// (the `query` convention — composable, no dump payload).
fn text_dump(dump: &Emit) -> String {
    let mut out = String::new();
    for f in &dump.files {
        out.push_str(&f.path);
        out.push('\n');
    }
    out
}

/// Machine form: `{store, files: [...], summary: {files, sources, records}}`,
/// compact single-line JSON.
fn json_dump(store_dir: &str, dump: &Emit) -> String {
    let files: Vec<serde_json::Value> = dump.files.iter().map(file_json).collect();
    serde_json::json!({
        "store": store_dir,
        "files": files,
        "summary": {
            "files": dump.files.len(),
            "sources": dump.sources,
            "records": dump.records,
        },
    })
    .to_string()
}

/// One emitted file as a JSON object. Absent derived fields render as `null`
/// (uniform shape for loaders); `layer` is the singular word (`source` /
/// `record`), `null` for the root `DB.md`; timestamps render canonical
/// RFC3339 (the raw spellings ride verbatim inside `frontmatter`).
fn file_json(f: &EmittedFile) -> serde_json::Value {
    serde_json::json!({
        "path": f.path,
        "layer": f.layer.map(layer_word),
        "frontmatter": f.frontmatter,
        "type": f.type_,
        "meta_type": f.meta_type,
        "title": f.title,
        "summary": f.summary,
        "body": f.body,
        "links": f.links,
        "link_spans": f.link_spans.iter().map(link_span_json).collect::<Vec<_>>(),
        "created": f.created.map(|t| t.to_rfc3339()),
        "updated": f.updated.map(|t| t.to_rfc3339()),
        "sha256": f.sha256,
    })
}

/// One wiki-link occurrence: where it sits in `body` and what it says.
///
/// `start`/`end` are BYTE offsets into this file's `body` string, `[start,
/// end)` covering the whole `[[…]]` token — a consumer splices at them without
/// knowing the grammar, which is the point (the alternative is every renderer
/// re-implementing bracket scanning AND fence tracking). `target` is the same
/// canonical spelling `links` carries, minus the appended `.md`; `raw` is the
/// inner text verbatim for hosts with their own conventions.
fn link_span_json(s: &EdgeSpan) -> serde_json::Value {
    serde_json::json!({
        "target": s.target,
        "raw": s.raw,
        "alias": s.alias,
        "start": s.start,
        "end": s.end,
    })
}

/// The singular layer word the dump uses (`sources/` holds `source` files,
/// `records/` holds `record` files).
fn layer_word(layer: Layer) -> &'static str {
    match layer {
        Layer::Sources => "source",
        Layer::Records => "record",
    }
}
