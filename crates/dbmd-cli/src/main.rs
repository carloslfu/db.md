//! `dbmd` — the reference command-line tool for **db.md**, the open database in
//! plain files.
//!
//! This binary is a **thin** wrapper: it parses arguments (clap), builds the
//! global [`Context`], dispatches to the matching subcommand body in [`cmd`],
//! and maps the result to the stable exit-code convention. **All toolkit logic
//! lives in `dbmd-core`** — keep `main.rs`, [`cli`], and the dispatch free of
//! business logic so the subcommand-body agents only ever touch `cmd/<name>.rs`.
//!
//! Agent-primary ergonomics (SPEC.md § Tooling), enforced at this layer:
//!   - `--json` is a global flag; every subcommand honors it. Errors render as
//!     `{"error": {"code", "message", "hint"}}` on stderr under `--json`.
//!   - `--color <auto|always|never>` defaults to `auto`, which means *off*
//!     (pipe-safe). Color is never auto-detected from a TTY.
//!   - No interactive prompts anywhere; flags only.
//!   - Exit codes are a documented contract (see [`error`]). clap owns exit
//!     code `2` for argument-parse failures; `--help` / `--version` exit `0`.

// The command tree, dispatch, and every subcommand body are fully implemented.
// What stays unconsumed is part of the *locked interface*, not dead product
// code: the reserved corners of the `ExitCode` vocabulary (`Usage`, owned by
// clap; `NotImplemented`/`64`, a documented stable code kept for future bodies)
// plus their `CliError` constructor, and the `--color` plumbing that the bodies
// thread through `Context` but do not yet branch on. The dead-code lint fires on
// exactly those not-yet-consumed members of the interface, so allow it
// crate-wide. This relaxes only `dead_code` — no correctness lint is touched.
#![allow(dead_code)]

mod cli;
mod cmd;
mod context;
mod error;

use clap::Parser;

use crate::cli::{Cli, Command};
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

fn main() {
    // clap handles `--help` / `--version` (exit 0) and arg-parse errors
    // (exit 2) before returning. Everything past here is a parsed invocation.
    let cli = Cli::parse();

    let ctx = Context {
        json: cli.json,
        color: cli.color,
    };

    let result = dispatch(&ctx, &cli.command);

    match result {
        Ok(()) => std::process::exit(ExitCode::Success.code()),
        Err(err) => {
            emit_error(&ctx, &err);
            std::process::exit(err.exit.code());
        }
    }
}

/// Exhaustive dispatch over the locked [`Command`] tree. Each arm calls exactly
/// one subcommand body. This match and the [`Command`] enum are the only two
/// places that change when a subcommand is added or removed — the bodies in
/// [`cmd`] never touch this file.
fn dispatch(ctx: &Context, command: &Command) -> CliResult {
    match command {
        Command::Validate(args) => cmd::validate::run(ctx, args),
        Command::Format(args) => cmd::format::run(ctx, args),
        Command::Query(args) => cmd::query::run(ctx, args),
        Command::Links(args) => cmd::links::run(ctx, args),
        Command::Sections(args) => cmd::sections::run(ctx, args),
        Command::Extract(args) => cmd::extract::run(ctx, args),
        Command::Search(args) => cmd::search::run(ctx, args),
        Command::Graph(args) => cmd::graph::run(ctx, args),
        Command::Fm(args) => cmd::fm::run(ctx, args),
        Command::Tree(args) => cmd::tree::run(ctx, args),
        Command::Stats(args) => cmd::stats::run(ctx, args),
        Command::Outline(args) => cmd::outline::run(ctx, args),
        Command::Index(args) => cmd::index::run(ctx, args),
        Command::Log(args) => cmd::log::run(ctx, args),
        Command::Write(args) => cmd::write::run(ctx, args),
        Command::Link(args) => cmd::link::run(ctx, args),
        Command::Rename(args) => cmd::rename::run(ctx, args),
        Command::Spec(args) => cmd::spec::run(ctx, args),
        Command::InstallSkill(args) => cmd::skill::install(ctx, args),
        Command::UninstallSkill(args) => cmd::skill::uninstall(ctx, args),
    }
}

/// Render an error to stderr: a structured `{"error": {...}}` object under
/// `--json`, or a `dbmd: <message>` line (plus an optional hint) otherwise.
fn emit_error(ctx: &Context, err: &CliError) {
    if ctx.json {
        // Compact, one-line JSON so callers can parse stderr line-by-line.
        eprintln!("{}", err.to_json());
    } else {
        eprintln!("dbmd: {}", err.message);
        if let Some(hint) = &err.hint {
            eprintln!("  hint: {hint}");
        }
    }
}
