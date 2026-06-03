//! `dbmd stats` — on-demand store overview (a SWEEP, off the loop).
//!
//! Thin wrapper: open the store, call `dbmd_core::stats::compute` (a full-store
//! walk, like `du` — never precomputed/maintained), and print the overview as
//! human text or the structured `Stats` (`--json`). All counting/aggregation
//! lives in `dbmd_core::stats`; this body only formats the returned struct.

use std::path::Path;

use dbmd_core::stats::{self, Stats};
use dbmd_core::Store;

use crate::cli::StatsArgs;
use crate::context::Context;
use crate::error::CliResult;

/// Run `dbmd stats`.
pub fn run(ctx: &Context, args: &StatsArgs) -> CliResult {
    let store = Store::open_strict(Path::new(&args.dir))?;
    let s = stats::compute(&store)?;

    if ctx.json {
        emit_json(&s);
    } else {
        emit_text(&s);
    }
    Ok(())
}

/// Human overview: totals, per-layer counts, size, orphan/broken-link counts,
/// the top types, and the schema-coverage split. Concise, label-per-line.
fn emit_text(s: &Stats) {
    println!("files: {}", s.total_files);
    for layer in dbmd_core::store::Layer::all() {
        let n = s.files_per_layer.get(&layer).copied().unwrap_or(0);
        println!("  {}: {}", layer.dir_name(), n);
    }
    println!("size: {} bytes", s.total_size_bytes);
    println!("orphans: {}", s.orphan_count);
    println!("broken links: {}", s.broken_link_count);

    if !s.top_types.is_empty() {
        println!("top types:");
        for (type_, count) in &s.top_types {
            println!("  {type_}: {count}");
        }
    }
}

/// Structured stats. `files_per_layer` and `type_distribution` are emitted as
/// objects keyed by layer/type name; `top_types` as an ordered array of
/// `[name, count]` pairs (order is the count-desc, name-asc ranking).
fn emit_json(s: &Stats) {
    let files_per_layer: serde_json::Map<String, serde_json::Value> = s
        .files_per_layer
        .iter()
        .map(|(layer, n)| (layer.dir_name().to_string(), serde_json::json!(n)))
        .collect();

    let type_distribution: serde_json::Map<String, serde_json::Value> = s
        .type_distribution
        .iter()
        .map(|(t, n)| (t.clone(), serde_json::json!(n)))
        .collect();

    let top_types: Vec<serde_json::Value> = s
        .top_types
        .iter()
        .map(|(t, n)| serde_json::json!([t, n]))
        .collect();

    let out = serde_json::json!({
        "total_files": s.total_files,
        "files_per_layer": files_per_layer,
        "total_size_bytes": s.total_size_bytes,
        "type_distribution": type_distribution,
        "orphan_count": s.orphan_count,
        "broken_link_count": s.broken_link_count,
        "top_types": top_types,
    });
    println!("{}", serde_json::to_string(&out).expect("serialize stats"));
}
