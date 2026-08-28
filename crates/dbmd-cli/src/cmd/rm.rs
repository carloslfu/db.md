// SPDX-License-Identifier: Apache-2.0

//! `dbmd rm <path>` — link-aware delete of one content file.
//!
//! The delete twin of `rename`, replacing the raw-`rm`-then-`index rebuild`
//! dance: the same guard set (content files only, reserved meta names and
//! frozen pages refused), the same pre-mutation incoming-link scan, then
//! `Store::remove_file` plus `Index::on_remove` write-through (which also
//! deletes a type-folder's index artifacts when its last record goes).
//!
//! Link integrity is the verb's point: while other CONTENT files still
//! wiki-link to the target, `rm` refuses (exit `5`, code `RM_LINKED`) and
//! lists them, because deleting would silently break their links — `rename`
//! rewrites links on move precisely so they never dangle. `--force` deletes
//! anyway and reports the now-broken linkers; `dbmd validate --all` then
//! flags each as `WIKI_LINK_BROKEN`. Derived index artifacts and non-content
//! files never block a delete (the catalog is rebuildable; `rm` does not own
//! those bytes).

use std::path::{Path, PathBuf};

use crate::cli::RmArgs;
use crate::cmd::rename::reserved_meta_name;
use crate::cmd::write::{
    core_err, enforce_frozen, index_on_remove, open_store, require_store_relative,
};
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};
use crate::sanitize::sanitize_single_line;

/// Run `dbmd rm`.
pub fn run(ctx: &Context, args: &RmArgs) -> CliResult {
    let store = open_store(&args.dir)?;
    let _transaction = store.transaction().map_err(CliError::from)?;
    let rel = require_store_relative(&store, &args.path)?;

    // Guard order mirrors `rename`: shape guards before existence, existence
    // before policy, policy before any scan — so the agent always sees the
    // most specific stable code for its situation and nothing touches disk
    // until every refusal has had its chance.
    if let Some(name) = reserved_meta_name(&rel) {
        return Err(reserved_meta_error(&rel, name));
    }
    if store
        .directory_exists(&rel)
        .map_err(|error| path_probe_error(&rel, &error))?
    {
        return Err(directory_error(&rel));
    }
    if !dbmd_core::store::is_content_path(&rel) {
        return Err(CliError::new(
            ExitCode::Policy,
            "RM_NOT_CONTENT",
            format!(
                "rm refused: `{}` is not a content file under sources/ or records/",
                path_to_unix(&rel)
            ),
        )
        .with_hint("`dbmd rm` deletes content records; remove other files with your shell"));
    }
    if !store
        .regular_file_exists(&rel)
        .map_err(|error| path_probe_error(&rel, &error))?
    {
        return Err(CliError::runtime(format!(
            "cannot rm `{}`: file does not exist",
            path_to_unix(&rel)
        )));
    }
    enforce_frozen(&store, &rel)?;

    // The pre-delete incoming-link scan — the same fence-aware content scan
    // `rename` runs, so the two verbs agree on what counts as a link. Only
    // CONTENT linkers gate the delete: index artifacts are derived (and
    // `index.jsonl` rows are dropped by the write-through below), and
    // non-content files are not part of the semantic graph.
    let backlinks: Vec<PathBuf> = store
        .find_links_to(&rel)
        .map_err(core_err)?
        .into_iter()
        .filter(|linker| linker != &rel && dbmd_core::store::is_content_path(linker))
        .collect();

    if !backlinks.is_empty() && !args.force {
        let listed: Vec<String> = backlinks.iter().map(|p| path_to_unix(p)).collect();
        let links = if backlinks.len() == 1 {
            "link"
        } else {
            "links"
        };
        return Err(CliError::new(
            ExitCode::Collision,
            "RM_LINKED",
            format!(
                "rm refused: {} incoming wiki-{links} still target `{}`",
                backlinks.len(),
                path_to_unix(&rel)
            ),
        )
        .with_details(serde_json::json!({ "backlinks": listed }))
        .with_hint(
            "rewrite or remove those links first (`dbmd rename` keeps links valid when moving), \
             or pass `--force` to delete anyway — `dbmd validate --all` will then report \
             WIKI_LINK_BROKEN on each linker",
        ));
    }

    store.remove_file(&rel).map_err(|error| {
        CliError::runtime(format!("cannot rm `{}`: {error}", path_to_unix(&rel)))
    })?;
    let index_warning = index_on_remove(&store, &rel);

    emit_result(ctx, &rel, &backlinks, args.force, &index_warning);
    Ok(())
}

/// Emit the result: a human summary line, or a `--json` object with the
/// removed path and the (now-broken, under `--force`) content linkers.
/// Non-fatal index warning to stderr, the write-surface convention.
fn emit_result(
    ctx: &Context,
    rel: &Path,
    backlinks: &[PathBuf],
    forced: bool,
    index_warning: &Option<String>,
) {
    if let Some(w) = index_warning {
        eprintln!("dbmd: warning: {}", sanitize_single_line(w));
    }
    let listed: Vec<String> = backlinks.iter().map(|p| path_to_unix(p)).collect();
    if ctx.json {
        let out = serde_json::json!({
            "removed": path_to_unix(rel),
            "backlinks": listed,
            "forced": forced,
        });
        println!("{out}");
    } else if listed.is_empty() {
        println!("removed {}", sanitize_single_line(&path_to_unix(rel)));
    } else {
        let links = if listed.len() == 1 { "link" } else { "links" };
        println!(
            "removed {} ({} incoming {links} now broken)",
            sanitize_single_line(&path_to_unix(rel)),
            listed.len()
        );
        for linker in &listed {
            eprintln!("  broken link in {}", sanitize_single_line(linker));
        }
    }
}

/// Structured error: the path is a reserved meta file (exit `4`, policy
/// refusal). Deleting `DB.md` destroys the store; the catalog machinery owns
/// `log.md` / `index.md` / `index.jsonl`.
fn reserved_meta_error(rel: &Path, name: &str) -> CliError {
    CliError::new(
        ExitCode::Policy,
        "RM_RESERVED_META",
        format!(
            "rm refused: `{}` is a reserved db.md meta file ({name}) and cannot be deleted",
            path_to_unix(rel)
        ),
    )
    .with_hint(
        "`DB.md`/`log.md`/`index.md`/`index.jsonl` are managed by db.md; never delete them by hand",
    )
}

/// Structured error: the path is a directory (exit `4`, policy refusal) —
/// `rm` deletes one content file at a time, mirroring `rename`.
fn directory_error(rel: &Path) -> CliError {
    CliError::new(
        ExitCode::Policy,
        "RM_NOT_A_FILE",
        format!(
            "rm refused: `{}` is a directory; `dbmd rm` deletes one content file at a time",
            path_to_unix(rel)
        ),
    )
    .with_hint("rm the individual files inside it, or remove the folder with your shell + run `dbmd index rebuild`")
}

/// A probe (existence / directory check) failed with a real I/O error.
fn path_probe_error(rel: &Path, error: &std::io::Error) -> CliError {
    CliError::runtime(format!("cannot probe `{}`: {error}", path_to_unix(rel)))
}

/// Render a path with `/` separators on every OS.
fn path_to_unix(p: &Path) -> String {
    p.components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}
