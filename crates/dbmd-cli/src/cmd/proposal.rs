// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use dbmd_core::linkmd;
use serde_json::Value;

use crate::cli::{ProposalArgs, ProposalCommand};
use crate::context::Context;
use crate::error::CliResult;
use crate::sanitize::sanitize_single_line;

pub fn run(ctx: &Context, args: &ProposalArgs) -> CliResult {
    let value = match &args.command {
        ProposalCommand::List(input) => {
            let cfg = linkmd::hub_config(input.hub.as_deref(), Path::new(&input.dir))?;
            linkmd::proposal_list(
                &cfg,
                strip_sigil(&input.brain),
                &input.state,
                input.after.as_deref(),
                input.limit,
            )?
        }
        ProposalCommand::Show(input) => {
            let cfg = linkmd::hub_config(input.hub.as_deref(), Path::new(&input.dir))?;
            linkmd::proposal_show(&cfg, strip_sigil(&input.brain), &input.proposal_id)?
        }
        ProposalCommand::Accept(input) => {
            let cfg = linkmd::hub_config(input.hub.as_deref(), Path::new(&input.dir))?;
            linkmd::proposal_accept_exact(
                &cfg,
                strip_sigil(&input.brain),
                &input.proposal_id,
                &input.mutation_id,
                &input.reason,
            )?
        }
        ProposalCommand::Reject(input) => {
            let cfg = linkmd::hub_config(input.hub.as_deref(), Path::new(&input.dir))?;
            linkmd::proposal_reject(
                &cfg,
                strip_sigil(&input.brain),
                &input.proposal_id,
                &input.mutation_id,
                &input.reason,
            )?
        }
    };
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
    } else {
        render(&value);
    }
    Ok(())
}

fn render(value: &Value) {
    if let Some(proposals) = value.get("proposals").and_then(Value::as_array) {
        if proposals.is_empty() {
            println!("no proposals");
            return;
        }
        for proposal in proposals {
            println!(
                "{}  {}  {}",
                sanitize_single_line(proposal.get("id").and_then(Value::as_str).unwrap_or("?")),
                sanitize_single_line(proposal.get("state").and_then(Value::as_str).unwrap_or("?")),
                sanitize_single_line(
                    proposal
                        .get("submitted_at")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                )
            );
        }
        return;
    }
    if let Some(proposal) = value.get("proposal") {
        println!(
            "proposal {} [{}]",
            sanitize_single_line(proposal.get("id").and_then(Value::as_str).unwrap_or("?")),
            sanitize_single_line(proposal.get("state").and_then(Value::as_str).unwrap_or("?"))
        );
        if let Some(changes) = proposal.get("changes_base64").and_then(Value::as_str) {
            if let Ok(bytes) =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, changes)
            {
                if let Ok(decoded) = serde_json::from_slice::<Value>(&bytes) {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&decoded).unwrap_or_default()
                    );
                }
            }
        }
        return;
    }
    println!(
        "proposal {} [{}]",
        sanitize_single_line(
            value
                .get("proposal_id")
                .and_then(Value::as_str)
                .unwrap_or("?")
        ),
        sanitize_single_line(
            value
                .get("state")
                .or_else(|| value.get("proposal_state"))
                .and_then(Value::as_str)
                .unwrap_or("accepted")
        )
    );
}

fn strip_sigil(value: &str) -> &str {
    value.trim().strip_prefix('@').unwrap_or(value.trim())
}
