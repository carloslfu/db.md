//! `dbmd links <target>` — list every incoming wiki-link to a file.
//!
//! Thin wrapper: parse [`LinksArgs`], open the store, call
//! `dbmd_core::store::Store::find_links_to` (embedded ripgrep — never a bundled
//! `rg`, never a whole-graph build), and print the linking files. Text emits one
//! store-relative path per line (`rg`-friendly); `--json` emits
//! `{target, count, links: [...]}`. All scan logic lives in `dbmd-core`.

use std::path::{Path, PathBuf};

use dbmd_core::Store;

use crate::cli::LinksArgs;
use crate::context::Context;
use crate::error::CliResult;

/// Run `dbmd links`.
pub fn run(ctx: &Context, args: &LinksArgs) -> CliResult {
    let store = open_store(&args.dir)?;

    // The target is a store-relative path; `find_links_to` normalizes a trailing
    // `.md` and matches every accepted spelling of an incoming `[[target]]`.
    let target = Path::new(&args.target);
    let mut links: Vec<PathBuf> = store.find_links_to(target).map_err(map_store_error)?;
    links.sort();

    if ctx.json {
        print!("{}", links_json(&args.target, &links));
    } else {
        print!("{}", links_text(&links));
    }
    Ok(())
}

/// Open the `--dir` as a db.md store, mapping a missing `DB.md` to the stable
/// `NOT_A_STORE` exit. Goes through `dbmd_core::Error` so the exit code +
/// machine code match every other store-walking subcommand.
fn open_store(dir: &str) -> Result<Store, crate::error::CliError> {
    Store::open_strict(Path::new(dir)).map_err(crate::error::CliError::from)
}

/// Map a store-walk error (failed ripgrep scan, I/O) to a CLI runtime error
/// through the canonical `dbmd_core::Error` conversion.
fn map_store_error(err: dbmd_core::StoreError) -> crate::error::CliError {
    crate::error::CliError::from(dbmd_core::Error::from(err))
}

/// Human form: one store-relative path per line, sorted, `rg`-composable. No
/// links → empty output (a clean "no backlinks" signal for pipelines).
fn links_text(links: &[PathBuf]) -> String {
    let mut out = String::new();
    for link in links {
        out.push_str(&link.to_string_lossy());
        out.push('\n');
    }
    out
}

/// Machine form: `{target, count, links: [...]}` — the target echoed back, the
/// count for a quick branch, and the sorted store-relative paths.
fn links_json(target: &str, links: &[PathBuf]) -> String {
    let paths: Vec<String> = links
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let obj = serde_json::json!({
        "target": target,
        "count": paths.len(),
        "links": paths,
    });
    let mut s = serde_json::to_string_pretty(&obj).unwrap_or_else(|_| "{}".to_string());
    s.push('\n');
    s
}
