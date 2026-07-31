// SPDX-License-Identifier: Apache-2.0

//! `dbmd sync` — pull the granted slice of a hosted brain as plain files, or
//! push the local store as a whole-store snapshot.
//!
//! Thin wrapper over [`dbmd_core::linkmd::sync_pull`] /
//! [`dbmd_core::linkmd::sync_push`]. Push opens the store strictly
//! (`Store::open_strict`) so pushing from
//!   a non-store exits with the standard `NOT_A_STORE` contract (exit `3`).
//! Pull deliberately does not run a second path-based index rebuild after the
//! capability-relative materialization: an attacker swapping a destination
//! ancestor between those phases could redirect the derived writes. Indexes
//! are disposable and the agent can rebuild them as a separate local action.

use std::path::Path;

use dbmd_core::linkmd;
use dbmd_core::Store;
use serde_json::Value;

use crate::cli::SyncArgs;
use crate::context::Context;
use crate::error::CliResult;
use crate::sanitize::sanitize_single_line;

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

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
        return Ok(());
    }

    println!(
        "pulled {} file{} (feed seq {}) into {}",
        report.files,
        if report.files == 1 { "" } else { "s" },
        report.head_seq,
        // Without --out the destination derives from the hub's slug —
        // hub-authored, so terminal-sanitized.
        sanitize_single_line(&report.dest),
    );
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
            println!("  {}", sanitize_single_line(p));
        }
    }
    Ok(())
}

fn push(ctx: &Context, cfg: &linkmd::HubConfig, brain: &str, dir: &str) -> CliResult {
    // Strict open: pushing from a non-store is the standard NOT_A_STORE exit.
    let store = Store::open_strict(Path::new(dir))?;
    let _transaction = store.transaction()?;
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
