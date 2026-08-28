// SPDX-License-Identifier: Apache-2.0

//! `dbmd body set|append <file>` — whole-body edit.
//!
//! Thin wrapper: resolve the store from the file itself (nearest ancestor
//! `DB.md`), take the store transaction, apply the frozen-page gate, replace
//! or extend the body (`dbmd_core::edit::append_body` for the raw append),
//! re-stamp `updated` (SPEC: auto-maintained on every edit), write back
//! atomically (canonical frontmatter, body byte-exact as given), and fold
//! the file into its type-folder index write-through — the exact `fm set`
//! contract, aimed at the body instead of a frontmatter key. `summary` is
//! never recomputed: the catalog line is the agent's judgment, changed only
//! explicitly (`fm set summary=…`).
//!
//! This module also hosts the shared edit-target plumbing (`EditTarget`,
//! `read_content`) that `section` reuses — the body/section editors are one
//! write surface with two addressing modes.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use dbmd_core::parser::{Frontmatter, MAX_DBMD_FILE_BYTES};
use dbmd_core::store::StoreTransaction;
use dbmd_core::Store;

use crate::cli::{BodyArgs, BodyCommand, BodyEditArgs};
use crate::cmd::file_target::{lexical_absolute_before_open, locate_store, store_relative};
use crate::cmd::write::{enforce_frozen, index_on_write, require_store_relative};
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// Run `dbmd body`.
pub fn run(ctx: &Context, args: &BodyArgs) -> CliResult {
    match &args.command {
        BodyCommand::Set(edit) => run_edit(ctx, edit, "set"),
        BodyCommand::Append(edit) => run_edit(ctx, edit, "append"),
    }
}

/// One body edit: `set` stores the content verbatim as the new body,
/// `append` joins it raw onto the existing body.
fn run_edit(ctx: &Context, args: &BodyEditArgs, action: &'static str) -> CliResult {
    let content = read_content(args.text.as_deref(), args.body_file.as_deref())?;
    let mut target = EditTarget::resolve(&args.file)?;

    let new_body = match action {
        "set" => content,
        _ => dbmd_core::edit::append_body(&target.body, &content),
    };
    let index_updated = target.commit(&new_body)?;

    if ctx.json {
        let out = serde_json::json!({
            "file": path_str(&target.rel),
            "action": action,
            "bytes": new_body.len(),
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

/// A resolved, locked, policy-checked edit target: the store (transaction
/// held for the value's lifetime), the store-relative path, and the parsed
/// `(frontmatter, body)` pair. The one construction path both `body` and
/// `section` edits go through, so the guard order can never drift between
/// them.
pub(crate) struct EditTarget {
    pub(crate) store: Store,
    pub(crate) rel: PathBuf,
    pub(crate) fm: Frontmatter,
    pub(crate) body: String,
    _transaction: StoreTransaction,
}

impl EditTarget {
    /// Locate the store from the file, lock it, gate on policy, and read the
    /// target: nearest-ancestor store resolution (the `format` flavor), the
    /// store-wide transaction, the dotfile/outside-store path gates, the
    /// frozen-page refusal, then the parse.
    pub(crate) fn resolve(file: &str) -> Result<Self, CliError> {
        let input = Path::new(file);
        let abs = lexical_absolute_before_open(input)?;
        let store = locate_store(&abs)?;
        let transaction = store.transaction().map_err(CliError::from)?;
        let rel = store_relative(&store, &abs);
        // Re-run the write-surface path gates on the resolved relative path
        // (dotfiles and store escapes refused with their stable codes).
        let rel = require_store_relative(&store, &path_str(&rel))?;
        enforce_frozen(&store, &rel)?;
        let (fm, body) = store
            .read_file(&rel)
            .map_err(|e| CliError::from(dbmd_core::Error::from(e)))?;
        Ok(Self {
            store,
            rel,
            fm,
            body,
            _transaction: transaction,
        })
    }

    /// Write the new body back: re-stamp `updated`, atomic canonical write,
    /// index write-through. Returns whether the index update succeeded
    /// (non-fatal on failure, the write-surface convention).
    pub(crate) fn commit(&mut self, new_body: &str) -> Result<bool, CliError> {
        // Auto-maintain `updated`: a body edit is an edit (SPEC: `updated` is
        // auto-maintained), so recency ordering and `--updated-after` queries
        // reflect it — the same re-stamp `fm set` and `link` perform.
        self.fm.updated = Some(dbmd_core::now());
        self.store
            .write_file(&self.rel, &self.fm, new_body)
            .map_err(|e| CliError::from(dbmd_core::Error::from(e)))?;
        Ok(index_on_write(&self.store, &self.rel).is_none())
    }
}

/// Resolve the edit content from `--text` / `--body-file` (clap guarantees
/// exactly one): inline text verbatim, a bounded file read, or bounded
/// standard input for `-`.
pub(crate) fn read_content(
    text: Option<&str>,
    body_file: Option<&str>,
) -> Result<String, CliError> {
    if let Some(t) = text {
        return Ok(t.to_string());
    }
    let path = body_file.unwrap_or_default();
    let bytes = if path == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .lock()
            .take(MAX_DBMD_FILE_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(CliError::from)?;
        if buf.len() as u64 > MAX_DBMD_FILE_BYTES {
            return Err(CliError::new(
                ExitCode::Runtime,
                "CONTENT_TOO_LARGE",
                format!("stdin content exceeds the {MAX_DBMD_FILE_BYTES}-byte file bound"),
            ));
        }
        buf
    } else {
        dbmd_core::fsx::read_bounded_nofollow(Path::new(path), MAX_DBMD_FILE_BYTES).map_err(
            |e| CliError::from(e).with_hint(format!("could not read content from `{path}`")),
        )?
    };
    String::from_utf8(bytes).map_err(|error| {
        CliError::new(
            ExitCode::Runtime,
            "CONTENT_NOT_UTF8",
            format!("content is not UTF-8: {error}"),
        )
    })
}

/// Render a path with `/` separators for stable, platform-independent output.
pub(crate) fn path_str(p: &Path) -> String {
    p.components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}
