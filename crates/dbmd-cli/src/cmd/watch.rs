// SPDX-License-Identifier: Apache-2.0

//! `dbmd watch` — follow the store's own files for changes.
//!
//! Thin wrapper over `dbmd_core::watch`: take a baseline snapshot of the
//! emit membership (content files + `DB.md`, optionally narrowed by
//! `--path`), then poll and print each snapshot diff — the local-filesystem
//! sibling of `subscribe`, and the composition point for anything that wants
//! to react to store edits (an app server refetching, an agent noticing
//! out-of-band writes).
//!
//! Output contract (the `subscribe` shape): one event per line. Under
//! `--json` each line is a single compact JSON object (NDJSON — a stream,
//! unlike the one-shot commands' pretty bodies): first
//! `{"event":"baseline","files":N}`, then
//! `{"event":"created|modified|removed","path":…,"at":…}` per change. The
//! baseline is always emitted so a consumer knows the watch is live and what
//! it covers. Transient snapshot errors warn on stderr and the poll retries;
//! a vanished store (`DB.md` gone) is a real state change and exits with
//! `NOT_A_STORE`. Purely observational: no locks taken, nothing written —
//! a watcher never blocks a writer.

use std::path::{Path, PathBuf};

use dbmd_core::watch::{self, Change, Snapshot};
use dbmd_core::Store;

use crate::cli::WatchArgs;
use crate::cmd::write::{core_err, open_store, require_store_relative};
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};
use crate::sanitize::sanitize_single_line;

/// Run `dbmd watch`.
pub fn run(ctx: &Context, args: &WatchArgs) -> CliResult {
    if args.interval == 0 {
        return Err(CliError::new(
            ExitCode::Runtime,
            "BAD_INTERVAL",
            "--interval must be at least 1 second",
        ));
    }
    let store = open_store(&args.dir)?;
    let prefix: Option<PathBuf> = match &args.path {
        Some(raw) => Some(require_store_relative(&store, raw)?),
        None => None,
    };

    let mut prev: Snapshot = watch::snapshot(&store, prefix.as_deref()).map_err(core_err)?;
    emit_baseline(ctx, prev.len(), prefix.as_deref());

    loop {
        std::thread::sleep(std::time::Duration::from_secs(args.interval));
        match watch::snapshot(&store, prefix.as_deref()) {
            Ok(next) => {
                for change in watch::diff(&prev, &next) {
                    emit_change(ctx, &change);
                }
                prev = next;
            }
            // Triage mirrors `subscribe`: a transient sweep failure (a
            // directory swapped mid-walk) must not kill a follower, but a
            // vanished store is a real state change and must.
            Err(e) => {
                if !store_still_present(&store) {
                    return Err(CliError::new(
                        ExitCode::NotAStore,
                        "NOT_A_STORE",
                        "the watched store's DB.md is gone; stopping",
                    ));
                }
                eprintln!(
                    "dbmd: watch: snapshot failed ({}); retrying",
                    sanitize_single_line(&e.to_string())
                );
            }
        }
    }
}

/// Whether the store marker still exists — the fatal/transient discriminator
/// for a failed snapshot.
fn store_still_present(store: &Store) -> bool {
    store
        .regular_file_exists(Path::new("DB.md"))
        .unwrap_or(false)
}

/// The first line: what the watch covers. Always emitted, so a consumer
/// knows the stream is live before the first change.
fn emit_baseline(ctx: &Context, files: usize, prefix: Option<&Path>) {
    if ctx.json {
        let mut v = serde_json::json!({ "event": "baseline", "files": files });
        if let Some(p) = prefix {
            v["path"] = serde_json::json!(p.to_string_lossy().replace('\\', "/"));
        }
        println!("{v}");
    } else {
        match prefix {
            Some(p) => println!(
                "watching {files} files under {}",
                sanitize_single_line(&p.to_string_lossy())
            ),
            None => println!("watching {files} files"),
        }
    }
}

/// One change line. JSON carries the event word, the store-relative path,
/// and the observation timestamp; the human form is `<word> <path>`.
fn emit_change(ctx: &Context, change: &Change) {
    let path = change.path.to_string_lossy().replace('\\', "/");
    if ctx.json {
        let v = serde_json::json!({
            "event": change.kind.word(),
            "path": path,
            "at": dbmd_core::now().to_rfc3339(),
        });
        println!("{v}");
    } else {
        println!("{} {}", change.kind.word(), sanitize_single_line(&path));
    }
}
