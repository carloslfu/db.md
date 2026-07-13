// SPDX-License-Identifier: Apache-2.0

//! `dbmd resolve` — the link.md client's address resolution.
//!
//! Thin wrapper: parse the `@brain[/id]` address, resolve the hub
//! configuration (flag > env > `.dbmd/config`), call
//! [`dbmd_core::linkmd::resolve`], and render. `--json` prints the hub's
//! response verbatim (the hub is the source of truth; reshaping it here would
//! only invite drift); text mode renders the load-bearing fields.

use std::path::Path;

use dbmd_core::linkmd::{self, Address, AddressTarget};
use serde_json::Value;

use crate::cli::ResolveArgs;
use crate::context::Context;
use crate::error::CliResult;
use crate::sanitize::sanitize;

/// Run `dbmd resolve`.
pub fn run(ctx: &Context, args: &ResolveArgs) -> CliResult {
    let addr = Address::parse(&args.address)?;
    let cfg = linkmd::hub_config(args.hub.as_deref(), Path::new(&args.dir))?;
    let body = linkmd::resolve(&cfg, &addr)?;

    if ctx.json {
        println!("{}", pretty(&body));
        return Ok(());
    }

    match &addr.target {
        // The brain card: identity + shape at a glance.
        None => {
            print_field(&body, "id", "id");
            print_field(&body, "slug", "slug");
            print_field(&body, "name", "name");
            print_field(&body, "scope", "scope");
            print_field(&body, "visibility", "visibility");
            print_field(&body, "handle", "handle");
            if let Some(seq) = body.get("indexedFeedSeq").and_then(Value::as_u64) {
                println!("feed seq: {seq}");
            }
            if let Some(stats) = body.get("stats") {
                if let (Some(r), Some(s)) = (
                    stats.get("records").and_then(Value::as_u64),
                    stats.get("sources").and_then(Value::as_u64),
                ) {
                    println!("records: {r}");
                    println!("sources: {s}");
                }
            }
        }
        // A record: a small header, then the body verbatim.
        Some(AddressTarget::Id(_)) | Some(AddressTarget::Path(_)) => {
            if let Some(doc) = body.get("document") {
                print_field(doc, "path", "path");
                print_field(doc, "id", "id");
                print_field(doc, "type", "type");
                print_field(doc, "summary", "summary");
                if let Some(text) = doc.get("body").and_then(Value::as_str) {
                    // Hub-authored text: strip terminal control sequences
                    // before it reaches a human terminal (`--json` above
                    // returned the verbatim body already).
                    let text = sanitize(text);
                    println!();
                    print!("{text}");
                    if !text.ends_with('\n') {
                        println!();
                    }
                }
            }
        }
    }
    Ok(())
}

/// Print `label: <string value>` when the field is a non-empty string. The
/// value is hub-authored, so it is terminal-sanitized on the way out.
fn print_field(v: &Value, key: &str, label: &str) {
    if let Some(s) = v.get(key).and_then(Value::as_str) {
        if !s.is_empty() {
            println!("{label}: {}", sanitize(s));
        }
    }
}

/// Pretty JSON (repo convention: pretty + trailing newline via `println!`).
fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_string())
}
