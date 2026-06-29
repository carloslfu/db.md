//! `dbmd index <sub>` — the write-through content catalog.
//!
//! Dispatches the [`IndexCommand`] to one of three leaf bodies:
//!   - `rebuild` → from-scratch repair (`dbmd_core::index::Index::rebuild_all`),
//!     or scoped via `--layer` / `--folder`; `--dry-run` previews.
//!   - `show` → print an `index.md` (root by default; `<path>`-scoped). On a
//!     missing index: exit 1, stderr hint, empty stdout.
//!   - `query` → complete structured read/filter over `index.jsonl`
//!     (`dbmd_core::query::Query` → `Store::read_type_index`).
//!
//! Thin wrapper: parse args, call into `dbmd-core`, format output. `rebuild`
//! and `show` are catalog maintenance/read; `query` is the complete (no
//! 500-cap) structured read every `## More` footer points at.

use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset};

use crate::cli::{IndexArgs, IndexCommand, IndexQueryArgs, IndexRebuildArgs, IndexShowArgs};
use crate::cmd::fm::parse_layer;
use crate::cmd::log::{into_cli, open_store, parse_flexible_timestamp};
use crate::cmd::write::require_store_relative;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

use dbmd_core::{Index, IndexLevel, IndexRecord, Layer, Query, Store};

/// Dispatch `dbmd index <sub>` to the matching leaf body.
pub fn run(ctx: &Context, args: &IndexArgs) -> CliResult {
    match &args.command {
        IndexCommand::Rebuild(a) => run_rebuild(ctx, a),
        IndexCommand::Show(a) => run_show(ctx, a),
        IndexCommand::Query(a) => run_query(ctx, a),
    }
}

/// `dbmd index rebuild [--layer --folder --dry-run]` — from-scratch repair.
/// Default rebuilds the full hierarchy; `--folder`/`--layer` scope it; both
/// together with `--dry-run` print what would be written without writing.
pub fn run_rebuild(ctx: &Context, args: &IndexRebuildArgs) -> CliResult {
    let store = open_store(&args.dir)?;

    if args.layer.is_some() && args.folder.is_some() {
        return Err(CliError::new(
            ExitCode::Runtime,
            "BAD_SCOPE",
            "pass at most one of --layer / --folder",
        ));
    }

    // Resolve the rebuild scope. `--folder` is one type-folder; `--layer` is one
    // layer; neither is the whole store.
    let scope = if let Some(folder) = &args.folder {
        RebuildScope::Folder(require_type_folder_scope(&store, folder)?)
    } else if let Some(layer) = &args.layer {
        RebuildScope::Layer(parse_layer(layer)?)
    } else {
        RebuildScope::Full
    };

    if args.dry_run {
        let preview = render_dry_run(&store, &scope)?;
        if ctx.json {
            let obj = serde_json::json!({
                "dry_run": true,
                "scope": scope.describe(),
                "preview": preview,
            });
            println!("{obj}");
        } else {
            print!("{preview}");
        }
        return Ok(());
    }

    match &scope {
        RebuildScope::Full => Index::rebuild_all(&store)?,
        RebuildScope::Layer(layer) => rebuild_layer(&store, *layer)?,
        RebuildScope::Folder(folder) => Index::rebuild_folder(&store, folder)?,
    }

    if ctx.json {
        let obj = serde_json::json!({ "rebuilt": true, "scope": scope.describe() });
        println!("{obj}");
    } else {
        println!("rebuilt {}", scope.describe());
    }
    Ok(())
}

/// `dbmd index show [<path>]` — print an `index.md` to stdout. Default is the
/// root `index.md`; `<path>` scopes to a layer or type-folder. A missing index
/// exits 1 with a stderr hint and an empty stdout (pipelines stay clean).
pub fn run_show(ctx: &Context, args: &IndexShowArgs) -> CliResult {
    let store = open_store(&args.dir)?;
    let index_rel = match &args.path {
        Some(p) => require_show_scope(&store, p)?.join("index.md"),
        None => PathBuf::from("index.md"),
    };
    let index_md = store.root.join(&index_rel);

    match std::fs::read_to_string(&index_md) {
        Ok(contents) => {
            if ctx.json {
                let obj = serde_json::json!({
                    "path": path_str(&index_rel),
                    "contents": contents,
                });
                println!("{obj}");
            } else {
                print!("{contents}");
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let where_ = args.path.as_deref().unwrap_or(".");
            Err(CliError::new(
                ExitCode::Runtime,
                "INDEX_MISSING",
                format!("no index.md at {where_}; run `dbmd index rebuild` to create"),
            ))
        }
        Err(e) => Err(e.into()),
    }
}

/// `dbmd index query [...]` — complete structured read/filter over the
/// `index.jsonl` sidecar(s). Resolves the type/where/layer query via the core
/// sidecar reader, then applies the time-window filters and `--limit` in memory.
pub fn run_query(ctx: &Context, args: &IndexQueryArgs) -> CliResult {
    let store = open_store(&args.dir)?;

    let mut query = Query::new();
    if let Some(t) = &args.r#type {
        query = query.with_type(t);
    }
    if let Some(layer) = &args.r#in {
        query = query.with_layer(parse_layer(layer)?);
    }
    for clause in &args.r#where {
        let (k, v) = split_where(clause)?;
        query = query.with_where(k, v);
    }

    let mut records = into_cli(query.execute(&store))?;

    // Time windows: parse each bound once, then retain matching records. A bound
    // a record can't satisfy (missing/!parseable timestamp) drops the record.
    let win = TimeWindow::from_args(args)?;
    records.retain(|r| win.accepts(r));

    // `query.execute` concatenates per-sidecar reads in sidecar-PATH order, which
    // is NOT globally record-path-sorted once a layer mixes loose files (a
    // layer-root sidecar) with type-folders (whose sidecar path sorts after the
    // loose file's record path). Sort by record path so the enumeration — and the
    // `--limit` cap below — match `dbmd query` / `dbmd fm query` exactly.
    records.sort_by(|a, b| a.path.cmp(&b.path));

    if let Some(limit) = args.limit {
        records.truncate(limit);
    }

    emit_records(ctx, &records);
    Ok(())
}

// ── Rebuild scope ─────────────────────────────────────────────────────────────

/// The resolved scope of an `index rebuild`.
enum RebuildScope {
    /// Root + every non-empty layer + every non-empty type-folder.
    Full,
    /// One layer's `index.md` rollup (and its type-folders, when written via
    /// the SWEEP builder).
    Layer(Layer),
    /// One type-folder's `index.md` + `index.jsonl`.
    Folder(PathBuf),
}

impl RebuildScope {
    /// A short human description of the scope for confirmation output.
    fn describe(&self) -> String {
        match self {
            RebuildScope::Full => "full hierarchy".to_string(),
            RebuildScope::Layer(l) => format!("layer {}", l.dir_name()),
            RebuildScope::Folder(p) => format!("folder {}", path_str(p)),
        }
    }
}

/// Render the `--dry-run` preview for a scope. Full scope previews root + every
/// layer + every type-folder; scoped runs preview just that level. Each rendered
/// artifact carries a `--- <path> ---` separator (the format `render_dry_run`
/// emits).
///
/// The preview must match what the *real* rebuild actually writes: `rebuild_all`
/// / `write_level` skip empty type-folders and delete (rather than write) the
/// `index.md` for a layer/root with no children (dbmd-core `index.rs`). Previewing
/// a `--- … ---` block for an empty level would claim a file "would be written"
/// that the rebuild instead skips or deletes, so every level is gated on the same
/// emptiness check the core write path uses before it is rendered.
fn render_dry_run(store: &Store, scope: &RebuildScope) -> Result<String, CliError> {
    let mut out = String::new();
    match scope {
        RebuildScope::Folder(folder) => {
            push_type_folder_preview(&mut out, store, folder)?;
        }
        RebuildScope::Layer(layer) => {
            // The layer rollup plus each of its type-folders AND the root rollup,
            // so the preview matches what a layer-scoped write produces (the write
            // re-renders root from the now-current sidecars — see `rebuild_layer`).
            for tf in type_folders_in_layer(store, *layer) {
                push_type_folder_preview(&mut out, store, &tf)?;
            }
            push_layer_preview(&mut out, store, *layer)?;
            push_root_preview(&mut out, store)?;
        }
        RebuildScope::Full => {
            for layer in Layer::all() {
                for tf in type_folders_in_layer(store, layer) {
                    push_type_folder_preview(&mut out, store, &tf)?;
                }
                push_layer_preview(&mut out, store, layer)?;
            }
            push_root_preview(&mut out, store)?;
        }
    }
    Ok(out)
}

/// Preview one type-folder's artifacts, but only when the real rebuild would
/// write them. `rebuild_all` / `write_level` skip a type-folder with no records
/// (and delete any stale `index.md`/`index.jsonl`), so an empty folder yields no
/// preview block — matching disk after a rebuild.
fn push_type_folder_preview(
    out: &mut String,
    store: &Store,
    folder: &Path,
) -> Result<(), CliError> {
    let idx = Index::build_type_folder(store, folder)?;
    if idx.records.is_empty() {
        return Ok(());
    }
    out.push_str(&Index::render_dry_run(
        store,
        &IndexLevel::TypeFolder(folder.to_path_buf()),
    )?);
    Ok(())
}

/// Preview a layer's `index.md`, but only when the real rebuild would write it.
/// `rebuild_all` / `write_level` remove the layer `index.md` when the layer has
/// no non-empty child type-folders, so an empty layer yields no preview block.
fn push_layer_preview(out: &mut String, store: &Store, layer: Layer) -> Result<(), CliError> {
    let idx = Index::build_layer(store, layer)?;
    if idx.child_counts.is_empty() {
        return Ok(());
    }
    out.push_str(&Index::render_dry_run(store, &IndexLevel::Layer(layer))?);
    Ok(())
}

/// Preview the root `index.md`, but only when the real rebuild would write it.
/// `rebuild_all` / `write_level` remove the root `index.md` when the store has no
/// non-empty type-folders, so a fully-empty store yields no preview block.
fn push_root_preview(out: &mut String, store: &Store) -> Result<(), CliError> {
    let idx = Index::build_root(store)?;
    if idx.child_counts.is_empty() {
        return Ok(());
    }
    out.push_str(&Index::render_dry_run(store, &IndexLevel::Root)?);
    Ok(())
}

fn rebuild_layer(store: &Store, layer: Layer) -> Result<(), dbmd_core::Error> {
    for tf in type_folders_in_layer(store, layer) {
        Index::write_level(store, &IndexLevel::TypeFolder(tf))?;
    }
    Index::write_level(store, &IndexLevel::Layer(layer))?;
    // The root `index.md` embeds per-folder `(n)` counts, per-layer totals, and a
    // derived `updated:` rolled up from the folder sidecars. A `--layer` repair
    // that changes a folder's record count would otherwise leave those stale —
    // the exact root/folder desync `Index::rebuild_folder` was written to avoid
    // (it cascades to root via `update_parents`). Re-render root from the
    // now-current sidecars so the whole hierarchy stays consistent.
    Index::write_level(store, &IndexLevel::Root)?;
    Ok(())
}

/// The immediate type-folders under a layer (one directory level below the layer
/// dir), as store-relative paths. Hidden dirs and `log/` are skipped. Mirrors
/// the core sweep enumeration so a dry-run preview lists the same folders a
/// rebuild writes.
fn type_folders_in_layer(store: &Store, layer: Layer) -> Vec<PathBuf> {
    let layer_dir = store.root.join(layer.dir_name());
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(&layer_dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for entry in rd.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') || name == "log" {
            continue;
        }
        out.push(PathBuf::from(layer.dir_name()).join(name));
    }
    out.sort();
    out
}

// ── Query output (shared with `fm query`) ─────────────────────────────────────

/// Render a result set of [`IndexRecord`]s: under `--json`, the full records as
/// a JSON array (path + summary + tags + links + fields); in text mode, one
/// store-relative path per line. Shared by `index query` and `fm query`.
pub(crate) fn emit_records(ctx: &Context, records: &[IndexRecord]) {
    if ctx.json {
        let arr: Vec<serde_json::Value> = records
            .iter()
            .map(|r| serde_json::to_value(r).unwrap_or(serde_json::Value::Null))
            .collect();
        println!("{}", serde_json::Value::Array(arr));
    } else {
        for r in records {
            println!("{}", path_str(&r.path));
        }
    }
}

/// The parsed `--created/updated-after/-before` window for `index query`.
struct TimeWindow {
    updated_after: Option<DateTime<FixedOffset>>,
    updated_before: Option<DateTime<FixedOffset>>,
    created_after: Option<DateTime<FixedOffset>>,
    created_before: Option<DateTime<FixedOffset>>,
}

impl TimeWindow {
    /// Parse every supplied bound (date-only tolerated, same contract as
    /// `log since`). Absent bounds are `None` (no constraint on that side).
    fn from_args(args: &IndexQueryArgs) -> Result<TimeWindow, CliError> {
        Ok(TimeWindow {
            updated_after: opt_ts(&args.updated_after)?,
            updated_before: opt_ts(&args.updated_before)?,
            created_after: opt_ts(&args.created_after)?,
            created_before: opt_ts(&args.created_before)?,
        })
    }

    /// True if `record` satisfies every set bound. An `*-after`/`*-before` bound
    /// is inclusive; a record missing the relevant timestamp fails any bound on
    /// that field (it can't be shown to be inside the window).
    fn accepts(&self, record: &IndexRecord) -> bool {
        if let Some(bound) = self.updated_after {
            match record.updated {
                Some(u) if u >= bound => {}
                _ => return false,
            }
        }
        if let Some(bound) = self.updated_before {
            match record.updated {
                Some(u) if u <= bound => {}
                _ => return false,
            }
        }
        if let Some(bound) = self.created_after {
            match record.created {
                Some(c) if c >= bound => {}
                _ => return false,
            }
        }
        if let Some(bound) = self.created_before {
            match record.created {
                Some(c) if c <= bound => {}
                _ => return false,
            }
        }
        true
    }
}

/// Parse an optional timestamp bound, threading through the shared flexible
/// parser (RFC3339 or bare `YYYY-MM-DD`).
fn opt_ts(raw: &Option<String>) -> Result<Option<DateTime<FixedOffset>>, CliError> {
    match raw {
        Some(s) => Ok(Some(parse_flexible_timestamp(s)?)),
        None => Ok(None),
    }
}

/// Split a `--where key=value` clause at the first `=`. The value may contain
/// further `=`. An empty key is a usage error.
fn split_where(clause: &str) -> Result<(&str, &str), CliError> {
    match clause.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k, v)),
        _ => Err(CliError::new(
            ExitCode::Runtime,
            "BAD_WHERE",
            format!("--where expects `key=value`, got {clause:?}"),
        )),
    }
}

// ── Path glue ────────────────────────────────────────────────────────────────

/// Resolve `--folder` to exactly `<layer>/<type-folder>` under the store. This
/// is a write scope, so parent traversal / absolute outside-store paths are
/// rejected before core writes or removes `index.md` / `index.jsonl`.
fn require_type_folder_scope(store: &Store, raw: &str) -> Result<PathBuf, CliError> {
    let rel = require_store_relative(store, raw)?;
    let comps = normal_components(&rel);
    if comps.len() == 2 && Layer::from_dir_name(comps[0]).is_some() {
        return Ok(rel);
    }
    Err(CliError::new(
        ExitCode::Runtime,
        "BAD_SCOPE",
        format!("--folder expects <layer>/<type-folder>, got {raw:?}"),
    )
    .with_hint("example: --folder records/contacts"))
}

/// Resolve an `index show <path>` scope. Show is read-only, but it still must
/// stay inside the store; accepted scopes are a layer (`records`) or a
/// type-folder (`records/contacts`).
fn require_show_scope(store: &Store, raw: &str) -> Result<PathBuf, CliError> {
    let rel = require_store_relative(store, raw)?;
    let comps = normal_components(&rel);
    if matches!(comps.len(), 1 | 2)
        && comps
            .first()
            .and_then(|c| Layer::from_dir_name(c))
            .is_some()
    {
        return Ok(rel);
    }
    Err(CliError::new(
        ExitCode::Runtime,
        "BAD_SCOPE",
        format!("index show path must be a layer or type-folder, got {raw:?}"),
    )
    .with_hint("examples: dbmd index show records; dbmd index show records/contacts"))
}

fn normal_components(path: &Path) -> Vec<&str> {
    path.components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect()
}

/// Render a path with `/` separators for stable, platform-independent output.
fn path_str(p: &Path) -> String {
    p.components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a store with one populated type-folder (`records/contacts`) plus an
    /// empty type-folder (`records/empty`), the empty `sources` layer, and a
    /// stray non-layer directory (`wiki`, no longer a recognized layer), mirroring
    /// the dry-run/real-rebuild divergence repro.
    fn store_with_empty_scopes() -> (tempfile::TempDir, Store) {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("DB.md"),
            "---\ntype: db-md\nscope: company\nowner: Test Owner\n---\n# Test DB\n",
        )
        .unwrap();

        // One real content file → records/contacts is non-empty.
        std::fs::create_dir_all(root.join("records/contacts")).unwrap();
        std::fs::write(
            root.join("records/contacts/jane-doe.md"),
            "---\ntype: contact\ncreated: 2026-01-01T00:00:00Z\nupdated: 2026-01-01T00:00:00Z\nsummary: Jane Doe\n---\n# Jane Doe\n",
        )
        .unwrap();

        // Empty type-folder + the empty `sources` layer + a stray non-layer dir:
        // a real rebuild writes nothing for these (and deletes any stale
        // artifacts). `wiki` is no longer a recognized layer, so it stands in here
        // as a directory the rebuild must ignore entirely.
        std::fs::create_dir_all(root.join("records/empty")).unwrap();
        std::fs::create_dir_all(root.join("sources")).unwrap();
        std::fs::create_dir_all(root.join("wiki")).unwrap();

        let store = Store::open(root).unwrap();
        (dir, store)
    }

    #[test]
    fn dry_run_skips_empty_folders_and_layers() {
        // The dry-run preview must match what the real rebuild writes: empty
        // type-folders are skipped and empty layer/root index.md are deleted (not
        // written), so the preview must NOT advertise those as would-be writes.
        let (_dir, store) = store_with_empty_scopes();
        let preview = render_dry_run(&store, &RebuildScope::Full).unwrap();

        // Non-empty scopes are previewed.
        assert!(
            preview.contains("--- records/contacts/index.md ---"),
            "non-empty folder must be previewed:\n{preview}"
        );
        assert!(
            preview.contains("--- records/contacts/index.jsonl ---"),
            "non-empty folder jsonl must be previewed:\n{preview}"
        );
        assert!(
            preview.contains("--- records/index.md ---"),
            "non-empty layer must be previewed:\n{preview}"
        );
        assert!(
            preview.contains("--- index.md ---"),
            "root with content must be previewed:\n{preview}"
        );

        // Empty scopes are NOT previewed (the real rebuild writes nothing there).
        assert!(
            !preview.contains("records/empty/index.md"),
            "empty type-folder must not be previewed:\n{preview}"
        );
        assert!(
            !preview.contains("sources/index.md"),
            "empty layer must not be previewed:\n{preview}"
        );
        assert!(
            !preview.contains("wiki/index.md"),
            "empty layer must not be previewed:\n{preview}"
        );
    }

    #[test]
    fn dry_run_empty_store_previews_nothing() {
        // A store with no content files at all: every level is empty, so the real
        // rebuild deletes/skips everything and the preview is empty too.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("DB.md"),
            "---\ntype: db-md\nscope: company\nowner: Test Owner\n---\n# Empty\n",
        )
        .unwrap();
        let store = Store::open(dir.path()).unwrap();

        let preview = render_dry_run(&store, &RebuildScope::Full).unwrap();
        assert!(
            preview.is_empty(),
            "an all-empty store must preview nothing, got:\n{preview}"
        );
    }
}
