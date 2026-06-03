//! `dbmd tree` — pretty-print the store as a tree.
//!
//! Thin wrapper: open the store, build the `dbmd_core::render::Tree` (optionally
//! scoped by `--layer` / `--type`), and print it as an indented text tree or as
//! the structured tree (`--json`). All grouping/sorting logic lives in
//! `dbmd_core::render`; this body only walks the returned structure and formats.

use std::path::Path;

use dbmd_core::render::{self, Tree};
use dbmd_core::store::Layer;
use dbmd_core::Store;

use crate::cli::TreeArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// Run `dbmd tree`.
pub fn run(ctx: &Context, args: &TreeArgs) -> CliResult {
    let store = Store::open_strict(Path::new(&args.dir))?;
    let layer = parse_layer(args.layer.as_deref())?;
    let tree =
        render::tree(&store, layer, args.r#type.as_deref()).map_err(dbmd_core::Error::from)?;

    if ctx.json {
        emit_json(&tree);
    } else {
        emit_text(&tree);
    }
    Ok(())
}

/// Indented text tree: each layer, then its type-folders, then the files under
/// each — two spaces of indent per level. A store with no content prints
/// nothing (exit 0).
fn emit_text(tree: &Tree) {
    for layer in &tree.layers {
        println!("{}", layer.layer.dir_name());
        for tf in &layer.type_folders {
            println!("  {}", path_str(&tf.path));
            for file in &tf.files {
                println!("    {}", path_str(file));
            }
        }
    }
}

/// Structured tree as `{layers:[{layer, type_folders:[{path, files:[...]}]}]}`.
/// Paths use `/` separators so JSON output is platform-stable.
fn emit_json(tree: &Tree) {
    let layers: Vec<serde_json::Value> = tree
        .layers
        .iter()
        .map(|layer| {
            let folders: Vec<serde_json::Value> = layer
                .type_folders
                .iter()
                .map(|tf| {
                    let files: Vec<String> = tf.files.iter().map(|p| path_str(p)).collect();
                    serde_json::json!({ "path": path_str(&tf.path), "files": files })
                })
                .collect();
            serde_json::json!({
                "layer": layer.layer.dir_name(),
                "type_folders": folders,
            })
        })
        .collect();
    let out = serde_json::json!({ "layers": layers });
    println!("{}", serde_json::to_string(&out).expect("serialize tree"));
}

/// Render a store-relative path with `/` separators (never `\`).
fn path_str(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Parse a `--layer` value into a [`Layer`]; `None` means "all layers". An
/// unknown name is a machine-parseable runtime error.
fn parse_layer(value: Option<&str>) -> Result<Option<Layer>, CliError> {
    match value {
        None => Ok(None),
        Some(name) => Layer::from_dir_name(name).map(Some).ok_or_else(|| {
            CliError::new(
                ExitCode::Runtime,
                "BAD_LAYER",
                format!("unknown layer `{name}` (expected sources, records, or wiki)"),
            )
        }),
    }
}
