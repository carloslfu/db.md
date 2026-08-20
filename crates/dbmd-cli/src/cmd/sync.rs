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

use crate::cli::{SyncAction, SyncArgs};
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};
use crate::sanitize::sanitize_single_line;

/// Run `dbmd sync`.
pub fn run(ctx: &Context, args: &SyncArgs) -> CliResult {
    if let Some(SyncAction::Conflicts(conflicts)) = &args.action {
        let body =
            linkmd::sync_conflicts(Path::new(&conflicts.dir), conflicts.prune, conflicts.all)?;
        if ctx.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
        } else {
            println!(
                "{} conflict bundle{}, {} pruned",
                body.get("bundles").and_then(Value::as_u64).unwrap_or(0),
                if body.get("bundles").and_then(Value::as_u64) == Some(1) {
                    ""
                } else {
                    "s"
                },
                body.get("pruned").and_then(Value::as_u64).unwrap_or(0),
            );
        }
        return Ok(());
    }
    if let Some(SyncAction::Resolve(resolve)) = &args.action {
        let cfg = linkmd::hub_config(resolve.hub.as_deref(), Path::new(&resolve.dir))?;
        let confirmation = resolve
            .confirm_bulk
            .as_deref()
            .map(linkmd::V2BulkConfirmation::parse)
            .transpose()?;
        let choice = if resolve.keep_local {
            linkmd::V2ConflictChoice::KeepLocal
        } else if resolve.take_remote {
            linkmd::V2ConflictChoice::TakeRemote
        } else {
            linkmd::V2ConflictChoice::From(
                resolve
                    .from
                    .as_deref()
                    .map(std::path::PathBuf::from)
                    .expect("clap requires one conflict resolution choice"),
            )
        };
        let body = linkmd::sync_resolve_conflict(
            &cfg,
            Path::new(&resolve.dir),
            &resolve.bundle,
            choice,
            confirmation.as_ref(),
        )?;
        if ctx.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
        } else {
            println!(
                "resolved conflict bundle {} [{}]",
                sanitize_single_line(&resolve.bundle),
                body.get("class")
                    .and_then(Value::as_str)
                    .map(sanitize_single_line)
                    .unwrap_or_else(|| "completed".to_string())
            );
        }
        return Ok(());
    }
    let brain = strip_sigil(args.brain.as_deref().ok_or_else(|| {
        CliError::new(
            ExitCode::Usage,
            "MISSING_BRAIN",
            "dbmd sync requires BRAIN or the `resolve`/`conflicts` action",
        )
    })?);
    let cfg = linkmd::hub_config(args.hub.as_deref(), Path::new(&args.dir))?;
    let bulk_confirmation = args
        .confirm_bulk
        .as_deref()
        .map(linkmd::V2BulkConfirmation::parse)
        .transpose()?;

    let checkout = Path::new(&args.dir);
    let strict_store_open = Store::open_strict(checkout).is_ok();
    let established_v2 = !strict_store_open && linkmd::has_v2_sync_baseline(&cfg, brain, checkout)?;

    if args.push {
        push(
            ctx,
            &cfg,
            brain,
            &args.dir,
            args.resume_local_policy,
            bulk_confirmation.as_ref(),
        )
    } else if args.out.is_some() {
        pull(ctx, &cfg, brain, args.out.as_deref())
    } else if args.pull_only {
        let destination = (strict_store_open || established_v2).then_some(args.dir.as_str());
        pull(ctx, &cfg, brain, destination)
    } else {
        if strict_store_open || established_v2 {
            converge(
                ctx,
                &cfg,
                brain,
                &args.dir,
                args.resume_local_policy,
                bulk_confirmation.as_ref(),
            )
        } else {
            pull(ctx, &cfg, brain, args.out.as_deref())
        }
    }
}

fn converge(
    ctx: &Context,
    cfg: &linkmd::HubConfig,
    brain: &str,
    dir: &str,
    resume_local_policy: bool,
    bulk_confirmation: Option<&linkmd::V2BulkConfirmation>,
) -> CliResult {
    let body = linkmd::sync_converge_with_options(
        cfg,
        brain,
        Path::new(dir),
        resume_local_policy,
        bulk_confirmation,
    )?;
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
    bulk_confirmation: Option<&linkmd::V2BulkConfirmation>,
) -> CliResult {
    // Strict open: pushing from a non-store is the standard NOT_A_STORE exit.
    let store = Store::open_strict(Path::new(dir))?;
    let _transaction = store.transaction()?;
    let body = linkmd::sync_push_incremental_with_options(
        cfg,
        brain,
        &store,
        resume_local_policy,
        bulk_confirmation,
    )?;
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

    // A v2 receipt reports only the atomic operation count. Do not rescan and
    // materialize the whole brain merely to print a success sentence after an
    // incremental five-file change. Legacy v1 already had to assemble its
    // whole snapshot, so retain the historical file-count wording there.
    let sent = match body.get("applied").and_then(Value::as_u64) {
        Some(value) => value,
        None => linkmd::collect_push_files(&store)?.len() as u64,
    };

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
