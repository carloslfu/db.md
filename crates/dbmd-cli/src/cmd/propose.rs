// SPDX-License-Identifier: Apache-2.0

//! `dbmd propose` — submit evidence to a published site's inbox.
//!
//! Write without trust: the submission lands in the owner's `sources/inbox/`
//! (never as truth) for their curator to accept or reject. The door is
//! unauthenticated by design — no credential is sent, hub-side rate limits
//! and per-brain caps guard it. Exactly one body source is required:
//! `--body <text>` inline, or `--body-file <path>` (e.g. a record file whose
//! full text travels as the evidence).

use std::path::Path;

use dbmd_core::linkmd;
use serde_json::Value;

use crate::cli::ProposeArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// Run `dbmd propose`.
pub fn run(ctx: &Context, args: &ProposeArgs) -> CliResult {
    let body = match (&args.body, &args.body_file) {
        (Some(text), None) => text.clone(),
        (None, Some(path)) => std::fs::read_to_string(path).map_err(|e| {
            CliError::new(
                ExitCode::Runtime,
                "IO_ERROR",
                format!("reading --body-file {path}: {e}"),
            )
        })?,
        _ => {
            return Err(CliError::new(
                ExitCode::Runtime,
                "BAD_BODY",
                "exactly one body source is required",
            )
            .with_hint("pass --body <text> or --body-file <path>"));
        }
    };

    let site = args.site.trim().trim_start_matches('@');
    let cfg = linkmd::hub_config(args.hub.as_deref(), Path::new(&args.dir))?;
    let receipt = linkmd::propose(&cfg, site, &args.app, &body)?;

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&receipt).unwrap_or_default()
        );
        return Ok(());
    }

    println!(
        "proposed to @{site}/{} — landed as {}",
        args.app,
        receipt
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("(inbox)"),
    );
    Ok(())
}
