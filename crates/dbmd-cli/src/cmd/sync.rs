// SPDX-License-Identifier: Apache-2.0

//! `dbmd sync` — negotiate verified incremental v2 reconciliation for a
//! permissioned brain, with the legacy whole-snapshot protocol retained only
//! for v1 hubs.
//!
//! Thin wrapper over the one-shot convergence engine, with explicit pull-only
//! and push-only policies. Push opens the store strictly
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
        push(ctx, &cfg, brain, &args.dir, args.resume_local_policy)
    } else if args.pull_only || args.out.is_some() {
        pull(ctx, &cfg, brain, args.out.as_deref())
    } else if Store::open_strict(Path::new(&args.dir)).is_ok() {
        converge(ctx, &cfg, brain, &args.dir, args.resume_local_policy)
    } else {
        pull(ctx, &cfg, brain, args.out.as_deref())
    }
}

fn converge(
    ctx: &Context,
    cfg: &linkmd::HubConfig,
    brain: &str,
    dir: &str,
    resume_local_policy: bool,
) -> CliResult {
    let body = linkmd::sync_converge(cfg, brain, Path::new(dir), resume_local_policy)?;
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );
        return Ok(());
    }
    if body.get("code").and_then(Value::as_str) == Some("proposal_queued") {
        println!(
            "queued proposal {} for review; remote changes were installed and local changes remain uncommitted",
            sanitize_single_line(
                body.get("proposal_id")
                    .and_then(Value::as_str)
                    .unwrap_or("(unknown)")
            )
        );
        return Ok(());
    }
    let pulled = body
        .get("pulled_files")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let status = body
        .get("sync_status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    let seq = body
        .get("headSeq")
        .or_else(|| body.get("seq"))
        .and_then(Value::as_u64);
    print!(
        "reconciled {pulled} remote file{}",
        if pulled == 1 { "" } else { "s" }
    );
    if let Some(seq) = seq {
        print!(", feed seq {seq}");
    }
    println!(" [{}]", sanitize_single_line(status));
    Ok(())
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
        "pulled {} file{} (feed seq {}) into {} [{}]",
        report.files,
        if report.files == 1 { "" } else { "s" },
        report.head_seq,
        // Without --out the destination derives from the hub's slug —
        // hub-authored, so terminal-sanitized.
        sanitize_single_line(&report.dest),
        sanitize_single_line(&report.sync_status),
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

fn push(
    ctx: &Context,
    cfg: &linkmd::HubConfig,
    brain: &str,
    dir: &str,
    resume_local_policy: bool,
) -> CliResult {
    // Strict open: pushing from a non-store is the standard NOT_A_STORE exit.
    let store = Store::open_strict(Path::new(dir))?;
    let _transaction = store.transaction()?;
    let sent = linkmd::collect_push_files(&store)?.len();
    let body = linkmd::sync_push_incremental_with_policy(cfg, brain, &store, resume_local_policy)?;

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );
        return Ok(());
    }

    if body.get("code").and_then(Value::as_str) == Some("proposal_queued") {
        println!(
            "queued proposal {} for review; local changes remain uncommitted",
            sanitize_single_line(
                body.get("proposal_id")
                    .and_then(Value::as_str)
                    .unwrap_or("(unknown)")
            )
        );
        return Ok(());
    }

    let head_seq = body
        .get("headSeq")
        .or_else(|| body.get("seq"))
        .and_then(Value::as_u64);
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
