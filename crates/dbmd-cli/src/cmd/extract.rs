//! `dbmd extract <file>` — document text extraction.
//!
//! Thin wrapper: parse [`ExtractArgs`], call [`dbmd_core::extract::extract`]
//! (which owns all format detection + adapter logic), and format the result —
//! plain text to stdout (or `--out <path>`), or a `{text, metadata}` object
//! under `--json`. No extraction logic lives here; this file only moves bytes
//! between `dbmd-core` and the chosen output sink and maps the typed
//! [`dbmd_core::extract::ExtractError`] onto the CLI's stable exit codes.

use std::path::Path;

use dbmd_core::extract::{self, ExtractError};

use crate::cli::ExtractArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// Run `dbmd extract`.
pub fn run(ctx: &Context, args: &ExtractArgs) -> CliResult {
    let path = Path::new(&args.file);

    // All the real work — extension dispatch, PDF/docx/xlsx/epub/html adapters,
    // text normalization, metadata — happens in dbmd-core.
    let extracted = extract::extract(path).map_err(map_extract_error)?;

    if ctx.json {
        // `{text, metadata}` exactly as `Extracted` serializes. Pretty-printed
        // for human-inspectable piping; one object, newline-terminated.
        let json = serde_json::to_string_pretty(&extracted)
            .map_err(|e| CliError::runtime(format!("failed to encode JSON: {e}")))?;
        emit(&args.out, &json, true)
    } else {
        // Plain mode: just the text. Metadata is discarded (it's a `--json`
        // affordance). `extracted.text` already ends in a single newline (or is
        // empty for a no-text-layer document), so don't add another.
        emit(&args.out, &extracted.text, false)
    }
}

/// Write `content` to `--out <path>` when given, else to stdout.
///
/// `add_trailing_newline` appends a `\n` only for JSON output (so a redirected
/// `--json` file ends cleanly); plain text is emitted verbatim because the
/// extractor already normalizes its trailing newline.
fn emit(out: &Option<String>, content: &str, add_trailing_newline: bool) -> CliResult {
    match out {
        Some(path) => {
            let mut body = content.to_string();
            if add_trailing_newline && !body.ends_with('\n') {
                body.push('\n');
            }
            std::fs::write(path, body).map_err(|e| {
                CliError::new(
                    ExitCode::Runtime,
                    "IO_ERROR",
                    format!("failed to write {path}: {e}"),
                )
            })?;
            Ok(())
        }
        None => {
            use std::io::Write;
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let res = if add_trailing_newline {
                writeln!(lock, "{content}")
            } else {
                write!(lock, "{content}")
            };
            res.map_err(|e| {
                // A broken pipe (downstream `head`/`grep` closed) is benign; any
                // other write failure is a real runtime error.
                CliError::new(ExitCode::Runtime, "IO_ERROR", format!("write failed: {e}"))
            })
        }
    }
}

/// Map a `dbmd_core::extract::ExtractError` onto a [`CliError`] with the right
/// exit code + stable machine code. Each variant carries a remediation hint
/// where one is actionable.
fn map_extract_error(err: ExtractError) -> CliError {
    match &err {
        // Bad/unknown extension — a usage-shaped problem, but `clap` owns exit
        // code 2, so a runtime failure with a distinct machine code is the
        // contract here (the file arg parsed fine; its *type* is the issue).
        ExtractError::UnsupportedFormat(_) => CliError::new(
            ExitCode::Runtime,
            err.code(),
            err.to_string(),
        )
        .with_hint(
            "supported document types: .pdf, .docx, .xlsx, .epub, .html (detected by extension)",
        ),
        // Encrypted/locked document — clean refusal, not a crash.
        ExtractError::Encrypted(_) => CliError::new(ExitCode::Runtime, err.code(), err.to_string())
            .with_hint("the document is password-protected; dbmd extract cannot open it"),
        // Corrupt/invalid document for its declared format.
        ExtractError::Parse { .. } => CliError::new(ExitCode::Runtime, err.code(), err.to_string()),
        // Missing/unreadable file.
        ExtractError::Io(_) => CliError::new(ExitCode::Runtime, "IO_ERROR", err.to_string()),
    }
}
