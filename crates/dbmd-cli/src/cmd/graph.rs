//! `dbmd graph <sub>` — the wiki-link relationship-retrieval axis.
//!
//! Dispatches the [`GraphCommand`] to one of four leaf bodies. Each leaf is a
//! **thin wrapper**: it opens the store, calls the matching `dbmd_core::graph`
//! function (which owns all the logic), applies the presentation-only `--limit`
//! cap, and formats the result as text (one item per line, pipe-clean) or JSON.
//!   - `backlinks`    → [`dbmd_core::graph::backlinks`]
//!   - `forwardlinks` → [`dbmd_core::graph::forwardlinks`]
//!   - `neighborhood` → [`dbmd_core::graph::neighborhood`]
//!   - `orphans`      → [`dbmd_core::graph::orphans`]
//!
//! All four are on-demand (no maintained graph). `backlinks` / `forwardlinks`
//! are loop-fast (ripgrep / single-file extract); `neighborhood` hydrates a
//! context slice; `orphans` is a SWEEP curation worklist.

use std::path::{Path, PathBuf};

use dbmd_core::graph::{self, ContextSlice, Direction};
use dbmd_core::store::Layer;
use dbmd_core::Store;

use crate::cli::{GraphArgs, GraphCommand, GraphTargetArgs, NeighborhoodArgs, OrphansArgs};
use crate::context::Context;
use crate::error::{CliError, CliResult};

/// Dispatch `dbmd graph <sub>` to the matching leaf body.
pub fn run(ctx: &Context, args: &GraphArgs) -> CliResult {
    match &args.command {
        GraphCommand::Backlinks(a) => run_backlinks(ctx, a),
        GraphCommand::Forwardlinks(a) => run_forwardlinks(ctx, a),
        GraphCommand::Neighborhood(a) => run_neighborhood(ctx, a),
        GraphCommand::Orphans(a) => run_orphans(ctx, a),
    }
}

/// `dbmd graph backlinks <path>` — incoming wiki-links (dependents / blast
/// radius). Sidecar-backed and loop-fast: `--type` / `--in` scope which
/// type-folder `index.jsonl` sidecars are read, so a scoped call is O(folder),
/// not a whole-store scan.
pub fn run_backlinks(ctx: &Context, args: &GraphTargetArgs) -> CliResult {
    let store = Store::open(Path::new(&args.dir)).map_err(dbmd_core::Error::from)?;
    let layer = parse_layer(args.r#in.as_deref())?;
    let types: Vec<String> = args.r#type.clone().into_iter().collect();
    let mut hits = graph::backlinks_filtered(&store, Path::new(&args.path), &types, layer)
        .map_err(dbmd_core::Error::from)?;
    apply_limit(&mut hits, args.limit);
    emit_paths(ctx, &hits);
    Ok(())
}

/// `dbmd graph forwardlinks <path>` — outgoing wiki-links (follow the chain).
/// Reads the one file; loop-fast. `--type` / `--in` filter the returned targets
/// (by the target's frontmatter `type` / its layer), so the chain can be walked
/// one type or one layer at a time.
pub fn run_forwardlinks(ctx: &Context, args: &GraphTargetArgs) -> CliResult {
    let store = Store::open(Path::new(&args.dir)).map_err(dbmd_core::Error::from)?;
    let layer = parse_layer(args.r#in.as_deref())?;
    let mut hits =
        graph::forwardlinks(&store, Path::new(&args.path)).map_err(dbmd_core::Error::from)?;

    // `--in` is a pure path-prefix check on each target (no I/O). `--type`
    // narrows to targets whose `type` matches, resolved from that type's
    // sidecar (one sequential read of the named type-folder), so the filter
    // stays sidecar-backed rather than opening every target file.
    if let Some(layer) = layer {
        hits.retain(|t| node_in_layer(t, layer));
    }
    if let Some(type_) = &args.r#type {
        let typed: std::collections::BTreeSet<PathBuf> = store
            .find_by_type(type_)
            .map_err(dbmd_core::Error::from)?
            .into_iter()
            .map(|r| bare_target(&r.path))
            .collect();
        hits.retain(|t| typed.contains(&bare_target(t)));
    }

    apply_limit(&mut hits, args.limit);
    emit_paths(ctx, &hits);
    Ok(())
}

/// `dbmd graph neighborhood <seed>` — bounded BFS hydration: each reached node,
/// its `summary`, and how it connects back toward the seed.
pub fn run_neighborhood(ctx: &Context, args: &NeighborhoodArgs) -> CliResult {
    let store = Store::open(Path::new(&args.dir)).map_err(dbmd_core::Error::from)?;
    let layer = parse_layer(args.r#in.as_deref())?;
    // Direction is always `Both` here: the SPEC frames neighborhood as
    // context hydration ("the relevant context around a seed"), which is the
    // union of incoming and outgoing edges. The `--in`/`--type` filters narrow
    // the *result*, not the edge direction.
    let types: Vec<String> = args.r#type.clone().into_iter().collect();
    let slice = graph::neighborhood(
        &store,
        Path::new(&args.seed),
        args.hops as u32,
        &types,
        Direction::Both,
    )
    .map_err(dbmd_core::Error::from)?;

    // Apply the layer filter (the core fn filters by type, not layer) and the
    // presentation-only `--limit`, then format. Both are CLI-boundary shaping,
    // not graph logic.
    let nodes: Vec<&dbmd_core::graph::ContextNode> = slice
        .nodes
        .iter()
        .filter(|n| layer.map(|l| node_in_layer(&n.path, l)).unwrap_or(true))
        .take(args.limit.unwrap_or(usize::MAX))
        .collect();

    emit_neighborhood(ctx, &slice, &nodes);
    Ok(())
}

/// `dbmd graph orphans` — content files with no incoming AND no outgoing links
/// (the curation worklist). A SWEEP; optionally scoped to one layer.
pub fn run_orphans(ctx: &Context, args: &OrphansArgs) -> CliResult {
    let store = Store::open(Path::new(&args.dir)).map_err(dbmd_core::Error::from)?;
    let layer = parse_layer(args.r#in.as_deref())?;
    let mut hits = graph::orphans(&store, layer).map_err(dbmd_core::Error::from)?;
    apply_limit(&mut hits, args.limit);
    emit_paths(ctx, &hits);
    Ok(())
}

// ── Formatting helpers (presentation only — no graph logic) ──────────────────

/// Truncate a result vector to the `--limit` cap, if any. A `None` limit leaves
/// the full result; the core functions already return sorted, deduped output.
fn apply_limit<T>(items: &mut Vec<T>, limit: Option<usize>) {
    if let Some(n) = limit {
        items.truncate(n);
    }
}

/// Emit a list of store-relative paths: one `/`-joined path per line (text), or
/// a JSON array of path strings (`--json`). An empty result prints nothing in
/// text mode and `[]` in JSON mode — both exit 0.
fn emit_paths(ctx: &Context, paths: &[PathBuf]) {
    if ctx.json {
        let arr: Vec<String> = paths.iter().map(|p| path_str(p)).collect();
        println!("{}", serde_json::to_string(&arr).expect("serialize paths"));
    } else {
        for p in paths {
            println!("{}", path_str(p));
        }
    }
}

/// Emit a neighborhood slice. Text: one `path\thops\tsummary` line per reached
/// node (tab-separated for clean field-splitting). JSON: `{seed, nodes:[...]}`
/// with each node's full shape.
fn emit_neighborhood(
    ctx: &Context,
    slice: &ContextSlice,
    nodes: &[&dbmd_core::graph::ContextNode],
) {
    if ctx.json {
        let nodes_json: Vec<serde_json::Value> = nodes
            .iter()
            .map(|n| {
                let (via_path, via_dir) = match &n.via {
                    Some((p, d)) => (Some(path_str(p)), Some(direction_str(*d))),
                    None => (None, None),
                };
                serde_json::json!({
                    "path": path_str(&n.path),
                    "summary": n.summary,
                    "type": n.type_,
                    "hops": n.hops,
                    "via": via_path,
                    "direction": via_dir,
                })
            })
            .collect();
        let out = serde_json::json!({
            "seed": path_str(&slice.seed),
            "nodes": nodes_json,
        });
        println!(
            "{}",
            serde_json::to_string(&out).expect("serialize neighborhood")
        );
    } else {
        for n in nodes {
            println!("{}\t{}\t{}", path_str(&n.path), n.hops, n.summary);
        }
    }
}

/// Render a store-relative path with `/` separators (never `\`), so output is
/// identical across platforms and matches the wiki-link spelling.
fn path_str(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// The canonical bare wiki-link form of a store-relative path: `/` separators,
/// no trailing `.md`. `forwardlinks` already emits this form; a sidecar
/// record's `path` keeps the real on-disk `.md`, so normalize both before
/// comparing them when filtering forwardlink targets by `--type`.
fn bare_target(p: &Path) -> PathBuf {
    let unix = path_str(p);
    PathBuf::from(unix.strip_suffix(".md").unwrap_or(&unix))
}

/// The stable string form of a traversal direction, used in JSON output.
fn direction_str(dir: Direction) -> &'static str {
    match dir {
        Direction::Incoming => "incoming",
        Direction::Outgoing => "outgoing",
        Direction::Both => "both",
    }
}

/// True if a store-relative path lives under the given layer (by first
/// component). The neighborhood `--in` filter is applied at the CLI boundary
/// because the core `neighborhood` takes a `type` filter, not a layer one.
fn node_in_layer(rel: &Path, layer: Layer) -> bool {
    rel.components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .map(|first| first == layer.dir_name())
        .unwrap_or(false)
}

/// Parse a `--in`/`--layer` value into a [`Layer`]. `None` (flag absent) means
/// "all layers". An unrecognized layer name is a usage-class runtime error so
/// the agent gets a precise, machine-parseable code rather than silent
/// whole-store output.
fn parse_layer(value: Option<&str>) -> Result<Option<Layer>, CliError> {
    match value {
        None => Ok(None),
        Some(name) => Layer::from_dir_name(name).map(Some).ok_or_else(|| {
            CliError::new(
                crate::error::ExitCode::Runtime,
                "BAD_LAYER",
                format!("unknown layer `{name}` (expected sources, records, or wiki)"),
            )
        }),
    }
}
