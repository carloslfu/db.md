// SPDX-License-Identifier: Apache-2.0

//! `dbmd subscribe` — follow a brain's feed head.
//!
//! v0 subscription is head-movement detection: the hub does not yet serve
//! per-entry feed reads, so the client polls the brain's feed cursor and
//! emits an event each time it advances (the caller re-pulls or re-queries on
//! advance). `--once` reads the current head and exits — the composable
//! building block; the loop is a convenience over it.
//!
//! Output contract: one event per line. Under `--json` each line is a single
//! compact JSON object (NDJSON — a stream, unlike the one-shot commands'
//! pretty bodies) so a consumer can parse line-by-line. Transient transport
//! errors do not kill the loop (they warn on stderr and the poll retries);
//! an HTTP error does (a revoked grant or a deleted brain is a real state
//! change, not noise).

use std::path::Path;

use dbmd_core::linkmd::{self, LinkError};

use crate::cli::SubscribeArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};
use crate::sanitize::sanitize;

/// Run `dbmd subscribe`.
pub fn run(ctx: &Context, args: &SubscribeArgs) -> CliResult {
    if args.interval == 0 {
        return Err(CliError::new(
            ExitCode::Runtime,
            "BAD_INTERVAL",
            "--interval must be at least 1 second",
        ));
    }

    let brain = args.brain.trim().trim_start_matches('@');
    let cfg = linkmd::hub_config(args.hub.as_deref(), Path::new(&args.dir))?;

    let head = linkmd::head(&cfg, brain)?;
    let mut baseline = args.since.unwrap_or(head.seq);

    // First observation: always reported, so `--once` is a usable head read
    // and a loop consumer knows its starting point.
    emit(ctx, &head, baseline);
    if args.once {
        return Ok(());
    }
    baseline = baseline.max(head.seq);

    loop {
        std::thread::sleep(std::time::Duration::from_secs(args.interval));
        match linkmd::head(&cfg, brain) {
            Ok(h) => {
                if h.seq > baseline {
                    emit(ctx, &h, baseline);
                    baseline = h.seq;
                }
            }
            // A hub blip must not kill a follower; a real HTTP answer must.
            Err(LinkError::Transport { hub, message }) => {
                eprintln!("dbmd: subscribe: hub unreachable at {hub} ({message}); retrying");
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// One event line. Text: `seq <prev> -> <seq>`; JSON: a compact object. The
/// brain id in the text form is hub-authored → terminal-sanitized (the JSON
/// stream stays verbatim).
fn emit(ctx: &Context, head: &linkmd::Head, prev: u64) {
    if ctx.json {
        let mut v = serde_json::to_value(head).unwrap_or_default();
        v["prev"] = serde_json::json!(prev);
        // Compact, one object per line: this is a stream, not a document.
        println!("{v}");
    } else if head.seq == prev {
        println!("{} at feed seq {}", sanitize(&head.brain), head.seq);
    } else {
        println!(
            "{} advanced: feed seq {} -> {}",
            sanitize(&head.brain),
            prev,
            head.seq
        );
    }
}
