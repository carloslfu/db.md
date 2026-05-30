//! `dbmd spec` — print the bundled canonical SPEC.md.
//!
//! Thin wrapper target: parse [`SpecArgs`], resolve the SPEC source
//! (compiled-in default via `include_str!`, overridable by `--spec <path>` or
//! the `DBMD_SPEC` env var), and print it to stdout. This is the agent
//! bootstrap point — `dbmd spec` loads the standard into a harness's system
//! prompt.
//!
//! Resolution precedence (most specific wins): `--spec <path>` flag, then the
//! `DBMD_SPEC` environment variable, then the SPEC.md compiled in at build
//! time. The flag overriding the env var matches the locked help text on
//! [`SpecArgs::spec`].

use std::path::Path;

use crate::cli::SpecArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// The canonical SPEC.md, compiled into the binary at build time. The repo root
/// (where `SPEC.md` lives) is four levels up from this file
/// (`crates/dbmd-cli/src/cmd/spec.rs`). Bundling it makes `dbmd spec` work from
/// any directory with no filesystem dependency — the install point that loads
/// the standard into an agent's system prompt.
const BUNDLED_SPEC: &str = include_str!("../../SPEC.md");

/// The environment variable that overrides the compiled-in SPEC. `--spec` takes
/// precedence over this.
const SPEC_ENV: &str = "DBMD_SPEC";

/// Run `dbmd spec`.
///
/// Prints the resolved SPEC to stdout verbatim. Under `--json`, wraps it in a
/// single `{"spec": "<text>"}` object so a calling agent can capture it as a
/// JSON string field rather than scraping stdout.
pub fn run(ctx: &Context, args: &SpecArgs) -> CliResult {
    let spec = resolve_spec(args)?;

    if ctx.json {
        let out = serde_json::json!({ "spec": spec });
        println!("{out}");
    } else {
        // Print verbatim. `print!` (not `println!`) so we don't append a
        // newline the bundled file doesn't already carry; the compiled-in
        // SPEC.md ends with its own trailing newline.
        print!("{spec}");
    }
    Ok(())
}

/// Resolve the SPEC text per the precedence `--spec` > `DBMD_SPEC` > compiled-in.
/// A path source that can't be read is a runtime error (the agent asked for a
/// specific SPEC and it isn't there) rather than a silent fall-through to the
/// bundled copy.
fn resolve_spec(args: &SpecArgs) -> Result<String, CliError> {
    if let Some(path) = &args.spec {
        return read_spec_file(Path::new(path), "--spec");
    }
    if let Some(path) = std::env::var_os(SPEC_ENV) {
        return read_spec_file(Path::new(&path), SPEC_ENV);
    }
    Ok(BUNDLED_SPEC.to_string())
}

/// Read a SPEC override file, mapping a read failure to a runtime error that
/// names which source (`--spec` flag or `DBMD_SPEC` env) pointed at it.
fn read_spec_file(path: &Path, source: &str) -> Result<String, CliError> {
    std::fs::read_to_string(path).map_err(|e| {
        CliError::new(
            ExitCode::Runtime,
            "SPEC_READ_FAILED",
            format!("cannot read SPEC from {} ({source}): {e}", path.display()),
        )
        .with_hint("check the path, or omit the override to print the compiled-in SPEC")
    })
}
