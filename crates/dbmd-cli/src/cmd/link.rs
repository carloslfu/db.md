//! `dbmd link <from> <to>` — append a wiki-link to a file.
//!
//! Thin wrapper target: parse [`LinkArgs`], enforce the `DB.md` frozen-page
//! policy on `<from>`, append a full store-relative `[[<to>]]` wiki-link to
//! `<from>`'s body via the `dbmd_core::parser` read/write round-trip, then keep
//! the catalog current write-through (`dbmd_core::index::on_write`). Report the
//! result (text or `--json`).
//!
//! The link target is always emitted as a **full store-relative path** (the
//! doctrine; a short-form target is a `dbmd validate` error). `link` refuses a
//! short-form `<to>` up front rather than writing an invalid edge.

use std::path::Path;

use dbmd_core::Store;

use crate::cli::LinkArgs;
use crate::cmd::write::{
    core_err, enforce_frozen, index_on_write, open_store, require_store_relative,
};
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// The two canonical layer dirs a full-path wiki-link target must start with.
const LAYER_DIRS: [&str; 2] = ["sources", "records"];

/// Run `dbmd link`.
///
/// Steps: (1) open the store; (2) refuse if `<from>` is a frozen page; (3) refuse
/// a short-form `<to>` (must be a full store-relative path); (4) append
/// `[[<to>]]` to `<from>`'s body; (5) update the index write-through; (6) report.
pub fn run(ctx: &Context, args: &LinkArgs) -> CliResult {
    let store = open_store(&args.dir)?;

    let from_rel = require_store_relative(&store, &args.from)?;
    let from_abs = store.abs_path(&from_rel);
    if !from_abs.exists() {
        return Err(missing_from_error(&from_rel));
    }

    // Policy: refuse a write to a frozen `<from>`.
    enforce_frozen(&store, &from_rel)?;

    // The target is recorded as a bare, full store-relative path.
    let target = canonical_link_target(&store, &args.to)?;

    append_wiki_link(&from_abs, &target)?;
    let index_warning = index_on_write(&store, &from_rel);

    emit_result(ctx, &path_to_unix(&from_rel), &target, &index_warning);
    Ok(())
}

/// Append `[[<target>]]` to a file's body, preserving frontmatter + the existing
/// body verbatim. The link goes on its own line at the end of the body, with a
/// single separating blank line if the body has content and doesn't already end
/// in a blank line.
fn append_wiki_link(abs: &Path, target: &str) -> Result<(), CliError> {
    let (mut fm, mut body) = dbmd_core::parser::read_file(abs).map_err(core_err)?;

    let link_line = format!("[[{target}]]\n");
    if body.is_empty() {
        body = link_line;
    } else {
        if !body.ends_with('\n') {
            body.push('\n');
        }
        // One blank line between prior content and the appended link.
        if !body.ends_with("\n\n") {
            body.push('\n');
        }
        body.push_str(&link_line);
    }

    // `link` edits the file's content (it appends a wiki-link to the body), so
    // re-stamp the auto-maintained `updated` timestamp the same way `write` sets
    // it on create and `fm set` bumps it on edit. Without this, the type-folder
    // `index.md` recency ordering and `dbmd search --updated-after` never reflect
    // the edit (SPEC: `updated` is auto-maintained on content edits).
    fm.updated = Some(dbmd_core::now());

    dbmd_core::parser::write_file(abs, &fm, &body).map_err(core_err)?;
    Ok(())
}

/// Normalize `<to>` to a canonical wiki-link target: `/` separators, no leading
/// `./`, no trailing `.md`. Refuses a short-form target (one that doesn't start
/// with a layer dir) so `link` never writes an edge `dbmd validate` would flag
/// `WIKI_LINK_SHORT_FORM`.
fn canonical_link_target(store: &Store, raw: &str) -> Result<String, CliError> {
    let rel = require_store_relative(store, raw)?;
    let unix = path_to_unix(&rel);
    let bare = unix.strip_suffix(".md").unwrap_or(&unix).to_string();

    let head = bare.split('/').next().unwrap_or("");
    let is_full_path = bare.contains('/') && LAYER_DIRS.contains(&head);
    if !is_full_path {
        return Err(short_form_error(raw));
    }
    Ok(bare)
}

/// Structured error: the `<from>` file doesn't exist (exit `1`).
fn missing_from_error(from: &Path) -> CliError {
    CliError::runtime(format!(
        "cannot link from `{}`: file does not exist",
        path_to_unix(from)
    ))
    .with_hint("create it first with `dbmd write`")
}

/// Structured error: the `<to>` target is short-form (exit `1`, code
/// `WIKI_LINK_SHORT_FORM` — mirrors the validate code).
fn short_form_error(raw: &str) -> CliError {
    CliError::new(
        ExitCode::Runtime,
        dbmd_core::validate::codes::WIKI_LINK_SHORT_FORM,
        format!("link target `{raw}` is not a full store-relative path"),
    )
    .with_hint("use the full path, e.g. `records/contacts/sarah-chen` (no short-form, no `.md`)")
}

/// Emit the success result. Stdout stays clean (the linked-from path, or the
/// `--json` object); a non-fatal index warning goes to stderr.
fn emit_result(ctx: &Context, from: &str, to: &str, index_warning: &Option<String>) {
    if let Some(w) = index_warning {
        eprintln!("dbmd: warning: {w}");
    }
    if ctx.json {
        let out = serde_json::json!({
            "linked": from,
            "to": to,
        });
        println!("{out}");
    } else {
        println!("{from} -> [[{to}]]");
    }
}

/// Render a path with `/` separators on every OS.
fn path_to_unix(p: &Path) -> String {
    p.components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn store_with_db_md() -> (TempDir, Store) {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("DB.md"), "---\ntype: db-md\n---\n# s\n").unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn canonical_link_target_accepts_full_path_and_strips_md() {
        let (_d, store) = store_with_db_md();
        assert_eq!(
            canonical_link_target(&store, "records/contacts/sarah.md").unwrap(),
            "records/contacts/sarah"
        );
        assert_eq!(
            canonical_link_target(&store, "records/concepts/scale").unwrap(),
            "records/concepts/scale"
        );
    }

    #[test]
    fn canonical_link_target_rejects_short_form() {
        let (_d, store) = store_with_db_md();
        let err = canonical_link_target(&store, "sarah-chen").unwrap_err();
        assert_eq!(err.code, dbmd_core::validate::codes::WIKI_LINK_SHORT_FORM);
        // A path under a non-layer dir is also short-form for our purposes.
        assert!(canonical_link_target(&store, "people/sarah").is_err());
    }

    #[test]
    fn append_wiki_link_preserves_frontmatter_and_appends_line() {
        let (_d, store) = store_with_db_md();
        let abs = store.root.join("records/contacts/sarah.md");
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(
            &abs,
            "---\ntype: contact\nsummary: x\n---\n# Sarah\n\nNotes.\n",
        )
        .unwrap();

        append_wiki_link(&abs, "records/companies/acme").unwrap();
        let text = fs::read_to_string(&abs).unwrap();
        assert!(text.contains("[[records/companies/acme]]"));
        // Frontmatter survived.
        assert!(text.starts_with("---\ntype: contact\n"));
        // Prior body survived.
        assert!(text.contains("# Sarah"));
        assert!(text.contains("Notes."));
    }

    #[test]
    fn append_wiki_link_into_empty_body() {
        let (_d, store) = store_with_db_md();
        let abs = store.root.join("records/contacts/empty.md");
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(&abs, "---\ntype: contact\nsummary: x\n---\n").unwrap();
        append_wiki_link(&abs, "records/companies/acme").unwrap();
        let text = fs::read_to_string(&abs).unwrap();
        assert!(text.ends_with("[[records/companies/acme]]\n"));
    }
}
