// SPDX-License-Identifier: Apache-2.0

//! `dbmd key` — agent signing keys (link.md §8 `LinkMD-Sig`).
//!
//! `generate` mints the keypair LOCALLY: the secret is written to a 0600
//! file and never travels; the printed public identity is what a hub's
//! register endpoint takes. With `DBMD_AGENT_KEY_FILE` set, every
//! authenticated verb signs its request per link.md §8 instead of sending a
//! bearer — the possession proof binds one method, one path, one body, one
//! ±60s window, so a leaked transcript or log line contains nothing reusable.

use std::path::Path;

use dbmd_core::linkmd;

use crate::cli::{KeyArgs, KeyCommand};
use crate::context::Context;
use crate::error::CliResult;

/// Run `dbmd key`.
pub fn run(ctx: &Context, args: &KeyArgs) -> CliResult {
    match &args.command {
        KeyCommand::Generate(generate) => {
            let minted = linkmd::generate_agent_key(Path::new(&generate.out))?;
            if ctx.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&minted).expect("serialize")
                );
            } else {
                println!("multikey:      {}", minted.multikey);
                println!("publicKeySpki: {}", minted.public_key_spki);
                println!(
                    "key file:      {} (0600 — the secret; never share, never commit)",
                    minted.key_file
                );
                println!();
                println!("register the publicKeySpki with your hub, then:");
                println!(
                    "  export {}={}",
                    linkmd::AGENT_KEY_FILE_ENV,
                    minted.key_file
                );
            }
            Ok(())
        }
    }
}
