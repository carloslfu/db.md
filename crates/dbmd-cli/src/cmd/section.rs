// SPDX-License-Identifier: Apache-2.0

//! `dbmd section get|set|append <file> <heading>` — section-addressed reads
//! and writes.
//!
//! Thin wrapper over `dbmd_core::edit`: sections are addressed by exact
//! heading text and span from the heading line to the next
//! sibling-or-shallower heading (fence-aware, H1 terminates), so `set`
//! replaces a section's whole subtree and `append` lands before the next
//! sibling — the same unit `sections` / `outline` present. `get` is a
//! store-free single-file read (any markdown file, the `sections`
//! convention, file-relative line numbers); `set` / `append` go through the
//! shared write surface in `cmd::body` (store transaction, frozen-page gate,
//! `updated` re-stamp, atomic canonical write, index write-through). A
//! missing heading fails with `SECTION_NOT_FOUND` unless `--create` appends
//! the section at the end of the body; a duplicated heading is
//! `SECTION_AMBIGUOUS` — the edit never guesses.

use std::path::Path;

use dbmd_core::edit::{self, EditError, SectionEdit};
use dbmd_core::parser::{extract_sections_in_file, Section, MAX_DBMD_FILE_BYTES};

use crate::cli::{SectionArgs, SectionCommand, SectionEditArgs, SectionGetArgs};
use crate::cmd::body::{path_str, read_content, EditTarget};
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};
use crate::sanitize::sanitize;

/// Run `dbmd section`.
pub fn run(ctx: &Context, args: &SectionArgs) -> CliResult {
    match &args.command {
        SectionCommand::Get(get) => run_get(ctx, get),
        SectionCommand::Set(edit) => run_edit(ctx, edit, Action::Set),
        SectionCommand::Append(edit) => run_edit(ctx, edit, Action::Append),
    }
}

/// The two edit modes, named for output and dispatch.
#[derive(Clone, Copy)]
enum Action {
    Set,
    Append,
}

impl Action {
    fn word(self) -> &'static str {
        match self {
            Action::Set => "set",
            Action::Append => "append",
        }
    }
}

/// `section get` — print the addressed section verbatim. Store-free, like
/// `sections`; `line` is file-relative (frontmatter offset applied), so the
/// two commands agree on where a section sits.
fn run_get(ctx: &Context, args: &SectionGetArgs) -> CliResult {
    let path = Path::new(&args.file);
    let text = dbmd_core::fsx::read_bounded_nofollow(path, MAX_DBMD_FILE_BYTES)
        .and_then(|bytes| {
            String::from_utf8(bytes)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        })
        .map_err(|e| {
            CliError::new(ExitCode::Runtime, "IO_ERROR", e.to_string())
                .with_hint(format!("could not read `{}`", args.file))
        })?;

    let sections = extract_sections_in_file(&text);
    let section = find_in(&sections, &args.heading, &args.file)?;

    if ctx.json {
        let obj = serde_json::json!({
            "file": args.file,
            "heading": section.heading,
            "level": section.level,
            "line": section.line,
            "body": section.body,
        });
        let mut s = serde_json::to_string_pretty(&obj).unwrap_or_else(|_| "{}".to_string());
        s.push('\n');
        print!("{s}");
    } else {
        print!("{}", sanitize(&section.body));
    }
    Ok(())
}

/// `section set` / `section append` — the write path, through the shared
/// `EditTarget` surface `body` uses.
fn run_edit(ctx: &Context, args: &SectionEditArgs, action: Action) -> CliResult {
    let content = read_content(args.text.as_deref(), args.body_file.as_deref())?;
    let heading = args.heading.trim();
    if heading.is_empty() {
        return Err(CliError::new(
            ExitCode::Runtime,
            "BAD_HEADING",
            "the heading text must not be empty",
        ));
    }
    let mut target = EditTarget::resolve(&args.file)?;

    let attempted = match action {
        Action::Set => edit::replace_section(&target.body, heading, &content),
        Action::Append => edit::append_to_section(&target.body, heading, &content),
    };
    let (edited, created): (SectionEdit, bool) = match attempted {
        Ok(edited) => (edited, false),
        // `--create` turns the missing-heading failure into an append of a
        // fresh section at the end of the body — for BOTH sub-verbs, so an
        // agent can upsert with one call either way.
        Err(EditError::SectionNotFound { .. }) if args.create => (
            edit::append_section(&target.body, heading, args.level, &content),
            true,
        ),
        Err(e) => return Err(edit_error(e, &args.file)),
    };
    let index_updated = target.commit(&edited.body)?;

    if ctx.json {
        let out = serde_json::json!({
            "file": path_str(&target.rel),
            "action": action.word(),
            "heading": heading,
            "level": edited.level,
            "created": created,
            "index_updated": index_updated,
        });
        println!("{out}");
    } else {
        println!("{}", path_str(&target.rel));
        if !index_updated {
            eprintln!(
                "  warning: index not updated; run `dbmd index rebuild --folder <type-folder>`"
            );
        }
    }
    Ok(())
}

/// Locate the addressed section among the extracted ones — the same
/// exact-match + ambiguity contract the core editors apply, here for the
/// store-free `get` (file-relative lines in the error details).
fn find_in<'a>(
    sections: &'a [Section],
    heading: &str,
    file: &str,
) -> Result<&'a Section, CliError> {
    let want = heading.trim();
    let matches: Vec<&Section> = sections.iter().filter(|s| s.heading == want).collect();
    match matches.as_slice() {
        [] => Err(section_not_found(want, file)),
        [one] => Ok(one),
        many => Err(section_ambiguous(
            want,
            many.iter().map(|s| s.line).collect(),
        )),
    }
}

/// Map a core [`EditError`] to the stable CLI contract.
fn edit_error(error: EditError, file: &str) -> CliError {
    match error {
        EditError::SectionNotFound { heading } => {
            section_not_found(&heading, file).with_hint(format!(
                "run `dbmd sections {file}` to list headings, or pass `--create` to add the section"
            ))
        }
        EditError::SectionAmbiguous { heading, lines } => section_ambiguous(&heading, lines),
    }
}

/// Structured error: no section carries the heading (exit `1`).
fn section_not_found(heading: &str, file: &str) -> CliError {
    CliError::new(
        ExitCode::Runtime,
        "SECTION_NOT_FOUND",
        format!("no section with heading `{heading}` in {file}"),
    )
    .with_hint(format!("run `dbmd sections {file}` to list headings"))
}

/// Structured error: the heading matches more than one section (exit `1`) —
/// the address is ambiguous and the operation never guesses.
fn section_ambiguous(heading: &str, lines: Vec<u32>) -> CliError {
    CliError::new(
        ExitCode::Runtime,
        "SECTION_AMBIGUOUS",
        format!(
            "heading `{heading}` matches {} sections; make headings unique to address them",
            lines.len()
        ),
    )
    .with_details(serde_json::json!({ "lines": lines }))
}
