// SPDX-License-Identifier: Apache-2.0

//! `dbmd login` / `dbmd logout` — subscription sign-in for the embedded
//! harness. Today one provider takes a native login: `codex`, which runs
//! OpenAI's public PKCE flow so a ChatGPT Plus/Pro subscription drives
//! `dbmd ask` with no vendor CLI installed.
//!
//! The browser opens on the authorize URL, this process serves
//! `127.0.0.1:1455` for the redirect, and the resulting tokens land in the
//! toolkit state directory (0600) — never inside a store. `--code` skips the
//! browser entirely for headless machines: run the printed URL anywhere, then
//! paste back the code (or the whole redirect URL).
//!
//! Other subscriptions (Claude Pro/Max, Copilot) are reached by delegating to
//! their own logged-in CLI — `--provider claude-code` / `codex-cli` — which
//! needs no login here.

use std::time::Duration;

use dbmd_core::harness::{auth, oauth};

use crate::cli::{LoginArgs, LogoutArgs};
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// The only provider with a native login today.
const CODEX: &str = "codex";

fn config_error(error: dbmd_core::harness::HarnessError) -> CliError {
    CliError::new(ExitCode::Runtime, "LOGIN_FAILED", error.to_string())
}

fn unknown_provider(name: &str) -> CliError {
    CliError::new(
        ExitCode::Runtime,
        "LOGIN_UNKNOWN_PROVIDER",
        format!("`{name}` has no native login"),
    )
    .with_hint(
        "the only native login is `dbmd login codex` (a ChatGPT Plus/Pro \
         subscription). Claude Pro/Max and Copilot subscriptions are used by \
         delegating to their own logged-in CLI: `dbmd ask … --provider claude-code`",
    )
}

/// How long the loopback callback waits before falling back to a paste.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);

/// Run `dbmd login`.
pub fn run(ctx: &Context, args: &LoginArgs) -> CliResult {
    if args.status {
        let providers = auth::logged_in_providers().map_err(config_error)?;
        if ctx.json {
            println!(
                "{}",
                serde_json::json!({
                    "logged_in": providers,
                    "credentials": auth::auth_path().map_err(config_error)?.to_string_lossy(),
                })
            );
        } else if providers.is_empty() {
            println!("not signed in to any provider");
        } else {
            for provider in &providers {
                let detail = auth::peek(provider)
                    .ok()
                    .flatten()
                    .and_then(|tokens| oauth::plan_type(&tokens.access))
                    .map(|plan| format!(" ({plan})"))
                    .unwrap_or_default();
                println!("signed in: {provider}{detail}");
            }
        }
        return Ok(());
    }

    let provider = args.provider.as_deref().unwrap_or(CODEX);
    if provider != CODEX {
        return Err(unknown_provider(provider));
    }

    let pkce = oauth::generate_pkce().map_err(config_error)?;
    let state = oauth::create_state().map_err(config_error)?;
    let url = oauth::authorize_url(&pkce.challenge, &state);

    // Paste mode never binds the port, so it works over SSH and in parallel.
    let code = if args.code {
        eprintln!("Open this URL, sign in, then paste the result below:\n\n{url}\n");
        eprint!("code (or the full redirect URL): ");
        use std::io::Write as _;
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|error| CliError::new(ExitCode::Runtime, "LOGIN_FAILED", error.to_string()))?;
        let (code, pasted_state) = oauth::parse_authorization_input(&line);
        if let Some(pasted_state) = pasted_state {
            if pasted_state != state {
                return Err(CliError::new(
                    ExitCode::Runtime,
                    "LOGIN_STATE_MISMATCH",
                    "the pasted state does not match this login attempt",
                )
                .with_hint("run `dbmd login codex` again and use the newest URL"));
            }
        }
        code.ok_or_else(|| {
            CliError::new(
                ExitCode::Runtime,
                "LOGIN_FAILED",
                "no authorization code was pasted",
            )
        })?
    } else {
        eprintln!("Opening your browser to sign in to ChatGPT…");
        eprintln!("If it does not open, visit:\n\n{url}\n");
        // Bind the callback BEFORE the browser can redirect to it.
        let waiting = std::thread::spawn({
            let state = state.clone();
            move || oauth::wait_for_callback(&state, CALLBACK_TIMEOUT)
        });
        std::thread::sleep(Duration::from_millis(150));
        if !oauth::open_browser(&url) {
            eprintln!("(could not launch a browser — open the URL above yourself)");
        }
        let outcome = waiting.join().map_err(|_| {
            CliError::new(
                ExitCode::Runtime,
                "LOGIN_FAILED",
                "the callback listener panicked",
            )
        })?;
        outcome.map_err(config_error)?.ok_or_else(|| {
            CliError::new(
                ExitCode::Runtime,
                "LOGIN_TIMEOUT",
                "no authorization arrived within 3 minutes",
            )
            .with_hint("retry, or use `dbmd login codex --code` to paste the code yourself")
        })?
    };

    let tokens = oauth::exchange_code(&code, &pkce.verifier).map_err(config_error)?;
    let plan = oauth::plan_type(&tokens.access);
    if oauth::account_id(&tokens.access).is_none() {
        return Err(CliError::new(
            ExitCode::Runtime,
            "LOGIN_NO_ACCOUNT",
            "the issued token carries no ChatGPT account id",
        )
        .with_hint("this account may not have Codex access; check your ChatGPT plan"));
    }
    auth::save(CODEX, &tokens).map_err(config_error)?;

    let path = auth::auth_path().map_err(config_error)?;
    if ctx.json {
        println!(
            "{}",
            serde_json::json!({
                "logged_in": CODEX,
                "plan": plan,
                "credentials": path.to_string_lossy(),
            })
        );
    } else {
        let plan = plan.map(|plan| format!(" ({plan})")).unwrap_or_default();
        println!(
            "signed in to ChatGPT{plan} — credentials in {}",
            path.display()
        );
        println!("run: dbmd ask \"…\" --provider codex");
    }
    Ok(())
}

/// Run `dbmd logout`.
pub fn run_logout(ctx: &Context, args: &LogoutArgs) -> CliResult {
    let provider = args.provider.as_deref().unwrap_or(CODEX);
    let removed = auth::clear(provider).map_err(config_error)?;
    if ctx.json {
        println!(
            "{}",
            serde_json::json!({ "provider": provider, "removed": removed })
        );
    } else if removed {
        println!("signed out of {provider}");
    } else {
        println!("not signed in to {provider}");
    }
    Ok(())
}
