//! `dbmd query` — frontmatter filter over the `index.jsonl` sidecar.
//!
//! Thin wrapper: parse [`QueryArgs`] into a `dbmd_core::query::Query`
//! (`with_type` / `with_layer` / `with_where`), `execute` it against the store
//! (sidecar-backed — never a whole-store parse), and print the matching paths
//! (text) or the full [`IndexRecord`]s (`--json`). Args parsing + record
//! formatting only; all resolution logic lives in `dbmd-core`.

use std::path::Path;

use dbmd_core::query::Query;
use dbmd_core::store::Layer;
use dbmd_core::{IndexRecord, Store};

use crate::cli::QueryArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// Run `dbmd query`.
pub fn run(ctx: &Context, args: &QueryArgs) -> CliResult {
    let store = open_store(&args.dir)?;
    let query = build_query(args)?;

    let mut records = query.execute(&store).map_err(map_store_error)?;
    // The sidecar readers return a path-sorted set; keep that order stable and
    // apply the optional cap after sorting so `--limit` is deterministic.
    records.sort_by(|a, b| a.path.cmp(&b.path));
    if let Some(limit) = args.limit {
        records.truncate(limit);
    }

    if ctx.json {
        print!("{}", records_json(&records));
    } else {
        print!("{}", records_text(&records));
    }
    Ok(())
}

/// Translate the parsed flags into a composable [`Query`]. A bad `--in` layer
/// name is a usage-class runtime error; a malformed `--where` (no `=`) likewise.
fn build_query(args: &QueryArgs) -> Result<Query, CliError> {
    let mut query = Query::new();
    if let Some(t) = &args.r#type {
        query = query.with_type(t);
    }
    if let Some(layer_name) = &args.r#in {
        query = query.with_layer(parse_layer(layer_name)?);
    }
    for clause in &args.r#where {
        let (key, value) = split_where(clause)?;
        query = query.with_where(key, value);
    }
    Ok(query)
}

/// Parse a `--in <layer>` value into a [`Layer`]; an unknown name is a runtime
/// error with a hint listing the three valid layers.
fn parse_layer(name: &str) -> Result<Layer, CliError> {
    Layer::from_dir_name(name).ok_or_else(|| {
        CliError::new(
            ExitCode::Runtime,
            "BAD_LAYER",
            format!("unknown layer `{name}`"),
        )
        .with_hint("layer must be one of: sources, records, wiki")
    })
}

/// Split a `key=value` clause; a clause with no `=` is a runtime error so the
/// agent gets a deterministic, machine-parseable failure instead of a silent
/// no-op.
fn split_where(clause: &str) -> Result<(&str, &str), CliError> {
    clause.split_once('=').ok_or_else(|| {
        CliError::new(
            ExitCode::Runtime,
            "BAD_WHERE",
            format!("`--where` clause `{clause}` is not `key=value`"),
        )
        .with_hint("write the filter as `key=value`, e.g. --where status=active")
    })
}

/// Open the `--dir` as a db.md store, mapping a missing `DB.md` to `NOT_A_STORE`.
fn open_store(dir: &str) -> Result<Store, CliError> {
    Store::open(Path::new(dir)).map_err(|e| CliError::from(dbmd_core::Error::from(e)))
}

/// Map a sidecar-read error to a CLI runtime error through the canonical
/// `dbmd_core::Error` conversion.
fn map_store_error(err: dbmd_core::StoreError) -> CliError {
    CliError::from(dbmd_core::Error::from(err))
}

/// Human form: one store-relative path per line (`rg`-composable). No matches →
/// empty output.
fn records_text(records: &[IndexRecord]) -> String {
    let mut out = String::new();
    for r in records {
        out.push_str(&r.path.to_string_lossy());
        out.push('\n');
    }
    out
}

/// Machine form: the full [`IndexRecord`] array straight from the sidecar
/// (path + type + summary + tags + links + timestamps + type-specific fields),
/// serialized with the same field shape the sidecar stores so a consumer can
/// round-trip it. Pretty-printed with a trailing newline for stable snapshots.
fn records_json(records: &[IndexRecord]) -> String {
    let mut s = serde_json::to_string_pretty(records).unwrap_or_else(|_| "[]".to_string());
    s.push('\n');
    s
}
