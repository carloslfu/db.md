// SPDX-License-Identifier: Apache-2.0

//! `dbmd sync` — pull the granted slice of a hosted brain as plain files, or
//! push the local store as a whole-store snapshot.
//!
//! Thin wrapper over [`dbmd_core::linkmd::sync_pull`] /
//! [`dbmd_core::linkmd::sync_push`]. Two behaviors live at this layer, not in
//! the library, because they are CLI ergonomics rather than wire semantics:
//!
//! - **After a pull that materialized an openable store** (a `DB.md` arrived),
//!   the local `index.md` / `index.jsonl` catalogs are rebuilt so loop ops
//!   work immediately — the index is derived and disposable, and a store
//!   without its catalog is half-delivered. A rebuild failure is reported as
//!   a note, never as a pull failure (the files ARE on disk).
//! - **Push opens the store strictly** (`Store::open_strict`) so pushing from
//!   a non-store exits with the standard `NOT_A_STORE` contract (exit `3`).

use std::path::Path;

use dbmd_core::linkmd;
use dbmd_core::{Index, Store};
use serde_json::{json, Value};

use crate::cli::SyncArgs;
use crate::context::Context;
use crate::error::CliResult;

/// Run `dbmd sync`.
pub fn run(ctx: &Context, args: &SyncArgs) -> CliResult {
    let brain = strip_sigil(&args.brain);
    let cfg = linkmd::hub_config(args.hub.as_deref(), Path::new(&args.dir))?;

    if args.push {
        push(ctx, &cfg, brain, &args.dir)
    } else {
        pull(ctx, &cfg, brain, args.out.as_deref())
    }
}

fn pull(ctx: &Context, cfg: &linkmd::HubConfig, brain: &str, out: Option<&str>) -> CliResult {
    let report = linkmd::sync_pull(cfg, brain, out.map(Path::new))?;

    // Rebuild the derived catalogs when the pull produced an openable store.
    // Best-effort by design: the pulled files are already durable on disk.
    let index_note = match Store::open_strict(Path::new(&report.dest)) {
        Ok(store) => match Index::rebuild_all(&store) {
            Ok(()) => Some("rebuilt".to_string()),
            Err(e) => Some(format!("rebuild failed: {e}")),
        },
        // A scoped pull may not include DB.md — not a store, nothing to index.
        Err(_) => None,
    };

    if ctx.json {
        let mut v = serde_json::to_value(&report).unwrap_or_else(|_| json!({}));
        v["index"] = match &index_note {
            Some(n) => json!(n),
            None => Value::Null,
        };
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return Ok(());
    }

    println!(
        "pulled {} file{} (feed seq {}) into {}",
        report.files,
        if report.files == 1 { "" } else { "s" },
        report.head_seq,
        report.dest,
    );
    if let Some(n) = index_note {
        println!("index: {n}");
    }
    if !report.extra_local.is_empty() {
        println!(
            "{} local content file{} the export did not carry (nothing was deleted):",
            report.extra_local.len(),
            if report.extra_local.len() == 1 {
                ""
            } else {
                "s"
            },
        );
        for p in &report.extra_local {
            println!("  {p}");
        }
    }
    Ok(())
}

fn push(ctx: &Context, cfg: &linkmd::HubConfig, brain: &str, dir: &str) -> CliResult {
    // Strict open: pushing from a non-store is the standard NOT_A_STORE exit.
    let store = Store::open_strict(Path::new(dir))?;
    let files = linkmd::collect_push_files(&store)?;
    let sent = files.len();
    let body = linkmd::sync_push(cfg, brain, &files)?;

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );
        return Ok(());
    }

    let head_seq = body.get("headSeq").and_then(Value::as_u64);
    let durable = body
        .get("durable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let docs = body
        .get("indexed")
        .and_then(|i| i.get("documents"))
        .and_then(Value::as_u64);
    print!("pushed {sent} file{}", if sent == 1 { "" } else { "s" });
    if let Some(d) = docs {
        print!(" ({d} documents indexed)");
    }
    if let Some(seq) = head_seq {
        print!(", feed seq {seq}");
    }
    println!("{}", if durable { ", durable" } else { "" });
    Ok(())
}

/// Accept `@brain` and `brain` alike — the sigil is address sugar.
fn strip_sigil(s: &str) -> &str {
    s.trim().strip_prefix('@').unwrap_or(s.trim())
}
