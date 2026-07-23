// SPDX-License-Identifier: Apache-2.0

//! `dbmd mirror` — verified replication (link.md §5.6: the feed is the
//! export). The whole chain is verified per §5.4 — not just the head — and
//! the identity is pinned trust-on-first-use, so a later mirror against a
//! swapped identity refuses. What lands is feed + files: the provable full
//! copy, re-servable by `dbmd serve`.

use std::path::Path;

use dbmd_core::linkmd;

use crate::cli::MirrorArgs;
use crate::context::Context;
use crate::error::CliResult;

/// Run `dbmd mirror`.
pub fn run(ctx: &Context, args: &MirrorArgs) -> CliResult {
    let brain = args.brain.trim().trim_start_matches('@');
    let cfg = linkmd::hub_config(None, Path::new("."))?;
    let report = linkmd::mirror(&cfg, brain, Path::new(&args.dir))?;
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("serialize")
        );
    } else {
        println!(
            "mirrored {}: {} entries verified, {} files, pinned {}",
            report.brain, report.entries, report.files, report.pinned
        );
    }
    Ok(())
}
