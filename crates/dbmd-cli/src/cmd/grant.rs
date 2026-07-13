// SPDX-License-Identifier: Apache-2.0

//! `dbmd grant` — issue / list / revoke capability grants, owner-side.
//!
//! Thin wrapper over the `dbmd_core::linkmd` grant calls. v0 hub reality,
//! surfaced honestly: grantees are hub principals named by email, `--scope`
//! is a store-path prefix (and a scoped grant is read-only), expiry is an
//! ISO 8601 `--until`. `--json` prints the hub's response verbatim.

use std::path::Path;

use dbmd_core::linkmd::{self, Capability};
use serde_json::Value;

use crate::cli::{GrantArgs, GrantCapability, GrantCommand};
use crate::context::Context;
use crate::error::CliResult;
use crate::sanitize::sanitize;

/// Run `dbmd grant`.
pub fn run(ctx: &Context, args: &GrantArgs) -> CliResult {
    match &args.command {
        GrantCommand::Issue(a) => {
            let cfg = linkmd::hub_config(a.hub.as_deref(), Path::new(&a.dir))?;
            let body = linkmd::grant_issue(
                &cfg,
                strip_sigil(&a.brain),
                &a.grantee,
                capability(a.can),
                a.scope.as_deref(),
                a.until.as_deref(),
            )?;
            if ctx.json {
                println!("{}", pretty(&body));
                return Ok(());
            }
            // 202-pending (no account yet — an invite was parked) vs 201-granted.
            // Every string below is hub-authored → terminal-sanitized.
            let pending = body
                .get("pending")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let cap = sanitize(
                body.get("capability")
                    .and_then(Value::as_str)
                    .unwrap_or(capability(a.can).as_str()),
            );
            if pending {
                println!(
                    "invited {} ({cap}) — the grant activates when they sign up",
                    a.grantee
                );
            } else {
                let id = sanitize(body.get("id").and_then(Value::as_str).unwrap_or("?"));
                println!("granted {cap} to {} (grant {id})", a.grantee);
            }
            if let Some(scope) = body.get("scopePrefix").and_then(Value::as_str) {
                println!("scope: {}", sanitize(scope));
            }
            if let Some(until) = body.get("expiresAt").and_then(Value::as_str) {
                println!("expires: {}", sanitize(until));
            }
            Ok(())
        }

        GrantCommand::List(a) => {
            let cfg = linkmd::hub_config(a.hub.as_deref(), Path::new(&a.dir))?;
            let body = linkmd::grant_list(&cfg, strip_sigil(&a.brain))?;
            if ctx.json {
                println!("{}", pretty(&body));
                return Ok(());
            }
            let grants = body
                .get("grants")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let invites = body
                .get("invites")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if grants.is_empty() && invites.is_empty() {
                println!("no grants");
                return Ok(());
            }
            // Every field is hub-authored → terminal-sanitized on the way out.
            let field =
                |v: &Value, key: &str| sanitize(v.get(key).and_then(Value::as_str).unwrap_or("?"));
            for g in &grants {
                println!(
                    "{}  {}  {}{}{}",
                    field(g, "id"),
                    field(g, "capability"),
                    field(g, "email"),
                    g.get("scopePrefix")
                        .and_then(Value::as_str)
                        .map(|s| format!("  scope={}", sanitize(s)))
                        .unwrap_or_default(),
                    g.get("expiresAt")
                        .and_then(Value::as_str)
                        .map(|s| format!("  until={}", sanitize(s)))
                        .unwrap_or_default(),
                );
            }
            for i in &invites {
                println!(
                    "{}  {}  {}  (invited, pending signup)",
                    field(i, "id"),
                    field(i, "capability"),
                    field(i, "email"),
                );
            }
            Ok(())
        }

        GrantCommand::Revoke(a) => {
            let cfg = linkmd::hub_config(a.hub.as_deref(), Path::new(&a.dir))?;
            let body = linkmd::grant_revoke(&cfg, strip_sigil(&a.brain), &a.grant_id)?;
            if ctx.json {
                println!("{}", pretty(&body));
                return Ok(());
            }
            println!("revoked {}", a.grant_id);
            Ok(())
        }
    }
}

/// clap's value-enum → the library capability.
fn capability(c: GrantCapability) -> Capability {
    match c {
        GrantCapability::Read => Capability::Read,
        GrantCapability::Write => Capability::Write,
    }
}

/// Accept `@brain` and `brain` alike — the sigil is address sugar.
fn strip_sigil(s: &str) -> &str {
    s.trim().strip_prefix('@').unwrap_or(s.trim())
}

/// Pretty JSON (repo convention: pretty + trailing newline via `println!`).
fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_string())
}
